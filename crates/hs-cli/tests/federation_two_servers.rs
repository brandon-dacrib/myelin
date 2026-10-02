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
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n"
    );
    // The server-wide send limit is off (decision 0016): this conversation is faster than a person.
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
    // More than one backfill batch's worth (`hs_cli::backfill::BATCH` is 100), so that reading
    // it all back takes more than one fetch.
    const BEFORE_BOB: usize = 120;
    for i in 1..=BEFORE_BOB {
        send_message(
            &client,
            &a.base,
            &alice_token,
            &room_id,
            &format!("before bob {i}"),
        )
        .await;
    }

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
    // The room's history before the join is on A and nothing has asked for it yet, so bob's
    // first sync starts at his join -- but it hands him somewhere to ask from.
    assert!(
        timeline_bodies(&bob_sync, &room_id).is_empty(),
        "nothing before the join has been fetched yet: {bob_sync}"
    );
    let prev_batch = bob_sync["rooms"]["join"][&room_id]["timeline"]["prev_batch"]
        .as_str()
        .unwrap_or_else(|| {
            panic!("a room with history before what is held must offer a prev_batch: {bob_sync}")
        })
        .to_owned();
    let bob_since = bob_sync["next_batch"]
        .as_str()
        .expect("a sync token")
        .to_owned();

    // Bob reads backwards from there, the way a client scrolling up does, until the server says
    // there is nothing further. Each page that reaches the edge of what B holds fetches the next
    // batch from A before answering: 120 messages and the room's six creation events, in 50s,
    // is a page from the first batch, a page from held events, and a page that fetches the rest
    // and reaches the create event.
    let mut from = Some(prev_batch);
    let mut chunk: Vec<Value> = Vec::new();
    let mut requests = 0;
    loop {
        requests += 1;
        assert!(
            requests <= 10,
            "still paginating after {requests} requests: {chunk:?}"
        );
        let mut url = format!(
            "{}/_matrix/client/v3/rooms/{room_id}/messages?dir=b&limit=50",
            b.base
        );
        if let Some(token) = &from {
            url.push_str("&from=");
            url.push_str(token);
        }
        let page: Value = client
            .get(url)
            .bearer_auth(&bob_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        chunk.extend(page["chunk"].as_array().cloned().unwrap_or_default());
        match page.get("end").and_then(Value::as_str) {
            Some(end) => from = Some(end.to_owned()),
            None => break,
        }
    }
    assert_eq!(
        requests, 3,
        "one backfill batch per page that reaches the edge"
    );
    let bodies: Vec<String> = chunk
        .iter()
        .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
        .collect();
    let expected: Vec<String> = (1..=BEFORE_BOB)
        .rev()
        .map(|i| format!("before bob {i}"))
        .collect();
    assert_eq!(
        bodies, expected,
        "every message before the join, newest first"
    );
    assert_eq!(
        chunk.last().map(|e| e["type"].as_str().unwrap_or("")),
        Some("m.room.create"),
        "the last page reaches the room's creation"
    );
    assert!(
        chunk.iter().any(|e| e["type"] == "m.room.name"),
        "the creation-time state events are in the history too: {chunk:?}"
    );

    // History is not news: bob's next incremental sync has nothing new in the room.
    let incremental: Value = client
        .get(format!(
            "{}/_matrix/client/v3/sync?since={bob_since}&timeout=0",
            b.base
        ))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        timeline_bodies(&incremental, &room_id).is_empty(),
        "backfilled history must not arrive as new events: {incremental}"
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
    // A fresh sync's timeline is the newest events whatever their origin: the exchange, and
    // the fetched history right before the join.
    let bodies = timeline_bodies(&bob_sync, &room_id);
    assert!(
        bodies.ends_with(&[
            "hi alice, from B".to_owned(),
            "welcome bob, from A".to_owned()
        ]),
        "bob's timeline on B: {bodies:?}"
    );
    assert!(
        bodies.contains(&format!("before bob {BEFORE_BOB}")),
        "the fetched history shows in a fresh sync: {bodies:?}"
    );

    // The joined room survives B being asked cold: the actor was made from a snapshot and is
    // reloaded from what was persisted.
    let room_name = |base: &str, token: &str| {
        let url = format!("{base}/_matrix/client/v3/rooms/{room_id}/state/m.room.name");
        let client = client.clone();
        let token = token.to_owned();
        async move {
            let state: Value = client
                .get(url)
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            state
        }
    };
    let state = room_name(&b.base, &bob_token).await;
    assert_eq!(
        state["name"], "two servers",
        "B's copy of the room state: {state}"
    );

    // Bob leaves, the room changes while nobody from B is in it, and bob comes back. B still
    // holds its copy of the room, so the rejoin could be made against it -- and would then be
    // a join made against the room as it was when bob left. It goes through A instead, the
    // same way the first join did, and the answer carries the room as it is now.
    let leave = client
        .post(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/leave",
            b.base
        ))
        .bearer_auth(&bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(leave.status(), 200);
    sync_until(&client, &a.base, &alice_token, |s| {
        s["rooms"]["leave"].get(&room_id).is_some()
            || s["rooms"]["join"][&room_id]["timeline"]["events"]
                .as_array()
                .is_some_and(|events| {
                    events.iter().any(|e| {
                        e["type"] == "m.room.member"
                            && e["state_key"] == bob
                            && e["content"]["membership"] == "leave"
                    })
                })
    })
    .await;
    // Alice talks while bob is out: more than one backfill batch's worth, so that filling the
    // gap his rejoin leaves takes more than one fetch.
    const WHILE_OUT: usize = 130;
    for i in 1..=WHILE_OUT {
        send_message(
            &client,
            &a.base,
            &alice_token,
            &room_id,
            &format!("while bob was out {i}"),
        )
        .await;
    }
    let renamed: Value = client
        .put(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/state/m.room.name",
            a.base
        ))
        .bearer_auth(&alice_token)
        .json(&json!({"name": "renamed while bob was out"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(renamed["event_id"].is_string(), "{renamed}");
    let rejoin = client
        .post(format!(
            "{}/_matrix/client/v3/join/{room_id}?server_name={}",
            b.base, a.name
        ))
        .bearer_auth(&bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejoin.status(), 200);
    let state = room_name(&b.base, &bob_token).await;
    assert_eq!(
        state["name"], "renamed while bob was out",
        "B's copy of the room after the rejoin must be the room as it is now: {state}"
    );
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
        "A knows bob is back before the join returned: {members}"
    );

    // And the timeline: reading back from his rejoin, bob reaches what alice said while he was
    // out, in her order, and then what B held from before he left -- the gap between his leave
    // and his rejoin is fetched from A as the pages reach it (`hs_room::actor::gaps`), not
    // skipped.
    let bob_since = sync_until(&client, &b.base, &bob_token, |s| {
        s["rooms"]["join"].get(&room_id).is_some()
    })
    .await["next_batch"]
        .as_str()
        .expect("a sync token")
        .to_owned();
    let mut from: Option<String> = None;
    let mut chunk: Vec<Value> = Vec::new();
    let mut requests = 0;
    loop {
        requests += 1;
        assert!(
            requests <= 15,
            "still paginating after {requests} requests: {} events",
            chunk.len()
        );
        let mut url = format!(
            "{}/_matrix/client/v3/rooms/{room_id}/messages?dir=b&limit=50",
            b.base
        );
        if let Some(token) = &from {
            url.push_str("&from=");
            url.push_str(token);
        }
        let page: Value = client
            .get(url)
            .bearer_auth(&bob_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        chunk.extend(page["chunk"].as_array().cloned().unwrap_or_default());
        match page.get("end").and_then(Value::as_str) {
            Some(end) => from = Some(end.to_owned()),
            None => break,
        }
    }
    let bodies: Vec<String> = chunk
        .iter()
        .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
        .collect();
    let mut expected: Vec<String> = (1..=WHILE_OUT)
        .rev()
        .map(|i| format!("while bob was out {i}"))
        .collect();
    expected.extend([
        "welcome bob, from A".to_owned(),
        "hi alice, from B".to_owned(),
    ]);
    expected.extend((1..=BEFORE_BOB).rev().map(|i| format!("before bob {i}")));
    assert_eq!(
        bodies, expected,
        "everything said while bob was out, newest first, then what B held from before"
    );
    let position = |wanted: &dyn Fn(&Value) -> bool| chunk.iter().position(wanted);
    let is_membership = |membership: &'static str| {
        let bob = bob.clone();
        move |e: &Value| {
            e["type"] == "m.room.member"
                && e["state_key"] == bob
                && e["content"]["membership"] == membership
        }
    };
    assert_eq!(
        position(&is_membership("join")),
        Some(0),
        "the newest event is the rejoin"
    );
    let rename = position(&|e: &Value| {
        e["type"] == "m.room.name" && e["content"]["name"] == "renamed while bob was out"
    })
    .expect("the rename made while bob was out is in his history");
    let first_while_out = position(&|e: &Value| e["content"]["body"] == "while bob was out 1")
        .expect("alice's first word while bob was out");
    let leave = position(&is_membership("leave")).expect("bob's leave");
    assert!(
        rename < first_while_out && first_while_out < leave,
        "the rename ({rename}), then alice's first word ({first_while_out}), then the leave \
         ({leave})"
    );
    assert_eq!(
        chunk.last().map(|e| e["type"].as_str().unwrap_or("")),
        Some("m.room.create"),
        "the last page still reaches the room's creation"
    );

    // History is not news: bob's next incremental sync has none of it.
    let incremental: Value = client
        .get(format!(
            "{}/_matrix/client/v3/sync?since={bob_since}&timeout=0",
            b.base
        ))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        timeline_bodies(&incremental, &room_id).is_empty(),
        "the history between the leave and the rejoin must not arrive as new events: \
         {incremental}"
    );

    // And it is counted: every event placed in the gap.
    let mut registry = prometheus_client::registry::Registry::default();
    hs_cli::backfill::register_metrics(&mut registry);
    let mut exposition = String::new();
    prometheus_client::encoding::text::encode(&mut exposition, &registry).unwrap();
    let gap_events: u64 = exposition
        .lines()
        .find(|line| line.starts_with("hs_room_backfilled_events_total{kind=\"rejoin_gap\"}"))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no rejoin_gap count in {exposition}"));
    assert!(
        gap_events > WHILE_OUT as u64,
        "the gap's messages and the rename were counted: {gap_events}"
    );

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// The value of the counter line in `exposition` that starts with `prefix`, `0` if absent.
fn counter(exposition: &str, prefix: &str) -> u64 {
    exposition
        .lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

async fn set_topic(client: &reqwest::Client, base: &str, token: &str, room_id: &str, topic: &str) {
    let response: Value = client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/state/m.room.topic"
        ))
        .bearer_auth(token)
        .json(&json!({"topic": topic}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(response["event_id"].is_string(), "{response}");
}

/// Bob joins late and reads back past a topic change; the state at history fetched from A is
/// A's, including a topic set before anything B fetched: B asked A for the state at the oldest
/// event of the batch (`/state_ids`, then `/event` for the old topic, which B never held)
/// rather than walking back from what it held, where that topic does not exist.
#[tokio::test]
async fn the_state_at_fetched_history_is_asked_of_the_server_that_sent_it() {
    let port_a = reserve_port();
    let port_b = reserve_port();
    let a = start(port_a).await;
    let b = start(port_b).await;
    let client = reqwest::Client::new();

    let (_alice, alice_token) = register(&client, &a.base, "alice").await;
    let (_bob, bob_token) = register(&client, &b.base, "bob").await;
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "public_chat", "room_version": "11"}))
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

    // The first topic, ten messages, the second topic, then 95 messages: a backfill batch of
    // 100 from bob's join reaches the second topic and three messages before it, never the
    // first topic.
    set_topic(&client, &a.base, &alice_token, &room_id, "first topic").await;
    let mut before = Vec::new();
    for i in 1..=10 {
        before.push(
            send_message(
                &client,
                &a.base,
                &alice_token,
                &room_id,
                &format!("before the second topic {i}"),
            )
            .await,
        );
    }
    set_topic(&client, &a.base, &alice_token, &room_id, "second topic").await;
    for i in 1..=95 {
        send_message(
            &client,
            &a.base,
            &alice_token,
            &room_id,
            &format!("after the second topic {i}"),
        )
        .await;
    }

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
    assert_eq!(join.status(), 200);
    let bob_sync = sync_until(&client, &b.base, &bob_token, |s| {
        s["rooms"]["join"].get(&room_id).is_some()
    })
    .await;
    let prev_batch = bob_sync["rooms"]["join"][&room_id]["timeline"]["prev_batch"]
        .as_str()
        .expect("a prev_batch")
        .to_owned();

    // One page back reaches the join's edge and fetches one batch from A.
    let page: Value = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/messages?dir=b&limit=50&from={prev_batch}",
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
        page["chunk"].as_array().map(Vec::len),
        Some(50),
        "a full page of fetched history: {page}"
    );

    // The tenth message before the second topic is in that batch; the state at it, as B's
    // `/context` shows it, has the first topic -- set before the batch, held by B only because
    // B asked A for the state there.
    let topic_at = |base: String, token: String, event_id: String| {
        let client = client.clone();
        let room_id = room_id.clone();
        async move {
            let context: Value = client
                .get(format!(
                    "{base}/_matrix/client/v3/rooms/{room_id}/context/{event_id}?limit=0"
                ))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let state = context["state"]
                .as_array()
                .unwrap_or_else(|| panic!("no state in /context: {context}"))
                .clone();
            state
                .iter()
                .find(|e| e["type"] == "m.room.topic")
                .and_then(|e| e["content"]["topic"].as_str())
                .map(str::to_owned)
        }
    };
    let tenth = before.last().expect("ten messages").clone();
    assert_eq!(
        topic_at(a.base.clone(), alice_token.clone(), tenth.clone()).await,
        Some("first topic".to_owned()),
        "A's own state at the event"
    );
    assert_eq!(
        topic_at(b.base.clone(), bob_token.clone(), tenth).await,
        Some("first topic".to_owned()),
        "B's state at the fetched event has the topic set before the batch"
    );

    // And the batch was counted as placed with a fetched state.
    let mut registry = prometheus_client::registry::Registry::default();
    hs_cli::backfill::register_metrics(&mut registry);
    let mut exposition = String::new();
    prometheus_client::encoding::text::encode(&mut exposition, &registry).unwrap();
    assert!(
        counter(
            &exposition,
            "hs_room_backfill_batches_total{kind=\"before_oldest\",outcome=\"state_fetched\"}"
        ) >= 1,
        "{exposition}"
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

/// Sytest's "Outbound/Inbound federation can query profile data" and "... room alias directory",
/// between two real servers: a profile and an alias of A's user and room, read through B's
/// client API, come from A over `/query/profile` and `/query/directory`; a user and an alias A
/// does not have are `404`s on B too.
#[tokio::test]
async fn a_remote_users_profile_and_a_remote_alias_are_asked_of_their_server() {
    let port_a = reserve_port();
    let port_b = reserve_port();
    let a = start(port_a).await;
    let b = start(port_b).await;
    let client = reqwest::Client::new();
    let (alice, alice_token) = register(&client, &a.base, "alice").await;
    let (_bob, bob_token) = register(&client, &b.base, "bob").await;

    let put = client
        .put(format!(
            "{}/_matrix/client/v3/profile/{alice}/displayname",
            a.base
        ))
        .bearer_auth(&alice_token)
        .json(&json!({"displayname": "Displayname Set For Federation Test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "public_chat", "room_alias_name": "☕"}))
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

    // B asks A for alice's display name, her whole profile, and the alias.
    let displayname: Value = client
        .get(format!(
            "{}/_matrix/client/v3/profile/{alice}/displayname",
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
        displayname,
        json!({"displayname": "Displayname Set For Federation Test"})
    );
    let profile: Value = client
        .get(format!("{}/_matrix/client/v3/profile/{alice}", b.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        profile,
        json!({"displayname": "Displayname Set For Federation Test"})
    );
    let alias = format!("%23%E2%98%95:{}", a.name);
    let directory: Value = client
        .get(format!(
            "{}/_matrix/client/v3/directory/room/{alias}",
            b.base
        ))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(directory["room_id"], room_id, "{directory}");
    assert_eq!(directory["servers"], json!([a.name]), "{directory}");

    // What A does not have is not found on B either.
    let unknown = client
        .get(format!(
            "{}/_matrix/client/v3/profile/@nobody:{}/avatar_url",
            b.base, a.name
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let unknown = client
        .get(format!(
            "{}/_matrix/client/v3/directory/room/%23nowhere:{}",
            b.base, a.name
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let body: Value = unknown.json().await.unwrap();
    assert_eq!(body["errcode"], "M_NOT_FOUND", "{body}");

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
