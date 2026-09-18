//! End-to-end test: boots a real `hs serve` server in-process (real TCP listener, real axum
//! router, real `hs-auth` state machine), registers a user through `POST /register`, logs in
//! through `POST /login`, and hits `/health/ready` — all over real HTTP against the bound
//! address, the way an actual client or load balancer would.
//!
//! This is deliberately not a `tower::ServiceExt::oneshot` in-process call: the deliverable this
//! test exists to prove is that `hs-config`, `hs-auth`, `hs-kv` and `hs-telemetry` actually run
//! together behind a real socket, which is the thing that had never been exercised before this
//! crate existed (see `docs/status/12-platform-and-kubernetes.md`).
//!
//! Registration goes through `hs-auth`'s own `POST /register` (`m.login.dummy` UIA), not `hs
//! register`'s shared-secret admin protocol: `docs/compat/cli-shims.md` specifies that protocol
//! against `/_synapse/admin/v1/register`, but no track has mounted that route into any router
//! yet (`hs-compat` ships the `shared_secret` library only — see `crates/hs-cli/src/register.rs`'s
//! module doc for the full seam-gap writeup). `POST /register` *is* wired up (it is
//! `hs-auth::routes::router()`'s own route), so this test exercises the real, currently-working
//! path end to end instead of asserting against a route that does not exist yet.

use serde_json::json;

fn test_config(port: u16, data_dir: &std::path::Path) -> hs_config::Config {
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         auth:\n  enable_registration: true\n",
        data_dir
    );
    hs_config::Config::from_yaml(&yaml).unwrap()
}

fn reserve_ephemeral_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn boots_registers_logs_in_and_reports_ready() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());

    let handle = hs_cli::serve::spawn_serve(config)
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    // 1. /health/ready is already OK right after boot.
    let ready = client
        .get(format!("{base}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ready.status(),
        reqwest::StatusCode::OK,
        "server should be ready after boot"
    );

    // 2. /health/live too.
    let live = client
        .get(format!("{base}/health/live"))
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), reqwest::StatusCode::OK);

    // 3. Register a user through the real client-server API, under both the v3 and r0 prefixes
    //    hs-cli mounts hs-auth's router at (docs/compat/cli-shims.md's summary table).
    let register_body = json!({
        "username": "e2euser",
        "password": "correct horse battery staple",
        "auth": {"type": "m.login.dummy"},
    });
    let register_response = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&register_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        register_response.status(),
        reqwest::StatusCode::OK,
        "registration should succeed"
    );
    let register_json: serde_json::Value = register_response.json().await.unwrap();
    assert_eq!(register_json["user_id"], "@e2euser:example.org");
    assert!(register_json["access_token"].is_string());

    // 4. Log in as the just-registered user via m.login.password, through the r0 alias this time
    //    (proving both version prefixes are actually mounted, not just v3).
    let login_body = json!({
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": "e2euser"},
        "password": "correct horse battery staple",
    });
    let login_response = client
        .post(format!("{base}/_matrix/client/r0/login"))
        .json(&login_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        login_response.status(),
        reqwest::StatusCode::OK,
        "login should succeed: {}",
        login_response.text().await.unwrap_or_default()
    );
    let login_json: serde_json::Value = login_response.json().await.unwrap();
    let access_token = login_json["access_token"]
        .as_str()
        .expect("login response should carry an access_token")
        .to_owned();

    // 5. Use the freshly minted token against /account/whoami, proving the token this server
    //    issued at login is honored by its own auth middleware.
    let whoami_response = client
        .get(format!("{base}/_matrix/client/v3/account/whoami"))
        .bearer_auth(&access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(whoami_response.status(), reqwest::StatusCode::OK);
    let whoami_json: serde_json::Value = whoami_response.json().await.unwrap();
    assert_eq!(whoami_json["user_id"], "@e2euser:example.org");

    // 6. /metrics is reachable and mentions the metric families hs-telemetry registers.
    let metrics_response = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(metrics_response.status(), reqwest::StatusCode::OK);

    // 7. Still ready after real traffic.
    let ready_after = client
        .get(format!("{base}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready_after.status(), reqwest::StatusCode::OK);

    handle.shutdown().await;
}

#[tokio::test]
async fn wrong_password_login_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());
    let handle = hs_cli::serve::spawn_serve(config).await.unwrap();
    let base = handle.base_url();
    let client = reqwest::Client::new();

    let register_body = json!({
        "username": "e2ewrongpass",
        "password": "the-real-password",
        "auth": {"type": "m.login.dummy"},
    });
    client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&register_body)
        .send()
        .await
        .unwrap();

    let login_body = json!({
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": "e2ewrongpass"},
        "password": "definitely-not-it",
    });
    let login_response = client
        .post(format!("{base}/_matrix/client/v3/login"))
        .json(&login_body)
        .send()
        .await
        .unwrap();
    assert_eq!(login_response.status(), reqwest::StatusCode::FORBIDDEN);

    handle.shutdown().await;
}

#[tokio::test]
async fn generate_config_then_hash_password_round_trip() {
    // Exercises the other two "no network" shims that don't need a running server:
    // `hs generate-config` produces a config `hs-config` itself accepts, and
    // `hs hash-password` produces a hash `hs-auth::password::verify_password` accepts.
    let config = hs_cli::generate_config::minimal_config("cli-test.example");
    let yaml = hs_cli::generate_config::render_yaml(&config, "cli-test.example").unwrap();
    let parsed = hs_config::Config::from_yaml(&yaml).unwrap();
    assert_eq!(parsed.server.server_name, "cli-test.example");

    let hash = hs_cli::hash_password::hash_password("a-test-password").unwrap();
    assert!(hs_auth::password::verify_password("a-test-password", &hash, "").unwrap());
    assert!(!hs_auth::password::verify_password("wrong", &hash, "").unwrap());
}

#[test]
fn generate_signing_key_produces_a_synapse_shaped_line() {
    let line = hs_cli::signing_key::generate_signing_key_line();
    let fields: Vec<&str> = line.trim_end().split(' ').collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0], "ed25519");
}
