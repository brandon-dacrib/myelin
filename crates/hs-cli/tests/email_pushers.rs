//! Email pushers through the real `hs` binary and a real SMTP server: an administrator binds
//! Alice's address, Alice registers an email pusher for it, Bob sends her a message, and the
//! notification email is read back out of Mailpit's API: its subject, the snippet, the room
//! link, and the `hs_push_email_sent_total{outcome="sent"}` metric.
//!
//! Mailpit runs in Docker. The test starts its own container (`mirror.gcr.io/axllent/mailpit`,
//! since Docker Hub pulls fail in agent sessions) on ports Docker picks, and removes it when it
//! is done; or set `HS_TEST_MAILPIT=<smtp port>,<api port>` to use one already running on
//! 127.0.0.1. Without either, the test prints `SKIP` and passes.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const MAILPIT_IMAGE: &str = "mirror.gcr.io/axllent/mailpit:latest";

/// A Mailpit the test can send to and read from.
struct Mailpit {
    smtp_port: u16,
    api: String,
    /// The container this test started, removed on drop.
    container: Option<String>,
}

impl Drop for Mailpit {
    fn drop(&mut self) {
        if let Some(name) = &self.container {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", name])
                .output();
        }
    }
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("docker is not available: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn published_port(container: &str, port: &str) -> Result<u16, String> {
    let mapping = docker(&["port", container, port])?;
    mapping
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("no published port in {mapping:?}"))
}

