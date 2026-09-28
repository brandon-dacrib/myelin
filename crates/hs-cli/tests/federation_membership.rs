//! Invites, leaves and knocks between two real `hs serve` instances in one process, federating
//! over plain HTTP -- the same harness as `federation_two_servers.rs` (see its module docs for
//! the server-name and port choices), for the membership changes that a join alone does not
//! cover:
//!
//! - alice on A invites bob on B: A sends the invite to B (`PUT /invite`), B co-signs it, and
//!   bob's `/sync` on B shows it with the room's stripped state; bob joins through A and they
//!   talk;
//! - bob rejects an invite to a room B is not in: `make_leave`/`send_leave` through A, and A
//!   sees him leave;
//! - alice rescinds an invite: the leave reaches B over `/send` although B is not in the room;
//! - bob knocks on alice's room (`make_knock`/`send_knock`), his `/sync` shows the knock, alice
//!   accepts it by inviting him, and he joins;
//! - alice refuses a knock, and bob's `/sync` shows it.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
    base: String,
    name: String,
    _dir: tempfile::TempDir,
}

async fn start() -> Server {
    let port = reserve_port();
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        config(port, dir.path()),
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots");
    Server {
        base: handle.base_url(),
        name: format!("127.0.0.1:{port}"),
        handle,
        _dir: dir,
    }
}

/// A user on a server: what every request below needs.
struct User {
    id: String,
    token: String,
    base: String,
}

/// Registers `username` through the real UIA dance.
async fn register(client: &reqwest::Client, server: &Server, username: &str) -> User {
    let base = &server.base;
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"]
        .as_str()
        .unwrap_or_else(|| panic!("registration did not offer a UIA session: {first}"));
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    User {
        id: done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        token: done["access_token"].as_str().unwrap().to_owned(),
        base: base.clone(),
    }
}

