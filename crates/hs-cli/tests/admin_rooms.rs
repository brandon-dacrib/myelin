//! The Rooms area's long tail through the real server, the way the room page drives it: a
//! room's state, timeline and events (each read of message content on the audit record), its
//! aliases (an alias added resolves for clients, a removed one does not), an administrator's
//! join, a space's hierarchy, the media the room refers to and quarantining it, purging history
//! (gone from the members' own `/messages`), deleting a room (its members leave, nobody can join
//! it again, it is not found), and forward extremities (a fork is reported and trimmed, and the
//! room goes on). Purges and deletions are tasks, and are counted on `/metrics`.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const SERVER: &str = "example.org";

fn test_config(data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: {SERVER}\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n",
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
}

/// A party that calls the server.
#[derive(Clone)]
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

    async fn expect(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        expected: StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, expected, "{path}: {body}");
        body
    }

    async fn get(&self, path: &str) -> Value {
        self.expect(Method::GET, path, None, StatusCode::OK).await
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        self.expect(Method::POST, path, Some(body), StatusCode::OK)
            .await
    }

    async fn create_room(&self, body: Value) -> String {
        self.post("/_matrix/client/v3/createRoom", body).await["room_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn say(&self, room_id: &str, body: &str) -> String {
        self.send(room_id, json!({"msgtype": "m.text", "body": body}))
            .await
    }

    async fn send(&self, room_id: &str, content: Value) -> String {
        let txn = format!(
            "t{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        self.expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                escape(room_id)
            ),
            Some(content),
            StatusCode::OK,
        )
        .await["event_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// The bodies of every message the caller's own `/messages` shows, oldest first.
    async fn message_bodies(&self, room_id: &str) -> Vec<String> {
        let page = self
            .get(&format!(
                "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100",
                escape(room_id)
            ))
            .await;
        let mut bodies: Vec<String> = page["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "m.room.message")
            .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
            .collect();
        bodies.reverse();
        bodies
    }
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('$', "%24")
}

/// The server, its first administrator (`@ops`), and `users` registered through the client API.
async fn boot(
    dir: &std::path::Path,
    users: &[&str],
) -> (hs_cli::serve::ServeHandle, Caller, Vec<Caller>) {
    let handle =
        hs_cli::serve::spawn_serve(test_config(dir), hs_cli::serve::ServeOptions::default())
            .await
            .expect("server should boot");
    let nobody = Caller {
        base: handle.base_url(),
        token: None,
    };
    let link = handle
        .setup_link
        .clone()
        .expect("a fresh server offers setup");
    let setup_token = link.split_once("#token=").unwrap().1.to_owned();
    let (status, session) = nobody
        .call(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
        )
        .await;
    assert!(status.is_success(), "{status}: {session}");
    let admin = Caller {
        base: handle.base_url(),
        token: Some(session["access_token"].as_str().unwrap().to_owned()),
    };
    let callers = register(&nobody, users).await;
    (handle, admin, callers)
}

async fn register(nobody: &Caller, users: &[&str]) -> Vec<Caller> {
    let mut callers = Vec::new();
    for name in users {
        let registered = nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}})),
                StatusCode::OK,
            )
            .await;
        callers.push(Caller {
            base: nobody.base.clone(),
            token: Some(registered["access_token"].as_str().unwrap().to_owned()),
        });
    }
    callers
}

