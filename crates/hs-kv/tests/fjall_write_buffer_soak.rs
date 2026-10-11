//! Resident memory of a process that rewrites one key in each of a hundred keyspaces at a steady
//! rate over the Fjall backend: flat under the write-buffer cap (RFC 0024), climbing without it.
//! Ignored: it runs for minutes and reads this process's RSS from `ps`, so it is for a hand run,
//! and its numbers go in `docs/status/01-storage-engine.md`.
//!
//! The demo's idle creep was 25 small rewrites a second, 8 MiB an hour; to show the same shape
//! in minutes the value is larger (`SOAK_VALUE_BYTES`, 8 KiB by default: 200 KiB/s, 60 MiB in
//! five minutes, under Fjall's default 64 MiB memtable so the uncapped run never flushes).
//!
//! ```text
//! SOAK_MINUTES=5 cargo test -p hs-kv --test fjall_write_buffer_soak -- --ignored --nocapture
//! SOAK_UNCAPPED=1 SOAK_MINUTES=5 cargo test -p hs-kv --test fjall_write_buffer_soak -- --ignored --nocapture
//! ```
//!
//! `SOAK_UNCAPPED=1` opens the backend as before RFC 0024 (no cap, Fjall's 64 MiB memtable);
//! `SOAK_WRITES_PER_SEC` (25) and `SOAK_KEYSPACES` (100) set the writer. It prints one sample a
//! ten seconds and a summary line (the slope over the last 40% of the run, past the first two
//! memtable flushes), and with the cap asserts that RSS after the warm-up grew by less than the
//! cap plus a margin.

use std::time::{Duration, Instant};

use hs_kv::fjall_backend::{FjallBackend, FjallOptions};
use hs_kv::{KvBackend, KvWrite};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("ps prints a number")
}

#[test]
#[ignore = "runs for minutes and reads RSS from ps; run by hand, see the module docs"]
fn rss_stays_flat_under_the_write_buffer_cap() {
    let minutes: u64 = env_or("SOAK_MINUTES", 5);
    let writes_per_sec: u64 = env_or("SOAK_WRITES_PER_SEC", 25);
    let keyspaces: usize = env_or("SOAK_KEYSPACES", 100);
    let value_bytes: usize = env_or("SOAK_VALUE_BYTES", 8 * 1024);
    let uncapped = std::env::var("SOAK_UNCAPPED").is_ok_and(|v| v == "1");
    let options = if uncapped {
        FjallOptions {
            write_buffer_cap: None,
            max_memtable_size: 64 * 1024 * 1024,
        }
    } else {
        FjallOptions::default()
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FjallBackend::open_with_options(dir.path(), options).expect("open");
    let handles: Vec<_> = (0..keyspaces)
        .map(|i| {
            backend
                .keyspace(&format!("hs_soak.table_{i:03}"))
                .expect("keyspace")
        })
        .collect();

    let interval = Duration::from_secs_f64(1.0 / writes_per_sec as f64);
    let end = Instant::now() + Duration::from_secs(minutes * 60);
    // The slope is measured over the last 40% of the run: with the defaults the memtable flushes
    // first at 80 s and the allocator keeps the first freed memtable, so the plateau starts at
    // about three minutes.
    let warm_up = Duration::from_secs(minutes * 60 * 3 / 5);
    let start = Instant::now();
    let mut next_write = start;
    let mut next_sample = start;
    let mut writes: u64 = 0;
    let mut samples: Vec<(u64, u64)> = Vec::new();
    println!(
        "options {options:?}; {keyspaces} keyspaces, {writes_per_sec} writes/s of {value_bytes} B"
    );
    println!("elapsed_s rss_kib write_buffer_bytes rotations sealed writes");
    while Instant::now() < end {
        if Instant::now() >= next_sample {
            let elapsed = start.elapsed().as_secs();
            let rss = rss_kib();
            let stats = backend.write_buffer_stats();
            println!(
                "{elapsed} {rss} {} {} {} {writes}",
                stats.bytes, stats.rotations, stats.sealed_memtables
            );
            samples.push((elapsed, rss));
            next_sample += Duration::from_secs(10);
        }
        let mut value = format!("write {writes} ").into_bytes();
        value.resize(value_bytes, b'v');
        let ks = &handles[(writes as usize) % keyspaces];
        let mut txn = backend.begin().expect("begin");
        txn.put(ks, b"state", &value).expect("put");
        backend.commit(txn).expect("commit").expect("no conflict");
        writes += 1;
        next_write += interval;
        if let Some(wait) = next_write.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }

    let (warm_at, warm) = samples
        .iter()
        .copied()
        .find(|(t, _)| Duration::from_secs(*t) >= warm_up)
        .expect("a sample after the warm-up");
    let (last_at, last) = *samples.last().expect("a sample");
    let span_s = last_at.saturating_sub(warm_at).max(1);
    let per_hour_kib = (last as i64 - warm as i64) * 3600 / span_s as i64;
    let stats = backend.write_buffer_stats();
    println!(
        "summary: {} writes; rss after warm-up {warm} KiB, at the end {last} KiB: {per_hour_kib} KiB/h over {span_s}s; write buffer {} B, {} rotations",
        writes, stats.bytes, stats.rotations
    );
    if let Some(cap) = options.write_buffer_cap {
        let allowed_kib = cap / 1024 + 16 * 1024;
        let grown_kib = last.saturating_sub(warm);
        assert!(
            grown_kib < allowed_kib,
            "RSS grew {grown_kib} KiB after the warm-up with a cap of {cap} B; allowed {allowed_kib} KiB"
        );
    }
}
