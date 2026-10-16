//! What one state resolution of a federated room's forks costs, cold: the demo's `#ping` room
//! (2026-10-16, v0.3.25) took 15.8 s in its load's `finish` phase -- one resolution of its
//! forward extremities -- with 797 state events, 2,157 outliers and 613 timeline events.
//!
//! `fork_reads.rs` measures a local room forked at its head, where the forks differ by a few
//! dozen membership changes; that resolves in tens of milliseconds and is not the demo's shape.
//! Here the room is the demo's: joined over federation (its state and auth chain held as
//! outliers), and forked by *stuck* extremities -- events another server sent citing a prev
//! event this server did not hold, taken with the state another server answered for it
//! (`/state_ids`, `RoomActor::accept_prev_event_with_state`), as `hs-federation`'s state
//! fallback does. Each such extremity carries the room's state as of a point far back in its
//! history, so the forks' auth chains differ by most of the room.
//!
//! As in `tests/load_speed.rs`, two backends stand in for two homeservers and nothing checks a
//! signature.
//!
//! - The non-ignored test checks that a cold reload of a small forked room answers the same
//!   current state as the actor that built it.
//! - The ignored `measure_cold_resolution_of_a_federated_fork` is the figure for status 02 and
//!   04. Run it alone, release:
//!
//! ```sh
//! cargo test -p hs-room --release --test fork_resolution -- --ignored --nocapture
//! ```
//!
//! Shape: `HS_FED_MEMBERS` members (default 800) join the resident's room, each saying
//! something; every `HS_FED_PL_EVERY` joins (default 10) the creator raises the newest member's
//! power level (a long power-level mainline), and every `HS_FED_KICK_EVERY` (default 25) kicks
//! the one before (power events). Then `b.example` joins, `HS_FED_AFTER` more members join
//! (default 100), and `HS_FED_FORKS` stuck extremities (default 24) arrive, citing messages
//! spread across the history before the join.

use std::time::{Duration, Instant};

use hs_kv::KvBackend;
use hs_kv::fjall_backend::FjallBackend;
use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_room::actor::{CreateRoomRequest, RoomActor, StateAtEvent};
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId};
use serde_json::json;

fn alice() -> OwnedUserId {
    OwnedUserId::try_from("@alice:a.example").expect("a user id")
}

fn member(n: usize) -> OwnedUserId {
    OwnedUserId::try_from(format!("@member{n}:a.example")).expect("a user id")
}

/// The shape of the room built.
#[derive(Debug, Clone, Copy)]
struct Shape {
    members: usize,
    pl_every: usize,
    kick_every: usize,
    after: usize,
    forks: usize,
    /// 12 (the demo's rooms: their IDs carry no server name) or 11.
    version: u8,
}

impl Shape {
    fn room_version(&self) -> RoomVersionId {
        if self.version == 11 {
            RoomVersionId::V11
        } else {
            RoomVersionId::V12
        }
    }
}

fn shape_from_env() -> Shape {
    let read = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    Shape {
        members: read("HS_FED_MEMBERS", 800),
        pl_every: read("HS_FED_PL_EVERY", 10),
        kick_every: read("HS_FED_KICK_EVERY", 25),
        after: read("HS_FED_AFTER", 100),
        forks: read("HS_FED_FORKS", 24),
        version: u8::try_from(read("HS_FED_VERSION", 12)).unwrap_or(12),
    }
}

struct Clock(i64);

impl Clock {
    fn tick(&mut self) -> i64 {
        self.0 += 1;
        self.0
    }
}

/// The resident's room before `b.example` joins. Returns the actor and the messages said, in
/// order (the points the stuck extremities fork from).
fn resident_history(
    shape: Shape,
    clock: &mut Clock,
) -> (RoomActor<MemoryBackend>, Vec<OwnedEventId>) {
    let backend = MemoryBackend::new();
    let mut actor = RoomActor::create_room(
        backend.clone(),
        Tables::open(&backend).expect("open tables"),
        HomeserverIdentity::for_tests("a.example"),
        alice(),
        CreateRoomRequest {
            preset: Some("public_chat".to_owned()),
            room_version: Some(shape.room_version()),
            name: Some("ping".to_owned()),
            ..Default::default()
        },
        clock.tick(),
    )
    .expect("create the resident room");
    let mut users = serde_json::Map::new();
    // From room version 12 a creator's power is implicit and may not be written down.
    if shape.version == 11 {
        users.insert(alice().to_string(), json!(100));
    }
    let mut messages = Vec::new();
    for n in 0..shape.members {
        let who = member(n);
        actor
            .membership_action(
                who.clone(),
                Action::Join,
                who.clone(),
                json!({}),
                clock.tick(),
            )
            .expect("a member joins");
        let said = actor
            .send_event(
                who.clone(),
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": format!("hello from {n}")}),
                None,
                clock.tick(),
            )
            .expect("a message");
        messages.push(said.event_id().to_owned());
        if shape.pl_every > 0 && n % shape.pl_every == 0 {
            users.insert(who.to_string(), json!(10));
            actor
                .send_event(
                    alice(),
                    "m.room.power_levels".to_owned(),
                    Some(String::new()),
                    json!({"users": users.clone(), "users_default": 0, "events_default": 0,
                           "state_default": 50, "ban": 50, "kick": 50, "redact": 50,
                           "invite": 0}),
                    None,
                    clock.tick(),
                )
                .expect("a power level change");
        }
        if shape.kick_every > 0 && n > 0 && n % shape.kick_every == 0 {
            actor
                .membership_action(
                    alice(),
                    Action::Kick,
                    member(n - 1),
                    json!({}),
                    clock.tick(),
                )
                .expect("a kick");
        }
        if n % 128 == 0 {
            actor.begin_operation();
        }
    }
    (actor, messages)
}

