//! The Cluster area of the admin API through the real `hs` binary: `cluster.replicas.*` and
//! `cluster.shards.list`.
//!
//! - Single node (always runs): one replica, owning every shard, which cannot be drained.
//! - Two replicas on one PostgreSQL (runs when a PostgreSQL server is reachable, and prints a
//!   skip message otherwise): drain one through the other, follow the task until it owns
//!   nothing, see that it stays drained across a restart, undrain it and see it take shards
//!   back. Start a server and run it with:
//!
//! ```sh
//! docker run --rm -d --name hs-cluster-admin-pg -e POSTGRES_PASSWORD=hspg \
//!     -p 127.0.0.1:5439:5432 postgres:17
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test -p hs-cli --test cluster_admin
//! ```
//!
//! Each run makes a database of its own (`hs_cluster_admin_<pid>_<nanos>`) and drops it after.

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

    /// Reads the log until a line contains `needle`.
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

    /// Everything logged so far, without waiting.
    fn drain_log(&mut self) -> String {
        while let Ok(line) = self.lines.try_recv() {
            self.seen.push(line);
        }
        self.seen.join("\n")
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

async fn get(client: &reqwest::Client, url: &str, token: &str) -> (reqwest::StatusCode, Value) {
    let response = client.get(url).bearer_auth(token).send().await.unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn post(client: &reqwest::Client, url: &str, token: &str) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(url)
        .bearer_auth(token)
        .header("idempotency-key", format!("{:x}", rand_suffix()))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// A replica id as a path segment (`127.0.0.1:18449` has a colon).
fn segment(id: &str) -> String {
    id.replace(':', "%3A")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_node_is_one_replica_owning_every_shard_and_cannot_be_drained() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n",
            dir.path().join("data"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut hs = HsProcess::serve(&config_path);
    let setup_line = hs.wait_for("setup_link=");
    let token = first_admin(&client, &base, &setup_line).await;

    let (status, page) = get(&client, &format!("{base}/api/v1/cluster/replicas"), &token).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{page}");
    let me = &items[0];
    assert_eq!(me["role"], "single-node");
    assert_eq!(me["status"], "active");
    assert_eq!(me["this_replica"], true);
    assert!(me["drain_requested_at"].is_null());
    let id = me["id"].as_str().unwrap().to_owned();
    let owned = me["shard_count"].as_u64().unwrap();

    // Every shard of the layout, and this replica owns each one.
    let (status, shards) = get(
        &client,
        &format!("{base}/api/v1/cluster/shards?include_total=true&limit=500"),
        &token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(shards["total"].as_u64().unwrap(), owned);
    let (_, rooms) = get(
        &client,
        &format!("{base}/api/v1/cluster/shards?kind=room&limit=500"),
        &token,
    )
    .await;
    let rooms = rooms["items"].as_array().unwrap();
    assert!(!rooms.is_empty());
    assert!(
        rooms
            .iter()
            .all(|s| s["kind"] == "room" && s["owner"] == id.as_str() && s["state"] == "owned")
    );
    let (status, _) = get(
        &client,
        &format!("{base}/api/v1/cluster/shards?kind=planet"),
        &token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);

    let replica_url = format!("{base}/api/v1/cluster/replicas/{}", segment(&id));
    let (status, got) = get(&client, &replica_url, &token).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(got["id"], id.as_str());
    let (status, _) = get(
        &client,
        &format!("{base}/api/v1/cluster/replicas/nobody"),
        &token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    // Nothing to drain to: refused, and said why.
    let (status, problem) = post(&client, &format!("{replica_url}/drain"), &token).await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("not running as a cluster"),
        "{problem}"
    );
    let (status, same) = post(&client, &format!("{replica_url}/undrain"), &token).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(same["status"], "active");

    // The drain metrics are exported, and nothing was counted.
    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("hs_cluster_admin_drain_duration_seconds_count 0"),
        "{metrics}"
    );
    assert!(!metrics.contains("hs_cluster_admin_drains_total{"));
    let log = hs.stop();
    assert!(log.contains("a drain was refused"), "{log}");
}

// -------------------------------------------------------------------------------------------
// Two replicas on one PostgreSQL.
// -------------------------------------------------------------------------------------------

struct Database {
    admin_dsn: String,
    name: String,
    host: String,
    port: u16,
    user: String,
    password: String,
}

impl Database {
    /// A fresh database on the test server, or `None` (after saying so) if there is none. On a
    /// thread of its own: the synchronous `postgres` client runs a runtime of its own inside,
    /// which cannot be started from within the test's.
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
                    "SKIP: the two-replica cluster admin test needs PostgreSQL at {admin_dsn:?}: \
                     {e}\nStart one with: docker run --rm -d --name hs-cluster-admin-pg \
                     -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17"
                );
                return None;
            }
        };
        let name = format!("hs_cluster_admin_{}_{}", std::process::id(), rand_suffix());
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

fn replica_config(db: &Database, dir: &std::path::Path, port: u16, mesh_port: u16) -> String {
    format!(
        "server:\n  server_name: cluster.example.org\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         cluster:\n  single_node: false\n  room_shards: 4\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    shared_secret: cluster-admin-test-secret\n",
        keys = dir.join("keys"),
        media = dir.join("media"),
        host = db.host,
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
    )
}

/// Replicas by id, as `base` answers.
async fn replicas(client: &reqwest::Client, base: &str, token: &str) -> Vec<Value> {
    let (status, page) = get(client, &format!("{base}/api/v1/cluster/replicas"), token).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{page}");
    page["items"].as_array().unwrap().clone()
}

fn find<'a>(replicas: &'a [Value], id: &str) -> Option<&'a Value> {
    replicas.iter().find(|r| r["id"] == id)
}

