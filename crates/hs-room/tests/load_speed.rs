//! How long a room takes to load, and where the time goes (`RoomActor::load`'s phases,
//! `hs_room::actor::load_report`), for the two shapes of room the demo was slow on on
//! 2026-10-15:
//!
//! - a room **joined over federation and read back** (`federated`): a resident server's room of
//!   many members and a long history, joined by a second server (on a Fjall store) and backfilled
//!   from its join to the room's creation, batch by batch with the state at each batch fetched,
//!   as `hs-cli`'s backfill does. Every placed event carries the state before it
//!   (`Tables::state_snapshots`), and every member event the join brought as an outlier is placed
//!   in the timeline when its history arrives;
//! - a **local** room of many members and a long history (`local`).
//!
//! The non-ignored tests run small rooms and check that a reloaded room answers the same; the
//! ignored `measure_*` tests are the figures for status 04. Run them alone, release:
//!
//! ```sh
//! HS_ROOM_LOAD_MEMBERS=500 cargo test -p hs-room --release --test load_speed -- --ignored --nocapture
//! ```
//!
//! As in `tests/backfill_state.rs`, two backends stand in for two homeservers and nothing checks
//! a signature.

use std::time::{Duration, Instant};

use hs_kv::KvBackend;
use hs_kv::fjall_backend::FjallBackend;
use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_room::actor::load_report::LoadReport;
use hs_room::actor::{CreateRoomRequest, RoomActor, StateAtEvent};
use hs_room::backfill::{FetchedState, HistoryKind};
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use hs_room::timeline::{Direction, PaginationToken};
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId};
use serde_json::json;

fn member(n: usize) -> OwnedUserId {
    OwnedUserId::try_from(format!("@member{n}:a.example")).expect("a user id")
}

fn alice() -> OwnedUserId {
    OwnedUserId::try_from("@alice:a.example").expect("a user id")
}

/// The shape of the room built.
#[derive(Debug, Clone, Copy)]
struct Shape {
    /// Members who join, each saying something.
    members: usize,
    /// Messages after everybody has joined.
    messages: usize,
    /// Every this many messages, a state change (the topic).
    topic_every: usize,
}

fn shape_from_env(default_members: usize, default_messages: usize) -> Shape {
    let read = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    Shape {
        members: read("HS_ROOM_LOAD_MEMBERS", default_members),
        messages: read("HS_ROOM_LOAD_MESSAGES", default_messages),
        topic_every: read("HS_ROOM_LOAD_TOPIC_EVERY", 50),
    }
}

/// Builds the room on `actor`'s backend: `shape.members` joins, each followed by a message and
/// every so often a topic change, then `shape.messages` more messages.
fn build_room<B: KvBackend>(actor: &mut RoomActor<B>, shape: Shape) {
    let mut ts = 10;
    let mut tick = || {
        ts += 1;
        ts
    };
    let mut sent = 0usize;
    let say = |actor: &mut RoomActor<B>, who: OwnedUserId, ts: i64, sent: &mut usize| {
        actor
            .send_event(
                who,
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": format!("message {sent}, about the length of a typical line of chat")}),
                None,
                ts,
            )
            .expect("a message");
        *sent += 1;
        if (*sent).is_multiple_of(256) {
            actor.begin_operation();
        }
    };
    for n in 0..shape.members {
        let who = member(n);
        actor
            .membership_action(who.clone(), Action::Join, who.clone(), json!({}), tick())
            .expect("a member joins");
        say(actor, who, tick(), &mut sent);
        if n % shape.topic_every == 0 {
            actor
                .send_event(
                    alice(),
                    "m.room.topic".to_owned(),
                    Some(String::new()),
                    json!({"topic": format!("topic {n}")}),
                    None,
                    tick(),
                )
                .expect("a topic");
        }
    }
    for n in 0..shape.messages {
        let who = if shape.members == 0 {
            alice()
        } else {
            member(n % shape.members)
        };
        say(actor, who, tick(), &mut sent);
    }
}

