//! Settings that apply on the running server (decision 0016, amended 2026-10-01), through the
//! real `hs` binary: an operator changes each through the admin API, as the Configuration page
//! does, the answer says it was reloaded with nothing waiting for a restart, and the very next
//! request shows the new value in force.
//!
//! - The rate-limit buckets other than `message`, which until this change nothing enforced:
//!   `login` and `registration` per client address (the address a local proxy forwards in
//!   `X-Forwarded-For`), `joins_local` per user, `admin_redaction` for a server administrator.
//! - `auth.enable_registration` and `auth.user_directory_search_all_users`.
//! - `media.max_upload_size`, also as `m.upload.size`.
//! - `server.public_baseurl`, as the client `.well-known` document.
//!
//! Each would fail without the change: the buckets answered every request, and the other
//! settings were read once into what serves them. `/metrics` counts the refusals in
//! `hs_rate_limited_total{bucket}` and every applied setting in
//! `hs_config_settings_applied_total{setting,outcome}`; the log names each setting applied.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One run of the real `hs` binary (the harness of `config_reload.rs`).
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

    /// Reads the log until a line contains every one of `needles`.
    fn wait_for(&mut self, needles: &[&str]) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if needles.iter().all(|needle| line.contains(needle)) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "the log never said {needles:?}; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone)]
struct Caller {
    base: String,
    token: Option<String>,
    /// What a proxy in front of the server would put in `X-Forwarded-For`.
    from: Option<&'static str>,
}

impl Caller {
    fn with(&self, token: &str) -> Self {
        Self {
            token: Some(token.to_owned()),
            ..self.clone()
        }
    }

    fn from(&self, address: &'static str) -> Self {
        Self {
            from: Some(address),
            ..self.clone()
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = reqwest::Client::new().request(method, format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(from) = self.from {
            request = request.header("x-forwarded-for", from);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn expect(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        want: StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, want, "{path}: {body}");
        body
    }

    async fn register(&self, username: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({
                "username": username,
                "password": format!("hunter2-{username}"),
                "auth": {"type": "m.login.dummy"},
            })),
        )
        .await
    }

    async fn login(&self, username: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": username},
                "password": format!("hunter2-{username}"),
            })),
        )
        .await
    }

    async fn upload(&self, bytes: usize) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(format!("{}/_matrix/media/v3/upload", self.base))
            .bearer_auth(self.token.as_ref().unwrap())
            .header("content-type", "application/octet-stream")
            .body(vec![7u8; bytes])
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn create_room(&self) -> String {
        self.expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "public_chat"})),
            StatusCode::OK,
        )
        .await["room_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn join(&self, room: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            &format!("/_matrix/client/v3/rooms/{}/join", escape(room)),
            Some(json!({})),
        )
        .await
    }

    /// Saves `patch` to `section` and checks the answer: reloaded at once, nothing waiting for a
    /// restart.
    async fn apply(&self, section: &str, patch: Value) {
        let updated = self
            .expect(
                Method::PATCH,
                &format!("/api/v1/config/{section}"),
                Some(patch),
                StatusCode::OK,
            )
            .await;
        assert_eq!(
            updated["applied"]["reloaded_sections"],
            json!([section]),
            "{updated}"
        );
        assert_eq!(
            updated["applied"]["requires_restart"],
            json!([]),
            "{updated}"
        );
        assert_eq!(updated["applied"]["errors"], json!([]), "{updated}");
    }
}

fn escape(room: &str) -> String {
    room.replace('!', "%21").replace(':', "%3A")
}

