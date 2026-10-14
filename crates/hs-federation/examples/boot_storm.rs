//! Reproduces the demo's federation boot freeze of 2026-10-14 on one machine
//! (`docs/status/06-federation.md`, "The boot freeze"): a server whose device-list announcer
//! starts each boot from an old place re-announces its users' whole device lists to every
//! server it shares a room with, and the live ones answer the way Synapse does -- with a
//! `GET /user/devices/{userId}` resync for each user, and their own traffic on top.
//!
//! One program, five commands:
//!
//! ```text
//! boot_storm peers   --count N --base-port P --hs http://127.0.0.1:18408 --hs-name NAME
//!                    [--rooms FILE] [--join] [--state FILE] [--pdus-per-sec R]
//!                    [--edus-per-sec E] [--resync-delay-ms D] [--stats-every S]
//! boot_storm setup   --hs URL --hs-name NAME --users U --devices D --rooms R --out FILE
//!                    [--first-user K] [--join-rooms FILE]
//! boot_storm local-chatter --hs URL --user NAME --rooms FILE [--per-sec R] [--typing-per-sec T]
//! boot_storm reset-announcer <data-dir>/db [position]
//! boot_storm probe   --url URL [--interval-ms 250] [--timeout-ms 5000] [--duration-s 300]
//!                    [--pid PID --sample-dir DIR --sample-over-ms 1000 --max-samples 3]
//!                    [--metrics URL --metrics-grep NAME,NAME]
//! ```
//!
//! - `peers` runs `N` federation peers, `127.0.0.1:P` to `127.0.0.1:P+N-1`, each with its own
//!   Ed25519 key (derived from its index, so a restart keeps it) and TLS (self-signed: the server
//!   under test needs `federation.verify_certificates: false` and an `ip_range_blocklist` that
//!   leaves `127.0.0.0/8` alone). Each serves its key, accepts `/send` (answering every PDU),
//!   and answers a device-list or signing-key update for a user it has not resynced in the last
//!   30 s with a signed `GET /user/devices/{userId}` after a random delay of up to
//!   `--resync-delay-ms`, counting the updates that came with no `prev_id`. With `--join` each
//!   peer first joins every room in `--rooms` over `make_join`/`send_join`, and with `--state`
//!   writes what it learned there (and reads it back on a later run without `--join`). Fewer
//!   peers than joined leaves the rest dead. `--pdus-per-sec` and `--edus-per-sec` add the peers'
//!   own traffic: messages into the rooms, typing, receipts and presence.
//! - `setup` registers `U` users with `D` devices each (keys uploaded), creates `R` public rooms
//!   (room version 10) and joins every user to each, writing the room IDs to `--out`; with
//!   `--join-rooms` the users (named from `--first-user` on) join those rooms instead.
//! - `local-chatter` logs in as a local user and talks in the rooms: `--per-sec` messages and
//!   `--typing-per-sec` typing notices, each one for every server in the room.
//! - `reset-announcer` moves every federation shard's device-list announcer place in a stopped
//!   server's store back to `position` (0 by default): the next boot announces every local user
//!   again from there, as each of the demo's boots did.
//! - `probe` asks `/health/live` every `--interval-ms` and prints each second's slowest answer;
//!   with `--pid`, an answer slower than `--sample-over-ms` runs `/usr/bin/sample` on the server
//!   (macOS) for 10 s into `--sample-dir`; with `--metrics`, the named metric lines are printed
//!   every two seconds.
//!
//! On 2026-10-14, with 700 live peers, 720 dead destinations and 35 users, 0.3.17 froze for 70 s
//! of 90 and the fixed server answered every probe within 29 ms (status 06 has the numbers).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::signing::SigningKeyPair;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rand::Rng;
use serde_json::{Value, json};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn main() -> Result<(), BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().cloned() else {
        return Err("usage: boot_storm <peers|setup|reset-announcer|probe> ...".into());
    };
    let flags = Flags::parse(&args[1..]);
    match command.as_str() {
        "reset-announcer" => reset_announcer(&args[1..]),
        "peers" | "setup" | "probe" | "local-chatter" => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                match command.as_str() {
                    "peers" => run_peers(&flags).await,
                    "setup" => run_setup(&flags).await,
                    "local-chatter" => run_local_chatter(&flags).await,
                    _ => run_probe(&flags).await,
                }
            })
        }
        other => Err(format!("unknown command {other}").into()),
    }
}

/// `--name value` pairs and bare `--flag`s.
struct Flags(HashMap<String, String>);

