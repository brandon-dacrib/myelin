//! Scenario tests over real HTTP: create a room, send messages, paginate, change membership and
//! redact -- driven through `hs-testkit`'s `Scenario` DSL against a router that mounts both
//! `hs-auth`'s real router (for registration/login) and this crate's own `hs_room::routes::router`
//! fragment, sharing one `AuthState`. This is `crate::state::RoomState`'s composition pattern
//! (`crates/hs-room/src/state.rs`'s module docs) exercised exactly the way a real server would
//! wire it, per this track's "definition of done": scenario tests through `hs-testkit`.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use hs_auth::state::AuthState;
use hs_kv::memory::MemoryBackend;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use hs_room::state::RoomState;
use hs_testkit::Scenario;
use serde_json::json;

/// Like [`app`], but also hands back the [`RoomRegistry`] directly -- for tests that need to
/// check something (like the room directory's publish flag) that has no HTTP surface of its own
/// in this crate today; see `crate::routes::directory`'s module doc for why
/// `GET`/`POST /publicRooms` are not mounted here (`hs-user` already serves them).
fn app_with_registry() -> (axum::Router, Arc<RoomRegistry<MemoryBackend>>) {
    let auth_state = AuthState::in_memory();
    let backend = MemoryBackend::new();
    let identity = HomeserverIdentity::for_tests("example.org");
    let registry = Arc::new(RoomRegistry::open(backend, identity.clone()).expect("open registry"));
    let room_state = RoomState {
        auth: auth_state.clone(),
        rooms: registry.clone(),
        identity,
    };
    let (room_router, _manifest) = hs_room::routes::router::<MemoryBackend>();

    let router = hs_auth::routes::router()
        .with_state(auth_state)
        .merge(room_router.with_state(room_state));
    (router, registry)
}

fn app() -> axum::Router {
    app_with_registry().0
}

#[tokio::test]
async fn create_room_send_paginate_membership_and_redact_round_trip() {
    let mut scenario = Scenario::new(app());

    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();

    // --- create a public room ---
    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat", "name": "Test Room"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    // The creator is already joined (bootstrap join), and the room's state reflects the preset.
    let state = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/state"),
            None,
        )
        .await;
    state.assert_ok();
    let types: Vec<&str> = state
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"m.room.create"));
    assert!(types.contains(&"m.room.power_levels"));
    assert!(types.contains(&"m.room.join_rules"));
    assert!(types.contains(&"m.room.name"));

    // --- bob joins the public room without an invite ---
    let join = scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await;
    join.assert_ok();

    let members = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/members"),
            None,
        )
        .await;
    members.assert_ok();
    assert_eq!(members.json["chunk"].as_array().unwrap().len(), 2);

    // --- send several messages ---
    let mut event_ids = Vec::new();
    for i in 0..3 {
        let send = scenario
            .send(
                Some("alice"),
                Method::PUT,
                &format!("/rooms/{room_id}/send/m.room.message/txn{i}"),
                Some(json!({"msgtype": "m.text", "body": format!("message {i}")})),
            )
            .await;
        send.assert_ok();
        event_ids.push(send.str_field("event_id").to_string());
    }

    // --- paginate backwards with a small page size ---
    let page = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/messages?dir=b&limit=2"),
            None,
        )
        .await;
    page.assert_ok();
    let chunk = page.json["chunk"].as_array().unwrap();
    assert_eq!(chunk.len(), 2, "limit=2 must cap the page at 2 events");
    // Backward pagination returns newest-first.
    assert_eq!(chunk[0]["content"]["body"], "message 2");
    assert_eq!(chunk[1]["content"]["body"], "message 1");

    let end_token = page.json["end"].as_str().unwrap().to_string();
    let next_page = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/messages?dir=b&limit=2&from={end_token}"),
            None,
        )
        .await;
    next_page.assert_ok();
    let next_chunk = next_page.json["chunk"].as_array().unwrap();
    // The page continues strictly before "message 1" in the full timeline (which also contains
    // this room's state events, not just messages), so the first entry of this page is "message
    // 0" and the second is whatever state event preceded it -- only the first is asserted on.
    assert_eq!(next_chunk[0]["content"]["body"], "message 0");

    // --- fetch one event directly and its context ---
    let get_event = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{}", event_ids[1]),
            None,
        )
        .await;
    get_event.assert_ok();
    assert_eq!(get_event.json["content"]["body"], "message 1");

    let context = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/context/{}?limit=1", event_ids[1]),
            None,
        )
        .await;
    context.assert_ok();
    assert_eq!(context.json["event"]["event_id"], event_ids[1]);
    let events_before = context.json["events_before"].as_array().unwrap();
    assert_eq!(events_before.len(), 1);
    assert_eq!(events_before[0]["content"]["body"], "message 0");
    let events_after = context.json["events_after"].as_array().unwrap();
    assert_eq!(events_after.len(), 1);
    assert_eq!(events_after[0]["content"]["body"], "message 2");

    // --- redact the first message ---
    let redact = scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/redact/{}/rtxn1", event_ids[0]),
            Some(json!({"reason": "test"})),
        )
        .await;
    redact.assert_ok();

    let redacted_event = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{}", event_ids[0]),
            None,
        )
        .await;
    redacted_event.assert_ok();
    assert!(
        redacted_event.json["content"]
            .as_object()
            .unwrap()
            .is_empty(),
        "a redacted m.room.message must have empty content, got {}",
        redacted_event.json["content"]
    );

    // --- bob leaves, then alice kicks nobody left to kick, so ban an invitee instead ---
    let leave = scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/leave"),
            Some(json!({})),
        )
        .await;
    leave.assert_ok();

    let members_after_leave = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/joined_members"),
            None,
        )
        .await;
    members_after_leave.assert_ok();
    assert_eq!(
        members_after_leave.json["joined"]
            .as_object()
            .unwrap()
            .len(),
        1,
        "only alice should remain joined after bob leaves"
    );
}

