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
    let user_store: DynUserStore = Arc::new(TablesUserStore::open(backend.clone()).unwrap());
    // A high threshold: these tests are about ordinary small-room fan-out-on-write behavior, not
    // the hot-room path (`crate::hub`'s own tests cover that in isolation).
    let hub: Hub = Arc::new(SessionHub::new(user_store, rooms.clone(), 10_000));
    let e2e: Arc<dyn hs_e2e::store::E2eStore> =
        Arc::new(hs_e2e::store::tables::TablesE2eStore::open(backend).unwrap());

    let room_state = RoomState {
        auth: auth.clone(),
        rooms: rooms.clone(),
        identity,
        remote_join: None,
    };
    let user_state = UserState {
        auth: auth.clone(),
        hub: hub.clone(),
        e2e,
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

    s.register("alice", "alice", "hunter2-alice")
        .await
        .assert_ok();
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
    hub.watch_room(handle).await;
    settle().await;

    // 2. Both users take an initial sync baseline before anything else happens.
    let alice_initial = s.sync("alice").await;
    alice_initial.assert_ok();
    let alice_token_before_invite = alice_initial.str_field("next_batch").to_owned();

    let bob_initial = s.sync("bob").await;
    bob_initial.assert_ok();
    assert!(
        bob_initial.json["rooms"]["invite"]
            .as_object()
            .is_none_or(|m| m.is_empty()),
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
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
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
        replay_events
            .iter()
            .any(|e| e["content"]["body"] == "hello bob"),
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
        alice_events
            .iter()
            .any(|e| e["content"]["body"] == "hello bob"),
        "alice's own old token should also resolve forward to the message: {}",
        alice_after_message.json
    );
}

#[tokio::test]
async fn a_sync_token_issued_before_a_message_returns_it_even_after_unrelated_activity_happens() {
    let (mut s, rooms, hub) = setup();
    s.register("alice", "alice", "hunter2-alice")
        .await
        .assert_ok();

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
    hub.watch_room(handle).await;
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
        events
            .iter()
            .any(|e| e["content"]["body"] == "delayed but not lost"),
        "the early token must still surface the message despite unrelated activity in between: {}",
        response.json
    );
}

/// The cross-track fix this session exists for: `GET /rooms/{roomId}/messages` (`hs-room`,
/// track 04) accepting a token minted by `/sync` (this crate's `hsu1_...`), the exact token every
/// real Matrix client has in hand right after syncing and hands straight to `/messages` to scroll
/// back -- and Complement's `room_messages_test.go` does the same (`TestSendAndFetchMessage`
/// feeds a bare `/sync` `next_batch` into `/messages?from=`). Before this session, `hs-room`'s
/// `PaginationToken::from_str` rejected this shape outright with `400 M_INVALID_PARAM`.
///
/// Pagination boundaries are exclusive of the token's own position (matching this crate's
/// existing room-scoped `timeline.prev_batch`, `crate::sync::build_incremental_timeline`'s doc
/// comment): a token minted right after observing message M pages *backward* to whatever is
/// older than M, and *forward* to whatever is newer than M -- never M itself, since the client
/// already has M from the sync response that handed it the token. This test sends an older and a
/// newer message around one sync token and checks each direction finds the right one.
#[tokio::test]
async fn messages_accepts_a_token_minted_by_sync_in_both_directions() {
    let (mut s, rooms, hub) = setup();
    s.register("alice", "alice", "hunter2-alice")
        .await
        .assert_ok();

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
    hub.watch_room(handle).await;
    settle().await;

    s.send(
        Some("alice"),
        Method::PUT,
        &format!("/rooms/{room_id}/send/m.room.message/txn-older"),
        Some(json!({"msgtype": "m.text", "body": "older message"})),
    )
    .await
    .assert_status(StatusCode::OK);
    settle().await;

    // The token under test: minted by `/sync`, right after observing "older message" and nothing
    // else -- exactly what a real client presents to `/messages` next.
    let baseline = s.sync("alice").await;
    baseline.assert_ok();
    let baseline_token = baseline.str_field("next_batch").to_owned();

    s.send(
        Some("alice"),
        Method::PUT,
        &format!("/rooms/{room_id}/send/m.room.message/txn-newer"),
        Some(json!({"msgtype": "m.text", "body": "newer message"})),
    )
    .await
    .assert_status(StatusCode::OK);
    settle().await;

    let after = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/sync?since={baseline_token}&timeout=0"),
            None,
        )
        .await;
    after.assert_ok();
    let after_token = after.str_field("next_batch").to_owned();

    // dir=f from the token minted *before* "newer message": walking forward must find it.
    let forward = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/messages?dir=f&from={baseline_token}"),
            None,
        )
        .await;
    forward.assert_ok();
    let forward_chunk = forward.json["chunk"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        forward_chunk
            .iter()
            .any(|e| e["content"]["body"] == "newer message"),
        "paginating forward from a pre-message /sync token should find the message: {}",
        forward.json
    );
    assert!(
        !forward_chunk
            .iter()
            .any(|e| e["content"]["body"] == "older message"),
        "forward pagination must not re-return the message already covered by the token: {}",
        forward.json
    );

    // dir=b from the token minted *after* "newer message": walking backward must find "older
    // message" (older than the token), not re-return "newer message" (already covered by it).
    let backward = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/messages?dir=b&from={after_token}"),
            None,
        )
        .await;
    backward.assert_ok();
    let backward_chunk = backward.json["chunk"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        backward_chunk
            .iter()
            .any(|e| e["content"]["body"] == "older message"),
        "paginating backward from a post-message /sync token should find the earlier message: {}",
        backward.json
    );
    assert!(
        !backward_chunk
            .iter()
            .any(|e| e["content"]["body"] == "newer message"),
        "backward pagination must not re-return the message already covered by the token: {}",
        backward.json
    );

    // A token minted by `/messages` itself (this endpoint's own `end`) must still work exactly as
    // before -- the "don't break what worked" half of this session's brief. Taken from a page of
    // one, which cannot have reached the start of the room: a page that does carries no `end`
    // at all (the spec's "no further events"), and the page above, with the default limit over
    // a room this small, is exactly that page.
    assert!(
        backward.json.get("end").is_none(),
        "a page that reached the room's first event must not offer a continuation: {}",
        backward.json
    );
    let one = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/messages?dir=b&from={after_token}&limit=1"),
            None,
        )
        .await;
    one.assert_ok();
    let room_local_token = one.json["end"].as_str().map(str::to_owned);
    let room_local_token =
        room_local_token.expect("a page short of the room's start returns a continuation token");
    let continued = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/messages?dir=b&from={room_local_token}"),
            None,
        )
        .await;
    continued.assert_ok();

    // And an outright malformed token (neither format) is still a clean 400, not a panic or a
    // silently-ignored constraint.
    s.send(
        Some("alice"),
        Method::GET,
        &format!("/rooms/{room_id}/messages?dir=b&from=garbage-not-a-token"),
        None,
    )
    .await
    .assert_status(StatusCode::BAD_REQUEST);
}