impl Flags {
    fn parse(args: &[String]) -> Self {
        let mut map = HashMap::new();
        let mut i = 0;
        while i < args.len() {
            if let Some(name) = args[i].strip_prefix("--") {
                match args.get(i + 1) {
                    Some(value) if !value.starts_with("--") => {
                        map.insert(name.to_owned(), value.clone());
                        i += 2;
                    }
                    _ => {
                        map.insert(name.to_owned(), String::new());
                        i += 1;
                    }
                }
            } else {
                i += 1;
            }
        }
        Self(map)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    fn string(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or(default).to_owned()
    }

    fn number<T: std::str::FromStr>(&self, name: &str, default: T) -> T {
        self.get(name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    fn has(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ------------------------------------------------------------------------------------------
// reset-announcer
// ------------------------------------------------------------------------------------------

fn reset_announcer(args: &[String]) -> Result<(), BoxError> {
    use hs_federation::outbound_store::{KvOutboundStore, OutboundStore};
    let db = args
        .first()
        .ok_or("usage: boot_storm reset-announcer <data-dir>/db [position]")?;
    let position: u64 = args.get(1).map_or(Ok(0), |s| s.parse())?;
    let backend = hs_kv::fjall_backend::FjallBackend::open(db)?;
    let store = KvOutboundStore::open(backend.clone())?;
    let mut cursors: Vec<(String, u64)> = (0..1024)
        .map(|shard| {
            (
                format!("device_list_announcer/federation/{shard}"),
                position,
            )
        })
        .collect();
    cursors.push(("device_list_announcer".to_owned(), position));
    store.set_cursors(&cursors)?;
    backend.persist_all()?;
    println!("every device-list announcer place in {db} is now {position}");
    Ok(())
}

// ------------------------------------------------------------------------------------------
// probe
// ------------------------------------------------------------------------------------------

async fn run_probe(flags: &Flags) -> Result<(), BoxError> {
    let url = flags.string("url", "http://127.0.0.1:18408/health/live");
    let interval = Duration::from_millis(flags.number("interval-ms", 250));
    let timeout = Duration::from_millis(flags.number("timeout-ms", 5000));
    let duration = Duration::from_secs(flags.number("duration-s", 300));
    let pid: Option<u32> = flags.get("pid").and_then(|p| p.parse().ok());
    let sample_dir = flags.string("sample-dir", ".");
    let sample_over = Duration::from_millis(flags.number("sample-over-ms", 1000));
    let max_samples: usize = flags.number("max-samples", 3);
    let client = reqwest::Client::builder().timeout(timeout).build()?;
    let started = Instant::now();
    if let Some(metrics_url) = flags.get("metrics").map(str::to_owned) {
        // Selected metric lines every two seconds, beside the probe and not in its way.
        let wanted: Vec<String> = flags
            .string("metrics-grep", "hs_federation_sender_destinations")
            .split(',')
            .map(str::to_owned)
            .collect();
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                let asked = Instant::now();
                let text = match client.get(&metrics_url).send().await {
                    Ok(response) => response.text().await.unwrap_or_default(),
                    Err(error) => format!("metrics: {error}"),
                };
                let picked: Vec<&str> = text
                    .lines()
                    .filter(|line| {
                        !line.starts_with('#')
                            && wanted.iter().any(|w| line.starts_with(w.as_str()))
                    })
                    .collect();
                println!(
                    "metrics t={:6.1}s took_ms={} {}",
                    started.elapsed().as_secs_f64(),
                    asked.elapsed().as_millis(),
                    picked.join(" | ")
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
    let samples_taken = Arc::new(AtomicUsize::new(0));
    let sampling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut second_start = Instant::now();
    let (mut worst, mut count, mut failures) = (Duration::ZERO, 0u32, 0u32);
    let mut overall_worst = Duration::ZERO;
    let mut over_1s = 0u32;
    while started.elapsed() < duration {
        let asked = Instant::now();
        let outcome = client.get(&url).send().await;
        let took = asked.elapsed();
        count += 1;
        match outcome {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                failures += 1;
                eprintln!("probe: HTTP {}", response.status());
            }
            Err(error) => {
                failures += 1;
                eprintln!("probe: {error}");
            }
        }
        worst = worst.max(took);
        overall_worst = overall_worst.max(took);
        if took > Duration::from_secs(1) {
            over_1s += 1;
        }
        if let Some(pid) = pid
            && took > sample_over
            && samples_taken.load(Ordering::Acquire) < max_samples
            && !sampling.swap(true, Ordering::AcqRel)
        {
            let n = samples_taken.fetch_add(1, Ordering::AcqRel);
            let file = format!("{sample_dir}/sample-{n}.txt");
            let sampling = sampling.clone();
            eprintln!(
                "probe: {} ms answer at {:.1} s; sampling pid {pid} into {file}",
                took.as_millis(),
                started.elapsed().as_secs_f64()
            );
            tokio::task::spawn_blocking(move || {
                let _ = std::process::Command::new("/usr/bin/sample")
                    .args([&pid.to_string(), "10", "-mayDie", "-file", &file])
                    .output();
                sampling.store(false, Ordering::Release);
            });
        }
        if second_start.elapsed() >= Duration::from_secs(1) {
            println!(
                "probe t={:6.1}s answers={count:3} failures={failures} worst_ms={}",
                started.elapsed().as_secs_f64(),
                worst.as_millis()
            );
            second_start = Instant::now();
            worst = Duration::ZERO;
            count = 0;
            failures = 0;
        }
        tokio::time::sleep(interval.saturating_sub(took)).await;
    }
    println!(
        "probe done: worst_ms={} answers_over_1s={over_1s}",
        overall_worst.as_millis()
    );
    Ok(())
}

// ------------------------------------------------------------------------------------------
// setup (client API)
// ------------------------------------------------------------------------------------------

async fn run_setup(flags: &Flags) -> Result<(), BoxError> {
    let hs = flags.string("hs", "http://127.0.0.1:18408");
    let hs_name = flags.string("hs-name", "boot-storm.test");
    let users: usize = flags.number("users", 5);
    let devices: usize = flags.number("devices", 4);
    let rooms: usize = flags.number("rooms", 3);
    let out = flags.string("out", "rooms.txt");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    let first: usize = flags.number("first-user", 0);
    let mut tokens: Vec<String> = Vec::new();
    for u in first..first + users {
        let username = format!("storm{u}");
        let password = "boot-storm-password";
        let token = register(&client, &hs, &username, password).await?;
        upload_keys(&client, &hs, &token, &hs_name, &username).await?;
        for _ in 1..devices {
            let body = json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": username},
                "password": password,
            });
            let response: Value = client
                .post(format!("{hs}/_matrix/client/v3/login"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;
            let device_token = response["access_token"]
                .as_str()
                .ok_or_else(|| format!("login failed: {response}"))?
                .to_owned();
            upload_keys(&client, &hs, &device_token, &hs_name, &username).await?;
        }
        tokens.push(token);
        println!("setup: user {username} with {devices} devices");
    }

    if let Some(existing) = flags.get("join-rooms") {
        // More users for rooms made before: each joins every one of them.
        let rooms: Vec<String> = std::fs::read_to_string(existing)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        for room_id in &rooms {
            for token in &tokens {
                let response = client
                    .post(format!("{hs}/_matrix/client/v3/join/{room_id}"))
                    .bearer_auth(token)
                    .json(&json!({}))
                    .send()
                    .await?;
                if !response.status().is_success() {
                    return Err(format!("join failed: {}", response.text().await?).into());
                }
            }
        }
        println!("setup: {} users joined {} rooms", tokens.len(), rooms.len());
        return Ok(());
    }
    let mut room_ids = Vec::new();
    for r in 0..rooms {
        let creator = tokens.first().ok_or("no users")?;
        let response: Value = client
            .post(format!("{hs}/_matrix/client/v3/createRoom"))
            .bearer_auth(creator)
            .json(&json!({
                "preset": "public_chat",
                "room_version": "10",
                "name": format!("boot storm {r}"),
            }))
            .send()
            .await?
            .json()
            .await?;
        let room_id = response["room_id"]
            .as_str()
            .ok_or_else(|| format!("createRoom failed: {response}"))?
            .to_owned();
        for token in tokens.iter().skip(1) {
            let response = client
                .post(format!("{hs}/_matrix/client/v3/join/{room_id}"))
                .bearer_auth(token)
                .json(&json!({}))
                .send()
                .await?;
            if !response.status().is_success() {
                return Err(format!("join failed: {}", response.text().await?).into());
            }
        }
        println!("setup: room {room_id}");
        room_ids.push(room_id);
    }
    std::fs::write(&out, room_ids.join("\n") + "\n")?;
    println!("setup: wrote {} rooms to {out}", room_ids.len());
    Ok(())
}

/// A local user talking in the rooms, `--per-sec` messages a second (each one goes to every
/// server in the room), until killed.
async fn run_local_chatter(flags: &Flags) -> Result<(), BoxError> {
    let hs = flags.string("hs", "http://127.0.0.1:18408");
    let user = flags.string("user", "storm0");
    let per_sec: f64 = flags.number("per-sec", 1.0);
    let rooms: Vec<String> = std::fs::read_to_string(flags.string("rooms", "rooms.txt"))?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let login: Value = client
        .post(format!("{hs}/_matrix/client/v3/login"))
        .json(&json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": user},
            "password": "boot-storm-password",
        }))
        .send()
        .await?
        .json()
        .await?;
    let token = login["access_token"]
        .as_str()
        .ok_or_else(|| format!("login: {login}"))?
        .to_owned();
    let typing_per_sec: f64 = flags.number("typing-per-sec", 0.0);
    if typing_per_sec > 0.0 {
        // Typing notices, each one an EDU for every server in the room.
        let (client, hs, token, user, rooms) = (
            client.clone(),
            hs.clone(),
            token.clone(),
            login["user_id"].as_str().unwrap_or_default().to_owned(),
            rooms.clone(),
        );
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / typing_per_sec));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut n = 0usize;
            loop {
                interval.tick().await;
                n += 1;
                let room = &rooms[n % rooms.len()];
                let started = Instant::now();
                let (client, hs, token, user, room) = (
                    client.clone(),
                    hs.clone(),
                    token.clone(),
                    user.clone(),
                    room.clone(),
                );
                tokio::spawn(async move {
                    let outcome = client
                        .put(format!("{hs}/_matrix/client/v3/rooms/{room}/typing/{user}"))
                        .bearer_auth(&token)
                        .json(&json!({"typing": n.is_multiple_of(2), "timeout": 30000}))
                        .send()
                        .await;
                    let took = started.elapsed().as_millis();
                    match outcome {
                        Ok(response) if response.status().is_success() => {
                            if took > 1000 {
                                println!("local-chatter: typing {n} took {took} ms");
                            }
                        }
                        Ok(response) => {
                            println!("local-chatter: typing HTTP {}", response.status())
                        }
                        Err(error) => println!("local-chatter: typing {error}"),
                    }
                });
            }
        });
    }
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / per_sec.max(0.01)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n = 0usize;
    loop {
        interval.tick().await;
        n += 1;
        let room = &rooms[n % rooms.len()];
        let started = Instant::now();
        let outcome = client
            .put(format!(
                "{hs}/_matrix/client/v3/rooms/{room}/send/m.room.message/storm{n}-{}",
                now_ms()
            ))
            .bearer_auth(&token)
            .json(&json!({"msgtype": "m.text", "body": format!("local message {n}")}))
            .send()
            .await;
        let took = started.elapsed().as_millis();
        match outcome {
            Ok(response) if response.status().is_success() => {
                if took > 1000 {
                    println!("local-chatter: message {n} took {took} ms");
                }
            }
            Ok(response) => println!("local-chatter: HTTP {}", response.status()),
            Err(error) => println!("local-chatter: {error}"),
        }
    }
}

