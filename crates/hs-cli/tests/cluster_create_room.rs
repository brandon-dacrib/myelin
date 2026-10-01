//! A version-12 room is built by the replica that owns its shard, whichever replica took the
//! `/createRoom` (RFC 0019, decision 0020).
//!
//! A version-12 room's id is its create event's hash, so the shard gate cannot choose it ahead
//! of the handler: the gate only picks the replica that runs the handler, and the handler
//! rebuilds the create event until its id hashes to a shard that replica owns. Before that, the
//! room was built, and its first events written, on whichever replica the gate chose, while
//! every later request for it went to the owner of the shard its hash happened to land on.
//!
//! Two replicas (the real `hs` binary, two processes, so that each has its own `/metrics`) share
//! one PostgreSQL. Twenty version-12 rooms are created through each. Each replica's
//! `hs_room_create_room_id_attempts_count` counts the rooms it built; it must equal the number
//! of rooms whose shard it owns, and every room then takes a message through the replica that
//! did not build it.
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise:
//!
//! ```sh
//! docker run --rm -d --name hs-cluster-create-pg -e POSTGRES_PASSWORD=hspg \
//!     -p 127.0.0.1:5439:5432 public.ecr.aws/docker/library/postgres:17
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test -p hs-cli --test cluster_create_room
//! ```

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
                    "SKIP: the two-replica createRoom test needs PostgreSQL at {admin_dsn:?}: {e}"
                );
                return None;
            }
        };
        let name = format!("hs_cluster_create_{}_{}", std::process::id(), rand_suffix());
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

/// One replica: its client listener on `port`, its mesh on `mesh_port`, both named
/// `server_name`.
fn replica_config(
    db: &Database,
    dir: &std::path::Path,
    server_name: &str,
    port: u16,
    mesh_port: u16,
) -> String {
    format!(
        "server:\n  server_name: \"{server_name}\"\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n  pool_size: 8\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         cluster:\n  single_node: false\n  room_shards: 4\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    advertise_address: \"127.0.0.1:{mesh_port}\"\n    shared_secret: cluster-create-room-test-secret\n",
        keys = dir.join("keys"),
        media = dir.join(format!("media-{port}")),
        host = db.host,
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
    )
}