/// The bug Element users actually saw: without `unsigned.prev_content` a client cannot tell a
/// display-name change from a join, and renders "Alice changed her display name to Alice Smith"
/// as "Alice joined the room".
///
/// `/messages` and `/state` are covered in `hs-room`'s own tests; this one goes through `/sync`,
/// which is where a client's live timeline actually comes from — a fix that reached only the
/// scrollback endpoints would have looked complete and changed nothing a user sees.
#[tokio::test]
async fn a_display_name_change_arrives_over_sync_with_the_old_name_attached() {
    let (mut s, rooms, hub) = setup();

    s.register("alice", "alice", "hunter2-alice")
        .await
        .assert_ok();
    let alice_user_id = s.session("alice").unwrap().user_id.clone().unwrap();

    let create = s
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "private_chat"})),
        )
        .await;
    create.assert_ok();
    let room_id = create.str_field("room_id").to_owned();

    let handle = rooms
        .get_or_load(&ruma::RoomId::parse(&room_id).unwrap())
        .await
        .unwrap();
    hub.watch_room(handle).await;
    settle().await;

    let set_name = s
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/profile/{alice_user_id}/displayname"),
            Some(json!({"displayname": "Alice"})),
        )
        .await;
    set_name.assert_ok();
    settle().await;

    // The baseline is taken *after* the first name is set, so the incremental sync below carries
    // the rename and nothing else.
    let baseline = s.sync("alice").await;
    baseline.assert_ok();
    let since = baseline.str_field("next_batch").to_owned();

    let rename = s
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/profile/{alice_user_id}/displayname"),
            Some(json!({"displayname": "Alice Smith"})),
        )
        .await;
    rename.assert_ok();
    settle().await;

    let after = s
        .send(
            Some("alice"),
            Method::GET,
            &format!("/sync?since={since}&timeout=0"),
            None,
        )
        .await;
    after.assert_ok();

    let timeline = after.json["rooms"]["join"][&room_id]["timeline"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let renamed = timeline
        .iter()
        .find(|e| e["type"] == "m.room.member" && e["content"]["displayname"] == "Alice Smith")
        .unwrap_or_else(|| {
            panic!(
                "the rename should be in alice's incremental sync: {}",
                after.json
            )
        });

    assert_eq!(
        renamed["unsigned"]["prev_content"]["displayname"], "Alice",
        "without the previous content a client renders this as a join: {renamed}"
    );
    assert_eq!(
        renamed["unsigned"]["prev_sender"],
        alice_user_id.to_string(),
        "{renamed}"
    );
    assert!(
        renamed["unsigned"]["replaces_state"].is_string(),
        "the superseded event's id must be there too: {renamed}"
    );
}