async fn metrics(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// A bucket of one request, refilled about once a quarter of an hour: the second request in a
/// test is always refused.
fn one_request() -> Value {
    json!({"per_second": 0.001, "burst_count": 1})
}

#[tokio::test]
async fn hot_settings_apply_to_the_running_server_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             federation:\n  enabled: false\n\
             auth:\n  enable_registration: true\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();

    let mut hs = HsProcess::serve(&config_path);
    let setup_line = hs.wait_for(&["setup_link="]);
    let setup_token = setup_line
        .rsplit_once("#token=")
        .unwrap()
        .1
        .trim()
        .to_owned();
    let nobody = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
        from: None,
    };
    let session = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
            StatusCode::CREATED,
        )
        .await;
    let ops = nobody.with(session["access_token"].as_str().unwrap());

    // Every setting says how a change to it applies, from the one table.
    let schema = ops
        .expect(Method::GET, "/api/v1/config/schema", None, StatusCode::OK)
        .await;
    let applies = |pointer: &str| {
        schema["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["pointer"] == pointer)
            .unwrap_or_else(|| panic!("{pointer} is missing"))["applies"]
            .clone()
    };
    assert_eq!(applies("/rate_limits/login/per_second"), "hot");
    assert_eq!(applies("/auth/enable_registration"), "hot");
    assert_eq!(applies("/media/max_upload_size"), "hot");
    assert_eq!(applies("/media/storage/path"), "restart");
    assert_eq!(applies("/server/server_name"), "bootstrap");
    assert_eq!(
        schema["schema"]["$defs"]["RateLimitConfig"]["properties"]["login"]["x-applies"], "hot",
        "the schema carries it too"
    );

    // Accounts made from this host with nothing forwarded are this host's own tooling, which no
    // per-address limit applies to.
    let (status, alice) = nobody.register("alice").await;
    assert_eq!(status, StatusCode::OK, "{alice}");
    let alice = nobody.with(alice["access_token"].as_str().unwrap());
    let (status, bob) = nobody.register("bob").await;
    assert_eq!(status, StatusCode::OK, "{bob}");
    let bob = nobody.with(bob["access_token"].as_str().unwrap());

    // `rate_limits.login`, per client address: lowered to one attempt, the second from the same
    // address is refused, and another address is not.
    let from_a = nobody.from("203.0.113.7");
    let (status, body) = from_a.login("alice").await;
    assert_eq!(status, StatusCode::OK, "under the default limit: {body}");
    ops.apply("rate_limits", json!({"login": one_request()}))
        .await;
    hs.wait_for(&["configuration setting applied", "/rate_limits/login"]);
    // What the address had left is kept, clamped to the new burst of one.
    let (status, body) = from_a.login("alice").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = from_a.login("alice").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["errcode"], "M_LIMIT_EXCEEDED", "{body}");
    assert!(body["retry_after_ms"].as_u64().unwrap() > 0, "{body}");
    let (status, body) = nobody.from("203.0.113.8").login("alice").await;
    assert_eq!(status, StatusCode::OK, "another address: {body}");

    // `rate_limits.registration`: one account per address, counted when it is made.
    ops.apply("rate_limits", json!({"registration": one_request()}))
        .await;
    let from_b = nobody.from("198.51.100.20");
    let (status, body) = from_b.register("carol").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = from_b.register("dave").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // `rate_limits.joins_local`, per user.
    let lounge = alice.create_room().await;
    let library = alice.create_room().await;
    let (status, body) = bob.join(&lounge).await;
    assert_eq!(status, StatusCode::OK, "under the default limit: {body}");
    ops.apply("rate_limits", json!({"joins_local": one_request()}))
        .await;
    let (status, body) = bob.join(&library).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let gallery = alice.create_room().await;
    let (status, body) = bob.join(&gallery).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["errcode"], "M_LIMIT_EXCEEDED", "{body}");

    // `rate_limits.admin_redaction`: a server administrator's redactions, in place of the
    // message limit (which would allow ten).
    let ops_room = ops.create_room().await;
    let mut events = Vec::new();
    for txn in ["m1", "m2"] {
        let sent = ops
            .expect(
                Method::PUT,
                &format!(
                    "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                    escape(&ops_room)
                ),
                Some(json!({"msgtype": "m.text", "body": txn})),
                StatusCode::OK,
            )
            .await;
        events.push(sent["event_id"].as_str().unwrap().to_owned());
    }
    ops.apply("rate_limits", json!({"admin_redaction": one_request()}))
        .await;
    let redact = |event: &str, txn: &str| {
        let ops = ops.clone();
        let path = format!(
            "/_matrix/client/v3/rooms/{}/redact/{}/{txn}",
            escape(&ops_room),
            event.replace('$', "%24")
        );
        async move { ops.call(Method::PUT, &path, Some(json!({}))).await }
    };
    let (status, body) = redact(&events[0], "r1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = redact(&events[1], "r2").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // `auth.user_directory_search_all_users`: carol shares no room with bob, so he cannot find
    // her until the directory searches everybody.
    let search = |caller: Caller| async move {
        caller
            .expect(
                Method::POST,
                "/_matrix/client/v3/user_directory/search",
                Some(json!({"search_term": "carol"})),
                StatusCode::OK,
            )
            .await["results"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(search(bob.clone()).await, 0);
    ops.apply("auth", json!({"user_directory_search_all_users": true}))
        .await;
    assert_eq!(search(bob.clone()).await, 1);

    // `auth.enable_registration`: switched off, the next registration is refused.
    ops.apply("auth", json!({"enable_registration": false}))
        .await;
    hs.wait_for(&["configuration setting applied", "/auth/enable_registration"]);
    let (status, body) = nobody.register("erin").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // `media.max_upload_size`: twenty bytes go through until the limit is ten.
    let (status, body) = alice.upload(20).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    ops.apply("media", json!({"max_upload_size": 10})).await;
    let (status, body) = alice.upload(20).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["errcode"], "M_TOO_LARGE", "{body}");
    let config = alice
        .expect(
            Method::GET,
            "/_matrix/client/v1/media/config",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(config["m.upload.size"], 10, "{config}");

    // `server.public_baseurl`: the client .well-known document appears.
    nobody
        .expect(
            Method::GET,
            "/.well-known/matrix/client",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    ops.apply(
        "server",
        json!({"public_baseurl": "https://matrix.example.org"}),
    )
    .await;
    let document = nobody
        .expect(
            Method::GET,
            "/.well-known/matrix/client",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        document["m.homeserver"]["base_url"],
        "https://matrix.example.org"
    );

    // A setting that waits for a restart still says so.
    let cold = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/media",
            Some(json!({"allow_legacy_unauthenticated_media": false})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        cold["applied"]["requires_restart"],
        json!(["media"]),
        "{cold}"
    );

    let text = metrics(&nobody.base).await;
    for line in [
        r#"hs_rate_limited_total{bucket="login"} 1"#,
        r#"hs_rate_limited_total{bucket="registration"} 1"#,
        r#"hs_rate_limited_total{bucket="joins_local"} 1"#,
        r#"hs_rate_limited_total{bucket="admin_redaction"} 1"#,
        r#"hs_config_settings_applied_total{setting="/rate_limits/login",outcome="applied"} 1"#,
        r#"hs_config_settings_applied_total{setting="/auth/enable_registration",outcome="applied"} 1"#,
        r#"hs_config_settings_applied_total{setting="/media/max_upload_size",outcome="applied"} 1"#,
        r#"hs_config_settings_applied_total{setting="/server/public_baseurl",outcome="applied"} 1"#,
    ] {
        assert!(text.contains(line), "{line} not in:\n{text}");
    }
}

/// The settings decision 0016's amendment found read by nothing (status 16, 2026-10-08): the
/// ones that should be read now are, on the running server, and the ones removed from the schema
/// no longer stop a configuration that still carries them from loading.
///
/// - A bootstrap file with `server.report_stats`, `auth.session_secret` and
///   `appservices.enabled` starts, and the log names each as ignored.
/// - `server.admin_contact` is `/.well-known/matrix/support`.
/// - `auth.password.enabled` off: `GET /login` does not offer a password and `POST /login`
///   refuses one; back on, it works again.
/// - A write of a removed setting is refused, naming it.
#[tokio::test]
async fn settings_that_had_no_reader_are_read_or_gone() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n  report_stats: true\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             federation:\n  enabled: false\n\
             appservices:\n  enabled: true\n\
             auth:\n  enable_registration: true\n  session_secret: an-old-macaroon-key\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();

    let mut hs = HsProcess::serve(&config_path);
    let setup_line = hs.wait_for(&["setup_link="]);
    // Logged once the log is listening: at the latest the follower's ten-second re-read.
    for pointer in [
        "/server/report_stats",
        "/auth/session_secret",
        "/appservices/enabled",
    ] {
        hs.wait_for(&["ignoring a setting that no longer exists", pointer]);
    }
    let setup_token = setup_line
        .rsplit_once("#token=")
        .unwrap()
        .1
        .trim()
        .to_owned();
    let nobody = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
        from: None,
    };
    let session = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
            StatusCode::CREATED,
        )
        .await;
    let ops = nobody.with(session["access_token"].as_str().unwrap());
    let (status, body) = nobody.register("alice").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // `server.admin_contact`: the support document appears, and says who.
    nobody
        .expect(
            Method::GET,
            "/.well-known/matrix/support",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    ops.apply(
        "server",
        json!({"admin_contact": "mailto:abuse@example.org"}),
    )
    .await;
    let support = nobody
        .expect(
            Method::GET,
            "/.well-known/matrix/support",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        support,
        json!({"contacts": [{"role": "m.role.admin", "email_address": "abuse@example.org"}]})
    );

    // `auth.password.enabled`.
    let flows = |caller: Caller| async move {
        caller
            .expect(
                Method::GET,
                "/_matrix/client/v3/login",
                None,
                StatusCode::OK,
            )
            .await["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert!(
        flows(nobody.clone())
            .await
            .contains(&"m.login.password".to_owned())
    );
    ops.apply("auth", json!({"password": {"enabled": false}}))
        .await;
    hs.wait_for(&["configuration setting applied", "/auth/password/enabled"]);
    let offered = flows(nobody.clone()).await;
    assert!(
        !offered.contains(&"m.login.password".to_owned()),
        "{offered:?}"
    );
    let (status, body) = nobody.login("alice").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["errcode"], "M_FORBIDDEN", "{body}");
    ops.apply("auth", json!({"password": {"enabled": true}}))
        .await;
    let (status, body) = nobody.login("alice").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A removed setting cannot be written back, and the answer says why it went.
    let refused = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/auth",
            Some(json!({"enable_legacy_login": false})),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(
        refused["errors"][0]["pointer"], "/auth/enable_legacy_login",
        "{refused}"
    );
    // ...and none is served as a setting.
    let schema = ops
        .expect(Method::GET, "/api/v1/config/schema", None, StatusCode::OK)
        .await;
    let pointers: Vec<&str> = schema["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["pointer"].as_str().unwrap())
        .collect();
    for gone in [
        "/server/report_stats",
        "/auth/session_secret",
        "/appservices/enabled",
    ] {
        assert!(!pointers.contains(&gone), "{gone} is still a setting");
    }
    assert!(pointers.contains(&"/media/remote_media_retention"));
    let applies = |pointer: &str| {
        schema["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["pointer"] == pointer)
            .unwrap()["applies"]
            .clone()
    };
    assert_eq!(applies("/auth/password/enabled"), "hot");
    assert_eq!(applies("/server/admin_contact"), "hot");
}
