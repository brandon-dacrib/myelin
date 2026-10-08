//! The device-list announcer's place in the device-list stream is kept per federation shard,
//! by whichever replica owns the shard, so a change no replica could announce when it was made
//! reaches the destination once the shard has an owner again.
//!
//! Server A is two replicas on one PostgreSQL: replica 1 in this process, replica 2 the real `hs`
//! binary, so it can be killed. Server B is one embedded server whose federation shard replica
//! 2 owns (B's port is picked so rendezvous hashing gives it to replica 2). alice of A and bob of
//! B share a room. Replica 2 is killed (`SIGKILL`, no handoff: its leases run out on their own),
//! and at once alice signs in on a new device through replica 1: a device-list change. Replica 1
//! reads it straight away, while B's shard is still replica 2's, so it cannot send to B. When
//! the shard's lease runs out replica 1 takes it, starts from the shard's stored place, and
//! announces the change, so bob's `/sync` lists alice in `device_lists.changed`. With one place
//! for the whole cluster (before 2026-10-08), replica 1 read past the change and stored where it
//! got to; B was never told.
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise:
//!
//! ```sh
//! docker run --rm -d --name hs-cluster-device-lists-pg -e POSTGRES_PASSWORD=hspg \
//!     -p 127.0.0.1:5439:5432 postgres:17
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test -p hs-cli --test cluster_device_lists
//! ```

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
                    "SKIP: the two-replica device-list test needs PostgreSQL at {admin_dsn:?}: {e}\n\
                     Start one with: docker run --rm -d --name hs-cluster-edus-pg \
                     -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17"
                );
                return None;
            }
        };
        let name = format!(
            "hs_cluster_device_lists_{}_{}",
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

/// One replica of A: its client/federation listener on `port`, its mesh on `mesh_port`, all of
/// them named `server_name`.
fn replica_config(
    db: &Database,
    dir: &std::path::Path,
    server_name: &str,
    port: u16,
    mesh_port: u16,
) -> hs_config::Config {
    hs_config::Config::from_yaml(&replica_yaml(db, dir, server_name, port, mesh_port))
        .expect("the replica configuration parses")
}

/// [`replica_config`] as the YAML a process is started with.
fn replica_yaml(
    db: &Database,
    dir: &std::path::Path,
    server_name: &str,
    port: u16,
    mesh_port: u16,
) -> String {
    format!(
        "server:\n  server_name: \"{server_name}\"\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n  max_retry_backoff: 2s\n\
         cluster:\n  single_node: false\n  room_shards: 4\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    advertise_address: \"127.0.0.1:{mesh_port}\"\n    shared_secret: cluster-device-lists-secret\n",
        keys = dir.join("keys"),
        media = dir.join(format!("media-{port}")),
        host = db.host,
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
    )
}

/// Server B: one embedded server.
fn single_config(port: u16, dir: &std::path::Path) -> hs_config::Config {
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n",
        data = dir.join("data"),
        media = dir.join("media"),
    );
    hs_config::Config::from_yaml(&yaml).expect("the configuration parses")
}

