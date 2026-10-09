//! A replica that loses a room's shard and gets it back must not write from the copy it kept
//! (the scale 1 -> 2 bug the cluster smoke found on 2026-10-09; status 05 of that date).
//!
//! Two real `hs serve` replicas on one PostgreSQL, the sequence `replicaCount` 3 -> 1 -> 2 in
//! miniature: a room B owns, B stopped (A takes the room and loads it), B back (B takes the
//! room back and writes to it), B stopped again (A takes the room back). A then writes to the
//! room. Before the fix the resident copy A loaded the first time was reused as it was: its
//! timeline head was two events behind the store, so its write landed on the position of an
//! event B had written, and the next replica to load the room from the store found an
//! extremity the timeline no longer held (`/sync` 500 `unknown event EventSn#...`, sends 500
//! `cited event not in history`). Now the registry notices the shard changed hands since the
//! copy was loaded (the fencing epoch moved) and loads the room again from the store.
//!
//! Runs when a PostgreSQL server is reachable (`HS_CLUSTER_TEST_POSTGRES_DSN`), and prints a
//! skip message otherwise. Each run makes a database of its own and drops it after.

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
                if line.contains(" ERROR ") || line.contains(" WARN ") {
                    eprintln!("hs: {line}");
                }
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
        let deadline = Instant::now() + Duration::from_secs(300);
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

    /// Stops it as Kubernetes would (`SIGTERM`: a drain, the shards handed to the other
    /// replica), waits, and returns its log.
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
                eprintln!("SKIP: the rejoin test needs PostgreSQL at {admin_dsn:?}: {e}");
                return None;
            }
        };
        let name = format!("hs_cluster_rejoin_{}_{}", std::process::id(), rand_suffix());
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

/// Room shards in the test cluster: few, so a room lands on either replica within a couple of
/// tries.
const ROOM_SHARDS: u32 = 4;

/// One replica's configuration: short leases, so a stopped replica's shards move within
/// seconds, and a small pool, since the test server allows a hundred connections.
fn replica_config(db: &Database, dir: &std::path::Path, port: u16, mesh_port: u16) -> String {
    format!(
        "server:\n  server_name: cluster.example.org\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n  pool_size: 8\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n\
         cluster:\n  single_node: false\n  room_shards: {ROOM_SHARDS}\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    shared_secret: cluster-rejoin-test-secret\n",
        keys = dir.join("keys"),
        media = dir.join(format!("media-{port}")),
        host = db.host,
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
    )
}

#[derive(Clone)]
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

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('$', "%24")
}

