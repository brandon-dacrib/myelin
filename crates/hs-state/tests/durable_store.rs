//! The production state store's durable records
//! (`docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`): a store reopened on the
//! same backend answers every question the first one did without reading an event record or
//! replaying an event; records are written inside the caller's transaction when it hands one in;
//! and a room written before the records existed is migrated once by feeding it its events.

use std::collections::BTreeMap;

use hs_kv::memory::MemoryBackend;
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec};
use hs_model::canonical::{CanonicalJsonObject, to_canonical_object};
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use hs_state::durable::{ALL_KEYSPACES, CacheSizes};
use hs_state::frames::FrameRepr;
use hs_state::kv_store::{KvStateStore, LAYOUT_VERSION, NewEvent, ProductionStateStore};
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UserId};
use serde_json::json;

fn obj(v: serde_json::Value) -> CanonicalJsonObject {
    to_canonical_object(&v, true).unwrap()
}

/// One event of the scripted room, as `add_event` takes it.
#[derive(Clone)]
struct Scripted {
    sn: EventSn,
    event_id: OwnedEventId,
    event_type: &'static str,
    state_key: Option<String>,
    sender: OwnedUserId,
    content: CanonicalJsonObject,
    auth: Vec<EventSn>,
    prev: Vec<EventSn>,
}

fn user(local: &str) -> OwnedUserId {
    UserId::parse(format!("@{local}:hs1")).unwrap().to_owned()
}

fn room() -> OwnedRoomId {
    RoomId::parse("!r:hs1").unwrap().to_owned()
}

/// A room with a create, three members, power levels, a topic fork that merges, a membership
/// change on one branch, and a tail of messages: enough history to exercise resolution, the
/// chain-cover index and non-state events.
fn script() -> Vec<Scripted> {
    let creator = user("c");
    let alice = user("alice");
    let bob = user("bob");
    let mut events = Vec::new();
    let mut n = 0u64;
    let mut push = |event_type: &'static str,
                    state_key: Option<String>,
                    sender: &OwnedUserId,
                    content: serde_json::Value,
                    auth: Vec<u64>,
                    prev: Vec<u64>|
     -> u64 {
        n += 1;
        events.push(Scripted {
            sn: EventSn::new(n),
            event_id: EventId::parse(format!("${n}:hs1")).unwrap().to_owned(),
            event_type,
            state_key,
            sender: sender.clone(),
            content: obj(content),
            auth: auth.into_iter().map(EventSn::new).collect(),
            prev: prev.into_iter().map(EventSn::new).collect(),
        });
        n
    };
    let create = push(
        "m.room.create",
        Some(String::new()),
        &creator,
        json!({"room_version": "11"}),
        vec![],
        vec![],
    );
    let c_join = push(
        "m.room.member",
        Some(creator.to_string()),
        &creator,
        json!({"membership": "join"}),
        vec![create],
        vec![create],
    );
    let pl = push(
        "m.room.power_levels",
        Some(String::new()),
        &creator,
        json!({"users": {creator.as_str(): 100, alice.as_str(): 50}, "state_default": 50, "ban": 50, "kick": 50}),
        vec![create, c_join],
        vec![c_join],
    );
    let jr = push(
        "m.room.join_rules",
        Some(String::new()),
        &creator,
        json!({"join_rule": "public"}),
        vec![create, c_join, pl],
        vec![pl],
    );
    let a_join = push(
        "m.room.member",
        Some(alice.to_string()),
        &alice,
        json!({"membership": "join"}),
        vec![create, pl, jr],
        vec![jr],
    );
    let b_join = push(
        "m.room.member",
        Some(bob.to_string()),
        &bob,
        json!({"membership": "join"}),
        vec![create, pl, jr],
        vec![a_join],
    );
    // A fork: two topics and a kick on one branch.
    let topic_a = push(
        "m.room.topic",
        Some(String::new()),
        &creator,
        json!({"topic": "a"}),
        vec![create, pl, c_join],
        vec![b_join],
    );
    let topic_b = push(
        "m.room.topic",
        Some(String::new()),
        &alice,
        json!({"topic": "b"}),
        vec![create, pl, a_join],
        vec![b_join],
    );
    let kick = push(
        "m.room.member",
        Some(bob.to_string()),
        &creator,
        json!({"membership": "leave"}),
        vec![create, pl, c_join, b_join],
        vec![topic_a],
    );
    let merge = push(
        "m.room.message",
        None,
        &creator,
        json!({"body": "merged"}),
        vec![create, pl, c_join],
        vec![kick, topic_b],
    );
    let mut tip = merge;
    for i in 0..40 {
        tip = push(
            "m.room.message",
            None,
            if i % 2 == 0 { &creator } else { &alice },
            json!({"body": format!("message {i}")}),
            vec![],
            vec![tip],
        );
    }
    let _ = tip;
    events
}

