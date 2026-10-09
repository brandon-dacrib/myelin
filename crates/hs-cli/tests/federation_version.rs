//! `GET /_matrix/federation/v1/version` answers an unsigned request on a real `hs serve`.
//!
//! The spec (`server-server/version.yaml`) gives the endpoint no `security` requirement;
//! Synapse answers it unsigned, and federation testers and other servers call it without an
//! `X-Matrix` header. Until 2026-10-09 it sat behind the `X-Matrix` layer and the live demo
//! answered an unsigned call `401 M_UNAUTHORIZED` ("signature verification failed"). It is
//! served only when federation is (`404` with `federation.enabled: false`), as Synapse serves it
//! only from its `federation` listener resource.

use serde_json::Value;

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path, federation: bool) -> hs_config::Config {
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media:?}\n\
         federation:\n  enabled: {federation}\n  ip_range_blocklist: []\n",
        data = data_dir.join("data"),
        media = data_dir.join("media"),
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

/// Boots `hs serve` and returns the status and body of an unsigned `GET /version`, and the
/// status of an unsigned `GET /publicRooms` (a signed route, for contrast).
async fn unsigned_version(federation: bool) -> (u16, Value, u16) {
    let port = reserve_port();
    let dir = tempfile::tempdir().unwrap();
    let server = hs_cli::serve::spawn_serve(
        config(port, dir.path(), federation),
        hs_cli::serve::ServeOptions {
            federation_scheme: Some("http"),
            ..Default::default()
        },
    )
    .await
    .expect("the server boots");
    let base = server.base_url();
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/_matrix/federation/v1/version"))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    let public_rooms = client
        .get(format!("{base}/_matrix/federation/v1/publicRooms"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    server.shutdown().await;
    (status, body, public_rooms)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsigned_federation_version_is_answered() {
    let (status, body, public_rooms) = unsigned_version(true).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["server"]["name"], "hs", "{body}");
    assert!(body["server"]["version"].is_string(), "{body}");
    assert_eq!(
        public_rooms, 401,
        "a signed route still refuses an unsigned call"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn federation_version_is_not_served_with_federation_off() {
    let (status, body, public_rooms) = unsigned_version(false).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(public_rooms, 404);
}
