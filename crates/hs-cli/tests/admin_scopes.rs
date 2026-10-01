//! The admin API's scopes, through the real binary: what the running `hs` documents for the
//! bridge listings is what it serves them under, and who it serves them to.
//!
//! One real `hs serve` process. The bridge listings (`appservices.list`, `bridge_types.list`,
//! `bridge_offerings.list`, `bridge_deployments.target`) are documented `bridges:read`, and
//! `users.list` `admin:read`, in the document the binary serves at `/api/v1/openapi.json`. The
//! first administrator's token lists all of them; a registered user who is not an administrator
//! is not an admin API caller at all (`401`, the answer an unknown token gets).
//!
//! What this cannot show yet: a token holding *only* `bridges:read`. The binary's one admin
//! credential is a Matrix access token of a user with the administrator flag, which
//! `hs_auth::admin_verifier::AdminTokenVerifier` grants `admin:read` and `admin:write`; nothing
//! in it mints a narrower token (RFC 0004 section 8.1's OAuth issuer and CLI service accounts are
//! not built). `GET /api/v1/me` below pins that, so the day a narrower grant exists this test is
//! where it shows up. That a `bridges:read`-only token is served the bridge listings and refused
//! `users.list` is proved against the same router this binary mounts, operation by operation, in
//! `crates/hs-admin/tests/scope_contract.rs`.

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

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

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header("accept", "application/json");
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
        expected: StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert_eq!(status, expected, "{path}: {body}");
        body
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

/// The scope the served OpenAPI document gives `operation_id`.
fn documented_scope(document: &Value, operation_id: &str) -> String {
    let paths = document["paths"]
        .as_object()
        .expect("the document has paths");
    for operations in paths.values() {
        for operation in operations.as_object().into_iter().flat_map(|o| o.values()) {
            if operation["operationId"] == operation_id {
                return operation["security"][0]["OAuth2"][0]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
            }
        }
    }
    panic!("{operation_id} is not in the served document");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_listings_are_served_under_the_scope_the_served_document_gives_them() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let media_dir = data_dir.join("media");
    let port = reserve_port();
    let config_path = dir.path().join("homeserver.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
             media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
             auth:\n  enable_registration: true\n\
             rate_limits:\n  enabled: false\n",
        ),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let mut hs = HsProcess::serve(&config_path);
    let line = hs.wait_for("setup_link=");
    let setup_token: String = line
        .split_once("#token=")
        .unwrap()
        .1
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    wait_healthy(&base).await;
    let nobody = Caller {
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
    let admin = Caller {
        base: base.clone(),
        token: Some(session["access_token"].as_str().unwrap().to_owned()),
    };
    let registered = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "alice", "password": "hunter2-alice", "auth": {"type": "m.login.dummy"}})),
            StatusCode::OK,
        )
        .await;
    let alice = Caller {
        base: base.clone(),
        token: Some(registered["access_token"].as_str().unwrap().to_owned()),
    };

    // The contract, as this binary serves it.
    let document = nobody
        .expect(Method::GET, "/api/v1/openapi.json", None, StatusCode::OK)
        .await;
    let listings = [
        ("appservices.list", "/api/v1/appservices"),
        ("bridge_types.list", "/api/v1/bridge-types"),
        ("bridge_offerings.list", "/api/v1/bridge-offerings"),
        (
            "bridge_deployments.target",
            "/api/v1/bridge-deployment-target",
        ),
    ];
    for (operation_id, _) in listings {
        assert_eq!(
            documented_scope(&document, operation_id),
            "bridges:read",
            "{operation_id}"
        );
    }
    assert_eq!(documented_scope(&document, "users.list"), "admin:read");

    // The binary's one admin credential holds admin:read and admin:write, and nothing narrower
    // can be minted yet (see the module comment).
    let me = admin
        .expect(Method::GET, "/api/v1/me", None, StatusCode::OK)
        .await;
    assert_eq!(me["scopes"], json!(["admin:read", "admin:write"]), "{me}");

    // admin:read satisfies bridges:read: the administrator is served every listing.
    for (operation_id, path) in listings {
        let body = admin.expect(Method::GET, path, None, StatusCode::OK).await;
        assert!(body.is_object(), "{operation_id}: {body}");
    }
    let types = admin
        .expect(Method::GET, "/api/v1/bridge-types", None, StatusCode::OK)
        .await;
    assert!(
        types["items"].as_array().is_some_and(|t| !t.is_empty()),
        "the catalogue lists bridge types: {types}"
    );
    admin
        .expect(Method::GET, "/api/v1/users", None, StatusCode::OK)
        .await;

    // A user who is not an administrator holds no admin scope at all: refused everywhere, with
    // the answer an unknown token gets.
    for (_, path) in listings {
        alice
            .expect(Method::GET, path, None, StatusCode::UNAUTHORIZED)
            .await;
    }
    alice
        .expect(Method::GET, "/api/v1/users", None, StatusCode::UNAUTHORIZED)
        .await;
}
