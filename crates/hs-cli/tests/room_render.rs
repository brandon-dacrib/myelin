//! What a room's events look like to a client, and what follows a member to an upgraded room,
//! against the real `hs` binary. Each part is a Complement test that failed on 2026-10-05
//! (`docs/status/04-room-and-events.md`, session 20):
//!
//! - a version-12 room's `m.room.create` event carries its `room_id` in every read that
//!   returns it (`TestMSC4291RoomIDAsHashOfCreateEvent_RoomIDIsOnCreateEvent`);
//! - an application service sends with `?ts=` and the event is given that timestamp; anyone
//!   else's `ts` is ignored; `/timestamp_to_event` over two events with one timestamp finds the
//!   later backwards and the earlier forwards (`TestJumpToDateEndpoint`'s appservice subtests);
//! - every local member's push rules for a room follow them to its replacement, for an
//!   `/upgrade` and for a room created by hand with a `predecessor`, and their next `/sync`
//!   carries the new `m.push_rules` (`TestPushRuleRoomUpgrade`).

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

const SERVER: &str = "render.example.org";
const AS_TOKEN: &str = "as_token_for_the_room_render_test_000000000000000000000000000000";
const HS_TOKEN: &str = "hs_token_for_the_room_render_test_000000000000000000000000000000";

fn config_yaml(port: u16, data_dir: &std::path::Path, registration: &std::path::Path) -> String {
    format!(
        "server:\n  server_name: \"{SERVER}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n\
         appservices:\n  registration_files: [{registration:?}]\n",
        media = data_dir.join("media"),
    )
}

/// Complement's blueprint appservice, give or take the server: its sender is the bridge user
/// the jump-to-date subtests send as. Its URL answers nothing; nothing here needs the bridge to.
fn registration_yaml() -> String {
    format!(
        "id: render-bridge\nurl: http://127.0.0.1:9\nas_token: {AS_TOKEN}\nhs_token: {HS_TOKEN}\n\
         sender_localpart: the-bridge-user\nrate_limited: false\n\
         namespaces:\n  users:\n    - regex: '@the-bridge-.*'\n      exclusive: true\n"
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

/// The `room` push rules of the user `token` belongs to, by `rule_id`.
async fn room_rules(client: &Client, token: &str) -> Vec<(String, Value)> {
    let rules = client
        .ok(Method::GET, "/_matrix/client/v3/pushrules/", token, None)
        .await;
    rules["global"]["room"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["rule_id"].as_str().unwrap().to_owned(),
                r["actions"].clone(),
            )
        })
        .collect()
}

/// `PUT /pushrules/global/{kind}/{ruleId}`.
async fn set_rule(client: &Client, token: &str, kind: &str, rule_id: &str, body: Value) {
    client
        .ok(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/pushrules/global/{kind}/{}",
                segment(rule_id)
            ),
            token,
            Some(body),
        )
        .await;
}

/// Waits until `token`'s `/sync` from `since` carries `m.push_rules` with a room rule for
/// every room in `rooms`, as Complement's `syncGlobalAccountDataHasPushRuleForRoomID` does.
async fn sync_until_room_rules(client: &Client, token: &str, since: &str, rooms: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut since = since.to_owned();
    loop {
        let body = client
            .ok(
                Method::GET,
                &format!(
                    "/_matrix/client/v3/sync?timeout=1000&since={}",
                    segment(&since)
                ),
                token,
                None,
            )
            .await;
        let found = body["account_data"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["type"] == "m.push_rules")
            .any(|e| {
                let ids: Vec<&str> = e["content"]["global"]["room"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r["rule_id"].as_str())
                    .collect();
                rooms.iter().all(|room| ids.contains(room))
            });
        if found {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no /sync carried m.push_rules for {rooms:?}"
        );
        since = body["next_batch"].as_str().unwrap().to_owned();
    }
}

