//! A room actor over the durable, shared state store
//! (`docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`, decision 0045).
//!
//! - A room whose state records are complete loads without feeding the store anything, and
//!   answers exactly what a replay of its history answers: the state at every event, the
//!   timeline, the forward extremities.
//! - A room from before the records were durable (its data dir holds the room's rows and none
//!   of the store's records) is replayed into the store once, on its first load, and no later
//!   load replays it.
//! - Replaying over records that are already there, by one writer or two at once, changes
//!   nothing.
//! - A read copy on the same backend catches up with its owner's rows without feeding the
//!   store again, and does not hold the owner's newer events before it has caught up.

use hs_kv::memory::MemoryBackend;
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec};
use hs_room::actor::catch_up::CatchUp;
use hs_room::actor::{CreateRoomRequest, RoomActor};
use hs_room::identity::HomeserverIdentity;
use hs_room::persist::Tables;
use ruma::{OwnedEventId, OwnedUserId, RoomId, RoomVersionId, user_id};
use serde_json::json;

fn alice() -> OwnedUserId {
    user_id!("@alice:hs1").to_owned()
}

fn identity() -> HomeserverIdentity {
    HomeserverIdentity::for_tests("hs1")
}

fn message<B: KvBackend>(actor: &mut RoomActor<B>, ts: &mut i64, body: &str) -> OwnedEventId {
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
        .event_id()
        .to_owned()
}

fn topic<B: KvBackend>(actor: &mut RoomActor<B>, ts: &mut i64, topic: &str) -> OwnedEventId {
    *ts += 1;
    actor
        .send_event(
            alice(),
            "m.room.topic".into(),
            Some(String::new()),
            json!({"topic": topic}),
            None,
            *ts,
        )
        .expect("set the topic")
        .event_id()
        .to_owned()
}

