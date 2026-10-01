//! `POST /_matrix/client/v3/search` against the real `hs` binary: what Element's search box asks.
//!
//! Two users and three rooms on one server. Bob finds what he may see -- the messages of a room
//! he shares with alice from his join on (the room's history is `joined`), and his own room --
//! and not the message from before his join or alice's room of her own. The answer is ordered
//! by rank or by recency, pages by `next_batch`, and carries the context around each result.
//! Then the server is stopped and started again over its data directory: the index is still
//! there, nothing is indexed a second time, a message written after the restart is found, and
//! `/metrics` shows the index and the searches.

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

    /// Stops it as Kubernetes would (`SIGTERM`) and waits for it to exit.
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

fn config_yaml(port: u16, data_dir: &std::path::Path) -> String {
    format!(
        "server:\n  server_name: \"search.example.org\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n",
        media = data_dir.join("media"),
    )
}

struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    async fn call(&self, method: reqwest::Method, path: &str, token: &str, body: Value) -> Value {
        loop {
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
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = value["retry_after_ms"].as_u64().unwrap_or(500);
                tokio::time::sleep(Duration::from_millis(wait.max(50))).await;
                continue;
            }
            assert!(status.is_success(), "{method} {path}: {status} {value}");
            return value;
        }
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

    async fn login(&self, username: &str) -> String {
        let done: Value = self
            .http
            .post(format!("{}/_matrix/client/v3/login", self.base))
            .json(&json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": username},
                "password": "correct horse",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        done["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("login failed: {done}"))
            .to_owned()
    }

    async fn say(&self, token: &str, room: &str, body: &str) -> String {
        let txn = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sent = self
            .call(
                reqwest::Method::PUT,
                &format!("/_matrix/client/v3/rooms/{room}/send/m.room.message/t{txn}"),
                token,
                json!({"msgtype": "m.text", "body": body}),
            )
            .await;
        // Distinct timestamps, so that `recent` has one answer.
        tokio::time::sleep(Duration::from_millis(5)).await;
        sent["event_id"].as_str().unwrap().to_owned()
    }

    async fn search(&self, token: &str, criteria: Value, next: Option<&str>) -> Value {
        let path = match next {
            Some(token) => format!("/_matrix/client/v3/search?next_batch={token}"),
            None => "/_matrix/client/v3/search".to_owned(),
        };
        self.call(
            reqwest::Method::POST,
            &path,
            token,
            json!({"search_categories": {"room_events": criteria}}),
        )
        .await["search_categories"]["room_events"]
            .clone()
    }

    /// Searches until `count` is `want` (indexing runs behind the writes), or panics.
    async fn search_until(&self, token: &str, criteria: Value, want: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let found = self.search(token, criteria.clone(), None).await;
            if found["count"].as_u64() == Some(want) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "search {criteria} never counted {want}: {found}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn metric(&self, sample: &str) -> f64 {
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
            .unwrap_or(0.0)
    }
}

fn ids(room_events: &Value) -> Vec<String> {
    room_events["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["result"]["event_id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn element_search_finds_what_the_requester_may_see_and_survives_a_restart() {
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
    let post = reqwest::Method::POST;
    let create = |token: &str, body: Value| {
        let (client, token, post) = (&client, token.to_owned(), post.clone());
        async move {
            client
                .call(post, "/_matrix/client/v3/createRoom", &token, body)
                .await["room_id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };

    let shared = create(
        &alice,
        json!({"preset": "public_chat", "initial_state": [{"type": "m.room.history_visibility",
            "state_key": "", "content": {"history_visibility": "joined"}}]}),
    )
    .await;
    let early = client.say(&alice, &shared, "kumquat before bob").await;
    client
        .call(
            post.clone(),
            &format!("/_matrix/client/v3/join/{shared}"),
            &bob,
            json!({}),
        )
        .await;
    let mut shared_ids = Vec::new();
    for n in 0..4 {
        shared_ids.push(
            client
                .say(&alice, &shared, &format!("kumquat number {n}"))
                .await,
        );
    }
    let loud = client.say(&bob, &shared, "kumquat kumquat kumquat!").await;
    let alone = create(&alice, json!({"preset": "private_chat"})).await;
    let secret = client.say(&alice, &alone, "my kumquat secret").await;
    let bobs = create(&bob, json!({"preset": "private_chat"})).await;
    let own = client.say(&bob, &bobs, "kumquat of my own").await;

    // Bob: four messages and his own loud one in the shared room, and his own room's; not the
    // message from before his join, not alice's room.
    let recent = client
        .search_until(
            &bob,
            json!({"search_term": "kumquat", "order_by": "recent"}),
            6,
        )
        .await;
    let found = ids(&recent);
    assert!(
        !found.contains(&early) && !found.contains(&secret),
        "{found:?}"
    );
    let mut expected = shared_ids.clone();
    expected.extend([loud.clone(), own.clone()]);
    expected.reverse();
    assert_eq!(found, expected, "newest first");

    let ranked = client
        .search(&bob, json!({"search_term": "kumquat"}), None)
        .await;
    assert_eq!(ids(&ranked)[0], loud, "three times ranks first");

    // Pages of two are the whole answer, in order, once each.
    let mut paged = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let page = client
            .search(
                &bob,
                json!({"search_term": "kumquat", "order_by": "recent", "filter": {"limit": 2}}),
                next.as_deref(),
            )
            .await;
        paged.extend(ids(&page));
        match page["next_batch"].as_str() {
            Some(token) => next = Some(token.to_owned()),
            None => break,
        }
    }
    assert_eq!(paged, found);

    // Context around one result, and the room filter.
    let context = client
        .search(
            &bob,
            json!({"search_term": "number 2", "keys": ["content.body"],
                   "event_context": {"before_limit": 1, "after_limit": 1}}),
            None,
        )
        .await;
    assert_eq!(ids(&context), std::slice::from_ref(&shared_ids[2]));
    let around = &context["results"][0]["context"];
    assert_eq!(
        around["events_before"][0]["event_id"],
        shared_ids[1].as_str()
    );
    assert_eq!(
        around["events_after"][0]["event_id"],
        shared_ids[3].as_str()
    );
    let only_own = client
        .search(
            &bob,
            json!({"search_term": "kumquat", "filter": {"rooms": [bobs.clone()]}}),
            None,
        )
        .await;
    assert_eq!(ids(&only_own), std::slice::from_ref(&own));

    // Alice sees her early message and her own room, not bob's.
    let alice_found = client
        .search_until(&alice, json!({"search_term": "kumquat"}), 7)
        .await;
    assert!(ids(&alice_found).contains(&early) && !ids(&alice_found).contains(&own));

    let indexed = client.metric("hs_room_search_index_documents").await;
    assert!(indexed >= 8.0, "documents: {indexed}");
    assert!(client.metric("hs_room_search_duration_seconds_count").await >= 1.0);

    // A restart over the same data: nothing is indexed again, and what is new is found.
    hs.stop();
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");
    let bob = client.login("bob").await;
    let after = client.say(&bob, &bobs, "kumquat after the restart").await;
    let again = client
        .search_until(
            &bob,
            json!({"search_term": "kumquat", "order_by": "recent"}),
            7,
        )
        .await;
    assert_eq!(ids(&again)[0], after);
    assert_eq!(
        client.metric("hs_room_search_indexed_events_total").await,
        1.0,
        "only the message written after the restart was indexed by the restarted server"
    );
    assert_eq!(
        client.metric("hs_room_search_index_documents").await,
        indexed + 1.0
    );
    hs.stop();
}
