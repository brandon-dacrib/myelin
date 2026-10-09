//! Room peeking (MSC2753) through the real `hs` binary: `POST /peek/{roomIdOrAlias}` into a
//! world-readable room, the room in `rooms.peek` of the peeking device's `/sync` and of no other
//! device's, `403` for a room that is not world-readable, a long-poll on the peeking device
//! woken by the room's next event, and joining moving the room to `rooms.join`. Sytest's
//! `31sync/17peeking.pl` is the same story, less the long-poll.

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

fn bodies(entry: &Value) -> Vec<String> {
    entry["timeline"]["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_device_peeks_into_a_world_readable_room_and_only_that_device_sees_it() {
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
    let phone = login(&nobody, "bob", "PHONE").await;
    let laptop = login(&nobody, "bob", "LAPTOP").await;

    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "public_chat", "room_alias_name": "peekable"})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let private = alice
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
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.room.history_visibility/",
                escape(&room)
            ),
            Some(json!({"history_visibility": "world_readable"})),
            StatusCode::OK,
        )
        .await;

    // Not world-readable: refused.
    let refused = phone
        .expect(
            Method::POST,
            &format!("/_matrix/client/v3/peek/{}", escape(&private)),
            Some(json!({})),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(refused["errcode"], "M_FORBIDDEN", "{refused}");

    let phone_start = phone
        .expect(
            Method::GET,
            "/_matrix/client/v3/sync?timeout=0",
            None,
            StatusCode::OK,
        )
        .await["next_batch"]
        .as_str()
        .unwrap()
        .to_owned();
    // By alias.
    let peeked = phone
        .expect(
            Method::POST,
            &format!(
                "/_matrix/client/v3/peek/{}",
                escape("#peekable:example.org")
            ),
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(peeked["room_id"], room.as_str(), "{peeked}");
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/t1",
                escape(&room)
            ),
            Some(json!({"msgtype": "m.text", "body": "something to peek"})),
            StatusCode::OK,
        )
        .await;

    // The peeking device's sync: the room in `peek`, whole, ending with the message.
    let mut since = phone_start;
    let mut found = None;
    for _ in 0..20 {
        let body = phone
            .expect(
                Method::GET,
                &format!("/_matrix/client/v3/sync?timeout=2000&since={since}"),
                None,
                StatusCode::OK,
            )
            .await;
        since = body["next_batch"].as_str().unwrap().to_owned();
        let entry = &body["rooms"]["peek"][room.as_str()];
        if bodies(entry).contains(&"something to peek".to_owned()) {
            found = Some(body.clone());
            break;
        }
    }
    let body = found.expect("the peeked room reaches the peeking device");
    let entry = &body["rooms"]["peek"][room.as_str()];
    assert_eq!(
        entry["timeline"]["events"][0]["type"], "m.room.create",
        "{body}"
    );
    assert!(body["rooms"]["join"].is_null(), "{body}");

    // A long-poll on the peeking device is woken by the room's next event, not left to its
    // timeout: the hub wakes a peeker as it wakes a member (`SessionHub::fan_out_to_peekers`,
    // and the hot-room stream for a room above the fan-out threshold).
    let poll = {
        let phone = phone.clone();
        let since = since.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let body = phone
                .expect(
                    Method::GET,
                    &format!("/_matrix/client/v3/sync?timeout=10000&since={since}"),
                    None,
                    StatusCode::OK,
                )
                .await;
            (started.elapsed(), body)
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/t2",
                escape(&room)
            ),
            Some(json!({"msgtype": "m.text", "body": "wakes the peeker"})),
            StatusCode::OK,
        )
        .await;
    let (elapsed, body) = poll.await.unwrap();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the peeking device's long-poll was woken, not timed out: {elapsed:?}"
    );
    assert_eq!(
        bodies(&body["rooms"]["peek"][room.as_str()]),
        vec!["wakes the peeker".to_owned()],
        "{body}"
    );
    since = body["next_batch"].as_str().unwrap().to_owned();

    // The other device: nothing peeked.
    let other = laptop
        .expect(
            Method::GET,
            "/_matrix/client/v3/sync?timeout=0",
            None,
            StatusCode::OK,
        )
        .await;
    assert!(other["rooms"]["peek"].is_null(), "{other}");

    // Joining moves it to `join`.
    bob_joins(&phone, &room).await;
    let mut moved = false;
    for _ in 0..20 {
        let body = phone
            .expect(
                Method::GET,
                &format!("/_matrix/client/v3/sync?timeout=2000&since={since}"),
                None,
                StatusCode::OK,
            )
            .await;
        since = body["next_batch"].as_str().unwrap().to_owned();
        if !body["rooms"]["join"][room.as_str()].is_null() {
            assert!(body["rooms"]["peek"][room.as_str()].is_null(), "{body}");
            moved = true;
            break;
        }
    }
    assert!(moved, "the joined room reaches `join`");
    let line = hs.wait_for(&["a device started peeking into a world-readable room"]);
    assert!(line.contains("PHONE"), "{line}");
}

async fn bob_joins(bob: &Caller, room: &str) {
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/join/{}", escape(room)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
}
