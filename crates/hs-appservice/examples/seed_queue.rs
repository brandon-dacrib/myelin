//! Fills an appservice's transaction queue in a stopped server's embedded (Fjall) database, the
//! way a bridge that has been off for a day leaves it: `total` transactions, the oldest `dead` of
//! them dead-lettered. For reproducing what a long queue does to the server when the bridge
//! comes back (the demo, 2026-10-13: ~34,700 queued for the iMessage bridge, 3,441 of them
//! dead-lettered at the front).
//!
//! ```text
//! cargo run --release -p hs-appservice --example seed_queue -- \
//!     <data-dir>/db <server-name> <appservice-id> <url> <total> <dead> [events-per-entry]
//! ```
//!
//! Registers the appservice (`url`, tokens `as_<id>` and `hs_<id>`, users `@<id>_.*`) if it is
//! not registered yet. Run it while `hs serve` is stopped: Fjall allows one process.

use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

use hs_appservice::registration::Registration;
use hs_appservice::registry::Registry;
use hs_appservice::store::{AppserviceStore, QueueStatus, QueuedTransaction};
use hs_kv::fjall_backend::FjallBackend;
use serde_json::json;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [db, server_name, id, url, total, dead, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: seed_queue <db> <server-name> <id> <url> <total> <dead> [events]".into(),
        );
    };
    let total: u64 = total.parse()?;
    let dead: u64 = dead.parse()?;
    let events_per_entry: usize = rest.first().map(|s| s.parse()).transpose()?.unwrap_or(3);
    let server_name = ruma::OwnedServerName::try_from(server_name.as_str())?;

    let backend = FjallBackend::open(db)?;
    let registry = Registry::open(backend.clone(), &server_name)?;
    if registry.get(id)?.is_none() {
        let yaml = format!(
            "id: {id}\nurl: '{url}'\nas_token: as_{id}\nhs_token: hs_{id}\n\
             sender_localpart: {id}bot\nrate_limited: false\n\
             namespaces:\n  users:\n  - exclusive: true\n    regex: '@{id}_.*'\n"
        );
        registry.add(&Registration::parse_yaml(&yaml)?)?;
    }

    let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    let store = AppserviceStore::open(backend)?;
    for n in 0..total {
        let events: Vec<_> = (0..events_per_entry)
            .map(|e| {
                json!({
                    "type": "m.room.message",
                    "room_id": format!("!room{}:{server_name}", n % 50),
                    "sender": format!("@someone{}:{server_name}", n % 7),
                    "event_id": format!("$event{n}x{e}"),
                    "origin_server_ts": now - (total - n) * 1_000,
                    "content": {
                        "msgtype": "m.text",
                        "body": format!("message {n}.{e}: {}", "lorem ipsum dolor sit amet ".repeat(12)),
                    },
                    "unsigned": {"age": 1_000},
                })
            })
            .collect();
        let body = json!({ "events": events });
        let enqueued_at_ms = now - (total - n) * 1_000;
        let seq = store.enqueue(id, body.clone(), enqueued_at_ms)?;
        if n < dead {
            store.put_queue_entry(
                id,
                &QueuedTransaction {
                    seq,
                    body,
                    enqueued_at_ms,
                    attempts: 10,
                    next_attempt_at_ms: enqueued_at_ms,
                    status: QueueStatus::DeadLettered,
                    last_error: Some("connection refused".to_owned()),
                },
            )?;
        }
        if (n + 1) % 5_000 == 0 {
            eprintln!("queued {}", n + 1);
        }
    }
    let summary = store.queue_summary(id)?;
    eprintln!(
        "{id}: {} waiting, {} dead-lettered",
        summary.pending, summary.dead_lettered
    );
    Ok(())
}
