//! End-to-end `/sync` v2 tests driven through real HTTP via `hs-testkit`'s [`Scenario`], the same
//! way `crates/hs-room/tests/scenario.rs` and `crates/hs-testkit/tests/hs_auth_round_trip.rs`
//! drive their own crates: this test assembles one merged router out of `hs-auth`'s, `hs-room`'s
//! and this crate's own route fragments (the same shape `hs-cli`'s `build_router` assembles them
//! in production -- see `crates/hs-cli/src/serve.rs`), sharing one `RoomRegistry` and one
//! `SessionHub` over it.
//!
//! # The discovery-gap workaround, made explicit
//!
//! `crate::hub`'s module docs describe "the discovery gap": this crate's [`SessionHub`] only
//! learns about a room once something calls [`SessionHub::watch_room`] for it, and no crate here
//! is allowed to add that hook to `hs_room::registry::RoomRegistry` itself (track 04's crate).
//! This test plays the role `hs-cli` (or the RFC'd registry hook) would play in production: right
//! after a room is created, it fetches that room's handle from the shared registry and calls
//! [`SessionHub::watch_room`] on it. Every other test in this file relies on that one line having
//! run.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use hs_auth::state::AuthState;
use hs_kv::memory::MemoryBackend;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use hs_room::state::RoomState;
use hs_testkit::scenario::Scenario;
use hs_user::hub::SessionHub;
use hs_user::state::UserState;
use hs_user::store::DynUserStore;
use hs_user::store::tables::TablesUserStore;
use serde_json::json;

/// The one server name this test's auth config and room registry must agree on.
const SERVER_NAME: &str = "sync-e2e.test";

type Registry = Arc<RoomRegistry<MemoryBackend>>;
type Hub = Arc<SessionHub<MemoryBackend, Registry>>;

/// Builds one merged `/_matrix/client/v3`-prefixed [`Scenario`] over `hs-auth` + `hs-room` +
/// `hs-user`, all sharing one embedded backend, one `AuthState` and one `RoomRegistry`. Returns
/// the scenario plus the registry and hub a test needs direct access to (for the discovery-gap
/// workaround, and for asserting on durable state directly).
fn setup() -> (Scenario, Registry, Hub) {
    let backend = MemoryBackend::new();
    let identity = HomeserverIdentity::for_tests(SERVER_NAME);
    let rooms: Registry = Arc::new(RoomRegistry::open(backend.clone(), identity.clone()).unwrap());
    // `AuthState::in_memory`'s default server name is `example.org`, which would mint
    // `@alice:example.org` while the registry mints `!room:sync-e2e.test` -- and `hs-room` rejects
    // a send whose room's server name does not match the sender's. One server name, named once.
    let auth = AuthState::in_memory_with_config(hs_auth::config::AuthConfig {
        server_name: ruma::ServerName::parse(SERVER_NAME).expect("a valid server name"),
        ..Default::default()
    });
    let user_store: DynUserStore = Arc::new(TablesUserStore::open(backend).unwrap());
    // A high threshold: these tests are about ordinary small-room fan-out-on-write behavior, not
    // the hot-room path (`crate::hub`'s own tests cover that in isolation).
    let hub: Hub = Arc::new(SessionHub::new(user_store, rooms.clone(), 10_000));

    let room_state = RoomState {
        auth: auth.clone(),
        rooms: rooms.clone(),
        identity,
    };
    let user_state = UserState {
        auth: auth.clone(),
        hub: hub.clone(),
    };

    let auth_router = hs_auth::routes::router().with_state(auth);
    let (room_router, _) = hs_room::routes::router::<MemoryBackend>();
    let room_router = room_router.with_state(room_state);
    let (user_router, _) = hs_user::routes::router::<MemoryBackend, Registry>();
    let user_router = user_router.with_state(user_state);

    let merged = auth_router.merge(room_router).merge(user_router);
    let top = axum::Router::new().nest("/_matrix/client/v3", merged);

    // The router is nested the way `hs-cli` mounts it, so the scenario needs the matching prefix:
    // `Scenario::new` would send every request to a bare `/register` and get a 404.
    (Scenario::with_prefix(top, "/_matrix/client/v3"), rooms, hub)
}

