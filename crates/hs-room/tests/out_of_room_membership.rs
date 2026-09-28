//! Membership in a room this server is not in: `RoomActor::accept_out_of_room_membership` and
//! `RoomRegistry::accept_out_of_room_membership` -- an invite from another server, the leave
//! that ends it, and the join that later brings the whole room over the top of it -- plus
//! `RoomActor::build_membership_event`, the unpersisted invite a resident sends out to be
//! co-signed.
//!
//! As in `remote_join.rs`, two backends stand in for two homeservers and no signature is
//! checked here: `hs-room` trusts its caller (`hs_federation`) to have verified every event.

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
use ruma::{OwnedUserId, RoomVersionId, user_id};
use serde_json::json;

fn alice() -> OwnedUserId {
    user_id!("@alice:a.example").to_owned()
}

fn bob() -> OwnedUserId {
    user_id!("@bob:b.example").to_owned()
}

/// `a.example`'s private room, with bob invited.
fn resident_with_bob_invited() -> (RoomActor<MemoryBackend>, Event) {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let mut actor = RoomActor::create_room(
        backend,
        tables,
        HomeserverIdentity::for_tests("a.example"),
        alice(),
        CreateRoomRequest {
            preset: Some("private_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            name: Some("the room".to_owned()),
            ..Default::default()
        },
        1,
    )
    .expect("create the resident room");
    let invite = actor
        .membership_action(alice(), Action::Invite, bob(), json!({}), 2)
        .expect("alice invites bob");
    (actor, invite)
}

fn registry(server: &str) -> Arc<RoomRegistry<MemoryBackend>> {
    Arc::new(
        RoomRegistry::open(MemoryBackend::new(), HomeserverIdentity::for_tests(server))
            .expect("open registry"),
    )
}

fn membership_of<B: hs_kv::KvBackend>(actor: &RoomActor<B>, user: &str) -> Option<String> {
    let event = actor.state_event("m.room.member", user).ok()??;
    let content: serde_json::Value =
        serde_json::from_slice(&event.json().get("content")?.to_canonical_bytes()).ok()?;
    content["membership"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn an_invite_makes_a_room_whose_only_state_is_the_invitees_membership() {
    let (resident, invite) = resident_with_bob_invited();
    let room_id = resident.room_id().to_owned();
    let b = registry("b.example");
    let mut updates = b.subscribe_global();

    let handle = b
        .accept_out_of_room_membership(&room_id, RoomVersionId::V11, invite.clone())
        .await
        .expect("the invite is recorded");
    let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("the invite's update arrives")
        .expect("the global sender is live");
    assert_eq!(update.event_id, *invite.event_id());
    assert_eq!(
        update.membership_deltas,
        vec![MembershipDelta {
            user_id: bob(),
            membership: "invite".to_owned(),
        }]
    );

    let (state, membership, through) = handle
        .query(|actor| {
            (
                actor.full_state().map(|s| s.len()).unwrap_or(0),
                membership_of(actor, "@bob:b.example"),
                actor.servers_to_join_through(),
            )
        })
        .await;
    assert_eq!(state, 1, "nothing but bob's own membership is held");
    assert_eq!(membership.as_deref(), Some("invite"));
    assert_eq!(
        through,
        Some(vec!["a.example".to_owned()]),
        "a join goes through the room's server, which is also the inviter's"
    );

    // Replaying it changes nothing and announces nothing.
    let replay = handle
        .accept_out_of_room_membership(invite.clone())
        .await
        .expect("a replay is not an error");
    assert!(matches!(replay, RemoteEventOutcome::AlreadyKnown));
    assert!(updates.try_recv().is_err());

    // The room is durable: a fresh registry over the same storage would load it; this one
    // reloads it after eviction.
    b.evict_idle(Duration::ZERO).await;
    let reloaded = b.get_or_load(&room_id).await.expect("the room is on disk");
    assert_eq!(
        reloaded
            .query(|actor| membership_of(actor, "@bob:b.example"))
            .await
            .as_deref(),
        Some("invite")
    );
}

#[tokio::test]
async fn the_leave_that_ends_an_invite_and_the_join_that_accepts_one_apply_over_it() {
    let (mut resident, invite) = resident_with_bob_invited();
    let room_id = resident.room_id().to_owned();
    let b = registry("b.example");
    b.accept_out_of_room_membership(&room_id, RoomVersionId::V11, invite.clone())
        .await
        .expect("the invite is recorded");

    // Alice rescinds it; the leave is recorded over the invite.
    let rescind = resident
        .membership_action(alice(), Action::Kick, bob(), json!({}), 3)
        .expect("alice rescinds the invite");
    let handle = b
        .accept_out_of_room_membership(&room_id, RoomVersionId::V11, rescind)
        .await
        .expect("the leave is recorded");
    assert_eq!(
        handle
            .query(|actor| membership_of(actor, "@bob:b.example"))
            .await
            .as_deref(),
        Some("leave")
    );

    // She invites him again and he joins through her server: the whole room arrives over the
    // top of what B held.
    let again = resident
        .membership_action(alice(), Action::Invite, bob(), json!({}), 4)
        .expect("alice invites bob again");
    b.accept_out_of_room_membership(&room_id, RoomVersionId::V11, again.clone())
        .await
        .expect("the second invite is recorded");
    let StateAtEvent { state, auth_chain } = resident
        .state_at_event(again.event_id())
        .expect("state lookup")
        .expect("the invite is known");
    let join = resident
        .membership_action(bob(), Action::Join, bob(), json!({}), 5)
        .expect("bob joins");
    b.bootstrap_from_remote_join(&room_id, RoomVersionId::V11, state, auth_chain, join)
        .await
        .expect("the join applies over the invite");
    let (membership, name, through) = handle
        .query(|actor| {
            (
                membership_of(actor, "@bob:b.example"),
                actor
                    .state_event("m.room.name", "")
                    .ok()
                    .flatten()
                    .is_some(),
                actor.servers_to_join_through(),
            )
        })
        .await;
    assert_eq!(membership.as_deref(), Some("join"));
    assert!(name, "the room's state came with the join");
    assert_eq!(through, None, "bob is in the room now");

    // With bob in, nothing more is recorded out of band: the room's events come through it.
    let carol_invite = resident
        .membership_action(
            alice(),
            Action::Invite,
            user_id!("@carol:b.example").to_owned(),
            json!({}),
            6,
        )
        .expect("alice invites carol");
    let err = handle
        .accept_out_of_room_membership(carol_invite)
        .await
        .expect_err("a server in the room takes its events through the room");
    assert!(matches!(err, RoomError::Forbidden(_)), "{err:?}");
}

#[tokio::test]
async fn a_join_or_somebody_elses_invite_is_not_recorded_out_of_band() {
    let (mut resident, invite) = resident_with_bob_invited();
    let room_id = resident.room_id().to_owned();

    // Somebody else's user.
    let c = registry("c.example");
    let err = c
        .accept_out_of_room_membership(&room_id, RoomVersionId::V11, invite.clone())
        .await
        .err()
        .expect("bob is not a c.example user");
    assert!(matches!(err, RoomError::Forbidden(_)), "{err:?}");
    assert_eq!(c.resident_count().await, 0, "no room is left behind");

    // A join needs the room's state; it is never out of band.
    let join = resident
        .membership_action(bob(), Action::Join, bob(), json!({}), 3)
        .expect("bob joins on the resident");
    let b = registry("b.example");
    let err = b
        .accept_out_of_room_membership(&room_id, RoomVersionId::V11, join)
        .await
        .err()
        .expect("a join is refused");
    assert!(matches!(err, RoomError::InvalidEvent(_)), "{err:?}");
    assert!(matches!(
        b.get_or_load(&room_id).await,
        Err(RoomError::RoomNotFound(_))
    ));
}

/// The first half of inviting somebody on another server: built, signed and authorized, but
/// not in the room until it comes back and is accepted.
#[test]
fn a_built_membership_event_is_not_in_the_room_until_accepted() {
    let (mut resident, _) = resident_with_bob_invited();
    let carol = user_id!("@carol:c.example").to_owned();
    let before = resident.events_after(0, 100).len();
    let built = resident
        .build_membership_event(alice(), Action::Invite, carol.clone(), json!({}), 3)
        .expect("the invite builds");
    assert_eq!(resident.events_after(0, 100).len(), before);
    assert!(membership_of(&resident, carol.as_str()).is_none());

    let outcome = resident
        .accept_remote_event(built.clone())
        .expect("the built invite is accepted");
    assert!(matches!(outcome, RemoteEventOutcome::Stored(_)));
    assert_eq!(
        membership_of(&resident, carol.as_str()).as_deref(),
        Some("invite")
    );

    // Built again unchanged, it is the event already in the room.
    let rebuilt = resident
        .build_membership_event(alice(), Action::Invite, carol, json!({}), 4)
        .expect("builds");
    assert_eq!(rebuilt.event_id(), built.event_id());

    // The stripped state describes the room, and names whoever the caller asks about.
    let stripped = resident
        .stripped_state(&["@alice:a.example"])
        .expect("stripped state");
    let has = |t: &str, k: &str| {
        stripped
            .iter()
            .any(|e| e["type"] == t && e["state_key"] == k)
    };
    assert!(has("m.room.create", ""));
    assert!(has("m.room.name", ""));
    assert!(has("m.room.join_rules", ""));
    assert!(has("m.room.member", "@alice:a.example"));
    assert!(!has("m.room.power_levels", ""));
    assert!(!has("m.room.member", "@bob:b.example"));
    assert!(stripped.iter().all(|e| e.get("event_id").is_none()));
}
