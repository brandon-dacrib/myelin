//! `/sync` cases from Sytest's `31sync/*.pl`, `44account_data.pl` and `31sync/14read-markers.pl`
//! that this server failed until 2026-10-04 (track 05's session 16), each pinned at the level of
//! [`super::build`]. The Sytest name each one stands for is in its doc comment.

use std::sync::Arc;
use std::time::Duration;

use hs_e2e::store::E2eStore;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_room::actor::{CreateRoomRequest, RoomActorHandle};
use hs_room::membership::Action;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId, user_id};
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
    let hub = Arc::new(SessionHub::new(store, registry("sync.test"), 500));
    std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
    hub
}

fn e2e() -> Arc<dyn E2eStore> {
    Arc::new(TablesE2eStore::open(MemoryBackend::new()).unwrap())
}

fn params(since: Option<SyncToken>, filter: Value) -> SyncParams {
    SyncParams {
        since,
        full_state: false,
        timeout: Duration::from_millis(20),
        filter: serde_json::from_value::<SyncFilter>(filter).unwrap(),
        device_id: None,
    }
}

async fn sync(
    hub: &TestHub,
    who: &UserId,
    since: Option<SyncToken>,
    filter: Value,
) -> (Value, SyncToken) {
    // The hub follows the rooms off a broadcast stream: let it catch up.
    tokio::time::sleep(Duration::from_millis(20)).await;
    build(hub, &e2e(), who, params(since, filter))
        .await
        .unwrap()
}

async fn create(
    hub: &TestHub,
    creator: &OwnedUserId,
) -> (RoomActorHandle<MemoryBackend>, OwnedRoomId) {
    let handle = hub
        .rooms()
        .create_room(
            creator.clone(),
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                ..Default::default()
            },
            1,
        )
        .await
        .unwrap();
    let room_id = handle.query(|a| a.room_id().to_owned()).await;
    (handle, room_id)
}

/// `by` acts on `who`'s membership (the same user for a join or a leave).
async fn member(
    handle: &RoomActorHandle<MemoryBackend>,
    who: &OwnedUserId,
    action: Action,
    by: &OwnedUserId,
) {
    handle
        .membership(by.clone(), action, who.clone(), json!({}), 5)
        .await
        .unwrap();
}

async fn say(handle: &RoomActorHandle<MemoryBackend>, who: &OwnedUserId, body: &str) -> String {
    handle
        .send_event(
            who.clone(),
            "m.room.message".to_owned(),
            None,
            json!({"body": body, "msgtype": "m.text"}),
            None,
            10,
        )
        .await
        .unwrap()
        .event_id()
        .to_string()
}

async fn state(
    handle: &RoomActorHandle<MemoryBackend>,
    who: &OwnedUserId,
    event_type: &str,
    state_key: &str,
    content: Value,
) {
    handle
        .send_event(
            who.clone(),
            event_type.to_owned(),
            Some(state_key.to_owned()),
            content,
            None,
            10,
        )
        .await
        .unwrap();
}

fn room<'a>(response: &'a Value, section: &str, room_id: &RoomId) -> &'a Value {
    &response["rooms"][section][room_id.as_str()]
}

fn events(section: &Value) -> Vec<Value> {
    section["events"].as_array().cloned().unwrap_or_default()
}

/// "Full state sync includes joined rooms": `full_state=true` with a token sends every joined
/// room, though nothing happened in it.
#[tokio::test]
async fn a_full_state_sync_with_a_token_carries_every_joined_room() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (_handle, room_id) = create(&hub, &alice).await;
    let (_, token) = sync(&hub, &alice, None, json!({})).await;

    let mut params = params(Some(token), json!({}));
    params.full_state = true;
    let (response, _) = build(&hub, &e2e(), &alice, params).await.unwrap();
    let entry = room(&response, "join", &room_id);
    assert!(!events(&entry["state"]).is_empty(), "{response}");
}