/// `POST`s `body` to `path` as `user`; the status and the JSON answer.
async fn post(client: &reqwest::Client, user: &User, path: &str, body: Value) -> (u16, Value) {
    let response = client
        .post(format!("{}/_matrix/client/v3/{path}", user.base))
        .bearer_auth(&user.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// `PUT`s `body` to `path` as `user`; the status and the JSON answer.
async fn put(client: &reqwest::Client, user: &User, path: &str, body: Value) -> (u16, Value) {
    let response = client
        .put(format!("{}/_matrix/client/v3/{path}", user.base))
        .bearer_auth(&user.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn create_room(client: &reqwest::Client, user: &User, body: Value) -> String {
    let (status, created) = post(client, user, "createRoom", body).await;
    assert_eq!(status, 200, "createRoom failed: {created}");
    created["room_id"].as_str().unwrap().to_owned()
}

async fn send_message(client: &reqwest::Client, user: &User, room_id: &str, body: &str) {
    let txn = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let response: Value = client
        .put(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/txn-{txn}",
            user.base
        ))
        .bearer_auth(&user.token)
        .json(&json!({"msgtype": "m.text", "body": body}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(response["event_id"].is_string(), "send failed: {response}");
}

/// Initial syncs (left rooms included) until `wanted` is true of the response, or fifteen
/// seconds have passed: federation delivery and the session hub both run off background tasks.
async fn sync_until(
    client: &reqwest::Client,
    user: &User,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let response: Value = client
            .get(format!("{}/_matrix/client/v3/sync", user.base))
            .query(&[
                ("timeout", "500"),
                ("filter", r#"{"room":{"include_leave":true}}"#),
            ])
            .bearer_auth(&user.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if wanted(&response) {
            return response;
        }
        last = response;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the sync never said what was expected; the last one said: {last}");
}

fn timeline_bodies(sync: &Value, room_id: &str) -> Vec<String> {
    sync["rooms"]["join"][room_id]["timeline"]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `user`'s membership in `room_id` as the server `viewer` is on holds it, read through
/// `viewer`'s own `/members` (every membership, not only joins).
async fn membership_on(
    client: &reqwest::Client,
    viewer: &User,
    room_id: &str,
    user: &str,
) -> Option<String> {
    let members: Value = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/members",
            viewer.base
        ))
        .bearer_auth(&viewer.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    members["chunk"].as_array()?.iter().find_map(|event| {
        (event["state_key"] == user)
            .then(|| event["content"]["membership"].as_str().map(str::to_owned))
            .flatten()
    })
}

/// Waits until `membership_on` says `wanted`.
async fn wait_for_membership(
    client: &reqwest::Client,
    viewer: &User,
    room_id: &str,
    user: &str,
    wanted: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = None;
    while Instant::now() < deadline {
        last = membership_on(client, viewer, room_id, user).await;
        if last.as_deref() == Some(wanted) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{user}'s membership in {room_id} never became {wanted}; it is {last:?}");
}

/// The `(type, state_key)` of every event in a stripped-state list, and the content of the one
/// named `event_type`/`state_key`.
fn stripped<'a>(events: &'a Value, event_type: &str, state_key: &str) -> Option<&'a Value> {
    events.as_array()?.iter().find_map(|e| {
        (e["type"] == event_type && e["state_key"] == state_key).then_some(&e["content"])
    })
}

/// Invites through federation: the invite in bob's `/sync` on B, with the room's stripped
/// state; his join through the inviting server; messages both ways.
#[tokio::test]
async fn an_invite_from_another_server_reaches_the_invitee_who_joins_through_it() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;

    let room_id = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "name": "by invitation", "room_version": "11"}),
    )
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{room_id}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "the invite failed: {body}");
    assert_eq!(
        membership_on(&client, &alice, &room_id, &bob.id)
            .await
            .as_deref(),
        Some("invite"),
        "A holds the invite as soon as the request returns"
    );

    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&room_id).is_some()
    })
    .await;
    let invite_state = &sync["rooms"]["invite"][&room_id]["invite_state"]["events"];
    assert_eq!(
        stripped(invite_state, "m.room.member", &bob.id).map(|c| c["membership"].clone()),
        Some(json!("invite")),
        "bob's own invite: {invite_state}"
    );
    assert_eq!(
        stripped(invite_state, "m.room.name", "").map(|c| c["name"].clone()),
        Some(json!("by invitation")),
        "the room's name, from the stripped state A sent: {invite_state}"
    );
    assert!(
        stripped(invite_state, "m.room.create", "").is_some(),
        "{invite_state}"
    );
    assert_eq!(
        stripped(invite_state, "m.room.member", &alice.id).map(|c| c["membership"].clone()),
        Some(json!("join")),
        "who invited him: {invite_state}"
    );
    assert!(
        sync["rooms"]["join"].get(&room_id).is_none(),
        "an invite is not a join: {sync}"
    );

    // Bob accepts the way a client does: a join naming no server. B is not in the room, so it
    // goes through the server that invited him.
    let (status, body) = post(&client, &bob, &format!("join/{room_id}"), json!({})).await;
    assert_eq!(status, 200, "the join failed: {body}");
    wait_for_membership(&client, &alice, &room_id, &bob.id, "join").await;
    sync_until(&client, &bob, |s| {
        s["rooms"]["join"].get(&room_id).is_some()
    })
    .await;

    send_message(&client, &bob, &room_id, "thanks for the invite").await;
    sync_until(&client, &alice, |s| {
        timeline_bodies(s, &room_id).contains(&"thanks for the invite".to_owned())
    })
    .await;
    send_message(&client, &alice, &room_id, "welcome in").await;
    sync_until(&client, &bob, |s| {
        timeline_bodies(s, &room_id).contains(&"welcome in".to_owned())
    })
    .await;

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// Rejecting an invite to a room B is not in goes through A (`make_leave`/`send_leave`); A sees
/// bob leave and bob's `/sync` moves the room to `leave`. Then alice rescinds a second invite,
/// and that leave reaches B although B holds nothing of the room but the invite.
#[tokio::test]
async fn an_invite_is_rejected_by_the_invitee_and_rescinded_by_the_inviter_across_servers() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;

    let rejected = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "name": "declined", "room_version": "11"}),
    )
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{rejected}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&rejected).is_some()
    })
    .await;

    let (status, body) = post(
        &client,
        &bob,
        &format!("rooms/{rejected}/leave"),
        json!({"reason": "no thanks"}),
    )
    .await;
    assert_eq!(status, 200, "rejecting the invite failed: {body}");
    // A accepted the leave before bob's request returned.
    assert_eq!(
        membership_on(&client, &alice, &rejected, &bob.id)
            .await
            .as_deref(),
        Some("leave")
    );
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["leave"].get(&rejected).is_some()
    })
    .await;
    assert!(
        sync["rooms"]["invite"].get(&rejected).is_none(),
        "a rejected invite is not still an invite: {sync}"
    );

    let rescinded = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "name": "changed my mind", "room_version": "11"}),
    )
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{rescinded}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&rescinded).is_some()
    })
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{rescinded}/kick"),
        json!({"user_id": bob.id, "reason": "sorry"}),
    )
    .await;
    assert_eq!(status, 200, "rescinding the invite failed: {body}");
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["leave"].get(&rescinded).is_some()
    })
    .await;
    assert!(
        sync["rooms"]["invite"].get(&rescinded).is_none(),
        "a rescinded invite is not still an invite: {sync}"
    );

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// Knocking through federation: bob's knock reaches A and his own `/sync` (with the stripped
/// state A answered with); alice accepts by inviting him; he joins. A second knock is refused,
/// and bob's `/sync` says so.
#[tokio::test]
async fn a_knock_from_another_server_is_accepted_and_one_is_refused() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;
    let knock_room = |name: &str| {
        json!({
            "preset": "private_chat",
            "name": name,
            "room_version": "11",
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {"join_rule": "knock"},
            }],
        })
    };

    let accepted = create_room(&client, &alice, knock_room("knock first")).await;
    let (status, body) = post(
        &client,
        &bob,
        &format!("knock/{accepted}?server_name={}", a.name),
        json!({"reason": "let me in"}),
    )
    .await;
    assert_eq!(status, 200, "the knock failed: {body}");
    assert_eq!(body["room_id"], accepted);
    assert_eq!(
        membership_on(&client, &alice, &accepted, &bob.id)
            .await
            .as_deref(),
        Some("knock"),
        "A holds the knock as soon as the request returns"
    );
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["knock"].get(&accepted).is_some()
    })
    .await;
    let knock_state = &sync["rooms"]["knock"][&accepted]["knock_state"]["events"];
    assert_eq!(
        stripped(knock_state, "m.room.name", "").map(|c| c["name"].clone()),
        Some(json!("knock first")),
        "the room's name, from the stripped state A answered with: {knock_state}"
    );
    assert_eq!(
        stripped(knock_state, "m.room.member", &bob.id).map(|c| c["membership"].clone()),
        Some(json!("knock")),
        "{knock_state}"
    );
    // Knocking again is allowed (Complement's "A user that has already knocked is allowed to
    // knock again on the same room").
    let (status, body) = post(
        &client,
        &bob,
        &format!("knock/{accepted}?server_name={}", a.name),
        json!({"reason": "still here"}),
    )
    .await;
    assert_eq!(status, 200, "the second knock failed: {body}");

    // Alice lets him in by inviting him; the invite replaces the knock on B.
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{accepted}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "accepting the knock failed: {body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&accepted).is_some()
            && s["rooms"]["knock"].get(&accepted).is_none()
    })
    .await;
    let (status, body) = post(&client, &bob, &format!("join/{accepted}"), json!({})).await;
    assert_eq!(status, 200, "the join failed: {body}");
    send_message(&client, &bob, &accepted, "knock knock").await;
    sync_until(&client, &alice, |s| {
        timeline_bodies(s, &accepted).contains(&"knock knock".to_owned())
    })
    .await;

    let refused = create_room(&client, &alice, knock_room("not today")).await;
    let (status, body) = post(
        &client,
        &bob,
        &format!("knock/{refused}?server_name={}", a.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "the knock failed: {body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["knock"].get(&refused).is_some()
    })
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{refused}/kick"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "refusing the knock failed: {body}");
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["leave"].get(&refused).is_some()
    })
    .await;
    assert!(sync["rooms"]["knock"].get(&refused).is_none(), "{sync}");

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// `createRoom`'s `invite` list names a user of another server: the invitation goes out over
/// `PUT /invite` like any other, so it reaches bob's `/sync` on B (with `is_direct`, since this
/// is a direct chat) and he can join through A.
#[tokio::test]
async fn a_create_room_invite_list_invites_a_user_of_another_server() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;
    let carol = register(&client, &a, "carol").await;

    let room_id = create_room(
        &client,
        &alice,
        json!({
            "preset": "trusted_private_chat",
            "is_direct": true,
            "name": "a direct chat",
            "room_version": "11",
            "invite": [bob.id, carol.id],
        }),
    )
    .await;
    assert_eq!(
        membership_on(&client, &alice, &room_id, &bob.id)
            .await
            .as_deref(),
        Some("invite"),
        "A holds bob's invite as soon as createRoom returns"
    );
    assert_eq!(
        membership_on(&client, &alice, &room_id, &carol.id)
            .await
            .as_deref(),
        Some("invite"),
        "the local invitee is invited as before"
    );

    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&room_id).is_some()
    })
    .await;
    let invite_state = &sync["rooms"]["invite"][&room_id]["invite_state"]["events"];
    let own = stripped(invite_state, "m.room.member", &bob.id)
        .unwrap_or_else(|| panic!("bob's own invite is in the invite state: {invite_state}"));
    assert_eq!(own["membership"], "invite");
    assert_eq!(own["is_direct"], true, "{invite_state}");
    assert_eq!(
        stripped(invite_state, "m.room.name", "").map(|c| c["name"].clone()),
        Some(json!("a direct chat")),
        "{invite_state}"
    );

    let (status, body) = post(&client, &bob, &format!("join/{room_id}"), json!({})).await;
    assert_eq!(status, 200, "the join failed: {body}");
    wait_for_membership(&client, &alice, &room_id, &bob.id, "join").await;
    send_message(&client, &bob, &room_id, "invited at birth").await;
    sync_until(&client, &alice, |s| {
        timeline_bodies(s, &room_id).contains(&"invited at birth".to_owned())
    })
    .await;

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// A restricted room (`join_rule: restricted`, room version 8+) that `owner` creates, joinable
/// from `lobby`, and `lobby` itself, public.
async fn restricted_room_and_lobby(client: &reqwest::Client, owner: &User) -> (String, String) {
    let lobby = create_room(
        client,
        owner,
        json!({"preset": "public_chat", "name": "lobby", "room_version": "11"}),
    )
    .await;
    let restricted = create_room(
        client,
        owner,
        json!({
            "preset": "private_chat",
            "name": "members only",
            "room_version": "11",
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {
                    "join_rule": "restricted",
                    "allow": [{"type": "m.room_membership", "room_id": lobby}],
                },
            }],
        }),
    )
    .await;
    (lobby, restricted)
}

