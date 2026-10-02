//! Redactions against the real `hs` binary, around a change of power levels: a redaction is
//! judged by the power levels in force when it is sent. Bob at power 0 cannot redact alice's
//! message; made a moderator he can, and the message is emptied for everyone; demoted again his
//! next redaction is refused, and the first stays redacted. (The cases where the redaction is
//! judged later than it was sent -- one waiting for its event, one received over federation --
//! are `hs_room::actor::tests`'s; one server cannot show them.)

use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One `hs serve` process, its stdout read line by line.
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

    /// Reads the log until a line contains `needle`. A debug `hs` under load can take a minute
    /// to boot, so the deadline is generous.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
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

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const SERVER: &str = "redact.example.org";

fn config_yaml(port: u16, data_dir: &std::path::Path) -> String {
    format!(
        "server:\n  server_name: \"{SERVER}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n",
        media = data_dir.join("media"),
    )
}

struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    /// `method path` with `body`, asserting success.
    async fn call(&self, method: reqwest::Method, path: &str, token: &str, body: Value) -> Value {
        let response = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    async fn get(&self, path: &str, token: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "GET {path}: {status} {value}");
        value
    }

    async fn register(&self, username: &str) -> String {
        let first: Value = self
            .http
            .post(format!("{}/_matrix/client/v3/register", self.base))
            .json(&json!({"username": username, "password": "correct horse"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let done: Value = self
            .http
            .post(format!("{}/_matrix/client/v3/register", self.base))
            .json(&json!({
                "username": username,
                "password": "correct horse",
                "auth": {"type": "m.login.dummy", "session": first["session"]},
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        done["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned()
    }

    async fn metric(&self, sample: &str) -> u64 {
        let text = self
            .http
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        text.lines()
            .find_map(|line| line.strip_prefix(sample)?.trim().parse::<f64>().ok())
            .map_or(0, |v| v as u64)
    }
}

/// A room ID or alias as one path segment.
fn segment(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
}

/// Alice makes a public room at `from` with alias `#{alias}`, bob joins it, alice upgrades it to
/// `to`; then bob joins the room the old room's tombstone names, by that ID, and says something
/// there. Returns `(old room, replacement room)`.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redaction_is_judged_by_the_power_levels_when_it_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let config = dir.path().join("hs.yaml");
    std::fs::write(&config, config_yaml(port, &dir.path().join("data"))).unwrap();
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");
    let post = reqwest::Method::POST;
    let put = reqwest::Method::PUT;

    let alice = client.register("alice").await;
    let bob = client.register("bob").await;
    let bob_id = format!("@bob:{SERVER}");
    let room = client
        .call(
            post.clone(),
            "/_matrix/client/v3/createRoom",
            &alice,
            json!({"preset": "public_chat"}),
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/join/{}", segment(&room)),
            &bob,
            json!({}),
        )
        .await;
    let mut messages = Vec::new();
    for n in 1..=2 {
        let sent = client
            .call(
                put.clone(),
                &format!(
                    "/_matrix/client/v3/rooms/{}/send/m.room.message/m{n}",
                    segment(&room)
                ),
                &alice,
                json!({"msgtype": "m.text", "body": format!("message {n}")}),
            )
            .await;
        messages.push(sent["event_id"].as_str().unwrap().to_owned());
    }

    let redact = |token: String, target: String, txn: &'static str| {
        let client = &client;
        let room = room.clone();
        async move {
            client
                .http
                .put(format!(
                    "{}/_matrix/client/v3/rooms/{}/redact/{}/{txn}",
                    client.base,
                    segment(&room),
                    segment(&target)
                ))
                .bearer_auth(token)
                .json(&json!({"reason": "cleanup"}))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    let set_bob = |level: i64| {
        let client = &client;
        let alice = alice.clone();
        let room = room.clone();
        let bob_id = bob_id.clone();
        let put = put.clone();
        async move {
            let mut levels = client
                .get(
                    &format!(
                        "/_matrix/client/v3/rooms/{}/state/m.room.power_levels/",
                        segment(&room)
                    ),
                    &alice,
                )
                .await;
            levels["users"][bob_id] = json!(level);
            client
                .call(
                    put.clone(),
                    &format!(
                        "/_matrix/client/v3/rooms/{}/state/m.room.power_levels/",
                        segment(&room)
                    ),
                    &alice,
                    levels,
                )
                .await;
        }
    };
    let content_of = |target: String| {
        let client = &client;
        let alice = alice.clone();
        let room = room.clone();
        async move {
            client
                .get(
                    &format!(
                        "/_matrix/client/v3/rooms/{}/event/{}",
                        segment(&room),
                        segment(&target)
                    ),
                    &alice,
                )
                .await["content"]
                .clone()
        }
    };

    assert_eq!(
        redact(bob.clone(), messages[0].clone(), "r0").await,
        reqwest::StatusCode::FORBIDDEN,
        "bob at power 0 may not redact alice's message"
    );
    set_bob(50).await;
    assert_eq!(
        redact(bob.clone(), messages[0].clone(), "r1").await,
        reqwest::StatusCode::OK,
        "bob at 50 may"
    );
    assert_eq!(
        content_of(messages[0].clone()).await,
        json!({}),
        "the message is emptied for everyone"
    );
    set_bob(0).await;
    assert_eq!(
        redact(bob.clone(), messages[1].clone(), "r2").await,
        reqwest::StatusCode::FORBIDDEN,
        "demoted, bob's next redaction is refused"
    );
    assert_eq!(content_of(messages[1].clone()).await["body"], "message 2");
    assert_eq!(
        content_of(messages[0].clone()).await,
        json!({}),
        "the redaction made while bob was a moderator stands"
    );
}
