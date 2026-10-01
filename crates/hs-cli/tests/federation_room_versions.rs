//! Two real `hs serve` instances over plain HTTP (as `federation_two_servers.rs`), for what
//! Sytest's first run found missing between servers: a room of version 1 could not be joined
//! over federation (`make_join` refused to cite events by hash), and a redaction that arrived
//! over federation was stored and never applied.

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
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
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
        handle,
        _dir: dir,
    }
}

async fn register(client: &reqwest::Client, base: &str, username: &str) -> (String, String) {
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
    (
        done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

/// Asks `url` until `wanted` holds of the answer, or fifteen seconds pass: federation delivery
/// runs off background tasks.
async fn get_until(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let response: Value = client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or(Value::Null);
        if wanted(&response) {
            return response;
        }
        last = response;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("{url} never answered what was expected; the last answer: {last}");
}

struct Joined {
    a: Server,
    b: Server,
    client: reqwest::Client,
    alice_token: String,
    bob_token: String,
    room_id: String,
}

/// Alice makes a public room of `version` on A; bob, on B, joins it through A.
async fn join_across(version: &str) -> Joined {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let (_, alice_token) = register(&client, &a.base, "alice").await;
    let (_, bob_token) = register(&client, &b.base, "bob").await;
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "public_chat", "room_version": version}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"]
        .as_str()
        .unwrap_or_else(|| panic!("createRoom failed: {created}"))
        .to_owned();
    let join = client
        .post(format!(
            "{}/_matrix/client/v3/join/{room_id}?server_name={}",
            b.base, a.name
        ))
        .bearer_auth(&bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = join.status();
    let body: Value = join.json().await.unwrap();
    assert_eq!(
        status, 200,
        "joining a version {version} room failed: {body}"
    );
    Joined {
        a,
        b,
        client,
        alice_token,
        bob_token,
        room_id,
    }
}

async fn send(joined: &Joined, base: &str, token: &str, body: &str) -> String {
    let response: Value = joined
        .client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            joined.room_id,
            uuid_like()
        ))
        .bearer_auth(token)
        .json(&json!({"msgtype": "m.text", "body": body}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    response["event_id"]
        .as_str()
        .unwrap_or_else(|| panic!("send failed: {response}"))
        .to_owned()
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// A room of version 1 -- events cited by `[id, {sha256}]`, ids not derived from hashes -- is
/// joined over federation, and messages cross both ways after. Before the fix, `make_join`
/// answered `M_UNSUPPORTED_ROOM_VERSION` for versions 1 and 2 (12 Sytest failures).
#[tokio::test]
async fn a_version_1_room_is_joined_over_federation_and_messages_cross() {
    let joined = join_across("1").await;
    let from_bob = send(&joined, &joined.b.base, &joined.bob_token, "from bob").await;
    let at_a = get_until(
        &joined.client,
        &format!(
            "{}/_matrix/client/v3/rooms/{}/event/{from_bob}",
            joined.a.base, joined.room_id
        ),
        &joined.alice_token,
        |event| event["content"]["body"] == "from bob",
    )
    .await;
    assert_eq!(at_a["event_id"], from_bob.as_str());
    let from_alice = send(&joined, &joined.a.base, &joined.alice_token, "from alice").await;
    get_until(
        &joined.client,
        &format!(
            "{}/_matrix/client/v3/rooms/{}/event/{from_alice}",
            joined.b.base, joined.room_id
        ),
        &joined.bob_token,
        |event| event["content"]["body"] == "from alice",
    )
    .await;
    joined.a.handle.shutdown().await;
    joined.b.handle.shutdown().await;
}

/// A room of version 3, whose event IDs are standard base64 and so about half the time carry a
/// `/`: six invites from A to users of B all go through. Before the client percent-encoded the
/// IDs in its request paths, the `/` split the path, B answered `404 M_UNRECOGNIZED`, and each
/// invite failed with even odds (Sytest's "User can invite remote user to room with version 3"
/// passed or failed from one run to the next); six in a row all succeeding was about 1 in 60.
#[tokio::test]
async fn invites_in_a_version_3_room_reach_another_server_whatever_their_event_ids() {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let (_, alice_token) = register(&client, &a.base, "alice").await;
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "private_chat", "room_version": "3"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"]
        .as_str()
        .unwrap_or_else(|| panic!("createRoom failed: {created}"))
        .to_owned();
    for i in 0..6 {
        let (invitee, _) = register(&client, &b.base, &format!("guest{i}")).await;
        let response = client
            .post(format!(
                "{}/_matrix/client/v3/rooms/{room_id}/invite",
                a.base
            ))
            .bearer_auth(&alice_token)
            .json(&json!({"user_id": invitee}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        assert_eq!(status, 200, "inviting {invitee} failed: {body}");
    }
    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// Bob, on B, redacts his own message; on A, where the redaction arrives over federation, the
/// message's content is gone. Before the fix the redaction was stored on A and the message kept
/// its body there (Sytest's "Can receive redactions from regular users over federation").
#[tokio::test]
async fn a_redaction_received_over_federation_is_applied() {
    let joined = join_across("11").await;
    let message = send(&joined, &joined.b.base, &joined.bob_token, "regrettable").await;
    let event_url = format!(
        "{}/_matrix/client/v3/rooms/{}/event/{message}",
        joined.a.base, joined.room_id
    );
    get_until(&joined.client, &event_url, &joined.alice_token, |event| {
        event["content"]["body"] == "regrettable"
    })
    .await;
    let redaction: Value = joined
        .client
        .put(format!(
            "{}/_matrix/client/v3/rooms/{}/redact/{message}/{}",
            joined.b.base,
            joined.room_id,
            uuid_like()
        ))
        .bearer_auth(&joined.bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(redaction["event_id"].is_string(), "{redaction}");
    let redacted = get_until(&joined.client, &event_url, &joined.alice_token, |event| {
        event["content"].as_object().is_some_and(|c| c.is_empty())
    })
    .await;
    assert_eq!(redacted["event_id"], message.as_str());
    joined.a.handle.shutdown().await;
    joined.b.handle.shutdown().await;
}
