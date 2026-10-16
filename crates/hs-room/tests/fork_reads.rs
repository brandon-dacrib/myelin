//! Reading the current state of a room whose forward extremities are a fork: what the demo's
//! session hub spent 30 to 50 seconds a room update on on 2026-10-16, for `#ping`-style rooms
//! full of forks that federation never merges.
//!
//! A room with more than one forward extremity has no single "state after the last event": its
//! current state is the state resolution of the states at each extremity
//! (`RoomActor::current_view`). Every read of the current state -- `state_event`,
//! `membership_list`, `full_state` -- asks for it, and the session hub's "reading the room's
//! members and directory entry" step makes nine such reads per room update.
//!
//! - The non-ignored tests check that the current state of a forked room is resolved once per
//!   set of extremities however many reads ask for it, and that a new extremity set is resolved
//!   afresh and answers what a fresh resolution answers.
//! - The ignored `measure_hub_reads_on_a_forked_room` is the figure for status 04 and 05: one
//!   hub-shaped read (the room's members, then its directory entry's eight fields) over a room of
//!   `HS_FORK_MEMBERS` members (default 2000) forked `HS_FORK_BRANCHES` ways (default 12), each
//!   branch `HS_FORK_DEPTH` (default 40) membership changes deep. Run it alone, release:
//!
//! ```sh
//! cargo test -p hs-room --release --test fork_reads -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use hs_kv::KvBackend;
use hs_kv::fjall_backend::FjallBackend;
use hs_kv::memory::MemoryBackend;
use hs_model::ids::EventSn;
use hs_room::actor::{CreateRoomRequest, RoomActor};
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use ruma::{OwnedUserId, RoomVersionId};
use serde_json::json;

fn alice() -> OwnedUserId {
    OwnedUserId::try_from("@alice:hs1").expect("a user id")
}

fn member(n: usize) -> OwnedUserId {
    OwnedUserId::try_from(format!("@member{n}:hs1")).expect("a user id")
}

/// The shape of the forked room built.
#[derive(Debug, Clone, Copy)]
struct Shape {
    members: usize,
    branches: usize,
    depth: usize,
}

fn shape_from_env() -> Shape {
    let read = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    Shape {
        members: read("HS_FORK_MEMBERS", 2000),
        branches: read("HS_FORK_BRANCHES", 12),
        depth: read("HS_FORK_DEPTH", 40),
    }
}