fn feed<B: KvBackend>(store: &ProductionStateStore<B>, events: &[Scripted]) {
    for e in events {
        store
            .add_event(
                e.sn,
                e.event_id.clone(),
                room(),
                e.event_type,
                e.state_key.as_deref(),
                e.sender.clone(),
                e.content.clone(),
                e.sn.get() as i64,
                1_790_000_000_000 + e.sn.get() as i64,
                &e.auth,
                &e.prev,
                e.prev.len() == 1 && e.prev[0] == EventSn::new(1),
            )
            .unwrap();
    }
}

/// The state after `sn`, as `(type, state_key) -> EventSn`, read back through the store.
fn state_after<B: KvBackend>(
    store: &ProductionStateStore<B>,
    events: &[Scripted],
    sn: EventSn,
) -> BTreeMap<(String, String), EventSn> {
    let root = store.state_at(sn).unwrap();
    let diff = store.diff(store.empty_root(), root).unwrap();
    let mut out = BTreeMap::new();
    for (_, value) in diff.added {
        let e = events.iter().find(|e| e.sn == value).unwrap();
        out.insert(
            (e.event_type.to_owned(), e.state_key.clone().unwrap()),
            value,
        );
    }
    out
}

fn all_states<B: KvBackend>(
    store: &ProductionStateStore<B>,
    events: &[Scripted],
) -> Vec<BTreeMap<(String, String), EventSn>> {
    events
        .iter()
        .map(|e| state_after(store, events, e.sn))
        .collect()
}

#[test]
fn a_reopened_store_answers_without_reading_records_or_replaying() {
    let backend = MemoryBackend::new();
    let events = script();
    let first = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    feed(&first, &events);
    let expected = all_states(&first, &events);
    let last = events.last().unwrap().sn;
    let first_root = first.state_at(last).unwrap();
    let chain_before = first
        .auth_chain_difference(&[vec![EventSn::new(9)], vec![EventSn::new(8)]])
        .unwrap();
    drop(first);

    let second = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    assert_eq!(second.stats(), Default::default(), "an open reads nothing");

    // Current state, by root and by key.
    assert_eq!(second.state_at(last).unwrap(), first_root);
    let topic = second.intern_state_key("m.room.topic", "").unwrap();
    let bob = second
        .intern_state_key("m.room.member", user("bob").as_str())
        .unwrap();
    assert_eq!(
        second.get(first_root, topic).unwrap(),
        Some(EventSn::new(8))
    );
    assert_eq!(second.get(first_root, bob).unwrap(), Some(EventSn::new(9)));
    assert_eq!(
        second.current_state(&RoomVersionId::V11, &[last]).unwrap(),
        first_root
    );
    assert!(second.has_event(last).unwrap());
    assert!(!second.has_event(EventSn::new(9_999)).unwrap());

    // The chain-cover index, lazily from the store.
    assert_eq!(
        second
            .auth_chain_contains(EventSn::new(9), EventSn::new(1))
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        second
            .auth_chain_contains(EventSn::new(1), EventSn::new(9))
            .unwrap(),
        Some(false)
    );
    assert!(
        second.chain_position(EventSn::new(10)).unwrap().is_none(),
        "a message is not indexed"
    );
    let mut chain_after = second
        .auth_chain_difference(&[vec![EventSn::new(9)], vec![EventSn::new(8)]])
        .unwrap();
    let mut chain_before = chain_before;
    chain_before.sort();
    chain_after.sort();
    assert_eq!(chain_after, chain_before);

    // Every historical state reconstructs from its frames.
    assert_eq!(all_states(&second, &events), expected);

    let stats = second.stats();
    assert_eq!(stats.records_read, 0, "no event record was read");
    assert_eq!(stats.events_replayed, 0);
    assert_eq!(stats.events_ingested, 0);
    assert!(stats.state_at_reads >= events.len() as u64);
}