#[tokio::test]
async fn invite_only_room_rejects_a_join_without_invite_then_succeeds_after_one() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("carol", "carol", "another passphrase")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "private_chat"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    let denied = scenario
        .send(
            Some("carol"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await;
    denied.assert_matrix_error(StatusCode::FORBIDDEN, "M_FORBIDDEN");

    let invite = scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/invite"),
            Some(json!({"user_id": "@carol:example.org"})),
        )
        .await;
    invite.assert_ok();

    let join = scenario
        .send(
            Some("carol"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await;
    join.assert_ok();
}

#[tokio::test]
async fn ban_prevents_rejoin_until_unbanned() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("dave", "dave", "yet another passphrase")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    scenario
        .send(
            Some("dave"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    let ban = scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/ban"),
            Some(json!({"user_id": "@dave:example.org", "reason": "spam"})),
        )
        .await;
    ban.assert_ok();

    let rejoin = scenario
        .send(
            Some("dave"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await;
    rejoin.assert_matrix_error(StatusCode::FORBIDDEN, "M_FORBIDDEN");

    let unban = scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/unban"),
            Some(json!({"user_id": "@dave:example.org"})),
        )
        .await;
    unban.assert_ok();

    let rejoin_after_unban = scenario
        .send(
            Some("dave"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await;
    rejoin_after_unban.assert_ok();
}

/// A user's profile (`PUT /profile/{userId}/displayname`, `PUT .../avatar_url`, both `hs-auth`
/// routes merged into the same router as this crate's room routes here -- see `app()`'s module
/// doc) is copied into their own `m.room.member` content at the moment a new membership event is
/// sent: joining, being invited, and knocking. Exercises the same propagation path
/// `crate::routes::membership`'s module doc describes, end to end over real HTTP through both
/// crates' real routers, not just this crate's own unit tests.
#[tokio::test]
async fn profile_propagates_into_join_invite_and_knock_membership_content() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();
    scenario
        .register("carol", "carol", "another passphrase")
        .await
        .assert_ok();

    // Bob sets his profile before joining anything.
    scenario
        .send(
            Some("bob"),
            Method::PUT,
            "/profile/@bob:example.org/displayname",
            Some(json!({"displayname": "Bob T. Builder"})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("bob"),
            Method::PUT,
            "/profile/@bob:example.org/avatar_url",
            Some(json!({"avatar_url": "mxc://example.org/bob-avatar"})),
        )
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    // --- join carries the target's profile ---
    scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    let bob_member = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/state/m.room.member/@bob:example.org"),
            None,
        )
        .await;
    bob_member.assert_ok();
    assert_eq!(bob_member.json["displayname"], "Bob T. Builder");
    assert_eq!(
        bob_member.json["avatar_url"],
        "mxc://example.org/bob-avatar"
    );

    // A user with no profile set at all gets a membership event with neither field, not
    // `null`-valued ones.
    let alice_member = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/state/m.room.member/@alice:example.org"),
            None,
        )
        .await;
    alice_member.assert_ok();
    assert!(alice_member.json.get("displayname").is_none());
    assert!(alice_member.json.get("avatar_url").is_none());

    // --- invite carries the target's profile too ---
    scenario
        .send(
            Some("carol"),
            Method::PUT,
            "/profile/@carol:example.org/displayname",
            Some(json!({"displayname": "Carol"})),
        )
        .await
        .assert_ok();
    let private = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "private_chat"})),
        )
        .await;
    private.assert_ok();
    let private_room_id = private.str_field("room_id").to_string();
    scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{private_room_id}/invite"),
            Some(json!({"user_id": "@carol:example.org"})),
        )
        .await
        .assert_ok();

    let carol_member = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{private_room_id}/state/m.room.member/@carol:example.org"),
            None,
        )
        .await;
    carol_member.assert_ok();
    assert_eq!(carol_member.json["displayname"], "Carol");

    // --- a profile change is not retroactive, but re-sending join picks up the new one ---
    scenario
        .send(
            Some("bob"),
            Method::PUT,
            "/profile/@bob:example.org/displayname",
            Some(json!({"displayname": "Bobby"})),
        )
        .await
        .assert_ok();

    let still_old = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/state/m.room.member/@bob:example.org"),
            None,
        )
        .await;
    still_old.assert_ok();
    assert_eq!(
        still_old.json["displayname"], "Bob T. Builder",
        "a profile change must not retroactively edit an already-sent membership event"
    );

    scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    let updated = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/state/m.room.member/@bob:example.org"),
            None,
        )
        .await;
    updated.assert_ok();
    assert_eq!(
        updated.json["displayname"], "Bobby",
        "re-sending join must pick up the new profile"
    );
}

