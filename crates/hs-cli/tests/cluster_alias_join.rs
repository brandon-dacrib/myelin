//! A join or knock by room alias through the replica that does not own the room: two real
//! `hs serve` processes on one PostgreSQL. The shard gate resolves the alias before it decides
//! (`hs_cli::cluster::AliasResolver`), so the request is forwarded to the room's owner like a
//! join by id -- and the owner's own room actor, which was already resident, has the member.
//!
//! Until it did, the alias was resolved inside the handler on whichever replica took the request:
//! a non-owner built a room actor of its own for a room it does not own and wrote the join
//! there, behind the owner's back; the owner's resident actor never learned of the member, and
//! the member's next message, forwarded to the owner by id, was refused.
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise:
//!
//! ```sh
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5462/postgres" \
//!     cargo test -p hs-cli --test cluster_alias_join
//! ```
//!
//! Each run makes a database of its own and drops it after.

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
        self.wait_for_all(&[needle])
    }

    /// The first line, read so far or from here on, that contains every one of `needles`.
    fn wait_for_all(&mut self, needles: &[&str]) -> String {
        let matches = |line: &str| needles.iter().all(|n| line.contains(n));
        if let Some(line) = self.seen.iter().find(|l| matches(l)) {
            return line.clone();
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if matches(&line) {
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
                    "SKIP: the two-replica alias-join test needs PostgreSQL at {admin_dsn:?}: {e}"
                );
                return None;
            }
        };
        let name = format!(
            "hs_cluster_alias_join_{}_{}",
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

const ROOM_SHARDS: u32 = 4;

/// One replica's configuration: a small pool, since the test server allows a hundred
/// connections and other tests may be running.
fn replica_config(db: &Database, dir: &std::path::Path, port: u16, mesh_port: u16) -> String {
    format!(
        "server:\n  server_name: cluster.example.org\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n  pool_size: 6\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         cluster:\n  single_node: false\n  room_shards: {ROOM_SHARDS}\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    shared_secret: cluster-alias-join-test-secret\n",
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

async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let mut request = client.request(method, url).bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Requests this replica forwarded over the mesh and got an answer to, from its `/metrics`.
async fn forwarded(client: &reqwest::Client, base: &str) -> u64 {
    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .filter(|l| {
            l.starts_with("hs_cluster_forward_latency_seconds_count{")
                && l.contains("route=\"forward\"")
                && l.contains("outcome=\"ok\"")
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
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

/// The replica (its base URL and mesh id) that owns `room`'s shard, as `admin_base` reports.
async fn owner_of(
    client: &reqwest::Client,
    admin_base: &str,
    admin: &str,
    room: &str,
) -> Option<String> {
    let shard = hs_cluster::ShardLayout {
        rooms: ROOM_SHARDS,
        ..hs_cluster::ShardLayout::default()
    }
    .room_shard(room)
    .to_string();
    let (_, shards) = call(
        client,
        reqwest::Method::GET,
        format!("{admin_base}/api/v1/cluster/shards?limit=500"),
        admin,
        None,
    )
    .await;
    shards["items"]
        .as_array()?
        .iter()
        .find(|s| s["id"] == shard.as_str())?["owner"]
        .as_str()
        .map(str::to_owned)
}

fn encoded(alias: &str) -> String {
    alias.replace('#', "%23").replace(':', "%3A")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_and_a_knock_by_alias_through_a_non_owner_reach_the_rooms_owner() {
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

    let mut hs_a = HsProcess::serve(&config_a);
    let setup_line = hs_a.wait_for("setup_link=");
    let admin = first_admin(&client, &a, &setup_line).await;
    let mut hs_b = HsProcess::serve(&config_b);
    hs_b.wait_for("listening");

    // Both replicas are active and own shards, and every shard is owned. (Which replica owns
    // the rooms below does not matter: each request goes through the other one.)
    eventually(
        Duration::from_secs(120),
        "two active replicas sharing every shard",
        || {
            let (client, a, admin, id_a, id_b) = (&client, &a, &admin, &id_a, &id_b);
            async move {
                let (_, replicas) = call(
                    client,
                    reqwest::Method::GET,
                    format!("{a}/api/v1/cluster/replicas"),
                    admin,
                    None,
                )
                .await;
                let items = replicas["items"].as_array()?;
                let both = [id_a, id_b].iter().all(|id| {
                    items.iter().any(|r| {
                        r["id"] == id.as_str()
                            && r["status"] == "active"
                            && r["shard_count"].as_u64() > Some(0)
                    })
                });
                let (_, shards) = call(
                    client,
                    reqwest::Method::GET,
                    format!("{a}/api/v1/cluster/shards?limit=500"),
                    admin,
                    None,
                )
                .await;
                let all_owned = shards["items"]
                    .as_array()?
                    .iter()
                    .all(|s| !s["owner"].is_null());
                (both && all_owned).then_some(())
            }
        },
    )
    .await;

    let alice = register(&client, &a, "alice").await;
    let base_of = |id: &str| if id == id_a { a.clone() } else { b.clone() };
    let other_than = |id: &str| if id == id_a { b.clone() } else { a.clone() };

    for (alias_name, join_rule, verb) in [("lobby", "public", "join"), ("door", "knock", "knock")] {
        // Alice makes the room through A; whichever replica owns it, it is resident there now.
        let (status, created) = call(
            &client,
            reqwest::Method::POST,
            format!("{a}/_matrix/client/v3/createRoom"),
            &alice.token,
            Some(json!({
                "room_alias_name": alias_name,
                "initial_state": [{"type": "m.room.join_rules", "state_key": "", "content": {"join_rule": join_rule}}],
            })),
        )
        .await;
        assert!(status.is_success(), "createRoom {alias_name}: {created}");
        let room = created["room_id"].as_str().unwrap().to_owned();
        let owner = owner_of(&client, &a, &admin, &room)
            .await
            .expect("the room's shard has an owner");
        let (owner_base, other) = (base_of(&owner), other_than(&owner));

        // Bob, on the replica that does not own the room, by alias.
        let bob = register(&client, &other, &format!("bob_{alias_name}")).await;
        let before = forwarded(&client, &other).await;
        let alias = format!("#{alias_name}:cluster.example.org");
        let (status, answered) = call(
            &client,
            reqwest::Method::POST,
            format!("{other}/_matrix/client/v3/{verb}/{}", encoded(&alias)),
            &bob.token,
            Some(json!({})),
        )
        .await;
        assert!(
            status.is_success(),
            "{verb} {alias} through {other}: {answered}"
        );
        assert_eq!(answered["room_id"], room.as_str(), "{answered}");
        assert!(
            forwarded(&client, &other).await > before,
            "the {verb} by alias was forwarded to the room's owner ({owner})"
        );
        let non_owner = if other == a { &mut hs_a } else { &mut hs_b };
        // And said so in its log.
        let line = non_owner.wait_for_all(&["ahead of the shard gate", &alias]);
        assert!(
            line.contains("false"),
            "resolved on a replica that does not own it: {line}"
        );

        // The owner's resident room actor has bob in the state it serves.
        let (status, member) = call(
            &client,
            reqwest::Method::GET,
            format!(
                "{owner_base}/_matrix/client/v3/rooms/{room}/state/m.room.member/{}",
                bob.id
            ),
            &alice.token,
            None,
        )
        .await;
        assert_eq!(status, 200, "bob's membership on the owner: {member}");
        assert_eq!(member["membership"], verb, "{member}");

        if verb == "join" {
            // And bob's next request -- forwarded by id, as every room request from the
            // non-owner is -- is taken.
            let (status, sent) = call(
                &client,
                reqwest::Method::PUT,
                format!("{other}/_matrix/client/v3/rooms/{room}/send/m.room.message/t1"),
                &bob.token,
                Some(json!({"msgtype": "m.text", "body": "joined by alias"})),
            )
            .await;
            assert_eq!(status, 200, "bob's message after the join: {sent}");
        }
    }
}