#[test]
fn interning_is_stable_across_reopen() {
    let backend = MemoryBackend::new();
    let first = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    let a = first.intern_state_key("m.room.member", "@a:hs1").unwrap();
    let b = first.intern_state_key("m.room.topic", "").unwrap();
    drop(first);
    let second = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    assert_eq!(second.intern_state_key("m.room.topic", "").unwrap(), b);
    assert_eq!(
        second.intern_state_key("m.room.member", "@a:hs1").unwrap(),
        a
    );
    let c = second.intern_state_key("m.room.name", "").unwrap();
    assert_ne!(c, a);
    assert_ne!(c, b);
}

#[test]
fn ingest_in_writes_inside_the_callers_transaction() {
    let backend = MemoryBackend::new();
    let events = script();
    let store = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    feed(&store, &events[..2]);

    let e = &events[2];
    fn new_event(e: &Scripted) -> NewEvent<'_> {
        NewEvent {
            event: e.sn,
            event_id: e.event_id.clone(),
            room_id: room(),
            event_type: e.event_type,
            state_key: e.state_key.as_deref(),
            sender: e.sender.clone(),
            content: e.content.clone(),
            depth: e.sn.get() as i64,
            origin_server_ts: e.sn.get() as i64,
            auth_events: &e.auth,
            prev_events: &e.prev,
            only_prev_event_is_room_create: false,
        }
    }

    // A transaction that is dropped leaves nothing behind.
    {
        let mut txn = backend.begin().unwrap();
        let root = store.ingest_in(&mut txn, &new_event(e), None).unwrap();
        assert_ne!(root, store.empty_root());
        drop(txn);
    }
    assert!(!store.has_event(e.sn).unwrap());
    let other = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    assert!(!other.has_event(e.sn).unwrap());

    // A committed one is visible to this store and to another handle, with the caller's own
    // row alongside it.
    let marker = backend.keyspace("test_marker").unwrap();
    let mut txn = backend.begin().unwrap();
    let root = store.ingest_in(&mut txn, &new_event(e), None).unwrap();
    txn.put(&marker, b"k", b"v").unwrap();
    backend.commit(txn).unwrap().unwrap();
    assert!(store.has_event(e.sn).unwrap());
    assert_eq!(store.state_at(e.sn).unwrap(), root);
    assert_eq!(other.state_at(e.sn).unwrap(), root);
    assert!(backend.snapshot().get(&marker, b"k").unwrap().is_some());
    assert!(
        other.chain_position(e.sn).unwrap().is_some(),
        "the chain position committed with the transaction"
    );

    // The rest of the room through the transactional path, in batches that read what the
    // same transaction already wrote.
    for batch in events[3..].chunks(5) {
        let mut txn = backend.begin().unwrap();
        for e in batch {
            store.ingest_in(&mut txn, &new_event(e), None).unwrap();
        }
        backend.commit(txn).unwrap().unwrap();
    }
    let last = events.last().unwrap().sn;
    let topic = store.intern_state_key("m.room.topic", "").unwrap();
    assert_eq!(
        store.get(store.state_at(last).unwrap(), topic).unwrap(),
        Some(EventSn::new(8))
    );
}

#[test]
fn replaying_a_known_event_counts_as_a_replay_and_changes_nothing() {
    let backend = MemoryBackend::new();
    let events = script();
    let store = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    feed(&store, &events);
    let expected = all_states(&store, &events);
    let positions: Vec<_> = events
        .iter()
        .map(|e| store.chain_position(e.sn).unwrap())
        .collect();
    let replayed_before = hs_state::metrics::events_replayed();

    feed(&store, &events);
    assert_eq!(all_states(&store, &events), expected);
    assert_eq!(
        events
            .iter()
            .map(|e| store.chain_position(e.sn).unwrap())
            .collect::<Vec<_>>(),
        positions,
        "a replayed event keeps its chain position"
    );
    assert_eq!(store.stats().events_replayed, events.len() as u64);
    assert_eq!(
        hs_state::metrics::events_replayed() - replayed_before,
        events.len() as u64
    );
}