/// The security fix this session's brief opened with: `m.room.history_visibility: joined` must
/// stop a departed member from seeing events sent after they left, on both
/// `GET .../messages` and `GET .../event/{eventId}` -- previously neither endpoint enforced
/// `history_visibility` at all, so a user who left a non-world-readable room kept full read
/// access to everything, including events sent after they left.
#[tokio::test]
async fn history_visibility_joined_hides_events_sent_after_a_member_leaves() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({
                "preset": "public_chat",
                "initial_state": [{
                    "type": "m.room.history_visibility",
                    "state_key": "",
                    "content": {"history_visibility": "joined"},
                }],
            })),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    let before = scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/send/m.room.message/txn-before"),
            Some(json!({"msgtype": "m.text", "body": "before bob left"})),
        )
        .await;
    before.assert_ok();
    let before_id = before.str_field("event_id").to_string();

    // Bob can see it while he is still joined.
    scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{before_id}"),
            None,
        )
        .await
        .assert_ok();

    scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/leave"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    let after = scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/send/m.room.message/txn-after"),
            Some(json!({"msgtype": "m.text", "body": "after bob left"})),
        )
        .await;
    after.assert_ok();
    let after_id = after.str_field("event_id").to_string();

    // A departed member (not forgotten) may still call /messages at all...
    let page = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/messages?dir=b&limit=20"),
            None,
        )
        .await;
    page.assert_ok();
    let bodies: Vec<&str> = page.json["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    assert!(
        bodies.contains(&"before bob left"),
        "an event sent while bob was joined must remain visible: {bodies:?}"
    );
    assert!(
        !bodies.contains(&"after bob left"),
        "an event sent after bob left a `joined`-visibility room must not be visible: {bodies:?}"
    );

    // ...but the event sent after he left is a 404 by direct ID too, not merely absent from a
    // page (the same "not found, not forbidden" shape `apidoc_room_history_visibility_test.go`
    // expects).
    scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{after_id}"),
            None,
        )
        .await
        .assert_matrix_error(StatusCode::NOT_FOUND, "M_NOT_FOUND");

    // The event sent before he left is still directly fetchable.
    scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{before_id}"),
            None,
        )
        .await
        .assert_ok();
}

