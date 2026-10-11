//! A soak of the outbound sender against destinations that do not resolve: the shape of the
//! 2026-10-10 demo incident (`docs/status/06-federation.md`, "2026-10-10: the leak hunt"),
//! where a room naming thousands of dead servers had the sender retrying "did not resolve to
//! any address" seven times a second and the pod's memory crept at a constant rate.
//!
//! Ignored: it takes `SOAK_MINUTES` (default 15) of wall time and asks the system's real DNS
//! resolver about `*.invalid` names. Run it with
//!
//! ```text
//! cargo test -p hs-federation --test sender_soak -- --ignored --nocapture
//! ```
//!
//! It builds the sender exactly as `hs serve` does (a `FjallBackend` under a temporary
//! directory for the destination and outbound stores, the system resolver, the HTTP
//! well-known fetcher behind its cache), queues a PDU for `SOAK_DESTINATIONS` (default 50)
//! unresolvable servers, keeps one more PDU a minute flowing, and samples this process's RSS
//! with `ps` every 30 s. It prints every sample and fails if RSS grew by more than
//! `SOAK_MAX_GROWTH_KIB` (default 8192) between the end of the two-minute warm-up and the last
//! sample. `SOAK_MAX_BACKOFF_SECS` (default 2) caps the sender's backoff so every destination
//! is retried every couple of seconds for the whole run: about 25 attempts a second for 50
//! destinations, a few times the demo's storm.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hs_federation::client::{ClientConfig, FederationClient};
use hs_federation::destination_store::KvDestinationStore;
use hs_federation::discovery::{CachingWellKnownFetcher, HickoryResolver, HttpWellKnownFetcher};
use hs_federation::outbound_store::KvOutboundStore;
use hs_federation::sender::{FederationSender, SenderConfig};
use hs_kv::fjall_backend::FjallBackend;
use hs_kv::memory::MemoryBackend;
use hs_model::signing::SigningKeyPair;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// This process's resident set in KiB, as `ps` reports it (Linux and macOS alike).
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a 15-minute soak against the system resolver; run by hand (see the module docs)"]
async fn rss_stays_flat_while_unresolvable_destinations_are_retried() {
    let minutes = env_u64("SOAK_MINUTES", 15);
    let destinations = env_u64("SOAK_DESTINATIONS", 50) as usize;
    let max_growth_kib = env_u64("SOAK_MAX_GROWTH_KIB", 8192);
    let max_backoff = Duration::from_secs(env_u64("SOAK_MAX_BACKOFF_SECS", 2));

    // `SOAK_BACKEND=memory` keeps both stores in `hs-kv`'s memory backend instead of Fjall: the
    // same store code over a backend with no memtable or journal, which is what tells a leak in
    // the sender from Fjall's own write buffering.
    let dir = tempfile::tempdir().unwrap();
    let use_memory = std::env::var("SOAK_BACKEND").is_ok_and(|b| b == "memory");
    let fjall = (!use_memory).then(|| FjallBackend::open(dir.path().join("db")).unwrap());
    let destinations_store: Arc<dyn hs_federation::destination_store::DestinationStore> =
        match &fjall {
            Some(backend) => Arc::new(KvDestinationStore::open(backend.clone()).unwrap()),
            None => Arc::new(KvDestinationStore::open(MemoryBackend::new()).unwrap()),
        };
    let outbound_store: Arc<dyn hs_federation::outbound_store::OutboundStore> = match &fjall {
        Some(backend) => Arc::new(KvOutboundStore::open(backend.clone()).unwrap()),
        None => Arc::new(KvOutboundStore::open(MemoryBackend::new()).unwrap()),
    };
    println!("backend: {}", if use_memory { "memory" } else { "fjall" });
    let resolver = Arc::new(HickoryResolver::from_system_conf().expect("system DNS"));
    let client = Arc::new(FederationClient::new(
        "soak.example",
        SigningKeyPair::generate("a_1"),
        ClientConfig {
            max_retry_backoff: max_backoff,
            ..ClientConfig::default()
        },
        destinations_store,
        Arc::new(CachingWellKnownFetcher::new(HttpWellKnownFetcher::new())),
        resolver.clone(),
        resolver,
    ));
    let sender = Arc::new(FederationSender::with_store(
        client,
        "soak.example",
        SenderConfig {
            initial_backoff: Duration::from_secs(1),
            max_backoff,
            reset_poll_interval: Duration::from_secs(1),
            ..SenderConfig::default()
        },
        outbound_store,
    ));
    let names: Vec<String> = (0..destinations)
        .map(|i| format!("dead-{i}.soak.invalid"))
        .collect();
    let pdu = |i: u64| {
        serde_json::json!({
            "type": "m.room.message",
            "room_id": "!soak:soak.example",
            "sender": "@soak:soak.example",
            "content": {"body": format!("soak {i}"), "msgtype": "m.text"},
            "i": i,
        })
    };
    sender.enqueue_pdu(names.clone(), pdu(0));

    let started = Instant::now();
    let deadline = started + Duration::from_secs(minutes * 60);
    let mut samples: Vec<(u64, u64)> = Vec::new();
    let mut next_pdu = Instant::now() + Duration::from_secs(60);
    let mut i = 1;
    println!(
        "elapsed_s rss_kib pending_pdus destinations_failing attempts_unresolvable attempts_deferred"
    );
    loop {
        let failing = sender
            .destination_states()
            .unwrap()
            .into_iter()
            .filter(|(_, state)| state.failures > 0)
            .count();
        let elapsed = started.elapsed().as_secs();
        let rss = rss_kib();
        println!(
            "{elapsed} {rss} {} {failing} {} {}",
            sender.pending_pdus(),
            hs_federation::metrics::transactions("unresolvable"),
            hs_federation::metrics::transactions("deferred"),
        );
        samples.push((elapsed, rss));
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
        if Instant::now() >= next_pdu {
            sender.enqueue_pdu(names.clone(), pdu(i));
            i += 1;
            next_pdu = Instant::now() + Duration::from_secs(60);
        }
    }
    sender.shutdown();

    // Two minutes of warm-up (connection pools, resolver caches, the allocator's arenas) and
    // then the run must be flat.
    let warm = samples
        .iter()
        .find(|(t, _)| *t >= 120)
        .unwrap_or(&samples[0])
        .1;
    let last = samples.last().unwrap().1;
    let span_s = samples.last().unwrap().0.saturating_sub(120).max(1);
    let per_hour_kib = (last as i64 - warm as i64) * 3600 / span_s as i64;
    println!(
        "rss after warm-up {warm} KiB, at the end {last} KiB: {per_hour_kib} KiB/h over {span_s}s"
    );
    assert!(
        last.saturating_sub(warm) <= max_growth_kib,
        "RSS grew {} KiB after warm-up (limit {max_growth_kib})",
        last.saturating_sub(warm)
    );
}