fn resident_room(shape: Shape) -> RoomActor<MemoryBackend> {
    let backend = MemoryBackend::new();
    let mut actor = RoomActor::create_room(
        backend.clone(),
        Tables::open(&backend).expect("open tables"),
        HomeserverIdentity::for_tests("a.example"),
        alice(),
        CreateRoomRequest {
            preset: Some("public_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            ..Default::default()
        },
        1,
    )
    .expect("create the resident room");
    build_room(&mut actor, shape);
    actor
}

/// The resident's `/backfill` from `from`, `limit` events, newest first.
fn resident_backfill(
    resident: &RoomActor<MemoryBackend>,
    from: &OwnedEventId,
    limit: usize,
) -> Vec<Event> {
    let pos = resident
        .timeline_position(from)
        .expect("in the resident's timeline");
    let (page, _) = resident.paginate(
        Some(PaginationToken::new(pos, Direction::Backward)),
        Direction::Backward,
        limit,
    );
    page.into_iter().cloned().collect()
}

/// Joins `resident`'s room from `b.example` on `backend` and backfills it to its creation, as
/// `hs-cli`'s backfill does: batches of `batch` events, the state at each batch's oldest event
/// fetched. Returns the room's ID.
fn join_and_backfill<B: KvBackend>(
    resident: &mut RoomActor<MemoryBackend>,
    backend: &B,
    batch: usize,
) -> OwnedRoomId {
    let tables = Tables::open(backend).expect("open tables");
    let bob = OwnedUserId::try_from("@bob:b.example").expect("a user id");
    let join = resident
        .membership_action(bob.clone(), Action::Join, bob, json!({}), 1_000_000_000)
        .expect("bob joins");
    let StateAtEvent { state, auth_chain } = resident
        .state_before_event(join.event_id())
        .expect("state lookup")
        .expect("known");
    let room_id = resident.room_id().to_owned();
    let mut joiner = RoomActor::create_from_remote_join(
        backend.clone(),
        tables,
        HomeserverIdentity::for_tests("b.example"),
        &room_id,
        RoomVersionId::V11,
        state,
        auth_chain,
        join.clone(),
    )
    .expect("bootstrap from the join response");
    let mut from = join.event_id().to_owned();
    loop {
        let page = resident_backfill(resident, &from, batch);
        let Some(oldest) = page.last() else {
            break;
        };
        let next_from = oldest.event_id().to_owned();
        let Some(plan) = joiner
            .plan_history(HistoryKind::BeforeOldest, &page)
            .expect("plan")
        else {
            break;
        };
        let StateAtEvent { state, auth_chain } = resident
            .state_before_event(&plan.oldest)
            .expect("state lookup")
            .expect("the resident knows the event");
        let state_ids: Vec<OwnedEventId> = state.iter().map(|e| e.event_id().to_owned()).collect();
        let mut wanted = state_ids.clone();
        wanted.extend(auth_chain.iter().map(|e| e.event_id().to_owned()));
        wanted.extend(plan.missing_auth.iter().cloned());
        let events = joiner
            .events_not_held(&wanted)
            .iter()
            .filter_map(|id| resident.event_by_id(id).cloned())
            .collect();
        joiner
            .accept_history(
                HistoryKind::BeforeOldest,
                page,
                Some(FetchedState {
                    at: plan.oldest.clone(),
                    state_ids,
                    events,
                }),
            )
            .expect("the batch is placed");
        joiner.begin_operation();
        if next_from == from {
            break;
        }
        from = next_from;
    }
    room_id
}

fn load<B: KvBackend>(backend: &B, room_id: &OwnedRoomId) -> (RoomActor<B>, LoadReport, Duration) {
    let started = Instant::now();
    let mut actor = RoomActor::load(
        backend.clone(),
        Tables::open(backend).expect("open tables"),
        HomeserverIdentity::for_tests("b.example"),
        room_id,
    )
    .expect("load")
    .expect("the room exists");
    let took = started.elapsed();
    let report = actor.take_load_report().expect("a load report");
    (actor, report, took)
}

fn print_report(label: &str, report: &LoadReport, took: Duration) {
    println!(
        "{label}: {:.3}s (timeline {}, outliers {}, state rows {}, deferred roots {}): {}",
        took.as_secs_f64(),
        report.timeline_events,
        report.outliers,
        report.state_rows,
        report.deferred_roots,
        report.phases_line()
    );
}

/// The members and the timeline of a room as its actor answers them, to compare a reload
/// against the actor that wrote the room.
fn fingerprint<B: KvBackend>(actor: &RoomActor<B>) -> (Vec<String>, Vec<OwnedEventId>) {
    let mut members: Vec<String> = actor
        .members()
        .expect("members")
        .iter()
        .filter_map(|e| e.header().state_key.clone())
        .collect();
    members.sort();
    let (page, _) = actor.paginate(None, Direction::Backward, 100);
    let timeline = page.iter().map(|e| e.event_id().to_owned()).collect();
    (members, timeline)
}

#[test]
fn a_backfilled_federated_room_reloads_the_same() {
    let shape = Shape {
        members: 30,
        messages: 40,
        topic_every: 7,
    };
    let mut resident = resident_room(shape);
    let dir = tempfile::tempdir().expect("a temporary directory");
    let backend = FjallBackend::open(dir.path().join("kv")).expect("open fjall");
    let room_id = join_and_backfill(&mut resident, &backend, 20);
    let (first, report, _) = load(&backend, &room_id);
    assert!(report.timeline_events > shape.members + shape.messages);
    let before = fingerprint(&first);
    // The state at every event of the history is the resident's.
    let (page, _) = first.paginate(None, Direction::Backward, 200);
    let ids: Vec<OwnedEventId> = page.iter().map(|e| e.event_id().to_owned()).collect();
    drop(page);
    for id in ids.iter().step_by(7) {
        let mine: std::collections::BTreeSet<OwnedEventId> = first
            .state_at_event(id)
            .expect("state")
            .expect("known")
            .state
            .iter()
            .map(|e| e.event_id().to_owned())
            .collect();
        let theirs: std::collections::BTreeSet<OwnedEventId> = resident
            .state_at_event(id)
            .expect("state")
            .expect("known")
            .state
            .iter()
            .map(|e| e.event_id().to_owned())
            .collect();
        assert_eq!(mine, theirs, "state at {id}");
    }
    drop(first);
    let (second, _, _) = load(&backend, &room_id);
    assert_eq!(fingerprint(&second), before);
}

/// The figures: a federated room joined and backfilled, and a local room, each of
/// `HS_ROOM_LOAD_MEMBERS` members (default 300) and `HS_ROOM_LOAD_MESSAGES` messages after
/// (default 2,000), loaded cold (the Fjall store reopened) and warm.
#[test]
#[ignore = "builds large rooms and times their loads; run alone, release, with --nocapture"]
fn measure_room_loads() {
    let shape = shape_from_env(300, 2_000);
    println!("shape: {shape:?}");
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("kv");

    let started = Instant::now();
    let mut resident = resident_room(shape);
    println!(
        "built the resident room in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    let (federated, local) = {
        let backend = FjallBackend::open(&path).expect("open fjall");
        let started = Instant::now();
        let federated = join_and_backfill(&mut resident, &backend, 100);
        println!(
            "joined and backfilled in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        let tables = Tables::open(&backend).expect("open tables");
        let mut local = RoomActor::create_room(
            backend.clone(),
            tables,
            HomeserverIdentity::for_tests("b.example"),
            OwnedUserId::try_from("@alice:b.example").expect("a user id"),
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                room_version: Some(RoomVersionId::V11),
                ..Default::default()
            },
            1,
        )
        .expect("create the local room");
        let local_shape = Shape {
            members: shape.members,
            messages: shape.messages,
            topic_every: shape.topic_every,
        };
        build_room_local(&mut local, local_shape);
        (federated, local.room_id().to_owned())
    };
    for (label, room_id) in [("federated", &federated), ("local", &local)] {
        // Cold: the store reopened, so nothing of the room is in Fjall's caches.
        let backend = FjallBackend::open(&path).expect("reopen fjall");
        let (actor, report, took) = load(&backend, room_id);
        print_report(&format!("{label} cold"), &report, took);
        drop(actor);
        let (actor, report, took) = load(&backend, room_id);
        print_report(&format!("{label} warm"), &report, took);
        let started = Instant::now();
        let members = actor.members().expect("members").len();
        println!(
            "{label}: members() of {members} took {:.3}s",
            started.elapsed().as_secs_f64()
        );
        // The state at every 50th event of the history, placed member joins among them: what a
        // client reading the room's history back makes the server compute.
        let (page, _) = actor.paginate(
            Some(PaginationToken::new(i64::MIN / 2, Direction::Forward)),
            Direction::Forward,
            usize::MAX,
        );
        let ids: Vec<OwnedEventId> = page
            .iter()
            .step_by(50)
            .map(|e| e.event_id().to_owned())
            .collect();
        drop(page);
        let started = Instant::now();
        for id in &ids {
            actor.state_at_event(id).expect("state").expect("known");
        }
        println!(
            "{label}: the state at {} events across the history took {:.3}s",
            ids.len(),
            started.elapsed().as_secs_f64()
        );
    }
}

/// [`build_room`] for a room of `b.example`'s own: its members are `b.example`'s.
fn build_room_local<B: KvBackend>(actor: &mut RoomActor<B>, shape: Shape) {
    let local = |n: usize| OwnedUserId::try_from(format!("@member{n}:b.example")).expect("id");
    let mut ts = 10;
    for n in 0..shape.members {
        ts += 1;
        actor
            .membership_action(local(n), Action::Join, local(n), json!({}), ts)
            .expect("a member joins");
        if n % 256 == 0 {
            actor.begin_operation();
        }
    }
    for n in 0..shape.messages + shape.members {
        ts += 1;
        let who = if shape.members == 0 {
            OwnedUserId::try_from("@alice:b.example").expect("id")
        } else {
            local(n % shape.members)
        };
        actor
            .send_event(
                who,
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": format!("message {n}, about the length of a typical line of chat")}),
                None,
                ts,
            )
            .expect("a message");
        if n % 256 == 0 {
            actor.begin_operation();
        }
    }
}