/// A public room `shape.members` members join, then forked `shape.branches` ways at its head:
/// in branch `b`, `shape.depth` of its members leave one after the other, each leave citing the
/// one before, so that every branch's state disagrees with every other's on the members who
/// left in it.
fn build_forked<B: KvBackend>(backend: &B, shape: Shape) -> RoomActor<B> {
    let mut actor = RoomActor::create_room(
        backend.clone(),
        Tables::open(backend).expect("open tables"),
        HomeserverIdentity::for_tests("hs1"),
        alice(),
        CreateRoomRequest {
            preset: Some("public_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            name: Some("forked".to_owned()),
            topic: Some("a room of forks".to_owned()),
            ..Default::default()
        },
        1_000,
    )
    .expect("create the room");
    let mut ts = 2_000;
    for n in 0..shape.members {
        ts += 1;
        actor
            .membership_action(member(n), Action::Join, member(n), json!({}), ts)
            .expect("a member joins");
        if n % 256 == 0 {
            actor.begin_operation();
        }
    }
    let head: Vec<EventSn> = actor
        .forward_extremity_ids()
        .iter()
        .map(|(id, _)| actor.event_sn_of(id).expect("the head is held"))
        .collect();
    assert_eq!(head.len(), 1, "one head before the fork");
    for branch in 0..shape.branches {
        let mut prev = head.clone();
        for step in 0..shape.depth {
            ts += 1;
            let who = member(branch * shape.depth + step);
            let event = actor
                .send_event_citing(
                    who.clone(),
                    "m.room.member".to_owned(),
                    Some(who.to_string()),
                    json!({"membership": "leave"}),
                    None,
                    ts,
                    &prev,
                )
                .expect("a member of the branch leaves");
            prev = vec![actor.event_sn_of(event.event_id()).expect("just sent")];
        }
        actor.begin_operation();
    }
    assert_eq!(
        actor.forward_extremity_ids().len(),
        shape.branches,
        "one extremity per branch"
    );
    actor
}

/// What `hs-user`'s session hub reads of a room per update ("reading the room's members and
/// directory entry"): the membership list, then the directory entry's fields one state event at
/// a time, then the joined count from the membership list again. Returns the joined count.
fn hub_shaped_read<B: KvBackend>(actor: &RoomActor<B>) -> usize {
    let members = actor.membership_list().expect("the membership list");
    for (event_type, _) in [
        ("m.room.join_rules", "join_rule"),
        ("m.room.history_visibility", "history_visibility"),
        ("m.room.guest_access", "guest_access"),
        ("m.room.name", "name"),
        ("m.room.topic", "topic"),
        ("m.room.canonical_alias", "alias"),
        ("m.room.avatar", "url"),
    ] {
        actor.state_event(event_type, "").expect("a state read");
    }
    let joined = actor
        .membership_list()
        .expect("the membership list again")
        .iter()
        .filter(|(_, m)| m == "join")
        .count();
    assert_eq!(joined, members.iter().filter(|(_, m)| m == "join").count());
    joined
}

/// The resolution counter is process-wide: the tests that count resolutions take turns.
static COUNTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn counting() -> std::sync::MutexGuard<'static, ()> {
    COUNTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn a_forked_rooms_current_state_is_resolved_once_per_extremity_set() {
    let _turn = counting();
    let backend = MemoryBackend::new();
    let shape = Shape {
        members: 40,
        branches: 4,
        depth: 3,
    };
    let actor = build_forked(&backend, shape);

    let before = hs_room::metrics::state_resolutions();
    let joined = hub_shaped_read(&actor);
    assert_eq!(joined, 1 + shape.members - shape.branches * shape.depth);
    let first = hs_room::metrics::state_resolutions() - before;
    let again_before = hs_room::metrics::state_resolutions();
    for _ in 0..5 {
        hub_shaped_read(&actor);
    }
    let again = hs_room::metrics::state_resolutions() - again_before;
    assert_eq!(first, 1, "the first read resolved the fork {first} times");
    assert_eq!(again, 0, "reads of an unchanged fork resolved it again");
}

#[test]
fn a_new_extremity_set_is_resolved_afresh() {
    let _turn = counting();
    let backend = MemoryBackend::new();
    let shape = Shape {
        members: 30,
        branches: 3,
        depth: 2,
    };
    let mut actor = build_forked(&backend, shape);
    let joined_before = hub_shaped_read(&actor);
    assert_eq!(
        joined_before,
        1 + shape.members - shape.branches * shape.depth
    );

    // A member who stayed in every branch leaves, citing one branch's tip only: still a fork,
    // of a different set of extremities, whose state counts one member fewer.
    let tips: Vec<EventSn> = actor
        .forward_extremity_ids()
        .iter()
        .map(|(id, _)| actor.event_sn_of(id).expect("held"))
        .collect();
    let who = member(shape.members - 1);
    actor
        .send_event_citing(
            who.clone(),
            "m.room.member".to_owned(),
            Some(who.to_string()),
            json!({"membership": "leave"}),
            None,
            9_000_000,
            &tips[..1],
        )
        .expect("a member leaves on one branch");
    actor.begin_operation();
    assert_eq!(actor.forward_extremity_ids().len(), shape.branches);
    assert_eq!(hub_shaped_read(&actor), joined_before - 1);
    let membership = actor
        .state_event("m.room.member", who.as_str())
        .expect("read")
        .and_then(|e| {
            e.json()
                .get("content")
                .and_then(|c| c.as_object())
                .and_then(|c| c.get("membership"))
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        });
    assert_eq!(membership.as_deref(), Some("leave"));

    // A send merges every extremity into one: the state is a lookup again, and agrees.
    actor
        .send_event(
            alice(),
            "m.room.message".to_owned(),
            None,
            json!({"msgtype": "m.text", "body": "merge"}),
            None,
            9_000_001,
        )
        .expect("a merging send");
    actor.begin_operation();
    assert_eq!(actor.forward_extremity_ids().len(), 1);
    assert_eq!(hub_shaped_read(&actor), joined_before - 1);
}

#[test]
#[ignore = "a measurement; run alone in release"]
fn measure_hub_reads_on_a_forked_room() {
    let _turn = counting();
    let shape = shape_from_env();
    let dir = tempfile::tempdir().expect("a temporary directory");
    let backend = FjallBackend::open(dir.path().join("kv")).expect("open fjall");
    let built = Instant::now();
    let mut actor = build_forked(&backend, shape);
    eprintln!("built {shape:?} in {:.1?}", built.elapsed());

    let mut times: Vec<Duration> = Vec::new();
    let resolutions_before = hs_room::metrics::state_resolutions();
    for _ in 0..5 {
        let started = Instant::now();
        hub_shaped_read(&actor);
        times.push(started.elapsed());
    }
    let resolutions = hs_room::metrics::state_resolutions() - resolutions_before;
    let one = Instant::now();
    actor.state_event("m.room.topic", "").expect("one read");
    eprintln!(
        "hub-shaped reads: {:?}; one state read after them: {:.1?}; resolutions: {resolutions}",
        times,
        one.elapsed()
    );

    // Who is joined, as `hs-cli`'s federation sender and appservice delivery asked it until
    // 2026-10-16 (every member event, parsed) and as they ask it now (the membership list).
    let misses_before = hs_room::metrics::event_cache_misses();
    let started = Instant::now();
    for _ in 0..5 {
        actor.begin_operation();
        let joined = actor.joined_members().expect("joined members").len();
        assert!(joined > 0);
    }
    let by_events = started.elapsed() / 5;
    let misses = (hs_room::metrics::event_cache_misses() - misses_before) / 5;
    let started = Instant::now();
    for _ in 0..5 {
        let joined = actor
            .membership_list()
            .expect("membership list")
            .iter()
            .filter(|(_, m)| m == "join")
            .count();
        assert!(joined > 0);
    }
    eprintln!(
        "joined members by their events: {by_events:.1?} and {misses} event reads each; by the \
         membership list: {:.1?}",
        started.elapsed() / 5
    );
}