/// Lets `hub`'s background watcher task process whatever it was just handed. Generous relative to
/// the in-process work involved (no real I/O), kept short so this test suite stays fast.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn register_invite_join_and_message_flow_reaches_both_users_syncs() {
    let (mut s, rooms, hub) = setup();

    s.register("alice", "alice", "hunter2-alice").await.assert_ok();
    s.register("bob", "bob", "hunter2-bob").await.assert_ok();
    let bob_user_id = s.session("bob").unwrap().user_id.clone().unwrap();

    // 1. alice creates a room.
    let create = s
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "private_chat", "name": "Test Room"})),
        )
        .await;
    create.assert_ok();
    let room_id = create.str_field("room_id").to_owned();

    // The discovery-gap workaround: tell the hub about this room now that it exists.
    let handle = rooms
        .get_or_load(&ruma::RoomId::parse(&room_id).unwrap())
        .await
        .unwrap();
    hub.watch_room(handle);
    settle().await;

    // 2. Both users take an initial sync baseline before anything else happens.
    let alice_initial = s.sync("alice").await;
    alice_initial.assert_ok();
    let alice_token_before_invite = alice_initial.str_field("next_batch").to_owned();

    let bob_initial = s.sync("bob").await;
    bob_initial.assert_ok();
    assert!(
        bob_initial.json["rooms"]["invite"].as_object().is_none_or(|m| m.is_empty()),
        "bob has not been invited yet"
    );

    // 3. alice invites bob.
    let invite = s
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/invite"),
            Some(json!({"user_id": bob_user_id})),
        )
        .await;
    invite.assert_ok();
    settle().await;

    // 4. bob's next sync (incremental, from his own initial token) must show the invite.
    let bob_since = bob_initial.str_field("next_batch").to_owned();
    let bob_after_invite = s
        .send(
            Some("bob"),
            Method::GET,
            &format!("/sync?since={bob_since}&timeout=0"),
            None,
        )
        .await;
    bob_after_invite.assert_ok();
    assert!(
        bob_after_invite.json["rooms"]["invite"]
            .get(&room_id)
            .is_some(),
        "bob's incremental sync should show the invite: {}",
        bob_after_invite.json
    );

    // 5. bob joins.
    let join = s
        .send(Some("bob"), Method::POST, &format!("/rooms/{room_id}/join"), Some(json!({})))
        .await;
    join.assert_ok();
    settle().await;

    let bob_after_join = s.sync("bob").await;
    bob_after_join.assert_ok();
    assert!(
        bob_after_join.json["rooms"]["join"].get(&room_id).is_some(),
        "bob should now be joined: {}",
        bob_after_join.json
    );
    // This is the token under test for the next two assertions -- issued strictly *before* the
    // message that follows.
    let bob_token_before_message = bob_after_join.str_field("next_batch").to_owned();

    // 6. alice sends a message.
    let send = s
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/send/m.room.message/txn-1"),
            Some(json!({"msgtype": "m.text", "body": "hello bob"})),
        )
        .await;
    send.assert_status(StatusCode::OK);
    settle().await;

    // 7. The message appears in bob's incremental sync (a message sent by one user appears in
    //    the other's incremental sync).
    let bob_after_message = s
        .send(
            Some("bob"),
            Method::GET,
            &format!("/sync?since={bob_token_before_message}&timeout=0"),
            None,
        )
        .await;
    bob_after_message.assert_ok();
    let events = bob_after_message.json["rooms"]["join"][&room_id]["timeline"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        events.iter().any(|e| e["content"]["body"] == "hello bob"),
        "bob's incremental sync should carry alice's message: {}",
        bob_after_message.json
    );

    // 8. The same token, presented *again* (the classic "sync token from before a message"
    //    case), still returns the message -- it must not have been consumed / invalidated by the
    //    previous call.
    let replay = s
        .send(
            Some("bob"),
            Method::GET,
            &format!("/sync?since={bob_token_before_message}&timeout=0"),
            None,
        )
        .await;
    replay.assert_ok();
    let replay_events = replay.json["rooms"]["join"][&room_id]["timeline"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        replay_events.iter().any(|e| e["content"]["body"] == "hello bob"),
        "presenting the same pre-message token again must still return the message: {}",
        replay.json
    );

    // alice's own old token (from before the invite, well before the message) must also still
    // resolve to the message once she syncs incrementally from it.
    let alice_after_message = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/sync?since={alice_token_before_invite}&timeout=0"),
            None,
        )
        .await;
    alice_after_message.assert_ok();
    let alice_events = alice_after_message.json["rooms"]["join"][&room_id]["timeline"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        alice_events.iter().any(|e| e["content"]["body"] == "hello bob"),
        "alice's own old token should also resolve forward to the message: {}",
        alice_after_message.json
    );
}

#[tokio::test]
async fn a_sync_token_issued_before_a_message_returns_it_even_after_unrelated_activity_happens() {
    let (mut s, rooms, hub) = setup();
    s.register("alice", "alice", "hunter2-alice").await.assert_ok();

    let create = s
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat"})),
        )
        .await;
    create.assert_ok();
    let room_id = create.str_field("room_id").to_owned();
    let handle = rooms
        .get_or_load(&ruma::RoomId::parse(&room_id).unwrap())
        .await
        .unwrap();
    hub.watch_room(handle);
    settle().await;

    // The token under test, issued right after the room was created.
    let early = s.sync("alice").await;
    early.assert_ok();
    let early_token = early.str_field("next_batch").to_owned();

    // Unrelated activity: a second, unrelated room is created (advancing alice's account in
    // general but not this room), then the message this test cares about is sent.
    s.send(
        Some("alice"),
        Method::POST,
        "/createRoom",
        Some(json!({"preset": "public_chat"})),
    )
    .await
    .assert_ok();
    settle().await;

    s.send(
        Some("alice"),
        Method::PUT,
        &format!("/rooms/{room_id}/send/m.room.message/txn-1"),
        Some(json!({"msgtype": "m.text", "body": "delayed but not lost"})),
    )
    .await
    .assert_status(StatusCode::OK);
    settle().await;

    let response = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/sync?since={early_token}&timeout=0"),
            None,
        )
        .await;
    response.assert_ok();
    let events = response.json["rooms"]["join"][&room_id]["timeline"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        events.iter().any(|e| e["content"]["body"] == "delayed but not lost"),
        "the early token must still surface the message despite unrelated activity in between: {}",
        response.json
    );
}
