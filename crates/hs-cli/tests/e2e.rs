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
    // `media.storage.path` is set explicitly, not left at its default. That default is the
    // *relative* `./media-store` (`hs_config::MediaStorageBackend::Default`), which for a test is
    // the crate's working directory — so every media upload in this file used to write real bytes
    // into the repository, which is how 15 test artifacts ended up committed once already (see
    // `docs/next-steps.md`'s known-gaps table). Pointing it inside the same tempdir as the
    // storage backend means the files go away with the test.
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: true\n",
        data_dir, media_dir
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

/// The three surfaces mounted in this pass -- `hs-user`'s `/sync`, `hs-e2e`'s `/keys` and
/// `hs-push`'s `/pushrules` and `/pushers` -- answering through the real binary, as a real logged-in
/// user, with real storage behind them. Mounting is the step that turns a finished crate into a
/// served one, and `docs/next-steps.md` records that four crates sat finished and unserved for most
/// of a day because nobody took it; this test is what makes a regression of that visible.
///
/// Deliberately asserts on *behavior*, not just "not 404": an unauthenticated request must be
/// refused, and an authenticated one must come back with the shape the spec names. A route that is
/// mounted but broken would pass a 404 check and fail this.
#[tokio::test]
async fn sync_keys_and_push_surfaces_answer_through_the_real_binary() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());

    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    // Every one of these refuses an anonymous caller before we bother logging in: proof the routes
    // are mounted *behind* authentication rather than merely present.
    for path in [
        "/_matrix/client/v3/sync",
        "/_matrix/client/v3/pushrules/",
        "/_matrix/client/v3/pushers",
        "/_matrix/client/v3/joined_rooms",
    ] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path} should require a token"
        );
    }

    let register: serde_json::Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": "surfaces",
            "password": "hunter2-surfaces",
            "auth": {"type": "m.login.dummy"},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = register["access_token"]
        .as_str()
        .expect("an access token")
        .to_owned();
    let device_id = register["device_id"]
        .as_str()
        .expect("a device id")
        .to_owned();
    let auth = |req: reqwest::RequestBuilder| req.bearer_auth(&token);

    // --- hs-push: the default ruleset is served, and it is the spec's, not an empty one ---------
    let rules_response = auth(client.get(format!("{base}/_matrix/client/v3/pushrules/")))
        .send()
        .await
        .unwrap();
    assert_eq!(rules_response.status(), reqwest::StatusCode::OK);
    let rules: serde_json::Value = rules_response.json().await.unwrap();
    let underride = rules["global"]["underride"]
        .as_array()
        .expect("the default ruleset has underride rules");
    assert!(
        underride.iter().any(|r| r["rule_id"] == ".m.rule.message"),
        "a brand new user must get the spec's predefined rules: {rules}"
    );

    // Disabling a rule persists and is readable back through the sub-resource the spec defines.
    let disable = auth(client.put(format!(
        "{base}/_matrix/client/v3/pushrules/global/underride/.m.rule.message/enabled"
    )))
    .json(&json!({"enabled": false}))
    .send()
    .await
    .unwrap();
    assert_eq!(disable.status(), reqwest::StatusCode::OK);
    let enabled: serde_json::Value = auth(client.get(format!(
        "{base}/_matrix/client/v3/pushrules/global/underride/.m.rule.message/enabled"
    )))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(enabled["enabled"], false, "the write must have persisted");

    // --- hs-push: pushers round-trip ------------------------------------------------------------
    let set_pusher = auth(client.post(format!("{base}/_matrix/client/v3/pushers/set")))
        .json(&json!({
            "pushkey": "a-pushkey",
            "app_id": "com.example.app",
            "kind": "http",
            "app_display_name": "Example",
            "device_display_name": "Phone",
            "lang": "en",
            "data": {"url": "https://push.example.org/_matrix/push/v1/notify"},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(set_pusher.status(), reqwest::StatusCode::OK);
    let pushers: serde_json::Value = auth(client.get(format!("{base}/_matrix/client/v3/pushers")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        pushers["pushers"][0]["pushkey"], "a-pushkey",
        "the pusher just set must come back: {pushers}"
    );

    // --- hs-e2e: device keys upload, and the one-time-key count comes back ----------------------
    let upload: serde_json::Value =
        auth(client.post(format!("{base}/_matrix/client/v3/keys/upload")))
            .json(&json!({
                "device_keys": {
                    "user_id": register["user_id"],
                    "device_id": device_id,
                    "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                    "keys": {format!("curve25519:{device_id}"): "curve25519+key+material"},
                    "signatures": {},
                },
                "one_time_keys": {"signed_curve25519:AAAAAQ": {"key": "otk-material"}},
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(
        upload["one_time_key_counts"]["signed_curve25519"], 1,
        "the server must report the key it just stored: {upload}"
    );

    // The keys read back through /keys/query, which is the call every other user's client makes.
    let query: serde_json::Value =
        auth(client.post(format!("{base}/_matrix/client/v3/keys/query")))
            .json(&json!({"device_keys": {register["user_id"].as_str().unwrap(): []}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(
        query["device_keys"][register["user_id"].as_str().unwrap()][&device_id]["device_id"],
        serde_json::Value::String(device_id.clone()),
        "the uploaded device must be queryable: {query}"
    );

    // --- hs-user: /sync answers, with a next_batch a client can come back with -----------------
    let sync: serde_json::Value =
        auth(client.get(format!("{base}/_matrix/client/v3/sync?timeout=0")))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let next_batch = sync["next_batch"].as_str().expect("a next_batch token");
    assert!(
        next_batch.starts_with("hsu1_"),
        "next_batch should be one of our own tokens: {next_batch}"
    );

    // And the token is accepted on the way back in, which is the whole point of issuing it.
    let incremental = auth(client.get(format!(
        "{base}/_matrix/client/v3/sync?timeout=0&since={next_batch}"
    )))
    .send()
    .await
    .unwrap();
    assert_eq!(incremental.status(), reqwest::StatusCode::OK);

    handle.shutdown().await;
}

/// A room created through the client-server API shows up in its creator's `/sync`, and a message
/// sent afterwards arrives in an incremental sync -- with nothing calling `watch_room` by hand.
///
/// This is the discovery gap closing (`docs/rfcs/0012-room-registry-global-updates.md`): before
/// `RoomRegistry::subscribe_global` existed, `hs-user` only learned a room existed if something
/// explicitly told it, and nothing in the server did, so every room was invisible to `/sync` in
/// production no matter how well sync itself worked. That is exactly the kind of gap that unit
/// tests on either crate cannot see, so the test lives here, against the running binary.
#[tokio::test]
async fn a_room_created_over_http_reaches_its_creators_sync() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());

    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    let register: serde_json::Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": "discovery",
            "password": "hunter2-discovery",
            "auth": {"type": "m.login.dummy"},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = register["access_token"]
        .as_str()
        .expect("an access token")
        .to_owned();

    let created: serde_json::Value = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(&token)
        .json(&json!({"preset": "private_chat", "name": "Discovered"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"].as_str().expect("a room id").to_owned();

    // The fan-out runs in a background task off the registry's global stream, so poll rather than
    // sleep a fixed amount: fast when it works, and a clear failure rather than a flake when it
    // does not.
    let mut sync = serde_json::Value::Null;
    for _ in 0..40 {
        sync = client
            .get(format!("{base}/_matrix/client/v3/sync?timeout=0"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if sync["rooms"]["join"].get(&room_id).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let room = sync["rooms"]["join"]
        .get(&room_id)
        .unwrap_or_else(|| panic!("the created room should reach /sync: {sync}"));
    assert!(
        !room["state"]["events"].as_array().unwrap().is_empty(),
        "the room's current state should come with it: {room}"
    );
    let since = sync["next_batch"]
        .as_str()
        .expect("a next_batch")
        .to_owned();

    // And a message sent after that token arrives in the next incremental sync.
    let sent = client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/txn-1"
        ))
        .bearer_auth(&token)
        .json(&json!({"msgtype": "m.text", "body": "found you"}))
        .send()
        .await
        .unwrap();
    assert_eq!(sent.status(), reqwest::StatusCode::OK);

    let mut found = false;
    for _ in 0..40 {
        let incremental: serde_json::Value = client
            .get(format!(
                "{base}/_matrix/client/v3/sync?timeout=0&since={since}"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let events = incremental["rooms"]["join"][&room_id]["timeline"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if events.iter().any(|e| e["content"]["body"] == "found you") {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(found, "the message should arrive in an incremental sync");

    handle.shutdown().await;
}

/// An invite reaches its target's `/sync` even though that user has never created, joined or
/// synced anything -- the case `docs/rfcs/0012-room-registry-global-updates.md` singles out as the
/// reason the discovery hook has to catch a room at creation rather than on first use.
#[tokio::test]
async fn an_invite_reaches_a_user_who_has_never_synced() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());

    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    let register = |username: &'static str| {
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/_matrix/client/v3/register"))
                .json(&json!({
                    "username": username,
                    "password": "hunter2-invites",
                    "auth": {"type": "m.login.dummy"},
                }))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };

    let inviter = register("inviter").await;
    let invitee = register("invitee").await;
    let inviter_token = inviter["access_token"].as_str().unwrap().to_owned();
    let invitee_token = invitee["access_token"].as_str().unwrap().to_owned();
    let invitee_id = invitee["user_id"].as_str().unwrap().to_owned();

    let created: serde_json::Value = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(&inviter_token)
        .json(&json!({"preset": "private_chat", "name": "Invited"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"].as_str().expect("a room id").to_owned();

    let invited = client
        .post(format!("{base}/_matrix/client/v3/rooms/{room_id}/invite"))
        .bearer_auth(&inviter_token)
        .json(&json!({"user_id": invitee_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(invited.status(), reqwest::StatusCode::OK);

    // The invitee's very first sync: they have done nothing at all until this moment.
    let mut sync = serde_json::Value::Null;
    for _ in 0..40 {
        sync = client
            .get(format!("{base}/_matrix/client/v3/sync?timeout=0"))
            .bearer_auth(&invitee_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if sync["rooms"]["invite"].get(&room_id).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let invite = sync["rooms"]["invite"]
        .get(&room_id)
        .unwrap_or_else(|| panic!("the invite should reach the invitee's first sync: {sync}"));

    // `invite_state` carries the stripped state a client needs to render the invite before joining.
    let events = invite["invite_state"]["events"]
        .as_array()
        .expect("invite_state.events");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "m.room.name" && e["content"]["name"] == "Invited"),
        "stripped state should let a client name the room: {events:?}"
    );

    handle.shutdown().await;
}

/// The key server: `GET /_matrix/key/v2/server` answers without authentication, and what it
/// answers is a *valid self-signature* over this server's own keys -- checked here by verifying
/// the response against the very key it publishes, the way a remote homeserver would before
/// trusting anything else this server says.
///
/// Unauthenticated is the point: it is the one federation endpoint a remote can call before it
/// has any key to sign with, so it must sit outside the `X-Matrix` layer. The second half of this
/// test confirms everything else does not.
#[tokio::test]
async fn the_key_server_publishes_a_self_signed_key_and_federation_requires_signatures() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());

    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/_matrix/key/v2/server"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();

    assert_eq!(body["server_name"], "example.org");
    let valid_until = body["valid_until_ts"].as_i64().expect("valid_until_ts");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        valid_until > now,
        "a key response that is already expired is useless: {valid_until} <= {now}"
    );

    let verify_keys = body["verify_keys"].as_object().expect("verify_keys");
    assert_eq!(
        verify_keys.len(),
        1,
        "one active signing key: {verify_keys:?}"
    );
    let (key_id, key) = verify_keys.iter().next().unwrap();
    let encoded = key["key"].as_str().expect("base64 public key");

    // Verify the response's own signature with the published key. If the server signed events
    // with one key and advertised another, this is what would catch it.
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .expect("the published key must be unpadded base64");
    let bytes: [u8; 32] = raw.try_into().expect("an ed25519 public key is 32 bytes");
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&bytes).expect("a valid key");

    let canonical = hs_model::signing::to_signable_object(&body).expect("canonicalizable");
    hs_model::signing::verify_object(&canonical, "example.org", key_id, &verifying_key)
        .expect("the key response must verify against the key it publishes");

    // Every other federation endpoint is behind the X-Matrix layer, including under the real
    // mount prefix (which is where a prefix-stripping router would silently break verification).
    let unsigned = client
        .get(format!("{base}/_matrix/federation/v1/version"))
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), reqwest::StatusCode::UNAUTHORIZED);

    handle.shutdown().await;
}
