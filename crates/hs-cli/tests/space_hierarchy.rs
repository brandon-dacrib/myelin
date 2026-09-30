//! Two real `hs serve` instances in one process (the harness of `federation_two_servers.rs`),
//! and a space that spans them: `GET /_matrix/client/v1/rooms/{roomId}/hierarchy` walked from a
//! user on each side. A child on the other server is asked about over federation `/hierarchy`;
//! a private child is left out; a restricted child appears once its server can see that the
//! requester's server has a user in the room it allows; `suggested_only` and a `limit` with a
//! `from` token page the walk; a bad token and a root the requester may not see are refused.

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

/// `POST /createRoom` with `body`, returning the room ID.
async fn create_room(client: &reqwest::Client, base: &str, token: &str, body: Value) -> String {
    let created: Value = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    created["room_id"]
        .as_str()
        .unwrap_or_else(|| panic!("createRoom failed: {created}"))
        .to_owned()
}

/// A room with `name`, `preset`, optionally a space, world-readable, or restricted to `allow`.
fn room_body(name: &str, preset: &str) -> Value {
    json!({ "preset": preset, "name": name, "room_version": "10" })
}

fn space_body(name: &str) -> Value {
    let mut body = room_body(name, "public_chat");
    body["creation_content"] = json!({ "type": "m.space" });
    body["initial_state"] = json!([{
        "type": "m.room.history_visibility",
        "state_key": "",
        "content": { "history_visibility": "world_readable" },
    }]);
    body
}

fn restricted_body(name: &str, allow: &str, via: &str) -> Value {
    let mut body = room_body(name, "public_chat");
    body["initial_state"] = json!([{
        "type": "m.room.join_rules",
        "state_key": "",
        "content": {
            "join_rule": "restricted",
            "allow": [{ "type": "m.room_membership", "room_id": allow, "via": [via] }],
        },
    }]);
    body
}

/// Sends an `m.space.child` link from `parent` to `child`.
async fn link(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    parent: &str,
    child: &str,
    content: Value,
) {
    let response = client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{parent}/state/m.space.child/{child}"
        ))
        .bearer_auth(token)
        .json(&content)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "linking {child} under {parent} failed: {body}");
    // Distinct `origin_server_ts` per link, so the spec's tie-break order is the link order.
    tokio::time::sleep(Duration::from_millis(5)).await;
}