/// Follows a task until it ends; answers it.
async fn finished(admin: &Caller, task: &Value) -> Value {
    let id = task["id"].as_str().unwrap();
    for _ in 0..200 {
        let now = admin.get(&format!("/api/v1/tasks/{id}")).await;
        if ["succeeded", "failed", "cancelled"].contains(&now["status"].as_str().unwrap()) {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("task {id} never ended");
}

async fn metrics(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rooms_state_timeline_aliases_join_hierarchy_and_media_through_the_real_server() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), &["alice", "carol"]).await;
    let (alice, carol) = (&users[0], &users[1]);
    let room_id = alice
        .create_room(json!({"preset": "private_chat", "name": "Lounge"}))
        .await;
    let room = format!("/api/v1/rooms/{}", escape(&room_id));
    let first = alice.say(&room_id, "first").await;
    let second = alice.say(&room_id, "second").await;
    alice.say(&room_id, "third").await;

    // State, by type.
    let state = admin.get(&format!("{room}/state")).await;
    let types: Vec<&str> = state["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"m.room.create"), "{state}");
    assert!(types.contains(&"m.room.name"), "{state}");

    // The timeline, newest first, and each event; every read of content is on the record.
    let page = admin.get(&format!("{room}/messages?limit=2")).await;
    assert_eq!(page["items"][0]["content"]["body"], "third", "{page}");
    assert_eq!(page["items"][1]["content"]["body"], "second");
    let older = admin
        .get(&format!(
            "{room}/messages?limit=2&cursor={}",
            page["next_cursor"].as_str().unwrap()
        ))
        .await;
    assert_eq!(older["items"][0]["content"]["body"], "first", "{older}");
    let event = admin
        .get(&format!("/api/v1/events/{}", escape(&first)))
        .await;
    assert_eq!(event["room_id"], room_id.as_str());
    let event = admin
        .get(&format!("{room}/events/{}", escape(&second)))
        .await;
    assert_eq!(event["sender"], "@alice:example.org");
    let context = admin
        .get(&format!(
            "{room}/events/{}/context?limit=1",
            escape(&second)
        ))
        .await;
    assert_eq!(context["events_before"][0]["content"]["body"], "first");
    assert_eq!(context["events_after"][0]["content"]["body"], "third");
    let ts = event["origin_server_ts"].as_i64().unwrap();
    let at = admin.get(&format!("{room}/events/at?ts={ts}&dir=b")).await;
    assert_eq!(at["event_id"], second.as_str());
    let reads = admin
        .get("/api/v1/audit-log?action=rooms.content.read")
        .await;
    assert_eq!(reads["items"].as_array().unwrap().len(), 6, "{reads}");
    assert_eq!(reads["items"][0]["actor"]["id"], "@ops:example.org");

    // An alias added through the admin API resolves for clients; removed, it does not.
    let added = admin
        .expect(
            Method::POST,
            &format!("{room}/aliases"),
            Some(json!({"alias": "#lounge:example.org"})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(added["creator"], "@ops:example.org");
    let resolved = carol
        .get("/_matrix/client/v3/directory/room/%23lounge%3Aexample.org")
        .await;
    assert_eq!(resolved["room_id"], room_id.as_str());
    let aliases = admin.get(&format!("{room}/aliases")).await;
    assert_eq!(aliases[0]["alias"], "#lounge:example.org");
    admin
        .expect(
            Method::POST,
            &format!("{room}/aliases"),
            Some(json!({"alias": "#lounge:example.org"})),
            StatusCode::CONFLICT,
        )
        .await;
    admin
        .expect(
            Method::DELETE,
            &format!("{room}/aliases/%23lounge%3Aexample.org"),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    carol
        .expect(
            Method::GET,
            "/_matrix/client/v3/directory/room/%23lounge%3Aexample.org",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    let audit = admin
        .get("/api/v1/audit-log?action=rooms.aliases.add")
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1);

    // An administrator joins carol to a private room she was never invited to.
    let member = admin
        .post(
            &format!("{room}/join"),
            json!({"user_id": "@carol:example.org"}),
        )
        .await;
    assert_eq!(member["membership"], "join", "{member}");
    let members = carol
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/joined_members",
            escape(&room_id)
        ))
        .await;
    assert!(members["joined"].get("@carol:example.org").is_some());
    admin
        .expect(
            Method::POST,
            &format!("{room}/join"),
            Some(json!({"user_id": "@nobody:example.org"})),
            StatusCode::BAD_REQUEST,
        )
        .await;

    // A space's hierarchy.
    let space = alice
        .create_room(json!({"preset": "public_chat", "name": "Space",
                             "creation_content": {"type": "m.space"}}))
        .await;
    alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/state/m.space.child/{}",
                escape(&space),
                escape(&room_id)
            ),
            Some(json!({"via": ["example.org"]})),
            StatusCode::OK,
        )
        .await;
    let tree = admin
        .get(&format!("/api/v1/rooms/{}/hierarchy", escape(&space)))
        .await;
    assert_eq!(tree["items"][0]["room_type"], "m.space", "{tree}");
    assert_eq!(tree["items"][1]["room_id"], room_id.as_str());
    assert_eq!(tree["items"][1]["name"], "Lounge");
    assert_eq!(tree["items"][1]["depth"], 1);

    // The media the room refers to, and quarantining all of it as a task.
    let uploaded: Value = reqwest::Client::new()
        .post(format!("{}/_matrix/media/v3/upload", alice.base))
        .bearer_auth(alice.token.as_ref().unwrap())
        .header("content-type", "image/png")
        .body(vec![0x89, b'P', b'N', b'G', 1, 2, 3])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mxc = uploaded["content_uri"].as_str().unwrap().to_owned();
    alice
        .send(
            &room_id,
            json!({"msgtype": "m.image", "body": "cat.png", "url": mxc}),
        )
        .await;
    let media = admin.get(&format!("{room}/media")).await;
    assert_eq!(media["items"].as_array().unwrap().len(), 1, "{media}");
    assert_eq!(media["items"][0]["quarantined"], false);
    let task = admin
        .expect(
            Method::POST,
            &format!("{room}/media/quarantine"),
            None,
            StatusCode::ACCEPTED,
        )
        .await;
    let done = finished(&admin, &task).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(done["result"]["quarantined"], 1);
    let media = admin.get(&format!("{room}/media")).await;
    assert_eq!(media["items"][0]["quarantined"], true, "{media}");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_purged_rooms_old_history_is_gone_from_messages() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), &["alice", "bob"]).await;
    let (alice, bob) = (&users[0], &users[1]);
    let room_id = alice.create_room(json!({"preset": "public_chat"})).await;
    bob.post(
        &format!("/_matrix/client/v3/join/{}", escape(&room_id)),
        json!({}),
    )
    .await;
    alice.say(&room_id, "old-1").await;
    bob.say(&room_id, "old-2").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let before = hs_http::time::now_rfc3339();
    tokio::time::sleep(Duration::from_millis(50)).await;
    alice.say(&room_id, "new-1").await;
    assert_eq!(
        bob.message_bodies(&room_id).await,
        ["old-1", "old-2", "new-1"]
    );

    let room = format!("/api/v1/rooms/{}", escape(&room_id));
    // By default this server's own users' messages are kept, and they all are its own.
    let task = admin
        .expect(
            Method::POST,
            &format!("{room}/purge-history"),
            Some(json!({"before": before})),
            StatusCode::ACCEPTED,
        )
        .await;
    let done = finished(&admin, &task).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(done["result"]["purged"], 0);
    assert_eq!(done["result"]["kept_local"], 2);

    let task = admin
        .expect(
            Method::POST,
            &format!("{room}/purge-history"),
            Some(json!({"before": before, "delete_local_events": true})),
            StatusCode::ACCEPTED,
        )
        .await;
    assert_eq!(task["action"], "rooms.purge_history");
    assert_eq!(task["resource"]["id"], room_id.as_str());
    let done = finished(&admin, &task).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(done["result"]["purged"], 2, "{done}");
    assert_eq!(done["progress"]["current"], 2, "{done}");

    assert_eq!(bob.message_bodies(&room_id).await, ["new-1"]);
    assert_eq!(alice.message_bodies(&room_id).await, ["new-1"]);
    let page = admin.get(&format!("{room}/messages")).await;
    assert!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["content"]["body"] != "old-1")
    );
    // The room goes on.
    bob.say(&room_id, "after").await;
    assert_eq!(alice.message_bodies(&room_id).await, ["new-1", "after"]);

    let audit = admin
        .get("/api/v1/audit-log?action=rooms.purge_history")
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 2, "{audit}");
    let text = metrics(&admin.base).await;
    assert!(
        text.contains(
            "hs_admin_room_operations_total{operation=\"purge_history\",outcome=\"succeeded\"} 2"
        ),
        "{text}"
    );
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_room_empties_moves_its_members_and_cannot_be_joined() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), &["alice", "bob"]).await;
    let (alice, bob) = (&users[0], &users[1]);
    let room_id = alice.create_room(json!({"preset": "public_chat"})).await;
    bob.post(
        &format!("/_matrix/client/v3/join/{}", escape(&room_id)),
        json!({}),
    )
    .await;
    bob.say(&room_id, "something awful").await;
    let room = format!("/api/v1/rooms/{}", escape(&room_id));
    admin
        .expect(
            Method::POST,
            &format!("{room}/aliases"),
            Some(json!({"alias": "#awful:example.org"})),
            StatusCode::CREATED,
        )
        .await;

    let task = admin
        .expect(
            Method::POST,
            &format!("{room}/delete"),
            Some(json!({
                "block": true,
                "message": "That room was removed.",
                "new_room": {"creator": "@ops:example.org", "name": "Notice"},
            })),
            StatusCode::ACCEPTED,
        )
        .await;
    let done = finished(&admin, &task).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    let result = &done["result"];
    assert_eq!(
        result["kicked_users"],
        json!(["@alice:example.org", "@bob:example.org"]),
        "{result}"
    );
    assert_eq!(result["local_aliases"], json!(["#awful:example.org"]));
    assert_eq!(result["purged"], true);
    let new_room = result["new_room_id"].as_str().unwrap().to_owned();

    // Gone for the administrator, for clients, and for anyone who tries to join.
    admin
        .expect(Method::GET, &room, None, StatusCode::NOT_FOUND)
        .await;
    let (status, body) = bob
        .call(
            Method::POST,
            &format!("/_matrix/client/v3/join/{}", escape(&room_id)),
            Some(json!({})),
        )
        .await;
    assert!(status.is_client_error(), "{status}: {body}");
    bob.expect(
        Method::GET,
        "/_matrix/client/v3/directory/room/%23awful%3Aexample.org",
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    // The members were moved to the new room, where the notice is.
    let bodies = bob.message_bodies(&new_room).await;
    assert_eq!(bodies, ["That room was removed."]);
    // A member's own sync still works, and no longer lists the room as joined.
    let sync = bob.get("/_matrix/client/v3/sync?timeout=0").await;
    assert!(sync["rooms"]["join"].get(&room_id).is_none(), "{sync}");

    let audit = admin.get("/api/v1/audit-log?action=rooms.delete").await;
    assert_eq!(audit["items"][0]["target"]["id"], room_id.as_str());
    let text = metrics(&admin.base).await;
    assert!(
        text.contains(
            "hs_admin_room_operations_total{operation=\"delete\",outcome=\"succeeded\"} 1"
        ),
        "{text}"
    );
    handle.shutdown().await;
}

