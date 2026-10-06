//! Long-polled `/sync` and a fresh batch's `prev_batch` (track 05's session 17): Sytest's
//! `31sync/08polling.pl`, `10apidoc/34room-messages.pl`, and Complement's
//! `TestGetRoomMembersAtPoint`, each pinned at the level of [`super::build`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use hs_e2e::store::E2eStore;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_room::actor::{CreateRoomRequest, RoomActorHandle};
use hs_room::membership::Action;
use hs_room::timeline::{Direction, PaginationToken};
use ruma::{OwnedRoomId, OwnedUserId, UserId, user_id};
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

/// Sytest's `08polling.pl` filter: everything but presence.
fn no_presence() -> Value {
    json!({"presence": {"not_types": ["m.presence"]}})
}

/// What `routes::sync` does before building: marks the caller online (no `set_presence`).
async fn sync(
    hub: &TestHub,
    who: &UserId,
    since: Option<SyncToken>,
    filter: Value,
    timeout: Duration,
) -> (Value, SyncToken) {
    hub.touch_presence(who, "online").await.unwrap();
    // The hub follows the rooms off a broadcast stream: let it catch up.
    tokio::time::sleep(Duration::from_millis(20)).await;
    build(
        hub,
        &e2e(),
        who,
        SyncParams {
            since,
            full_state: false,
            timeout,
            filter: serde_json::from_value::<SyncFilter>(filter).unwrap(),
            device_id: None,
        },
    )
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

fn event_ids(timeline: &Value) -> Vec<String> {
    timeline["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["event_id"].as_str().map(str::to_owned))
        .collect()
}

/// "Sync can be polled for updates": with a filter that drops presence, a long-poll waits for
/// the message sent 100 ms into it and carries it. It used to answer at once with nothing: the
/// caller's own presence (which every `/sync` refreshes) was filtered out of the batch, so the
/// token never moved past it, and every long-poll woke for it immediately.
#[tokio::test]
async fn a_long_poll_with_presence_filtered_out_waits_for_the_message() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    let (_, token) = sync(&hub, &alice, None, no_presence(), Duration::ZERO).await;

    let sender = {
        let handle = handle.clone();
        let alice = alice.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            say(&handle, &alice, "1").await
        })
    };
    let started = Instant::now();
    let (response, _) = sync(
        &hub,
        &alice,
        Some(token),
        no_presence(),
        Duration::from_secs(10),
    )
    .await;
    let event_id = sender.await.unwrap();
    let timeline = &response["rooms"]["join"][room_id.as_str()]["timeline"];
    assert_eq!(event_ids(timeline), vec![event_id], "{response}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "woken by the message"
    );
}

/// "Sync is woken up for leaves": the same long-poll, woken by the user's own leave, has the
/// room in `rooms.leave` with the leave in its timeline.
#[tokio::test]
async fn a_long_poll_with_presence_filtered_out_is_woken_by_a_leave() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    let (_, token) = sync(&hub, &alice, None, no_presence(), Duration::ZERO).await;

    let leaver = {
        let handle = handle.clone();
        let alice = alice.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            handle
                .membership(alice.clone(), Action::Leave, alice, json!({}), 20)
                .await
                .unwrap();
        })
    };
    let (response, _) = sync(
        &hub,
        &alice,
        Some(token),
        no_presence(),
        Duration::from_secs(10),
    )
    .await;
    leaver.await.unwrap();
    let timeline = &response["rooms"]["leave"][room_id.as_str()]["timeline"];
    assert_eq!(event_ids(timeline).len(), 1, "{response}");
}

/// The token moves past presence the filter drops, and a long-poll with nothing else new waits
/// out its timeout rather than answering at once with the same token.
#[tokio::test]
async fn filtered_presence_moves_the_token_and_does_not_end_a_long_poll() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (_handle, _room_id) = create(&hub, &alice).await;
    let (_, token) = sync(&hub, &alice, None, no_presence(), Duration::ZERO).await;
    let own = hub.presence_of(&alice).await.unwrap().seq;
    assert!(
        token.presence_seq >= own,
        "the token covers the dropped presence"
    );

    let started = Instant::now();
    let (response, _) = sync(
        &hub,
        &alice,
        Some(token),
        no_presence(),
        Duration::from_millis(400),
    )
    .await;
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "nothing for this client, so the poll waits: {:?} {response}",
        started.elapsed()
    );
    assert!(
        response["rooms"].as_object().unwrap().is_empty(),
        "{response}"
    );
}

