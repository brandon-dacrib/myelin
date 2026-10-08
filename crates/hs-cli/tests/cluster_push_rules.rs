//! Push rules across replicas: two real `hs serve` processes on one PostgreSQL. Alice's push
//! rules are changed on one replica while the other has them cached, and the change shows
//! everywhere at once: in `GET /pushrules` and `/sync`'s `m.push_rules` on the other replica,
//! and in the pushes the room's owner sends for her, whichever replica that is
//! (`hs_cli::push_cluster`, `hs_push::compiled`'s "Across replicas").
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise:
//!
//! ```sh
//! docker run --rm -d --name hs-cluster-push-rules-pg -e POSTGRES_PASSWORD=hspg \
//!     -p 127.0.0.1:5439:5432 postgres:17
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test -p hs-cli --test cluster_push_rules
//! ```
//!
//! The rules are changed twice, once on each replica, so that one of the two changes is made
//! on the replica that does not own the room (whose pipeline evaluates for alice) while the
//! owner has her rules cached from the push before. Every check waits for a condition with a
//! deadline, never for a fixed time.

use std::time::{Duration, Instant};

use hs_testkit::fake_pushgw::FakePushGateway;
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
                    "SKIP: the two-replica push-rules test needs PostgreSQL at {admin_dsn:?}: \
                     {e}\nStart one with: docker run --rm -d --name hs-cluster-push-rules-pg \
                     -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 postgres:17"
                );
                return None;
            }
        };
        let name = format!(
            "hs_cluster_push_rules_{}_{}",
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
         cluster:\n  single_node: false\n  room_shards: 4\n  user_shards: 4\n  heartbeat_interval: 500ms\n  lease_ttl: 3s\n  mesh:\n    port: {mesh_port}\n    shared_secret: cluster-push-rules-test-secret\n",
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

/// How long a cross-replica change may take to show. Generous for a loaded machine; well
/// under the replicas' revalidation interval (30 s), so only the mesh message can pass it.
const WITHIN: Duration = Duration::from_secs(15);

/// A push gateway in this process, and the URL to give a pusher.
async fn gateway() -> (FakePushGateway, String) {
    let gateway = FakePushGateway::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/_matrix/push/v1/notify",
        listener.local_addr().unwrap()
    );
    let router = gateway.router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (gateway, url)
}

/// The push for `event_id`, once the gateway has it.
async fn push_for(gateway: &FakePushGateway, event_id: &str) -> Value {
    eventually(WITHIN, &format!("a push for {event_id}"), || async move {
        gateway
            .notifications()
            .into_iter()
            .map(|n| n.notification)
            .find(|n| n["event_id"] == event_id)
    })
    .await
}

