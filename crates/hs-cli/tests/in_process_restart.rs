//! An in-process server can be shut down and started again over the same data directory, in the
//! same process: `ServeHandle::shutdown` stops every task the server spawned and closes the store
//! before it returns.
//!
//! Until it did, the background tasks (the session hub's watchers, the federation sender, the push
//! and appservice workers, ...) kept handles on the embedded store after `shutdown()`, Fjall kept
//! its exclusive lock on the directory, and the second start failed with the lock error; every
//! restart test had to run the real binary as a subprocess.

use serde_json::{Value, json};

/// Any free port, set after parsing (the configuration refuses `0`, see `e2e.rs`).
fn config(data_dir: &std::path::Path) -> hs_config::Config {
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: true\n",
        data_dir.join("data"),
        data_dir.join("media"),
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
}

async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: Option<&str>,
    body: Value,
) -> (reqwest::StatusCode, Value) {
    let mut request = client.request(method, url).json(&body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn login(client: &reqwest::Client, base: &str) -> String {
    let (status, body) = call(
        client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/login"),
        None,
        json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "correct horse battery"}),
    )
    .await;
    assert_eq!(status, 200, "login: {body}");
    body["access_token"].as_str().unwrap().to_owned()
}

/// The server's log on stdout, when `RUST_LOG` asks for it.
fn log_if_asked() -> Option<hs_telemetry::Guard> {
    std::env::var_os("RUST_LOG")?;
    hs_telemetry::init(&hs_telemetry::Options::default()).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_process_server_restarts_over_its_own_data_directory() {
    let _log = log_if_asked();
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();

    // First run: an account, a room, a message -- enough to wake the background work that used
    // to outlive `shutdown()`.
    let handle =
        hs_cli::serve::spawn_serve(config(dir.path()), hs_cli::serve::ServeOptions::default())
            .await
            .expect("the first start");
    let base = handle.base_url();
    let (status, body) = call(
        &client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/register"),
        None,
        json!({"username": "alice", "password": "correct horse battery", "auth": {"type": "m.login.dummy"}}),
    )
    .await;
    assert_eq!(status, 200, "register: {body}");
    let token = body["access_token"].as_str().unwrap().to_owned();
    let (status, created) = call(
        &client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/createRoom"),
        Some(&token),
        json!({"name": "kept"}),
    )
    .await;
    assert_eq!(status, 200, "createRoom: {created}");
    let room = created["room_id"].as_str().unwrap().to_owned();
    let (status, sent) = call(
        &client,
        reqwest::Method::PUT,
        format!("{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/t1"),
        Some(&token),
        json!({"msgtype": "m.text", "body": "before the restart"}),
    )
    .await;
    assert_eq!(status, 200, "send: {sent}");
    let (status, _) = call(
        &client,
        reqwest::Method::GET,
        format!("{base}/_matrix/client/v3/sync?timeout=0"),
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let outlived = handle.shutdown().await;
    assert!(
        outlived.is_empty(),
        "these outlived the first run's shutdown (a reference cycle holds them): {outlived:?}"
    );

    // Second and third runs over the same directory, in this same process.
    for run in 2..=3 {
        let handle =
            hs_cli::serve::spawn_serve(config(dir.path()), hs_cli::serve::ServeOptions::default())
                .await
                .unwrap_or_else(|e| panic!("start {run} over the same data directory: {e}"));
        let base = handle.base_url();
        let token = login(&client, &base).await;
        let (status, messages) = call(
            &client,
            reqwest::Method::GET,
            format!("{base}/_matrix/client/v3/rooms/{room}/messages?dir=b&limit=20"),
            Some(&token),
            Value::Null,
        )
        .await;
        assert_eq!(status, 200, "messages on start {run}: {messages}");
        assert!(
            messages["chunk"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["content"]["body"] == "before the restart"),
            "the message from the first run is still there on start {run}: {messages}"
        );
        let outlived = handle.shutdown().await;
        assert!(outlived.is_empty(), "outlived run {run}: {outlived:?}");
    }
}