/// A long-poll woken by news the filter drops (bob typing, with `m.typing` filtered out of
/// `room.ephemeral`) keeps waiting, and answers with the message that follows.
#[tokio::test]
async fn a_long_poll_woken_by_filtered_news_waits_on_for_real_news() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let bob = user_id!("@bob:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    handle
        .membership(bob.clone(), Action::Join, bob.clone(), json!({}), 5)
        .await
        .unwrap();
    let filter = json!({
        "presence": {"not_types": ["m.presence"]},
        "room": {"ephemeral": {"not_types": ["m.typing"]}},
    });
    let (_, token) = sync(&hub, &alice, None, filter.clone(), Duration::ZERO).await;

    let actor = {
        let hub = hub.clone();
        let handle = handle.clone();
        let room_id = room_id.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            hub.set_typing(&room_id, &bob, true, Duration::from_secs(30))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            say(&handle, &bob, "after typing").await
        })
    };
    let (response, _) = sync(&hub, &alice, Some(token), filter, Duration::from_secs(10)).await;
    let event_id = actor.await.unwrap();
    let joined = &response["rooms"]["join"][room_id.as_str()];
    assert_eq!(event_ids(&joined["timeline"]), vec![event_id], "{response}");
    assert!(
        joined["ephemeral"]["events"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{response}"
    );
}

/// A fresh batch that holds the whole room gives the batch's end as `prev_batch` (Synapse's
/// `_load_filtered_recents`), so `/messages?dir=b` from it returns the room's events (Sytest's
/// "GET /rooms/:room_id/messages returns a message") and `/members?at=` it is the membership
/// as of this sync (Complement's `TestGetRoomMembersAtPoint`). A truncated one still gives the
/// point before its first event (Sytest's "A prev_batch token can be used in the v1 messages
/// API").
#[tokio::test]
async fn a_fresh_timeline_holding_the_whole_room_gives_its_end_as_prev_batch() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    let message = say(&handle, &alice, "Hello world!").await;

    let (response, _) = sync(&hub, &alice, None, json!({}), Duration::ZERO).await;
    let timeline = &response["rooms"]["join"][room_id.as_str()]["timeline"];
    assert_eq!(timeline["limited"], json!(false), "{response}");
    let prev_batch: PaginationToken = timeline["prev_batch"].as_str().unwrap().parse().unwrap();
    let page: Vec<String> = handle
        .query(move |actor| {
            actor
                .paginate(Some(prev_batch), Direction::Backward, 1)
                .0
                .iter()
                .map(|e| e.event_id().to_string())
                .collect()
        })
        .await;
    assert_eq!(
        page,
        vec![message.clone()],
        "paging back starts at the newest event"
    );

    // Truncated: before the first (and only) event of the batch.
    let (response, _) = sync(
        &hub,
        &alice,
        None,
        json!({"room": {"timeline": {"limit": 1}}}),
        Duration::ZERO,
    )
    .await;
    let timeline = &response["rooms"]["join"][room_id.as_str()]["timeline"];
    assert_eq!(timeline["limited"], json!(true), "{response}");
    assert_eq!(event_ids(timeline), vec![message.clone()]);
    let prev_batch: PaginationToken = timeline["prev_batch"].as_str().unwrap().parse().unwrap();
    let page: Vec<String> = handle
        .query(move |actor| {
            actor
                .paginate(Some(prev_batch), Direction::Backward, 1)
                .0
                .iter()
                .map(|e| e.event_id().to_string())
                .collect()
        })
        .await;
    assert_eq!(page.len(), 1);
    assert_ne!(page[0], message, "paging back continues before the batch");
}