/// A user who never joined a non-world-readable room at all gets the same `404` on
/// `GET .../event/{eventId}` as a departed member denied by history-visibility -- and, per
/// `room_messages_test.go`'s "you aren't a member of the room", `403` outright on
/// `GET .../messages` before any per-event filtering runs.
#[tokio::test]
async fn a_stranger_to_a_shared_visibility_room_is_denied_reads() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("eve", "eve", "another passphrase entirely")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    let sent = scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/send/m.room.message/txn1"),
            Some(json!({"msgtype": "m.text", "body": "hello"})),
        )
        .await;
    sent.assert_ok();
    let event_id = sent.str_field("event_id").to_string();

    scenario
        .send(
            Some("eve"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{event_id}"),
            None,
        )
        .await
        .assert_matrix_error(StatusCode::NOT_FOUND, "M_NOT_FOUND");

    scenario
        .send(
            Some("eve"),
            Method::GET,
            &format!("/rooms/{room_id}/messages"),
            None,
        )
        .await
        .assert_matrix_error(StatusCode::FORBIDDEN, "M_FORBIDDEN");
}

/// `POST /rooms/{roomId}/forget`: rejects a still-joined member, rejects a room that does not
/// exist, and -- once forgotten -- blocks `/messages` outright even though the room's
/// `shared`-visibility default would otherwise let a past member read what they saw while joined;
/// rejoining un-forgets it.
#[tokio::test]
async fn forget_validates_membership_and_blocks_messages_until_rejoin() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    // Still joined: rejected.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/forget"),
            Some(json!({})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_UNKNOWN");

    // A room that does not exist at all: also rejected, same shape.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/rooms/!does-not-exist:example.org/forget",
            Some(json!({})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_UNKNOWN");

    scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/send/m.room.message/txn1"),
            Some(json!({"msgtype": "m.text", "body": "hello"})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/leave"),
            Some(json!({})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/forget"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/messages"),
            None,
        )
        .await
        .assert_matrix_error(StatusCode::FORBIDDEN, "M_FORBIDDEN");

    // Rejoining (the room is `public_chat`, so no invite is needed) un-forgets it.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/messages"),
            None,
        )
        .await
        .assert_ok();
}

/// `POST /createRoom` validates `room_version`'s JSON *type* (a number is `M_BAD_JSON`, distinct
/// from a well-typed-but-unrecognized version string, which stays `M_UNSUPPORTED_ROOM_VERSION`),
/// `preset` against the three spec-defined values, `visibility` against `public`/`private`, and
/// `creation_content`'s type.
#[tokio::test]
async fn create_room_validates_request_shape() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();

    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"room_version": 1, "preset": "public_chat"})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_BAD_JSON");

    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"room_version": "not-a-real-version", "preset": "public_chat"})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_UNSUPPORTED_ROOM_VERSION");

    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "not-a-real-preset"})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_BAD_JSON");

    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"visibility": "not-a-real-visibility"})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_BAD_JSON");

    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"creation_content": "not-an-object"})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_BAD_JSON");

    // A well-formed request still works after all that.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat", "room_version": "10"})),
        )
        .await
        .assert_ok();
}

