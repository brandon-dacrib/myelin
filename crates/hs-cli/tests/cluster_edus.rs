//! EDUs in a cluster reach other servers from whichever replica took the request.
//!
//! Server A is two replicas (`hs serve`, in process) on one PostgreSQL; server B is one
//! embedded server. Which replica of A sends to B is decided by the owner of B's federation
//! shard. A to-device message sent to bob on B through the *other* replica is an EDU that
//! replica may not send itself: it hands it over the mesh to the owner
//! (`hs_cli::edu_forward`), which sends it. Before that existed it was dropped, and bob never
//! got the message. Both replicas' `/metrics` say what happened
//! (`hs_federation_edus_forwarded_total`).
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise:
//!
//! ```sh
//! docker run --rm -d --name hs-cluster-edus-pg -e POSTGRES_PASSWORD=hspg \
//!     -p 127.0.0.1:5439:5432 postgres:17
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test -p hs-cli --test cluster_edus
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
                    "SKIP: the two-replica EDU test needs PostgreSQL at {admin_dsn:?}: {e}\n\
                     Start one with: docker run --rm -d --name hs-cluster-edus-pg \
                     -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17"
                );
                return None;
            }
        };
        let name = format!("hs_cluster_edus_{}_{}", std::process::id(), rand_suffix());
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
    let yaml = format!(
        "server:\n  server_name: \"{server_name}\"\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n\
         cluster:\n  single_node: false\n  room_shards: 4\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    advertise_address: \"127.0.0.1:{mesh_port}\"\n    shared_secret: cluster-edus-test-secret\n",
        keys = dir.join("keys"),
        media = dir.join(format!("media-{port}")),
        host = db.host,
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
    );
    hs_config::Config::from_yaml(&yaml).expect("the replica configuration parses")
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
    device: String,
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
        device: done["device_id"].as_str().unwrap().to_owned(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_to_device_message_sent_through_a_replica_that_does_not_send_for_its_destination_arrives()
{
    let Some(db) = Database::create() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let (port_1, port_2, port_b) = (reserve_port(), reserve_port(), reserve_port());
    let (mesh_1, mesh_2) = (reserve_port(), reserve_port());
    let a_name = format!("127.0.0.1:{port_1}");
    let b_name = format!("127.0.0.1:{port_b}");

    // Replica 1 first (it makes the signing key and the schema), then replica 2.
    let replica_1 = serve(replica_config(&db, dir.path(), &a_name, port_1, mesh_1)).await;
    let setup_link = replica_1
        .setup_link
        .clone()
        .expect("a fresh server offers a setup link");
    let replica_2 = serve(replica_config(&db, dir.path(), &a_name, port_2, mesh_2)).await;
    let b_dir = tempfile::tempdir().unwrap();
    let b = serve(single_config(port_b, b_dir.path())).await;
    let bases = [
        (format!("127.0.0.1:{mesh_1}"), replica_1.base_url()),
        (format!("127.0.0.1:{mesh_2}"), replica_2.base_url()),
    ];

    // An administrator, to read the shard map.
    let setup_token: String = setup_link
        .split_once("#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let admin: Value = client
        .post(format!("{}/api/v1/setup", bases[0].1))
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

    // Both replicas own shards, every shard is owned, and B's federation shard stays with one
    // owner for a while: that is the replica that sends to B, and the other one does not.
    let layout = hs_cluster::ShardLayout {
        rooms: 4,
        users: 4,
        ..hs_cluster::ShardLayout::default()
    };
    let b_shard = format!("federation/{}", layout.federation_shard(&b_name).index);
    let owner_of_b = eventually(
        Duration::from_secs(60),
        "two replicas sharing every shard, B's federation shard settled",
        || {
            let (client, base, token, b_shard) = (&client, &bases[0].1, &admin_token, &b_shard);
            async move {
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
                let owners: std::collections::BTreeSet<&str> =
                    items.iter().filter_map(|s| s["owner"].as_str()).collect();
                if owners.len() < 2 {
                    return None;
                }
                items.iter().find(|s| s["id"] == b_shard.as_str())?["owner"]
                    .as_str()
                    .map(str::to_owned)
            }
        },
    )
    .await;
    // Settled: the same owner a few heartbeats later.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (sender_base, other_base) = if owner_of_b == bases[0].0 {
        (bases[0].1.clone(), bases[1].1.clone())
    } else {
        assert_eq!(
            owner_of_b, bases[1].0,
            "B's federation shard is one replica's"
        );
        (bases[1].1.clone(), bases[0].1.clone())
    };

    let alice = register(&client, &other_base, "alice").await;
    let bob = register(&client, &b.base_url(), "bob").await;
    let since: Value = client
        .get(format!("{}/_matrix/client/v3/sync?timeout=0", b.base_url()))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut since = since["next_batch"].as_str().unwrap().to_owned();

    // One message through each replica: the one that sends to B, and the one that does not.
    for (base, n) in [(&sender_base, 1), (&other_base, 2)] {
        let response = client
            .put(format!(
                "{base}/_matrix/client/v3/sendToDevice/m.test.ping/t{n}"
            ))
            .bearer_auth(&alice.token)
            .json(&json!({"messages": {bob.id.clone(): {bob.device.clone(): {"n": n}}}}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut arrived: Vec<Value> = Vec::new();
    while arrived.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "only {arrived:?} of two to-device messages reached bob on B"
        );
        let sync: Value = client
            .get(format!(
                "{}/_matrix/client/v3/sync?timeout=500&since={since}",
                b.base_url()
            ))
            .bearer_auth(&bob.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        arrived.extend(
            sync["to_device"]["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["sender"] == alice.id.as_str())
                .map(|e| e["content"].clone()),
        );
        since = sync["next_batch"].as_str().unwrap().to_owned();
    }
    arrived.sort_by_key(|c| c["n"].as_u64());
    assert_eq!(arrived, [json!({"n": 1}), json!({"n": 2})]);

    // The replica that took the second message forwarded it; the one that sends to B took it.
    let forwarded =
        r#"hs_federation_edus_forwarded_total{edu_type="m.direct_to_device",outcome="forwarded"}"#;
    let received =
        r#"hs_federation_edus_forwarded_total{edu_type="m.direct_to_device",outcome="received"}"#;
    assert_eq!(metric(&client, &other_base, forwarded).await, 1);
    assert_eq!(metric(&client, &sender_base, received).await, 1);
    assert_eq!(metric(&client, &sender_base, forwarded).await, 0);

    b.shutdown().await;
    replica_2.shutdown().await;
    replica_1.shutdown().await;
}