/// The content of `user`'s `m.room.member` event in `room_id`, read by `viewer`.
async fn member_content(
    client: &reqwest::Client,
    viewer: &User,
    room_id: &str,
    user: &str,
) -> Value {
    client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/state/m.room.member/{user}",
            viewer.base
        ))
        .bearer_auth(&viewer.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Restricted joins over federation, both ways round. A user of the other server who is not in
/// the allowed room is refused; once in it, the resident authorises the join by naming one of
/// its own users in `join_authorised_via_users_server` and co-signing it, and the joining
/// server keeps the co-signed event. A resident that is in none of the allowed rooms says
/// `M_UNABLE_TO_AUTHORISE_JOIN`.
#[tokio::test]
async fn a_restricted_room_is_joined_through_a_resident_that_authorises_it() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;

    // Bob on B joins alice's restricted room on A.
    let (lobby, restricted) = restricted_room_and_lobby(&client, &alice).await;
    let (status, body) = post(&client, &bob, &format!("join/{restricted}"), json!({})).await;
    assert_eq!(
        status, 403,
        "bob is in no room the join rules allow: {body}"
    );
    let (status, body) = post(&client, &bob, &format!("join/{lobby}"), json!({})).await;
    assert_eq!(status, 200, "joining the lobby failed: {body}");
    let (status, body) = post(&client, &bob, &format!("join/{restricted}"), json!({})).await;
    assert_eq!(status, 200, "the restricted join failed: {body}");
    wait_for_membership(&client, &alice, &restricted, &bob.id, "join").await;
    let on_a = member_content(&client, &alice, &restricted, &bob.id).await;
    assert_eq!(
        on_a["join_authorised_via_users_server"], alice.id,
        "A named its own user as the authoriser: {on_a}"
    );
    let on_b = member_content(&client, &bob, &restricted, &bob.id).await;
    assert_eq!(on_b["membership"], "join", "B holds bob's join: {on_b}");
    send_message(&client, &bob, &restricted, "let in through the lobby").await;
    sync_until(&client, &alice, |s| {
        timeline_bodies(s, &restricted).contains(&"let in through the lobby".to_owned())
    })
    .await;

    // Alice leaves and comes back through B, which now authorises her (bob may invite, and
    // she is in the lobby). B's answer carries the room's state, bob's join among it, and A
    // checks that join for its own signature: B has to have kept the co-signed copy.
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{restricted}/leave"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    wait_for_membership(&client, &bob, &restricted, &alice.id, "leave").await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("join/{restricted}?server_name={}", b.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "rejoining through B failed: {body}");
    let on_a = member_content(&client, &alice, &restricted, &alice.id).await;
    assert_eq!(on_a["join_authorised_via_users_server"], bob.id, "{on_a}");
    wait_for_membership(&client, &bob, &restricted, &alice.id, "join").await;

    // The other way round: alice on A joins bob's restricted room on B.
    let (bobs_lobby, bobs_restricted) = restricted_room_and_lobby(&client, &bob).await;
    let (status, body) = post(&client, &alice, &format!("join/{bobs_lobby}"), json!({})).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        &client,
        &alice,
        &format!("join/{bobs_restricted}"),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "the restricted join failed: {body}");
    wait_for_membership(&client, &bob, &bobs_restricted, &alice.id, "join").await;
    let on_b = member_content(&client, &bob, &bobs_restricted, &alice.id).await;
    assert_eq!(on_b["join_authorised_via_users_server"], bob.id, "{on_b}");
    send_message(&client, &alice, &bobs_restricted, "and back").await;
    sync_until(&client, &bob, |s| {
        timeline_bodies(s, &bobs_restricted).contains(&"and back".to_owned())
    })
    .await;

    // A room whose join rules allow only a room A is not in: A cannot vouch for anybody.
    let elsewhere = format!("!elsewhere:{}", b.name);
    let unvouched = create_room(
        &client,
        &alice,
        json!({
            "preset": "private_chat",
            "room_version": "11",
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {
                    "join_rule": "restricted",
                    "allow": [{"type": "m.room_membership", "room_id": elsewhere}],
                },
            }],
        }),
    )
    .await;
    let (status, body) = post(&client, &bob, &format!("join/{unvouched}"), json!({})).await;
    assert_eq!(status, 502, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("M_UNABLE_TO_AUTHORISE_JOIN"),
        "A said it could not authorise the join: {error}"
    );

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// The inviting server is gone: bob's rejection cannot go through any server in the room, so B
/// rejects the invite alone, and his `/sync` moves the room to `leave`. A knock that no server
/// can take fails with a message that names the knock, not a join.
#[tokio::test]
async fn an_invite_is_rejected_locally_when_no_server_in_the_room_answers() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;

    let room_id = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "name": "soon gone", "room_version": "11"}),
    )
    .await;
    let knock_room = create_room(
        &client,
        &alice,
        json!({
            "preset": "private_chat",
            "room_version": "11",
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {"join_rule": "knock"},
            }],
        }),
    )
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{room_id}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&room_id).is_some()
    })
    .await;

    let a_name = a.name.clone();
    a.handle.shutdown().await;

    let (status, body) = post(
        &client,
        &bob,
        &format!("rooms/{room_id}/leave"),
        json!({"reason": "nobody home"}),
    )
    .await;
    assert_eq!(status, 200, "rejecting the invite failed: {body}");
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["leave"].get(&room_id).is_some()
    })
    .await;
    assert!(
        sync["rooms"]["invite"].get(&room_id).is_none(),
        "a rejected invite is not still an invite: {sync}"
    );
    let leave = sync["rooms"]["leave"][&room_id]["timeline"]["events"]
        .as_array()
        .and_then(|events| {
            events
                .iter()
                .rfind(|e| e["type"] == "m.room.member" && e["state_key"] == bob.id)
        })
        .cloned()
        .unwrap_or_else(|| panic!("bob's leave is in his timeline: {sync}"));
    assert_eq!(leave["content"]["membership"], "leave");
    assert_eq!(leave["content"]["reason"], "nobody home");
    assert_eq!(leave["sender"], bob.id);

    // Rejecting again: nothing left to reject, and no server to ask.
    let (status, _) = post(&client, &bob, &format!("rooms/{room_id}/leave"), json!({})).await;
    assert_ne!(status, 200, "a second rejection has nothing to reject");

    let (status, body) = post(
        &client,
        &bob,
        &format!("knock/{knock_room}?server_name={a_name}"),
        json!({}),
    )
    .await;
    assert_eq!(status, 502, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("knock"),
        "the error names the knock: {error}"
    );
    assert!(
        !error.contains("join"),
        "the error does not call a knock a join: {error}"
    );

    b.handle.shutdown().await;
}

