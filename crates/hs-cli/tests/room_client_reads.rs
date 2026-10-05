//! The room read endpoints a client pages and previews with, against the real `hs` binary:
//! `/messages` (an empty `from`, `filter` with `types` and `lazy_load_members`, `end` on every
//! page with events), `/context` (a stranger is refused), `/relations` pagination,
//! `/room_summary`, `/timestamp_to_event`, `/publicRooms` paging, a redaction naming another
//! room's event, deleting a canonical alias, an ephemeral message expiring (MSC2228) and an
//! erased user's message pruned for who joined after it. Each is a Sytest or Complement test
//! that failed on 2026-10-04 (`docs/status/04-room-and-events.md`, session 19).

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
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

const SERVER: &str = "reads.example.org";

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
    /// `method path` with `body` (and a token, if any): the status and the JSON body.
    async fn request(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// [`Client::request`], asserting success.
    async fn ok(&self, method: Method, path: &str, token: &str, body: Option<Value>) -> Value {
        let (status, value) = self.request(method.clone(), path, Some(token), body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    async fn register(&self, username: &str) -> String {
        let (_, first) = self
            .request(
                Method::POST,
                "/_matrix/client/v3/register",
                None,
                Some(json!({"username": username, "password": "correct horse"})),
            )
            .await;
        let (_, done) = self
            .request(
                Method::POST,
                "/_matrix/client/v3/register",
                None,
                Some(json!({
                    "username": username,
                    "password": "correct horse",
                    "auth": {"type": "m.login.dummy", "session": first["session"]},
                })),
            )
            .await;
        done["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned()
    }

    async fn create_room(&self, token: &str, body: Value) -> String {
        self.ok(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            token,
            Some(body),
        )
        .await["room_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn join(&self, token: &str, room: &str) {
        self.ok(
            Method::POST,
            &format!("/_matrix/client/v3/join/{}", segment(room)),
            token,
            Some(json!({})),
        )
        .await;
    }

    async fn send(&self, token: &str, room: &str, txn: &str, content: Value) -> String {
        self.ok(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                segment(room)
            ),
            token,
            Some(content),
        )
        .await["event_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

/// A room ID, alias or event ID as one path segment, or a query value.
fn segment(id: &str) -> String {
    id.replace('%', "%25")
        .replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('$', "%24")
        .replace('{', "%7B")
        .replace('}', "%7D")
        .replace('"', "%22")
        .replace(' ', "%20")
        .replace('[', "%5B")
        .replace(']', "%5D")
        .replace(',', "%2C")
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn the_room_read_endpoints_answer_as_synapse_does() {
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

    let alice = client.register("alice").await;
    let bob = client.register("bob").await;
    let carol = client.register("carol").await;

    // `/messages` from `from=`, lazy-loading: an `end`, alice's member event alone in `state`,
    // and the page from that `end` is empty with no `end`.
    let room = client
        .create_room(&alice, json!({"preset": "public_chat"}))
        .await;
    client
        .send(
            &alice,
            &room,
            "m1",
            json!({"msgtype": "m.text", "body": "hello"}),
        )
        .await;
    let messages = |from: &str, filter: &str| {
        format!(
            "/_matrix/client/v3/rooms/{}/messages?dir=b&from={}&filter={}",
            segment(&room),
            segment(from),
            segment(filter)
        )
    };
    let page = client
        .ok(
            Method::GET,
            &messages("", r#"{"lazy_load_members":true}"#),
            &alice,
            None,
        )
        .await;
    assert!(!page["chunk"].as_array().unwrap().is_empty(), "{page}");
    let state = page["state"].as_array().expect("lazy-loaded state");
    assert_eq!(state.len(), 1, "{page}");
    assert_eq!(state[0]["state_key"], format!("@alice:{SERVER}"));
    let end = page["end"].as_str().expect("an end").to_owned();
    let last = client
        .ok(Method::GET, &messages(&end, "{}"), &alice, None)
        .await;
    assert_eq!(last["chunk"], json!([]));
    assert!(last.get("end").is_none(), "{last}");
    // Every event carries the reader's membership at it (MSC4115).
    assert_eq!(page["chunk"][0]["unsigned"]["membership"], "join");

    // An ephemeral message (MSC2228) is shown empty once its time has passed.
    client
        .send(
            &alice,
            &room,
            "m2",
            json!({"msgtype": "m.text", "body": "short-lived", "org.matrix.self_destruct_after": now_ms() + 1000}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let page = client
        .ok(
            Method::GET,
            &messages("", r#"{"types":["m.room.message"]}"#),
            &alice,
            None,
        )
        .await;
    let chunk = page["chunk"].as_array().unwrap();
    assert_eq!(chunk.len(), 2, "{page}");
    assert_eq!(chunk[0]["content"], json!({}), "{page}");
    assert_eq!(chunk[1]["content"]["body"], "hello");

    // A redaction naming another room's event is refused.
    let bobs_room = client.create_room(&bob, json!({})).await;
    let alices_event = client
        .send(
            &alice,
            &room,
            "m3",
            json!({"msgtype": "m.text", "body": "mine"}),
        )
        .await;
    let (status, body) = client
        .request(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/redact/{}/r1",
                segment(&bobs_room),
                segment(&alices_event)
            ),
            Some(&bob),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // `/context` in a room a stranger may not read is `403`, not `404`.
    let bobs_event = client
        .send(
            &bob,
            &bobs_room,
            "b1",
            json!({"msgtype": "m.text", "body": "private"}),
        )
        .await;
    let (status, body) = client
        .request(
            Method::GET,
            &format!(
                "/_matrix/client/v3/rooms/{}/context/{}",
                segment(&bobs_room),
                segment(&bobs_event)
            ),
            Some(&carol),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // `/relations` pages three at a time with `next_batch`.
    let root = client
        .send(
            &alice,
            &room,
            "root",
            json!({"msgtype": "m.text", "body": "root"}),
        )
        .await;
    let mut replies = Vec::new();
    for i in 0..5 {
        replies.push(
            client
                .send(
                    &alice,
                    &room,
                    &format!("reply{i}"),
                    json!({"msgtype": "m.text", "body": format!("reply {i}"),
                           "m.relates_to": {"rel_type": "m.thread", "event_id": root}}),
                )
                .await,
        );
    }
    let relations = format!(
        "/_matrix/client/v1/rooms/{}/relations/{}?limit=3",
        segment(&room),
        segment(&root)
    );
    let page = client.ok(Method::GET, &relations, &alice, None).await;
    let ids: Vec<&str> = page["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![&replies[4], &replies[3], &replies[2]], "{page}");
    let next = page["next_batch"].as_str().expect("next_batch").to_owned();
    let page = client
        .ok(
            Method::GET,
            &format!("{relations}&from={}", segment(&next)),
            &alice,
            None,
        )
        .await;
    assert_eq!(page["chunk"].as_array().unwrap().len(), 2, "{page}");
    assert!(page.get("next_batch").is_none(), "{page}");

    // `/room_summary`, with a token and without one.
    let summary = format!("/_matrix/client/v1/room_summary/{}", segment(&room));
    let body = client.ok(Method::GET, &summary, &bob, None).await;
    assert_eq!(body["room_id"], room.as_str());
    assert_eq!(body["join_rule"], "public");
    assert_eq!(body["membership"], "leave");
    let (status, body) = client.request(Method::GET, &summary, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = client
        .request(
            Method::GET,
            &format!("/_matrix/client/v1/room_summary/{}", segment(&bobs_room)),
            Some(&carol),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // `/timestamp_to_event`: the last event at or before now is the newest reply.
    let (status, body) = client
        .request(
            Method::GET,
            &format!(
                "/_matrix/client/v1/rooms/{}/timestamp_to_event?ts={}&dir=b",
                segment(&room),
                now_ms() + 60_000
            ),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["event_id"], replies[4].as_str());
    let (status, _) = client
        .request(
            Method::GET,
            &format!(
                "/_matrix/client/v1/rooms/{}/timestamp_to_event?ts={}&dir=b",
                segment(&bobs_room),
                now_ms()
            ),
            Some(&carol),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // `/publicRooms` pages: five published rooms, two at a time, each seen once.
    for _ in 0..4 {
        client
            .create_room(
                &alice,
                json!({"preset": "public_chat", "visibility": "public"}),
            )
            .await;
    }
    client
        .ok(
            Method::PUT,
            &format!("/_matrix/client/v3/directory/list/room/{}", segment(&room)),
            &alice,
            Some(json!({"visibility": "public"})),
        )
        .await;
    let mut seen = std::collections::HashMap::new();
    let mut since: Option<String> = None;
    loop {
        let page = client
            .ok(
                Method::POST,
                "/_matrix/client/v3/publicRooms",
                &alice,
                Some(json!({"limit": 2, "since": since})),
            )
            .await;
        for room in page["chunk"].as_array().unwrap() {
            *seen
                .entry(room["room_id"].as_str().unwrap().to_owned())
                .or_insert(0) += 1;
        }
        match page["next_batch"].as_str() {
            Some(next) => since = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5, "{seen:?}");
    assert!(seen.values().all(|n| *n == 1), "{seen:?}");

    // Deleting the canonical alias takes it out of `m.room.canonical_alias`.
    let alias = format!("#gone:{SERVER}");
    client
        .ok(
            Method::PUT,
            &format!("/_matrix/client/v3/directory/room/{}", segment(&alias)),
            &alice,
            Some(json!({"room_id": room})),
        )
        .await;
    let state_path = format!(
        "/_matrix/client/v3/rooms/{}/state/m.room.canonical_alias/",
        segment(&room)
    );
    client
        .ok(
            Method::PUT,
            &state_path,
            &alice,
            Some(json!({"alias": alias})),
        )
        .await;
    client
        .ok(
            Method::DELETE,
            &format!("/_matrix/client/v3/directory/room/{}", segment(&alias)),
            &alice,
            None,
        )
        .await;
    let content = client.ok(Method::GET, &state_path, &alice, None).await;
    assert_eq!(content, json!({}));

    // Erasure: bob was in the room when alice spoke, carol joined after; once alice erases her
    // account, bob still reads her message and carol reads it empty.
    client.join(&bob, &room).await;
    let said = client
        .send(
            &alice,
            &room,
            "erased",
            json!({"msgtype": "m.text", "body": "body1"}),
        )
        .await;
    client.join(&carol, &room).await;
    client
        .ok(
            Method::POST,
            "/_matrix/client/v3/account/deactivate",
            &alice,
            Some(json!({
                "erase": true,
                "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "correct horse"},
            })),
        )
        .await;
    let content_for = |token: String| {
        let path = format!(
            "/_matrix/client/v3/rooms/{}/event/{}",
            segment(&room),
            segment(&said)
        );
        let client = &client;
        async move { client.ok(Method::GET, &path, &token, None).await["content"].clone() }
    };
    assert_eq!(content_for(bob.clone()).await["body"], "body1");
    assert_eq!(content_for(carol.clone()).await, json!({}));
}
