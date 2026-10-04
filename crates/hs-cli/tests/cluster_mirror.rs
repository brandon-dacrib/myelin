//! What a new event costs a replica that does not own its room (RFC 0018, decision 0022): three
//! real `hs serve` processes on one PostgreSQL. A owns the rooms (the test makes rooms until A does);
//! B1 and B2 own none of them and answer `/sync` for bob and carol through their room mirrors.
//! B1 runs with `HS_SYNC_MIRROR_FULL_RELOAD=1`, the mirror as it was before decision 0022 (every
//! new event loads the room again whole); B2 runs as shipped (it reads only the new rows). Both
//! see the same events at the same time, so their `/metrics` side by side are the before and
//! the after.
//!
//! Two phases, each `MESSAGES` messages from alice on A with bob long-polling on B1 and carol on
//! B2: a small room, then a big one (`EVENTS` messages of history and `MEMBERS` more members).
//! Per phase it prints, for each of B1 and B2, the mirror's work per event (the
//! `hs_user_mirror_catchup_duration_seconds` sum over the phase, divided by the messages) and
//! the write-to-woken-sync latency (from alice's send on A to the woken long-poll's answer
//! carrying it), and it checks that B2 never loaded a room whole during either phase and did
//! less work per event than B1 in the big room.
//!
//! The sizes default to a quick run for the gate. The measurement in `docs/status/05-sync.md`
//! (session 12) used a release build and
//!
//! ```sh
//! HS_MIRROR_BENCH_EVENTS=2000 HS_MIRROR_BENCH_MEMBERS=300 HS_MIRROR_BENCH_MESSAGES=100 \
//! HS_CLUSTER_TEST_POSTGRES_DSN="postgres://postgres:hspg@127.0.0.1:5439/postgres" \
//!     cargo test --release -p hs-cli --test cluster_mirror -- --nocapture
//! ```
//!
//! which took 55 minutes, most of it the owner's session hub catching up after the 300 joins (it
//! writes every member's records one at a time; status 05, session 12).
//!
//! Runs when a PostgreSQL server is reachable, and prints a skip message otherwise (see
//! `tests/cluster_ephemeral.rs` for a container to start). Each run makes a database of its own
//! and drops it after.

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

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One run of the real `hs` binary, reading its log.
struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    fn serve(config_path: &std::path::Path, envs: &[(&str, &str)]) -> Self {
        use std::io::BufRead;
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hs"));
        command
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .env_remove("HS_SYNC_MIRROR_FULL_RELOAD")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("the hs binary should start");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                // A server's errors reach the test's own output, so a failure that is a
                // server's 500 says why (the log is otherwise only read on a settle timeout).
                if line.contains(" ERROR ") {
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

    /// Everything logged so far, without waiting.
    fn drain(&mut self) -> String {
        while let Ok(line) = self.lines.try_recv() {
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
                    "SKIP: the room mirror measurement needs PostgreSQL at {admin_dsn:?}: {e}"
                );
                return None;
            }
        };
        let name = format!("hs_cluster_mirror_{}_{}", std::process::id(), rand_suffix());
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

/// How long a reader may wait for the owner's hub to reach an update (see [`settled_token`]).
const SETTLE_WITHIN: Duration = Duration::from_secs(4 * 3600);

/// Room shards in the test cluster: enough that each of three replicas owns some.
const ROOM_SHARDS: u32 = 16;

/// One replica's configuration: long leases, so that a heartbeat late on a loaded machine
/// does not move a shard in the middle of the measurement, and a small pool, since the test
/// server allows a hundred connections and three replicas share it.
fn replica_config(db: &Database, dir: &std::path::Path, port: u16, mesh_port: u16) -> String {
    format!(
        "server:\n  server_name: cluster.example.org\n  signing_key_path: {keys:?}\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: postgres\n  host: {host}\n  port: {pg_port}\n  database: {name}\n  user: {user}\n  password: {password}\n  tls: false\n  pool_size: 8\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n\
         cluster:\n  single_node: false\n  room_shards: {room_shards}\n  user_shards: 4\n  heartbeat_interval: 2s\n  lease_ttl: 30s\n  mesh:\n    port: {mesh_port}\n    shared_secret: cluster-mirror-test-secret\n",
        keys = dir.join("keys"),
        media = dir.join(format!("media-{port}")),
        host = db.host,
        pg_port = db.port,
        name = db.name,
        user = db.user,
        password = db.password,
        room_shards = ROOM_SHARDS,
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

/// A room made by `user` through `base` whose shard `owner` owns. `createRoom` picks the room
/// id first and builds the room on whichever replica owns its shard (decision 0020), so the
/// room a client asks one replica for may live on another; this asks again until it does not.
async fn room_owned_by(
    client: &reqwest::Client,
    base: &str,
    admin: &str,
    user: &User,
    owner: &str,
    name: &str,
) -> String {
    let mut seen = Vec::new();
    for _ in 0..64 {
        let room = create_room(client, base, user, name).await;
        let landed = owner_of(client, base, admin, &room).await;
        if landed.as_deref() == Some(owner) {
            return room;
        }
        seen.push(landed);
    }
    panic!("no room made through {base} landed on {owner} in 64 tries: {seen:?}");
}

async fn join(client: &reqwest::Client, base: &str, room: &str, user: &User) {
    let joined = client
        .post(format!("{base}/_matrix/client/v3/rooms/{room}/join"))
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
        // A shard changing hands under a loaded machine is answered as a client would: again.
        tries += 1;
        let text = response.to_string();
        let transient = text.contains("fenced")
            || text.contains("M_HS_NOT_SHARD_OWNER")
            || text.contains("M_LIMIT_EXCEEDED");
        assert!(transient && tries < 20, "send failed: {response}");
        tokio::time::sleep(Duration::from_millis(250 * tries)).await;
    }
}

/// `count` messages from `user`, `parallel` requests at a time (the room serializes them).
async fn send_many(
    client: &reqwest::Client,
    base: &str,
    room: &str,
    user: &User,
    count: usize,
    parallel: usize,
) {
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..count {
        if tasks.len() >= parallel {
            tasks.join_next().await.unwrap().unwrap();
        }
        let (client, base, room, user) = (
            client.clone(),
            base.to_owned(),
            room.to_owned(),
            user.clone(),
        );
        tasks.spawn(async move {
            send_message(&client, &base, &room, &user, &format!("history #{i}")).await;
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.unwrap();
    }
}

/// An administrator's `POST`, sent again while it answers `503`: concurrent admin writes
/// conflict on the audit log's counter and run out of retries on a loaded machine. (A `503`
/// can follow a write that landed, so a retried creation may answer `409`.)
async fn admin_post(
    client: &reqwest::Client,
    url: &str,
    admin: &str,
    body: Value,
) -> reqwest::Response {
    let mut tries = 0;
    loop {
        let response = client
            .post(url)
            .bearer_auth(admin)
            .json(&body)
            .send()
            .await
            .unwrap();
        tries += 1;
        if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE || tries >= 20 {
            return response;
        }
        tokio::time::sleep(Duration::from_millis(100 * tries)).await;
    }
}

/// `count` members made by the administrator on `base` and joined to `room` there, `parallel`
/// at a time.
async fn add_members(
    client: &reqwest::Client,
    base: &str,
    admin: &str,
    room: &str,
    count: usize,
    parallel: usize,
) {
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..count {
        if tasks.len() >= parallel {
            tasks.join_next().await.unwrap().unwrap();
        }
        let (client, base, admin, room) = (
            client.clone(),
            base.to_owned(),
            admin.to_owned(),
            room.to_owned(),
        );
        tasks.spawn(async move {
            let localpart = format!("member{i:04}");
            let created = admin_post(
                &client,
                &format!("{base}/api/v1/users"),
                &admin,
                json!({"localpart": localpart, "password": "member password, long enough"}),
            )
            .await;
            assert!(
                created.status().is_success() || created.status() == reqwest::StatusCode::CONFLICT,
                "create {localpart}: {} {}",
                created.status(),
                created.text().await.unwrap_or_default()
            );
            let joined = admin_post(
                &client,
                &format!("{base}/api/v1/rooms/{}/join", escape(&room)),
                &admin,
                json!({"user_id": format!("@{localpart}:cluster.example.org")}),
            )
            .await;
            assert!(
                joined.status().is_success(),
                "join {localpart}: {} {}",
                joined.status(),
                joined.text().await.unwrap_or_default()
            );
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.unwrap();
    }
}

/// One `/sync`, a long-poll of `timeout_ms`. A request that fails outright or answers an error
/// is sent again, as a client does: on a loaded machine a sync's store transaction can run out
/// of retries against the owner's feed writes.
async fn sync(
    client: &reqwest::Client,
    base: &str,
    user: &User,
    since: Option<&str>,
    timeout_ms: u64,
) -> Value {
    let mut url =
        format!("{base}/_matrix/client/v3/sync?timeout={timeout_ms}&set_presence=offline");
    if let Some(since) = since {
        url.push_str("&since=");
        url.push_str(since);
    }
    let mut tries = 0u64;
    loop {
        tries += 1;
        let answer = match client.get(&url).bearer_auth(&user.token).send().await {
            Ok(response) => response.json::<Value>().await.map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match answer {
            Ok(value) if value["next_batch"].is_string() => return value,
            other => {
                assert!(
                    tries < 10,
                    "{} could not sync on {base}: {other:?}",
                    user.id
                );
                tokio::time::sleep(Duration::from_millis(250 * tries)).await;
            }
        }
    }
}

fn next_batch(response: &Value) -> String {
    response["next_batch"]
        .as_str()
        .unwrap_or_else(|| panic!("no next_batch: {response}"))
        .to_owned()
}

fn bodies(response: &Value, room: &str) -> Vec<String> {
    response["rooms"]["join"][room]["timeline"]["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
        .collect()
}

/// An initial sync, repeated until it shows every room in `rooms` joined, then long-polls until
/// the message `marker` shows in `marker_room`; the token it ends on.
///
/// The wait is long on purpose. The owner's session hub writes each update's membership
/// records and feed entries one member at a time, two store round trips each, so after the big
/// room's members join it is far behind (an hour, at 300 members, in a debug build on a loaded
/// machine), and the marker sent after them reaches a reader only when the hub gets there. That
/// is the owner's fan-out cost, which this test does not measure; measuring before the hub has
/// caught up would.
async fn settled_token(
    client: &reqwest::Client,
    base: &str,
    user: &User,
    rooms: &[&str],
    marker_room: &str,
    marker: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + SETTLE_WITHIN;
    let mut token = loop {
        let initial = sync(client, base, user, None, 0).await;
        if rooms
            .iter()
            .all(|r| initial["rooms"]["join"][*r].is_object())
        {
            // A reader whose first sync comes after the owner caught up has the marker already.
            if bodies(&initial, marker_room).iter().any(|b| b == marker) {
                return Ok(next_batch(&initial));
            }
            break next_batch(&initial);
        }
        if Instant::now() >= deadline {
            let shown: Vec<&String> = initial["rooms"]["join"]
                .as_object()
                .map(|o| o.keys().collect())
                .unwrap_or_default();
            return Err(format!(
                "{}'s initial sync on {base} never showed every room of {rooms:?}; it showed {shown:?}",
                user.id
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let deadline = Instant::now() + SETTLE_WITHIN;
    loop {
        let response = sync(client, base, user, Some(&token), 10_000).await;
        token = next_batch(&response);
        if bodies(&response, marker_room).iter().any(|b| b == marker) {
            return Ok(token);
        }
        if Instant::now() >= deadline {
            return Err(format!("{} never saw {marker:?} on {base}", user.id));
        }
    }
}

/// Long-polls until a message with `body` arrives in `room`; answers when it did and the token
/// after it.
async fn until_body(
    client: reqwest::Client,
    base: String,
    user: User,
    mut since: String,
    room: String,
    body: String,
) -> (Instant, String) {
    let deadline = Instant::now() + SETTLE_WITHIN;
    loop {
        let response = sync(&client, &base, &user, Some(&since), 10_000).await;
        let at = Instant::now();
        since = next_batch(&response);
        if bodies(&response, &room).contains(&body) {
            return (at, since);
        }
        assert!(
            Instant::now() < deadline,
            "{} never saw {body:?} on {base}",
            user.id
        );
    }
}

/// The value of the first `/metrics` sample at `base` whose name and labels are exactly
/// `sample`; 0 when there is none.
fn sample(text: &str, sample: &str) -> f64 {
    text.lines()
        .find_map(|line| {
            let rest = line.strip_prefix(sample)?;
            rest.starts_with(' ')
                .then(|| rest.trim().parse::<f64>().ok())?
        })
        .unwrap_or(0.0)
}

/// What a replica's mirror had done, from its `/metrics`.
#[derive(Debug, Clone, Copy, Default)]
struct MirrorWork {
    full_seconds: f64,
    full_count: f64,
    incremental_seconds: f64,
    incremental_count: f64,
    caught_up_events: f64,
    rooms: f64,
}

impl MirrorWork {
    async fn read(client: &reqwest::Client, base: &str) -> Self {
        let text = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let name = "hs_user_mirror_catchup_duration_seconds";
        Self {
            full_seconds: sample(&text, &format!("{name}_sum{{kind=\"full\"}}")),
            full_count: sample(&text, &format!("{name}_count{{kind=\"full\"}}")),
            incremental_seconds: sample(&text, &format!("{name}_sum{{kind=\"incremental\"}}")),
            incremental_count: sample(&text, &format!("{name}_count{{kind=\"incremental\"}}")),
            caught_up_events: sample(&text, "hs_user_mirror_catchup_events_total"),
            rooms: sample(&text, "hs_user_mirror_rooms"),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            full_seconds: self.full_seconds - before.full_seconds,
            full_count: self.full_count - before.full_count,
            incremental_seconds: self.incremental_seconds - before.incremental_seconds,
            incremental_count: self.incremental_count - before.incremental_count,
            caught_up_events: self.caught_up_events - before.caught_up_events,
            rooms: self.rooms,
        }
    }

    /// Milliseconds of mirror work per message.
    fn per_event_ms(self, messages: usize) -> f64 {
        (self.full_seconds + self.incremental_seconds) * 1000.0 / messages as f64
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// One replica's results for one phase.
struct Seen {
    work: MirrorWork,
    latencies: Vec<Duration>,
}

impl Seen {
    fn report(&self, label: &str, messages: usize) -> String {
        let mut sorted = self.latencies.clone();
        sorted.sort();
        format!(
            "{label}: mirror work {:.2} ms/event ({} whole loads, {} catch-ups of {} events); \
             write-to-woken-sync p50 {:?} p95 {:?} max {:?}",
            self.work.per_event_ms(messages),
            self.work.full_count,
            self.work.incremental_count,
            self.work.caught_up_events,
            percentile(&sorted, 0.5),
            percentile(&sorted, 0.95),
            sorted.last().copied().unwrap_or_default(),
        )
    }
}

/// The two non-owners as one phase drives them.
struct Readers<'a> {
    client: &'a reqwest::Client,
    owner: &'a str,
    alice: &'a User,
    b1: &'a str,
    bob: &'a User,
    b2: &'a str,
    carol: &'a User,
}

impl Readers<'_> {
    /// `messages` messages from alice on the owner, each awaited in bob's long-poll on B1 and
    /// carol's on B2.
    async fn phase(
        &self,
        label: &str,
        room: &str,
        messages: usize,
        tokens: &mut (String, String),
    ) -> (Seen, Seen) {
        let before = (
            MirrorWork::read(self.client, self.b1).await,
            MirrorWork::read(self.client, self.b2).await,
        );
        let mut latencies = (Vec::new(), Vec::new());
        for i in 0..messages {
            let body = format!("{label} #{i}");
            let on_b1 = tokio::spawn(until_body(
                self.client.clone(),
                self.b1.to_owned(),
                self.bob.clone(),
                tokens.0.clone(),
                room.to_owned(),
                body.clone(),
            ));
            let on_b2 = tokio::spawn(until_body(
                self.client.clone(),
                self.b2.to_owned(),
                self.carol.clone(),
                tokens.1.clone(),
                room.to_owned(),
                body.clone(),
            ));
            // Long enough for both polls to be parked; if one is not yet, the message is simply
            // there when it starts, which only flatters that one sample.
            tokio::time::sleep(Duration::from_millis(40)).await;
            let sent_at = Instant::now();
            send_message(self.client, self.owner, room, self.alice, &body).await;
            let (b1_at, b1_token) = on_b1.await.unwrap();
            let (b2_at, b2_token) = on_b2.await.unwrap();
            latencies.0.push(b1_at.saturating_duration_since(sent_at));
            latencies.1.push(b2_at.saturating_duration_since(sent_at));
            *tokens = (b1_token, b2_token);
        }
        let after = (
            MirrorWork::read(self.client, self.b1).await,
            MirrorWork::read(self.client, self.b2).await,
        );
        (
            Seen {
                work: after.0.since(before.0),
                latencies: latencies.0,
            },
            Seen {
                work: after.1.since(before.1),
                latencies: latencies.1,
            },
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replica_that_does_not_own_a_room_reads_only_its_new_events() {
    let Some(db) = Database::create() else {
        return;
    };
    let events = env_or("HS_MIRROR_BENCH_EVENTS", 150);
    let members = env_or("HS_MIRROR_BENCH_MEMBERS", 10);
    let messages = env_or("HS_MIRROR_BENCH_MESSAGES", 10).max(1);

    let dir = tempfile::tempdir().unwrap();
    let ports: Vec<(u16, u16)> = (0..3).map(|_| (reserve_port(), reserve_port())).collect();
    let configs: Vec<std::path::PathBuf> = ["a", "b1", "b2"]
        .iter()
        .zip(&ports)
        .map(|(name, (port, mesh))| {
            let path = dir.path().join(format!("{name}.yaml"));
            std::fs::write(&path, replica_config(&db, dir.path(), *port, *mesh)).unwrap();
            path
        })
        .collect();
    let bases: Vec<String> = ports
        .iter()
        .map(|(port, _)| format!("http://127.0.0.1:{port}"))
        .collect();
    let ids: Vec<String> = ports
        .iter()
        .map(|(_, mesh)| format!("127.0.0.1:{mesh}"))
        .collect();
    let (a, b1, b2) = (&bases[0], &bases[1], &bases[2]);
    let client = reqwest::Client::new();

    // A first (it makes the signing key and the schema), then the two readers.
    let mut hs_a = HsProcess::serve(&configs[0], &[]);
    let setup_line = hs_a.wait_for("setup_link=");
    let admin = first_admin(&client, a, &setup_line).await;
    let mut hs_b1 = HsProcess::serve(&configs[1], &[("HS_SYNC_MIRROR_FULL_RELOAD", "1")]);
    hs_b1.wait_for("listening");
    let mut hs_b2 = HsProcess::serve(&configs[2], &[]);
    hs_b2.wait_for("listening");

    // Every replica active with shards, and every shard owned.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_owners: Vec<(String, String)> = Vec::new();
    let mut stable_since = Instant::now();
    loop {
        let replicas: Value = client
            .get(format!("{a}/api/v1/cluster/replicas"))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or_default();
        let shards: Value = client
            .get(format!("{a}/api/v1/cluster/shards?limit=500"))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or_default();
        let items = replicas["items"].as_array().cloned().unwrap_or_default();
        let all_active = ids.iter().all(|id| {
            items.iter().any(|r| {
                r["id"] == id.as_str()
                    && r["status"] == "active"
                    && r["shard_count"].as_u64() > Some(0)
            })
        });
        let all_owned = shards["items"]
            .as_array()
            .is_some_and(|s| !s.is_empty() && s.iter().all(|s| !s["owner"].is_null()));
        // ... and no shard has changed hands for ten seconds: right after the third replica
        // becomes active, shards are still moving to it, and a room made then may move.
        let owners: Vec<(String, String)> = shards["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| (s["id"].to_string(), s["owner"].to_string()))
            .collect();
        if all_active && all_owned && owners == last_owners {
            if stable_since.elapsed() >= Duration::from_secs(10) {
                break;
            }
        } else {
            stable_since = Instant::now();
        }
        last_owners = owners;
        assert!(
            Instant::now() < deadline,
            "three active replicas never settled on sharing every shard: {replicas}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let alice = register(&client, a, "alice").await;
    let bob = register(&client, b1, "bob").await;
    let carol = register(&client, b2, "carol").await;

    // Both rooms are owned by A (made until they land there).
    let small = room_owned_by(&client, a, &admin, &alice, &ids[0], "small").await;
    let big = room_owned_by(&client, a, &admin, &alice, &ids[0], "big").await;
    // Bob and carol join before the history is written, so their records exist early and
    // neither reader holds a copy of the big room while it is being filled.
    for room in [&small, &big] {
        join(&client, b1, room, &bob).await;
        join(&client, b2, room, &carol).await;
        let members: Value = client
            .get(format!(
                "{a}/_matrix/client/v3/rooms/{}/joined_members",
                escape(room)
            ))
            .bearer_auth(&alice.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or_default();
        for user in [&bob, &carol] {
            assert!(
                members["joined"].get(&user.id).is_some(),
                "{} is not a member of {room} after joining it: {members}",
                user.id
            );
        }
    }
    let built = Instant::now();
    send_many(&client, a, &big, &alice, events, 4).await;
    eprintln!("sent {events} messages in {:?}", built.elapsed());
    add_members(&client, a, &admin, &big, members, 8).await;
    eprintln!(
        "built the big room ({events} messages, {members} members) in {:?}",
        built.elapsed()
    );
    // The owner's hub is far behind after the members' joins; a marker sent now reaches the
    // readers once it has caught up, and the measurement starts from there.
    let marker = format!("ready {}", rand_suffix());
    send_message(&client, a, &big, &alice, &marker).await;
    eprintln!("waiting for the owner to catch up and the readers' syncs to settle");
    let settle_started = Instant::now();
    let settled = (
        settled_token(&client, b1, &bob, &[&small, &big], &big, &marker).await,
        settled_token(&client, b2, &carol, &[&small, &big], &big, &marker).await,
    );
    eprintln!("settled in {:?}", settle_started.elapsed());
    let mut tokens = match settled {
        (Ok(bob_token), Ok(carol_token)) => (bob_token, carol_token),
        (bob_result, carol_result) => {
            let mut why = format!("{bob_result:?}\n{carol_result:?}\n");
            for room in [&small, &big] {
                why.push_str(&format!(
                    "{room} is owned by {:?}\n",
                    owner_of(&client, a, &admin, room).await
                ));
            }
            for (base, user) in [(a, &bob), (b1, &bob), (a, &carol), (b2, &carol)] {
                let joined: Value = client
                    .get(format!("{base}/_matrix/client/v3/joined_rooms"))
                    .bearer_auth(&user.token)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap_or_default();
                why.push_str(&format!("{} joined_rooms on {base}: {joined}\n", user.id));
            }
            for (name, process) in [("A", &mut hs_a), ("B1", &mut hs_b1), ("B2", &mut hs_b2)] {
                let log = process.drain();
                if let Ok(dir) = std::env::var("HS_MIRROR_LOG_DIR") {
                    let _ = std::fs::write(format!("{dir}/{name}.log"), &log);
                }
                for line in log.lines() {
                    if line.contains(" WARN ")
                        || line.contains(" ERROR ")
                        || line.contains(small.as_str())
                    {
                        why.push_str(&format!("{name}: {line}\n"));
                    }
                }
            }
            panic!("{why}");
        }
    };
    for (base, name) in [(b1, "B1"), (b2, "B2")] {
        assert_eq!(
            MirrorWork::read(&client, base).await.rooms,
            2.0,
            "{name} must read both rooms through its mirror, i.e. own neither"
        );
    }
    for room in [&small, &big] {
        assert_eq!(
            owner_of(&client, a, &admin, room).await.as_deref(),
            Some(ids[0].as_str()),
            "A must still own {room}"
        );
    }

    let readers = Readers {
        client: &client,
        owner: a,
        alice: &alice,
        b1,
        bob: &bob,
        b2,
        carol: &carol,
    };
    let (small_b1, small_b2) = readers.phase("small", &small, messages, &mut tokens).await;
    eprintln!(
        "{}",
        small_b2.report("small room, B2 (incremental, after)", messages)
    );
    let (big_b1, big_b2) = readers.phase("big", &big, messages, &mut tokens).await;

    let report = [
        format!("{messages} messages per phase; big room: {events} messages, {members} members"),
        small_b1.report("small room, B1 (whole reload, before)", messages),
        small_b2.report("small room, B2 (incremental, after)", messages),
        big_b1.report("big room, B1 (whole reload, before)", messages),
        big_b2.report("big room, B2 (incremental, after)", messages),
    ]
    .join("\n");
    eprintln!("{report}");

    for (label, seen) in [("small", &small_b2), ("big", &big_b2)] {
        assert_eq!(
            seen.work.full_count,
            0.0,
            "B2 loaded a room whole during the {label} phase\n{report}\n{}",
            hs_b2.drain()
        );
        assert!(
            seen.work.caught_up_events >= messages as f64,
            "B2 caught up fewer events than were sent in the {label} phase\n{report}"
        );
    }
    for (label, seen) in [("small", &small_b1), ("big", &big_b1)] {
        assert!(
            seen.work.full_count >= messages as f64,
            "B1, with catch-up off, must load the room whole for every message ({label})\n{report}"
        );
    }
    assert!(
        big_b2.work.per_event_ms(messages) < big_b1.work.per_event_ms(messages),
        "reading only the new rows must cost B2 less than reloading costs B1\n{report}"
    );
    drop(hs_a);
}