/// A user of the server that holds a restricted room joins it without naming an authoriser,
/// as a client does: the server picks one the way `make_join` does for a user of another
/// server. On A alone, carol is let in once she is in the lobby, with alice named; out of the
/// lobby, she is refused again. In a restricted room where no user of B may invite, a join by
/// dave on B -- whose server is in the room -- goes through A instead, and A names alice.
#[tokio::test]
async fn a_local_user_joins_a_restricted_room_without_naming_an_authoriser() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let carol = register(&client, &a, "carol").await;
    let bob = register(&client, &b, "bob").await;
    let dave = register(&client, &b, "dave").await;

    // A local join, authorised locally.
    let (lobby, restricted) = restricted_room_and_lobby(&client, &alice).await;
    let (status, body) = post(&client, &carol, &format!("join/{restricted}"), json!({})).await;
    assert_eq!(
        status, 403,
        "carol is in no room the join rules allow: {body}"
    );
    let (status, body) = post(&client, &carol, &format!("join/{lobby}"), json!({})).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&client, &carol, &format!("join/{restricted}"), json!({})).await;
    assert_eq!(status, 200, "the local restricted join failed: {body}");
    let content = member_content(&client, &alice, &restricted, &carol.id).await;
    assert_eq!(content["membership"], "join", "{content}");
    assert_eq!(
        content["join_authorised_via_users_server"], alice.id,
        "A named its own user who may invite: {content}"
    );
    // Joining again (a profile change) needs no authoriser.
    let (status, body) = post(
        &client,
        &carol,
        &format!("rooms/{restricted}/join"),
        json!({"displayname": "Carol"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // A client's own `join_authorised_via_users_server` is dropped, never checked (Synapse's
    // `update_membership`; Complement's "Join should succeed when joined to allowed room").
    let (status, body) = put(
        &client,
        &carol,
        &format!("rooms/{restricted}/state/m.room.member/{}", carol.id),
        json!({"membership": "join", "displayname": "Carol", "join_authorised_via_users_server": "unused"}),
    )
    .await;
    assert_eq!(
        status, 200,
        "a profile change naming a bogus authoriser: {body}"
    );
    let content = member_content(&client, &alice, &restricted, &carol.id).await;
    assert!(
        content.get("join_authorised_via_users_server").is_none(),
        "the client's value is not kept: {content}"
    );
    for room in [&restricted, &lobby] {
        let (status, body) = post(&client, &carol, &format!("rooms/{room}/leave"), json!({})).await;
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) = post(&client, &carol, &format!("join/{restricted}"), json!({})).await;
    assert_eq!(
        status, 403,
        "out of the lobby, carol is refused again: {body}"
    );

    // A restricted room on A in which only alice may invite; bob (B) is invited in, so B is
    // in the room but cannot vouch for anybody.
    let guarded = create_room(
        &client,
        &alice,
        json!({
            "preset": "private_chat",
            "room_version": "11",
            "power_level_content_override": {"invite": 50},
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {
                    "join_rule": "restricted",
                    "allow": [{"type": "m.room_membership", "room_id": lobby}],
                },
            }],
        }),
    )
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{guarded}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&guarded).is_some()
    })
    .await;
    let (status, body) = post(&client, &bob, &format!("join/{guarded}"), json!({})).await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = post(&client, &dave, &format!("rooms/{guarded}/join"), json!({})).await;
    assert_eq!(
        status, 403,
        "dave is in no room the join rules allow: {body}"
    );
    let (status, body) = post(&client, &dave, &format!("join/{lobby}"), json!({})).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(&client, &dave, &format!("rooms/{guarded}/join"), json!({})).await;
    assert_eq!(status, 200, "the join through A failed: {body}");
    wait_for_membership(&client, &alice, &guarded, &dave.id, "join").await;
    let content = member_content(&client, &alice, &guarded, &dave.id).await;
    assert_eq!(
        content["join_authorised_via_users_server"], alice.id,
        "{content}"
    );
    wait_for_membership(&client, &bob, &guarded, &dave.id, "join").await;
    send_message(&client, &dave, &guarded, "in through A").await;
    sync_until(&client, &alice, |s| {
        timeline_bodies(s, &guarded).contains(&"in through A".to_owned())
    })
    .await;

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// A knock on a room whose version has no knocking (before 7) is the room refusing it: A
/// answers `make_knock` with `403 M_FORBIDDEN`, as Synapse does, and bob's client on B is
/// told `403` rather than that the other server could not be reached.
#[tokio::test]
async fn a_knock_on_a_room_version_without_knocking_is_forbidden() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;
    let old = create_room(
        &client,
        &alice,
        json!({
            "preset": "private_chat",
            "room_version": "6",
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {"join_rule": "knock"},
            }],
        }),
    )
    .await;

    let (status, body) = post(
        &client,
        &bob,
        &format!("knock/{old}?server_name={}", a.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["errcode"], "M_FORBIDDEN", "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("does not support knocking"),
        "{body}"
    );
    assert_eq!(membership_on(&client, &alice, &old, &bob.id).await, None);

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// Every event in `events` (an array of client events) that carries stripped state in its
/// `unsigned`, by event ID.
fn with_stripped_state(events: &Value) -> Vec<String> {
    events
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| {
            e["unsigned"].get("invite_room_state").is_some()
                || e["unsigned"].get("knock_room_state").is_some()
        })
        .map(|e| e["event_id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The stripped state an invite or knock from another server arrived with is how B describes
/// the room in bob's `/sync` `invite` and `knock` sections, and nothing else: once he is in the
/// room, the invite and the knock in his timeline, `/messages` and `/event` are the events as
/// they are, without it.
#[tokio::test]
async fn stripped_state_stays_out_of_the_timeline() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;

    // An invite: its stripped state is in the invite section, then gone from the events.
    let invited = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "name": "by invitation", "room_version": "11"}),
    )
    .await;
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{invited}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&invited).is_some()
    })
    .await;
    let invite_state = &sync["rooms"]["invite"][&invited]["invite_state"]["events"];
    assert_eq!(
        stripped(invite_state, "m.room.name", "").map(|c| c["name"].clone()),
        Some(json!("by invitation")),
        "{invite_state}"
    );

    // A knock: the same for the knock section.
    let knocked = create_room(
        &client,
        &alice,
        json!({
            "preset": "private_chat",
            "name": "knock first",
            "room_version": "11",
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {"join_rule": "knock"},
            }],
        }),
    )
    .await;
    let (status, body) = post(
        &client,
        &bob,
        &format!("knock/{knocked}?server_name={}", a.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["knock"].get(&knocked).is_some()
    })
    .await;
    let knock_state = &sync["rooms"]["knock"][&knocked]["knock_state"]["events"];
    assert_eq!(
        stripped(knock_state, "m.room.name", "").map(|c| c["name"].clone()),
        Some(json!("knock first")),
        "{knock_state}"
    );
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{knocked}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&knocked).is_some()
    })
    .await;

    for room in [&invited, &knocked] {
        let (status, body) = post(&client, &bob, &format!("join/{room}"), json!({})).await;
        assert_eq!(status, 200, "{body}");
    }
    let sync = sync_until(&client, &bob, |s| {
        s["rooms"]["join"].get(&invited).is_some() && s["rooms"]["join"].get(&knocked).is_some()
    })
    .await;
    for room in [&invited, &knocked] {
        let timeline = &sync["rooms"]["join"][room]["timeline"]["events"];
        assert_eq!(
            with_stripped_state(timeline),
            Vec::<String>::new(),
            "{timeline}"
        );
        let messages: Value = client
            .get(format!(
                "{}/_matrix/client/v3/rooms/{room}/messages?dir=b&limit=50",
                bob.base
            ))
            .bearer_auth(&bob.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let chunk = &messages["chunk"];
        assert!(
            chunk
                .as_array()
                .is_some_and(|c| c.iter().any(|e| e["content"]["membership"] == "invite")),
            "the invite is in bob's history: {messages}"
        );
        assert_eq!(
            with_stripped_state(chunk),
            Vec::<String>::new(),
            "{messages}"
        );
        for event in chunk.as_array().into_iter().flatten() {
            let event_id = event["event_id"].as_str().unwrap_or_default();
            let single: Value = client
                .get(format!(
                    "{}/_matrix/client/v3/rooms/{room}/event/{event_id}",
                    bob.base
                ))
                .bearer_auth(&bob.token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(
                with_stripped_state(&json!([single])),
                Vec::<String>::new(),
                "{single}"
            );
        }
    }

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// Every server bob's client names refuses his restricted join with
/// `M_UNABLE_TO_AUTHORISE_JOIN` (C is in the room but not in the lobby it allows); B then asks
/// the servers of the lobby, which it knows from the stripped state bob's knock came back with,
/// and A, which is in both, authorises the join. Three servers: with two, the only server to
/// ask and the only one to fall back to would be the same.
#[tokio::test]
async fn a_restricted_join_nobody_asked_can_authorise_goes_to_the_allowed_rooms_servers() {
    let (a, b, c) = (start().await, start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;
    let carol = register(&client, &c, "carol").await;

    // Version 12: room IDs name no server, so nothing but the client's `via` says where to go.
    let lobby = create_room(
        &client,
        &alice,
        json!({"preset": "public_chat", "name": "lobby", "room_version": "12"}),
    )
    .await;
    let room = create_room(
        &client,
        &alice,
        json!({
            "preset": "private_chat",
            "name": "members only",
            "room_version": "12",
            "initial_state": [{
                "type": "m.room.join_rules",
                "state_key": "",
                "content": {
                    "join_rule": "knock_restricted",
                    "allow": [{"type": "m.room_membership", "room_id": lobby, "via": [a.name]}],
                },
            }],
        }),
    )
    .await;
    // Carol (C) is invited in: C is in the room, and in no room its join rules allow.
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{room}/invite"),
        json!({"user_id": carol.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    sync_until(&client, &carol, |s| {
        s["rooms"]["invite"].get(&room).is_some()
    })
    .await;
    let (status, body) = post(
        &client,
        &carol,
        &format!("join/{room}?server_name={}", a.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // Bob knocks through C, which is how B learns the room's join rules.
    let (status, body) = post(
        &client,
        &bob,
        &format!("knock/{room}?server_name={}", c.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "the knock through C failed: {body}");
    let (status, body) = post(
        &client,
        &bob,
        &format!("join/{lobby}?server_name={}", a.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // Joining through C alone: C cannot vouch for him, the lobby's server can.
    let (status, body) = post(
        &client,
        &bob,
        &format!("join/{room}?server_name={}", c.name),
        json!({}),
    )
    .await;
    assert_eq!(status, 200, "the restricted join failed: {body}");
    wait_for_membership(&client, &alice, &room, &bob.id, "join").await;
    let content = member_content(&client, &alice, &room, &bob.id).await;
    assert_eq!(
        content["join_authorised_via_users_server"], alice.id,
        "{content}"
    );
    wait_for_membership(&client, &carol, &room, &bob.id, "join").await;
    send_message(&client, &bob, &room, "through the lobby's server").await;
    sync_until(&client, &carol, |s| {
        timeline_bodies(s, &room).contains(&"through the lobby's server".to_owned())
    })
    .await;

    a.handle.shutdown().await;
    b.handle.shutdown().await;
    c.handle.shutdown().await;
}

/// A room version 12 room (MSC4291: the room ID is the create event's ID, the create event
/// carries no `room_id` and no event cites it in `auth_events`) crosses servers: bob on B is
/// invited, joins through A, and each side sees the other's messages. Without the implied
/// create event, B refused the join's snapshot (a create event "for room <none>") and A refused
/// B's events (no create event in the state their `auth_events` imply).
#[tokio::test]
async fn a_version_12_room_is_joined_and_used_across_servers() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let bob = register(&client, &b, "bob").await;

    let room_id = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "name": "hashed", "room_version": "12"}),
    )
    .await;
    assert!(
        !room_id.contains(':'),
        "a version 12 room ID names no server: {room_id}"
    );
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{room_id}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "the invite failed: {body}");
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&room_id).is_some()
    })
    .await;
    let (status, body) = post(&client, &bob, &format!("join/{room_id}"), json!({})).await;
    assert_eq!(status, 200, "the join failed: {body}");
    wait_for_membership(&client, &alice, &room_id, &bob.id, "join").await;

    send_message(&client, &bob, &room_id, "from B").await;
    sync_until(&client, &alice, |s| {
        timeline_bodies(s, &room_id).contains(&"from B".to_owned())
    })
    .await;
    send_message(&client, &alice, &room_id, "from A").await;
    sync_until(&client, &bob, |s| {
        timeline_bodies(s, &room_id).contains(&"from A".to_owned())
    })
    .await;

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// Only the inviter can rescind an invite over federation: B cannot check the room's power
/// levels, so a kick of bob's invite by anyone else in the room is not shown to him, and he is
/// still invited (Synapse's rule; Complement's "Non-invitee user cannot rescind invite over
/// federation"). The inviter's own rescission still reaches him.
#[tokio::test]
async fn only_the_inviter_rescinds_an_invite_across_servers() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let alice2 = register(&client, &a, "alice2").await;
    let bob = register(&client, &b, "bob").await;

    // A room bob is in, to know when B has seen what A sent after the kick.
    let shared = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "invite": [bob.id], "room_version": "11"}),
    )
    .await;
    sync_until(&client, &bob, |s| {
        s["rooms"]["invite"].get(&shared).is_some()
    })
    .await;
    let (status, body) = post(&client, &bob, &format!("join/{shared}"), json!({})).await;
    assert_eq!(status, 200, "{body}");

    let room = create_room(
        &client,
        &alice,
        json!({"preset": "private_chat", "invite": [alice2.id], "room_version": "11"}),
    )
    .await;
    let (status, body) = post(&client, &alice2, &format!("join/{room}"), json!({})).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        &client,
        &alice2,
        &format!("rooms/{room}/invite"),
        json!({"user_id": bob.id}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    sync_until(&client, &bob, |s| s["rooms"]["invite"].get(&room).is_some()).await;

    // Alice, who did not invite him, kicks him.
    let (status, body) = post(
        &client,
        &alice,
        &format!("rooms/{room}/kick"),
        json!({"user_id": bob.id, "reason": "not you"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    send_message(&client, &alice, &shared, "after the kick").await;
    sync_until(&client, &bob, |s| {
        timeline_bodies(s, &shared).contains(&"after the kick".to_owned())
    })
    .await;
    let sync = sync_until(&client, &bob, |_| true).await;
    assert!(
        sync["rooms"]["invite"].get(&room).is_some(),
        "bob is still invited: {sync}"
    );

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
