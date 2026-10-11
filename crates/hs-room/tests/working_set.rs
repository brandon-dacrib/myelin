//! The room actor's bounded working set (`hs_room::actor::event_cache`): a room with far more
//! events than its cache holds still answers every read correctly -- pages, one event by id,
//! the state at an old event, relations, a redaction -- once the window is exceeded and again
//! after the actor is dropped and loaded from the store, and the actor holds no more than the
//! window.
//!
//! The ignored `measure_*` test is the before/after figure for status 04: a room of 50,000
//! events on a Fjall store, loaded with a window as large as the room (how every room was held
//! until 2026-10-10) and with the default window, with this process's resident size read each
//! time (`hs_room::metrics::process_memory`). Run it alone, release, with `--nocapture`.

use std::collections::HashSet;

use hs_kv::KvBackend;
use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_room::actor::event_cache::CacheCapacity;
use hs_room::actor::{CreateRoomRequest, RoomActor};
use hs_room::identity::HomeserverIdentity;
use hs_room::persist::Tables;
use hs_room::timeline::Direction;
use ruma::{OwnedEventId, OwnedUserId, RoomVersionId, user_id};
use serde_json::json;

fn alice() -> OwnedUserId {
    user_id!("@alice:hs1").to_owned()
}

/// `content.<key>` of `event` as a string.
fn content_str<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|content| content.get(key))
        .and_then(CanonicalJsonValue::as_str)
}

const WINDOW: usize = 50;
/// Well over the window, with a redaction, relations and a state change in the old part.
const MESSAGES: usize = 600;

/// What the room was built with, to check reads against.
struct Built {
    room_id: ruma::OwnedRoomId,
    /// Every message, oldest first.
    messages: Vec<OwnedEventId>,
    /// The message the reactions target (old enough to be out of the window).
    reacted: OwnedEventId,
    reactions: Vec<OwnedEventId>,
    /// The message redacted (also old).
    redacted: OwnedEventId,
    /// The message sent while the topic was "first topic", before the change.
    under_first_topic: OwnedEventId,
}

fn send<B: KvBackend>(actor: &mut RoomActor<B>, ts: &mut i64, body: String) -> Event {
    *ts += 1;
    actor
        .send_event(
            alice(),
            "m.room.message".into(),
            None,
            json!({"msgtype": "m.text", "body": body}),
            None,
            *ts,
        )
        .expect("send a message")
}