/// Every event of `resident`'s state before `at`, and its auth chain, that `joiner` does not
/// hold: what the state fallback fetches.
fn fetched_for<B: KvBackend>(
    resident: &RoomActor<MemoryBackend>,
    joiner: &RoomActor<B>,
    at: &OwnedEventId,
) -> (Vec<OwnedEventId>, Vec<Event>) {
    let StateAtEvent { state, auth_chain } = resident
        .state_before_event(at)
        .expect("state lookup")
        .expect("the resident knows the event");
    let state_ids: Vec<OwnedEventId> = state.iter().map(|e| e.event_id().to_owned()).collect();
    let mut wanted = state_ids.clone();
    wanted.extend(auth_chain.iter().map(|e| e.event_id().to_owned()));
    let events = joiner
        .events_not_held(&wanted)
        .iter()
        .filter_map(|id| resident.event_by_id(id).cloned())
        .collect();
    (state_ids, events)
}

/// Builds the forked room on `backend` as `b.example` holds it. Returns its ID, the joiner (as
/// built), and how long each stuck extremity's arrival took.
fn build<B: KvBackend>(backend: &B, shape: Shape) -> (OwnedRoomId, RoomActor<B>, Vec<Duration>) {
    let mut clock = Clock(1_000);
    let (mut resident, messages) = resident_history(shape, &mut clock);
    let room_id = resident.room_id().to_owned();

    // b.example joins.
    let bob = OwnedUserId::try_from("@bob:b.example").expect("a user id");
    let join = resident
        .membership_action(bob.clone(), Action::Join, bob, json!({}), clock.tick())
        .expect("bob joins");
    let StateAtEvent { state, auth_chain } = resident
        .state_before_event(join.event_id())
        .expect("state lookup")
        .expect("known");
    let mut joiner = RoomActor::create_from_remote_join(
        backend.clone(),
        Tables::open(backend).expect("open tables"),
        HomeserverIdentity::for_tests("b.example"),
        &room_id,
        shape.room_version(),
        state,
        auth_chain,
        join,
    )
    .expect("bootstrap from the join response");

    // The room goes on, and b.example hears of it.
    for k in 0..shape.after {
        let who = member(shape.members + k);
        let joined = resident
            .membership_action(who.clone(), Action::Join, who, json!({}), clock.tick())
            .expect("a member joins after b.example");
        joiner
            .accept_remote_event(joined)
            .expect("b.example takes the join");
        if k % 64 == 0 {
            resident.begin_operation();
            joiner.begin_operation();
        }
    }

    // Stuck extremities: an event citing a message from far back, its prev event taken with
    // the state another server answered for it.
    let mut arrivals = Vec::new();
    for f in 0..shape.forks {
        let index = (f * messages.len()) / shape.forks.max(1);
        let point = messages[index].clone();
        let point_event = resident.event_by_id(&point).expect("held").clone();
        let sender = point_event.header().sender.clone();
        let sn = resident.event_sn_of(&point).expect("held");
        let stuck = resident
            .send_event_citing(
                OwnedUserId::try_from(sender.as_str()).expect("a user id"),
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": format!("a reply from long ago, {f}")}),
                None,
                clock.tick(),
                &[sn],
            )
            .expect("a stuck event");
        let started = Instant::now();
        let (state_ids, fetched) = fetched_for(&resident, &joiner, &point);
        joiner
            .accept_prev_event_with_state(point_event, &state_ids, fetched)
            .expect("the prev event is held with its state");
        joiner
            .accept_remote_event(stuck)
            .expect("the stuck event is taken");
        arrivals.push(started.elapsed());
        resident.begin_operation();
        joiner.begin_operation();
    }
    (room_id, joiner, arrivals)
}

fn load<B: KvBackend>(backend: &B, room_id: &OwnedRoomId) -> (RoomActor<B>, Duration) {
    let started = Instant::now();
    let actor = RoomActor::load(
        backend.clone(),
        Tables::open(backend).expect("open tables"),
        HomeserverIdentity::for_tests("b.example"),
        room_id,
    )
    .expect("load")
    .expect("the room exists");
    (actor, started.elapsed())
}

/// The current state's membership, sorted: what a reload must agree on.
fn membership<B: KvBackend>(actor: &RoomActor<B>) -> Vec<(OwnedUserId, String)> {
    let mut list: Vec<(OwnedUserId, String)> = actor
        .membership_list()
        .expect("the membership list")
        .to_vec();
    list.sort();
    list
}

