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

    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    // 0. GET /_matrix/client/versions: the single most important endpoint, per the integration
    //    review that flagged it missing — every client and bridge calls this first, and
    //    Complement's image contract requires it to answer 200 before any test runs.
    let versions_response = client
        .get(format!("{base}/_matrix/client/versions"))
        .send()
        .await
        .unwrap();
    assert_eq!(versions_response.status(), reqwest::StatusCode::OK);
    let versions_json: serde_json::Value = versions_response.json().await.unwrap();
    assert!(
        versions_json["versions"]
            .as_array()
            .unwrap()
            .contains(&json!("v1.1"))
    );
    assert!(versions_json["unstable_features"].is_object());

    // 0b. GET /_matrix/client/v3/capabilities, also called by every client right after login.
    let capabilities_response = client
        .get(format!("{base}/_matrix/client/v3/capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(capabilities_response.status(), reqwest::StatusCode::OK);
    let capabilities_json: serde_json::Value = capabilities_response.json().await.unwrap();
    assert_eq!(
        capabilities_json["capabilities"]["m.change_password"]["enabled"],
        true
    );

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

    // 6. /metrics is reachable, and after the traffic above actually has data in it (this was
    //    the second gap the integration review found: the registry existed and was served, but
    //    nothing ever incremented it, so every scrape came back empty).
    let metrics_response = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(metrics_response.status(), reqwest::StatusCode::OK);
    let metrics_body = metrics_response.text().await.unwrap();
    assert!(
        metrics_body.contains("hs_http_requests_total"),
        "{metrics_body}"
    );
    assert!(
        metrics_body.contains("route=\"/_matrix/client/versions\""),
        "{metrics_body}"
    );
    assert!(
        metrics_body.contains("route=\"/_matrix/client/v3/register\""),
        "{metrics_body}"
    );

    // 7. Still ready after real traffic.
    let ready_after = client
        .get(format!("{base}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready_after.status(), reqwest::StatusCode::OK);

    // 8. Create a room (hs-room's router, mounted alongside hs-auth's under both /_matrix/client
    //    prefixes -- see docs/status/12-platform-and-kubernetes.md, "mounting what already
    //    exists"). This is the deliverable this test extension exists to prove: hs-room's router
    //    is actually served, not just built and unit-tested in its own crate.
    let create_room_response = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(&access_token)
        .json(&json!({"preset": "private_chat", "name": "e2e test room"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        create_room_response.status(),
        reqwest::StatusCode::OK,
        "createRoom should succeed: {}",
        create_room_response.text().await.unwrap_or_default()
    );
    let create_room_json: serde_json::Value = create_room_response.json().await.unwrap();
    let room_id = create_room_json["room_id"]
        .as_str()
        .expect("createRoom response should carry a room_id")
        .to_owned();

    // 9. Send a message into it.
    let send_response = client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/e2e-txn-1"
        ))
        .bearer_auth(&access_token)
        .json(&json!({"msgtype": "m.text", "body": "hello from the e2e test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        send_response.status(),
        reqwest::StatusCode::OK,
        "sending a message should succeed: {}",
        send_response.text().await.unwrap_or_default()
    );
    let send_json: serde_json::Value = send_response.json().await.unwrap();
    let sent_event_id = send_json["event_id"]
        .as_str()
        .expect("send response should carry an event_id")
        .to_owned();

    // 10. Read it back through /context/{eventId}, proving the event that was actually
    //     persisted (not just accepted) round-trips over the same HTTP surface.
    let context_response = client
        .get(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/context/{sent_event_id}"
        ))
        .bearer_auth(&access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        context_response.status(),
        reqwest::StatusCode::OK,
        "reading the sent message back should succeed: {}",
        context_response.text().await.unwrap_or_default()
    );
    let context_json: serde_json::Value = context_response.json().await.unwrap();
    assert_eq!(context_json["event"]["event_id"], sent_event_id);
    assert_eq!(
        context_json["event"]["content"]["body"],
        "hello from the e2e test"
    );

    // 11. Upload a media file (hs-media's authenticated router, mounted under
    //     /_matrix/client/v1/media -- the other half of "mounting what already exists").
    let upload_response = client
        .post(format!(
            "{base}/_matrix/client/v1/media/upload?filename=hello.txt"
        ))
        .bearer_auth(&access_token)
        .header("content-type", "text/plain")
        .body("hello from the e2e test's media upload")
        .send()
        .await
        .unwrap();
    assert_eq!(
        upload_response.status(),
        reqwest::StatusCode::OK,
        "media upload should succeed: {}",
        upload_response.text().await.unwrap_or_default()
    );
    let upload_json: serde_json::Value = upload_response.json().await.unwrap();
    let content_uri = upload_json["content_uri"]
        .as_str()
        .expect("upload response should carry a content_uri")
        .to_owned();
    assert!(content_uri.starts_with("mxc://example.org/"));
    let media_id = content_uri
        .rsplit('/')
        .next()
        .expect("content_uri has a media id after the last slash");

    // 12. Download it back and check the bytes round-trip exactly.
    let download_response = client
        .get(format!(
            "{base}/_matrix/client/v1/media/download/example.org/{media_id}"
        ))
        .bearer_auth(&access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        download_response.status(),
        reqwest::StatusCode::OK,
        "media download should succeed"
    );
    let downloaded_bytes = download_response.bytes().await.unwrap();
    assert_eq!(
        downloaded_bytes.as_ref(),
        b"hello from the e2e test's media upload"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn wrong_password_login_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());
    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .unwrap();
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
fn routes_manifest_covers_versions_and_the_auth_surface() {
    // The third gap the integration review found: track 14's spec-coverage tool reported 0 of
    // 235 routes purely because nothing wrote routes.json. `hs_cli::serve::route_manifest()`
    // needs no config, no server, no network at all.
    let manifest = hs_cli::serve::route_manifest();
    let paths: Vec<&str> = manifest.routes.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"/_matrix/client/versions"));
    assert!(paths.contains(&"/_matrix/client/v3/capabilities"));
    assert!(paths.contains(&"/_matrix/client/v3/register"));
    assert!(paths.contains(&"/_matrix/client/r0/login"));
}

#[test]
fn generate_signing_key_produces_a_synapse_shaped_line() {
    let line = hs_cli::signing_key::generate_signing_key_line();
    let fields: Vec<&str> = line.trim_end().split(' ').collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0], "ed25519");
}
