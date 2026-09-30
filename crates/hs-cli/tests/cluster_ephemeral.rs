//! Typing, receipts and presence cross replicas: two real `hs serve` processes on one
//! PostgreSQL, alice's session on A and bob's on B, in one room. What one of them does on their
//! replica shows in the other's `/sync` on the other replica, in both directions, through the
//! same batches that carry the room wakes (`hs_cli::sync_cluster`, `hs_user::cluster`'s
//! "Typing, receipts and presence").
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise:
//!
//! ```sh
//! docker run --rm -d --name hs-cluster-ephemeral-pg -e POSTGRES_PASSWORD=hspg \
//!     -p 127.0.0.1:5439:5432 postgres:17
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test -p hs-cli --test cluster_ephemeral
//! ```
//!
//! Each run makes a database of its own (`hs_cluster_ephemeral_<pid>_<nanos>`) and drops it
//! after. Every check waits for a condition through `/sync` with a deadline, never for a fixed
//! time: a `/sync` here is a long-poll of up to a second, repeated until the deadline.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A port for a subprocess's configuration file; never the same one twice in this process.
fn reserve_port() -> u16 {
    static HANDED_OUT: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut handed_out = HANDED_OUT.lock().unwrap();
        if !handed_out.contains(&port) {
            handed_out.push(port);
            return port;
        }
    }
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// One run of the real `hs` binary, reading its log.
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

    /// Reads the log until a line contains `needle`. A debug `hs` under load can take a
    /// minute to boot, so the deadline is generous.
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

    /// Stops it as Kubernetes would (`SIGTERM`), waits, and returns its log.
    fn stop(mut self) -> String {
        let pid = self.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let _ = self.child.wait();
        while let Ok(line) = self.lines.recv() {
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

fn setup_token_of(line: &str) -> String {
    let link = line.split_once("setup_link=").unwrap().1;
    link.split_once("#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect()
}

/// The first administrator, made through the setup link the log printed.
async fn first_admin(client: &reqwest::Client, base: &str, setup_line: &str) -> String {
    let admin: Value = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({
            "setup_token": setup_token_of(setup_line),
            "username": "ops",
            "password": "hunter2-first-admin",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    admin["access_token"].as_str().unwrap().to_owned()
}

/// A database of this test's own on the test server, dropped afterwards.
struct Database {
    admin_dsn: String,
    name: String,
    host: String,
    port: u16,
    user: String,
    password: String,
}

impl Database {
    /// A fresh database, or `None` (after saying so) when no server is reachable. On a thread of
    /// its own: the synchronous `postgres` client runs a runtime inside.
    fn create() -> Option<Self> {
        std::thread::spawn(Self::create_blocking).join().unwrap()
    }

    fn create_blocking() -> Option<Self> {
        let admin_dsn = std::env::var("HS_CLUSTER_TEST_POSTGRES_DSN")
            .unwrap_or_else(|_| "postgres://postgres:hspg@127.0.0.1:5439/postgres".to_owned());
        let config: postgres::Config = admin_dsn.parse().expect("a postgres:// DSN");
        let mut client = match config.connect(postgres::NoTls) {
            Ok(client) => client,
            Err(e) => {
                eprintln!(
                    "SKIP: the two-replica ephemeral test needs PostgreSQL at {admin_dsn:?}: \
                     {e}\nStart one with: docker run --rm -d --name hs-cluster-ephemeral-pg \
                     -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17"
                );
                return None;
            }
        };
        let name = format!(
            "hs_cluster_ephemeral_{}_{}",
            std::process::id(),
            rand_suffix()
        );
        client
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .unwrap();
        let host = match config.get_hosts().first() {
            Some(postgres::config::Host::Tcp(host)) => host.clone(),
            _ => "127.0.0.1".to_owned(),
        };
        Some(Self {
            admin_dsn,
            name,
            host,
            port: config.get_ports().first().copied().unwrap_or(5432),
            user: config.get_user().unwrap_or("postgres").to_owned(),
            password: String::from_utf8_lossy(config.get_password().unwrap_or_default())
                .into_owned(),
        })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let (dsn, name) = (self.admin_dsn.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            if let Ok(mut client) = dsn
                .parse::<postgres::Config>()
                .and_then(|c| c.connect(postgres::NoTls))
            {
                let _ =
                    client.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
            }
        })
        .join();
    }
}

/// One replica's configuration: a small pool, since the test server allows a hundred
/// connections and other tests may be running.
fn replica_config(db: &Database, dir: &std::path::Path, port: u16, mesh_port: u16) -> String {
    format!(
        "server:\n  server_name: cluster.example.org\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n  pool_size: 8\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         cluster:\n  single_node: false\n  room_shards: 4\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    shared_secret: cluster-ephemeral-test-secret\n",
        keys = dir.join("keys"),
        media = dir.join(format!("media-{port}")),
        host = db.host,
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
    )
}

struct User {
    id: String,
    token: String,
}

async fn register(client: &reqwest::Client, base: &str, username: &str) -> User {
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"]
        .as_str()
        .unwrap_or_else(|| panic!("no UIA session: {first}"))
        .to_owned();
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    User {
        id: done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        token: done["access_token"].as_str().unwrap().to_owned(),
    }
}

/// A session: one user polling one replica, carrying its `since` token forward.
struct Session<'a> {
    client: &'a reqwest::Client,
    base: String,
    user: &'a User,
    since: Option<String>,
}

impl<'a> Session<'a> {
    /// An initial sync, repeated until it shows `room` joined: the join was written on the
    /// room's owner and this replica may be another, whose feed it reaches a moment later.
    async fn start(client: &'a reqwest::Client, base: &str, user: &'a User, room: &str) -> Self {
        let mut session = Self {
            client,
            base: base.to_owned(),
            user,
            since: None,
        };
        let deadline = Instant::now() + WITHIN;
        loop {
            session.since = None;
            let initial = session.sync(0).await;
            if initial["rooms"]["join"][room].is_object() {
                return session;
            }
            assert!(
                Instant::now() < deadline,
                "{}'s initial sync on {} never showed {room} joined: {initial}",
                user.id,
                session.base
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// One `/sync`, a long-poll of `timeout_ms`, with `set_presence=offline` so that polling
    /// itself changes no presence: the only presence changes in this test are the explicit
    /// ones. Moves `since` forward.
    async fn sync(&mut self, timeout_ms: u64) -> Value {
        let mut url = format!(
            "{}/_matrix/client/v3/sync?timeout={timeout_ms}&set_presence=offline",
            self.base
        );
        if let Some(since) = &self.since {
            url.push_str("&since=");
            url.push_str(since);
        }
        let response: Value = self
            .client
            .get(url)
            .bearer_auth(&self.user.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        self.since = Some(
            response["next_batch"]
                .as_str()
                .unwrap_or_else(|| panic!("no next_batch: {response}"))
                .to_owned(),
        );
        response
    }

    /// Polls `/sync` (one-second long-polls) until `check` answers `Some`, or panics after
    /// `within` with `what`.
    async fn until<T>(
        &mut self,
        within: Duration,
        what: &str,
        mut check: impl FnMut(&Value) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + within;
        loop {
            let response = self.sync(1000).await;
            if let Some(value) = check(&response) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the last sync was {response}"
            );
        }
    }
}

/// The `m.typing` event for `room_id` in a response, if there is one: its `user_ids`.
fn typing_user_ids(response: &Value, room_id: &str) -> Option<Vec<String>> {
    response["rooms"]["join"][room_id]["ephemeral"]["events"]
        .as_array()?
        .iter()
        .find(|e| e["type"] == "m.typing")
        .map(|e| {
            e["content"]["user_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|u| u.as_str().map(str::to_owned))
                .collect()
        })
}

/// The `m.receipt` content for `room_id` in a response, if there is one.
fn receipt_content(response: &Value, room_id: &str) -> Option<Value> {
    response["rooms"]["join"][room_id]["ephemeral"]["events"]
        .as_array()?
        .iter()
        .find(|e| e["type"] == "m.receipt")
        .map(|e| e["content"].clone())
}

/// The presence event from `sender` in a response, if there is one: its content.
fn presence_from(response: &Value, sender: &str) -> Option<Value> {
    response["presence"]["events"]
        .as_array()?
        .iter()
        .find(|e| e["sender"] == sender)
        .map(|e| e["content"].clone())
}

async fn put_typing(client: &reqwest::Client, base: &str, room: &str, user: &User, body: Value) {
    let response = client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{room}/typing/{}",
            user.id
        ))
        .bearer_auth(&user.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "typing: {} {}",
        response.status(),
        response.text().await.unwrap_or_default()
    );
}

async fn send_message(
    client: &reqwest::Client,
    base: &str,
    room: &str,
    user: &User,
    body: &str,
) -> String {
    let response: Value = client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/{}",
            rand_suffix()
        ))
        .bearer_auth(&user.token)
        .json(&json!({"msgtype": "m.text", "body": body}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    response["event_id"]
        .as_str()
        .unwrap_or_else(|| panic!("send failed: {response}"))
        .to_owned()
}

async fn post_receipt(
    client: &reqwest::Client,
    base: &str,
    room: &str,
    user: &User,
    event_id: &str,
) {
    let response = client
        .post(format!(
            "{base}/_matrix/client/v3/rooms/{room}/receipt/m.read/{event_id}"
        ))
        .bearer_auth(&user.token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "receipt: {}",
        response.status()
    );
}

async fn put_presence(
    client: &reqwest::Client,
    base: &str,
    user: &User,
    presence: &str,
    msg: &str,
) {
    let response = client
        .put(format!(
            "{base}/_matrix/client/v3/presence/{}/status",
            user.id
        ))
        .bearer_auth(&user.token)
        .json(&json!({"presence": presence, "status_msg": msg}))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "presence: {}",
        response.status()
    );
}

/// The value of one sample line of `/metrics` at `base`, `0` when it is not there.
async fn metric(client: &reqwest::Client, base: &str, sample: &str) -> u64 {
    let text = client
        .get(format!("{base}/metrics"))
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

/// Polls `check` until it answers `Some`, or panics after `within` with `what`.
async fn eventually<T, F, Fut>(within: Duration, what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How long a cross-replica change may take to show in the other user's `/sync`. Generous for
/// a loaded machine; on an idle one it is one long-poll woken within milliseconds.
const WITHIN: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_receipts_and_presence_cross_two_replicas_in_both_directions() {
    let Some(db) = Database::create() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (port_a, port_b) = (reserve_port(), reserve_port());
    let (mesh_a, mesh_b) = (reserve_port(), reserve_port());
    let config_a = dir.path().join("a.yaml");
    let config_b = dir.path().join("b.yaml");
    std::fs::write(&config_a, replica_config(&db, dir.path(), port_a, mesh_a)).unwrap();
    std::fs::write(&config_b, replica_config(&db, dir.path(), port_b, mesh_b)).unwrap();
    let a = format!("http://127.0.0.1:{port_a}");
    let b = format!("http://127.0.0.1:{port_b}");
    let (id_a, id_b) = (format!("127.0.0.1:{mesh_a}"), format!("127.0.0.1:{mesh_b}"));
    let client = reqwest::Client::new();

    // A first (it makes the signing key and the schema), then B.
    let mut hs_a = HsProcess::serve(&config_a);
    let setup_line = hs_a.wait_for("setup_link=");
    let admin = first_admin(&client, &a, &setup_line).await;
    let mut hs_b = HsProcess::serve(&config_b);
    hs_b.wait_for("listening");

    // Both replicas own shards and every shard is owned, so that every room request has an
    // owner to land on or be forwarded to.
    eventually(
        Duration::from_secs(60),
        "two active replicas sharing every shard",
        || {
            let (client, a, admin, id_a, id_b) = (&client, &a, &admin, &id_a, &id_b);
            async move {
                let replicas: Value = client
                    .get(format!("{a}/api/v1/cluster/replicas"))
                    .bearer_auth(admin)
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?;
                let items = replicas["items"].as_array()?;
                let both = [id_a, id_b].iter().all(|id| {
                    items.iter().any(|r| {
                        r["id"] == id.as_str()
                            && r["status"] == "active"
                            && r["shard_count"].as_u64() > Some(0)
                    })
                });
                let shards: Value = client
                    .get(format!("{a}/api/v1/cluster/shards?limit=500"))
                    .bearer_auth(admin)
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?;
                let all_owned = shards["items"]
                    .as_array()?
                    .iter()
                    .all(|s| !s["owner"].is_null());
                (both && all_owned).then_some(())
            }
        },
    )
    .await;

    // Alice lives on A, bob on B. Whichever replica owns the room, every `/rooms/...` request
    // is forwarded to it, so the two directions below cross the mesh at least once each.
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;
    let created: Value = client
        .post(format!("{a}/_matrix/client/v3/createRoom"))
        .bearer_auth(&alice.token)
        .json(&json!({"preset": "public_chat", "name": "ephemeral"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room = created["room_id"]
        .as_str()
        .unwrap_or_else(|| panic!("createRoom failed: {created}"))
        .to_owned();
    let joined = client
        .post(format!("{b}/_matrix/client/v3/rooms/{room}/join"))
        .bearer_auth(&bob.token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(joined.status().is_success(), "join: {}", joined.status());

    // Both sessions see the room joined before anything ephemeral is asked about it.
    let mut alice_on_a = Session::start(&client, &a, &alice, &room).await;
    let mut bob_on_b = Session::start(&client, &b, &bob, &room).await;

    // ---- Typing, A -> B: alice types on A, bob's long-poll on B shows it, and the stop. ----
    put_typing(
        &client,
        &a,
        &room,
        &alice,
        json!({"typing": true, "timeout": 30000}),
    )
    .await;
    bob_on_b
        .until(WITHIN, "alice typing (set on A) in bob's sync on B", |r| {
            typing_user_ids(r, &room).filter(|ids| ids.contains(&alice.id))
        })
        .await;
    put_typing(&client, &a, &room, &alice, json!({"typing": false})).await;
    bob_on_b
        .until(WITHIN, "alice's stop (set on A) in bob's sync on B", |r| {
            typing_user_ids(r, &room).filter(|ids| !ids.contains(&alice.id))
        })
        .await;

    // ---- Typing, B -> A, with the timeout lapsing on A by itself. ----
    put_typing(
        &client,
        &b,
        &room,
        &bob,
        json!({"typing": true, "timeout": 1500}),
    )
    .await;
    alice_on_a
        .until(WITHIN, "bob typing (set on B) in alice's sync on A", |r| {
            typing_user_ids(r, &room).filter(|ids| ids.contains(&bob.id))
        })
        .await;
    let lapse_started = Instant::now();
    alice_on_a
        .until(
            WITHIN,
            "bob's typing to lapse in alice's sync on A without a stop",
            |r| typing_user_ids(r, &room).filter(|ids| !ids.contains(&bob.id)),
        )
        .await;
    assert!(
        lapse_started.elapsed() < Duration::from_secs(10),
        "a 1.5 s typing timeout lapsed after {:?}",
        lapse_started.elapsed()
    );

    // ---- Receipts, A -> B: a receipt, then a later one for the same room. ----
    let first = send_message(&client, &a, &room, &alice, "one").await;
    let second = send_message(&client, &a, &room, &alice, "two").await;
    post_receipt(&client, &a, &room, &alice, &first).await;
    bob_on_b
        .until(
            WITHIN,
            "alice's receipt on the first message (set on A) in bob's sync on B",
            |r| receipt_content(r, &room).filter(|c| c[&first]["m.read"][&alice.id].is_object()),
        )
        .await;
    post_receipt(&client, &a, &room, &alice, &second).await;
    let content = bob_on_b
        .until(
            WITHIN,
            "alice's later receipt (set on A) in bob's sync on B, not the cached one",
            |r| receipt_content(r, &room).filter(|c| c[&second]["m.read"][&alice.id].is_object()),
        )
        .await;
    assert!(
        content.get(&first).is_none(),
        "the later receipt replaces the earlier: {content}"
    );

    // ---- Receipts, B -> A. ----
    post_receipt(&client, &b, &room, &bob, &second).await;
    alice_on_a
        .until(
            WITHIN,
            "bob's receipt (set on B) in alice's sync on A",
            |r| receipt_content(r, &room).filter(|c| c[&second]["m.read"][&bob.id].is_object()),
        )
        .await;

    // ---- Presence, A -> B: a change, then another (B had her record cached). ----
    put_presence(&client, &a, &alice, "unavailable", "lunch").await;
    bob_on_b
        .until(
            WITHIN,
            "alice unavailable (set on A) in bob's sync on B",
            |r| {
                presence_from(r, &alice.id)
                    .filter(|c| c["presence"] == "unavailable" && c["status_msg"] == "lunch")
            },
        )
        .await;
    put_presence(&client, &a, &alice, "online", "back").await;
    bob_on_b
        .until(
            WITHIN,
            "alice online again (set on A) in bob's sync on B",
            |r| {
                presence_from(r, &alice.id)
                    .filter(|c| c["presence"] == "online" && c["status_msg"] == "back")
            },
        )
        .await;

    // ---- Presence, B -> A. ----
    put_presence(&client, &b, &bob, "unavailable", "afk").await;
    alice_on_a
        .until(
            WITHIN,
            "bob unavailable (set on B) in alice's sync on A",
            |r| {
                presence_from(r, &bob.id)
                    .filter(|c| c["presence"] == "unavailable" && c["status_msg"] == "afk")
            },
        )
        .await;

    // Nothing is sent twice: with nothing new, the next sync on each side carries none of it.
    let quiet_b = bob_on_b.sync(0).await;
    assert_eq!(typing_user_ids(&quiet_b, &room), None, "{quiet_b}");
    assert_eq!(receipt_content(&quiet_b, &room), None, "{quiet_b}");
    assert_eq!(presence_from(&quiet_b, &alice.id), None, "{quiet_b}");
    let quiet_a = alice_on_a.sync(0).await;
    assert_eq!(typing_user_ids(&quiet_a, &room), None, "{quiet_a}");
    assert_eq!(receipt_content(&quiet_a, &room), None, "{quiet_a}");
    assert_eq!(presence_from(&quiet_a, &bob.id), None, "{quiet_a}");

    // Counted on both sides, and what one sent the other received, kind by kind: a sent
    // update is counted once the peer has answered, so the two agree once things are quiet.
    // Typing and receipts are room requests, forwarded to the room's owner, so only the owner
    // ever sends those two; presence lands wherever it is set, so both do.
    for kind in ["typing", "receipt", "presence"] {
        let sent =
            format!(r#"hs_cluster_ephemeral_updates_total{{kind="{kind}",direction="sent"}}"#);
        let received =
            format!(r#"hs_cluster_ephemeral_updates_total{{kind="{kind}",direction="received"}}"#);
        eventually(
            Duration::from_secs(10),
            &format!("{kind} updates sent by each replica to match those the other received"),
            || {
                let (client, a, b, sent, received) = (&client, &a, &b, &sent, &received);
                async move {
                    let a_sent = metric(client, a, sent).await;
                    let b_received = metric(client, b, received).await;
                    let b_sent = metric(client, b, sent).await;
                    let a_received = metric(client, a, received).await;
                    (a_sent + b_sent > 0 && a_sent == b_received && b_sent == a_received)
                        .then_some(())
                }
            },
        )
        .await;
    }

    let log_b = hs_b.stop();
    let log_a = hs_a.stop();
    for (log, name) in [(&log_a, "A"), (&log_b, "B")] {
        assert!(
            log.contains("typing, receipts and presence cross it"),
            "{name} did not install the cluster-aware sync: {log}"
        );
    }
}