async fn mailpit() -> Result<Mailpit, String> {
    let mailpit = if let Ok(ports) = std::env::var("HS_TEST_MAILPIT") {
        let (smtp, api) = ports
            .split_once(',')
            .ok_or("HS_TEST_MAILPIT is <smtp port>,<api port>")?;
        Mailpit {
            smtp_port: smtp.parse().map_err(|_| "bad smtp port")?,
            api: format!("http://127.0.0.1:{api}"),
            container: None,
        }
    } else {
        let name = format!("hs-email-pushers-mail-{}", std::process::id());
        docker(&[
            "run",
            "--rm",
            "-d",
            "--name",
            &name,
            "-p",
            "127.0.0.1::1025",
            "-p",
            "127.0.0.1::8025",
            MAILPIT_IMAGE,
        ])?;
        let mut mailpit = Mailpit {
            smtp_port: 0,
            api: String::new(),
            container: Some(name.clone()),
        };
        mailpit.smtp_port = published_port(&name, "1025")?;
        mailpit.api = format!("http://127.0.0.1:{}", published_port(&name, "8025")?);
        mailpit
    };
    // Up when its API answers.
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if let Ok(r) = client
            .get(format!("{}/api/v1/messages", mailpit.api))
            .send()
            .await
            && r.status().is_success()
        {
            return Ok(mailpit);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(format!("Mailpit at {} never answered", mailpit.api))
}

fn config_yaml(port: u16, data_dir: &std::path::Path, smtp_port: u16) -> String {
    format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: true\n\
         email:\n  smtp:\n    host: 127.0.0.1\n    port: {smtp_port}\n    security: none\n\
         \x20 from: \"Myelin <noreply@example.org>\"\n  app_name: Myelin\n  client_base_url: https://app.example.org\n",
        data_dir,
        data_dir.join("media"),
    )
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    fn serve(config_path: &std::path::Path) -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("the hs binary should start");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Reads the log until a line contains `needle`; 120 s bounds a broken server.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needle:?}; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    fn log(&mut self) -> String {
        while let Ok(line) = self
            .lines
            .recv_timeout(std::time::Duration::from_millis(200))
        {
            self.seen.push(line);
        }
        self.seen.join("\n")
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = reqwest::Client::new().request(method, format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn expect(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    fn with_token(&self, token: &Value) -> Caller {
        Caller {
            base: self.base.clone(),
            token: Some(token.as_str().unwrap().to_owned()),
        }
    }
}

fn email_pusher(address: &str) -> Value {
    json!({
        "pushkey": address,
        "app_id": "m.email",
        "kind": "email",
        "app_display_name": "Email Notifications",
        "device_display_name": address,
        "lang": "en",
        "data": {},
    })
}

/// Mailpit's messages to `address`, newest first, until there is at least one.
async fn mail_to(api: &str, address: &str) -> Vec<Value> {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let list: Value = client
            .get(format!("{api}/api/v1/messages"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let to_address: Vec<Value> = list["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| {
                m["To"]
                    .as_array()
                    .is_some_and(|to| to.iter().any(|t| t["Address"] == address))
            })
            .collect();
        if !to_address.is_empty() || std::time::Instant::now() > deadline {
            return to_address;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_someone_with_an_email_pusher_reaches_their_inbox() {
    let mailpit = match mailpit().await {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "SKIP: the email pusher test needs Mailpit: {e}\nStart one with: docker run --rm \
                 -d --name hs-email-pushers-mail -p 1025:1025 -p 8025:8025 {MAILPIT_IMAGE} and \
                 set HS_TEST_MAILPIT=1025,8025"
            );
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        config_yaml(port, &dir.path().join("data"), mailpit.smtp_port),
    )
    .unwrap();
    let mut server = HsProcess::serve(&config_path);
    let line = server.wait_for("setup_link=");
    let setup_token: String = line
        .split_once("/admin/setup#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let log = server.log();
    assert!(
        log.contains("email: notification emails go through this SMTP server"),
        "the boot log says where mail goes:\n{log}"
    );

    let nobody = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
    };
    let admin = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
        )
        .await;
    let admin = nobody.with_token(&admin["access_token"]);
    let mut sessions = Vec::new();
    for name in ["alice", "bob"] {
        let registered = nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}})),
            )
            .await;
        sessions.push(nobody.with_token(&registered["access_token"]));
    }
    let (alice, bob) = (&sessions[0], &sessions[1]);

    // An address Alice does not own is refused; once an administrator binds it, it is taken.
    let (status, refused) = alice
        .call(
            Method::POST,
            "/_matrix/client/v3/pushers/set",
            Some(email_pusher("alice@example.org")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    admin
        .expect(
            Method::POST,
            "/api/v1/users/%40alice%3Aexample.org/threepids",
            Some(json!({"medium": "email", "address": "alice@example.org"})),
        )
        .await;
    alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/pushers/set",
            Some(email_pusher("alice@example.org")),
        )
        .await;
    // The admin API lists it as an email pusher with the address (what the Users page shows).
    let listed = admin
        .expect(
            Method::GET,
            "/api/v1/users/%40alice%3Aexample.org/pushers",
            None,
        )
        .await;
    assert_eq!(listed["items"][0]["kind"], "email", "{listed}");
    assert_eq!(listed["items"][0]["pushkey"], "alice@example.org");

    // Alice makes a room called Lunch and invites Bob, who joins and writes.
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"name": "Lunch", "invite": ["@bob:example.org"]})),
        )
        .await;
    let room_id = room["room_id"].as_str().unwrap().to_owned();
    bob.expect(
        Method::PUT,
        "/_matrix/client/v3/profile/@bob:example.org/displayname",
        Some(json!({"displayname": "Bob"})),
    )
    .await;
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/join/{room_id}"),
        Some(json!({})),
    )
    .await;
    bob.expect(
        Method::PUT,
        &format!("/_matrix/client/v3/rooms/{room_id}/send/m.room.message/txn1"),
        Some(json!({"msgtype": "m.text", "body": "Soup at noon? <b>bring bowls</b>"})),
    )
    .await;

    let mail = mail_to(&mailpit.api, "alice@example.org").await;
    assert_eq!(
        mail.len(),
        1,
        "one email for one message; the server said:\n{}",
        server.log()
    );
    let subject = mail[0]["Subject"].as_str().unwrap();
    assert!(
        subject == "[Myelin] You have a message on Myelin from Bob in the Lunch room...",
        "{subject}"
    );
    assert_eq!(mail[0]["From"]["Address"], "noreply@example.org");
    let id = mail[0]["ID"].as_str().unwrap();
    let message: Value = reqwest::get(format!("{}/api/v1/message/{id}", mailpit.api))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = message["Text"].as_str().unwrap();
    let html = message["HTML"].as_str().unwrap();
    assert!(text.contains("Lunch (1 unread)"), "{text}");
    assert!(text.contains("Soup at noon? <b>bring bowls</b>"), "{text}");
    assert!(
        text.contains(&format!("https://app.example.org/#/room/{room_id}")),
        "{text}"
    );
    assert!(
        html.contains("Soup at noon? &lt;b&gt;bring bowls&lt;/b&gt;"),
        "the snippet is escaped in HTML: {html}"
    );
    assert!(html.contains(&format!(
        "href=\"https://app.example.org/#/room/{room_id}\""
    )));

    // A second message straight after is held by the throttle (ten minutes), not mailed.
    bob.expect(
        Method::PUT,
        &format!("/_matrix/client/v3/rooms/{room_id}/send/m.room.message/txn2"),
        Some(json!({"msgtype": "m.text", "body": "hello?"})),
    )
    .await;

    let metrics = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("hs_push_email_sent_total{outcome=\"sent\"} 1"),
        "{}",
        metrics
            .lines()
            .filter(|l| l.contains("hs_push_email"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(
        mail_to(&mailpit.api, "alice@example.org").await.len(),
        1,
        "the throttle holds the second message"
    );
    let log = server.log();
    assert!(log.contains("notification email sent"), "{log}");
}
