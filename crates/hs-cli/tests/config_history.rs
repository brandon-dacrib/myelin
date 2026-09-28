//! A configuration section's per-setting history and its revert, through the real `hs serve`
//! binary (the store-backed `ConfigSource` over the embedded store):
//!
//! - a change is listed setting by setting, with what the database held before and what the
//!   change wrote, who made it and when, newest first and paged;
//! - reverting the latest change puts the old value back as a new revision that says what it
//!   reverted, and is on the audit record;
//! - reverting an older change a later one overwrote is refused with the later revision named,
//!   and goes through when forced;
//! - a secret rotated and reverted is never readable through the history, the revert's answer or
//!   the audit log -- and the store holds the old secret again afterwards.

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One run of the real `hs` binary (the harness of `migration.rs`).
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

    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
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

    fn stop(mut self) {
        let pid = self.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let _ = self.child.wait();
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Admin {
    base: String,
    token: String,
}

impl Admin {
    /// The status, the headers and the body as text.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, reqwest::header::HeaderMap, String) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(etag) = if_match {
            request = request.header("if-match", etag);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        (status, headers, response.text().await.unwrap())
    }

    async fn expect(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        want: StatusCode,
    ) -> Value {
        let (status, _, text) = self.call(method, path, body, None).await;
        assert_eq!(status, want, "{path}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_setting_is_listed_reverted_and_a_secret_never_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("hs.yaml");
    let data_dir = dir.path().join("db");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n",
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
    let base = format!("http://127.0.0.1:{port}");
    let response = reqwest::Client::new()
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let session: Value = response.json().await.unwrap();
    let ops = Admin {
        base,
        token: session["access_token"].as_str().unwrap().to_owned(),
    };

    // ---- a setting changed twice, listed setting by setting ----
    let (status, headers, _) = ops
        .call(Method::GET, "/api/v1/config/rate_limits", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers["etag"].to_str().unwrap().to_owned();
    let (status, _, text) = ops
        .call(
            Method::PATCH,
            "/api/v1/config/rate_limits",
            Some(json!({"login": {"per_second": 5.0, "burst_count": 3}})),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let first: Value = serde_json::from_str(&text).unwrap();
    let first_revision = first["revision"].as_u64().unwrap();
    ops.expect(
        Method::PATCH,
        "/api/v1/config/rate_limits",
        Some(json!({"login": {"per_second": 10.0}})),
        StatusCode::OK,
    )
    .await;

    let history = ops
        .expect(
            Method::GET,
            "/api/v1/config/rate_limits/history?limit=1",
            None,
            StatusCode::OK,
        )
        .await;
    let latest = &history["items"][0];
    let latest_revision = latest["revision"].as_u64().unwrap();
    assert_eq!(latest_revision, first_revision + 1);
    assert_eq!(latest["actor"], "@ops:example.org");
    assert_eq!(latest["revertible"], true);
    assert_eq!(
        latest["settings"],
        json!([{
            "pointer": "/rate_limits/login/per_second",
            "path": "rate_limits.login.per_second",
            "secret": false,
            "from": {"set": true, "value": 5.0},
            "to": {"set": true, "value": 10.0},
        }])
    );
    let older = ops
        .expect(
            Method::GET,
            &format!(
                "/api/v1/config/rate_limits/history?limit=1&cursor={}",
                history["next_cursor"].as_str().unwrap()
            ),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(older["items"][0]["revision"], first_revision);
    assert_eq!(
        older["items"][0]["settings"][0]["from"],
        json!({"set": false}),
        "the database held nothing: the value came from the file or the default"
    );

    // ---- the older change is behind the newer one ----
    let (status, _, text) = ops
        .call(
            Method::POST,
            &format!("/api/v1/config/rate_limits/history/{first_revision}/revert"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(
        text.contains(&format!("revision {latest_revision} by @ops:example.org")),
        "{text}"
    );

    // ---- the latest is reverted ----
    let (status, headers, text) = ops
        .call(
            Method::POST,
            &format!("/api/v1/config/rate_limits/history/{latest_revision}/revert"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let reverted: Value = serde_json::from_str(&text).unwrap();
    let logged = hs.wait_for("reverted a configuration change");
    assert!(logged.contains("rate_limits"), "{logged}");
    assert_eq!(reverted["values"]["login"]["per_second"], 5.0);
    assert_eq!(
        headers["etag"].to_str().unwrap(),
        format!("\"{}\"", latest_revision + 1)
    );
    let section = ops
        .expect(
            Method::GET,
            "/api/v1/config/rate_limits",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(section["values"]["login"]["per_second"], 5.0);
    assert_eq!(section["history"][0]["reverts"], latest_revision);
    let audit = ops
        .expect(
            Method::GET,
            "/api/v1/audit-log?action=config.history.revert",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1, "{audit}");
    assert_eq!(audit["items"][0]["target"]["id"], "rate_limits");

    // ---- forced over a later change ----
    ops.expect(
        Method::POST,
        &format!("/api/v1/config/rate_limits/history/{first_revision}/revert"),
        Some(json!({"force": true})),
        StatusCode::OK,
    )
    .await;
    let section = ops
        .expect(
            Method::GET,
            "/api/v1/config/rate_limits",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        section["origins"]["/rate_limits/login/per_second"], "default",
        "before the first change nothing was stored, so the setting is back at its default"
    );

    // ---- a secret, rotated and reverted ----
    let source = |password: &str| {
        json!({"synapse": {"database": {
            "host": "synapse-db.internal", "database": "synapse", "user": "synapse",
            "password": password,
        }}})
    };
    ops.expect(
        Method::PATCH,
        "/api/v1/config/migration",
        Some(source("first-pass-hs")),
        StatusCode::OK,
    )
    .await;
    let rotated = ops
        .expect(
            Method::PATCH,
            "/api/v1/config/migration",
            Some(json!({"synapse": {"database": {"password": "second-pass-hs"}}})),
            StatusCode::OK,
        )
        .await;
    let rotated_revision = rotated["revision"].as_u64().unwrap();
    let (_, _, history) = ops
        .call(Method::GET, "/api/v1/config/migration/history", None, None)
        .await;
    assert!(!history.contains("-pass-hs"), "a secret leaked: {history}");
    let history: Value = serde_json::from_str(&history).unwrap();
    assert_eq!(history["items"][0]["settings"][0]["secret"], true);
    let (status, _, text) = ops
        .call(
            Method::POST,
            &format!("/api/v1/config/migration/history/{rotated_revision}/revert"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(!text.contains("-pass-hs"), "a secret leaked: {text}");
    let (_, _, audit) = ops.call(Method::GET, "/api/v1/audit-log", None, None).await;
    assert!(!audit.contains("-pass-hs"), "a secret leaked: {audit}");

    // The store holds the old secret again: read it directly once the server has let go.
    hs.stop();
    let storage = hs_cli::storage::open_storage(&hs_config::StorageConfig::Embedded(
        hs_config::storage::EmbeddedStorageConfig {
            data_dir: data_dir.clone(),
        },
    ))
    .unwrap();
    let store = hs_cli::bootstrap::OpenedConfigStore::open(&storage).unwrap();
    assert_eq!(
        store
            .load()
            .unwrap()
            .document
            .pointer("/migration/synapse/database/password"),
        Some(&json!("first-pass-hs")),
        "the revert restored the old secret without it crossing the wire"
    );
}
