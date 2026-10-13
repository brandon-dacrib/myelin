//! Seeds a stopped `hs serve` data directory with outbound federation queues for many dead
//! destinations: the shape of the 2026-10-13 demo crash loop (`docs/status/06-federation.md`),
//! where every boot resumed about 420 queues of unreachable servers at once.
//!
//! ```text
//! cargo run --release -p hs-federation --example seed_outbound -- <data-dir>/db [scale]
//! ```
//!
//! With the default `scale` of 1 it writes, through the same [`KvOutboundStore`] `hs serve`
//! reads at boot, 13 durable `m.device_list_update` EDUs for each of:
//!
//! - 150 names under `.invalid`, which never resolve (`did not resolve; will retry`);
//! - 150 `nip.io` names that resolve to `10.255.255.x`, a blackholed private address: the
//!   well-known fetch and the connection both hang until their timeouts (`operation timed
//!   out`), as dead public servers do (the server's `federation.ip_range_blocklist` must not
//!   cover `10.0.0.0/8` for this);
//! - 80 IP literals `127.0.0.1:<closed port>`, refused at once;
//! - 40 IP literals in `192.0.2.0/24`, which the server's blocklist should forbid (`dropped:
//!   this server's own policy forbids the destination`);
//!
//! plus a PDU for every fifth destination. `scale` multiplies every count.
//!
//! The server must not be running: Fjall holds a lock on the directory.

use std::sync::Arc;

use hs_federation::outbound_store::{KvOutboundStore, OutboundStore};
use hs_kv::fjall_backend::FjallBackend;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let db = args
        .next()
        .ok_or("usage: seed_outbound <data-dir>/db [scale]")?;
    let scale: usize = args.next().map_or(Ok(1), |s| s.parse())?;
    let backend = FjallBackend::open(&db)?;
    let store: Arc<dyn OutboundStore> = Arc::new(KvOutboundStore::open(backend.clone())?);

    let mut destinations: Vec<String> = Vec::new();
    for i in 0..150 * scale {
        destinations.push(format!("dead-{i}.boot-storm.invalid"));
    }
    for i in 0..150 * scale {
        // 10.255.255.0/24 and 10.255.254.0/24: nothing answers there.
        let (third, fourth) = (255 - (i / 250) % 2, 1 + i % 250);
        destinations.push(format!("10-255-{third}-{fourth}.nip.io"));
    }
    for i in 0..80 * scale {
        destinations.push(format!("127.0.0.1:{}", 40_000 + i));
    }
    for i in 0..40 * scale {
        destinations.push(format!("192.0.2.{}:8448", 1 + i % 250));
    }
    destinations.sort();
    destinations.dedup();

    for n in 0..13 {
        let edu = json!({
            "edu_type": "m.device_list_update",
            "content": {
                "user_id": format!("@user{n}:boot-storm.test"),
                "device_id": format!("DEVICE{n}"),
                "stream_id": n,
                "prev_id": [],
                "deleted": false,
                "keys": { "algorithms": ["m.olm.v1.curve25519-aes-sha2"], "padding": "x".repeat(400) }
            }
        });
        store.enqueue_durable_edu(&destinations, &edu, None, 10_000)?;
    }
    let pdu_destinations: Vec<String> = destinations.iter().step_by(5).cloned().collect();
    for n in 0..3 {
        let pdu = json!({
            "type": "m.room.message",
            "room_id": "!bootstorm:boot-storm.test",
            "sender": "@user0:boot-storm.test",
            "origin_server_ts": 1_791_000_000_000u64 + n,
            "content": { "body": format!("message {n}"), "msgtype": "m.text" },
            "depth": 10 + n,
            "prev_events": [],
            "auth_events": [],
            "hashes": { "sha256": "x" },
            "signatures": {}
        });
        store.enqueue(
            &pdu_destinations,
            Some("!bootstorm:boot-storm.test"),
            &pdu,
            10_000,
        )?;
    }
    backend.persist_all()?;
    println!(
        "seeded {} destinations ({} with PDUs) in {db}",
        destinations.len(),
        pdu_destinations.len()
    );
    Ok(())
}
