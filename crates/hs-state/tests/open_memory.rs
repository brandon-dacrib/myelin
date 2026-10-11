//! The before/after figure for `docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`:
//! a room of 50,000 events on a Fjall store, opened the way every room was opened until
//! 2026-10-10 (the store fed the whole history again: the `replay` leg) and the way it is now
//! (the store opened and current state read: the `durable` leg), with this process's resident
//! size and the elapsed time read each time. One process per leg, because a process keeps what
//! a dropped store freed and the next leg would reuse it.
//!
//! ```sh
//! export HS_STATE_MEASURE_DIR=/tmp/state-measure   # empty: the first run builds the room
//! cargo test -p hs-state --release --test open_memory measure_open_of_a_large_room -- --ignored --nocapture
//! HS_STATE_MEASURE_LEG=replay  cargo test -p hs-state --release --test open_memory measure_open_of_a_large_room -- --ignored --nocapture
//! HS_STATE_MEASURE_LEG=durable cargo test -p hs-state --release --test open_memory measure_open_of_a_large_room -- --ignored --nocapture
//! ```
//!
//! Without `HS_STATE_MEASURE_DIR` it builds and runs both legs in a temporary directory, as a
//! smoke of the measurement itself (the second leg's figures are then understated).

use hs_kv::fjall_backend::FjallBackend;
use hs_model::canonical::{CanonicalJsonObject, to_canonical_object};
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use hs_state::kv_store::ProductionStateStore;
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UserId};
use serde_json::json;

const EVENTS: u64 = 50_000;
const MEMBERS: u64 = 200;

fn rss() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(0, |kib| kib * 1024)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn room() -> OwnedRoomId {
    RoomId::parse("!measure:hs1").unwrap().to_owned()
}

fn user(n: u64) -> OwnedUserId {
    UserId::parse(format!("@user{n}:hs1")).unwrap().to_owned()
}

fn event_id(n: u64) -> OwnedEventId {
    EventId::parse(format!("$e{n}:hs1")).unwrap().to_owned()
}

/// Event `n` of the scripted room: create, creator join, power levels, join rules, then a
/// history in which every 25th event is a membership change of one of `MEMBERS` users (so the
/// state churns and the chain-cover index grows) and the rest are messages.
struct Line {
    event_type: &'static str,
    state_key: Option<String>,
    sender: OwnedUserId,
    content: CanonicalJsonObject,
    auth: Vec<EventSn>,
}

fn line(n: u64) -> Line {
    let creator = user(0);
    let obj = |v: serde_json::Value| to_canonical_object(&v, true).unwrap();
    let base_auth = vec![EventSn::new(1), EventSn::new(2), EventSn::new(3)];
    match n {
        1 => Line {
            event_type: "m.room.create",
            state_key: Some(String::new()),
            sender: creator,
            content: obj(json!({"room_version": "11"})),
            auth: vec![],
        },
        2 => Line {
            event_type: "m.room.member",
            state_key: Some(creator.to_string()),
            sender: creator,
            content: obj(json!({"membership": "join"})),
            auth: vec![EventSn::new(1)],
        },
        3 => Line {
            event_type: "m.room.power_levels",
            state_key: Some(String::new()),
            content: obj(json!({"users": {creator.as_str(): 100}, "state_default": 50})),
            sender: creator,
            auth: vec![EventSn::new(1), EventSn::new(2)],
        },
        4 => Line {
            event_type: "m.room.join_rules",
            state_key: Some(String::new()),
            sender: creator,
            content: obj(json!({"join_rule": "public"})),
            auth: base_auth,
        },
        n if n.is_multiple_of(25) => {
            let who = user(1 + (n / 25) % MEMBERS);
            let joining = (n / 25 / MEMBERS).is_multiple_of(2);
            Line {
                event_type: "m.room.member",
                state_key: Some(who.to_string()),
                sender: who,
                content: obj(json!({
                    "membership": if joining { "join" } else { "leave" },
                    "displayname": format!("User {}", n),
                })),
                auth: vec![EventSn::new(1), EventSn::new(3), EventSn::new(4)],
            }
        }
        n => Line {
            event_type: "m.room.message",
            state_key: None,
            sender: user(1 + n % MEMBERS),
            // The room actor hands the store empty content for a message; so does this.
            content: CanonicalJsonObject::new(),
            auth: vec![],
        },
    }
}