/// `GET /rooms/{root}/hierarchy` with `query`, as `(status, body)`.
async fn hierarchy(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    root: &str,
    query: &str,
) -> (u16, Value) {
    let response = client
        .get(format!(
            "{base}/_matrix/client/v1/rooms/{root}/hierarchy{query}"
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap();
    (status, body)
}

fn room_ids(body: &Value) -> Vec<&str> {
    body["rooms"]
        .as_array()
        .unwrap_or_else(|| panic!("no rooms in {body}"))
        .iter()
        .map(|r| r["room_id"].as_str().unwrap())
        .collect()
}

/// Walks until the rooms are `wanted`, or a few seconds pass: a membership change on the other
/// server reaches this one over federation in the background.
async fn hierarchy_until(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    root: &str,
    wanted: &[&str],
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let (status, body) = hierarchy(client, base, token, root, "").await;
        if status == 200 && room_ids(&body) == wanted {
            return body;
        }
        last = json!({ "status": status, "body": body });
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the hierarchy never became {wanted:?}; the last answer was {last}");
}

async fn join(client: &reqwest::Client, base: &str, token: &str, room: &str, via: &str) {
    let response = client
        .post(format!(
            "{base}/_matrix/client/v3/join/{room}?server_name={via}"
        ))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "join of {room} failed: {body}");
}

#[tokio::test]
async fn a_space_across_two_servers_is_walked_from_either_side() {
    let port_a = reserve_port();
    let port_b = reserve_port();
    let a = start(port_a).await;
    let b = start(port_b).await;
    let client = reqwest::Client::new();

    let (alice, alice_token) = register(&client, &a.base, "alice").await;
    let (_carol, carol_token) = register(&client, &a.base, "carol").await;
    let (_bob, bob_token) = register(&client, &b.base, "bob").await;

    // On A: the space, a public child, a private child, and a suggested child.
    let space = create_room(&client, &a.base, &alice_token, space_body("Space")).await;
    let r1 = create_room(
        &client,
        &a.base,
        &alice_token,
        room_body("R1", "public_chat"),
    )
    .await;
    let private = create_room(
        &client,
        &a.base,
        &alice_token,
        room_body("Private", "private_chat"),
    )
    .await;
    let suggested = create_room(
        &client,
        &a.base,
        &alice_token,
        room_body("Suggested", "public_chat"),
    )
    .await;
    // On B: a public child, a sub-space with its own child, and a room restricted to the space.
    let r2 = create_room(&client, &b.base, &bob_token, room_body("R2", "public_chat")).await;
    let sub = create_room(&client, &b.base, &bob_token, space_body("Sub")).await;
    let r3 = create_room(&client, &b.base, &bob_token, room_body("R3", "public_chat")).await;
    let restricted = create_room(
        &client,
        &b.base,
        &bob_token,
        restricted_body("Restricted", &space, &a.name),
    )
    .await;

    let via_a = json!({ "via": [a.name] });
    let via_b = json!({ "via": [b.name] });
    link(&client, &a.base, &alice_token, &space, &r1, via_a.clone()).await;
    link(
        &client,
        &a.base,
        &alice_token,
        &space,
        &private,
        via_a.clone(),
    )
    .await;
    link(
        &client,
        &a.base,
        &alice_token,
        &space,
        &suggested,
        json!({ "via": [a.name], "suggested": true }),
    )
    .await;
    link(&client, &a.base, &alice_token, &space, &r2, via_b.clone()).await;
    link(&client, &a.base, &alice_token, &space, &sub, via_b.clone()).await;
    link(
        &client,
        &a.base,
        &alice_token,
        &space,
        &restricted,
        via_b.clone(),
    )
    .await;
    link(&client, &b.base, &bob_token, &sub, &r3, via_b).await;

    // Alice, on A, sees everything but the restricted room: B does not hold the space, so it
    // cannot tell that A has a user in it, and answers 404 for the room.
    let (status, body) = hierarchy(&client, &a.base, &alice_token, &space, "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        room_ids(&body),
        [
            space.as_str(),
            r1.as_str(),
            private.as_str(),
            suggested.as_str(),
            r2.as_str(),
            sub.as_str(),
            r3.as_str()
        ]
    );
    assert!(body.get("next_batch").is_none(), "{body}");
    let rooms = body["rooms"].as_array().unwrap();
    let by_id = |id: &str| rooms.iter().find(|r| r["room_id"] == id).unwrap();
    assert_eq!(by_id(&space)["room_type"], "m.space");
    assert_eq!(by_id(&space)["world_readable"], true);
    assert_eq!(by_id(&space)["children_state"].as_array().unwrap().len(), 6);
    assert_eq!(by_id(&r2)["name"], "R2", "described by B");
    assert_eq!(by_id(&r2)["join_rule"], "public");
    assert_eq!(by_id(&r2)["children_state"], json!([]));
    assert_eq!(by_id(&sub)["room_type"], "m.space");
    assert_eq!(
        by_id(&sub)["children_state"][0]["state_key"],
        r3,
        "B's sub-space lists its child"
    );

    // Carol, on A, is not in the private room and cannot see it.
    let (status, body) = hierarchy(&client, &a.base, &carol_token, &space, "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        room_ids(&body),
        [
            space.as_str(),
            r1.as_str(),
            suggested.as_str(),
            r2.as_str(),
            sub.as_str(),
            r3.as_str()
        ]
    );

    // suggested_only keeps the one suggested link.
    let (status, body) = hierarchy(
        &client,
        &a.base,
        &alice_token,
        &space,
        "?suggested_only=true",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(room_ids(&body), [space.as_str(), suggested.as_str()]);
    assert_eq!(
        body["rooms"][0]["children_state"].as_array().unwrap().len(),
        1,
        "only the suggested link is listed: {body}"
    );

    // max_depth=1 stops above R3.
    let (status, body) = hierarchy(&client, &a.base, &alice_token, &space, "?max_depth=1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        room_ids(&body),
        [
            space.as_str(),
            r1.as_str(),
            private.as_str(),
            suggested.as_str(),
            r2.as_str(),
            sub.as_str()
        ]
    );

    // A limit pages the walk, and the token resumes it where it stopped.
    let (status, page1) = hierarchy(&client, &a.base, &alice_token, &space, "?limit=3").await;
    assert_eq!(status, 200, "{page1}");
    assert_eq!(
        room_ids(&page1),
        [space.as_str(), r1.as_str(), private.as_str()]
    );
    let token = page1["next_batch"]
        .as_str()
        .unwrap_or_else(|| panic!("no next_batch: {page1}"))
        .to_owned();
    let (status, page2) = hierarchy(
        &client,
        &a.base,
        &alice_token,
        &space,
        &format!("?from={token}"),
    )
    .await;
    assert_eq!(status, 200, "{page2}");
    assert_eq!(
        room_ids(&page2),
        [suggested.as_str(), r2.as_str(), sub.as_str(), r3.as_str()]
    );
    assert!(page2.get("next_batch").is_none(), "{page2}");
    // The token belongs to alice's walk with its filters; anything else is refused.
    let (status, body) = hierarchy(
        &client,
        &a.base,
        &carol_token,
        &space,
        &format!("?from={token}"),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errcode"], "M_INVALID_PARAM");
    let (status, body) = hierarchy(
        &client,
        &a.base,
        &alice_token,
        &space,
        &format!("?from={token}&suggested_only=true"),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errcode"], "M_INVALID_PARAM");
    let (status, body) = hierarchy(&client, &a.base, &alice_token, &space, "?from=nonsense").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errcode"], "M_INVALID_PARAM");
    let (status, body) = hierarchy(&client, &a.base, &alice_token, &space, "?limit=0").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errcode"], "M_INVALID_PARAM");

    // A root carol may not see is forbidden, as is one nobody holds.
    let (status, body) = hierarchy(&client, &a.base, &carol_token, &private, "").await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["errcode"], "M_FORBIDDEN");
    let (status, body) = hierarchy(
        &client,
        &b.base,
        &bob_token,
        &format!("!nowhere:{}", a.name),
        "",
    )
    .await;
    assert_eq!(status, 403, "{body}");

    // Bob, on B, is not in the space and B does not hold it: nothing to walk from.
    let (status, body) = hierarchy(&client, &b.base, &bob_token, &space, "").await;
    assert_eq!(status, 403, "{body}");

    // Bob joins the space through A. Now B holds it, and walks it: its own rooms locally, A's
    // over federation, and A refuses the private room to a server with nobody in it.
    join(&client, &b.base, &bob_token, &space, &a.name).await;
    let body = hierarchy_until(
        &client,
        &b.base,
        &bob_token,
        &space,
        &[
            space.as_str(),
            r1.as_str(),
            suggested.as_str(),
            r2.as_str(),
            sub.as_str(),
            r3.as_str(),
            restricted.as_str(),
        ],
    )
    .await;
    let rooms = body["rooms"].as_array().unwrap();
    let by_id = |id: &str| rooms.iter().find(|r| r["room_id"] == id).unwrap();
    assert_eq!(by_id(&r1)["name"], "R1", "described by A");
    assert_eq!(by_id(&restricted)["join_rule"], "restricted");
    assert_eq!(by_id(&restricted)["allowed_room_ids"], json!([space]));
    assert!(
        by_id(&space)["num_joined_members"].as_u64().unwrap() >= 2,
        "{body}"
    );

    // And now that B holds the space and sees alice in it, it describes the restricted room to
    // A: alice, who is in the space, is shown it; carol, who is not, is not.
    hierarchy_until(
        &client,
        &a.base,
        &alice_token,
        &space,
        &[
            &space,
            &r1,
            &private,
            &suggested,
            &r2,
            &sub,
            &r3,
            &restricted,
        ],
    )
    .await;
    let (status, body) = hierarchy(&client, &a.base, &carol_token, &space, "").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        room_ids(&body),
        [
            space.as_str(),
            r1.as_str(),
            suggested.as_str(),
            r2.as_str(),
            sub.as_str(),
            r3.as_str()
        ]
    );
    assert_eq!(alice, format!("@alice:{}", a.name));

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