async fn register(client: &reqwest::Client, base: &str, username: &str) -> String {
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
    done["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("registration failed: {done}"))
        .to_owned()
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

/// The room shards' owners (`room/N` to the owner's mesh address), once every shard has one and
/// both replicas own some; `None` before then.
async fn room_shard_owners(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Option<BTreeMap<String, String>> {
    let page: Value = client
        .get(format!("{base}/api/v1/cluster/shards?limit=500"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let items = page["items"].as_array()?;
    if items.iter().any(|s| s["owner"].is_null()) {
        return None;
    }
    let rooms: BTreeMap<String, String> = items
        .iter()
        .filter_map(|s| {
            let id = s["id"].as_str()?;
            let owner = s["owner"].as_str()?;
            id.starts_with("room/")
                .then(|| (id.to_owned(), owner.to_owned()))
        })
        .collect();
    let owners: std::collections::BTreeSet<&String> = rooms.values().collect();
    (owners.len() == 2).then_some(rooms)
}

const SAMPLE: &str = "hs_room_create_room_id_attempts_count";
const ROOMS_PER_REPLICA: usize = 20;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_v12_room_is_built_by_the_owner_of_its_shard_whichever_replica_took_the_request() {
    let Some(db) = Database::create() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let (port_1, port_2) = (reserve_port(), reserve_port());
    let (mesh_1, mesh_2) = (reserve_port(), reserve_port());
    let name = format!("127.0.0.1:{port_1}");

    // Replica 1 first (it makes the signing key and the schema), then replica 2.
    let config_1 = dir.path().join("1.yaml");
    let config_2 = dir.path().join("2.yaml");
    std::fs::write(
        &config_1,
        replica_config(&db, dir.path(), &name, port_1, mesh_1),
    )
    .unwrap();
    std::fs::write(
        &config_2,
        replica_config(&db, dir.path(), &name, port_2, mesh_2),
    )
    .unwrap();
    let mut replica_1 = HsProcess::serve(&config_1);
    let setup_line = replica_1.wait_for("setup_link=");
    let mut replica_2 = HsProcess::serve(&config_2);
    replica_2.wait_for("listening");
    let replicas = [
        (
            format!("127.0.0.1:{mesh_1}"),
            format!("http://127.0.0.1:{port_1}"),
        ),
        (
            format!("127.0.0.1:{mesh_2}"),
            format!("http://127.0.0.1:{port_2}"),
        ),
    ];

    let setup_token: String = setup_line
        .split_once("#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let admin: Value = client
        .post(format!("{}/api/v1/setup", replicas[0].1))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_token = admin["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("setup failed: {admin}"))
        .to_owned();

    // Both replicas own room shards, and the map stays put for a few heartbeats.
    let deadline = Instant::now() + Duration::from_secs(60);
    let owners = loop {
        if Instant::now() >= deadline {
            let page: Value = client
                .get(format!("{}/api/v1/cluster/shards?limit=500", replicas[0].1))
                .bearer_auth(&admin_token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            panic!("two replicas never settled sharing the room shards: {page}");
        }
        if let Some(first) = room_shard_owners(&client, &replicas[0].1, &admin_token).await {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if room_shard_owners(&client, &replicas[0].1, &admin_token).await == Some(first.clone())
            {
                break first;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let alice = register(&client, &replicas[0].1, "alice").await;
    let mut before = Vec::new();
    for (_, base) in &replicas {
        before.push(metric(&client, base, SAMPLE).await);
    }

    let layout = hs_cluster::ShardLayout {
        rooms: 4,
        users: 4,
        ..hs_cluster::ShardLayout::default()
    };
    let mut rooms: Vec<(String, String)> = Vec::new();
    for (_, base) in &replicas {
        for _ in 0..ROOMS_PER_REPLICA {
            let created: Value = client
                .post(format!("{base}/_matrix/client/v3/createRoom"))
                .bearer_auth(&alice)
                .json(&json!({"room_version": "12", "preset": "private_chat"}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let room_id = created["room_id"]
                .as_str()
                .unwrap_or_else(|| panic!("createRoom failed: {created}"))
                .to_owned();
            let shard = format!("room/{}", layout.room_shard(&room_id).index);
            rooms.push((room_id, owners[&shard].clone()));
        }
    }
    assert_eq!(
        room_shard_owners(&client, &replicas[0].1, &admin_token).await,
        Some(owners.clone()),
        "the shard map moved while the rooms were created; the counts below would not mean much"
    );

    // Each replica built exactly the rooms whose shard it owns.
    for (i, (mesh, base)) in replicas.iter().enumerate() {
        let built = metric(&client, base, SAMPLE).await - before[i];
        let owned = rooms.iter().filter(|(_, owner)| owner == mesh).count() as u64;
        assert_eq!(
            built,
            owned,
            "replica {mesh} built {built} rooms but owns the shards of {owned} of the {}",
            rooms.len()
        );
    }

    // And each room takes a message through the replica that did not build it.
    for (n, (room_id, owner)) in rooms.iter().enumerate() {
        let other = &replicas
            .iter()
            .find(|(mesh, _)| mesh != owner)
            .expect("two replicas")
            .1;
        // The server-wide message rate limit applies; a `429` is waited out as a client would.
        let status = loop {
            let response = client
                .put(format!(
                    "{other}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/t{n}"
                ))
                .bearer_auth(&alice)
                .json(&json!({"msgtype": "m.text", "body": format!("message {n}")}))
                .send()
                .await
                .unwrap();
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                let status = response.status();
                break (status, response.text().await.unwrap_or_default());
            }
            let body: Value = response.json().await.unwrap_or_default();
            let wait = body["retry_after_ms"].as_u64().unwrap_or(500);
            tokio::time::sleep(Duration::from_millis(wait.max(50))).await;
        };
        assert!(
            status.0.is_success(),
            "{room_id}: {} {}",
            status.0,
            status.1
        );
    }

    drop(replica_2);
    drop(replica_1);
}
