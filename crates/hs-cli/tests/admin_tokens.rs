//! Admin tokens narrower than an administrator's, through the real binary (RFC 0004 section
//! 8.1): the first administrator mints a `bridges:read` token on `/api/v1/admin-tokens`, and the
//! running `hs` serves that token the bridge listings, refuses it `users.list` with the RFC's
//! `403 insufficient-scope` naming `admin:read`, reports its scopes on `/me`, counts the refusal
//! in `hs_admin_scope_refusals_total`, records the mint in the audit log with the scopes, and
//! refuses the token everywhere once it is revoked. `hs admin-token create`, `list` and `revoke`
//! do the same from a shell, against the same server.
//!
//! This is the case `crates/hs-cli/tests/admin_scopes.rs` said it could not show.

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

fn hs_admin_token(server: &str, token: &str, args: &[&str]) -> (i32, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hs"))
        .args(["admin-token", "--server", server, "--token", token])
        .args(args)
        .env_remove("HS_ADMIN_TOKEN")
        .output()
        .expect("the hs binary runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bridges_read_token_is_served_the_bridge_listings_and_refused_the_rest() {
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
    let admin_secret = session["access_token"].as_str().unwrap().to_owned();
    let admin = Caller {
        base: base.clone(),
        token: Some(admin_secret.clone()),
    };

    // Mint a bridges:read token through the API, as the Settings page does.
    let created = admin
        .expect(
            Method::POST,
            "/api/v1/admin-tokens",
            Some(json!({"name": "bridge team", "scopes": ["bridges:read"]})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(created["scopes"], json!(["bridges:read"]), "{created}");
    assert_eq!(created["created_by"], "@ops:example.org");
    let secret = created["token"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("hsa_"), "{secret}");
    let id = created["id"].as_str().unwrap().to_owned();
    let narrow = Caller {
        base: base.clone(),
        token: Some(secret.clone()),
    };

    // /me says what it holds.
    let me = narrow
        .expect(Method::GET, "/api/v1/me", None, StatusCode::OK)
        .await;
    assert_eq!(me["scopes"], json!(["bridges:read"]), "{me}");
    assert_eq!(me["kind"], "service_account");
    assert_eq!(me["display_name"], "bridge team");

    // Inside its scopes: the bridge listings, with real data.
    let types = narrow
        .expect(Method::GET, "/api/v1/bridge-types", None, StatusCode::OK)
        .await;
    assert!(
        types["items"].as_array().is_some_and(|t| !t.is_empty()),
        "the catalogue lists bridge types: {types}"
    );
    narrow
        .expect(Method::GET, "/api/v1/appservices", None, StatusCode::OK)
        .await;

    // Outside them: the RFC's 403, naming the scope.
    let problem = narrow
        .expect(Method::GET, "/api/v1/users", None, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(
        problem["type"], "urn:hs:problem:insufficient-scope",
        "{problem}"
    );
    assert_eq!(problem["required_scope"], "admin:read", "{problem}");
    assert_eq!(problem["status"], 403);
    let problem = narrow
        .expect(
            Method::POST,
            "/api/v1/admin-tokens",
            Some(json!({"name": "escalate"})),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(problem["required_scope"], "admin:write", "{problem}");

    // The refusals are counted by the scope they lacked.
    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("hs_admin_scope_refusals_total{required_scope=\"admin:read\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("hs_admin_scope_refusals_total{required_scope=\"admin:write\"} 1"),
        "{metrics}"
    );

    // The administrator lists it with its scopes and never its secret, and the audit log holds
    // the mint with the scopes.
    let page = admin
        .expect(Method::GET, "/api/v1/admin-tokens", None, StatusCode::OK)
        .await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{page}");
    assert_eq!(items[0]["id"], id);
    assert_eq!(items[0]["scopes"], json!(["bridges:read"]));
    assert!(items[0]["token"].is_null());
    assert!(!page.to_string().contains(&secret));
    let audit = admin
        .expect(
            Method::GET,
            "/api/v1/audit-log?action=admin_tokens.create",
            None,
            StatusCode::OK,
        )
        .await;
    let entry = &audit["items"][0];
    assert_eq!(entry["target"]["type"], "admin_token", "{audit}");
    assert_eq!(entry["target"]["id"], id);
    assert_eq!(entry["actor"]["id"], "@ops:example.org");
    let scopes_change = entry["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["pointer"] == "/scopes")
        .expect("the audit entry records the scopes");
    assert_eq!(scopes_change["to"], json!(["bridges:read"]));
    assert!(!audit.to_string().contains(&secret));

    // Revoked: the next request with it is 401.
    admin
        .expect(
            Method::DELETE,
            &format!("/api/v1/admin-tokens/{id}"),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    narrow
        .expect(Method::GET, "/api/v1/me", None, StatusCode::UNAUTHORIZED)
        .await;
    narrow
        .expect(
            Method::GET,
            "/api/v1/bridge-types",
            None,
            StatusCode::UNAUTHORIZED,
        )
        .await;

    // The same from a shell: `hs admin-token create` prints the token alone on stdout.
    let (code, stdout, stderr) = hs_admin_token(
        &base,
        &admin_secret,
        &[
            "create",
            "--name",
            "moderators",
            "--scope",
            "moderation:write",
            "--expires-in",
            "1d",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let cli_secret = stdout.trim().to_owned();
    assert!(cli_secret.starts_with("hsa_"), "{stdout}");
    assert!(stderr.contains("moderation:write"), "{stderr}");
    let moderator = Caller {
        base: base.clone(),
        token: Some(cli_secret),
    };
    let me = moderator
        .expect(Method::GET, "/api/v1/me", None, StatusCode::OK)
        .await;
    assert_eq!(me["scopes"], json!(["moderation:write"]), "{me}");
    assert!(me["expires_at"].is_string(), "{me}");
    // moderation:write satisfies moderation:read, so the reports inbox is served; the audit
    // log, admin:read, is not.
    moderator
        .expect(Method::GET, "/api/v1/reports", None, StatusCode::OK)
        .await;
    let problem = moderator
        .expect(
            Method::GET,
            "/api/v1/audit-log",
            None,
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(problem["required_scope"], "admin:read", "{problem}");

    let (code, stdout, stderr) = hs_admin_token(&base, &admin_secret, &["list"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("moderators\tmoderation:write"), "{stdout}");
    assert!(
        !stdout.contains("bridge team"),
        "the revoked token is gone: {stdout}"
    );
    let cli_id = stdout.split('\t').next().unwrap().to_owned();

    // A narrow token cannot use the CLI to mint: the refusal names the scope.
    let (code, _, stderr) = hs_admin_token(
        &base,
        moderator.token.as_deref().unwrap(),
        &["create", "--name", "escalate"],
    );
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("required scope: admin:write"), "{stderr}");
    // And an unknown scope is refused before the server is asked.
    let (code, _, stderr) = hs_admin_token(
        &base,
        &admin_secret,
        &["create", "--name", "x", "--scope", "root"],
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("unknown scope"), "{stderr}");

    let (code, _, stderr) = hs_admin_token(&base, &admin_secret, &["revoke", &cli_id]);
    assert_eq!(code, 0, "{stderr}");
    moderator
        .expect(Method::GET, "/api/v1/me", None, StatusCode::UNAUTHORIZED)
        .await;
}