fn build<B: KvBackend>(backend: &B, tables: &Tables<B>) -> (RoomActor<B>, Built) {
    let mut actor = RoomActor::create_room(
        backend.clone(),
        tables.clone(),
        HomeserverIdentity::for_tests("hs1"),
        alice(),
        CreateRoomRequest {
            preset: Some("private_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            topic: Some("first topic".to_owned()),
            ..Default::default()
        },
        1,
    )
    .expect("create the room");
    actor.set_cache_capacity(CacheCapacity::new(WINDOW));
    let mut ts = 10;
    let mut messages = Vec::with_capacity(MESSAGES);
    for n in 0..MESSAGES / 2 {
        messages.push(
            send(&mut actor, &mut ts, format!("message {n}"))
                .event_id()
                .to_owned(),
        );
    }
    let under_first_topic = messages.last().expect("messages").clone();
    ts += 1;
    actor
        .send_event(
            alice(),
            "m.room.topic".into(),
            Some(String::new()),
            json!({"topic": "second topic"}),
            None,
            ts,
        )
        .expect("change the topic");
    let reacted = messages[3].clone();
    let mut reactions = Vec::new();
    for key in ["👍", "🎉", "❤️"] {
        ts += 1;
        let reaction = actor
            .send_event(
                alice(),
                "m.reaction".into(),
                None,
                json!({"m.relates_to": {"rel_type": "m.annotation", "event_id": reacted, "key": key}}),
                None,
                ts,
            )
            .expect("react");
        reactions.push(reaction.event_id().to_owned());
    }
    let redacted = messages[7].clone();
    ts += 1;
    // As the `redact` command does: send the redaction, then apply it to its target.
    let redaction = actor
        .send_event(
            alice(),
            "m.room.redaction".into(),
            None,
            json!({"redacts": redacted, "reason": "oops"}),
            Some(redacted.clone()),
            ts,
        )
        .expect("redact");
    actor
        .apply_redaction_by(&redacted, redaction.event_id())
        .expect("apply the redaction");
    for n in MESSAGES / 2..MESSAGES {
        messages.push(
            send(&mut actor, &mut ts, format!("message {n}"))
                .event_id()
                .to_owned(),
        );
    }
    let built = Built {
        room_id: actor.room_id().to_owned(),
        messages,
        reacted,
        reactions,
        redacted,
        under_first_topic,
    };
    (actor, built)
}

fn check_reads<B: KvBackend>(actor: &RoomActor<B>, built: &Built) {
    // The actor holds a bounded set, never the room.
    assert!(
        actor.cached_events() <= WINDOW.max(hs_room::actor::event_cache::DEFAULT_CAPACITY),
        "{} events cached",
        actor.cached_events()
    );

    // Every message, paged backwards from the head, in order, nothing missing or doubled.
    let mut seen: Vec<OwnedEventId> = Vec::new();
    let mut from = None;
    loop {
        let (page, next) = actor.paginate(from, Direction::Backward, 37);
        seen.extend(
            page.iter()
                .filter(|e| e.header().event_type == "m.room.message")
                .map(|e| e.event_id().to_owned()),
        );
        match next {
            Some(token) => from = Some(token),
            None => break,
        }
    }
    seen.reverse();
    assert_eq!(seen.len(), built.messages.len(), "every message is paged");
    assert_eq!(seen, built.messages, "in timeline order");
    assert_eq!(
        seen.iter().collect::<HashSet<_>>().len(),
        built.messages.len()
    );

    // One old event by id, with its context around it (`/context`).
    let old = &built.messages[11];
    let event = actor.event_by_id(old).expect("an old event is read back");
    assert_eq!(content_str(event, "body"), Some("message 11"));
    let pos = actor.timeline_position(old).expect("it has a position");
    let (older, newer) = actor.events_around(pos, 3, 3);
    assert_eq!(older.len(), 3);
    assert_eq!(newer.len(), 3);
    assert_eq!(older[0].1.event_id(), built.messages[10]);
    assert_eq!(newer[0].1.event_id(), built.messages[12]);

    // The state at an old event: the topic as it was then, not as it is now.
    let then = actor
        .state_at_event(&built.under_first_topic)
        .expect("state at an old event")
        .expect("the event is held");
    let topic_then = then
        .state
        .iter()
        .find(|e| e.header().event_type == "m.room.topic")
        .expect("a topic then");
    assert_eq!(content_str(topic_then, "topic"), Some("first topic"));
    let topic_now = actor
        .state_event("m.room.topic", "")
        .expect("read the topic")
        .expect("a topic now");
    assert_eq!(content_str(topic_now, "topic"), Some("second topic"));

    // Relations of an old target.
    let children: Vec<OwnedEventId> = actor
        .relations_of(&built.reacted, Some("m.annotation"))
        .iter()
        .map(|e| e.event_id().to_owned())
        .collect();
    assert_eq!(children, built.reactions);

    // The redaction took: the old event comes back redacted, with its redaction named.
    let redacted = actor
        .event_by_id(&built.redacted)
        .expect("the redacted event is held");
    assert!(redacted.header().flags.is_redacted());
    // (The stored JSON stays whole; `Event::redacted_json` is what a render shows, driven by
    // the flag.)
    assert!(
        hs_room::actor::redactions::redacted_by(redacted).is_some(),
        "unsigned.redacted_by names the redaction"
    );

    // Members, through the state.
    let members = actor.members().expect("members");
    assert_eq!(members.len(), 1);
    assert_eq!(
        members[0].header().state_key.as_deref(),
        Some(alice().as_str())
    );
}

#[test]
fn a_room_larger_than_its_window_answers_every_read_and_holds_only_the_window() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let misses_before = hs_room::metrics::event_cache_misses();
    let (mut actor, built) = build(&backend, &tables);
    assert!(
        actor.cached_events() <= WINDOW,
        "{} cached after building",
        actor.cached_events()
    );
    actor.begin_operation();
    check_reads(&actor, &built);
    assert!(
        actor.cached_events() <= WINDOW,
        "{} cached after reading",
        actor.cached_events()
    );
    assert!(
        hs_room::metrics::event_cache_misses() > misses_before,
        "reads beyond the window went to the store"
    );
    assert!(hs_room::metrics::event_cache_evictions() > 0);
    // The pins of this operation are what the reads touched: bounded by the operation.
    assert!(actor.pinned_events() >= built.messages.len());
    actor.begin_operation();
    assert_eq!(actor.pinned_events(), 0);
}

#[test]
fn the_same_reads_hold_after_the_actor_is_dropped_and_loaded_again() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let (actor, built) = build(&backend, &tables);
    drop(actor);

    let mut loaded = RoomActor::load(
        backend.clone(),
        tables.clone(),
        HomeserverIdentity::for_tests("hs1"),
        &built.room_id,
    )
    .expect("load")
    .expect("the room exists");
    // Loaded with the default window, then shrunk to the test's.
    assert!(loaded.cached_events() <= hs_room::actor::event_cache::DEFAULT_CAPACITY);
    loaded.set_cache_capacity(CacheCapacity::new(WINDOW));
    assert!(loaded.cached_events() <= WINDOW);
    check_reads(&loaded, &built);
    assert!(loaded.cached_events() <= WINDOW);

    // And it keeps working as a writer: a send cites the extremity, which is in the window.
    let sent = loaded
        .send_event(
            alice(),
            "m.room.message".into(),
            None,
            json!({"msgtype": "m.text", "body": "after the reload"}),
            None,
            100_000,
        )
        .expect("send after reload");
    assert_eq!(
        loaded.paginate(None, Direction::Backward, 1).0[0].event_id(),
        sent.event_id()
    );
}