#[test]
fn the_record_cache_is_bounded_and_counted() {
    let backend = MemoryBackend::new();
    let events = script();
    let repr = FrameRepr::new(backend.clone()).unwrap();
    let store = KvStateStore::with_cache_sizes(
        RoomVersionId::V11,
        repr,
        backend.clone(),
        CacheSizes {
            records: 3,
            ..CacheSizes::default()
        },
    )
    .unwrap();
    feed(&store, &events);
    // Resolving the fork again reads records: the two topics, the kick, and their auth events.
    let a = store.state_at(EventSn::new(9)).unwrap();
    let b = store.state_at(EventSn::new(8)).unwrap();
    let merged = store.resolve(&RoomVersionId::V11, &[a, b]).unwrap();
    let topic = store.intern_state_key("m.room.topic", "").unwrap();
    assert_eq!(store.get(merged, topic).unwrap(), Some(EventSn::new(8)));
    assert!(store.stats().records_read > 0);
    assert!(store.resolution_events_cached() <= 3);
    assert!(store.resolution_events_cached() > 0);
    // The process-wide gauge sums every live store's cache (other tests' stores included), so
    // it is at least this store's count while it lives and never negative once it is gone.
    assert!(
        hs_state::metrics::resolution_events_cached() >= store.resolution_events_cached() as i64
    );
    drop(store);
    assert!(hs_state::metrics::resolution_events_cached() >= 0);
}

/// Deletes every row of every durable record keyspace: what a data directory written before
/// the records existed looks like (frames only).
fn wipe_records(backend: &MemoryBackend) {
    for name in ALL_KEYSPACES {
        let ks = backend.keyspace(name).unwrap();
        let keys: Vec<Vec<u8>> = backend
            .snapshot()
            .range(&ks, RangeSpec::full())
            .map(|item| item.unwrap().0.to_vec())
            .collect();
        for chunk in keys.chunks(1_000) {
            let mut txn = backend.begin().unwrap();
            for key in chunk {
                txn.delete(&ks, key).unwrap();
            }
            backend.commit(txn).unwrap().unwrap();
        }
    }
}

#[test]
fn a_room_from_before_the_records_is_migrated_once_and_answers_the_same() {
    let backend = MemoryBackend::new();
    let events = script();
    let room_id = room();

    // The legacy layout: frames referencing a per-process key numbering (shifted here by
    // interning two keys first, as a process that had seen other rooms would have), and no
    // records at all.
    let legacy = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    legacy.intern_state_key("m.room.name", "").unwrap();
    legacy.intern_state_key("m.room.avatar", "").unwrap();
    feed(&legacy, &events);
    let expected = all_states(&legacy, &events);
    drop(legacy);
    wipe_records(&backend);

    let migrations_before = hs_state::metrics::migrations();
    let store = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    assert!(store.needs_migration(&room_id).unwrap());
    assert!(!store.has_event(EventSn::new(1)).unwrap());

    // The migration is the room's events, fed once.
    feed(&store, &events);
    store.mark_migrated(&room_id, events.len() as u64).unwrap();
    assert!(!store.needs_migration(&room_id).unwrap());
    assert_eq!(
        store.layout_version(&room_id).unwrap(),
        Some(LAYOUT_VERSION)
    );
    assert_eq!(hs_state::metrics::migrations() - migrations_before, 1);
    assert_eq!(
        store.stats().events_replayed,
        0,
        "nothing was there to replay"
    );
    assert_eq!(all_states(&store, &events), expected);
    drop(store);

    // Later opens find the marker and read current state only.
    let later = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    assert!(!later.needs_migration(&room_id).unwrap());
    assert_eq!(all_states(&later, &events), expected);
    assert_eq!(later.stats().records_read, 0);
    assert_eq!(later.stats().events_ingested, 0);
}

#[test]
fn a_room_created_after_the_change_is_marked_in_its_own_transaction() {
    let backend = MemoryBackend::new();
    let store = ProductionStateStore::open(RoomVersionId::V11, backend.clone()).unwrap();
    let room_id = room();
    assert!(store.needs_migration(&room_id).unwrap());
    let mut txn = backend.begin().unwrap();
    store.mark_migrated_in(&mut txn, &room_id).unwrap();
    assert!(
        store.needs_migration(&room_id).unwrap(),
        "not until it commits"
    );
    backend.commit(txn).unwrap().unwrap();
    assert!(!store.needs_migration(&room_id).unwrap());
}
