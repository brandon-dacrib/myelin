//! `GET /rooms/{roomId}/members?at=` on a real `hs serve`: Complement's
//! `TestGetRoomMembersAtPoint`, step for step (a fresh sync's `prev_batch` names the point before
//! bob joined, and the members there are alice alone), and a token no event precedes, which is
//! `404 M_NOT_FOUND` as Synapse answers ("Can't find event for token") rather than the current
//! members.

use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        done["user_id"].as_str().unwrap().to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_at_a_point_are_the_members_then_and_before_every_event_not_found() {
    let port = reserve_port();
    let dir = tempfile::tempdir().unwrap();
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  enabled: false\n",
        data = dir.path().join("data"),
        media = dir.path().join("media"),
    );
    let server = hs_cli::serve::spawn_serve(
        hs_config::Config::from_yaml(&yaml).unwrap(),
        hs_cli::serve::ServeOptions::default(),
    )
    .await
    .expect("the server boots");
    let base = server.base_url();
    let client = reqwest::Client::new();
    let (alice, alice_token) = register(&client, &base, "alice").await;
    let (bob, bob_token) = register(&client, &base, "bob").await;

    let created: Value = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "public_chat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    let send = |token: String, txn: &'static str, body: &'static str| {
        let (client, base, room_id) = (client.clone(), base.clone(), room_id.clone());
        async move {
            let status = client
                .put(format!(
                    "{base}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/{txn}"
                ))
                .bearer_auth(token)
                .json(&json!({"msgtype": "m.text", "body": body}))
                .send()
                .await
                .unwrap()
                .status();
            assert!(status.is_success(), "{status}");
        }
    };
    send(alice_token.clone(), "t1", "Hello world!").await;

    let sync: Value = client
        .get(format!("{base}/_matrix/client/v3/sync?timeout=0"))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let at = sync["rooms"]["join"][&room_id]["timeline"]["prev_batch"]
        .as_str()
        .unwrap_or_else(|| panic!("no prev_batch: {sync}"))
        .to_owned();

    let joined = client
        .post(format!("{base}/_matrix/client/v3/rooms/{room_id}/join"))
        .bearer_auth(&bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status();
    assert!(joined.is_success(), "{joined}");
    send(bob_token.clone(), "t2", "Hello back").await;

    let members = |at: String| {
        let (client, base, room_id, token) = (
            client.clone(),
            base.clone(),
            room_id.clone(),
            alice_token.clone(),
        );
        async move {
            let response = client
                .get(format!("{base}/_matrix/client/v3/rooms/{room_id}/members"))
                .query(&[("at", at)])
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            (
                response.status().as_u16(),
                response.json::<Value>().await.unwrap(),
            )
        }
    };
    let (status, body) = members(at).await;
    assert_eq!(status, 200, "{body}");
    let keys: Vec<&str> = body["chunk"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            assert_eq!(e["room_id"], room_id.as_str());
            e["state_key"].as_str().unwrap()
        })
        .collect();
    assert_eq!(keys, [alice.as_str()], "{body}");
    assert!(!keys.contains(&bob.as_str()));

    // The point before the room's first event: the start of a backward page that has reached
    // the room's creation.
    let page: Value = client
        .get(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/messages?dir=b&limit=100"
        ))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let oldest = page["chunk"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(oldest["type"], "m.room.create", "{page}");
    let before_everything = match page["end"].as_str() {
        Some(end) => end.to_owned(),
        None => {
            // The page reached the start and says so by leaving `end` out: ask from just before
            // the create event instead.
            let context: Value = client
                .get(format!(
                    "{base}/_matrix/client/v3/rooms/{room_id}/context/{}?limit=0",
                    oldest["event_id"].as_str().unwrap()
                ))
                .bearer_auth(&alice_token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            context["start"].as_str().unwrap().to_owned()
        }
    };
    let (status, body) = members(before_everything).await;
    assert_eq!(
        (status, &body["errcode"]),
        (404, &json!("M_NOT_FOUND")),
        "{body}"
    );

    server.shutdown().await;
}
