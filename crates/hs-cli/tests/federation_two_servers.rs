//! Two real `hs serve` instances in one process, federating with each other over plain HTTP
//! (`ServeOptions::federation_scheme`, the one seam a test may use that a configuration file
//! cannot): a user on one joins a room the other hosts, and each side's messages reach the
//! other's `/sync`. This is the automated version of
//! `crates/hs-federation/scripts/two-server-federation.sh`, which needs `stunnel` and a private
//! CA to do the same thing over TLS -- and it covers what that script could not until now: the
//! join being *usable* on the joining side (RFC 0015), and locally sent events being *sent*
//! anywhere at all.
//!
//! Server names are IP literals with an explicit port (`127.0.0.1:{port}`), which bypass
//! discovery entirely (no `.well-known`, no SRV, no DNS), the same choice the script makes and
//! for the same reason: `hs-federation`'s resolver is a userspace stub that does not read
//! `/etc/hosts`. The ports are reserved before the servers start because a server's own name has
//! to be in its configuration before it binds; the window in which another process on the
//! machine could take one of them is real and small.

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

/// Registers `username` through the real UIA dance and returns `(user_id, access_token)`.
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
    (
        done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

async fn send_message(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    room_id: &str,
    body: &str,
) -> String {
    let txn = format!("txn-{}", uuid_like());
    let response: Value = client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/{txn}"
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

/// Initial syncs until `wanted` is true of the response, or a few seconds have passed. A
/// condition, not a duration: federation delivery and the session hub both run off background
/// tasks, so the first sync after a join or a remote message can honestly come back without it.
async fn sync_until(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let response: Value = client
            .get(format!("{base}/_matrix/client/v3/sync?timeout=500"))
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

fn state_types(sync: &Value, room_id: &str) -> Vec<String> {
    let mut types: Vec<String> = ["state", "timeline"]
        .iter()
        .flat_map(|section| {
            sync["rooms"]["join"][room_id][section]["events"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|e| e.get("state_key").is_some())
        .map(|e| {
            format!(
                "{}/{}",
                e["type"].as_str().unwrap_or(""),
                e["state_key"].as_str().unwrap_or("")
            )
        })
        .collect();
    types.sort();
    types
}

#[tokio::test]
async fn a_user_joins_a_room_on_another_server_and_messages_flow_both_ways() {
    let port_a = reserve_port();
    let port_b = reserve_port();
    let a = start(port_a).await;
    let b = start(port_b).await;
    let client = reqwest::Client::new();

    let (alice, alice_token) = register(&client, &a.base, "alice").await;
    let (bob, bob_token) = register(&client, &b.base, "bob").await;
    assert_eq!(alice, format!("@alice:{}", a.name));
    assert_eq!(bob, format!("@bob:{}", b.name));

    // Alice, on A, makes a public room and says something before anybody else is there.
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "public_chat", "name": "two servers", "room_version": "11"}))
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
    send_message(&client, &a.base, &alice_token, &room_id, "hello before bob").await;

    // Bob, on B, joins it the way a client does: the room ID and the server to ask.
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
    let join_body: Value = join.json().await.unwrap();
    assert_eq!(status, 200, "join failed: {join_body}");
    assert_eq!(join_body["room_id"], room_id);

    // A holds bob as a joined member (the resident side, which already worked)...
    let members: Value = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/joined_members",
            a.base
        ))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        members["joined"].get(&bob).is_some(),
        "A does not list bob as joined: {members}"
    );

    // ...and, new, so does B: bob's own /sync carries the room, its state and his join.
    let bob_sync = sync_until(&client, &b.base, &bob_token, |s| {
        s["rooms"]["join"].get(&room_id).is_some()
    })
    .await;
    let types = state_types(&bob_sync, &room_id);
    for wanted in [
        "m.room.create/".to_owned(),
        "m.room.power_levels/".to_owned(),
        "m.room.join_rules/".to_owned(),
        "m.room.name/".to_owned(),
        format!("m.room.member/{alice}"),
        format!("m.room.member/{bob}"),
    ] {
        assert!(
            types.contains(&wanted),
            "bob's sync lacks {wanted}: {types:?}"
        );
    }
    let bob_timeline = &bob_sync["rooms"]["join"][&room_id]["timeline"]["events"];
    assert!(
        bob_timeline
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "m.room.member" && e["state_key"] == bob),
        "bob's timeline lacks his own join: {bob_timeline}"
    );
    // The room's history before the join is not backfilled yet; what bob sees starts at his
    // join. Written down here so the day it changes, this line changes with it.
    assert!(
        !timeline_bodies(&bob_sync, &room_id).contains(&"hello before bob".to_owned()),
        "the pre-join history is not expected yet: {bob_sync}"
    );

    // Bob speaks on B; alice reads it on A. That is the outbound sender, and B's event being
    // accepted by A's inbound /send with bob's join as its ancestor.
    send_message(&client, &b.base, &bob_token, &room_id, "hi alice, from B").await;
    sync_until(&client, &a.base, &alice_token, |s| {
        timeline_bodies(s, &room_id).contains(&"hi alice, from B".to_owned())
    })
    .await;

    // Alice answers on A; bob reads it on B: the same path in the other direction, with A's
    // event citing bob's message as its ancestor.
    send_message(
        &client,
        &a.base,
        &alice_token,
        &room_id,
        "welcome bob, from A",
    )
    .await;
    let bob_sync = sync_until(&client, &b.base, &bob_token, |s| {
        timeline_bodies(s, &room_id).contains(&"welcome bob, from A".to_owned())
    })
    .await;
    let bodies = timeline_bodies(&bob_sync, &room_id);
    assert_eq!(
        bodies,
        vec!["hi alice, from B", "welcome bob, from A"],
        "bob's timeline on B"
    );

    // The joined room survives B being asked cold: the actor was made from a snapshot and is
    // reloaded from what was persisted.
    let state: Value = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/state/m.room.name",
            b.base
        ))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        state["name"], "two servers",
        "B's copy of the room state: {state}"
    );

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

#[tokio::test]
async fn joining_through_a_server_that_does_not_know_the_room_is_not_found() {
    let port_a = reserve_port();
    let port_b = reserve_port();
    let a = start(port_a).await;
    let b = start(port_b).await;
    let client = reqwest::Client::new();
    let (_bob, bob_token) = register(&client, &b.base, "bob").await;

    let join = client
        .post(format!(
            "{}/_matrix/client/v3/join/!nowhere:{}?server_name={}",
            b.base, a.name, a.name
        ))
        .bearer_auth(&bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(join.status(), 404);
    let body: Value = join.json().await.unwrap();
    assert_eq!(body["errcode"], "M_NOT_FOUND", "{body}");

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