async fn register(
    client: &reqwest::Client,
    hs: &str,
    username: &str,
    password: &str,
) -> Result<String, BoxError> {
    let mut body = json!({
        "username": username,
        "password": password,
        "auth": {"type": "m.login.dummy"},
    });
    for _ in 0..3 {
        let response = client
            .post(format!("{hs}/_matrix/client/v3/register"))
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let answer: Value = response.json().await?;
        if status.is_success() {
            return answer["access_token"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("register answered no token: {answer}").into());
        }
        if status == StatusCode::UNAUTHORIZED
            && let Some(session) = answer["session"].as_str()
        {
            body["auth"] = json!({"type": "m.login.dummy", "session": session});
            continue;
        }
        return Err(format!("register {username}: {status} {answer}").into());
    }
    Err(format!("register {username}: no luck after three tries").into())
}

/// Uploads a device's identity keys, signed with its own Ed25519 key the way a client does.
async fn upload_keys(
    client: &reqwest::Client,
    hs: &str,
    token: &str,
    hs_name: &str,
    username: &str,
) -> Result<(), BoxError> {
    let whoami: Value = client
        .get(format!("{hs}/_matrix/client/v3/account/whoami"))
        .bearer_auth(token)
        .send()
        .await?
        .json()
        .await?;
    let device_id = whoami["device_id"]
        .as_str()
        .ok_or_else(|| format!("whoami: {whoami}"))?
        .to_owned();
    let user_id = format!("@{username}:{hs_name}");
    let signing = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
    let ed = base64_unpadded(signing.verifying_key().as_bytes());
    let curve = base64_unpadded(&rand::random::<[u8; 32]>());
    let mut keys = json!({
        "user_id": user_id,
        "device_id": device_id,
        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        "keys": {
            format!("curve25519:{device_id}"): curve,
            format!("ed25519:{device_id}"): ed,
        },
    });
    let canonical = to_canonical_object(&keys, true)?;
    let bytes = CanonicalJsonValue::Object(canonical).to_canonical_bytes();
    let pair = SigningKeyPair::new(device_id.clone(), signing);
    let signature = hs_model::signing::sign_bytes(&bytes, &pair);
    keys["signatures"] = json!({
        user_id.clone(): { format!("ed25519:{device_id}"): base64_unpadded(&signature.to_bytes()) }
    });
    let response = client
        .post(format!("{hs}/_matrix/client/v3/keys/upload"))
        .bearer_auth(token)
        .json(&json!({"device_keys": keys}))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("keys/upload: {}", response.text().await?).into());
    }
    Ok(())
}