/// "Newly joined room has correct timeline in incremental sync": a room joined since the token
/// is `limited`, as Synapse marks a newly joined room.
#[tokio::test]
async fn a_newly_joined_room_is_limited() {
    let hub = hub();
    let (alice, bob) = (
        user_id!("@alice:sync.test").to_owned(),
        user_id!("@bob:sync.test").to_owned(),
    );
    let (handle, room_id) = create(&hub, &alice).await;
    for n in 0..4 {
        say(&handle, &alice, &format!("before {n}")).await;
    }
    let filter = json!({"room": {"timeline": {"types": ["m.room.message"], "limit": 10}}});
    let (_, token) = sync(&hub, &bob, None, filter.clone()).await;
    member(&handle, &bob, Action::Join, &bob).await;
    let (response, _) = sync(&hub, &bob, Some(token), filter).await;
    let timeline = &room(&response, "join", &room_id)["timeline"];
    assert_eq!(events(timeline).len(), 4, "{response}");
    assert_eq!(timeline["limited"], json!(true), "{response}");
}

/// "Newly joined room includes presence in incremental sync" and "Get presence for newly
/// joined members in incremental sync": whoever comes into view has presence in the batch,
/// `offline` when this server has none for them.
#[tokio::test]
async fn presence_comes_with_a_join_from_either_side_offline_when_unknown() {
    let hub = hub();
    let (alice, bob) = (
        user_id!("@alice:sync.test").to_owned(),
        user_id!("@bob:sync.test").to_owned(),
    );
    let (handle, _room_id) = create(&hub, &alice).await;
    let (_, alice_token) = sync(&hub, &alice, None, json!({})).await;
    let (_, bob_token) = sync(&hub, &bob, None, json!({})).await;
    member(&handle, &bob, Action::Join, &bob).await;

    let senders = |response: &Value| -> Vec<(String, String)> {
        events(&response["presence"])
            .iter()
            .map(|e| {
                (
                    e["sender"].as_str().unwrap().to_owned(),
                    e["content"]["presence"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let (response, next) = sync(&hub, &bob, Some(bob_token), json!({})).await;
    assert_eq!(
        senders(&response),
        vec![("@alice:sync.test".to_owned(), "offline".to_owned())],
        "{response}"
    );
    let (response, _) = sync(&hub, &bob, Some(next), json!({})).await;
    assert!(senders(&response).is_empty(), "news once: {response}");

    let (response, _) = sync(&hub, &alice, Some(alice_token), json!({})).await;
    assert_eq!(
        senders(&response),
        vec![("@bob:sync.test".to_owned(), "offline".to_owned())],
        "{response}"
    );
}

/// "A prev_batch token from incremental sync can be used in the v1 messages API": paging back
/// from `prev_batch` starts with the event just before the batch.
#[tokio::test]
async fn an_incremental_prev_batch_pages_back_to_the_event_before_the_batch() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    let first = say(&handle, &alice, "1").await;
    let (_, token) = sync(&hub, &alice, None, json!({})).await;
    say(&handle, &alice, "2").await;
    let (response, _) = sync(&hub, &alice, Some(token), json!({})).await;
    let timeline = &room(&response, "join", &room_id)["timeline"];
    assert_eq!(events(timeline).len(), 1, "{response}");
    let prev_batch: hs_room::timeline::PaginationToken =
        timeline["prev_batch"].as_str().unwrap().parse().unwrap();
    let before = handle
        .query(move |actor| {
            let (page, _) =
                actor.paginate(Some(prev_batch), hs_room::timeline::Direction::Backward, 1);
            page.first().map(|e| e.event_id().to_string())
        })
        .await;
    assert_eq!(before, Some(first));
}

/// "Changes to state are included in an gapped incremental sync": after a gap, `state` is
/// what changed inside it, not the whole state again.
#[tokio::test]
async fn a_gapped_sync_sends_only_the_state_that_changed_in_the_gap() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    state(
        &handle,
        &alice,
        "a.madeup.test.state",
        "this_state_changes",
        json!({"my_key": 1}),
    )
    .await;
    state(
        &handle,
        &alice,
        "a.madeup.test.state",
        "this_state_does_not_change",
        json!({"my_key": 1}),
    )
    .await;
    let filter = json!({"room": {
        "timeline": {"types": ["a.made.up.filler.type"], "limit": 1},
        "state": {"types": ["a.madeup.test.state"]},
    }});
    let (response, token) = sync(&hub, &alice, None, filter.clone()).await;
    assert_eq!(events(&room(&response, "join", &room_id)["state"]).len(), 2);

    state(
        &handle,
        &alice,
        "a.madeup.test.state",
        "this_state_changes",
        json!({"my_key": 2}),
    )
    .await;
    for n in 0..20 {
        handle
            .send_event(
                alice.clone(),
                "a.made.up.filler.type".to_owned(),
                None,
                json!({"filler": n}),
                None,
                20,
            )
            .await
            .unwrap();
    }
    let (response, _) = sync(&hub, &alice, Some(token), filter).await;
    let entry = room(&response, "join", &room_id);
    assert_eq!(entry["timeline"]["limited"], json!(true), "{response}");
    let state = events(&entry["state"]);
    assert_eq!(state.len(), 1, "{response}");
    assert_eq!(state[0]["content"]["my_key"], json!(2));
}

/// "When user joins and leaves a room in the same batch, the full state is still included in
/// the next sync": invited at the token, joined and left since, the room is in `leave` whole.
#[tokio::test]
async fn joining_and_leaving_within_a_batch_sends_the_left_room_whole() {
    let hub = hub();
    let (alice, bob) = (
        user_id!("@alice:sync.test").to_owned(),
        user_id!("@bob:sync.test").to_owned(),
    );
    let (handle, room_id) = create(&hub, &alice).await;
    state(
        &handle,
        &alice,
        "a.madeup.test.state",
        "",
        json!({"my_key": 1}),
    )
    .await;
    member(&handle, &bob, Action::Invite, &alice).await;
    let filter = json!({"room": {
        "timeline": {"types": []},
        "state": {"types": ["a.madeup.test.state"]},
        "include_leave": true,
    }});
    let (_, token) = sync(&hub, &bob, None, filter.clone()).await;
    member(&handle, &bob, Action::Join, &bob).await;
    member(&handle, &bob, Action::Leave, &bob).await;
    let (response, _) = sync(&hub, &bob, Some(token), filter).await;
    let state = events(&room(&response, "leave", &room_id)["state"]);
    assert_eq!(state.len(), 1, "{response}");
    assert_eq!(state[0]["content"]["my_key"], json!(1));
}

/// "Current state appears in timeline in private history": somebody out of a
/// `joined`-visibility room while another member joined still sees that join, which is the
/// room's current state, when they come back.
#[tokio::test]
async fn current_state_is_in_the_timeline_whatever_the_history_visibility() {
    let hub = hub();
    let (creator, syncer, invitee) = (
        user_id!("@creator:sync.test").to_owned(),
        user_id!("@syncer:sync.test").to_owned(),
        user_id!("@invitee:sync.test").to_owned(),
    );
    let (handle, room_id) = create(&hub, &creator).await;
    member(&handle, &syncer, Action::Join, &syncer).await;
    state(
        &handle,
        &creator,
        "m.room.history_visibility",
        "",
        json!({"history_visibility": "joined"}),
    )
    .await;
    member(&handle, &invitee, Action::Invite, &creator).await;
    let (_, token) = sync(&hub, &syncer, None, json!({})).await;
    member(&handle, &syncer, Action::Leave, &syncer).await;
    member(&handle, &invitee, Action::Join, &invitee).await;
    for n in 0..30 {
        say(&handle, &creator, &format!("while away {n}")).await;
    }
    member(&handle, &syncer, Action::Join, &syncer).await;
    let (response, _) = sync(&hub, &syncer, Some(token), json!({})).await;
    let timeline = events(&room(&response, "join", &room_id)["timeline"]);
    assert!(
        timeline.iter().any(|e| e["type"] == "m.room.member"
            && e["state_key"] == invitee.as_str()
            && e["content"]["membership"] == "join"),
        "{response}"
    );
    assert!(
        !timeline.iter().any(|e| e["type"] == "m.room.message"),
        "what was said while they were away stays hidden: {response}"
    );
}

/// "Read markers appear in incremental v2 /sync": a change of room account data alone brings
/// the room into the batch.
#[tokio::test]
async fn room_account_data_alone_brings_the_room_into_an_incremental_sync() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    let event_id = say(&handle, &alice, "hello").await;
    let (_, token) = sync(&hub, &alice, None, json!({})).await;
    hub.store()
        .put_room_account_data(
            &alice,
            &room_id,
            "m.fully_read",
            json!({"event_id": event_id}),
        )
        .await
        .unwrap();
    let (response, next) = sync(&hub, &alice, Some(token), json!({})).await;
    let account_data = events(&room(&response, "join", &room_id)["account_data"]);
    assert_eq!(account_data.len(), 1, "{response}");
    assert_eq!(account_data[0]["type"], "m.fully_read");
    let (response, _) = sync(&hub, &alice, Some(next), json!({})).await;
    assert!(room(&response, "join", &room_id).is_null(), "{response}");
}

/// "Latest account data appears in v2 /sync": the account-data filters apply, globally and per
/// room.
#[tokio::test]
async fn account_data_filters_apply_to_global_and_room_account_data() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (_handle, room_id) = create(&hub, &alice).await;
    let store = hub.store();
    store
        .put_global_account_data(&alice, "my.test.type", json!({"cats_or_rats": "cats"}))
        .await
        .unwrap();
    store
        .put_global_account_data(&alice, "my.other.type", json!({}))
        .await
        .unwrap();
    store
        .put_room_account_data(
            &alice,
            &room_id,
            "my.test.type",
            json!({"cats_or_rats": "rats"}),
        )
        .await
        .unwrap();
    store
        .put_room_account_data(&alice, &room_id, "my.other.type", json!({}))
        .await
        .unwrap();
    let (response, _) = sync(
        &hub,
        &alice,
        None,
        json!({"account_data": {"types": ["my.test.type"]},
               "room": {"account_data": {"types": ["my.test.type"]}}}),
    )
    .await;
    let global = events(&response["account_data"]);
    assert_eq!(global.len(), 1, "{response}");
    assert_eq!(global[0]["content"]["cats_or_rats"], "cats");
    let per_room = events(&room(&response, "join", &room_id)["account_data"]);
    assert_eq!(per_room.len(), 1, "{response}");
    assert_eq!(per_room[0]["content"]["cats_or_rats"], "rats");
}

/// "Can request federation format via the filter".
#[tokio::test]
async fn the_federation_format_carries_what_servers_exchange() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    let event_id = say(&handle, &alice, "Test message").await;
    let (response, _) = sync(
        &hub,
        &alice,
        None,
        json!({"event_format": "federation", "room": {"timeline": {"limit": 1}}}),
    )
    .await;
    let timeline = events(&room(&response, "join", &room_id)["timeline"]);
    assert_eq!(timeline.len(), 1, "{response}");
    let event = &timeline[0];
    for key in [
        "event_id",
        "content",
        "room_id",
        "sender",
        "origin_server_ts",
        "type",
        "prev_events",
        "auth_events",
        "depth",
        "hashes",
        "signatures",
    ] {
        assert!(event.get(key).is_some(), "{key} missing: {event}");
    }
    assert_eq!(event["event_id"], event_id.as_str());
    assert_eq!(event["content"]["body"], "Test message");
}