#[test]
fn a_zero_window_reads_everything_from_the_store_and_still_answers() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let (mut actor, built) = build(&backend, &tables);
    actor.set_cache_capacity(CacheCapacity::new(0));
    assert_eq!(actor.cached_events(), 0);
    actor.begin_operation();
    check_reads(&actor, &built);
    assert_eq!(actor.cached_events(), 0);
}

/// The before/after figure, one process per leg, because a process keeps what a dropped actor
/// freed and the next load reuses it, so two loads in one process understate the second:
///
/// ```sh
/// export HS_ROOM_MEASURE_DIR=/tmp/room-measure   # empty: the first run builds the room
/// cargo test -p hs-room --release --test working_set measure_resident_memory_of_a_large_room -- --ignored --nocapture
/// HS_ROOM_MEASURE_WINDOW=1000  cargo test -p hs-room --release --test working_set measure_resident_memory_of_a_large_room -- --ignored --nocapture
/// HS_ROOM_MEASURE_WINDOW=51000 cargo test -p hs-room --release --test working_set measure_resident_memory_of_a_large_room -- --ignored --nocapture
/// ```
///
/// Without `HS_ROOM_MEASURE_DIR` it builds and loads in a temporary directory, as a smoke of the
/// measurement itself.
#[test]
#[ignore = "builds a room of 50,000 events on disk and measures this process; run alone, release, with --nocapture"]
fn measure_resident_memory_of_a_large_room() {
    use hs_kv::fjall_backend::FjallBackend;
    const EVENTS: usize = 50_000;

    let temp;
    let dir: std::path::PathBuf = match std::env::var_os("HS_ROOM_MEASURE_DIR") {
        Some(dir) => dir.into(),
        None => {
            temp = tempfile::tempdir().expect("a temporary directory");
            temp.path().to_owned()
        }
    };
    let kv = dir.join("kv");
    let build = !kv.exists();
    let backend = FjallBackend::open(&kv).expect("open fjall");
    let tables = Tables::open(&backend).expect("open tables");
    let identity = HomeserverIdentity::for_tests("hs1");
    let rss = || {
        hs_room::metrics::process_memory()
            .map(|m| m.resident_bytes)
            .unwrap_or(0)
    };
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    let room_id_file = dir.join("room_id");

    if build {
        let started = std::time::Instant::now();
        let mut actor = RoomActor::create_room(
            backend.clone(),
            tables.clone(),
            identity.clone(),
            alice(),
            CreateRoomRequest {
                preset: Some("private_chat".to_owned()),
                room_version: Some(RoomVersionId::V11),
                ..Default::default()
            },
            1,
        )
        .expect("create the room");
        for n in 0..EVENTS {
            actor
                .send_event(
                    alice(),
                    "m.room.message".into(),
                    None,
                    json!({"msgtype": "m.text", "body": format!("message {n} of a long history, about the length of a typical line of chat")}),
                    None,
                    10 + n as i64,
                )
                .expect("send");
            if n % 1000 == 0 {
                actor.begin_operation();
            }
        }
        std::fs::write(&room_id_file, actor.room_id().as_str()).expect("write the room id");
        println!(
            "built {EVENTS} events in {:.1}s; rss now {:.0} MiB",
            started.elapsed().as_secs_f64(),
            mib(rss())
        );
        if std::env::var_os("HS_ROOM_MEASURE_DIR").is_some() {
            return;
        }
    }
    let room_id = ruma::OwnedRoomId::try_from(
        std::fs::read_to_string(&room_id_file).expect("the room id of the built room"),
    )
    .expect("a room id");

    let load = |capacity: usize| {
        let before = rss();
        let started = std::time::Instant::now();
        let mut actor =
            RoomActor::load(backend.clone(), tables.clone(), identity.clone(), &room_id)
                .expect("load")
                .expect("exists");
        actor.set_cache_capacity(CacheCapacity::new(capacity));
        // As the load would have filled a window this wide.
        actor.warm_cache();
        // Read the newest page and an old page, as a client would.
        actor.begin_operation();
        let (page, _) = actor.paginate(None, Direction::Backward, 100);
        assert_eq!(page.len(), 100);
        let old = actor.events_around(10, 50, 50);
        assert_eq!(old.1.len(), 50);
        actor.begin_operation();
        let after = rss();
        println!(
            "window {capacity:>6}: load {:.2}s, cached events {:>6}, rss before {:.0} MiB, after {:.0} MiB, delta {:.0} MiB",
            started.elapsed().as_secs_f64(),
            actor.cached_events(),
            mib(before),
            mib(after),
            mib(after.saturating_sub(before)),
        );
        actor.cached_events()
    };

    match std::env::var("HS_ROOM_MEASURE_WINDOW") {
        Ok(window) => {
            let window: usize = window.parse().expect("HS_ROOM_MEASURE_WINDOW is a number");
            load(window);
        }
        Err(_) => {
            // The smoke: both legs in one process, the smaller first.
            let cached = load(hs_room::actor::event_cache::DEFAULT_CAPACITY);
            assert!(cached <= hs_room::actor::event_cache::DEFAULT_CAPACITY);
            let cached = load(EVENTS + 1_000);
            assert!(cached >= EVENTS, "{cached} cached: the whole room");
        }
    }
}
