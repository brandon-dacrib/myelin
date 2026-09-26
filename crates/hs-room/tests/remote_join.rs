//! RFC 0015 (`docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`): a room this server's
//! own user joined on *another* server comes to exist here from that server's `send_join`
//! response -- `RoomActor::create_from_remote_join`, `RoomActor::accept_remote_join_with_state`
//! and `RoomRegistry::bootstrap_from_remote_join`.
//!
//! Two backends stand in for two homeservers. `a.example` hosts the room the ordinary way
//! (`RoomActor::create_room`); what it hands over is exactly what `hs_federation::join::send_join`
//! answers with: the room's resolved state *before* the join, that state's auth chain, and the
//! join event. `b.example` builds its own actor from that alone. No signatures are checked
//! anywhere in this crate -- bob's join is signed by `a.example`'s key, which a real resident
//! would refuse, but `hs-room` trusts its caller (`hs_federation::outbound_join::join_room`) to
//! have verified every event before it gets here, and that is what these tests exercise.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_room::RoomError;
use hs_room::actor::{CreateRoomRequest, RemoteEventOutcome, RoomActor, StateAtEvent};
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use hs_room::protocol::MembershipDelta;
use hs_room::registry::RoomRegistry;
use hs_room::timeline::Direction;
use ruma::{OwnedEventId, OwnedRoomId, RoomVersionId, user_id};
use serde_json::json;

const ALICE: &str = "@alice:a.example";
const BOB: &str = "@bob:b.example";

/// The room as `a.example` hosts it, and the three things its `send_join` would answer bob's
/// join with.
struct Resident {
    actor: RoomActor<MemoryBackend>,
    room_id: OwnedRoomId,
    /// Alice's message from before bob joined: the join's `prev_events`, which the snapshot does
    /// not contain.
    message: Event,
    join: Event,
    state: Vec<Event>,
    auth_chain: Vec<Event>,
}

fn resident_with_bob_joined() -> Resident {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let identity = HomeserverIdentity::for_tests("a.example");
    let alice = user_id!("@alice:a.example").to_owned();
    let bob = user_id!("@bob:b.example").to_owned();
    let mut actor = RoomActor::create_room(
        backend,
        tables,
        identity,
        alice.clone(),
        CreateRoomRequest {
            preset: Some("public_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            ..Default::default()
        },
        1,
    )
    .expect("create the resident room");
    let message = actor
        .send_event(
            alice,
            "m.room.message".to_owned(),
            None,
            json!({"msgtype": "m.text", "body": "before bob"}),
            None,
            2,
        )
        .expect("alice sends a message");
    // Bob joins through the resident's own membership pipeline. The resident signs this join
    // with `a.example`'s key; nothing in `hs-room` verifies signatures (see the module docs), so
    // for this crate it is indistinguishable from the join a real `make_join`/`send_join` round
    // trip would have produced. Its `prev_events` is alice's message.
    let join = actor
        .membership_action(bob.clone(), Action::Join, bob, json!({}), 3)
        .expect("bob joins the public room");
    let StateAtEvent { state, auth_chain } = actor
        .state_at_event(message.event_id())
        .expect("state lookup")
        .expect("the message is known");
    let room_id = actor.room_id().to_owned();
    Resident {
        actor,
        room_id,
        message,
        join,
        state,
        auth_chain,
    }
}

fn ids<'a>(events: impl IntoIterator<Item = &'a Event>) -> BTreeSet<OwnedEventId> {
    events
        .into_iter()
        .map(|e| e.event_id().to_owned())
        .collect()
}

fn keys<'a>(events: impl IntoIterator<Item = &'a Event>) -> BTreeSet<(String, String)> {
    events
        .into_iter()
        .map(|e| {
            (
                e.header().event_type.clone(),
                e.header().state_key.clone().unwrap_or_default(),
            )
        })
        .collect()
}

fn state_at<B: hs_kv::KvBackend>(actor: &RoomActor<B>, event: &Event) -> BTreeSet<OwnedEventId> {
    ids(&actor
        .state_at_event(event.event_id())
        .expect("state lookup")
        .expect("the event is known")
        .state)
}

fn timeline_ids<B: hs_kv::KvBackend>(actor: &RoomActor<B>) -> Vec<OwnedEventId> {
    actor
        .events_after(0, 100)
        .into_iter()
        .map(|(_, e)| e.event_id().to_owned())
        .collect()
}

