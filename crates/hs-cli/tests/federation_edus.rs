//! Ephemeral data between two in-process servers (`federation_two_servers.rs` is the pattern):
//! typing, read receipts and presence cross in both directions, a device added on either
//! server is a device-list change for the other's users, and the other's `/keys/query` for that
//! user returns the new device, asked of the server that holds it.

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
    _handle: hs_cli::serve::ServeHandle,
    base: String,
    name: String,
    _dir: tempfile::TempDir,
}

async fn start(port: u16) -> Server {
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
        _handle: handle,
        _dir: dir,
    }
}

/// A registered user: id, token and device.
struct User {
    id: String,
    token: String,
    device: String,
}

async fn register(client: &reqwest::Client, base: &str, username: &str) -> User {
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"].as_str().unwrap().to_owned();
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
        device: done["device_id"].as_str().unwrap().to_owned(),
    }
}

async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Value,
) -> Value {
    let response = client
        .request(method, &url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "{url} answered {status}: {body}");
    body
}

/// Syncs (incrementally from `since`, or initially) until `wanted` is true of a response, or
/// fifteen seconds have passed. Returns the matching response.
async fn sync_until(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    since: Option<&str>,
    what: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    let mut since = since.map(str::to_owned);
    while Instant::now() < deadline {
        let url = match &since {
            Some(since) => format!("{base}/_matrix/client/v3/sync?timeout=500&since={since}"),
            None => format!("{base}/_matrix/client/v3/sync?timeout=500"),
        };
        let response: Value = client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if wanted(&response) {
            return response;
        }
        // An incremental sync moves on: what it carried is not repeated in the next one, so a
        // condition on it must be met by one response, and the next is asked from here.
        if since.is_some() {
            since = response["next_batch"].as_str().map(str::to_owned);
        }
        last = response;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the sync never showed {what}; the last one said: {last}");
}

fn ephemeral<'a>(sync: &'a Value, room_id: &str) -> Vec<&'a Value> {
    sync["rooms"]["join"][room_id]["ephemeral"]["events"]
        .as_array()
        .map(|events| events.iter().collect())
        .unwrap_or_default()
}

fn typing_in(sync: &Value, room_id: &str) -> Vec<String> {
    ephemeral(sync, room_id)
        .into_iter()
        .filter(|e| e["type"] == "m.typing")
        .flat_map(|e| {
            e["content"]["user_ids"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|u| u.as_str().map(str::to_owned))
        .collect()
}

fn read_receipt_of(sync: &Value, room_id: &str, event_id: &str, user: &str) -> bool {
    ephemeral(sync, room_id)
        .into_iter()
        .any(|e| e["type"] == "m.receipt" && e["content"][event_id]["m.read"].get(user).is_some())
}

fn presence_of<'a>(sync: &'a Value, user: &str) -> Option<&'a Value> {
    sync["presence"]["events"]
        .as_array()?
        .iter()
        .rev()
        .find(|e| e["sender"] == user)
        .map(|e| &e["content"])
}

fn device_list_changed(sync: &Value, user: &str) -> bool {
    sync["device_lists"]["changed"]
        .as_array()
        .is_some_and(|users| users.iter().any(|u| u == user))
}

/// Alice on A makes a public room and bob on B joins it; returns the room once both servers'
/// `/sync` show both of them joined.
async fn shared_room(
    client: &reqwest::Client,
    a: &Server,
    b: &Server,
    alice: &User,
    bob: &User,
) -> String {
    let created = call(
        client,
        reqwest::Method::POST,
        format!("{}/_matrix/client/v3/createRoom", a.base),
        &alice.token,
        json!({"preset": "public_chat", "room_version": "11"}),
    )
    .await;
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    call(
        client,
        reqwest::Method::POST,
        format!(
            "{}/_matrix/client/v3/join/{room_id}?server_name={}",
            b.base, a.name
        ),
        &bob.token,
        json!({}),
    )
    .await;
    let joined = |me: &str| {
        let room_id = room_id.clone();
        let me = me.to_owned();
        move |s: &Value| {
            s["rooms"]["join"][&room_id]["timeline"]["events"]
                .as_array()
                .into_iter()
                .chain(s["rooms"]["join"][&room_id]["state"]["events"].as_array())
                .flatten()
                .any(|e| {
                    e["type"] == "m.room.member"
                        && e["state_key"] == me.as_str()
                        && e["content"]["membership"] == "join"
                })
        }
    };
    sync_until(
        client,
        &a.base,
        &alice.token,
        None,
        "bob joined, on A",
        joined(&bob.id),
    )
    .await;
    sync_until(
        client,
        &b.base,
        &bob.token,
        None,
        "bob joined, on B",
        joined(&bob.id),
    )
    .await;
    room_id
}

