//! Sytest's "Invites over federation are correctly pushed with name"
//! (`tests/61push/01message-pushed.pl`) through two real `hs serve` instances in one process,
//! federating over plain HTTP (the harness of `federation_membership.rs`; see
//! `federation_two_servers.rs` for the server-name and port choices): alice on A has an HTTP
//! pusher pointing at a push gateway this test runs; charlie on B creates a named room and
//! invites her. A holds nothing of the room but the invite and the stripped state it carried,
//! and the push A sends for it must still name the room (and the inviter), from that state.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::mpsc;

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
    _handle: hs_cli::serve::ServeHandle,
    base: String,
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
        _handle: handle,
        _dir: dir,
    }
}

struct User {
    id: String,
    token: String,
    base: String,
}

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
        base: base.clone(),
    }
}

async fn post_json(client: &reqwest::Client, user: &User, path: &str, body: Value) -> Value {
    let response = client
        .post(format!("{}/_matrix/client/v3/{path}", user.base))
        .bearer_auth(&user.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "POST {path} answered {status}: {body}");
    body
}

/// A push gateway: every notification posted to it goes down the channel.
async fn push_gateway() -> (String, mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let router = axum::Router::new()
        .route(
            "/_matrix/push/v1/notify",
            post(
                |State(tx): State<mpsc::UnboundedSender<Value>>, Json(body): Json<Value>| async move {
                    let _ = tx.send(body);
                    Json(json!({"rejected": []}))
                },
            ),
        )
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}/_matrix/push/v1/notify"), rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invite_over_federation_is_pushed_with_the_rooms_name() {
    let (a, b) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &a, "alice").await;
    let charlie = register(&client, &b, "charlie").await;
    // The inviter's display name, which the invitee's server also learns only from the
    // invite's stripped state.
    let response = client
        .put(format!(
            "{}/_matrix/client/v3/profile/{}/displayname",
            charlie.base, charlie.id
        ))
        .bearer_auth(&charlie.token)
        .json(&json!({"displayname": "Charlie"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let (gateway_url, mut pushes) = push_gateway().await;
    post_json(
        &client,
        &alice,
        "pushers/set",
        json!({
            "kind": "http",
            "app_id": "sytest",
            "app_display_name": "sytest_display_name",
            "device_display_name": "device_display_name",
            "pushkey": "a_push_key",
            "lang": "en",
            "data": {"url": gateway_url},
        }),
    )
    .await;

    let created = post_json(
        &client,
        &charlie,
        "createRoom",
        json!({"preset": "private_chat", "name": "Test Name"}),
    )
    .await;
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    post_json(
        &client,
        &charlie,
        &format!("rooms/{room_id}/invite"),
        json!({"user_id": alice.id}),
    )
    .await;

    let notification = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let body = pushes.recv().await.expect("the gateway is up");
            if body["notification"]["type"] == "m.room.member" {
                return body["notification"].clone();
            }
        }
    })
    .await
    .expect("A pushes the invite within 30 s");
    assert_eq!(notification["room_id"], room_id.as_str());
    assert_eq!(notification["sender"], charlie.id.as_str());
    assert_eq!(notification["membership"], "invite");
    assert_eq!(notification["user_is_target"], true);
    assert_eq!(
        notification["room_name"], "Test Name",
        "the room's name comes from the invite's stripped state: {notification}"
    );
    assert_eq!(
        notification["sender_display_name"], "Charlie",
        "so does the inviter's display name: {notification}"
    );
}