/// An invite that does not say who was invited is not an invite a client can render. The spec's
/// own `invite_state` example carries the recipient's `m.room.member` event, and every checker
/// that reads membership out of `invite_state` — Complement's `syncMembershipIn`, and so every
/// invite test in its `csapi` suite — needs it. It was missing: `invite_state` carried
/// `m.room.create` and `m.room.join_rules` and nothing about the invitee at all.
///
/// The second half of the same rule: a stripped state event may carry only `sender`, `type`,
/// `state_key` and `content`. These were full client events, with `event_id`, `origin_server_ts`,
/// `room_id` and `unsigned` on them.
#[tokio::test]
async fn an_invite_carries_the_invitees_own_membership_and_nothing_it_should_not() {
    let (mut s, rooms, hub) = setup();

    s.register("alice", "alice", "hunter2-alice")
        .await
        .assert_ok();
    s.register("bob", "bob", "hunter2-bob").await.assert_ok();
    let bob_user_id = s.session("bob").unwrap().user_id.clone().unwrap();
    let alice_user_id = s.session("alice").unwrap().user_id.clone().unwrap();

    let create = s
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "private_chat", "name": "Invite Me"})),
        )
        .await;
    create.assert_ok();
    let room_id = create.str_field("room_id").to_owned();

    let handle = rooms
        .get_or_load(&ruma::RoomId::parse(&room_id).unwrap())
        .await
        .unwrap();
    hub.watch_room(handle).await;
    settle().await;

    s.send(
        Some("alice"),
        Method::POST,
        &format!("/rooms/{room_id}/invite"),
        Some(json!({"user_id": bob_user_id})),
    )
    .await
    .assert_ok();
    settle().await;

    let sync = s.sync("bob").await;
    sync.assert_ok();
    let events = sync.json["rooms"]["invite"][&room_id]["invite_state"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("bob should see the invite: {}", sync.json));

    let own_membership = events
        .iter()
        .find(|e| {
            e["type"] == "m.room.member" && e["state_key"] == bob_user_id.to_string().as_str()
        })
        .unwrap_or_else(|| panic!("invite_state must say who was invited: {events:#?}"));
    assert_eq!(own_membership["content"]["membership"], "invite");
    assert_eq!(own_membership["sender"], alice_user_id.to_string());

    assert!(
        events.iter().any(|e| {
            e["type"] == "m.room.member" && e["state_key"] == alice_user_id.to_string().as_str()
        }),
        "the inviter's own membership belongs there too, or a client cannot render \
         \"Alice invited you\" without joining: {events:#?}"
    );
    assert!(
        events.iter().any(|e| e["type"] == "m.room.create"),
        "m.room.create is required in invite_state as of Matrix v1.16: {events:#?}"
    );

    for event in &events {
        let keys: Vec<&str> = event
            .as_object()
            .expect("each stripped event is an object")
            .keys()
            .map(String::as_str)
            .collect();
        for key in &keys {
            assert!(
                matches!(*key, "sender" | "type" | "state_key" | "content"),
                "a stripped state event may carry only sender/type/state_key/content, \
                 found {key:?} in {event:#?}"
            );
        }
    }
}

/// Complement's "Presence can be set from sync": polling `/sync` is itself a presence signal, and
/// `?set_presence=` is how a client says which one. This was parsed and thrown away, so a user's
/// presence only ever changed through an explicit `PUT`, and a client that never calls that --
/// which is most of them -- looked permanently offline to everybody they shared a room with.
#[tokio::test]
async fn a_sync_poll_sets_the_callers_presence_and_the_other_member_sees_it() {
    let (mut s, rooms, hub) = setup();

    s.register("alice", "alice", "hunter2-alice")
        .await
        .assert_ok();
    s.register("bob", "bob", "hunter2-bob").await.assert_ok();
    let alice_user_id = s.session("alice").unwrap().user_id.clone().unwrap();
    let bob_user_id = s.session("bob").unwrap().user_id.clone().unwrap();

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
    hub.watch_room(handle).await;
    settle().await;

    s.send(
        Some("alice"),
        Method::POST,
        &format!("/rooms/{room_id}/invite"),
        Some(json!({"user_id": bob_user_id})),
    )
    .await
    .assert_ok();
    s.send(
        Some("bob"),
        Method::POST,
        &format!("/rooms/{room_id}/join"),
        Some(json!({})),
    )
    .await
    .assert_ok();
    settle().await;

    // Bob's baseline, taken before alice says anything about her presence.
    let baseline = s.sync("bob").await;
    baseline.assert_ok();
    let since = baseline.str_field("next_batch").to_owned();

    // Alice polls with `set_presence=unavailable` -- no PUT anywhere in this test.
    s.send(
        Some("alice"),
        Method::GET,
        "/sync?timeout=0&set_presence=unavailable",
        None,
    )
    .await
    .assert_ok();
    settle().await;

    let after = s
        .send(
            Some("bob"),
            Method::GET,
            &format!("/sync?since={since}&timeout=0"),
            None,
        )
        .await;
    after.assert_ok();

    let events = after.json["presence"]["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let alice_presence = events
        .iter()
        .find(|e| e["sender"] == alice_user_id.to_string().as_str())
        .unwrap_or_else(|| {
            panic!(
                "bob shares a room with alice and must see her presence: {}",
                after.json
            )
        });
    assert_eq!(alice_presence["type"], "m.presence");
    assert_eq!(alice_presence["content"]["presence"], "unavailable");
}
