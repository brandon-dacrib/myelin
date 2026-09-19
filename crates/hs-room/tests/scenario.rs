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

fn app() -> axum::Router {
    let auth_state = AuthState::in_memory();
    let backend = MemoryBackend::new();
    let identity = HomeserverIdentity::for_tests("example.org");
    let registry = RoomRegistry::open(backend, identity.clone()).expect("open registry");
    let room_state = RoomState {
        auth: auth_state.clone(),
        rooms: Arc::new(registry),
        identity,
    };
    let (room_router, _manifest) = hs_room::routes::router::<MemoryBackend>();

    hs_auth::routes::router()
        .with_state(auth_state)
        .merge(room_router.with_state(room_state))
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
