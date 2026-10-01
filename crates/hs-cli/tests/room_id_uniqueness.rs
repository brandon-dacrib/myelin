//! Version-12 rooms created at once by one user are as many rooms as were asked for, against
//! the real `hs` binary.
//!
//! A version-12 room's ID is its create event's hash, and the create event holds the sender,
//! the content and `origin_server_ts`: two `createRoom` calls by one user with the same body in
//! the same millisecond built one create event, and both were answered with one room's ID
//! (Sytest's "/joined_rooms returns only joined rooms" and "Events come down the correct room"
//! failed from it). Here alice sends twenty identical version-12 `createRoom` requests at once,
//! in bursts until the server has seen at least one ID taken (`/metrics`), and every burst must
//! be twenty new rooms, all in her `/joined_rooms`.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
            .env_remove("HS_DATA_DIR")
            .env("RUST_LOG", "info")
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

    /// Every line logged so far that contains `needle`.
    fn logged(&mut self, needle: &str) -> Vec<String> {
        while let Ok(line) = self.lines.try_recv() {
            self.seen.push(line);
        }
        self.seen
            .iter()
            .filter(|line| line.contains(needle))
            .cloned()
            .collect()
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config_yaml(port: u16, data_dir: &std::path::Path) -> String {
    // No rate limits: the twenty requests must reach the handler at once, not be spread out
    // by `429`s and their retries.
    format!(
        "server:\n  server_name: \"ids.example.org\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n",
        media = data_dir.join("media"),
    )
}

async fn register(http: &reqwest::Client, base: &str, username: &str) -> String {
    let first: Value = http
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let done: Value = http
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": first["session"]},
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

async fn metric(http: &reqwest::Client, base: &str, sample: &str) -> f64 {
    let text = http
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .find_map(|line| line.strip_prefix(sample)?.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Twenty identical version-12 `createRoom` requests by one user, all sent before any is
/// answered. Returns the room IDs answered, in no order.
async fn burst(http: &reqwest::Client, base: &str, token: &str) -> Vec<String> {
    let requests: Vec<_> = (0..20)
        .map(|_| {
            let request = http
                .post(format!("{base}/_matrix/client/v3/createRoom"))
                .bearer_auth(token)
                .json(&json!({"room_version": "12"}));
            tokio::spawn(async move {
                let response = request.send().await.unwrap();
                let status = response.status();
                let body: Value = response.json().await.unwrap_or(Value::Null);
                assert!(status.is_success(), "createRoom: {status} {body}");
                body["room_id"].as_str().unwrap().to_owned()
            })
        })
        .collect();
    let mut ids = Vec::new();
    for request in requests {
        ids.push(request.await.unwrap());
    }
    ids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_v12_rooms_created_at_once_by_one_user_are_twenty_rooms() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_port();
    let config = dir.path().join("hs.yaml");
    std::fs::write(&config, config_yaml(port, &dir.path().join("data"))).unwrap();
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let mut hs = HsProcess::serve(&config);
    hs.wait_for("listening");

    let alice = register(&http, &base, "alice").await;

    // Bursts until one of them has made the server find an ID taken, so that the path this
    // test is about has run; usually the first does. Each burst is twenty new rooms either way.
    let mut created = BTreeSet::new();
    let mut bursts = 0;
    loop {
        bursts += 1;
        let ids = burst(&http, &base, &alice).await;
        let distinct: BTreeSet<_> = ids.iter().cloned().collect();
        assert_eq!(
            distinct.len(),
            20,
            "burst {bursts}: twenty createRoom calls answered {} distinct room ids: {ids:?}",
            distinct.len()
        );
        assert!(
            distinct.iter().all(|id| !created.contains(id)),
            "burst {bursts} answered a room an earlier burst created"
        );
        created.extend(distinct);
        let taken = metric(&http, &base, "hs_room_create_room_id_taken_total").await;
        println!("after burst {bursts}: {taken} ids taken");
        if taken >= 1.0 || bursts == 5 {
            assert!(
                taken >= 1.0,
                "five bursts of twenty identical creates never had two in one millisecond"
            );
            break;
        }
    }

    let joined: Value = http
        .get(format!("{base}/_matrix/client/v3/joined_rooms"))
        .bearer_auth(&alice)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let joined: BTreeSet<String> = joined["joined_rooms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        joined, created,
        "/joined_rooms lists every room alice created"
    );

    // Every attempt is counted in the histogram, the taken ones too.
    let rooms = metric(&http, &base, "hs_room_create_room_id_attempts_count").await;
    let attempts = metric(&http, &base, "hs_room_create_room_id_attempts_sum").await;
    let taken = metric(&http, &base, "hs_room_create_room_id_taken_total").await;
    assert_eq!(rooms as usize, created.len());
    assert!(
        attempts >= rooms + taken,
        "{attempts} attempts, {rooms} rooms, {taken} taken"
    );
    let logged = hs.logged("was already a room's");
    assert_eq!(
        logged.len() as f64,
        taken,
        "an info line per id taken: {logged:?}"
    );
}