fn base64_unpadded(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

// ------------------------------------------------------------------------------------------
// peers
// ------------------------------------------------------------------------------------------

/// What a peer knows of one room it joined: the events its own events cite.
#[derive(Clone)]
struct PeerRoom {
    create: String,
    power_levels: String,
    join: String,
}

struct Peer {
    name: String,
    user_id: String,
    key: SigningKeyPair,
    key_response: Value,
    rooms: Mutex<HashMap<String, PeerRoom>>,
    resynced: Mutex<HashMap<String, Instant>>,
}

/// Counters every peer adds to, printed every `--stats-every` seconds.
#[derive(Default)]
struct Stats {
    txns_in: AtomicU64,
    device_list_edus_in: AtomicU64,
    /// Device-list updates with no `prev_id`: each makes a real server ask for the whole list.
    no_prev_id: AtomicU64,
    resyncs_ok: AtomicU64,
    resyncs_failed: AtomicU64,
    resync_ms_total: AtomicU64,
    resync_ms_max: AtomicU64,
    sends_ok: AtomicU64,
    sends_failed: AtomicU64,
    send_ms_total: AtomicU64,
    send_ms_max: AtomicU64,
    joins_ok: AtomicU64,
    joins_failed: AtomicU64,
}

impl Stats {
    fn record(&self, ok: bool, took: Duration, resync: bool) {
        let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        let (oks, fails, total, max) = if resync {
            (
                &self.resyncs_ok,
                &self.resyncs_failed,
                &self.resync_ms_total,
                &self.resync_ms_max,
            )
        } else {
            (
                &self.sends_ok,
                &self.sends_failed,
                &self.send_ms_total,
                &self.send_ms_max,
            )
        };
        if ok {
            oks.fetch_add(1, Ordering::Relaxed);
        } else {
            fails.fetch_add(1, Ordering::Relaxed);
        }
        total.fetch_add(ms, Ordering::Relaxed);
        max.fetch_max(ms, Ordering::Relaxed);
    }
}

struct World {
    hs: String,
    hs_name: String,
    client: reqwest::Client,
    stats: Stats,
    /// Each room's newest event as the peers know it: what their next message cites.
    heads: Mutex<HashMap<String, (String, u64)>>,
    resync_delay_ms: u64,
    rules: hs_model::room_version::RoomVersionRules,
}

async fn run_peers(flags: &Flags) -> Result<(), BoxError> {
    let count: usize = flags.number("count", 100);
    let base_port: u16 = flags.number("base-port", 20_000);
    let version = ruma::RoomVersionId::V10;
    let world = Arc::new(World {
        hs: flags.string("hs", "http://127.0.0.1:18408"),
        hs_name: flags.string("hs-name", "boot-storm.test"),
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(1024)
            .build()?,
        stats: Stats::default(),
        heads: Mutex::new(HashMap::new()),
        resync_delay_ms: flags.number("resync-delay-ms", 2000),
        rules: hs_model::room_version::rules_for(&version).ok_or("room version 10")?,
    });
    let tls = tls_acceptor()?;
    let mut peers = Vec::with_capacity(count);
    for index in 0..count {
        let port = base_port + u16::try_from(index)?;
        let name = format!("127.0.0.1:{port}");
        let seed: [u8; 32] = sha2_seed(&format!("boot-storm-peer-{index}"));
        let key = SigningKeyPair::new("storm", ed25519_dalek::SigningKey::from_bytes(&seed));
        let own = hs_federation::keys::OwnSigningKeys::from_keys(vec![key.clone()]);
        let key_response =
            hs_federation::keys::build_server_key_response(&name, &own, &[], 7 * 24 * 3600)?;
        let peer = Arc::new(Peer {
            user_id: format!("@peer:{name}"),
            name,
            key,
            key_response,
            rooms: Mutex::new(HashMap::new()),
            resynced: Mutex::new(HashMap::new()),
        });
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
        tokio::spawn(serve_peer(
            listener,
            tls.clone(),
            peer.clone(),
            world.clone(),
        ));
        peers.push(peer);
    }
    println!("peers: {count} listening from port {base_port}");

    let rooms: Vec<String> = match flags.get("rooms") {
        Some(path) => std::fs::read_to_string(path)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        None => Vec::new(),
    };
    let state_file = flags.get("state").map(str::to_owned);
    if flags.has("join") {
        join_all(&world, &peers, &rooms, flags.number("join-concurrency", 16)).await;
        if let Some(path) = &state_file {
            save_state(&world, &peers, path)?;
        }
    } else if let Some(path) = &state_file {
        // What the peers learned when they joined, so a restarted peer process can talk in
        // the rooms without joining again.
        load_state(&world, &peers, path)?;
    }

    let stats_every = Duration::from_secs(flags.number("stats-every", 5));
    tokio::spawn(print_stats(world.clone(), stats_every));

    let pdus_per_sec: f64 = flags.number("pdus-per-sec", 0.0);
    let edus_per_sec: f64 = flags.number("edus-per-sec", 0.0);
    if pdus_per_sec > 0.0 && !rooms.is_empty() {
        tokio::spawn(chatter_pdus(
            world.clone(),
            peers.clone(),
            rooms.clone(),
            pdus_per_sec,
        ));
    }
    if edus_per_sec > 0.0 && !rooms.is_empty() {
        tokio::spawn(chatter_edus(
            world.clone(),
            peers.clone(),
            rooms.clone(),
            edus_per_sec,
        ));
    }
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// Writes what each peer knows of its rooms, and the rooms' heads, to `path`.
fn save_state(world: &World, peers: &[Arc<Peer>], path: &str) -> Result<(), BoxError> {
    let mut known = serde_json::Map::new();
    for peer in peers {
        let rooms = peer.rooms.lock().unwrap_or_else(|e| e.into_inner());
        let entry: serde_json::Map<String, Value> = rooms
            .iter()
            .map(|(room, r)| (room.clone(), json!([r.create, r.power_levels, r.join])))
            .collect();
        known.insert(peer.name.clone(), Value::Object(entry));
    }
    let heads = world.heads.lock().unwrap_or_else(|e| e.into_inner());
    let heads: serde_json::Map<String, Value> = heads
        .iter()
        .map(|(room, (id, depth))| (room.clone(), json!([id, depth])))
        .collect();
    std::fs::write(
        path,
        serde_json::to_vec(&json!({"peers": known, "heads": heads}))?,
    )?;
    println!("peers: state saved to {path}");
    Ok(())
}

/// Reads what [`save_state`] wrote back into `peers` and `world`.
fn load_state(world: &World, peers: &[Arc<Peer>], path: &str) -> Result<(), BoxError> {
    let state: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    for peer in peers {
        let Some(rooms) = state["peers"][&peer.name].as_object() else {
            continue;
        };
        let mut known = peer.rooms.lock().unwrap_or_else(|e| e.into_inner());
        for (room, ids) in rooms {
            let id = |i: usize| ids[i].as_str().unwrap_or_default().to_owned();
            known.insert(
                room.clone(),
                PeerRoom {
                    create: id(0),
                    power_levels: id(1),
                    join: id(2),
                },
            );
        }
    }
    if let Some(heads) = state["heads"].as_object() {
        let mut world_heads = world.heads.lock().unwrap_or_else(|e| e.into_inner());
        for (room, head) in heads {
            if let (Some(id), Some(depth)) = (head[0].as_str(), head[1].as_u64()) {
                world_heads.insert(room.clone(), (id.to_owned(), depth));
            }
        }
    }
    println!("peers: state loaded from {path}");
    Ok(())
}

fn sha2_seed(text: &str) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(text.as_bytes()).into()
}

fn tls_acceptor() -> Result<tokio_rustls::TlsAcceptor, BoxError> {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let cert_der = rustls_pki_types::CertificateDer::from(cert.cert.der().to_vec());
    let key_der = rustls_pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert_der], key_der)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