/// `PUT`/`GET /_matrix/client/v3/directory/list/room/{roomId}` end to end, `GET`/`POST
/// /publicRooms`'s listing, and `POST /createRoom`'s own `visibility: "public"` field. Also
/// checks the underlying publish flag directly through
/// [`RoomRegistry::is_directory_public`]/[`RoomRegistry::list_published_room_ids`] -- see
/// `crate::routes::directory`'s module doc for why this crate's `/publicRooms` (not `hs-user`'s)
/// is the one actually mounted in `hs-cli`.
#[tokio::test]
async fn room_directory_publish_and_unpublish_round_trip() {
    let (router, registry) = app_with_registry();
    let mut scenario = Scenario::new(router);
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({
                "preset": "public_chat",
                "name": "Wombat Discussion",
                "topic": "All about wombats",
            })),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();
    let room_id_parsed = <&ruma::RoomId>::try_from(room_id.as_str()).unwrap();

    // Not published yet: `GET .../directory/list/room/{roomId}` and the registry agree.
    let get_visibility = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/directory/list/room/{room_id}"),
            None,
        )
        .await;
    get_visibility.assert_ok();
    assert_eq!(get_visibility.json["visibility"], "private");
    assert!(!registry.is_directory_public(room_id_parsed).unwrap());
    assert!(
        !registry
            .list_published_room_ids()
            .unwrap()
            .iter()
            .any(|id| id.as_str() == room_id)
    );
    let not_listed = scenario
        .send(Some("alice"), Method::GET, "/publicRooms", None)
        .await;
    not_listed.assert_ok();
    assert!(
        !not_listed.json["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["room_id"] == room_id)
    );

    scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/directory/list/room/{room_id}"),
            Some(json!({"visibility": "public"})),
        )
        .await
        .assert_ok();

    let get_visibility_after = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/directory/list/room/{room_id}"),
            None,
        )
        .await;
    get_visibility_after.assert_ok();
    assert_eq!(get_visibility_after.json["visibility"], "public");
    assert!(registry.is_directory_public(room_id_parsed).unwrap());
    assert!(
        registry
            .list_published_room_ids()
            .unwrap()
            .iter()
            .any(|id| id.as_str() == room_id)
    );

    let listed = scenario
        .send(Some("alice"), Method::GET, "/publicRooms", None)
        .await;
    listed.assert_ok();
    let entry = listed.json["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["room_id"] == room_id)
        .cloned()
        .expect("the published room must appear in /publicRooms");
    assert_eq!(entry["name"], "Wombat Discussion");
    assert_eq!(entry["topic"], "All about wombats");
    assert_eq!(entry["num_joined_members"], 1);
    assert_eq!(entry["world_readable"], false);
    assert!(entry.get("guest_can_join").is_some());

    // The `POST` filtered form finds it by a case-insensitive substring of its topic.
    let searched = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/publicRooms",
            Some(json!({"filter": {"generic_search_term": "WOMBATS"}})),
        )
        .await;
    searched.assert_ok();
    let search_chunk = searched.json["chunk"].as_array().unwrap();
    assert_eq!(search_chunk.len(), 1);
    assert_eq!(search_chunk[0]["room_id"], room_id);

    // Unpublishing removes it again.
    scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/directory/list/room/{room_id}"),
            Some(json!({"visibility": "private"})),
        )
        .await
        .assert_ok();
    assert!(!registry.is_directory_public(room_id_parsed).unwrap());

    // `POST /createRoom`'s own `visibility: "public"` field publishes without a separate
    // directory call.
    let created_public = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"visibility": "public", "name": "Born Public"})),
        )
        .await;
    created_public.assert_ok();
    let public_room_id = created_public.str_field("room_id").to_string();
    let public_room_id_parsed = <&ruma::RoomId>::try_from(public_room_id.as_str()).unwrap();
    assert!(registry.is_directory_public(public_room_id_parsed).unwrap());

    // A malformed `visibility` value is `M_BAD_JSON`, not silently treated as private.
    scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/directory/list/room/{room_id}"),
            Some(json!({"visibility": "nonsense"})),
        )
        .await
        .assert_matrix_error(StatusCode::BAD_REQUEST, "M_BAD_JSON");

    // Publishing a room that does not exist 404s rather than silently succeeding.
    scenario
        .send(
            Some("alice"),
            Method::PUT,
            "/directory/list/room/!does-not-exist:example.org",
            Some(json!({"visibility": "public"})),
        )
        .await
        .assert_matrix_error(StatusCode::NOT_FOUND, "M_NOT_FOUND");
}