/// The presence filter applies: `presence: {types: []}` sends none.
#[tokio::test]
async fn the_presence_filter_applies() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    hub.set_presence(&alice, "online".to_owned(), None)
        .await
        .unwrap();
    let (response, _) = sync(&hub, &alice, None, json!({"presence": {"types": []}})).await;
    assert!(events(&response["presence"]).is_empty(), "{response}");
    let (response, _) = sync(&hub, &alice, None, json!({})).await;
    assert_eq!(events(&response["presence"]).len(), 1, "{response}");
}

/// Complement's `TestRoomForget`, "Forgetting room does not show up in v2 initial /sync": a
/// forgotten room is not offered again, `include_leave` or not.
#[tokio::test]
async fn a_forgotten_room_is_not_in_an_initial_sync_with_include_leave() {
    let hub = hub();
    let (alice, bob) = (
        user_id!("@alice:sync.test").to_owned(),
        user_id!("@bob:sync.test").to_owned(),
    );
    let (handle, room_id) = create(&hub, &alice).await;
    member(&handle, &bob, Action::Join, &bob).await;
    member(&handle, &alice, Action::Leave, &alice).await;
    let include_leave = json!({"room": {"include_leave": true}});
    let (response, _) = sync(&hub, &alice, None, include_leave.clone()).await;
    assert!(!room(&response, "leave", &room_id).is_null(), "{response}");
    handle.forget(alice.clone()).await.unwrap();
    let (response, _) = sync(&hub, &alice, None, include_leave).await;
    assert!(room(&response, "leave", &room_id).is_null(), "{response}");
}

