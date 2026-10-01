//! The deprecated event stream and initial syncs through the real `hs` binary:
//! `GET /events` (`from`, `timeout`, `start`/`end`, a `chunk` of events with their rooms),
//! `GET /initialSync` and `GET /rooms/{roomId}/initialSync`. All three answered 404 before, and
//! Sytest's helpers wait on `/events` even in tests about something else.
//!
//! A long poll is checked too: an `/events` request waiting for news returns as soon as a
//! message is sent, not at its timeout.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One run of the real `hs` binary (the harness of `config_hot.rs`).
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

    /// Reads the log until a line contains every one of `needles`.
    fn wait_for(&mut self, needles: &[&str]) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if needles.iter().all(|needle| line.contains(needle)) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needles:?}; it said:\n{}",
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

#[derive(Clone)]
struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
    fn with(&self, token: &str) -> Self {
        Self {
            token: Some(token.to_owned()),
            ..self.clone()
        }
    }

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

    async fn expect(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        want: StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, want, "{path}: {body}");
        body
    }
}

fn escape(room: &str) -> String {
    room.replace('!', "%21").replace(':', "%3A")
}

#[tokio::test]
async fn the_legacy_event_stream_and_initial_syncs_answer_from_the_sync_feed() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             federation:\n  enabled: false\n\
             rate_limits:\n  enabled: false\n\
             auth:\n  enable_registration: true\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let mut hs = HsProcess::serve(&config_path);
    hs.wait_for(&["setup_link="]);
    let nobody = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
    };
    let alice = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "alice", "password": "hunter2-alice", "auth": {"type": "m.login.dummy"}})),
            StatusCode::OK,
        )
        .await;
    let alice = nobody.with(alice["access_token"].as_str().unwrap());

    // No `from`: now, and nothing yet.
    let now = alice
        .expect(
            Method::GET,
            "/_matrix/client/r0/events?timeout=0",
            None,
            StatusCode::OK,
        )
        .await;
    assert!(now["chunk"].as_array().unwrap().is_empty(), "{now}");
    assert!(now["start"].is_string(), "{now}");
    let from = now["end"].as_str().unwrap().to_owned();

    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "public_chat"})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let send = |text: &'static str, txn: &'static str| {
        let alice = alice.clone();
        let room = room.clone();
        async move {
            alice
                .expect(
                    Method::PUT,
                    &format!(
                        "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                        escape(&room)
                    ),
                    Some(json!({"msgtype": "m.text", "body": text})),
                    StatusCode::OK,
                )
                .await;
        }
    };
    send("hello", "t1").await;
    let next = alice
        .expect(
            Method::GET,
            &format!("/_matrix/client/r0/events?timeout=5000&from={from}"),
            None,
            StatusCode::OK,
        )
        .await;
    let chunk = next["chunk"].as_array().unwrap();
    let hello = chunk
        .iter()
        .find(|e| e["content"]["body"] == "hello")
        .unwrap_or_else(|| panic!("{next}"));
    assert_eq!(hello["room_id"], room.as_str());
    assert!(
        chunk.iter().any(|e| e["type"] == "m.room.create"),
        "the room's creation is news too: {next}"
    );
    let end = next["end"].as_str().unwrap().to_owned();

    // A long poll returns when something happens, not at its timeout.
    let started = std::time::Instant::now();
    let poll = {
        let alice = alice.clone();
        let end = end.clone();
        tokio::spawn(async move {
            alice
                .expect(
                    Method::GET,
                    &format!("/_matrix/client/r0/events?timeout=20000&from={end}"),
                    None,
                    StatusCode::OK,
                )
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    send("are you there", "t2").await;
    let woken = poll.await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the poll waited for its timeout"
    );
    assert!(
        woken["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["content"]["body"] == "are you there"),
        "{woken}"
    );

    let (status, body) = alice
        .call(Method::GET, "/_matrix/client/r0/events?timeout=hello", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let initial = alice
        .expect(
            Method::GET,
            "/_matrix/client/v3/initialSync?limit=5",
            None,
            StatusCode::OK,
        )
        .await;
    assert!(initial["end"].is_string(), "{initial}");
    let listed = initial["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["room_id"] == room.as_str())
        .unwrap_or_else(|| panic!("{initial}"));
    assert_eq!(listed["membership"], "join");
    assert!(
        listed["state"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "m.room.create"),
        "{listed}"
    );

    let one_room = alice
        .expect(
            Method::GET,
            &format!("/_matrix/client/v3/rooms/{}/initialSync", escape(&room)),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(one_room["membership"], "join", "{one_room}");
    let texts: Vec<&str> = one_room["messages"]["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    assert_eq!(texts, vec!["hello", "are you there"], "{one_room}");
}
