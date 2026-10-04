//! A first boot of the real `hs` binary over an empty data directory: how long it takes, what it
//! says about it, and that a kill right after it leaves a store the next boot opens whole.
//!
//! A cold boot used to take about five seconds on this project's desktop (a debug build, longer
//! under load), nearly all of it creating eighty-odd Fjall keyspaces one fsynced creation at a
//! time; every hs-kv keyspace now lives in one shared Fjall keyspace (decision 0024).
//!
//! What is a regression guard and what is timing:
//!
//! - Guard: the cold boot's `listening` line says `cold=true keyspaces_created=1` and the warm
//!   one `cold=false keyspaces_created=0`. Before the shared keyspace a cold boot created one
//!   Fjall keyspace per table (77 on 2026-09-27), so this fails on that layout whatever the
//!   machine's speed.
//! - Guard: an account registered on the cold boot is still signed in after a `SIGKILL` and a
//!   restart: the store a first boot leaves behind survives a crash.
//! - Timing, with generous bounds: the cold boot's own `boot_ms` is under ten seconds (the old
//!   layout took eight to ten in a debug build here under a moderate load, and up to fifty under
//!   a heavy one, which is why this is not the guard), and the warm boot is no slower than the
//!   cold one beyond a second of noise. With the fix the two are close, so "strictly faster"
//!   would be a coin toss on a loaded machine. The wait for the line itself allows 120 s, as
//!   other real-binary tests do, and the binary is run once (`--help`) before anything is timed:
//!   macOS scans a freshly linked binary on its first exec, which took 23 s for this one.

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A port nothing is listening on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The real `hs` binary, killed (`SIGKILL`) when dropped.
struct HsProcess(std::process::Child);

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the `listening` line said.
#[derive(Debug)]
struct Listening {
    /// From launch to the line, measured by this test.
    seen_after: Duration,
    /// The server's own measurement.
    boot_ms: u64,
    cold: bool,
    keyspaces_created: u64,
}

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let start = line.find(&format!("{name}="))? + name.len() + 1;
    let rest = &line[start..];
    Some(rest.split_whitespace().next().unwrap_or(rest))
}

/// Starts `hs serve` and waits, up to `bound`, for its `listening` line.
fn boot(config: &std::path::Path, bound: Duration) -> (HsProcess, Listening) {
    let launched = Instant::now();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
        .args(["serve", "-c"])
        .arg(config)
        .env_remove("RUST_LOG")
        .env_remove("HS_DATA_DIR")
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("the hs binary should start");
    let stdout = child.stdout.take().unwrap();
    let process = HsProcess(child);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if line.contains(" listening ") || line.ends_with(" listening") {
                let _ = tx.send((Instant::now(), line));
            }
            // Keep draining so the server never blocks on a full pipe.
        }
    });
    let (at, line) = rx
        .recv_timeout(bound)
        .unwrap_or_else(|_| panic!("no `listening` line within {bound:?}"));
    let parse =
        |name| -> &str { field(&line, name).unwrap_or_else(|| panic!("no {name} in {line:?}")) };
    let listening = Listening {
        seen_after: at - launched,
        boot_ms: parse("boot_ms").parse().unwrap(),
        cold: parse("cold").parse().unwrap(),
        keyspaces_created: parse("keyspaces_created").parse().unwrap(),
    };
    (process, listening)
}

async fn register(base: &str, username: &str) -> String {
    let client = reqwest::Client::new();
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"].as_str().expect("a UIA session").to_owned();
    let response = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let done: Value = response.json().await.unwrap();
    done["access_token"].as_str().unwrap().to_owned()
}

async fn whoami(base: &str, token: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}/_matrix/client/v3/account/whoami"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_boot_is_quick_and_a_kill_right_after_it_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             auth:\n  enable_registration: true\n",
            dir.path().join("data"),
            dir.path().join("media"),
        ),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");

    // Outside the measurement: macOS's first-exec scan of a freshly linked binary.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());

    let (cold_process, cold) = boot(&config, Duration::from_secs(120));
    eprintln!("cold boot: {cold:?}, line after {:?}", cold.seen_after);
    assert!(cold.cold, "the first boot over an empty directory is cold");
    assert!(
        cold.boot_ms < 10_000,
        "a first boot took {} ms; it used to be the eighty-odd keyspace creations",
        cold.boot_ms
    );
    assert_eq!(
        cold.keyspaces_created, 1,
        "a first boot creates one Fjall keyspace, the shared one"
    );
    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("hs_boot_duration_seconds{cold=\"true\"}"),
        "{metrics}"
    );
    // The store's flush metrics (RFC 0021) are registered whatever the backend: on the
    // embedded one they stay at zero, but an operator can see they exist.
    for name in [
        "hs_kv_postgres_flush_writes_count",
        "hs_kv_postgres_flush_duration_seconds_count",
        "hs_kv_postgres_flush_statements_total",
    ] {
        assert!(
            metrics.contains(name),
            "{name} missing from /metrics:\n{metrics}"
        );
    }
    let token = register(&base, "alice").await;
    assert_eq!(whoami(&base, &token).await["user_id"], "@alice:example.org");

    // A crash, not a shutdown: nothing gets to flush or persist on the way out.
    drop(cold_process);

    let (_warm_process, warm) = boot(&config, Duration::from_secs(120));
    eprintln!("warm boot: {warm:?}, line after {:?}", warm.seen_after);
    assert!(!warm.cold);
    assert_eq!(warm.keyspaces_created, 0);
    assert!(
        warm.boot_ms <= cold.boot_ms + 1000,
        "the second boot ({} ms) should be no slower than the first ({} ms)",
        warm.boot_ms,
        cold.boot_ms
    );
    assert_eq!(
        whoami(&base, &token).await["user_id"],
        "@alice:example.org",
        "the account and its token survived the kill"
    );
}
