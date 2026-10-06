//! A bridge's alias, asked for by another server: server A has a bridge whose alias namespace is
//! `#bridged-*:A`; bob of server B joins `#bridged-room:A`, which nobody has made yet. B asks A
//! (`GET /_matrix/federation/v1/query/directory`), A asks the bridge
//! (`GET /_matrix/app/v1/rooms/{alias}`), the bridge makes the alias, and A answers with the room
//! and the servers in it, so bob joins. Synapse's `DirectoryHandler.get_association` asks the
//! appservices for a federation query as for a client's; until 2026-10-05 this server answered
//! another server `404` without asking.
//!
//! Two in-process `hs serve`s over plain HTTP federation, and the bridge an axum listener here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const AS_TOKEN: &str = "as_token_for_the_federated_alias_test_000000000000000000000000000";
const HS_TOKEN: &str = "hs_token_for_the_federated_alias_test_000000000000000000000000000";

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path, registration: Option<&str>) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let appservices = registration
        .map(|path| format!("appservices:\n  registration_files: [{path:?}]\n"))
        .unwrap_or_default();
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n{appservices}"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
    base: String,
    name: String,
    _dir: tempfile::TempDir,
}

async fn start(port: u16, dir: tempfile::TempDir, registration: Option<&str>) -> Server {
    let handle = hs_cli::serve::spawn_serve(
        config(port, dir.path(), registration),
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
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy"},
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

/// The bridge: what it was asked, and the homeserver and room it points its alias at.
#[derive(Clone, Default)]
struct Bridge {
    asked: Arc<Mutex<Vec<String>>>,
    homeserver: Arc<Mutex<Option<String>>>,
    room: Arc<Mutex<Option<String>>>,
}

async fn start_bridge() -> (String, Bridge) {
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};

    fn authorized(headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {HS_TOKEN}"))
    }

    async fn transaction(headers: HeaderMap) -> (StatusCode, axum::Json<Value>) {
        if !authorized(&headers) {
            return (StatusCode::FORBIDDEN, axum::Json(json!({})));
        }
        (StatusCode::OK, axum::Json(json!({})))
    }

    /// Provides `#bridged-room:{server}`, pointing it at the room the test chose.
    async fn room(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        Path(alias): Path<String>,
    ) -> (StatusCode, axum::Json<Value>) {
        assert!(authorized(&headers));
        bridge.asked.lock().unwrap().push(alias.clone());
        if !alias.starts_with("#bridged-room:") {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"errcode": "M_NOT_FOUND"})),
            );
        }
        let homeserver = bridge.homeserver.lock().unwrap().clone().unwrap();
        let room_id = bridge.room.lock().unwrap().clone().unwrap();
        let made = reqwest::Client::new()
            .put(format!(
                "{homeserver}/_matrix/client/v3/directory/room/{}",
                alias.replace('#', "%23").replace(':', "%3A")
            ))
            .bearer_auth(AS_TOKEN)
            .json(&json!({"room_id": room_id}))
            .send()
            .await
            .unwrap();
        assert!(made.status().is_success(), "{:?}", made.text().await);
        (StatusCode::OK, axum::Json(json!({})))
    }

    let bridge = Bridge::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn}",
            axum::routing::put(transaction),
        )
        .route("/_matrix/app/v1/rooms/{alias}", axum::routing::get(room))
        .with_state(bridge.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, bridge)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_server_asking_for_a_bridges_alias_gets_the_room_the_bridge_makes() {
    let (bridge_url, bridge) = start_bridge().await;
    let a_port = reserve_port();
    let a_name = format!("127.0.0.1:{a_port}");
    let a_dir = tempfile::tempdir().unwrap();
    let registration = a_dir.path().join("bridge.yaml");
    let escaped_name = a_name.replace('.', "\\.");
    std::fs::write(
        &registration,
        format!(
            "id: alias-bridge\nurl: {bridge_url}\nas_token: {AS_TOKEN}\nhs_token: {HS_TOKEN}\n\
             sender_localpart: aliasbot\nrate_limited: false\n\
             namespaces:\n  users: []\n  aliases:\n    - regex: '#bridged-.*:{escaped_name}'\n      exclusive: true\n"
        ),
    )
    .unwrap();
    let a = start(a_port, a_dir, Some(registration.to_str().unwrap())).await;
    let b = start(reserve_port(), tempfile::tempdir().unwrap(), None).await;
    assert_eq!(a.name, a_name);
    *bridge.homeserver.lock().unwrap() = Some(a.base.clone());

    let client = reqwest::Client::new();
    let (_alice, alice) = register(&client, &a.base, "alice").await;
    let (_bob, bob) = register(&client, &b.base, "bob").await;
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice)
        .json(&json!({"preset": "public_chat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    *bridge.room.lock().unwrap() = Some(room_id.clone());

    // Bob joins by the alias nobody has made: B asks A, A asks the bridge.
    let alias = format!("#bridged-room:{a_name}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let joined: Value = loop {
        let response = client
            .post(format!(
                "{}/_matrix/client/v3/join/{}",
                b.base,
                alias.replace('#', "%23").replace(':', "%3A")
            ))
            .bearer_auth(&bob)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "bob never joined by the bridge's alias: {status} {body}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(joined["room_id"], room_id.as_str(), "{joined}");
    assert!(
        bridge.asked.lock().unwrap().contains(&alias),
        "the bridge was asked: {:?}",
        bridge.asked.lock().unwrap()
    );

    // An alias in the namespace the bridge does not provide is still unknown to B.
    let missing = client
        .get(format!(
            "{}/_matrix/client/v3/directory/room/%23bridged-nothing%3A{}",
            b.base,
            a_name.replace(':', "%3A")
        ))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