/// The whole RFC 0015 claim, end to end: the joining server can read the room's state, see its
/// members, post into it, accept the resident's next event, survive a reload identically, and
/// leave again -- from a snapshot whose `prev_events` it never held.
#[test]
fn a_room_joined_elsewhere_is_bootstrapped_from_the_resident_snapshot() {
    let mut resident = resident_with_bob_joined();
    let alice = user_id!("@alice:a.example").to_owned();
    let bob = user_id!("@bob:b.example").to_owned();

    let backend_b = MemoryBackend::new();
    let tables_b = Tables::open(&backend_b).expect("open tables");
    let identity_b = HomeserverIdentity::for_tests("b.example");
    let mut local = RoomActor::create_from_remote_join(
        backend_b.clone(),
        tables_b.clone(),
        identity_b.clone(),
        &resident.room_id,
        RoomVersionId::V11,
        resident.state.clone(),
        resident.auth_chain.clone(),
        resident.join.clone(),
    )
    .expect("bootstrap from the join response");

    // --- the state is the resident's, plus the join ---
    let state_keys = keys(local.full_state().expect("full state"));
    for expected in [
        ("m.room.create", ""),
        ("m.room.power_levels", ""),
        ("m.room.join_rules", ""),
        ("m.room.history_visibility", ""),
        ("m.room.member", ALICE),
        ("m.room.member", BOB),
    ] {
        assert!(
            state_keys.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "current state must hold {expected:?}, got {state_keys:?}"
        );
    }
    let joined = keys(local.joined_members().expect("joined members"));
    assert_eq!(
        joined,
        BTreeSet::from([
            ("m.room.member".to_owned(), ALICE.to_owned()),
            ("m.room.member".to_owned(), BOB.to_owned()),
        ])
    );

    // --- the timeline holds exactly the join; the snapshot is outliers, not history ---
    assert_eq!(
        timeline_ids(&local),
        vec![resident.join.event_id().to_owned()]
    );
    let (page, _) = local.paginate(None, Direction::Backward, 100);
    assert_eq!(ids(page), ids([&resident.join]));
    assert!(
        local.event_by_id(resident.message.event_id()).is_none(),
        "the join's prev event was not in the snapshot and must not be invented"
    );
    let create = resident
        .state
        .iter()
        .find(|e| e.header().event_type == "m.room.create")
        .expect("the snapshot has the create event");
    let held_create = local
        .event_by_id(create.event_id())
        .expect("the create event is held as an outlier");
    assert!(held_create.header().flags.is_outlier());
    assert!(
        local
            .event_visible_to(held_create, &bob)
            .expect("visibility"),
        "a joined member may read the snapshot's events"
    );

    // --- state_at(join) is exactly the snapshot plus the join ---
    let mut expected = ids(&resident.state);
    expected.insert(resident.join.event_id().to_owned());
    assert_eq!(state_at(&local, &resident.join), expected);

    // --- bob can post; the join is the only extremity, so it is the message's prev ---
    let bob_message = local
        .send_event(
            bob.clone(),
            "m.room.message".to_owned(),
            None,
            json!({"msgtype": "m.text", "body": "hello from b"}),
            None,
            4,
        )
        .expect("bob posts from b.example");
    let prevs: Vec<String> = bob_message.json()["prev_events"]
        .as_array()
        .expect("prev_events is an array")
        .iter()
        .map(|v| v.as_str().expect("an event id").to_owned())
        .collect();
    assert_eq!(prevs, vec![resident.join.event_id().to_string()]);

    // --- the resident's next event, citing bob's join, is accepted here ---
    let alice_after = resident
        .actor
        .send_event(
            alice,
            "m.room.message".to_owned(),
            None,
            json!({"msgtype": "m.text", "body": "welcome bob"}),
            None,
            5,
        )
        .expect("alice posts after the join");
    assert!(
        alice_after.json()["prev_events"]
            .as_array()
            .expect("prev_events")
            .iter()
            .any(|v| v.as_str() == Some(resident.join.event_id().as_str())),
        "the resident's extremity is bob's join"
    );
    let outcome = local
        .accept_remote_event(alice_after.clone())
        .expect("the resident's event is accepted");
    assert!(matches!(outcome, RemoteEventOutcome::Stored(_)));
    assert_eq!(
        timeline_ids(&local),
        vec![
            resident.join.event_id().to_owned(),
            bob_message.event_id().to_owned(),
            alice_after.event_id().to_owned(),
        ]
    );

    // --- a reload reproduces all of it ---
    let full_before = ids(local.full_state().expect("full state"));
    let timeline_before = timeline_ids(&local);
    let join_state_before = state_at(&local, &resident.join);
    drop(local);
    let mut reloaded = RoomActor::load(backend_b, tables_b, identity_b, &resident.room_id)
        .expect("load")
        .expect("the bootstrapped room is persisted");
    assert_eq!(ids(reloaded.full_state().expect("full state")), full_before);
    assert_eq!(timeline_ids(&reloaded), timeline_before);
    assert_eq!(state_at(&reloaded, &resident.join), join_state_before);
    assert!(
        reloaded
            .event_by_id(create.event_id())
            .expect("the outlier survives a reload")
            .header()
            .flags
            .is_outlier()
    );

    // --- and bob can leave ---
    reloaded
        .membership_action(bob.clone(), Action::Leave, bob, json!({}), 6)
        .expect("bob leaves after the reload");
    assert_eq!(
        keys(reloaded.joined_members().expect("joined members")),
        BTreeSet::from([("m.room.member".to_owned(), ALICE.to_owned())])
    );
}