async fn serve(config: hs_config::Config) -> hs_cli::serve::ServeHandle {
    hs_cli::serve::spawn_serve(
        config,
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots")
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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Replica 2, as the real binary: killed (`SIGKILL`) when dropped, or earlier by the test.
struct HsProcess(std::process::Child);

impl HsProcess {
    fn serve(config_path: &std::path::Path, log: &std::path::Path) -> Self {
        let log = std::fs::File::create(log).unwrap();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("the hs binary should start");
        Self(child)
    }

    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The shard map, as `(shard, owner)`, from the admin API at `base`.
async fn shard_owners(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Option<Vec<(String, Option<String>)>> {
    let page: Value = client
        .get(format!("{base}/api/v1/cluster/shards?limit=500"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        page["items"]
            .as_array()?
            .iter()
            .map(|s| {
                (
                    s["id"].as_str().unwrap_or_default().to_owned(),
                    s["owner"].as_str().map(str::to_owned),
                )
            })
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_list_change_made_while_its_destinations_shard_had_no_live_owner_is_announced() {
    let Some(db) = Database::create() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let (port_1, port_2) = (reserve_port(), reserve_port());
    let a_name = format!("127.0.0.1:{port_1}");
    let layout = hs_cluster::ShardLayout {
        rooms: 4,
        users: 4,
        ..hs_cluster::ShardLayout::default()
    };
    // Mesh ports (the replicas' identities) such that replica 1, the one that lives, is the
    // rendezvous owner of at least one user shard and one room shard: alice and her room go
    // there.
    let mesh_1 = reserve_port();
    let replica_1_id = hs_cluster::ReplicaId::new(format!("127.0.0.1:{mesh_1}"));
    let (mesh_2, replica_2_id) = loop {
        let port = reserve_port();
        let id = hs_cluster::ReplicaId::new(format!("127.0.0.1:{port}"));
        let ones = |kind| {
            (0..4).any(|index| {
                hs_cluster::hash::desired_owner(
                    hs_cluster::ShardId::new(kind, index),
                    [&replica_1_id, &id],
                ) == Some(&replica_1_id)
            })
        };
        if ones(hs_cluster::ShardKind::User) && ones(hs_cluster::ShardKind::Room) {
            break (port, id);
        }
    };
    // B's port, such that its federation shard is replica 2's.
    let (port_b, b_shard) = loop {
        let port = reserve_port();
        let shard = layout.federation_shard(&format!("127.0.0.1:{port}"));
        if hs_cluster::hash::desired_owner(shard, [&replica_1_id, &replica_2_id])
            == Some(&replica_2_id)
        {
            break (port, format!("federation/{}", shard.index));
        }
    };

    // Replica 1 first (it makes the signing key and the schema), then replica 2.
    let replica_1 = serve(replica_config(&db, dir.path(), &a_name, port_1, mesh_1)).await;
    let setup_link = replica_1
        .setup_link
        .clone()
        .expect("a fresh server offers a setup link");
    let config_2 = dir.path().join("replica-2.yaml");
    std::fs::write(
        &config_2,
        replica_yaml(&db, dir.path(), &a_name, port_2, mesh_2),
    )
    .unwrap();
    let mut replica_2 = HsProcess::serve(&config_2, &dir.path().join("replica-2.log"));
    let b_dir = tempfile::tempdir().unwrap();
    let b = serve(single_config(port_b, b_dir.path())).await;
    let base_1 = replica_1.base_url();

    let setup_token: String = setup_link
        .split_once("#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let admin: Value = client
        .post(format!("{base_1}/api/v1/setup"))
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

    // Both replicas serving, every shard owned, and B's federation shard replica 2's.
    eventually(
        Duration::from_secs(120),
        "replica 2 up and owning B's federation shard",
        || {
            let (client, base, token, b_shard, two) = (
                &client,
                &base_1,
                &admin_token,
                &b_shard,
                replica_2_id.as_str(),
            );
            async move {
                let owners = shard_owners(client, base, token).await?;
                (owners.iter().all(|(_, owner)| owner.is_some())
                    && owners
                        .iter()
                        .any(|(id, owner)| id == b_shard && owner.as_deref() == Some(two)))
                .then_some(())
            }
        },
    )
    .await;

    eprintln!("replica 2 owns B's federation shard");
    // alice (A, through replica 1, whose user shard is replica 1's so that her sign-in after
    // the kill needs nothing of replica 2) and bob (B) share a room.
    let alice_name = (0..1000)
        .map(|i| format!("alice{i}"))
        .find(|name| {
            let shard = layout.user_shard(&format!("@{name}:{a_name}"));
            hs_cluster::hash::desired_owner(shard, [&replica_1_id, &replica_2_id])
                == Some(&replica_1_id)
        })
        .unwrap();
    let alice = register(&client, &base_1, &alice_name).await;
    let bob = register(&client, &b.base_url(), "bob").await;
    // A room whose shard is replica 1's. B's join reaches replica 1 either way, and would be
    // forwarded to replica 2 if the room were its (decision 0035); but replica 2 is the binary,
    // which fetches B's key over HTTPS only and so could not verify B's signature on it.
    let mut room_id = String::new();
    for _ in 0..60 {
        let created: Value = client
            .post(format!("{base_1}/_matrix/client/v3/createRoom"))
            .bearer_auth(&alice.token)
            .json(&json!({"preset": "public_chat"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["room_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no room: {created}"))
            .to_owned();
        if hs_cluster::hash::desired_owner(layout.room_shard(&id), [&replica_1_id, &replica_2_id])
            == Some(&replica_1_id)
        {
            room_id = id;
            break;
        }
    }
    assert!(!room_id.is_empty(), "no room landed on replica 1's shard");
    let joined = client
        .post(format!(
            "{}/_matrix/client/v3/join/{room_id}?server_name={a_name}",
            b.base_url()
        ))
        .bearer_auth(&bob.token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(
        joined.status().is_success(),
        "{}",
        joined.text().await.unwrap()
    );
    eprintln!("bob joined");

    // bob's sync position once the join has settled: what comes after it is new.
    let mut since = eventually(
        Duration::from_secs(30),
        "bob's sync showing the room",
        || {
            let (client, base, token, room_id) = (&client, b.base_url(), &bob.token, &room_id);
            async move {
                let sync: Value = client
                    .get(format!("{base}/_matrix/client/v3/sync?timeout=0"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?;
                sync["rooms"]["join"]
                    .get(room_id.as_str())
                    .and(sync["next_batch"].as_str().map(str::to_owned))
            }
        },
    )
    .await;
    // Anything the join itself brought is read now: the position after it is what counts.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let settled: Value = client
        .get(format!(
            "{}/_matrix/client/v3/sync?timeout=0&since={since}",
            b.base_url()
        ))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    since = settled["next_batch"].as_str().unwrap().to_owned();
    eprintln!("bob is in the room; killing replica 2");

    // Replica 2 dies holding B's shard; alice signs in on a new device through replica 1 at once.
    replica_2.kill();
    let login: Value = client
        .post(format!("{base_1}/_matrix/client/v3/login"))
        .json(&json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": alice_name},
            "password": "correct horse",
            "device_id": "NEWDEVICE",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(login["device_id"], "NEWDEVICE", "{login}");

    // Once replica 1 has B's shard, bob is told alice's devices changed.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            Instant::now() < deadline,
            "bob was never told alice's devices changed; replica 1 owns B's shard: {:?}",
            shard_owners(&client, &base_1, &admin_token)
                .await
                .and_then(|owners| owners.into_iter().find(|(id, _)| *id == b_shard))
        );
        let sync: Value = client
            .get(format!(
                "{}/_matrix/client/v3/sync?timeout=1000&since={since}",
                b.base_url()
            ))
            .bearer_auth(&bob.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let changed = sync["device_lists"]["changed"]
            .as_array()
            .is_some_and(|users| users.iter().any(|u| u == alice.id.as_str()));
        if changed {
            break;
        }
        since = sync["next_batch"].as_str().unwrap().to_owned();
    }
    // And it was the catch-up that told it.
    let caught_up = metric(
        &client,
        &base_1,
        "hs_federation_device_list_catch_ups_total",
    )
    .await;
    assert!(caught_up >= 1, "no catch-up was counted");

    b.shutdown().await;
    replica_1.shutdown().await;
}