/// A room with state changes, messages, a fork (two events citing the same parent, one of them
/// a state event) merged by the next send, and a redaction.
fn build<B: KvBackend>(backend: &B, tables: &Tables<B>) -> RoomActor<B> {
    let mut actor = RoomActor::create_room(
        backend.clone(),
        tables.clone(),
        identity(),
        alice(),
        CreateRoomRequest {
            preset: Some("private_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            ..Default::default()
        },
        1_000,
    )
    .expect("create the room");
    let mut ts = 2_000;
    topic(&mut actor, &mut ts, "first topic");
    for i in 0..10 {
        message(&mut actor, &mut ts, &format!("message {i}"));
    }
    let doomed = message(&mut actor, &mut ts, "to be redacted");
    let liked = message(&mut actor, &mut ts, "to be reacted to");
    ts += 1;
    actor
        .send_event(
            alice(),
            "m.reaction".into(),
            None,
            json!({"m.relates_to": {"rel_type": "m.annotation", "event_id": liked, "key": "+1"}}),
            None,
            ts,
        )
        .expect("react");

    // A fork: both branches cite the current head.
    let head = actor.forward_extremity_ids();
    assert_eq!(head.len(), 1);
    let head_sn = actor.event_sn_of(&head[0].0).expect("the head is held");
    ts += 1;
    actor
        .send_event_citing(
            alice(),
            "m.room.topic".into(),
            Some(String::new()),
            json!({"topic": "branch topic"}),
            None,
            ts,
            &[head_sn],
        )
        .expect("one branch");
    ts += 1;
    actor
        .send_event_citing(
            alice(),
            "m.room.name".into(),
            Some(String::new()),
            json!({"name": "branch name"}),
            None,
            ts,
            &[head_sn],
        )
        .expect("the other branch");
    assert_eq!(actor.forward_extremity_ids().len(), 2, "a fork");
    message(&mut actor, &mut ts, "after the fork");
    assert_eq!(actor.forward_extremity_ids().len(), 1, "merged");

    ts += 1;
    actor
        .redact_txn(alice(), None, "txn-1", doomed, Some("test".to_owned()), ts)
        .expect("redact");
    topic(&mut actor, &mut ts, "last topic");
    message(&mut actor, &mut ts, "the newest");
    actor
}

/// Everything a load must reproduce: the timeline (position, ID, redacted), the forward
/// extremities, the current state, and the state at every timeline event.
#[derive(Debug, PartialEq, Eq)]
struct Answers {
    timeline: Vec<(i64, String, bool)>,
    extremities: Vec<String>,
    current_state: Vec<String>,
    state_at: Vec<(String, Vec<String>)>,
    /// Each timeline event's relations, by the relating events' IDs.
    relations: Vec<(String, Vec<String>)>,
}

fn answers<B: KvBackend>(actor: &RoomActor<B>) -> Answers {
    let timeline: Vec<(i64, String, bool)> = actor
        .events_after(i64::MIN, usize::MAX)
        .into_iter()
        .map(|(pos, e)| {
            (
                pos,
                e.event_id().to_string(),
                e.header().flags.is_redacted(),
            )
        })
        .collect();
    let mut extremities: Vec<String> = actor
        .forward_extremity_ids()
        .into_iter()
        .map(|(id, _)| id.to_string())
        .collect();
    extremities.sort();
    let mut current_state: Vec<String> = actor
        .full_state()
        .expect("the current state")
        .iter()
        .map(|e| e.event_id().to_string())
        .collect();
    current_state.sort();
    let state_at = timeline
        .iter()
        .map(|(_, id, _)| {
            let event_id = OwnedEventId::try_from(id.as_str()).expect("an event ID");
            let mut state: Vec<String> = actor
                .state_at_event(&event_id)
                .expect("the state at an event")
                .expect("a held event")
                .state
                .iter()
                .map(|e| e.event_id().to_string())
                .collect();
            state.sort();
            (id.clone(), state)
        })
        .collect();
    let relations = timeline
        .iter()
        .filter_map(|(_, id, _)| {
            let event_id = OwnedEventId::try_from(id.as_str()).expect("an event ID");
            let children: Vec<String> = actor
                .relations_of(&event_id, None)
                .iter()
                .map(|e| e.event_id().to_string())
                .collect();
            (!children.is_empty()).then(|| (id.clone(), children))
        })
        .collect();
    Answers {
        relations,
        timeline,
        extremities,
        current_state,
        state_at,
    }
}

fn load<B: KvBackend>(backend: &B, tables: &Tables<B>, room_id: &RoomId) -> RoomActor<B> {
    RoomActor::load(backend.clone(), tables.clone(), identity(), room_id)
        .expect("load the room")
        .expect("the room exists")
}

/// Removes every record of the state store from `backend`, leaving the room's own rows and the
/// store's content-addressed frames: a data dir written before the records were durable.
fn wipe_state_records<B: KvBackend>(backend: &B) {
    for name in hs_state::durable::ALL_KEYSPACES {
        let keyspace = backend.keyspace(name).expect("a keyspace");
        let keys: Vec<Vec<u8>> = backend
            .snapshot()
            .range(&keyspace, RangeSpec::full())
            .map(|item| item.expect("a row").0.to_vec())
            .collect();
        // In batches: a transaction's size is bounded.
        for batch in keys.chunks(1_000) {
            let mut txn = backend.begin().expect("begin");
            for key in batch {
                txn.delete(&keyspace, key).expect("delete");
            }
            backend.commit(txn).expect("commit").expect("no conflict");
        }
    }
}

/// Removes `room_id`'s "records complete" marker alone, so its next load replays over records
/// that are all there.
fn forget_migration<B: KvBackend>(backend: &B, room_id: &RoomId) {
    let keyspace = backend
        .keyspace(hs_state::durable::KS_LAYOUT)
        .expect("the layout keyspace");
    let mut txn = backend.begin().expect("begin");
    txn.delete(&keyspace, room_id.as_bytes()).expect("delete");
    backend.commit(txn).expect("commit").expect("no conflict");
}

#[test]
fn a_room_whose_records_are_complete_loads_as_a_replay_would_without_feeding_the_store() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).unwrap();
    let owner = build(&backend, &tables);
    let room_id = owner.room_id().to_owned();
    let expected = answers(&owner);
    assert!(expected.timeline.len() > 15, "{expected:?}");
    assert_eq!(expected.relations.len(), 1, "{expected:?}");

    let loaded = load(&backend, &tables, &room_id);
    let stats = loaded.state_store_stats();
    assert_eq!(stats.events_ingested, 0, "nothing fed: {stats:?}");
    assert_eq!(stats.events_replayed, 0, "nothing replayed: {stats:?}");
    assert_eq!(answers(&loaded), expected);

    // The same answers from a replay of the whole history into an empty store.
    drop(loaded);
    wipe_state_records(&backend);
    let replayed = load(&backend, &tables, &room_id);
    assert!(replayed.state_store_stats().events_ingested > 15);
    assert_eq!(answers(&replayed), expected);

    // And a room that keeps going after a load that read only its records.
    let mut ts = 900_000;
    let mut reloaded = load(&backend, &tables, &room_id);
    assert_eq!(reloaded.state_store_stats().events_ingested, 0);
    message(&mut reloaded, &mut ts, "after the reload");
    topic(&mut reloaded, &mut ts, "after the reload");
    let after = answers(&reloaded);
    drop(reloaded);
    wipe_state_records(&backend);
    assert_eq!(answers(&load(&backend, &tables, &room_id)), after);
}

