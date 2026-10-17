//! A member's own departure from a hot room reaches them, through the real `hs` binary.
//! Found on the demo (2026-10-17): the owner left two rooms of 600-odd members through
//! Element, the server accepted both leaves, Element went on showing both rooms, and leaving
//! again was refused (`Event is not authorized`, from the resident asked for a `make_leave`
//! template: a leave over a leave). A hot room (over `server.sync.fan_out_threshold_members`
//! members) wrote no feed entries and `/sync` listed a hot room only while the user was in it,
//! so their own leave reached nobody. Here the threshold is one member, so a room of two is
//! hot: bob's leave wakes his long-poll and is in it under `leave`, a second `/leave` is
//! answered `200` with nothing sent, and the sync after that has nothing of the room.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

/// One run of the real `hs` binary (the harness of `config_hot.rs`).
struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    /// Starts `hs serve`, handing it `listeners` still bound (`hs_testkit::ports::pass_to`).
    fn serve(
        config_path: &std::path::Path,
        listeners: impl IntoIterator<Item = hs_testkit::ports::ReservedPort>,
    ) -> Self {
        use std::io::BufRead;
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hs"));
        command
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        hs_testkit::ports::pass_to(&mut command, listeners);
        let mut child = command.spawn().expect("the hs binary should start");
        drop(command);
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
async fn a_leave_from_a_hot_room_reaches_the_leaver_and_a_repeat_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let listener = hs_testkit::ports::reserve();
    let port = listener.port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n  sync:\n    fan_out_threshold_members: 1\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health]\n\
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
    let mut hs = HsProcess::serve(&config_path, [listener]);
    hs.wait_for(&["fan_out_threshold_members=1"]);
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

    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "public_chat", "name": "Big"})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/join/{}", escape(&room)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    // Two members over a threshold of one: hot. Bob's initial sync has the room, joined.
    let initial = sync(&bob, "timeout=0").await;
    assert!(
        initial["rooms"]["join"][&room].is_object(),
        "bob's initial sync lacks the room: {initial}"
    );
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/t1",
                escape(&room)
            ),
            Some(json!({"msgtype": "m.text", "body": "a hot room"})),
            StatusCode::OK,
        )
        .await;
    // The message reaches bob the hot way (the hot-room stream), which says the room is hot.
    let since = initial["next_batch"].as_str().unwrap().to_owned();
    let after_message = sync(&bob, &format!("since={since}&timeout=10000")).await;
    assert_eq!(
        after_message["rooms"]["join"][&room]["timeline"]["events"]
            .as_array()
            .map(|e| e.len()),
        Some(1),
        "{after_message}"
    );
    let since = after_message["next_batch"].as_str().unwrap().to_owned();

    // Bob leaves 100 ms into his long-poll: it wakes, and carries the leave under `leave`.
    let leaver = {
        let bob = bob.clone();
        let room = room.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            bob.expect(
                Method::POST,
                &format!("/_matrix/client/v3/rooms/{}/leave", escape(&room)),
                Some(json!({})),
                StatusCode::OK,
            )
            .await;
        })
    };
    let started = std::time::Instant::now();
    let polled = sync(&bob, &format!("since={since}&timeout=10000")).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "woken by the leave, not timed out: {:?}",
        started.elapsed()
    );
    leaver.await.unwrap();
    assert!(
        polled["rooms"]["join"].get(&room).is_none(),
        "the room is not joined any more: {polled}"
    );
    let left = &polled["rooms"]["leave"][&room];
    assert!(left.is_object(), "bob was not told he left: {polled}");
    let own_leave = left["timeline"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| {
            e["type"] == "m.room.member"
                && e["state_key"] == "@bob:example.org"
                && e["content"]["membership"] == "leave"
        });
    assert!(
        own_leave,
        "the leave event is not in the timeline: {polled}"
    );
    let since = polled["next_batch"].as_str().unwrap().to_owned();

    // A client that missed the leave would try again: `200`, nothing sent, nothing refused.
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/rooms/{}/leave", escape(&room)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    let members = alice
        .expect(
            Method::GET,
            &format!("/_matrix/client/v3/rooms/{}/members", escape(&room)),
            None,
            StatusCode::OK,
        )
        .await;
    let bobs_leaves = members["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["state_key"] == "@bob:example.org" && e["content"]["membership"] == "leave")
        .count();
    assert_eq!(bobs_leaves, 1, "one leave event, not two: {members}");

    // The room goes on without him, and his next sync has nothing of it: the leave once.
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/t2",
                escape(&room)
            ),
            Some(json!({"msgtype": "m.text", "body": "after bob"})),
            StatusCode::OK,
        )
        .await;
    let quiet = sync(&bob, &format!("since={since}&timeout=500")).await;
    assert!(
        quiet["rooms"]["leave"].get(&room).is_none() && quiet["rooms"]["join"].get(&room).is_none(),
        "the room came back: {quiet}"
    );
}