async fn create_room(client: &reqwest::Client, base: &str, user: &User, name: &str) -> String {
    let created: Value = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(&user.token)
        .json(&json!({"preset": "public_chat", "name": name}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    created["room_id"]
        .as_str()
        .unwrap_or_else(|| panic!("createRoom failed: {created}"))
        .to_owned()
}

/// The replica that owns `room`'s shard right now, as `/api/v1/cluster/shards` says.
async fn owner_of(client: &reqwest::Client, base: &str, admin: &str, room: &str) -> Option<String> {
    let layout = hs_cluster::ShardLayout {
        rooms: ROOM_SHARDS,
        ..hs_cluster::ShardLayout::default()
    };
    let shard = layout.room_shard(room).to_string();
    let shards: Value = client
        .get(format!("{base}/api/v1/cluster/shards?limit=500"))
        .bearer_auth(admin)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    shards["items"]
        .as_array()?
        .iter()
        .find(|s| s["id"] == shard.as_str())?["owner"]
        .as_str()
        .map(str::to_owned)
}

/// Waits until `owner` owns `room`'s shard, as `base`'s admin API says.
async fn wait_for_owner(
    client: &reqwest::Client,
    base: &str,
    admin: &str,
    room: &str,
    owner: &str,
) {
    eventually(
        Duration::from_secs(90),
        &format!("{owner} to own {room}"),
        || async {
            (owner_of(client, base, admin, room).await.as_deref() == Some(owner)).then_some(())
        },
    )
    .await;
}

async fn join(client: &reqwest::Client, base: &str, room: &str, user: &User) {
    let joined = client
        .post(format!(
            "{base}/_matrix/client/v3/rooms/{}/join",
            escape(room)
        ))
        .bearer_auth(&user.token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = joined.status();
    let body: Value = joined.json().await.unwrap_or_default();
    assert!(
        status.is_success() && body["room_id"] == room,
        "{} joining {room} through {base}: {status} {body}",
        user.id
    );
}

/// Sends `body` through `base`, retrying a shard mid-handoff, and returns the event id.
async fn send_message(
    client: &reqwest::Client,
    base: &str,
    room: &str,
    user: &User,
    body: &str,
) -> String {
    // One transaction id for every try: a retry of a send that did land returns its event.
    let txn = rand_suffix();
    let mut tries = 0;
    loop {
        let response: Value = client
            .put(format!(
                "{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/{txn}"
            ))
            .bearer_auth(&user.token)
            .json(&json!({"msgtype": "m.text", "body": body}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(event_id) = response["event_id"].as_str() {
            return event_id.to_owned();
        }
        tries += 1;
        let text = response.to_string();
        let transient = text.contains("fenced")
            || text.contains("M_HS_NOT_SHARD_OWNER")
            || text.contains("no owner is currently known")
            || text.contains("M_LIMIT_EXCEEDED");
        assert!(
            transient && tries < 20,
            "send {body:?} through {base} failed: {response}"
        );
        tokio::time::sleep(Duration::from_millis(250 * tries)).await;
    }
}

/// The bodies of the room's messages, newest first, as `base` answers `/messages` (with the
/// status and body, so a failure says what the server said).
async fn message_bodies(
    client: &reqwest::Client,
    base: &str,
    room: &str,
    user: &User,
) -> (reqwest::StatusCode, Value, Vec<String>) {
    let response = client
        .get(format!(
            "{base}/_matrix/client/v3/rooms/{room}/messages?dir=b&limit=50"
        ))
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    let bodies = body["chunk"]
        .as_array()
        .map(|chunk| {
            chunk
                .iter()
                .filter(|e| e["type"] == "m.room.message")
                .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (status, body, bodies)
}

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

/// The hub's line when a room update cannot be turned into feeds: what the smoke saw on both
/// pods once the room's rows had been written from a stale copy.
const HUB_FAILURE: &str = "failed to process a room update into user feeds";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replica_that_gets_a_room_back_writes_from_the_store_not_its_old_copy() {
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

    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &a, "bob").await;

    // A room B owns, with both of them in it. B builds it; A has never loaded it.
    let room = eventually(Duration::from_secs(90), "a room on B", || async {
        // Until both replicas own shards, a room may land nowhere; try again.
        let room = create_room(&client, &a, &alice, "rejoin").await;
        (owner_of(&client, &a, &admin, &room).await.as_deref() == Some(id_b.as_str()))
            .then_some(room)
    })
    .await;
    let room_path = escape(&room);
    join(&client, &a, &room, &bob).await;

    // 1. B goes (a drain, as a scale-down is). A takes the room over and loads it from the
    //    store to write the first message: A now holds a resident copy whose head is m1.
    let log_b1 = hs_b.stop();
    wait_for_owner(&client, &a, &admin, &room, &id_a).await;
    send_message(&client, &a, &room_path, &alice, "m1 (A, first time)").await;

    // 2. B comes back and takes the room back. B loads the room from the store and writes two
    //    messages; A's copy is now two events behind the store, and nothing tells it.
    let mut hs_b = HsProcess::serve(&config_b);
    hs_b.wait_for("listening");
    wait_for_owner(&client, &a, &admin, &room, &id_b).await;
    send_message(&client, &b, &room_path, &alice, "m2 (B)").await;
    send_message(&client, &b, &room_path, &alice, "m3 (B)").await;

    // 3. B goes again; A takes the room back. The write that follows is the one that used to
    //    come from the stale copy: it landed on m2's timeline position and left m3 citing an
    //    event the timeline no longer held.
    let log_b2 = hs_b.stop();
    wait_for_owner(&client, &a, &admin, &room, &id_a).await;
    send_message(&client, &a, &room_path, &alice, "m4 (A, again)").await;

    // 4. B rejoins and loads the room from the store, as hs-1 did in the smoke. Everything a
    //    client reads of the room must still work, through either replica, and carry all four
    //    messages.
    let mut hs_b = HsProcess::serve(&config_b);
    hs_b.wait_for("listening");
    wait_for_owner(&client, &a, &admin, &room, &id_b).await;

    for base in [&b, &a] {
        let (status, body, bodies) = message_bodies(&client, base, &room_path, &bob).await;
        assert_eq!(status, 200, "/messages through {base}: {body}");
        let expected = ["m4 (A, again)", "m3 (B)", "m2 (B)", "m1 (A, first time)"];
        assert_eq!(
            bodies, expected,
            "the room's messages through {base} (the store was written from a stale copy)"
        );

        let members = client
            .get(format!(
                "{base}/_matrix/client/v3/rooms/{room_path}/members"
            ))
            .bearer_auth(&bob.token)
            .send()
            .await
            .unwrap();
        let status = members.status();
        let body: Value = members.json().await.unwrap_or_default();
        assert_eq!(status, 200, "/members through {base}: {body}");

        let sync = client
            .get(format!("{base}/_matrix/client/v3/sync?timeout=0"))
            .bearer_auth(&bob.token)
            .send()
            .await
            .unwrap();
        let status = sync.status();
        let body: Value = sync.json().await.unwrap_or_default();
        assert_eq!(status, 200, "/sync through {base}: {body}");
        assert!(
            body["rooms"]["join"][&room].is_object(),
            "bob's /sync through {base} does not show the room: {body}"
        );
    }

    // And a send after the rejoin, on the new owner, still works: the extremities it cites
    // are all in the history it loaded.
    send_message(&client, &b, &room_path, &bob, "m5 (B, after the rejoin)").await;

    // The logs: no hub failure on either replica, and A said why it loaded the room again.
    let log_b3 = hs_b.stop();
    let log_a = hs_a.stop();
    for (name, log) in [
        ("A", &log_a),
        ("B1", &log_b1),
        ("B2", &log_b2),
        ("B3", &log_b3),
    ] {
        assert!(
            !log.contains(HUB_FAILURE),
            "{name}'s log has the hub failure the smoke saw:\n{}",
            log.lines()
                .filter(|l| l.contains(HUB_FAILURE))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    assert!(
        log_a.contains("changed hands since this copy was loaded"),
        "A never said it dropped its stale copy of the room; its log:\n{log_a}"
    );
}