async fn serve_peer(
    listener: tokio::net::TcpListener,
    tls: tokio_rustls::TlsAcceptor,
    peer: Arc<Peer>,
    world: Arc<World>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let tls = tls.clone();
        let peer = peer.clone();
        let world = world.clone();
        tokio::spawn(async move {
            let Ok(stream) = tls.accept(stream).await else {
                return;
            };
            let service = hyper::service::service_fn(move |request| {
                let peer = peer.clone();
                let world = world.clone();
                async move { Ok::<_, Infallible>(answer(&peer, &world, request).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
        });
    }
}

fn reply(status: StatusCode, body: &Value) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

async fn answer(
    peer: &Arc<Peer>,
    world: &Arc<World>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let body = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };
    if path == "/_matrix/key/v2/server" || path.starts_with("/_matrix/key/v2/server/") {
        return reply(StatusCode::OK, &peer.key_response);
    }
    if path == "/_matrix/federation/v1/version" {
        return reply(
            StatusCode::OK,
            &json!({"server": {"name": "boot-storm-peer", "version": "0"}}),
        );
    }
    if method == hyper::Method::PUT && path.starts_with("/_matrix/federation/v1/send/") {
        world.stats.txns_in.fetch_add(1, Ordering::Relaxed);
        let transaction: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        receive_transaction(peer, world, &transaction);
        return reply(StatusCode::OK, &json!({"pdus": {}}));
    }
    if let Some(user) = path.strip_prefix("/_matrix/federation/v1/user/devices/") {
        return reply(
            StatusCode::OK,
            &json!({"user_id": user, "stream_id": 1, "devices": []}),
        );
    }
    if path == "/_matrix/federation/v1/user/keys/query" {
        return reply(StatusCode::OK, &json!({"device_keys": {}}));
    }
    if path.starts_with("/_matrix/federation/v1/query/profile") {
        return reply(StatusCode::OK, &json!({}));
    }
    reply(
        StatusCode::NOT_FOUND,
        &json!({"errcode": "M_UNRECOGNIZED", "error": "boot_storm peer"}),
    )
}

/// What a peer does with a transaction from the server: notes each room's newest event, and
/// resyncs each user whose device list or signing keys changed (as Synapse does on an update
/// whose `prev_id` it cannot place).
fn receive_transaction(peer: &Arc<Peer>, world: &Arc<World>, transaction: &Value) {
    if let Some(pdus) = transaction["pdus"].as_array() {
        for pdu in pdus {
            let (Some(room), Some(depth)) = (pdu["room_id"].as_str(), pdu["depth"].as_u64()) else {
                continue;
            };
            let Ok(object) = to_canonical_object(pdu, true) else {
                continue;
            };
            let Ok(event_id) = hs_model::hash::derive_event_id(&object, &world.rules) else {
                continue;
            };
            let mut heads = world.heads.lock().unwrap_or_else(|e| e.into_inner());
            let head = heads
                .entry(room.to_owned())
                .or_insert((event_id.clone(), 0));
            if depth > head.1 {
                *head = (event_id, depth);
            }
        }
    }
    let Some(edus) = transaction["edus"].as_array() else {
        return;
    };
    for edu in edus {
        let edu_type = edu["edu_type"].as_str().unwrap_or_default();
        if edu_type != "m.device_list_update" && edu_type != "m.signing_key_update" {
            continue;
        }
        world
            .stats
            .device_list_edus_in
            .fetch_add(1, Ordering::Relaxed);
        if edu_type == "m.device_list_update"
            && edu["content"]["prev_id"]
                .as_array()
                .is_none_or(Vec::is_empty)
        {
            world.stats.no_prev_id.fetch_add(1, Ordering::Relaxed);
        }
        let Some(user) = edu["content"]["user_id"].as_str() else {
            continue;
        };
        {
            let mut resynced = peer.resynced.lock().unwrap_or_else(|e| e.into_inner());
            if resynced
                .get(user)
                .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
            {
                continue;
            }
            resynced.insert(user.to_owned(), Instant::now());
        }
        let peer = peer.clone();
        let world = world.clone();
        let user = user.to_owned();
        tokio::spawn(async move {
            let delay = rand::rng().random_range(0..=world.resync_delay_ms.max(1));
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let path = format!("/_matrix/federation/v1/user/devices/{user}");
            let started = Instant::now();
            let outcome = signed(&world, &peer, "GET", &path, None).await;
            let ok = matches!(&outcome, Ok((200, _)));
            if let Err(error) = &outcome {
                eprintln!("resync {user} from {}: {error}", peer.name);
            }
            world.stats.record(ok, started.elapsed(), true);
        });
    }
}

/// Sends a request to the server signed as `peer`.
async fn signed(
    world: &World,
    peer: &Peer,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), BoxError> {
    let auth = hs_federation::xmatrix::sign_request(
        method,
        path,
        &peer.name,
        &world.hs_name,
        body,
        &peer.key,
    )?;
    let url = format!("{}{path}", world.hs);
    let mut request = world
        .client
        .request(reqwest::Method::from_bytes(method.as_bytes())?, url)
        .header("Authorization", auth);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, value))
}

