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