/// One run of the real `hs` binary over a configuration file, read line by line from its log.
/// The fork below is made in the store between two runs, and only a process that has exited has
/// certainly let go of the store's lock.
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

    /// Reads the log until a line contains `needle`.
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

    /// Stops the server the way an orchestrator would, and waits for it to exit.
    fn stop(mut self) {
        let pid = self.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let _ = self.child.wait();
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Waits until the server answers its health check.
async fn wait_healthy(base: &str) {
    for _ in 0..400 {
        if let Ok(response) = reqwest::get(format!("{base}/health/live")).await
            && response.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{base} never became healthy");
}

/// Through the real binary: a fork is reported and trimmed, and the room goes on from what was
/// kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forked_rooms_extremities_are_reported_and_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let port = free_port();
    let config_path = dir.path().join("homeserver.yaml");
    let media_dir = data_dir.join("media");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: {SERVER}\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
             media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
             auth:\n  enable_registration: true\n",
        ),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let nobody = Caller {
        base: base.clone(),
        token: None,
    };

    // A room with one extremity.
    let mut first = HsProcess::serve(&config_path);
    let line = first.wait_for("setup_link=");
    let setup_token: String = line
        .split_once("#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    wait_healthy(&base).await;
    let session = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
            StatusCode::CREATED,
        )
        .await;
    let admin = Caller {
        base: base.clone(),
        token: Some(session["access_token"].as_str().unwrap().to_owned()),
    };
    let alice = register(&nobody, &["alice"]).await.remove(0);
    let room_id = alice.create_room(json!({"preset": "public_chat"})).await;
    alice.say(&room_id, "before the fork").await;
    let room = format!("/api/v1/rooms/{}", escape(&room_id));
    let one = admin.get(&format!("{room}/forward-extremities")).await;
    assert_eq!(one.as_array().unwrap().len(), 1, "{one}");
    first.stop();

    // What a federated room ends up with when two servers send at once, made here directly in
    // the store while the server is down: an event citing an older event than the newest. On a
    // runtime of its own, dropped afterwards, so the room actor lets go of the store.
    {
        let data_dir = data_dir.clone();
        let room_id = room_id.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let config = test_config(&data_dir);
                let hs_cli::storage::OpenedStorage::Embedded(backend) =
                    hs_cli::storage::open_storage(&config.storage).unwrap()
                else {
                    panic!("the test configuration is embedded storage");
                };
                let registry = hs_room::registry::RoomRegistry::open(
                    backend,
                    hs_room::identity::HomeserverIdentity::for_tests(SERVER),
                )
                .unwrap();
                let parsed = ruma::RoomId::parse(&room_id).unwrap();
                let actor = registry.get_or_load(&parsed).await.unwrap();
                actor
                    .administer(|actor| {
                        let older = actor
                            .paginate(None, hs_room::timeline::Direction::Backward, 2)
                            .0
                            .get(1)
                            .map(|e| e.event_id().to_owned())
                            .unwrap();
                        let sn = actor.event_sn_of(&older).unwrap();
                        actor.send_event_citing(
                            ruma::UserId::parse("@alice:example.org").unwrap(),
                            "m.room.message".to_owned(),
                            None,
                            json!({"msgtype": "m.text", "body": "on the fork"}),
                            None,
                            1,
                            &[sn],
                        )
                    })
                    .await
                    .unwrap();
            });
            runtime.shutdown_timeout(Duration::from_secs(10));
        })
        .join()
        .unwrap();
    }

    // The server again: two extremities, trimmed to one, and the room goes on.
    let mut second = HsProcess::serve(&config_path);
    second.wait_for("listening");
    wait_healthy(&base).await;
    let two = admin.get(&format!("{room}/forward-extremities")).await;
    assert_eq!(two.as_array().unwrap().len(), 2, "{two}");

    let pruned = admin
        .expect(
            Method::DELETE,
            &format!("{room}/forward-extremities"),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(pruned["deleted"].as_array().unwrap().len(), 1, "{pruned}");
    assert_eq!(pruned["remaining"][0]["event_id"], two[0]["event_id"]);
    let one = admin.get(&format!("{room}/forward-extremities")).await;
    assert_eq!(one.as_array().unwrap().len(), 1, "{one}");
    // The room goes on from the one kept.
    alice.say(&room_id, "after the trim").await;
    let one = admin.get(&format!("{room}/forward-extremities")).await;
    assert_eq!(one.as_array().unwrap().len(), 1, "{one}");
    let audit = admin
        .get("/api/v1/audit-log?action=rooms.forward_extremities.delete")
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1, "{audit}");
    second.stop();
}