/// Signs an event template as `peer`: the content hash, then the signature over the redacted
/// form, as `crates/hs-cli/tests/federation_reads.rs`'s `sign_as_remote` does. Returns the
/// event and its ID.
fn sign_event(world: &World, peer: &Peer, template: &Value) -> Result<(Value, String), BoxError> {
    let rules = &world.rules;
    let mut object = to_canonical_object(template, rules.strict_canonical_json)?;
    object.remove("signatures");
    object.remove("hashes");
    object.remove("unsigned");
    let hash = hs_model::hash::content_hash_base64(&object);
    object.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(CanonicalJsonObject::from([(
            "sha256".to_owned(),
            CanonicalJsonValue::String(hash),
        )])),
    );
    let server = ruma::ServerName::parse(&peer.name)?;
    let mut redacted = hs_model::redaction::redact(&object, &rules.redaction)?;
    hs_model::signing::sign_object(&mut redacted, &server, &peer.key)?;
    object.insert(
        "signatures".to_owned(),
        redacted
            .remove("signatures")
            .ok_or("signing inserted no signatures")?,
    );
    let event_id = hs_model::hash::derive_event_id(&object, rules)?;
    let event = serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes())?;
    Ok((event, event_id))
}

async fn join_all(world: &Arc<World>, peers: &[Arc<Peer>], rooms: &[String], concurrency: usize) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let mut tasks = Vec::new();
    for peer in peers {
        for room in rooms {
            let (world, peer, room, semaphore) =
                (world.clone(), peer.clone(), room.clone(), semaphore.clone());
            tasks.push(tokio::spawn(async move {
                let Ok(_permit) = semaphore.acquire().await else {
                    return;
                };
                match join(&world, &peer, &room).await {
                    Ok(()) => {
                        world.stats.joins_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        world.stats.joins_failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("join {room} as {}: {error}", peer.name);
                    }
                }
            }));
        }
    }
    let started = Instant::now();
    for task in tasks {
        let _ = task.await;
    }
    println!(
        "peers: joins ok={} failed={} in {:.1} s",
        world.stats.joins_ok.load(Ordering::Relaxed),
        world.stats.joins_failed.load(Ordering::Relaxed),
        started.elapsed().as_secs_f64()
    );
}