async fn next_batch(client: &Client, token: &str) -> String {
    client
        .ok(
            Method::GET,
            "/_matrix/client/v3/sync?timeout=0",
            token,
            None,
        )
        .await["next_batch"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn events_render_and_follow_an_upgrade_as_synapse_does() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let registration = dir.path().join("bridge.yaml");
    std::fs::write(&registration, registration_yaml()).unwrap();
    let config = dir.path().join("hs.yaml");
    std::fs::write(
        &config,
        config_yaml(port, &dir.path().join("data"), &registration),
    )
    .unwrap();
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");

    let alice = client.register("alice").await;
    let alice2 = client.register("alice2").await;

    // ---- A version-12 create event carries its room_id everywhere it is read. ----
    let v12 = client
        .create_room(&alice, json!({"room_version": "12"}))
        .await;
    let message = client
        .send(
            &alice,
            &v12,
            "v12m",
            json!({"msgtype": "m.text", "body": "Hello"}),
        )
        .await;
    let create_id = format!("${}", &v12[1..]);
    let room_path = format!("/_matrix/client/v3/rooms/{}", segment(&v12));
    let find_create = |events: &Value| -> Value {
        events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "m.room.create")
            .cloned()
            .unwrap_or_else(|| panic!("no create event in {events}"))
    };
    let state = client
        .ok(Method::GET, &format!("{room_path}/state"), &alice, None)
        .await;
    let messages = client
        .ok(
            Method::GET,
            &format!("{room_path}/messages?dir=b&limit=100"),
            &alice,
            None,
        )
        .await;
    let event = client
        .ok(
            Method::GET,
            &format!("{room_path}/event/{}", segment(&create_id)),
            &alice,
            None,
        )
        .await;
    let direct = client
        .ok(
            Method::GET,
            &format!("{room_path}/context/{}", segment(&create_id)),
            &alice,
            None,
        )
        .await;
    let context = client
        .ok(
            Method::GET,
            &format!("{room_path}/context/{}?limit=100", segment(&message)),
            &alice,
            None,
        )
        .await;
    let whole = client
        .ok(
            Method::GET,
            &format!("{room_path}/state/m.room.create/?format=event"),
            &alice,
            None,
        )
        .await;
    for (read, create) in [
        ("/state", find_create(&state)),
        ("/messages", find_create(&messages["chunk"])),
        ("/event", event),
        ("/context direct", direct["event"].clone()),
        ("/context indirect", find_create(&context["events_before"])),
        ("/context state", find_create(&context["state"])),
        ("/state?format=event", whole),
    ] {
        assert_eq!(create["room_id"], v12.as_str(), "{read}: {create}");
        assert_eq!(create["event_id"], create_id.as_str(), "{read}: {create}");
    }

    // ---- An application service sends with a timestamp of its choosing. ----
    let room = client
        .create_room(&alice, json!({"preset": "public_chat"}))
        .await;
    let bridge = format!("@the-bridge-user:{SERVER}");
    client.join(AS_TOKEN, &room).await;
    let at: i64 = 1_641_168_000_000; // 2022-01-03, an import from before the room.
    let send_at = |txn: &str, token: &str, body: &str| {
        let path = format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}?ts={at}",
            segment(&room)
        );
        let token = token.to_owned();
        let body = body.to_owned();
        let client = &client;
        async move {
            client
                .ok(
                    Method::PUT,
                    &path,
                    &token,
                    Some(json!({"msgtype": "m.text", "body": body})),
                )
                .await["event_id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let first = send_at("ts1", AS_TOKEN, "messageWithSameTime1").await;
    let second = send_at("ts2", AS_TOKEN, "messageWithSameTime2").await;
    let not_bridged = send_at("ts3", &alice, "a person's ts is ignored").await;
    let ts_of = |event_id: String| {
        let path = format!(
            "/_matrix/client/v3/rooms/{}/event/{}",
            segment(&room),
            segment(&event_id)
        );
        let client = &client;
        let alice = alice.clone();
        async move {
            client.ok(Method::GET, &path, &alice, None).await["origin_server_ts"]
                .as_i64()
                .unwrap()
        }
    };
    assert_eq!(ts_of(first.clone()).await, at);
    assert_eq!(ts_of(second.clone()).await, at);
    assert!(ts_of(not_bridged).await > at);
    let sender = client
        .ok(
            Method::GET,
            &format!(
                "/_matrix/client/v3/rooms/{}/event/{}",
                segment(&room),
                segment(&first)
            ),
            &alice,
            None,
        )
        .await["sender"]
        .clone();
    assert_eq!(sender, bridge.as_str());
    let nearest = |dir: &str| {
        let path = format!(
            "/_matrix/client/v1/rooms/{}/timestamp_to_event?ts={at}&dir={dir}",
            segment(&room)
        );
        let client = &client;
        let alice = alice.clone();
        async move {
            client.ok(Method::GET, &path, &alice, None).await["event_id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    assert_eq!(
        nearest("b").await,
        second,
        "backwards finds the later of the two"
    );
    assert_eq!(
        nearest("f").await,
        first,
        "forwards finds the earlier of the two"
    );
    let (status, _) = client
        .request(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/bad?ts=soon",
                segment(&room)
            ),
            Some(AS_TOKEN),
            Some(json!({"msgtype": "m.text", "body": "x"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a ts that is not a number");

    // ---- Push rules follow every local member to the replacement, both ways of upgrading. ----
    for manual in [false, true] {
        let old = client
            .create_room(
                &alice,
                json!({"preset": "public_chat", "room_version": "10"}),
            )
            .await;
        client.join(&alice2, &old).await;
        for token in [&alice, &alice2] {
            set_rule(
                &client,
                token,
                "room",
                &old,
                json!({"actions": ["dont_notify"]}),
            )
            .await;
        }
        // An override rule on the old room, named after it, follows too.
        set_rule(
            &client,
            &alice,
            "override",
            &format!("mute-{old}"),
            json!({
                "actions": ["dont_notify"],
                "conditions": [{"kind": "event_match", "key": "room_id", "pattern": old}],
            }),
        )
        .await;
        let alice_since = next_batch(&client, &alice).await;
        let alice2_since = next_batch(&client, &alice2).await;
        let new = if manual {
            let new = client
                .create_room(
                    &alice,
                    json!({
                        "preset": "public_chat",
                        "room_version": "11",
                        "creation_content": {"predecessor": {"room_id": old}},
                    }),
                )
                .await;
            client
                .ok(
                    Method::PUT,
                    &format!(
                        "/_matrix/client/v3/rooms/{}/state/m.room.tombstone/",
                        segment(&old)
                    ),
                    &alice,
                    Some(json!({"body": "replaced", "replacement_room": new})),
                )
                .await;
            new
        } else {
            client
                .ok(
                    Method::POST,
                    &format!("/_matrix/client/v3/rooms/{}/upgrade", segment(&old)),
                    &alice,
                    Some(json!({"new_version": "11"})),
                )
                .await["replacement_room"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        client.join(&alice2, &new).await;
        for (token, since) in [(&alice, &alice_since), (&alice2, &alice2_since)] {
            sync_until_room_rules(&client, token, since, &[&old, &new]).await;
            let rules = room_rules(&client, token).await;
            for room in [&old, &new] {
                let (_, actions) = rules
                    .iter()
                    .find(|(id, _)| id == room)
                    .unwrap_or_else(|| panic!("manual={manual}: no rule for {room}: {rules:?}"));
                assert_eq!(actions[0], "dont_notify", "manual={manual}");
            }
        }
        let copied = client
            .ok(
                Method::GET,
                &format!(
                    "/_matrix/client/v3/pushrules/global/override/{}",
                    segment(&format!("mute-{new}"))
                ),
                &alice,
                None,
            )
            .await;
        assert_eq!(
            copied["conditions"][0]["pattern"],
            new.as_str(),
            "manual={manual}"
        );
        assert_eq!(copied["actions"][0], "dont_notify");
    }
    hs.wait_for("their push rules for the old room followed");
}
