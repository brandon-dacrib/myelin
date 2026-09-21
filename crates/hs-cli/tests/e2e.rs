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

fn test_config_yaml(port: u16, data_dir: &std::path::Path) -> String {
    // `media.storage.path` is set explicitly, not left at its default. That default is the
    // *relative* `./media-store` (`hs_config::MediaStorageBackend::Default`), which for a test is
    // the crate's working directory — so every media upload in this file used to write real bytes
    // into the repository, which is how 15 test artifacts ended up committed once already (see
    // `docs/next-steps.md`'s known-gaps table). Pointing it inside the same tempdir as the
    // storage backend means the files go away with the test.
    let media_dir = data_dir.join("media");
    format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: true\n",
        data_dir, media_dir
    )
}

fn test_config(port: u16, data_dir: &std::path::Path) -> hs_config::Config {
    hs_config::Config::from_yaml(&test_config_yaml(port, data_dir)).unwrap()
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
    let data_dir = tempfile::tempdir().unwrap();
    let yaml = hs_cli::generate_config::render_yaml("cli-test.example", data_dir.path());
    let parsed = hs_config::Config::from_yaml(&yaml).unwrap();
    assert_eq!(parsed.server.server_name, "cli-test.example");
    // The bootstrap file must work as written: no path in it may still point at the process's
    // working directory, which in a container is neither writable nor mounted.
    assert!(parsed.server.signing_key_path.starts_with(data_dir.path()));

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

/// Complement's `TestUnknownEndpoints`, against the real assembled router: an endpoint this
/// server does not have must answer a JSON Matrix error, not axum's empty-bodied 404. A client
/// library that always decodes the error shape fails on the decode rather than on the status, and
/// a bridge cannot tell "this server does not implement that" from "the network ate it".
///
/// It lives here rather than in `hs-http` because what it checks is that the fallback was applied
/// *after* every route and every merge — a unit test of the fallback itself cannot see that, and
/// getting the order wrong silently leaves half the router on the default.
#[tokio::test]
async fn an_endpoint_this_server_does_not_have_answers_a_json_matrix_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(reserve_ephemeral_port(), dir.path());
    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    // The four prefixes Complement asks about, plus a bare unknown one.
    for path in [
        "/_matrix/unknown",
        "/_matrix/client/unknown",
        "/_matrix/client/v3/room/unknown",
        "/_matrix/federation/v1/unknown",
        "/_matrix/key/v2/unknown",
        "/_matrix/media/v3/unknown",
    ] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
        let body: serde_json::Value = response
            .json()
            .await
            .unwrap_or_else(|e| panic!("{path} did not answer JSON: {e}"));
        assert_eq!(body["errcode"], "M_UNRECOGNIZED", "{path}");
    }

    // A path that exists, with a method it does not serve, is 405 rather than 404 — the
    // distinction tells a client "you have the wrong verb", not "upgrade your server".
    let response = client
        .put(format!("{base}/_matrix/client/v3/login"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["errcode"], "M_UNRECOGNIZED");

    // The admin API keeps its own contract: RFC 9457, not a Matrix errcode.
    let response = client
        .get(format!("{base}/api/v1/nonsense"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body.get("errcode").is_none(), "{body}");
    assert!(body["type"].is_string(), "{body}");

    handle.shutdown().await;
}

/// Every client-server route that reads a body, sent a body that is not JSON, through the real
/// assembled router with a real access token -- so the request gets past authentication and
/// reaches whatever parses the body.
///
/// The client-server spec's "Standard error response" section wants a JSON object with an
/// `errcode` for every error, and `M_NOT_JSON` for this one. `axum::Json`'s own rejection is a
/// plain-text `400`/`415`/`422`, which is what a route gets by taking a bare `Json<T>` instead of
/// `hs_http::body::PermissiveJson<T>`. The walk is over the route manifest rather than a list, so
/// a route added tomorrow is covered without anybody remembering this test exists.
///
/// The body is the one Complement's `TestRequestEncodingFails` sends: a JSON string holding a
/// lone `0x81`.
#[tokio::test]
async fn a_body_that_is_not_json_is_m_not_json_on_every_route_never_plain_text() {
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
            "username": "encoding",
            "password": "hunter2-encoding",
            "auth": {"type": "m.login.dummy"},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = register["access_token"].as_str().unwrap().to_owned();
    let user_id = register["user_id"].as_str().unwrap().to_owned();
    let created: serde_json::Value = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(&token)
        .json(&json!({"preset": "private_chat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"].as_str().unwrap().to_owned();

    let not_json: &[u8] = b"{ \"test\":\"a\x81\" }";
    let mut not_json_answers = 0usize;
    let mut report = Vec::new();
    let mut failures = Vec::new();

    let mut routes = hs_cli::serve::route_manifest().routes;
    routes.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
    for route in routes {
        if route.surface != hs_http::Surface::MatrixClient
            || !matches!(route.method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH")
        {
            continue;
        }
        // These take no body and end the session every later request depends on.
        if route.path.contains("/logout") {
            continue;
        }
        let path = route
            .path
            .split('/')
            .map(|segment| match segment {
                "{roomId}" | "{room_id}" => room_id.replace('!', "%21").replace(':', "%3A"),
                "{userId}" | "{user_id}" => user_id.replace('@', "%40").replace(':', "%3A"),
                s if s.starts_with('{') => "placeholder".to_owned(),
                s => s.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("/");
        let response = client
            .request(route.method.parse().unwrap(), format!("{base}{path}"))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body(not_json)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let text = response.text().await.unwrap();
        let errcode = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["errcode"].as_str().map(str::to_owned));
        let label = format!("{} {}", route.method, route.path);
        report.push(format!("{status} {errcode:?} {label}"));

        if errcode.as_deref() == Some("M_NOT_JSON") {
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{label}");
            not_json_answers += 1;
        }
        if (status.is_client_error() || status.is_server_error()) && errcode.is_none() {
            failures.push(format!(
                "{label}: {status} with no errcode ({content_type}): {text:.120}"
            ));
        }
        if matches!(status.as_u16(), 415 | 422) {
            failures.push(format!("{label}: {status} is an axum::Json rejection"));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Without a floor this passes when nothing was tested: a token that stopped working would
    // turn every answer into a well-formed `401 M_UNKNOWN_TOKEN`. 130 of 153 routes answered
    // `M_NOT_JSON` when this was written; the other 23 take no JSON body (media upload,
    // `forget`, the `DELETE`s).
    assert!(
        not_json_answers >= 100,
        "only {not_json_answers} routes answered M_NOT_JSON:\n{}",
        report.join("\n")
    );

    handle.shutdown().await;
}

fn setup_token_of(link: &str) -> &str {
    link.split_once("/admin/setup#token=")
        .unwrap_or_else(|| panic!("not a setup link: {link}"))
        .1
}

/// A server with no administrator says so in one place -- the link `hs serve` logs -- and that
/// link is all it takes to get from an empty data directory to a signed-in administrator. Runs
/// against the real on-disk store, because "exactly one of several simultaneous claims wins" is
/// a property of its transactions, not of the in-memory fake the unit tests use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_setup_link_creates_exactly_one_administrator_however_many_ask_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let handle = hs_cli::serve::spawn_serve(
        test_config(reserve_ephemeral_port(), dir.path()),
        hs_cli::serve::ServeOptions::default(),
    )
    .await
    .expect("server should boot");
    let base = handle.base_url();
    let link = handle
        .setup_link
        .clone()
        .expect("a fresh server offers setup");
    let token = setup_token_of(&link).to_owned();
    assert_eq!(token.len(), 40);
    assert!(token.bytes().all(|b| b.is_ascii_alphabetic()), "{token}");
    assert!(
        link.starts_with(&format!("http://localhost:{}/", handle.addrs[0].port())),
        "{link}"
    );
    assert!(needs_setup(&client, &base).await);

    // A guess is refused as a bad credential, and costs the real token nothing.
    let response = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": "a".repeat(40), "username": "mallory", "password": "hunter2-mallory"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()["content-type"],
        "application/problem+json"
    );

    // Six claims at once, all carrying the right token.
    let mut claims = Vec::new();
    for i in 0..6 {
        let (client, base, token) = (client.clone(), base.clone(), token.clone());
        claims.push(tokio::spawn(async move {
            let response = client
                .post(format!("{base}/api/v1/setup"))
                .json(&json!({
                    "setup_token": token,
                    "username": format!("admin{i}"),
                    "password": "hunter2-first-admin",
                }))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body: serde_json::Value = response.json().await.unwrap();
            (status, body)
        }));
    }
    let mut sessions = Vec::new();
    for claim in claims {
        let (status, body) = claim.await.unwrap();
        match status {
            reqwest::StatusCode::CREATED => sessions.push(body),
            reqwest::StatusCode::CONFLICT => {}
            other => panic!("a claim answered {other}: {body}"),
        }
    }
    assert_eq!(sessions.len(), 1, "exactly one claim wins: {sessions:?}");
    let session = &sessions[0];
    let user_id = session["user_id"].as_str().unwrap();
    let access_token = session["access_token"].as_str().unwrap();

    // The session it handed back is an administrator's, as far as the admin API is concerned...
    let me: serde_json::Value = client
        .get(format!("{base}/api/v1/me"))
        .bearer_auth(access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["id"], user_id, "{me}");
    assert!(
        me["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "admin:write"),
        "{me}"
    );
    let users: serde_json::Value = client
        .get(format!("{base}/api/v1/users"))
        .bearer_auth(access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admins = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|u| u["admin"] == true)
        .count();
    assert_eq!(admins, 1, "one administrator exists, not six: {users}");

    // ...and the password typed into the form is the one the account signs in with afterwards.
    let login = client
        .post(format!("{base}/_matrix/client/v3/login"))
        .json(&json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": user_id},
            "password": "hunter2-first-admin",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::OK);

    // The offer is over, including to the token that just worked.
    assert!(!needs_setup(&client, &base).await);
    let response = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": token, "username": "late", "password": "hunter2-too-late"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);

    handle.shutdown().await;
}

async fn needs_setup(client: &reqwest::Client, base: &str) -> bool {
    let body: serde_json::Value = client
        .get(format!("{base}/api/v1/setup"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["needs_setup"].as_bool().unwrap()
}

/// One run of the real `hs` binary, for what only a real process can show: what an operator
/// reads in the log, and what survives the process ending. (An in-process server cannot be
/// restarted over the same data directory -- its background tasks keep the store's lock until
/// the process exits.)
struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    fn serve(config_path: &std::path::Path) -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("the hs binary should start");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Reads the log until a line contains `needle`. A condition, not a duration: the timeout
    /// only bounds how long a broken server can hang the suite.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needle:?}; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    /// Asks the server to stop the way `docker stop` or Kubernetes would, waits for it to, and
    /// returns everything it logged.
    fn stop(mut self) -> String {
        let pid = self.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let _ = self.child.wait();
        while let Ok(line) = self.lines.recv() {
            self.seen.push(line);
        }
        self.seen.join("\n")
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        // Only reached with the child still running when a test panicked; `stop` has already
        // waited otherwise, and killing an exited child is a harmless error.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the operator actually sees and does, with the real binary: the link is in the log, the
/// same link is in the log after a restart, using it works, and then it is never logged again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_binary_logs_the_same_setup_link_until_it_is_used_and_never_after() {
    let dir = tempfile::tempdir().unwrap();
    let port = reserve_ephemeral_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        test_config_yaml(port, &dir.path().join("data")),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let mut first = HsProcess::serve(&config_path);
    let line = first.wait_for("setup_link=");
    assert!(
        line.contains("WARN"),
        "the one line to act on should stand out: {line}"
    );
    let link = line.split_once("setup_link=").unwrap().1;
    let token: String = setup_token_of(link)
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    assert_eq!(token.len(), 40, "{line}");
    assert!(
        link.starts_with(&format!("http://localhost:{port}/admin/setup#token=")),
        "{line}"
    );
    first.stop();

    // A restart before anybody has used it: yesterday's link still works, because it is the
    // same link.
    let mut second = HsProcess::serve(&config_path);
    let line = second.wait_for("setup_link=");
    assert!(
        line.contains(&token),
        "the token changed across a restart: {line}"
    );

    let response = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let log = second.stop();
    assert!(
        !log.contains("hunter2-first-admin"),
        "the password reached the log"
    );

    // And now there is an administrator, so there is no offer and nothing to log.
    let mut third = HsProcess::serve(&config_path);
    third.wait_for("listening");
    assert!(!needs_setup(&client, &base).await);
    let log = third.stop();
    assert!(
        !log.contains("setup_link"),
        "setup was offered again:\n{log}"
    );
    assert!(log.contains("listening"));
}

/// With `server.public_baseurl` set, the link is one the operator's browser can actually open:
/// the address the server is reached at, not the one it happens to be bound to.
#[tokio::test]
async fn the_setup_link_is_rooted_at_the_public_base_url_when_there_is_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(reserve_ephemeral_port(), dir.path());
    config.server.public_baseurl = Some("https://matrix.example.org/".to_owned());
    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let link = handle.setup_link.clone().unwrap();
    assert!(
        link.starts_with("https://matrix.example.org/admin/setup#token="),
        "{link}"
    );
    handle.shutdown().await;
}

/// The Overview page's numbers, through the real server: what a new administrator sees in the
/// first minute. Before this the two operations answered `501` and the page said "Not
/// implemented" where these go.
#[tokio::test]
async fn the_overview_counts_real_accounts_and_rooms_and_omits_what_nobody_counts() {
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        test_config(reserve_ephemeral_port(), dir.path()),
        hs_cli::serve::ServeOptions::default(),
    )
    .await
    .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    // The administrator, by the front door.
    let token = setup_token_of(handle.setup_link.as_deref().unwrap()).to_owned();
    let admin: serde_json::Value = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_token = admin["access_token"].as_str().unwrap().to_owned();

    // An ordinary user, who makes a room.
    let alice: serde_json::Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": "alice", "password": "hunter2-alice", "auth": {"type": "m.login.dummy"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let created = client
        .post(format!("{base}/_matrix/client/v3/createRoom"))
        .bearer_auth(alice["access_token"].as_str().unwrap())
        .json(&json!({"preset": "private_chat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), reqwest::StatusCode::OK);

    let overview = |client: reqwest::Client, base: String, token: String| async move {
        let response = client
            .get(format!("{base}/api/v1/statistics/overview"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response.json::<serde_json::Value>().await.unwrap()
    };
    let first = overview(client.clone(), base.clone(), admin_token.clone()).await;
    assert_eq!(first["users_count"], 2, "{first}");
    assert_eq!(first["rooms_count"], 1, "{first}");
    // Alice has just made authenticated requests, so she at least is active today.
    assert!(
        first["daily_active_users"].as_u64().unwrap() >= 1,
        "{first}"
    );
    assert!(
        first["monthly_active_users"].as_u64().unwrap()
            >= first["daily_active_users"].as_u64().unwrap(),
        "{first}"
    );
    // Nothing here can count these yet, so they are absent -- not 0, not null.
    for unknown in [
        "media_count",
        "media_bytes",
        "federation_destinations_failing_count",
        "pending_reports_count",
    ] {
        assert!(first.get(unknown).is_none(), "{unknown} in {first}");
    }

    // The dashboard polls. A count is shared for a minute rather than redone on every poll, so
    // an account made a moment later is not in the next answer yet.
    let _bob = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": "bob", "password": "hunter2-bob", "auth": {"type": "m.login.dummy"}}))
        .send()
        .await
        .unwrap();
    let second = overview(client.clone(), base.clone(), admin_token.clone()).await;
    assert_eq!(second, first);

    let cluster: serde_json::Value = client
        .get(format!("{base}/api/v1/cluster"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cluster, json!({"mode": "single-node", "replica_count": 1}));

    // Not for just anybody: an ordinary account's token is not an administrator's.
    let response = client
        .get(format!("{base}/api/v1/statistics/overview"))
        .bearer_auth(alice["access_token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

    handle.shutdown().await;
}

/// With registration closed -- the default, and what this server is started with here -- the
/// admin API is how anybody after the first administrator gets an account. Until
/// `AuthStoreUserDirectory::create_user` existed this answered `503` on a real server.
#[tokio::test]
async fn an_administrator_can_add_a_user_who_can_then_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(reserve_ephemeral_port(), dir.path());
    config.auth.enable_registration = false;
    let handle = hs_cli::serve::spawn_serve(config, hs_cli::serve::ServeOptions::default())
        .await
        .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    let token = setup_token_of(handle.setup_link.as_deref().unwrap()).to_owned();
    let admin: serde_json::Value = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_token = admin["access_token"].as_str().unwrap().to_owned();

    // The front door really is closed.
    let register = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": "carol", "password": "hunter2-carol", "auth": {"type": "m.login.dummy"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(register.status(), reqwest::StatusCode::FORBIDDEN);

    let create = |body: serde_json::Value, token: String| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let response = client
                .post(format!("{base}/api/v1/users"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            (status, response.json::<serde_json::Value>().await.unwrap())
        }
    };

    // A refusal names the field, so the interface can put it beside the input.
    let (status, problem) = create(
        json!({"localpart": "carol", "password": "short"}),
        admin_token.clone(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["errors"][0]["pointer"], "/password", "{problem}");

    let (status, created) = create(
        json!({"localpart": "Carol", "password": "hunter2-carol", "display_name": "Carol D"}),
        admin_token.clone(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CREATED, "{created}");
    assert_eq!(created["user_id"], "@carol:example.org");
    assert_eq!(created["display_name"], "Carol D");
    assert_eq!(created["admin"], false);
    assert!(created.get("password").is_none(), "{created}");

    let (status, problem) = create(
        json!({"localpart": "carol", "password": "hunter2-carol"}),
        admin_token.clone(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{problem}");

    // She can sign in with the password she was given, and she is not an administrator.
    let login: serde_json::Value = client
        .post(format!("{base}/_matrix/client/v3/login"))
        .json(&json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": "carol"},
            "password": "hunter2-carol",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let carol_token = login["access_token"]
        .as_str()
        .expect("carol can sign in")
        .to_owned();
    let (status, _) = create(
        json!({"localpart": "mallory", "password": "hunter2-mallory"}),
        carol_token,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);

    // Who made the account is on the record.
    let audit: serde_json::Value = client
        .get(format!("{base}/api/v1/audit-log?action=users.create"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = audit["items"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{audit}");
    assert_eq!(entries[0]["actor"]["id"], "@ops:example.org");
    assert_eq!(entries[0]["target"]["id"], "@carol:example.org");
    assert!(
        !audit.to_string().contains("hunter2"),
        "a password reached the audit log"
    );

    handle.shutdown().await;
}

/// Who the user directory shows to whom, through the real server: Complement's
/// `TestRoomSpecificUsername*` scenario, which this server failed by showing a searcher somebody
/// they share nothing with. Alice is in a public room, so everybody can find her; Bob shares a
/// private room with Alice and nothing with Eve.
#[tokio::test]
async fn the_user_directory_shows_a_searcher_only_who_they_could_already_see() {
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        test_config(reserve_ephemeral_port(), dir.path()),
        hs_cli::serve::ServeOptions::default(),
    )
    .await
    .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();

    let register = |name: &'static str| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let body: serde_json::Value = client
                .post(format!("{base}/_matrix/client/v3/register"))
                .json(&json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            body["access_token"].as_str().unwrap().to_owned()
        }
    };
    let alice = register("dir-alice").await;
    let bob = register("dir-bob").await;
    let eve = register("dir-eve").await;

    let create_room = |token: String, body: serde_json::Value| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let created: serde_json::Value = client
                .post(format!("{base}/_matrix/client/v3/createRoom"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            created["room_id"].as_str().expect("a room id").to_owned()
        }
    };
    // Only `visibility`, exactly as Complement sends it: the spec makes that imply the
    // `public_chat` preset, and a room that is published but invite-only would not make Alice
    // findable by anybody.
    let public_room = create_room(alice.clone(), json!({"visibility": "public"})).await;
    let explicit = create_room(
        alice.clone(),
        json!({"visibility": "public", "preset": "private_chat"}),
    )
    .await;
    for (room, expected) in [(&public_room, "public"), (&explicit, "invite")] {
        let rules: serde_json::Value = client
            .get(format!(
                "{base}/_matrix/client/v3/rooms/{}/state/m.room.join_rules",
                room.replace('!', "%21").replace(':', "%3A")
            ))
            .bearer_auth(&alice)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            rules["join_rule"], expected,
            "{room}: an explicit preset still wins"
        );
    }
    create_room(
        alice.clone(),
        json!({"preset": "private_chat", "invite": ["@dir-bob:example.org"]}),
    )
    .await;
    // Bob accepts, so that he and Alice share a private room.
    let bob_sync: serde_json::Value = client
        .get(format!("{base}/_matrix/client/v3/sync?timeout=0"))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let invited_to = bob_sync["rooms"]["invite"]
        .as_object()
        .and_then(|rooms| rooms.keys().next().cloned())
        .expect("bob has an invite");
    let joined = client
        .post(format!(
            "{base}/_matrix/client/v3/join/{}",
            invited_to.replace('!', "%21").replace(':', "%3A")
        ))
        .bearer_auth(&bob)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(joined.status(), reqwest::StatusCode::OK);

    let search = |token: String, term: &'static str| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let found: serde_json::Value = client
                .post(format!("{base}/_matrix/client/v3/user_directory/search"))
                .bearer_auth(token)
                .json(&json!({"search_term": term}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let mut ids: Vec<String> = found["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["user_id"].as_str().unwrap().to_owned())
                .collect();
            ids.sort();
            ids
        }
    };

    // "dir-" matches all three accounts. Each searcher gets the ones they could already see.
    assert_eq!(
        search(eve.clone(), "dir-").await,
        vec!["@dir-alice:example.org"],
        "eve sees alice (public room) and not bob (shares nothing with her)"
    );
    assert_eq!(
        search(bob.clone(), "dir-").await,
        vec!["@dir-alice:example.org"],
        "bob sees alice and not eve"
    );
    assert_eq!(
        search(alice.clone(), "dir-").await,
        vec!["@dir-bob:example.org"],
        "alice sees bob (their private room) and not eve, who is in no room with her"
    );

    handle.shutdown().await;
}