fn feed(store: &ProductionStateStore<FjallBackend>, from: u64, to: u64) {
    for n in from..=to {
        let l = line(n);
        let prev: Vec<EventSn> = if n == 1 {
            Vec::new()
        } else {
            vec![EventSn::new(n - 1)]
        };
        store
            .add_event(
                EventSn::new(n),
                event_id(n),
                room(),
                l.event_type,
                l.state_key.as_deref(),
                l.sender,
                l.content,
                n as i64,
                1_790_000_000_000 + n as i64,
                &l.auth,
                &prev,
                n == 2,
            )
            .expect("ingest");
    }
}

#[test]
#[ignore = "builds a room of 50,000 events on disk and measures this process; run alone, release, with --nocapture"]
fn measure_open_of_a_large_room() {
    let temp;
    let dir: std::path::PathBuf = match std::env::var_os("HS_STATE_MEASURE_DIR") {
        Some(dir) => dir.into(),
        None => {
            temp = tempfile::tempdir().expect("a temporary directory");
            temp.path().to_owned()
        }
    };
    let kv = dir.join("kv");
    let build = !kv.exists();
    let leg = std::env::var("HS_STATE_MEASURE_LEG").ok();

    if build {
        let backend = FjallBackend::open(&kv).expect("open fjall");
        let store = ProductionStateStore::open(RoomVersionId::V11, backend).expect("open");
        let before = rss();
        let started = std::time::Instant::now();
        feed(&store, 1, EVENTS);
        store.mark_migrated(&room(), 0).expect("mark");
        println!(
            "built {EVENTS} events in {:.1}s; rss {:.0} MiB -> {:.0} MiB",
            started.elapsed().as_secs_f64(),
            mib(before),
            mib(rss())
        );
        drop(store);
        if std::env::var_os("HS_STATE_MEASURE_DIR").is_some() {
            return;
        }
    }

    let run_leg = |name: &str| {
        let backend = FjallBackend::open(&kv).expect("open fjall");
        let before = rss();
        let started = std::time::Instant::now();
        let store = ProductionStateStore::open(RoomVersionId::V11, backend).expect("open");
        match name {
            "replay" => feed(&store, 1, EVENTS),
            "durable" => {}
            other => panic!("unknown leg {other}"),
        }
        let root = store
            .state_at(EventSn::new(EVENTS))
            .expect("state at the last event");
        assert_eq!(
            store
                .current_state(&RoomVersionId::V11, &[EventSn::new(EVENTS)])
                .expect("current state"),
            root
        );
        let full = store.diff(store.empty_root(), root).expect("full state");
        let pl = store
            .intern_state_key("m.room.power_levels", "")
            .expect("intern");
        assert_eq!(store.get(root, pl).expect("get"), Some(EventSn::new(3)));
        assert!(
            store
                .has_event(EventSn::new(EVENTS / 2))
                .expect("has_event")
        );
        let elapsed = started.elapsed();
        let after = rss();
        let stats = store.stats();
        println!(
            "leg {name}: open+current state in {:.2}s; rss {:.0} MiB -> {:.0} MiB (+{:.0} MiB); \
             state entries {}; ingested {} replayed {} state rows read {} records read {}",
            elapsed.as_secs_f64(),
            mib(before),
            mib(after),
            mib(after.saturating_sub(before)),
            full.added.len(),
            stats.events_ingested,
            stats.events_replayed,
            stats.state_at_reads,
            stats.records_read,
        );
        if name == "durable" {
            assert_eq!(stats.events_ingested, 0);
            assert_eq!(stats.records_read, 0);
        } else {
            assert_eq!(stats.events_replayed, EVENTS);
        }
    };

    match leg.as_deref() {
        Some(leg) => run_leg(leg),
        None => {
            run_leg("replay");
            run_leg("durable");
        }
    }
}
