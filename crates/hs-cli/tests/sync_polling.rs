//! Long-polled `/sync` and a fresh batch's `prev_batch` through the real `hs` binary (track
//! 05's session 17): a long-poll whose filter drops presence waits for the message or the leave
//! that comes 100 ms into it (Sytest's `31sync/08polling.pl`; it used to answer at once with
//! nothing), and an initial sync of a small room hands out a `prev_batch` that `/messages`
//! pages back from and `/members?at=` reads the membership at (Sytest's
//! `10apidoc/34room-messages.pl`, Complement's `TestGetRoomMembersAtPoint`).

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
    room.replace('!', "%21")
        .replace(':', "%3A")
        .replace('#', "%23")
}

async fn login(nobody: &Caller, user: &str, device: &str) -> Caller {
    let body = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": user},
                "password": format!("hunter2-{user}"),
                "device_id": device,
            })),
            StatusCode::OK,
        )
        .await;
    nobody.with(body["access_token"].as_str().unwrap())
}

async fn sync(caller: &Caller, query: &str) -> Value {
    caller
        .expect(
            Method::GET,
            &format!("/_matrix/client/v3/sync?{query}"),
            None,
            StatusCode::OK,
        )
        .await
}

#[tokio::test]
async fn a_filtered_long_poll_waits_and_a_fresh_prev_batch_is_the_batch_end() {
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
    for user in ["alice", "bob"] {
        nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": user, "password": format!("hunter2-{user}"), "auth": {"type": "m.login.dummy"}})),
                StatusCode::OK,
            )
            .await;
    }
    let alice = login(&nobody, "alice", "ALICE").await;
    let bob = login(&nobody, "bob", "BOB").await;

    // Sytest's `08polling.pl`: a filter that drops presence, which every `/sync` refreshes.
    let filter_id = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/user/@alice:example.org/filter",
            Some(json!({"presence": {"not_types": ["m.presence"]}})),
            StatusCode::OK,
        )
        .await["filter_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let create = |alice: Caller| async move {
        alice
            .expect(
                Method::POST,
                "/_matrix/client/v3/createRoom",
                Some(json!({"preset": "public_chat"})),
                StatusCode::OK,
            )
            .await["room_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let room = create(alice.clone()).await;
    let since = sync(&alice, &format!("filter={filter_id}&timeout=0")).await["next_batch"]
        .as_str()
        .unwrap()
        .to_owned();
    let sender = {
        let alice = alice.clone();
        let room = room.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            alice
                .expect(
                    Method::PUT,
                    &format!(
                        "/_matrix/client/v3/rooms/{}/send/m.room.message/t1",
                        escape(&room)
                    ),
                    Some(json!({"msgtype": "m.text", "body": "1"})),
                    StatusCode::OK,
                )
                .await["event_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
    };
    let started = std::time::Instant::now();
    let polled = sync(
        &alice,
        &format!("filter={filter_id}&since={since}&timeout=10000"),
    )
    .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "woken, not timed out: {:?}",
        started.elapsed()
    );
    let event_id = sender.await.unwrap();
    let events = polled["rooms"]["join"][&room]["timeline"]["events"].clone();
    assert_eq!(
        events.as_array().map(|e| e.len()),
        Some(1),
        "the long-poll carries the message: {polled}"
    );
    assert_eq!(events[0]["event_id"], event_id.as_str(), "{polled}");

    // "Sync is woken up for leaves".
    let left = create(alice.clone()).await;
    let since = sync(&alice, &format!("filter={filter_id}&timeout=0")).await["next_batch"]
        .as_str()
        .unwrap()
        .to_owned();
    let leaver = {
        let alice = alice.clone();
        let left = left.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            alice
                .expect(
                    Method::POST,
                    &format!("/_matrix/client/v3/rooms/{}/leave", escape(&left)),
                    Some(json!({})),
                    StatusCode::OK,
                )
                .await;
        })
    };
    let started = std::time::Instant::now();
    let polled = sync(
        &alice,
        &format!("filter={filter_id}&since={since}&timeout=10000"),
    )
    .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "woken, not timed out: {:?}",
        started.elapsed()
    );
    leaver.await.unwrap();
    assert_eq!(
        polled["rooms"]["leave"][&left]["timeline"]["events"]
            .as_array()
            .map(|e| e.len()),
        Some(1),
        "the long-poll carries the leave: {polled}"
    );

    // Complement's `TestGetRoomMembersAtPoint` and Sytest's "GET /rooms/:room_id/messages
    // returns a message".
    let point = create(alice.clone()).await;
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/t2",
                escape(&point)
            ),
            Some(json!({"msgtype": "m.text", "body": "Hello world!"})),
            StatusCode::OK,
        )
        .await;
    let initial = sync(&alice, "timeout=0").await;
    let prev_batch = initial["rooms"]["join"][&point]["timeline"]["prev_batch"]
        .as_str()
        .unwrap()
        .to_owned();
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/join/{}", escape(&point)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    bob.expect(
        Method::PUT,
        &format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/t3",
            escape(&point)
        ),
        Some(json!({"msgtype": "m.text", "body": "Hello back"})),
        StatusCode::OK,
    )
    .await;
    let members = alice
        .expect(
            Method::GET,
            &format!(
                "/_matrix/client/v3/rooms/{}/members?at={prev_batch}",
                escape(&point)
            ),
            None,
            StatusCode::OK,
        )
        .await;
    let keys: Vec<&str> = members["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["state_key"].as_str())
        .collect();
    assert_eq!(keys, vec!["@alice:example.org"], "{members}");
    let page = alice
        .expect(
            Method::GET,
            &format!(
                "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=1&from={prev_batch}",
                escape(&point)
            ),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        page["chunk"][0]["content"]["body"], "Hello world!",
        "{page}"
    );
}