/// The rows of the state store one cold resolution of a room built to `shape` reads: the
/// load's, on a fresh actor over a reopened store.
fn rows_of_a_cold_resolution(shape: Shape) -> u64 {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("kv");
    let room_id = {
        let backend = FjallBackend::open(&path).expect("open fjall");
        build(&backend, shape).0
    };
    let backend = FjallBackend::open(&path).expect("reopen fjall");
    let resolutions = hs_state::metrics::resolution_profile::resolutions();
    let (actor, _) = load(&backend, &room_id);
    actor.membership_list().expect("the membership list");
    assert!(
        hs_state::metrics::resolution_profile::resolutions() > resolutions,
        "the forks were resolved"
    );
    // Thread-local: what this thread's last resolution read, whatever other tests do.
    hs_state::metrics::resolution_profile::last_resolution_rows()
}

/// One resolution reads each row it needs about once, however many forks it resolves: the
/// state events the forks share, their links in the chain-cover index and their auth chains
/// are read for the first fork and remembered for the rest. Until 2026-10-16 every fork's
/// state events' links and whole auth chain were read again (`measure_*` below: 156,800 rows
/// for 289 forks of an 800-member room, 5,477 since).
#[test]
fn a_resolution_reads_about_the_same_rows_for_four_forks_as_for_sixteen() {
    let shape = |forks| Shape {
        members: 60,
        pl_every: 6,
        kick_every: 9,
        after: 10,
        forks,
        version: 12,
    };
    let few = rows_of_a_cold_resolution(shape(4));
    let many = rows_of_a_cold_resolution(shape(16));
    eprintln!("rows read by one resolution: 4 forks {few}, 16 forks {many}");
    assert!(
        many < few * 3 / 2,
        "sixteen forks read {many} rows, four read {few}: the reads grow with the forks"
    );
}

#[test]
fn a_cold_reload_of_a_federated_fork_answers_the_same_state() {
    let shape = Shape {
        members: 40,
        pl_every: 5,
        kick_every: 7,
        after: 6,
        forks: 4,
        version: 12,
    };
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("kv");
    let (room_id, before, power) = {
        let backend = FjallBackend::open(&path).expect("open fjall");
        let (room_id, joiner, _) = build(&backend, shape);
        assert!(
            joiner.forward_extremity_ids().len() > 1,
            "the room is forked"
        );
        let power = joiner
            .state_event("m.room.power_levels", "")
            .expect("read")
            .map(|e| e.event_id().to_owned());
        (room_id, membership(&joiner), power)
    };
    let backend = FjallBackend::open(&path).expect("reopen fjall");
    let (reloaded, _) = load(&backend, &room_id);
    assert_eq!(membership(&reloaded), before);
    assert_eq!(
        reloaded
            .state_event("m.room.power_levels", "")
            .expect("read")
            .map(|e| e.event_id().to_owned()),
        power
    );
}

#[test]
#[ignore = "a measurement; run alone in release"]
fn measure_cold_resolution_of_a_federated_fork() {
    let shape = shape_from_env();
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("kv");
    let room_id = {
        let backend = FjallBackend::open(&path).expect("open fjall");
        let built = Instant::now();
        let (room_id, joiner, arrivals) = build(&backend, shape);
        eprintln!(
            "built {shape:?} in {:.1?}: {} extremities; the last stuck extremity's arrival took \
             {:.1?} (first {:.1?})",
            built.elapsed(),
            joiner.forward_extremity_ids().len(),
            arrivals.last().copied().unwrap_or_default(),
            arrivals.first().copied().unwrap_or_default(),
        );
        room_id
    };
    let backend = FjallBackend::open(&path).expect("reopen fjall (cold)");
    hs_state::metrics::resolution_profile::reset();
    let resolutions = hs_room::metrics::state_resolutions();
    let records = hs_state::metrics::event_records_read();
    let (mut actor, took) = load(&backend, &room_id);
    let report = actor.take_load_report().expect("a load report");
    eprintln!(
        "cold load {:.3}s (timeline {}, outliers {}, deferred roots {}): {}",
        took.as_secs_f64(),
        report.timeline_events,
        report.outliers,
        report.deferred_roots,
        report.phases_line()
    );
    let started = Instant::now();
    let members = actor.membership_list().expect("the membership list").len();
    eprintln!(
        "then the membership list ({members}): {:.3}s; resolutions {}, records read {}",
        started.elapsed().as_secs_f64(),
        hs_room::metrics::state_resolutions() - resolutions,
        hs_state::metrics::event_records_read() - records,
    );
    eprintln!(
        "first load: {}",
        hs_state::metrics::resolution_profile_line()
    );

    // Again, from a fresh actor (the store's caches empty; the backend's warm).
    drop(actor);
    hs_state::metrics::resolution_profile::reset();
    let (mut actor, took) = load(&backend, &room_id);
    let report = actor.take_load_report().expect("a load report");
    eprintln!(
        "second load {:.3}s: {}\nsecond load: {}",
        took.as_secs_f64(),
        report.phases_line(),
        hs_state::metrics::resolution_profile_line()
    );
}