/// `GET .../state`, `.../state/{eventType}(/{stateKey})` and `.../members` must show a departed
/// member the room **as of when they left**, not its live current state -- the same
/// history-visibility principle `event_visible_to` enforces per-event, applied to these bulk
/// reads. Regression test for exactly the bug `apidoc_room_history_visibility_test.go`'s sibling,
/// `room_leave_test.go`'s `TestLeftRoomFixture`, demonstrated: before this session's fix, a
/// departed member's `GET .../state/{type}` returned whatever the room's state had become by the
/// time they asked, including changes made after they left, and `.../members` included members
/// who joined afterward too.
#[tokio::test]
async fn departed_member_sees_state_and_members_as_of_when_they_left() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();
    scenario
        .register("carol", "carol", "another passphrase")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat", "name": "Before"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/leave"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    // After bob leaves: the name changes, and carol joins.
    scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/state/m.room.name/"),
            Some(json!({"name": "After"})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("carol"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    // Bob still sees "Before", both via the single-key and full-state endpoints...
    let name_with_key = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/state/m.room.name/"),
            None,
        )
        .await;
    name_with_key.assert_ok();
    assert_eq!(name_with_key.json["name"], "Before");

    let full_state = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/state"),
            None,
        )
        .await;
    full_state.assert_ok();
    let name_event = full_state
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "m.room.name")
        .expect("m.room.name must be in the departed member's state snapshot");
    assert_eq!(name_event["content"]["name"], "Before");

    // ...and does not see carol, who joined after bob left.
    let members = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/members"),
            None,
        )
        .await;
    members.assert_ok();
    let member_ids: Vec<&str> = members.json["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["state_key"].as_str().unwrap())
        .collect();
    assert!(member_ids.contains(&"@alice:example.org"));
    assert!(
        !member_ids.contains(&"@carol:example.org"),
        "a departed member must not see a member who joined after they left: {member_ids:?}"
    );

    // Meanwhile alice, still joined, sees the live state.
    let alice_name = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/state/m.room.name/"),
            None,
        )
        .await;
    alice_name.assert_ok();
    assert_eq!(alice_name.json["name"], "After");
}

/// `GET /rooms/{roomId}/event/{eventId}` carries `unsigned.transaction_id` back to the sending
/// user, per `txnid_test.go`'s `TestTxnInEvent`, but not to a different user reading the same
/// event -- the client-server API's local-echo field is scoped to the sender, not public.
#[tokio::test]
async fn transaction_id_is_echoed_back_to_the_sender_only() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();

    let created = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat"})),
        )
        .await;
    created.assert_ok();
    let room_id = created.str_field("room_id").to_string();

    scenario
        .send(
            Some("bob"),
            Method::POST,
            &format!("/rooms/{room_id}/join"),
            Some(json!({})),
        )
        .await
        .assert_ok();

    let sent = scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/rooms/{room_id}/send/m.room.message/my-txn-id"),
            Some(json!({"msgtype": "m.text", "body": "hello"})),
        )
        .await;
    sent.assert_ok();
    let event_id = sent.str_field("event_id").to_string();

    let seen_by_sender = scenario
        .send(
            Some("alice"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{event_id}"),
            None,
        )
        .await;
    seen_by_sender.assert_ok();
    assert_eq!(
        seen_by_sender.json["unsigned"]["transaction_id"], "my-txn-id",
        "the sender must see its own transaction id echoed back"
    );

    let seen_by_bob = scenario
        .send(
            Some("bob"),
            Method::GET,
            &format!("/rooms/{room_id}/event/{event_id}"),
            None,
        )
        .await;
    seen_by_bob.assert_ok();
    assert!(
        seen_by_bob.json["unsigned"]["transaction_id"].is_null(),
        "a different user must never see another user's transaction id: {:?}",
        seen_by_bob.json["unsigned"]
    );
}