#[test]
fn a_room_from_before_the_records_were_durable_is_replayed_once() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).unwrap();
    let owner = build(&backend, &tables);
    let room_id = owner.room_id().to_owned();
    let expected = answers(&owner);
    drop(owner);
    wipe_state_records(&backend);

    let first = load(&backend, &tables, &room_id);
    let stats = first.state_store_stats();
    assert!(
        stats.events_ingested > 15,
        "the history is fed once: {stats:?}"
    );
    assert_eq!(stats.events_replayed, 0, "into an empty store: {stats:?}");
    assert_eq!(answers(&first), expected);
    drop(first);

    for _ in 0..2 {
        let later = load(&backend, &tables, &room_id);
        let stats = later.state_store_stats();
        assert_eq!(
            stats.events_ingested, 0,
            "no replay after the migration: {stats:?}"
        );
        assert_eq!(stats.events_replayed, 0, "{stats:?}");
        assert_eq!(answers(&later), expected);
    }
}

#[test]
fn replaying_over_complete_records_by_two_writers_at_once_changes_nothing() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).unwrap();
    let owner = build(&backend, &tables);
    let room_id = owner.room_id().to_owned();
    let expected = answers(&owner);
    drop(owner);

    // One writer, then two at once, each replaying every event over records already there.
    forget_migration(&backend, &room_id);
    let replayed = load(&backend, &tables, &room_id);
    let stats = replayed.state_store_stats();
    assert_eq!(stats.events_replayed, stats.events_ingested, "{stats:?}");
    assert!(stats.events_replayed > 15, "{stats:?}");
    assert_eq!(answers(&replayed), expected);
    drop(replayed);

    forget_migration(&backend, &room_id);
    let writers: Vec<_> = (0..2)
        .map(|_| {
            let backend = backend.clone();
            let tables = tables.clone();
            let room_id = room_id.clone();
            std::thread::spawn(move || {
                let actor = load(&backend, &tables, &room_id);
                (answers(&actor), actor.state_store_stats().events_replayed)
            })
        })
        .collect();
    for writer in writers {
        let (answers, replayed) = writer.join().expect("a writer");
        assert!(replayed > 0);
        assert_eq!(answers, expected);
    }
    let after = load(&backend, &tables, &room_id);
    assert_eq!(after.state_store_stats().events_ingested, 0);
    assert_eq!(answers(&after), expected);
}

#[test]
fn a_read_copy_catches_up_from_its_owners_rows_without_feeding_the_store_again() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).unwrap();
    let mut owner = build(&backend, &tables);
    let room_id = owner.room_id().to_owned();
    let mut copy = load(&backend, &tables, &room_id);

    let mut ts = 500_000;
    let newer = message(&mut owner, &mut ts, "written after the copy was loaded");
    topic(&mut owner, &mut ts, "a topic the copy has not read");
    message(&mut owner, &mut ts, "and one more");

    // The store knows the owner's new events; the copy does not hold them until it reads
    // their rows.
    assert!(
        copy.event_by_id(&newer).is_none(),
        "not held before the catch-up"
    );
    assert_ne!(answers(&copy), answers(&owner));

    match copy.catch_up().expect("catch up") {
        CatchUp::Advanced { events, .. } => assert_eq!(events, 3),
        CatchUp::Reload(why) => panic!("asked to reload: {why}"),
    }
    let stats = copy.state_store_stats();
    assert_eq!(stats.events_ingested, 0, "the owner fed them: {stats:?}");
    assert!(copy.event_by_id(&newer).is_some());
    assert_eq!(answers(&copy), answers(&owner));
    assert_eq!(answers(&copy), answers(&load(&backend, &tables, &room_id)));
}

#[test]
fn an_actor_does_not_hold_another_rooms_events_from_the_shared_store() {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).unwrap();
    let mut first = build(&backend, &tables);
    let second = RoomActor::create_room(
        backend.clone(),
        tables.clone(),
        identity(),
        alice(),
        CreateRoomRequest::default(),
        5_000,
    )
    .unwrap();
    let mut ts = 600_000;
    let theirs = message(&mut first, &mut ts, "in the first room");
    assert!(first.event_by_id(&theirs).is_some());
    assert!(second.event_by_id(&theirs).is_none());
    assert!(second.event_sn_of(&theirs).is_none());
    let reloaded = load(&backend, &tables, second.room_id());
    assert!(reloaded.event_by_id(&theirs).is_none());
}