/// Records what the hub hands its EDU outbox.
#[derive(Default)]
struct RecordingOutbox(std::sync::Mutex<Vec<(std::collections::BTreeSet<String>, String, Value)>>);

impl crate::edu::EduOutbox for RecordingOutbox {
    fn send_edu(
        &self,
        destinations: std::collections::BTreeSet<String>,
        edu_type: &str,
        content: Value,
        _coalesce_key: Option<String>,
    ) {
        self.0
            .lock()
            .unwrap()
            .push((destinations, edu_type.to_owned(), content));
    }
}

/// "New federated private chats get full presence information (SYN-115)": a remote user's
/// join sends their server this server's members' presence, and a local user's join sends
/// theirs to the other servers in the room.
#[tokio::test]
async fn a_join_shares_presence_across_the_servers_in_the_room() {
    let hub = hub();
    let outbox = Arc::new(RecordingOutbox::default());
    hub.install_edu_outbox(outbox.clone());
    let alice = user_id!("@alice:sync.test").to_owned();
    let carol = user_id!("@carol:sync.test").to_owned();
    let bob = user_id!("@bob:remote.test").to_owned();
    hub.set_presence(&alice, "online".to_owned(), None)
        .await
        .unwrap();
    hub.set_presence(&carol, "unavailable".to_owned(), None)
        .await
        .unwrap();
    let (handle, _room_id) = create(&hub, &alice).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    outbox.0.lock().unwrap().clear();

    member(&handle, &bob, Action::Join, &bob).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let sent = std::mem::take(&mut *outbox.0.lock().unwrap());
    let to_remote: Vec<&Value> = sent
        .iter()
        .filter(|(to, kind, _)| kind == "m.presence" && to.contains("remote.test"))
        .map(|(_, _, content)| content)
        .collect();
    assert_eq!(to_remote.len(), 1, "{sent:?}");
    let pushed: Vec<&str> = to_remote[0]["push"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["user_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        pushed,
        vec!["@alice:sync.test"],
        "only the room's local members: {sent:?}"
    );

    member(&handle, &carol, Action::Join, &carol).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let sent = std::mem::take(&mut *outbox.0.lock().unwrap());
    assert!(
        sent.iter().any(|(to, kind, content)| kind == "m.presence"
            && to.contains("remote.test")
            && content["push"][0]["user_id"] == "@carol:sync.test"),
        "{sent:?}"
    );
}

/// Sytest's "User in remote room doesn't appear in user directory after server left room", the
/// room layer's half: another server's user sharing a room is offered with their room profile.
#[tokio::test]
async fn another_servers_member_is_offered_to_the_directory_with_their_room_profile() {
    use hs_auth::state::UserDirectoryVisibility;
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let zed = user_id!("@zed:remote.test").to_owned();
    let (handle, _room_id) = create(&hub, &alice).await;
    handle
        .membership(
            zed.clone(),
            Action::Join,
            zed.clone(),
            json!({"displayname": "Zed Faraway"}),
            5,
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let visible = hub.visible_to(&alice).await.unwrap();
    assert!(visible.contains(&zed), "{visible:?}");
    let profiles = hub
        .remote_profiles(&std::collections::BTreeSet::from([zed.clone()]))
        .await
        .unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].display_name.as_deref(), Some("Zed Faraway"));
}