/// The sound a push asked the device to play.
fn sound_of(push: &Value) -> Value {
    push["devices"][0]["tweaks"]["sound"].clone()
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

/// `PUT` on one of alice's push rules, at `path` under `/pushrules/global/`.
async fn put_rule(client: &reqwest::Client, base: &str, user: &User, path: &str, body: Value) {
    let response = client
        .put(format!("{base}/_matrix/client/v3/pushrules/global/{path}"))
        .bearer_auth(&user.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "PUT pushrules/global/{path}: {} {}",
        response.status(),
        response.text().await.unwrap_or_default()
    );
}

/// The `sound` tweak of alice's content rule `rule_id` in a `{"global": ...}` ruleset.
fn rule_sound(rules: &Value, rule_id: &str) -> Option<Value> {
    rules["global"]["content"]
        .as_array()?
        .iter()
        .find(|r| r["rule_id"] == rule_id)?["actions"]
        .as_array()?
        .iter()
        .find(|a| a["set_tweak"] == "sound")
        .map(|a| a["value"].clone())
}

/// `GET /pushrules/` at `base`.
async fn get_rules(client: &reqwest::Client, base: &str, user: &User) -> Value {
    client
        .get(format!("{base}/_matrix/client/v3/pushrules/"))
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// One user's `/sync` on one replica, carrying `since` forward.
struct Session<'a> {
    client: &'a reqwest::Client,
    base: String,
    user: &'a User,
    since: Option<String>,
}

impl<'a> Session<'a> {
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

    /// Syncs (one-second long-polls) until `m.push_rules` comes down with `rule_id`'s sound
    /// at `sound`.
    async fn until_rule_sound(&mut self, rule_id: &str, sound: &str) {
        let deadline = Instant::now() + WITHIN;
        loop {
            let response = self.sync(1000).await;
            let rules = response["account_data"]["events"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|e| e["type"] == "m.push_rules")
                .map(|e| e["content"].clone());
            if rules.is_some_and(|r| rule_sound(&r, rule_id) == Some(json!(sound))) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{}'s sync on {} never brought {rule_id} with sound {sound}; the last was {response}",
                self.user.id,
                self.base
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_rule_change_on_one_replica_shows_on_the_other_at_once() {
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
    let (gateway, gateway_url) = gateway().await;

    let mut hs_a = HsProcess::serve(&config_a);
    let setup_line = hs_a.wait_for("setup_link=");
    let admin = first_admin(&client, &a, &setup_line).await;
    let mut hs_b = HsProcess::serve(&config_b);
    hs_b.wait_for("listening");

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

    // Alice and bob in one room; alice has an HTTP pusher to the gateway in this test.
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;
    let created: Value = client
        .post(format!("{a}/_matrix/client/v3/createRoom"))
        .bearer_auth(&alice.token)
        .json(&json!({"preset": "public_chat", "name": "rules"}))
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
    let pusher = client
        .post(format!("{a}/_matrix/client/v3/pushers/set"))
        .bearer_auth(&alice.token)
        .json(&json!({
            "pushkey": "alice-phone",
            "app_id": "org.example.app",
            "kind": "http",
            "app_display_name": "Example",
            "device_display_name": "Phone",
            "lang": "en",
            "data": {"url": gateway_url},
        }))
        .send()
        .await
        .unwrap();
    assert!(
        pusher.status().is_success(),
        "pushers/set: {}",
        pusher.status()
    );

    // The room's owner evaluates bob's message for alice, and has her rules cached from then.
    let first = send_message(&client, &b, &room, &bob, "alpha zero").await;
    assert_eq!(
        sound_of(&push_for(&gateway, &first).await),
        json!("default")
    );

    // Alice's sessions on both replicas, past their initial sync.
    let mut alice_on_a = Session {
        client: &client,
        base: a.clone(),
        user: &alice,
        since: None,
    };
    let mut alice_on_b = Session {
        client: &client,
        base: b.clone(),
        user: &alice,
        since: None,
    };
    alice_on_a.sync(0).await;
    alice_on_b.sync(0).await;
    // And each replica has her rules cached for the client side too.
    get_rules(&client, &a, &alice).await;
    get_rules(&client, &b, &alice).await;

    // Changed on A, then on B: each time the owner's next push for her follows it, whichever
    // replica took the change, and the other replica shows it at once.
    for (round, (on, other, other_session, sound)) in [
        (&a, &b, &mut alice_on_b, "alpha-a"),
        (&b, &a, &mut alice_on_a, "alpha-b"),
    ]
    .into_iter()
    .enumerate()
    {
        put_rule(
            &client,
            on,
            &alice,
            "content/alpha",
            json!({"pattern": "alpha", "actions": ["notify", {"set_tweak": "sound", "value": sound}]}),
        )
        .await;
        // The push first: a client read on the owner would read the store and refresh its
        // copy itself, so the owner must be reached by the change alone.
        let event = send_message(&client, &b, &room, &bob, &format!("alpha {round}")).await;
        let push = push_for(&gateway, &event).await;
        assert_eq!(
            sound_of(&push),
            json!(sound),
            "round {round}: the owner pushed with the rules from before the change: {push}"
        );
        let shown = get_rules(&client, other, &alice).await;
        assert_eq!(
            rule_sound(&shown, "alpha"),
            Some(json!(sound)),
            "round {round}: GET /pushrules on the other replica is behind: {shown}"
        );
        other_session.until_rule_sound("alpha", sound).await;
    }

    // Changes made on both replicas at once all land: each replica's write is conditional on
    // the change-seq its edit started from, and an edit that lost runs again.
    for wave in 0..3 {
        let mut puts = tokio::task::JoinSet::new();
        for i in 0..8 {
            let base = if i % 2 == 0 { a.clone() } else { b.clone() };
            let (client, alice) = (
                client.clone(),
                User {
                    id: alice.id.clone(),
                    token: alice.token.clone(),
                },
            );
            puts.spawn(async move {
                put_rule(
                    &client,
                    &base,
                    &alice,
                    &format!("content/w{wave}r{i}"),
                    json!({"pattern": format!("w{wave}r{i}"), "actions": ["notify"]}),
                )
                .await;
            });
        }
        while let Some(done) = puts.join_next().await {
            done.unwrap();
        }
        let rules = get_rules(&client, &a, &alice).await;
        let missing: Vec<String> = (0..8)
            .map(|i| format!("w{wave}r{i}"))
            .filter(|id| {
                !rules["global"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|r| r["rule_id"] == id.as_str())
            })
            .collect();
        assert!(
            missing.is_empty(),
            "wave {wave}: rules added on two replicas at once were lost: {missing:?}"
        );
    }

    // At least one replica dropped its copy on the other's word.
    let peer = r#"hs_push_rule_cache_invalidations_total{source="peer"}"#;
    let dropped = metric(&client, &a, peer).await + metric(&client, &b, peer).await;
    assert!(
        dropped > 0,
        "no cached copy was dropped on a peer's message"
    );

    let log_b = hs_b.stop();
    let log_a = hs_a.stop();
    for (log, name) in [(&log_a, "A"), (&log_b, "B")] {
        assert!(
            log.contains("push rules are cluster-aware"),
            "{name} did not install the push-rule change feed: {log}"
        );
    }
}
