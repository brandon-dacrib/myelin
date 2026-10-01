//! A configuration change taking effect on a running server, through the real `hs` binary and
//! nothing else: an operator lowers the server-wide send limit with `config.update` (as the
//! Configuration page does) and the very next messages from a Matrix client are refused
//! `429 M_LIMIT_EXCEEDED` -- no restart. The answer to the update says it reloaded
//! `rate_limits`; a change to a section only read at startup says it needs a restart instead,
//! and so do `config.validate` and `config.reload`. The log level and the federation domain
//! allowlist, hot settings, apply at once too: a debug line appears that the process started
//! without, and a join to a server outside the list is refused. Switching the limit off lets
//! the client send again at once. The log says which section was reloaded and `/metrics`
//! counts it in `hs_config_reloads_total{section,outcome}`.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One run of the real `hs` binary (the harness of `admin_user_identity.rs`).
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

    /// Reads the log until a line contains `needle`.
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
}

impl Caller {
    fn with(&self, token: &str) -> Self {
        Self {
            base: self.base.clone(),
            token: Some(token.to_owned()),
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = reqwest::Client::new().request(method, format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
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

    /// `PUT /send` of an `m.room.message`: the status and body.
    async fn send(&self, room: &str, txn: &str) -> (StatusCode, Value) {
        self.call(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                room.replace('!', "%21").replace(':', "%3A")
            ),
            Some(json!({"msgtype": "m.text", "body": txn})),
        )
        .await
    }
}

async fn metrics(base: &str) -> String {
    reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn lowering_the_send_limit_through_the_admin_api_limits_the_next_message_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    // The default server-wide limit: ten back to back, then one every five seconds.
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n\
             auth:\n  enable_registration: true\n",
            dir.path().join("db"),
            dir.path().join("media"),
        ),
    )
    .unwrap();