async fn join(world: &World, peer: &Peer, room: &str) -> Result<(), BoxError> {
    let versions: String = (1..=12).map(|v| format!("ver={v}&")).collect();
    let path = format!(
        "/_matrix/federation/v1/make_join/{room}/{}?{}",
        peer.user_id,
        versions.trim_end_matches('&')
    );
    let (status, made) = signed(world, peer, "GET", &path, None).await?;
    if status != 200 {
        return Err(format!("make_join: {status} {made}").into());
    }
    let mut template = made["event"].clone();
    template["origin_server_ts"] = json!(now_ms());
    let depth = template["depth"].as_u64().unwrap_or(0);
    let (event, event_id) = sign_event(world, peer, &template)?;
    let path = format!("/_matrix/federation/v2/send_join/{room}/{event_id}");
    let (status, joined) = signed(world, peer, "PUT", &path, Some(&event)).await?;
    if status != 200 {
        return Err(format!("send_join: {status} {joined}").into());
    }
    let mut create = None;
    let mut power_levels = None;
    for state in joined["state"].as_array().into_iter().flatten() {
        let event_type = state["type"].as_str().unwrap_or_default();
        if event_type != "m.room.create" && event_type != "m.room.power_levels" {
            continue;
        }
        let object = to_canonical_object(state, true)?;
        let id = hs_model::hash::derive_event_id(&object, &world.rules)?;
        if event_type == "m.room.create" {
            create = Some(id);
        } else {
            power_levels = Some(id);
        }
    }
    let (Some(create), Some(power_levels)) = (create, power_levels) else {
        return Err("send_join answered no create or power levels".into());
    };
    peer.rooms.lock().unwrap_or_else(|e| e.into_inner()).insert(
        room.to_owned(),
        PeerRoom {
            create,
            power_levels,
            join: event_id.clone(),
        },
    );
    let mut heads = world.heads.lock().unwrap_or_else(|e| e.into_inner());
    let head = heads
        .entry(room.to_owned())
        .or_insert((event_id.clone(), 0));
    if depth > head.1 {
        *head = (event_id, depth);
    }
    Ok(())
}