/// A remote `m.room.name` from `sender` whose only `prev_event` is `parent`: a fork of the
/// room's DAG that reaches this server after the events that followed `parent` here.
async fn forked_name_event(
    handle: &RoomActorHandle<MemoryBackend>,
    sender: &UserId,
    parent: &str,
    name: &str,
) -> hs_model::Event {
    use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
    use hs_model::{hash, signing};

    let parent = ruma::EventId::parse(parent).unwrap();
    let sender = sender.to_owned();
    let name = name.to_owned();
    let (object, version) = handle
        .query(move |actor| {
            let version = actor.room_version().clone();
            let create = actor.state_event("m.room.create", "").unwrap().unwrap();
            let power = actor
                .state_event("m.room.power_levels", "")
                .unwrap()
                .unwrap();
            let member = actor
                .state_event("m.room.member", sender.as_str())
                .unwrap()
                .unwrap();
            let depth = actor.event_by_id(&parent).unwrap().header().depth + 1;
            let mut auth = vec![power.event_id().to_string(), member.event_id().to_string()];
            if !matches!(version.as_str(), "12") {
                auth.push(create.event_id().to_string());
            }
            let object = json!({
                "type": "m.room.name",
                "state_key": "",
                "sender": sender.as_str(),
                "room_id": actor.room_id().as_str(),
                "origin_server_ts": 3,
                "depth": depth,
                "content": {"name": name},
                "prev_events": [parent.as_str()],
                "auth_events": auth,
            });
            (object, version)
        })
        .await;
    let mut canonical = to_canonical_object(&object, true).unwrap();
    let content_hash = hash::content_hash_base64(&canonical);
    canonical.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(CanonicalJsonObject::from([(
            "sha256".to_owned(),
            CanonicalJsonValue::String(content_hash),
        )])),
    );
    let server = ruma::ServerName::parse("remote.example").unwrap();
    signing::sign_object(
        &mut canonical,
        &server,
        &signing::SigningKeyPair::generate("1"),
    )
    .unwrap();
    let bytes = CanonicalJsonValue::Object(canonical).to_canonical_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    hs_model::Event::parse(&value, version).unwrap()
}

/// Complement's `TestSyncOmitsStateChangeOnFilteredEvents`: E1, then E3 and E4 here, then S2
/// (a room name from another server whose `prev_events` is E1) arrives, then E5 merges the
/// forks. An initial sync filtering E5's type out with `limit: 1` has E4 in its timeline (the
/// newest by depth, as Synapse orders a fresh batch) and S2 in `state`. By arrival, S2 was the
/// timeline's one event and `state` lacked it.
#[tokio::test]
async fn a_fresh_batch_is_ordered_by_depth_so_a_late_fork_lands_in_state() {
    let hub = hub();
    let alice = user_id!("@alice:sync.test").to_owned();
    let bob = user_id!("@bob:remote.example").to_owned();
    let (handle, room_id) = create(&hub, &alice).await;
    handle
        .membership(bob.clone(), Action::Join, bob.clone(), json!({}), 5)
        .await
        .unwrap();
    // Bob may name the room (in Complement, his server made it).
    handle
        .send_event(
            alice.clone(),
            "m.room.power_levels".to_owned(),
            Some(String::new()),
            json!({"users": {alice.as_str(): 100, bob.as_str(): 100}}),
            None,
            6,
        )
        .await
        .unwrap();
    let e1 = say(&handle, &alice, "E1").await;
    let s2 = forked_name_event(&handle, &bob, &e1, "I am the room name, S2").await;
    let s2_id = s2.event_id().to_string();
    say(&handle, &alice, "E3").await;
    let e4 = say(&handle, &alice, "E4").await;
    handle.accept_remote_event(s2).await.unwrap();
    handle
        .send_event(
            alice.clone(),
            "please_filter_me".to_owned(),
            None,
            json!({"body": "E5", "msgtype": "m.text"}),
            None,
            10,
        )
        .await
        .unwrap();

    let (response, _) = sync(
        &hub,
        &alice,
        None,
        json!({"room": {"timeline": {"not_types": ["please_filter_me"], "limit": 1}}}),
        Duration::ZERO,
    )
    .await;
    let joined = &response["rooms"]["join"][room_id.as_str()];
    assert_eq!(event_ids(&joined["timeline"]), vec![e4], "{response}");
    assert!(
        event_ids(&joined["state"]).contains(&s2_id),
        "the fork's room name is in state: {response}"
    );
}