/// The same response applied twice is a no-op the second time, and a room that already holds
/// the join reports it as already known rather than storing it again.
#[test]
fn applying_the_same_join_response_twice_is_a_no_op() {
    let resident = resident_with_bob_joined();
    let backend_b = MemoryBackend::new();
    let tables_b = Tables::open(&backend_b).expect("open tables");
    let mut local = RoomActor::create_from_remote_join(
        backend_b,
        tables_b,
        HomeserverIdentity::for_tests("b.example"),
        &resident.room_id,
        RoomVersionId::V11,
        resident.state.clone(),
        resident.auth_chain.clone(),
        resident.join.clone(),
    )
    .expect("bootstrap");
    let outcome = local
        .accept_remote_join_with_state(resident.state, resident.auth_chain, resident.join)
        .expect("a replay is not an error");
    assert_eq!(outcome, RemoteEventOutcome::AlreadyKnown);
    assert_eq!(timeline_ids(&local).len(), 1);
}

fn bootstrap_with(
    resident: &Resident,
    server_name: &str,
    state: Vec<Event>,
    auth_chain: Vec<Event>,
) -> Result<RoomActor<MemoryBackend>, RoomError> {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    RoomActor::create_from_remote_join(
        backend,
        tables,
        HomeserverIdentity::for_tests(server_name),
        &resident.room_id,
        RoomVersionId::V11,
        state,
        auth_chain,
        resident.join.clone(),
    )
}

/// A `send_join` response is only ever about *this* server's own user joining.
#[test]
fn a_join_by_a_user_of_another_server_is_refused() {
    let resident = resident_with_bob_joined();
    let err = bootstrap_with(
        &resident,
        "c.example",
        resident.state.clone(),
        resident.auth_chain.clone(),
    )
    .err()
    .expect("bob is not a c.example user");
    assert!(matches!(err, RoomError::Forbidden(_)), "got {err:?}");
}

/// A state snapshot without `m.room.create` is not a room's state at all.
#[test]
fn a_snapshot_without_a_create_event_is_refused() {
    let resident = resident_with_bob_joined();
    let state: Vec<Event> = resident
        .state
        .iter()
        .filter(|e| e.header().event_type != "m.room.create")
        .cloned()
        .collect();
    let auth_chain: Vec<Event> = resident
        .auth_chain
        .iter()
        .filter(|e| e.header().event_type != "m.room.create")
        .cloned()
        .collect();
    let err = bootstrap_with(&resident, "b.example", state, auth_chain)
        .err()
        .expect("no create event, no room");
    assert!(matches!(err, RoomError::InvalidEvent(_)), "got {err:?}");
}