/// Every shard, as `base` answers.
async fn shards(client: &reqwest::Client, base: &str, token: &str) -> Vec<Value> {
    let (status, page) = get(
        client,
        &format!("{base}/api/v1/cluster/shards?limit=500"),
        token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{page}");
    assert!(page["next_cursor"].is_null(), "more than 500 shards");
    page["items"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replica_drained_through_another_hands_off_every_shard_stays_drained_and_comes_back() {
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
    let token = first_admin(&client, &a, &setup_line).await;
    let mut hs_b = HsProcess::serve(&config_b);
    hs_b.wait_for("listening");

    // Both active, and between them they own the whole layout.
    let total = eventually(
        Duration::from_secs(60),
        "two active replicas sharing every shard",
        || {
            let (client, a, token, id_a, id_b) = (&client, &a, &token, &id_a, &id_b);
            async move {
                let listed = replicas(client, a, token).await;
                let both = [id_a, id_b].iter().all(|id| {
                    find(&listed, id).is_some_and(|r| {
                        r["status"] == "active" && r["shard_count"].as_u64() > Some(0)
                    })
                });
                let all = shards(client, a, token).await;
                (both && all.iter().all(|s| !s["owner"].is_null())).then_some(all.len())
            }
        },
    )
    .await;
    let seen_by_b = replicas(&client, &b, &token).await;
    assert_eq!(find(&seen_by_b, &id_b).unwrap()["this_replica"], true);
    assert_eq!(find(&seen_by_b, &id_a).unwrap()["this_replica"], false);

    // Drain B, asking A.
    let (status, drained) = post(
        &client,
        &format!("{a}/api/v1/cluster/replicas/{}/drain", segment(&id_b)),
        &token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{drained}");
    assert!(
        drained["status"] == "draining" || drained["status"] == "drained",
        "{drained}"
    );
    assert_eq!(drained["drain_requested_by"], "@ops:cluster.example.org");
    let task_id = drained["drain_task_id"].as_str().unwrap().to_owned();

    // The task follows it to the end.
    let task = eventually(Duration::from_secs(60), "the drain task to succeed", || {
        let (client, a, token, task_id) = (&client, &a, &token, &task_id);
        async move {
            let (_, task) = get(client, &format!("{a}/api/v1/tasks/{task_id}"), token).await;
            assert_ne!(task["status"], "failed", "{task}");
            (task["status"] == "succeeded").then_some(task)
        }
    })
    .await;
    assert_eq!(task["action"], "cluster.replicas.drain");
    assert_eq!(task["resource"]["id"], id_b.as_str());

    // B, asked itself, says it is drained and owns nothing; A owns everything.
    let seen_by_b = replicas(&client, &b, &token).await;
    let b_row = find(&seen_by_b, &id_b).unwrap();
    assert_eq!(b_row["status"], "drained", "{b_row}");
    assert_eq!(b_row["shard_count"], 0);
    assert_eq!(b_row["drain_task_id"], task_id.as_str());
    // What B released, A takes at its next heartbeat.
    eventually(Duration::from_secs(90), "A to own every shard", || {
        let (client, b, token, id_a) = (&client, &b, &token, &id_a);
        async move {
            let all = shards(client, b, token).await;
            assert_eq!(all.len(), total);
            all.iter()
                .all(|s| s["owner"] == id_a.as_str())
                .then_some(())
        }
    })
    .await;
    // Drained is not down: B still serves.
    let ready = client
        .get(format!("{b}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), reqwest::StatusCode::OK);

    // A is now the only replica serving: it cannot be drained.
    let (status, problem) = post(
        &client,
        &format!("{b}/api/v1/cluster/replicas/{}/drain", segment(&id_a)),
        &token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{problem}");

    // Audited, and counted.
    let (_, audit) = get(
        &client,
        &format!("{a}/api/v1/audit-log?action=cluster.replicas.drain"),
        &token,
    )
    .await;
    let entries = audit["items"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{audit}");
    assert_eq!(entries[0]["target"]["id"], id_b.as_str());
    let metrics = client
        .get(format!("{a}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("hs_cluster_admin_drains_total{event=\"requested\"} 1"),
        "{metrics}"
    );
    assert!(metrics.contains("hs_cluster_admin_drains_total{event=\"completed\"} 1"));

    // A restart does not undo a drain: stopped, B is still listed, drained; started again, it
    // stays drained and takes nothing.
    let log_b = hs_b.stop();
    assert!(
        log_b.contains("an administrator asked this replica to drain"),
        "{log_b}"
    );
    let listed = replicas(&client, &a, &token).await;
    assert_eq!(find(&listed, &id_b).unwrap()["status"], "drained");
    let mut hs_b = HsProcess::serve(&config_b);
    hs_b.wait_for("listening");
    eventually(
        Duration::from_secs(30),
        "the restarted B to heartbeat, drained",
        || {
            let (client, a, token, id_b) = (&client, &a, &token, &id_b);
            async move {
                let listed = replicas(client, a, token).await;
                find(&listed, id_b)
                    .filter(|r| r["status"] == "drained" && !r["last_heartbeat_at"].is_null())
                    .map(|_| ())
            }
        },
    )
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        shards(&client, &a, &token)
            .await
            .iter()
            .all(|s| s["owner"] == id_a.as_str())
    );

    // Undrained through B itself, it takes its share back.
    let (status, back) = post(
        &client,
        &format!("{b}/api/v1/cluster/replicas/{}/undrain", segment(&id_b)),
        &token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{back}");
    assert!(back["drain_requested_at"].is_null());
    eventually(Duration::from_secs(60), "B to own shards again", || {
        let (client, a, token, id_b) = (&client, &a, &token, &id_b);
        async move {
            let listed = replicas(client, a, token).await;
            find(&listed, id_b)
                .filter(|r| r["status"] == "active" && r["shard_count"].as_u64() > Some(0))
                .map(|_| ())
        }
    })
    .await;
    let (_, audit) = get(
        &client,
        &format!("{a}/api/v1/audit-log?action=cluster.replicas.undrain"),
        &token,
    )
    .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1, "{audit}");

    let log_b = hs_b.stop();
    assert!(log_b.contains("drain was withdrawn"), "{log_b}");
    let log_a = hs_a.drain_log();
    assert!(
        log_a.contains("an administrator asked a replica to drain"),
        "{log_a}"
    );
    hs_a.stop();
}