/// Complement's `TestDeviceListsUpdateOverFederation`: whoever joins a room is in their own
/// `device_lists.changed` with the room's other members, as Synapse counts every member of a
/// newly joined room; they are never in `left`.
#[tokio::test]
async fn the_joiner_is_in_their_own_device_lists_changed() {
    let hub = hub();
    let (alice, bob) = (
        user_id!("@alice:sync.test").to_owned(),
        user_id!("@bob:sync.test").to_owned(),
    );
    let (handle, _room_id) = create(&hub, &alice).await;
    let (_, token) = sync(&hub, &bob, None, json!({})).await;
    member(&handle, &bob, Action::Join, &bob).await;
    let (response, _) = sync(&hub, &bob, Some(token), json!({})).await;
    let changed: Vec<&str> = response["device_lists"]["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap())
        .collect();
    assert!(changed.contains(&"@bob:sync.test"), "{response}");
    assert!(changed.contains(&"@alice:sync.test"), "{response}");
    assert!(
        response["device_lists"]["left"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{response}"
    );
}

/// Complement's `TestMembershipOnEvents` (MSC4115): each timeline event carries the reader's
/// membership at it in `unsigned.membership`.
#[tokio::test]
async fn timeline_events_carry_the_readers_membership_at_them() {
    let hub = hub();
    let (alice, bob) = (
        user_id!("@alice:sync.test").to_owned(),
        user_id!("@bob:sync.test").to_owned(),
    );
    let (handle, room_id) = create(&hub, &alice).await;
    say(&handle, &alice, "before bob").await;
    member(&handle, &bob, Action::Join, &bob).await;
    say(&handle, &alice, "with bob").await;
    let (response, _) = sync(&hub, &bob, None, json!({})).await;
    let timeline = events(&room(&response, "join", &room_id)["timeline"]);
    let membership_of = |body: &str| {
        timeline
            .iter()
            .find(|e| e["content"]["body"] == body)
            .map(|e| e["unsigned"]["membership"].clone())
    };
    assert_eq!(
        membership_of("before bob"),
        Some(json!("leave")),
        "{response}"
    );
    assert_eq!(membership_of("with bob"), Some(json!("join")), "{response}");
}

/// Sytest's "Only original members of the room can see messages from erased users": an erased
/// local sender's message is pruned for somebody who joined after it, and kept for somebody who
/// was there.
#[tokio::test]
async fn an_erased_senders_messages_are_pruned_for_later_members_only() {
    let hub = hub();
    let accounts: Arc<dyn hs_auth::store::AuthStore> =
        Arc::new(hs_auth::store::memory::InMemoryAuthStore::new());
    hub.install_account_store(accounts.clone());
    let (alice, bob, carol) = (
        user_id!("@alice:sync.test").to_owned(),
        user_id!("@bob:sync.test").to_owned(),
        user_id!("@carol:sync.test").to_owned(),
    );
    let (handle, room_id) = create(&hub, &alice).await;
    member(&handle, &bob, Action::Join, &bob).await;
    say(&handle, &alice, "said before carol").await;
    member(&handle, &carol, Action::Join, &carol).await;
    let mut record = hs_auth::store::UserRecord::new(alice.clone(), 0);
    record.erased = true;
    accounts.create_user(record).await.unwrap();

    let body_seen_by = |response: &Value| -> Option<Value> {
        events(&room(response, "join", &room_id)["timeline"])
            .iter()
            .find(|e| e["type"] == "m.room.message")
            .map(|e| e["content"].clone())
    };
    let (response, _) = sync(&hub, &carol, None, json!({})).await;
    assert_eq!(body_seen_by(&response), Some(json!({})), "{response}");
    let (response, _) = sync(&hub, &bob, None, json!({})).await;
    assert_eq!(
        body_seen_by(&response).map(|c| c["body"].clone()),
        Some(json!("said before carol")),
        "{response}"
    );
}

/// Complement's `TestGetRoomMembersAtPoint`: a fresh timeline that reaches the room's first
/// event still hands out a `prev_batch`, at that event.
#[tokio::test]
async fn a_timeline_reaching_the_first_event_still_has_a_prev_batch() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (_handle, room_id) = create(&hub, &alice).await;
    let (response, _) = sync(&hub, &alice, None, json!({})).await;
    let timeline = &room(&response, "join", &room_id)["timeline"];
    assert_eq!(events(timeline)[0]["type"], "m.room.create", "{response}");
    assert!(timeline["prev_batch"].is_string(), "{response}");
}