/// The join must be authorizable from the snapshot alone: an `auth_events` entry the snapshot
/// does not carry means the resident's answer does not support its own join.
#[test]
fn a_join_whose_auth_events_leave_the_snapshot_is_refused() {
    let resident = resident_with_bob_joined();
    assert!(
        resident.join.json()["auth_events"]
            .as_array()
            .expect("auth_events")
            .iter()
            .any(|v| {
                resident.state.iter().any(|e| {
                    e.header().event_type == "m.room.join_rules"
                        && v.as_str() == Some(e.event_id().as_str())
                })
            }),
        "the join cites the join rules, so removing them from the snapshot must be noticed"
    );
    let without_join_rules = |events: &[Event]| -> Vec<Event> {
        events
            .iter()
            .filter(|e| e.header().event_type != "m.room.join_rules")
            .cloned()
            .collect()
    };
    let err = bootstrap_with(
        &resident,
        "b.example",
        without_join_rules(&resident.state),
        without_join_rules(&resident.auth_chain),
    )
    .err()
    .expect("an auth event outside the snapshot is refused");
    assert!(matches!(err, RoomError::Forbidden(_)), "got {err:?}");
}

/// The registry entry point: an unknown room is created, and the join reaches the global stream
/// with bob's membership delta -- what `hs-user`'s session hub needs to put the room in his
/// `/sync`. A second, identical response changes nothing and announces nothing.
#[tokio::test]
async fn bootstrap_from_remote_join_creates_the_room_and_announces_the_join_once() {
    let resident = resident_with_bob_joined();
    let registry = Arc::new(
        RoomRegistry::open(
            MemoryBackend::new(),
            HomeserverIdentity::for_tests("b.example"),
        )
        .expect("open registry"),
    );
    let mut updates = registry.subscribe_global();

    let handle = registry
        .bootstrap_from_remote_join(
            &resident.room_id,
            RoomVersionId::V11,
            resident.state.clone(),
            resident.auth_chain.clone(),
            resident.join.clone(),
        )
        .await
        .expect("bootstrap through the registry");
    assert_eq!(
        handle.query(|a| a.room_id().to_owned()).await,
        resident.room_id
    );
    assert_eq!(registry.resident_count().await, 1);

    let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("the join's update arrives")
        .expect("the global sender is live");
    assert_eq!(update.room_id, resident.room_id);
    assert_eq!(update.event_id, *resident.join.event_id());
    assert_eq!(update.room_pos, 1);
    assert_eq!(
        update.membership_deltas,
        vec![MembershipDelta {
            user_id: user_id!("@bob:b.example").to_owned(),
            membership: "join".to_owned(),
        }]
    );
    assert!(
        update.global_seq >= 1,
        "the update is numbered on the global stream"
    );

    let again = registry
        .bootstrap_from_remote_join(
            &resident.room_id,
            RoomVersionId::V11,
            resident.state.clone(),
            resident.auth_chain.clone(),
            resident.join.clone(),
        )
        .await
        .expect("a replay through the registry is not an error");
    assert_eq!(
        again.query(|a| a.events_after(0, 100).len()).await,
        1,
        "the join is stored once"
    );
    assert!(
        updates.try_recv().is_err(),
        "nothing new was published for the replay"
    );
    assert_eq!(registry.resident_count().await, 1);

    // The room is a real, loadable room now: a fresh registry over the same storage finds it.
    let loaded = registry
        .get_or_load(&resident.room_id)
        .await
        .expect("the room exists");
    assert!(loaded.query(|a| a.head_update().is_some()).await);
}

/// A refused join leaves nothing behind: no resident shell, and no room on disk.
#[tokio::test]
async fn a_refused_bootstrap_leaves_no_room_behind() {
    let resident = resident_with_bob_joined();
    let registry = Arc::new(
        RoomRegistry::open(
            MemoryBackend::new(),
            HomeserverIdentity::for_tests("c.example"),
        )
        .expect("open registry"),
    );
    let err = registry
        .bootstrap_from_remote_join(
            &resident.room_id,
            RoomVersionId::V11,
            resident.state.clone(),
            resident.auth_chain.clone(),
            resident.join.clone(),
        )
        .await
        .err()
        .expect("bob is not a c.example user");
    assert!(matches!(err, RoomError::Forbidden(_)), "got {err:?}");
    assert_eq!(registry.resident_count().await, 0);
    assert!(matches!(
        registry.get_or_load(&resident.room_id).await,
        Err(RoomError::RoomNotFound(_))
    ));
}