/// Each peer's messages into the rooms, `rate` a second across all of them, one PDU to a
/// transaction, citing the room's newest event the peers know of.
async fn chatter_pdus(world: Arc<World>, peers: Vec<Arc<Peer>>, rooms: Vec<String>, rate: f64) {
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / rate));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n: usize = 0;
    loop {
        interval.tick().await;
        n += 1;
        let peer = peers[n % peers.len()].clone();
        let room = rooms[(n / peers.len()) % rooms.len()].clone();
        let Some(known) = peer
            .rooms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&room)
            .cloned()
        else {
            continue;
        };
        let world = world.clone();
        tokio::spawn(async move {
            let (prev, depth) = {
                let heads = world.heads.lock().unwrap_or_else(|e| e.into_inner());
                match heads.get(&room) {
                    Some(head) => head.clone(),
                    None => return,
                }
            };
            let template = json!({
                "type": "m.room.message",
                "room_id": room,
                "sender": peer.user_id,
                "origin": peer.name,
                "origin_server_ts": now_ms(),
                "depth": depth + 1,
                "prev_events": [prev],
                "auth_events": [known.create, known.power_levels, known.join],
                "content": {"msgtype": "m.text", "body": format!("storm message {n}")},
            });
            let Ok((event, event_id)) = sign_event(&world, &peer, &template) else {
                return;
            };
            {
                let mut heads = world.heads.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(head) = heads.get_mut(&room)
                    && depth + 1 > head.1
                {
                    *head = (event_id, depth + 1);
                }
            }
            let txn = json!({
                "origin": peer.name,
                "origin_server_ts": now_ms(),
                "pdus": [event],
                "edus": [],
            });
            send_txn(&world, &peer, &txn).await;
        });
    }
}

/// Typing, receipts and presence from the peers, `rate` EDUs a second across all of them.
async fn chatter_edus(world: Arc<World>, peers: Vec<Arc<Peer>>, rooms: Vec<String>, rate: f64) {
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / rate));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n: usize = 0;
    loop {
        interval.tick().await;
        n += 1;
        let peer = peers[(n * 7) % peers.len()].clone();
        let room = rooms[n % rooms.len()].clone();
        let world = world.clone();
        let head = world
            .heads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&room)
            .map(|(id, _)| id.clone());
        tokio::spawn(async move {
            let edu = match n % 3 {
                0 => json!({"edu_type": "m.typing", "content": {
                    "room_id": room, "user_id": peer.user_id, "typing": true}}),
                1 => match head {
                    Some(event_id) => json!({"edu_type": "m.receipt", "content": {
                        room.clone(): {"m.read": {peer.user_id.clone(): {
                            "event_ids": [event_id], "data": {"ts": now_ms()}}}}}}),
                    None => return,
                },
                _ => json!({"edu_type": "m.presence", "content": {"push": [{
                    "user_id": peer.user_id, "presence": "online",
                    "last_active_ago": 1000, "currently_active": true}]}}),
            };
            let txn = json!({
                "origin": peer.name,
                "origin_server_ts": now_ms(),
                "pdus": [],
                "edus": [edu],
            });
            send_txn(&world, &peer, &txn).await;
        });
    }
}

static TXN: AtomicU64 = AtomicU64::new(0);

async fn send_txn(world: &World, peer: &Peer, txn: &Value) {
    let id = format!("{}-{}", now_ms(), TXN.fetch_add(1, Ordering::Relaxed));
    let path = format!("/_matrix/federation/v1/send/{id}");
    let started = Instant::now();
    let outcome = signed(world, peer, "PUT", &path, Some(txn)).await;
    let ok = matches!(&outcome, Ok((200, _)));
    match &outcome {
        Ok((200, _)) => {}
        Ok((status, body)) => eprintln!("send from {}: {status} {body}", peer.name),
        Err(error) => eprintln!("send from {}: {error}", peer.name),
    }
    world.stats.record(ok, started.elapsed(), false);
}

async fn print_stats(world: Arc<World>, every: Duration) {
    let started = Instant::now();
    loop {
        tokio::time::sleep(every).await;
        let s = &world.stats;
        let resyncs =
            s.resyncs_ok.load(Ordering::Relaxed) + s.resyncs_failed.load(Ordering::Relaxed);
        let sends = s.sends_ok.load(Ordering::Relaxed) + s.sends_failed.load(Ordering::Relaxed);
        println!(
            "peers t={:6.1}s txns_in={} device_list_edus_in={} without_prev_id={} resyncs ok={} failed={} avg_ms={} max_ms={} sends ok={} failed={} avg_ms={} max_ms={}",
            started.elapsed().as_secs_f64(),
            s.txns_in.load(Ordering::Relaxed),
            s.device_list_edus_in.load(Ordering::Relaxed),
            s.no_prev_id.load(Ordering::Relaxed),
            s.resyncs_ok.load(Ordering::Relaxed),
            s.resyncs_failed.load(Ordering::Relaxed),
            s.resync_ms_total.load(Ordering::Relaxed) / resyncs.max(1),
            s.resync_ms_max.swap(0, Ordering::Relaxed),
            s.sends_ok.load(Ordering::Relaxed),
            s.sends_failed.load(Ordering::Relaxed),
            s.send_ms_total.load(Ordering::Relaxed) / sends.max(1),
            s.send_ms_max.swap(0, Ordering::Relaxed),
        );
    }
}
