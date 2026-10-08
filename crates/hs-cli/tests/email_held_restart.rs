//! A notification email waiting to be sent survives the server being killed: the real `hs`
//! binary holds Alice's email for `notifications.delay_before_mail`, is killed (`SIGKILL`, no
//! graceful shutdown) while it waits, and the next `hs` on the same data directory restores it
//! from the push store (`hs_push::email::held`) and sends it, once.
//!
//! Also: a receipt in a thread takes only that thread's messages out of the waiting email, so
//! the email about the room's main timeline still goes (`hs_push::email`'s module docs).
//!
//! The SMTP server is a few lines of this test rather than Mailpit, so the test needs nothing
//! but the binary: it accepts plain SMTP and keeps each message's text.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A plain-SMTP sink: every message it is sent, as its raw `DATA`.
#[derive(Clone, Default)]
struct SmtpSink {
    messages: Arc<Mutex<Vec<String>>>,
}

impl SmtpSink {
    async fn start() -> (Self, u16) {
        let sink = Self::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let messages = sink.messages.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let messages = messages.clone();
                tokio::spawn(async move {
                    let _ = session(stream, messages).await;
                });
            }
        });
        (sink, port)
    }

    fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

async fn session(
    stream: tokio::net::TcpStream,
    messages: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    write.write_all(b"220 sink ESMTP\r\n").await?;
    while let Some(line) = lines.next_line().await? {
        let verb = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        match verb.as_str() {
            "EHLO" | "HELO" => write.write_all(b"250-sink\r\n250 8BITMIME\r\n").await?,
            "DATA" => {
                write.write_all(b"354 go ahead\r\n").await?;
                let mut data = String::new();
                while let Some(line) = lines.next_line().await? {
                    if line == "." {
                        break;
                    }
                    data.push_str(&line);
                    data.push('\n');
                }
                messages.lock().unwrap().push(data);
                write.write_all(b"250 queued\r\n").await?;
            }
            "QUIT" => {
                write.write_all(b"221 bye\r\n").await?;
                return Ok(());
            }
            _ => write.write_all(b"250 ok\r\n").await?,
        }
    }
    Ok(())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config_yaml(port: u16, data_dir: &std::path::Path, smtp_port: u16) -> String {
    config_yaml_holding(port, data_dir, smtp_port, "20s")
}

/// [`config_yaml`] with notification emails held for `delay`.
fn config_yaml_holding(
    port: u16,
    data_dir: &std::path::Path,
    smtp_port: u16,
    delay: &str,
) -> String {
    format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n\
         email:\n  smtp:\n    host: 127.0.0.1\n    port: {smtp_port}\n    security: none\n\
         \x20 from: \"Myelin <noreply@example.org>\"\n  app_name: Myelin\n\
         \x20 notifications:\n    delay_before_mail: {delay}\n",
        data_dir,
        data_dir.join("media"),
    )
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
            // The worker's "holding" line is a debug one.
            .env("RUST_LOG", "info,hs_push::email=debug")
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

    /// Reads the log until a line contains `needle`; 120 s bounds a broken (or cold) server.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
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
}

impl HsProcess {
    /// Everything logged so far and for `quiet` more.
    fn drain(&mut self, quiet: Duration) -> String {
        let deadline = std::time::Instant::now() + quiet;
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waiting_notification_email_is_sent_after_the_server_is_killed_and_restarted() {
    let (smtp, smtp_port) = SmtpSink::start().await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        config_yaml(port, &dir.path().join("data"), smtp_port),
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
            Some(json!({
                "pushkey": "alice@example.org",
                "app_id": "m.email",
                "kind": "email",
                "app_display_name": "Email Notifications",
                "device_display_name": "alice@example.org",
                "lang": "en",
                "data": {},
            })),
        )
        .await;
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"name": "Lunch", "invite": ["@bob:example.org"]})),
        )
        .await;
    let room_id = room["room_id"].as_str().unwrap().to_owned();
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/join/{room_id}"),
        Some(json!({})),
    )
    .await;
    bob.expect(
        Method::PUT,
        &format!("/_matrix/client/v3/rooms/{room_id}/send/m.room.message/txn1"),
        Some(json!({"msgtype": "m.text", "body": "Soup at noon?"})),
    )
    .await;

    // Held for 20 s; the server is killed well inside them.
    server.wait_for("holding a notification for an email");
    assert!(
        smtp.messages().is_empty(),
        "nothing is sent before it is due"
    );
    drop(server);

    let mut restarted = HsProcess::serve(&config_path);
    let restored = restarted.wait_for("restored the notification emails left waiting");
    assert!(restored.contains("emails=1"), "{restored}");
    restarted.wait_for("notification email sent");
    let messages = smtp.messages();
    assert_eq!(messages.len(), 1, "sent once: {messages:?}");
    // The subject names the room the held notification was in (bodies may be encoded).
    assert!(
        messages[0].contains("Lunch"),
        "the email has what was held: {}",
        messages[0]
    );
    assert!(messages[0].contains("alice@example.org"));
    drop(restarted);

    // Sent, it is not held any more: a third start restores nothing and sends nothing.
    let mut third = HsProcess::serve(&config_path);
    third.wait_for("listening");
    let log = third.drain(Duration::from_secs(3));
    assert!(
        !log.contains("restored the notification emails"),
        "nothing left to restore:\n{log}"
    );
    assert_eq!(smtp.messages().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thread_receipt_leaves_the_main_timeline_in_the_waiting_email() {
    let (smtp, smtp_port) = SmtpSink::start().await;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        config_yaml_holding(port, &dir.path().join("data"), smtp_port, "15s"),
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
            Some(json!({
                "pushkey": "alice@example.org",
                "app_id": "m.email",
                "kind": "email",
                "app_display_name": "Email Notifications",
                "device_display_name": "alice@example.org",
                "lang": "en",
                "data": {},
            })),
        )
        .await;
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"name": "Lunch", "invite": ["@bob:example.org"]})),
        )
        .await;
    let room_id = room["room_id"].as_str().unwrap().to_owned();
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/join/{room_id}"),
        Some(json!({})),
    )
    .await;

    // A message on the main timeline, and a thread under it.
    let send = |txn: &'static str, content: Value| {
        let path = format!("/_matrix/client/v3/rooms/{room_id}/send/m.room.message/{txn}");
        async move { bob.expect(Method::PUT, &path, Some(content)).await }
    };
    let root = send(
        "txn1",
        json!({"msgtype": "m.text", "body": "Soup at noon?"}),
    )
    .await;
    let root = root["event_id"].as_str().unwrap().to_owned();
    let reply = send(
        "txn2",
        json!({"msgtype": "m.text", "body": "Or salad",
               "m.relates_to": {"rel_type": "m.thread", "event_id": root}}),
    )
    .await;
    let reply = reply["event_id"].as_str().unwrap().to_owned();
    server.wait_for("holding a notification for an email");
    server.wait_for("holding a notification for an email");

    // Alice reads the thread, not the room: the thread's message leaves the email, the root
    // (on the main timeline) stays, and the email goes when due.
    alice
        .expect(
            Method::POST,
            &format!("/_matrix/client/v3/rooms/{room_id}/receipt/m.read/{reply}"),
            Some(json!({"thread_id": root})),
        )
        .await;
    let read = server.wait_for("a receipt took what it read out of a waiting notification email");
    assert!(read.contains("lines_read=1"), "{read}");
    server.wait_for("notification email sent");
    let messages = smtp.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("Lunch"), "{}", messages[0]);
}
