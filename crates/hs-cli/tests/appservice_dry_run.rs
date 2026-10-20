//! `POST /api/v1/appservices?dry_run=true` through the real binary: it creates nothing.
//!
//! Found on the demo on 2026-10-20: a dry run of Hookshot's registration registered it, and the
//! real create that followed was refused with `409 namespace conflict ... already exclusively
//! claimed by "hookshot"`, a conflict with itself. One real `hs serve` process; the first
//! administrator is made through `/api/v1/setup`. A dry run of a registration answers `200`
//! with the parsed appservice and no conflicts and leaves the registry, the audit log and the
//! idempotency cache untouched; the real create with the same `Idempotency-Key` is a `201`, not
//! a replay; a dry run of the same registration again lists the conflicts a create would be
//! refused for (the sender, the namespaces, both tokens, the id) in a `200`; a dry run of a
//! second registration that shares only the exclusive namespace names that one conflict, and
//! still registers nothing.

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

struct HsProcess {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HsProcess {
    /// Starts `hs serve`, handing it `listener` still bound (`hs_testkit::ports::pass_to`).
    fn serve(config_path: &std::path::Path, listener: hs_testkit::ports::ReservedPort) -> Self {
        use std::io::BufRead;
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hs"));
        command
            .args(["serve", "-c"])
            .arg(config_path)
            .env_remove("RUST_LOG")
            .env_remove("HS_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        hs_testkit::ports::pass_to(&mut command, [listener]);
        let mut child = command.spawn().expect("the hs binary should start");
        drop(command);
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

    /// Reads the log until a line contains `needle`; the timeout only bounds a broken server
    /// (a debug binary on a loaded machine can take a minute to boot).
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(line) = self.seen.iter().find(|l| l.contains(needle)) {
                return line.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
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
struct Admin {
    base: String,
    token: Option<String>,
}

impl Admin {
    /// Sends the request; answers the status, whether the answer was an idempotent replay, and
    /// the body as JSON (`Null` when it is not JSON).
    async fn call(
        &self,
        method: Method,
        path: &str,
        idempotency_key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, bool, Value) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header("accept", "application/json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let replayed = response.headers().contains_key("idempotency-replayed");
        let text = response.text().await.unwrap();
        (
            status,
            replayed,
            serde_json::from_str(&text).unwrap_or(Value::Null),
        )
    }

    async fn expect(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        expected: StatusCode,
    ) -> Value {
        let (status, _, body) = self.call(method, path, None, body).await;
        assert_eq!(status, expected, "{path}: {body}");
        body
    }

    /// The audit log's entries for `action`.
    async fn audited(&self, action: &str) -> Vec<Value> {
        let page = self
            .expect(
                Method::GET,
                "/api/v1/audit-log?limit=100",
                None,
                StatusCode::OK,
            )
            .await;
        page["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry["action"] == action)
            .collect()
    }
}

async fn wait_healthy(base: &str) {
    for _ in 0..2400 {
        if let Ok(response) = reqwest::get(format!("{base}/health/live")).await
            && response.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{base} never became healthy");
}

/// A registration like the one the demo's dry run was given: an exclusive users namespace and
/// an alias namespace, tokens, a url nothing listens on (a dry run never contacts the bridge).
fn hookshot_registration() -> Value {
    json!({
        "id": "hookshot",
        "url": "http://127.0.0.1:9",
        "as_token": "as-token-hookshot-0123456789",
        "hs_token": "hs-token-hookshot-0123456789",
        "sender_localpart": "hookshot",
        "rate_limited": false,
        "namespaces": {
            "users": [{"regex": "@_webhooks_.*:example\\.org", "exclusive": true}],
            "aliases": [{"regex": "#_webhooks_.*:example\\.org", "exclusive": true}],
            "rooms": []
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dry_run_registers_nothing_and_the_real_create_after_it_is_not_a_conflict_with_itself() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let media_dir = data_dir.join("media");
    let listener = hs_testkit::ports::reserve();
    let port = listener.port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
             media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
             rate_limits:\n  enabled: false\n",
        ),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let mut hs = HsProcess::serve(&config_path, listener);
    let line = hs.wait_for("setup_link=");
    let setup_token: String = line
        .split_once("#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    wait_healthy(&base).await;
    let nobody = Admin {
        base: base.clone(),
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
    let admin = Admin {
        base: base.clone(),
        token: Some(session["access_token"].as_str().unwrap().to_owned()),
    };

    // The dry run, with the Idempotency-Key the interface would send: 200, the appservice as a
    // create would answer it, no conflicts.
    let (status, _, report) = admin
        .call(
            Method::POST,
            "/api/v1/appservices?dry_run=true",
            Some("hookshot-create-1"),
            Some(json!({"registration": hookshot_registration()})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["appservice"]["id"], "hookshot", "{report}");
    assert_eq!(report["appservice"]["sender_localpart"], "hookshot");
    assert_eq!(report["appservice"]["health"], "unknown");
    assert_eq!(report["appservice"]["rate_limited"], false);
    assert_eq!(
        report["appservice"]["namespaces"]["users"][0]["exclusive"], true,
        "{report}"
    );
    assert!(
        report["appservice"].get("as_token").is_none()
            && report["appservice"].get("hs_token").is_none(),
        "tokens are not on the appservice: {report}"
    );
    assert_eq!(report["conflicts"], json!([]), "{report}");

    // Nothing was registered, nothing audited.
    admin
        .expect(
            Method::GET,
            "/api/v1/appservices/hookshot",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    // (The server registers its own `myelin-bridges` appservice at boot, so the list is
    // counted, not expected empty.)
    let page = admin
        .expect(Method::GET, "/api/v1/appservices", None, StatusCode::OK)
        .await;
    let before = page["items"].as_array().unwrap().len();
    assert!(
        !page["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "hookshot"),
        "{page}"
    );
    assert!(
        admin.audited("appservices.create").await.is_empty(),
        "a dry run is not a mutation"
    );

    // The real create, same key, same body: 201 and not a replay of the dry run's 200.
    let (status, replayed, created) = admin
        .call(
            Method::POST,
            "/api/v1/appservices",
            Some("hookshot-create-1"),
            Some(json!({"registration": hookshot_registration()})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(!replayed, "the dry run stored an idempotency entry");
    assert_eq!(created["id"], "hookshot");
    assert_eq!(admin.audited("appservices.create").await.len(), 1);
    admin
        .expect(
            Method::GET,
            "/api/v1/appservices/hookshot",
            None,
            StatusCode::OK,
        )
        .await;

    // The same registration dry-run again: what the demo's second POST was refused for, now
    // listed in a 200 so an operator can see it before trying.
    let (status, _, report) = admin
        .call(
            Method::POST,
            "/api/v1/appservices?dry_run=true",
            None,
            Some(json!({"registration": hookshot_registration()})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let kinds: Vec<&str> = report["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["sender", "users", "aliases", "as_token", "hs_token", "id"],
        "the order a create checks them: {report}"
    );
    for conflict in report["conflicts"].as_array().unwrap() {
        assert_eq!(conflict["existing_id"], "hookshot", "{conflict}");
        assert!(
            conflict["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{conflict}"
        );
        if conflict["kind"].as_str().unwrap().ends_with("_token") {
            assert_eq!(conflict["value"], "<redacted>", "{conflict}");
        }
    }
    assert!(
        !report.to_string().contains("as-token-hookshot"),
        "a dry run never repeats a token: {report}"
    );

    // A second bridge that shares only the exclusive users namespace: that one conflict, and
    // the create it predicts is a 409. Nothing registered either way.
    let mut second = hookshot_registration();
    second["id"] = json!("hookshot-2");
    second["as_token"] = json!("as-token-second-0123456789");
    second["hs_token"] = json!("hs-token-second-0123456789");
    second["sender_localpart"] = json!("hookshot2");
    second["namespaces"]["aliases"] = json!([]);
    let (status, _, report) = admin
        .call(
            Method::POST,
            "/api/v1/appservices?dry_run=true",
            None,
            Some(json!({"registration": second.clone()})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["appservice"]["id"], "hookshot-2");
    assert_eq!(
        report["conflicts"],
        json!([{
            "kind": "users",
            "value": "@_webhooks_.*:example\\.org",
            "existing_id": "hookshot",
            // The pattern is Debug-quoted in the message, as the 409's is: its backslash doubled.
            "message": "users namespace \"@_webhooks_.*:example\\\\.org\" is already exclusively claimed by \"hookshot\"",
        }]),
        "{report}"
    );
    admin
        .expect(
            Method::GET,
            "/api/v1/appservices/hookshot-2",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    let (status, _, problem) = admin
        .call(
            Method::POST,
            "/api/v1/appservices",
            None,
            Some(json!({"registration": second})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // A body a create would refuse is refused the same way by a dry run.
    let (status, _, problem) = admin
        .call(
            Method::POST,
            "/api/v1/appservices?dry_run=true",
            None,
            Some(json!({"registration_yaml": "id: [not a registration"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["errors"][0]["pointer"], "/registration_yaml",
        "{problem}"
    );

    let page = admin
        .expect(Method::GET, "/api/v1/appservices", None, StatusCode::OK)
        .await;
    assert_eq!(
        page["items"].as_array().unwrap().len(),
        before + 1,
        "only the real create registered anything: {page}"
    );
    assert_eq!(admin.audited("appservices.create").await.len(), 1);
    drop(hs);
}
