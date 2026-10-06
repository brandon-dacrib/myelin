//! A membership event the room did not take up does not move the user's membership
//! (`crate::hub`'s `drop_memberships_that_lost`): Complement's `TestUnbanViaInvite`, where the
//! unban reaches the invitee's server after the invite that follows it.
//!
//! Two servers: the room lives on `b.example` (a bare [`RoomActor`], as `hs-room`'s own
//! out-of-room tests stand one up) and this hub is `a.example`'s, whose user alice joins, is
//! banned, and is invited back. Events reach `a.example`'s registry through the calls the
//! federation sinks make (`hs_cli::federation`); nothing is signed or checked.

use std::sync::Arc;
use std::time::Duration;

use hs_e2e::store::E2eStore;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_room::actor::{CreateRoomRequest, RoomActor};
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use ruma::{OwnedUserId, RoomVersionId, user_id};
use serde_json::{Value, json};

use super::{SyncParams, build};
use crate::filter::SyncFilter;
use crate::hub::SessionHub;
use crate::room_source::test_support::registry;
use crate::store::DynUserStore;
use crate::store::tables::TablesUserStore;
use crate::token::SyncToken;

type TestHub = SessionHub<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>;

fn hub() -> Arc<TestHub> {
    let store: DynUserStore = Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
    let hub = Arc::new(SessionHub::new(store, registry("a.example"), 500));
    std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
    hub
}

fn alice() -> OwnedUserId {
    user_id!("@alice:a.example").to_owned()
}

fn bob() -> OwnedUserId {
    user_id!("@bob:b.example").to_owned()
}

async fn sync(hub: &TestHub, since: Option<SyncToken>) -> (Value, SyncToken) {
    // The hub follows the rooms off a broadcast stream: let it catch up.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let e2e: Arc<dyn E2eStore> = Arc::new(TablesE2eStore::open(MemoryBackend::new()).unwrap());
    build(
        hub,
        &e2e,
        &alice(),
        SyncParams {
            since,
            full_state: false,
            timeout: Duration::ZERO,
            filter: SyncFilter::none(),
            device_id: None,
        },
    )
    .await
    .unwrap()
}

/// `event`, received over `/send`, applied as `RegistryWriteSink` applies it.
async fn receive(hub: &TestHub, event: Event) {
    let room_id = ruma::RoomId::parse(
        event
            .json()
            .get("room_id")
            .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
            .unwrap(),
    )
    .unwrap();
    hub.rooms()
        .get_or_load(&room_id)
        .await
        .unwrap()
        .accept_remote_event(event)
        .await
        .expect("the event is accepted");
}

/// Bob's public room on `b.example`, with alice joined through `a.example` and then banned.
async fn alice_banned(hub: &TestHub) -> RoomActor<MemoryBackend> {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).unwrap();
    let mut resident = RoomActor::create_room(
        backend,
        tables,
        HomeserverIdentity::for_tests("b.example"),
        bob(),
        CreateRoomRequest {
            preset: Some("public_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            ..Default::default()
        },
        1,
    )
    .unwrap();
    let room_id = resident.room_id().to_owned();
    let join = resident
        .membership_action(alice(), Action::Join, alice(), json!({}), 2)
        .unwrap();
    let at_join = resident.state_at_event(join.event_id()).unwrap().unwrap();
    hub.rooms()
        .bootstrap_from_remote_join(
            &room_id,
            RoomVersionId::V11,
            at_join.state,
            at_join.auth_chain,
            join,
        )
        .await
        .unwrap();
    let ban = resident
        .membership_action(bob(), Action::Ban, alice(), json!({}), 3)
        .unwrap();
    receive(hub, ban).await;
    resident
}

#[tokio::test]
async fn an_invite_survives_the_unban_before_it_arriving_after_it() {
    let hub = hub();
    let mut resident = alice_banned(&hub).await;
    let room_id = resident.room_id().to_string();
    let (_, before) = sync(&hub, None).await;

    let unban = resident
        .membership_action(bob(), Action::Unban, alice(), json!({}), 4)
        .unwrap();
    let invite = resident
        .membership_action(bob(), Action::Invite, alice(), json!({}), 5)
        .unwrap();
    // `PUT /invite` first: recorded out of band, nobody of `a.example` being in the room.
    hub.rooms()
        .accept_out_of_room_membership(resident.room_id(), RoomVersionId::V11, invite)
        .await
        .unwrap();
    // Then the unban, over `/send`: it resolves away under the invite.
    receive(&hub, unban).await;

    let (initial, _) = sync(&hub, None).await;
    assert!(
        initial["rooms"]["invite"].get(&room_id).is_some(),
        "a fresh sync does not show the invite: {initial}"
    );
    let (incremental, _) = sync(&hub, Some(before)).await;
    assert!(
        incremental["rooms"]["invite"].get(&room_id).is_some(),
        "an incremental sync does not show the invite: {incremental}"
    );
}

/// The usual order, for contrast: the unban arrives first and moves alice to `leave`, and the
/// invite after it to `invite`.
#[tokio::test]
async fn an_unban_then_an_invite_arriving_in_order_end_in_the_invite() {
    let hub = hub();
    let mut resident = alice_banned(&hub).await;
    let room_id = resident.room_id().to_string();
    let unban = resident
        .membership_action(bob(), Action::Unban, alice(), json!({}), 4)
        .unwrap();
    let invite = resident
        .membership_action(bob(), Action::Invite, alice(), json!({}), 5)
        .unwrap();
    receive(&hub, unban).await;
    let room = ruma::RoomId::parse(&room_id).unwrap();
    let membership = async |hub: &TestHub| {
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub.store()
            .get_membership(&alice(), &room)
            .await
            .unwrap()
            .map(|m| m.membership)
    };
    assert_eq!(membership(&hub).await.as_deref(), Some("leave"));
    hub.rooms()
        .accept_out_of_room_membership(resident.room_id(), RoomVersionId::V11, invite)
        .await
        .unwrap();
    assert_eq!(membership(&hub).await.as_deref(), Some("invite"));
}