#[tokio::test]
async fn typing_receipts_and_presence_cross_between_servers_in_both_directions() {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let alice = register(&client, &a.base, "alice").await;
    let bob = register(&client, &b.base, "bob").await;
    let room_id = shared_room(&client, &a, &b, &alice, &bob).await;

    let sent = call(
        &client,
        reqwest::Method::PUT,
        format!(
            "{}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/t1",
            a.base
        ),
        &alice.token,
        json!({"msgtype": "m.text", "body": "hello"}),
    )
    .await;
    let event_id = sent["event_id"].as_str().unwrap().to_owned();
    sync_until(
        &client,
        &b.base,
        &bob.token,
        None,
        "alice's message on B",
        |s| {
            s["rooms"]["join"][&room_id]["timeline"]["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|e| e["event_id"] == event_id.as_str()))
        },
    )
    .await;

    // Typing, A to B and B to A.
    for (from, from_base, to, to_base) in [
        (&alice, &a.base, &bob, &b.base),
        (&bob, &b.base, &alice, &a.base),
    ] {
        call(
            &client,
            reqwest::Method::PUT,
            format!(
                "{from_base}/_matrix/client/v3/rooms/{room_id}/typing/{}",
                from.id
            ),
            &from.token,
            json!({"typing": true, "timeout": 30000}),
        )
        .await;
        sync_until(
            &client,
            to_base,
            &to.token,
            None,
            "the other server's user typing",
            |s| typing_in(s, &room_id).contains(&from.id),
        )
        .await;
    }
    // And stopping reaches the other side too.
    call(
        &client,
        reqwest::Method::PUT,
        format!(
            "{}/_matrix/client/v3/rooms/{room_id}/typing/{}",
            a.base, alice.id
        ),
        &alice.token,
        json!({"typing": false}),
    )
    .await;
    sync_until(
        &client,
        &b.base,
        &bob.token,
        None,
        "alice no longer typing",
        |s| {
            let typing = typing_in(s, &room_id);
            typing.contains(&bob.id) && !typing.contains(&alice.id)
        },
    )
    .await;

    // Read receipts, both ways.
    for (from, from_base, to, to_base) in [
        (&alice, &a.base, &bob, &b.base),
        (&bob, &b.base, &alice, &a.base),
    ] {
        call(
            &client,
            reqwest::Method::POST,
            format!("{from_base}/_matrix/client/v3/rooms/{room_id}/receipt/m.read/{event_id}"),
            &from.token,
            json!({}),
        )
        .await;
        sync_until(
            &client,
            to_base,
            &to.token,
            None,
            "the other server's read receipt",
            |s| read_receipt_of(s, &room_id, &event_id, &from.id),
        )
        .await;
    }

    // Presence, both ways: each sets a state and a message, and the other server's user sees it.
    for (from, from_base, to, to_base, msg) in [
        (&alice, &a.base, &bob, &b.base, "at lunch"),
        (&bob, &b.base, &alice, &a.base, "in a meeting"),
    ] {
        call(
            &client,
            reqwest::Method::PUT,
            format!("{from_base}/_matrix/client/v3/presence/{}/status", from.id),
            &from.token,
            json!({"presence": "unavailable", "status_msg": msg}),
        )
        .await;
        sync_until(
            &client,
            to_base,
            &to.token,
            None,
            "the other server's user's presence",
            |s| {
                presence_of(s, &from.id)
                    .is_some_and(|p| p["presence"] == "unavailable" && p["status_msg"] == msg)
            },
        )
        .await;
    }
}

/// Uploads device keys for `user`'s device `device`, returning the ed25519 key it claimed.
async fn upload_keys(
    client: &reqwest::Client,
    base: &str,
    user: &User,
    token: &str,
    device: &str,
) -> String {
    let key = format!("key-of-{device}");
    call(
        client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/keys/upload"),
        token,
        json!({"device_keys": {
            "user_id": user.id,
            "device_id": device,
            "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
            "keys": {format!("ed25519:{device}"): key, format!("curve25519:{device}"): "c"},
            "signatures": {},
        }}),
    )
    .await;
    key
}

#[tokio::test]
async fn a_device_added_on_one_server_is_a_device_list_change_on_the_other() {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let alice = register(&client, &a.base, "alice").await;
    let bob = register(&client, &b.base, "bob").await;
    shared_room(&client, &a, &b, &alice, &bob).await;

    // Alice is caught up: bob's join, and with it the first news of his device list, is behind
    // her token. Only an EDU from B can put him in `device_lists.changed` from here.
    let caught_up = sync_until(
        &client,
        &a.base,
        &alice.token,
        None,
        "an initial sync",
        |_| true,
    )
    .await;
    let since = caught_up["next_batch"].as_str().unwrap().to_owned();

    // Bob signs in on a second device, on B, and it uploads its keys, as every client does.
    let login = call(
        &client,
        reqwest::Method::POST,
        format!("{}/_matrix/client/v3/login", b.base),
        &bob.token,
        json!({"type": "m.login.password",
               "identifier": {"type": "m.id.user", "user": "bob"},
               "password": "correct horse"}),
    )
    .await;
    let laptop = login["device_id"].as_str().unwrap().to_owned();
    let laptop_token = login["access_token"].as_str().unwrap().to_owned();
    let laptop_key = upload_keys(&client, &b.base, &bob, &laptop_token, &laptop).await;

    sync_until(
        &client,
        &a.base,
        &alice.token,
        Some(&since),
        "bob's device-list change on A",
        |s| device_list_changed(s, &bob.id),
    )
    .await;
    // And alice's client, told to, asks: A asks B, and the new device is there.
    let keys = call(
        &client,
        reqwest::Method::POST,
        format!("{}/_matrix/client/v3/keys/query", a.base),
        &alice.token,
        json!({"device_keys": {bob.id.clone(): []}}),
    )
    .await;
    assert_eq!(
        keys["device_keys"][&bob.id][&laptop]["keys"][format!("ed25519:{laptop}")],
        laptop_key.as_str(),
        "A's /keys/query for bob: {keys}"
    );
    assert_eq!(keys["failures"], json!({}), "{keys}");

    // The other direction: alice's phone uploads keys on A; bob, on B, is told.
    let bob_caught_up = sync_until(
        &client,
        &b.base,
        &bob.token,
        None,
        "an initial sync",
        |_| true,
    )
    .await;
    let bob_since = bob_caught_up["next_batch"].as_str().unwrap().to_owned();
    let alice_key = upload_keys(&client, &a.base, &alice, &alice.token, &alice.device).await;
    sync_until(
        &client,
        &b.base,
        &bob.token,
        Some(&bob_since),
        "alice's device-list change on B",
        |s| device_list_changed(s, &alice.id),
    )
    .await;
    let keys = call(
        &client,
        reqwest::Method::POST,
        format!("{}/_matrix/client/v3/keys/query", b.base),
        &bob.token,
        json!({"device_keys": {alice.id.clone(): []}}),
    )
    .await;
    assert_eq!(
        keys["device_keys"][&alice.id][&alice.device]["keys"][format!("ed25519:{}", alice.device)],
        alice_key.as_str(),
        "B's /keys/query for alice: {keys}"
    );
}
