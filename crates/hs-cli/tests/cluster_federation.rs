//! Another server's requests for a room reach the replica that owns the room, whichever replica
//! they arrive at (`docs/decisions/0035-federation-requests-go-to-the-rooms-owner.md`).
//!
//! Server A is two replicas (`hs serve`, in process) on one PostgreSQL; server B is one embedded
//! server. A's server name is replica 1's address, so every request B sends to A arrives at
//! replica 1, and every room of A this test uses (and the room of B's that alice is invited to)
//! is on a shard replica 2 owns. bob of B joins a room of A (`make_join`, `send_join`), talks in
//! it and leaves it (`/send`), knocks on another (`make_knock`, `send_knock`) and takes the knock
//! back (`make_leave`, `send_leave`); and bob invites alice into a room of B's (`invite`). Each
//! of those reaches A at replica 1, which does not own the room, and is answered by replica 2.
//! Before, `send_join` was refused there with `501 M_HS_INBOUND_INGESTION_UNSUPPORTED`
//! ("fenced: ..."), and `/send`'s PDUs were refused one by one.
//!
//! Both replicas run in this process rather than as the `hs` binary: the binary federates over
//! HTTPS only (`ServeOptions::federation_scheme` cannot be set from a configuration file), and B
//! listens on plain HTTP. The serve path is the binary's own (`hs_cli::serve::spawn_serve`).
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise:
//!
//! ```sh
//! docker run --rm -d --name hs-cluster-federation-pg -e POSTGRES_PASSWORD=hspg \
//!     -p 127.0.0.1:5439:5432 postgres:17
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test -p hs-cli --test cluster_federation
//! ```
//!
//! Each run makes a database of its own and drops it after.

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
                    "SKIP: the two-replica federation test needs PostgreSQL at {admin_dsn:?}: \
                     {e}\nStart one with: docker run --rm -d --name hs-cluster-federation-pg \
                     -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17"
                );
                return None;
            }
        };
        let name = format!(
            "hs_cluster_federation_{}_{}",
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
    let yaml = format!(
        "server:\n  server_name: \"{server_name}\"\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n  max_retry_backoff: 2s\n\
         cluster:\n  single_node: false\n  room_shards: 4\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    advertise_address: \"127.0.0.1:{mesh_port}\"\n    shared_secret: cluster-federation-test-secret\n",
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
         federation:\n  ip_range_blocklist: []\n  max_retry_backoff: 2s\n",
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

/// The sum of every sample of `/metrics` at `base` whose line starts with `prefix` and contains
/// each of `labels`.
async fn metric_sum(client: &reqwest::Client, base: &str, prefix: &str, labels: &[&str]) -> u64 {
    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .filter(|line| line.starts_with(prefix) && labels.iter().all(|l| line.contains(l)))
        .filter_map(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `POST {base}{path}` as `token` with `body`, panicking unless it succeeds; the response body.
async fn post_ok(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: Value,
) -> Value {
    let response = client
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert!(status.is_success(), "POST {path}: {status} {text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// `user`'s membership in `room_id` as `token` reads it through `base`, if any.
async fn membership(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    room_id: &str,
    user: &str,
) -> Option<String> {
    let state: Value = client
        .get(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/state/m.room.member/{user}"
        ))
        .bearer_auth(token)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    state["membership"].as_str().map(str::to_owned)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn federation_requests_reaching_a_replica_that_does_not_own_the_room_are_answered_by_its_owner()
 {
    let Some(db) = Database::create() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let layout = hs_cluster::ShardLayout {
        rooms: 4,
        users: 4,
        ..hs_cluster::ShardLayout::default()
    };
    let (port_1, port_2, port_b) = (reserve_port(), reserve_port(), reserve_port());
    let a_name = format!("127.0.0.1:{port_1}");
    // Mesh ports (the replicas' identities) such that each replica is the rendezvous owner of
    // at least one room shard: replica 2's are where this test's rooms go.
    let mesh_1 = reserve_port();
    let replica_1_id = hs_cluster::ReplicaId::new(format!("127.0.0.1:{mesh_1}"));
    let (mesh_2, replica_2_id) = loop {
        let port = reserve_port();
        let id = hs_cluster::ReplicaId::new(format!("127.0.0.1:{port}"));
        let rooms_of = |who: &hs_cluster::ReplicaId| {
            (0..4).any(|index| {
                hs_cluster::hash::desired_owner(
                    hs_cluster::ShardId::new(hs_cluster::ShardKind::Room, index),
                    [&replica_1_id, &id],
                ) == Some(who)
            })
        };
        if rooms_of(&replica_1_id) && rooms_of(&id) {
            break (port, id);
        }
    };
    let on_replica_2 = |room_id: &str| {
        hs_cluster::hash::desired_owner(layout.room_shard(room_id), [&replica_1_id, &replica_2_id])
            == Some(&replica_2_id)
    };

    // Replica 1 first (it makes the signing key and the schema), then replica 2.
    let replica_1 = serve(replica_config(&db, dir.path(), &a_name, port_1, mesh_1)).await;
    let setup_link = replica_1
        .setup_link
        .clone()
        .expect("a fresh server offers a setup link");
    let replica_2 = serve(replica_config(&db, dir.path(), &a_name, port_2, mesh_2)).await;
    let b_dir = tempfile::tempdir().unwrap();
    let b = serve(single_config(port_b, b_dir.path())).await;
    let (base_1, base_2, base_b) = (replica_1.base_url(), replica_2.base_url(), b.base_url());

    // An administrator, to read the shard map.
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
    let admin_token = admin["access_token"].as_str().unwrap().to_owned();

    // Every room shard is owned by its rendezvous owner, and stays so.
    eventually(
        Duration::from_secs(60),
        "every room shard with its rendezvous owner",
        || {
            let (client, base, token) = (&client, &base_1, &admin_token);
            let (one, two) = (&replica_1_id, &replica_2_id);
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
                let settled = (0..4).all(|index| {
                    let shard = hs_cluster::ShardId::new(hs_cluster::ShardKind::Room, index);
                    let want = hs_cluster::hash::desired_owner(shard, [one, two])
                        .map(hs_cluster::ReplicaId::as_str);
                    page["items"].as_array().is_some_and(|items| {
                        items.iter().any(|s| {
                            s["id"] == format!("room/{index}").as_str()
                                && s["owner"].as_str() == want
                        })
                    })
                });
                settled.then_some(())
            }
        },
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let alice = register(&client, &base_1, "alice").await;
    let bob = register(&client, &base_b, "bob").await;

    // Rooms of A on replica 2's shards, made through replica 1 (the gate places each room).
    let mut rooms = Vec::new();
    for attempt in 0..80 {
        if rooms.len() == 2 {
            break;
        }
        let knock = rooms.len() == 1;
        let mut body = json!({"preset": "public_chat", "name": format!("room {attempt}")});
        if knock {
            body["initial_state"] = json!([{
                "type": "m.room.join_rules", "state_key": "", "content": {"join_rule": "knock"}
            }]);
        }
        let created = post_ok(
            &client,
            &base_1,
            "/_matrix/client/v3/createRoom",
            &alice.token,
            body,
        )
        .await;
        let room_id = created["room_id"].as_str().unwrap().to_owned();
        if on_replica_2(&room_id) {
            rooms.push(room_id);
        }
    }
    assert_eq!(rooms.len(), 2, "no rooms landed on replica 2's shards");
    let (public_room, knock_room) = (rooms[0].clone(), rooms[1].clone());
    eprintln!("rooms on replica 2's shards: {public_room} (public), {knock_room} (knock)");

    let before_federation = metric_sum(
        &client,
        &base_1,
        "hs_cluster_forward_latency_seconds_count{",
        &["kind=\"federation\"", "outcome=\"ok\""],
    )
    .await;

    // bob joins: `make_join` and `send_join`, at replica 1, answered by replica 2.
    post_ok(
        &client,
        &base_b,
        &format!("/_matrix/client/v3/join/{public_room}?server_name={a_name}"),
        &bob.token,
        json!({}),
    )
    .await;
    assert_eq!(
        membership(&client, &base_1, &alice.token, &public_room, &bob.id).await,
        Some("join".to_owned()),
        "A has bob in the room"
    );
    eprintln!("bob joined over federation");

    // bob talks: B's `/send` reaches replica 1, which hands the PDU to replica 2.
    let before_pdus = metric_sum(
        &client,
        &base_1,
        "hs_cluster_forward_latency_seconds_count{",
        &["kind=\"federation_pdu\"", "outcome=\"ok\""],
    )
    .await;
    client
        .put(format!(
            "{base_b}/_matrix/client/v3/rooms/{public_room}/send/m.room.message/t1"
        ))
        .bearer_auth(&bob.token)
        .json(&json!({"msgtype": "m.text", "body": "hello from B"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    eventually(Duration::from_secs(30), "bob's message on A", || {
        let (client, base, token, room) = (&client, &base_2, &alice.token, &public_room);
        async move {
            let page: Value = client
                .get(format!(
                    "{base}/_matrix/client/v3/rooms/{room}/messages?dir=b&limit=20"
                ))
                .bearer_auth(token)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            page["chunk"]
                .as_array()?
                .iter()
                .any(|e| e["content"]["body"] == "hello from B")
                .then_some(())
        }
    })
    .await;
    eprintln!("bob's message reached A");

    // bob leaves, over `/send` too.
    post_ok(
        &client,
        &base_b,
        &format!("/_matrix/client/v3/rooms/{public_room}/leave"),
        &bob.token,
        json!({}),
    )
    .await;
    eventually(Duration::from_secs(30), "bob's leave on A", || {
        let (client, base, token, room, bob) = (&client, &base_1, &alice.token, &public_room, &bob);
        async move {
            (membership(client, base, token, room, &bob.id)
                .await
                .as_deref()
                == Some("leave"))
            .then_some(())
        }
    })
    .await;
    assert!(
        metric_sum(
            &client,
            &base_1,
            "hs_cluster_forward_latency_seconds_count{",
            &["kind=\"federation_pdu\"", "outcome=\"ok\""],
        )
        .await
            >= before_pdus + 2,
        "replica 1 handed bob's message and leave to replica 2"
    );
    eprintln!("bob left over federation");

    // bob knocks (`make_knock`, `send_knock`) and takes it back (`make_leave`, `send_leave`).
    post_ok(
        &client,
        &base_b,
        &format!("/_matrix/client/v3/knock/{knock_room}?server_name={a_name}"),
        &bob.token,
        json!({"reason": "let me in"}),
    )
    .await;
    assert_eq!(
        membership(&client, &base_1, &alice.token, &knock_room, &bob.id).await,
        Some("knock".to_owned()),
        "A has bob's knock"
    );
    post_ok(
        &client,
        &base_b,
        &format!("/_matrix/client/v3/rooms/{knock_room}/leave"),
        &bob.token,
        json!({}),
    )
    .await;
    assert_eq!(
        membership(&client, &base_1, &alice.token, &knock_room, &bob.id).await,
        Some("leave".to_owned()),
        "A has bob's knock taken back"
    );
    eprintln!("bob knocked and took it back over federation");

    // bob invites alice into a room of B's whose shard, on A, is replica 2's: `invite`.
    let mut b_room = None;
    for attempt in 0..80 {
        let created = post_ok(
            &client,
            &base_b,
            "/_matrix/client/v3/createRoom",
            &bob.token,
            json!({"preset": "private_chat", "name": format!("b room {attempt}")}),
        )
        .await;
        let room_id = created["room_id"].as_str().unwrap().to_owned();
        if on_replica_2(&room_id) {
            b_room = Some(room_id);
            break;
        }
    }
    let b_room = b_room.expect("no room of B's landed on replica 2's shard");
    post_ok(
        &client,
        &base_b,
        &format!("/_matrix/client/v3/rooms/{b_room}/invite"),
        &bob.token,
        json!({"user_id": alice.id}),
    )
    .await;
    eventually(Duration::from_secs(30), "alice's invite on A", || {
        let (client, base, token, room) = (&client, &base_1, &alice.token, &b_room);
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
            sync["rooms"]["invite"].get(room.as_str()).map(|_| ())
        }
    })
    .await;
    eprintln!("alice was invited over federation");

    // Replica 1 forwarded every one of them, and counted them: make_join, send_join,
    // make_knock, send_knock, make_leave, send_leave and invite at the least.
    let forwarded = metric_sum(
        &client,
        &base_1,
        "hs_cluster_forward_latency_seconds_count{",
        &["kind=\"federation\"", "outcome=\"ok\""],
    )
    .await;
    assert!(
        forwarded >= before_federation + 7,
        "replica 1 forwarded {} federation requests",
        forwarded - before_federation
    );
    b.shutdown().await;
    replica_2.shutdown().await;
    replica_1.shutdown().await;
}