/// [`replaying_over_complete_records_by_two_writers_at_once_changes_nothing`] on PostgreSQL,
/// whose serializable transactions refuse two concurrent writes of one row where the embedded
/// backends let the second win: two writers migrating a room into an empty store at once write
/// the same content-addressed frames and records, and then two replay over them. Gated like
/// `hs-kv`'s PostgreSQL tests on `HS_KV_TEST_POSTGRES_DSN`, and `SKIP` when none is reachable.
#[test]
fn two_writers_migrating_a_room_at_once_on_postgresql_agree() {
    use hs_kv::postgres_backend::{PostgresBackend, PostgresOpenOptions};
    let dsn = std::env::var("HS_KV_TEST_POSTGRES_DSN")
        .unwrap_or_else(|_| "postgres://postgres:hskvtest@localhost:5433/postgres".to_owned());
    let schema = format!("hs_room_durable_state_{}", std::process::id());
    let backend = match PostgresBackend::open_with(
        &dsn,
        &PostgresOpenOptions {
            schema: schema.clone(),
            pool_size: 8,
            ..PostgresOpenOptions::default()
        },
    ) {
        Ok(backend) => backend,
        Err(e) => {
            eprintln!("SKIP: no PostgreSQL reachable at {dsn:?}: {e}");
            return;
        }
    };
    let tables = Tables::open(&backend).unwrap();
    let owner = build(&backend, &tables);
    let room_id = owner.room_id().to_owned();
    let expected = answers(&owner);
    drop(owner);

    let two_at_once = |label: &str| {
        let writers: Vec<_> = (0..2)
            .map(|_| {
                let backend = backend.clone();
                let tables = tables.clone();
                let room_id = room_id.clone();
                std::thread::spawn(move || {
                    let actor = load(&backend, &tables, &room_id);
                    (answers(&actor), actor.state_store_stats().events_ingested)
                })
            })
            .collect();
        for writer in writers {
            let (answers, ingested) = writer.join().expect("a writer");
            assert!(ingested > 15, "{label}: each writer fed the history");
            assert_eq!(answers, expected, "{label}");
        }
    };
    wipe_state_records(&backend);
    two_at_once("into an empty store");
    forget_migration(&backend, &room_id);
    two_at_once("over complete records");
    let after = load(&backend, &tables, &room_id);
    assert_eq!(after.state_store_stats().events_ingested, 0);
    assert_eq!(answers(&after), expected);
    drop(after);
    backend
        .drop_schema_for_test()
        .expect("drop the test schema");
}

/// The figure for status 04 and decision 0045: loading a room of 50,000 messages on a Fjall
/// store whose state records are complete, against replaying them over the records (what every
/// load did until 2026-10-12) and into an empty store (the one-time migration). Run it alone,
/// release: `cargo test -p hs-room --release --test durable_state measure -- --ignored
/// --nocapture`.
#[test]
#[ignore = "a measurement, minutes in a debug build"]
fn measure_load_of_a_large_room() {
    use hs_kv::fjall_backend::FjallBackend;
    const EVENTS: usize = 50_000;
    let dir = tempfile::tempdir().expect("a temporary directory");
    let backend = FjallBackend::open(dir.path().join("kv")).expect("open fjall");
    let tables = Tables::open(&backend).expect("open tables");
    let started = std::time::Instant::now();
    let mut actor = RoomActor::create_room(
        backend.clone(),
        tables.clone(),
        identity(),
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
                json!({"msgtype": "m.text", "body": format!("message {n} of a long history")}),
                None,
                10 + n as i64,
            )
            .expect("send");
        if n % 1000 == 0 {
            actor.begin_operation();
        }
    }
    let room_id = actor.room_id().to_owned();
    drop(actor);
    println!(
        "built {EVENTS} events in {:.1}s",
        started.elapsed().as_secs_f64()
    );

    let timed = |label: &str| {
        let started = std::time::Instant::now();
        let actor = load(&backend, &tables, &room_id);
        let elapsed = started.elapsed();
        let stats = actor.state_store_stats();
        println!(
            "{label}: {:.2}s, {} events fed to the state store ({} over records already there)",
            elapsed.as_secs_f64(),
            stats.events_ingested,
            stats.events_replayed
        );
        stats
    };
    let read = timed("records complete");
    assert_eq!(read.events_ingested, 0);
    forget_migration(&backend, &room_id);
    timed("replay over the records (every load until 2026-10-12)");
    wipe_state_records(&backend);
    timed("migration into an empty store (once)");
    let after = timed("records complete again");
    assert_eq!(after.events_ingested, 0);
}