    let mut hs = HsProcess::serve(&config_path);
    let setup_line = hs.wait_for("setup_link=");
    let setup_token = setup_line
        .rsplit_once("#token=")
        .unwrap()
        .1
        .trim()
        .to_owned();
    let nobody = Caller {
        base: format!("http://127.0.0.1:{port}"),
        token: None,
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

    let registered = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "alice", "password": "hunter2-alice", "auth": {"type": "m.login.dummy"}})),
            StatusCode::OK,
        )
        .await;
    let alice = nobody.with(registered["access_token"].as_str().unwrap());
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Under the default limit, a few messages go straight through.
    for txn in ["a1", "a2", "a3"] {
        let (status, body) = alice.send(&room, txn).await;
        assert_eq!(status, StatusCode::OK, "{txn}: {body}");
    }

    // Asked beforehand, the server says this change needs no restart...
    // One field of the bucket, as the interface sends it when one field is edited: the other
    // keeps `message`'s own default (0.2 per second).
    let lower = json!({"message": {"burst_count": 1}});
    let report = ops
        .expect(
            Method::POST,
            "/api/v1/config/validate",
            Some(json!({"rate_limits": lower})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(report["valid"], true, "{report}");
    assert_eq!(report["requires_restart"], json!([]), "{report}");

    // ...and saying it, the answer lists the section as reloaded.
    let updated = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/rate_limits",
            Some(lower),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        updated["applied"]["reloaded_sections"],
        json!(["rate_limits"]),
        "{updated}"
    );
    assert_eq!(updated["applied"]["requires_restart"], json!([]));
    assert_eq!(updated["applied"]["errors"], json!([]));
    assert!(
        updated["last_reloaded_at"].is_string(),
        "the section says when it was last reloaded: {updated}"
    );
    hs.wait_for("the server-wide rate limits are now in force");
    let reloaded = hs.wait_for("configuration section reloaded");
    assert!(reloaded.contains("rate_limits"), "{reloaded}");

    // The log level, too: turned up to debug, the refusal below is logged at a level the
    // process started without.
    let debug = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/telemetry",
            Some(json!({"logging": {"level": "debug"}})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        debug["applied"]["reloaded_sections"],
        json!(["telemetry"]),
        "{debug}"
    );
    assert_eq!(debug["applied"]["requires_restart"], json!([]));
    hs.wait_for("the log level is now in force");

    // The same client, the same process: what was left of the burst is clamped to the new one,
    // and the message after that is refused.
    let (status, body) = alice.send(&room, "b1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = alice.send(&room, "b2").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["errcode"], "M_LIMIT_EXCEEDED", "{body}");
    assert!(body["retry_after_ms"].as_u64().unwrap() > 1_000, "{body}");
    hs.wait_for("a sender is over the server-wide rate limit");
    // And back down, so the rest of this test's log is readable.
    ops.expect(
        Method::PATCH,
        "/api/v1/config/telemetry",
        Some(json!({"logging": {"level": "info"}})),
        StatusCode::OK,
    )
    .await;

    let text = metrics(&nobody.base).await;
    assert!(
        text.contains(r#"hs_config_reloads_total{section="rate_limits",outcome="applied"} 1"#),
        "{text}"
    );
    assert!(
        text.contains("hs_room_server_rate_limited_writes_total 1"),
        "{text}"
    );

    // A section read only at startup: stored, and reported as waiting for a restart -- by the
    // update, by validate and by reload -- with nothing claimed as reloaded.
    let cold = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/federation",
            Some(json!({"client_timeout": "45s"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(cold["applied"]["reloaded_sections"], json!([]), "{cold}");
    assert_eq!(cold["applied"]["requires_restart"], json!(["federation"]));
    let report = ops
        .expect(
            Method::POST,
            "/api/v1/config/validate",
            Some(json!({"server": {"public_baseurl": "https://matrix.example.org"}})),
            StatusCode::OK,
        )
        .await;
    // `server.public_baseurl` is hot (read per request), so only the federation timeout
    // already saved is pending.
    assert_eq!(
        report["requires_restart"],
        json!(["federation"]),
        "{report}"
    );
    let reload = ops
        .expect(Method::POST, "/api/v1/config/reload", None, StatusCode::OK)
        .await;
    assert_eq!(reload["reloaded_sections"], json!([]), "{reload}");
    assert_eq!(reload["requires_restart"], json!(["federation"]));

    // The federation allowlist is hot although the rest of the section is not: applied now,
    // with the timeout still waiting for a restart. The very next outbound request is refused
    // before anything is resolved or sent.
    let allow = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/federation",
            Some(json!({"domain_allowlist": ["friend.example"]})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        allow["applied"]["reloaded_sections"],
        json!(["federation"]),
        "{allow}"
    );
    assert_eq!(allow["applied"]["requires_restart"], json!(["federation"]));
    hs.wait_for("the federation domain and IP-range lists are now in force");
    let (status, body) = alice
        .call(
            Method::POST,
            "/_matrix/client/v3/join/%21elsewhere%3Adenied.example?server_name=denied.example",
            Some(json!({})),
        )
        .await;
    assert!(!status.is_success(), "{status}: {body}");
    hs.wait_for("not in the domain allowlist");

    // Switched off, the client sends again at once.
    let off = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/rate_limits",
            Some(json!({"enabled": false})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(off["applied"]["reloaded_sections"], json!(["rate_limits"]));
    for txn in ["c1", "c2", "c3"] {
        let (status, body) = alice.send(&room, txn).await;
        assert_eq!(status, StatusCode::OK, "{txn}: {body}");
    }
    let text = metrics(&nobody.base).await;
    assert!(
        text.contains(r#"hs_config_reloads_total{section="rate_limits",outcome="applied"} 2"#),
        "{text}"
    );

    // Reverting a saved hot setting follows the same apply path as PATCH. The response
    // and the next client writes must agree: the single-message bucket is enforced again.
    let reverted = ops
        .expect(
            Method::POST,
            &format!(
                "/api/v1/config/rate_limits/history/{}/revert",
                off["revision"]
            ),
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        reverted["applied"]["reloaded_sections"],
        json!(["rate_limits"])
    );
    assert_eq!(
        reverted["applied"]["requires_restart"],
        json!(["federation"])
    );
    assert_eq!(reverted["history"][0]["reverts"], off["revision"]);
    let _ = alice.send(&room, "after-revert-1").await;
    let (status, body) = alice.send(&room, "after-revert-2").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // A cold setting reverted to its boot value clears the pending restart without
    // claiming that the federation section was hot-applied.
    let cold_reverted = ops
        .expect(
            Method::POST,
            &format!(
                "/api/v1/config/federation/history/{}/revert",
                cold["revision"]
            ),
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(cold_reverted["applied"]["reloaded_sections"], json!([]));
    assert_eq!(cold_reverted["applied"]["requires_restart"], json!([]));
    assert_eq!(
        cold_reverted["values"]["domain_allowlist"],
        json!(["friend.example"])
    );
}
