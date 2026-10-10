//! Decision 0040, through the real `hs` binary: a server whose `server.public_baseurl` is an
//! `https://` URL publishes `/.well-known/matrix/server` without any further configuration, so
//! other servers can find it (the demo spent 2026-10-10 undiscoverable: a client document, no
//! server document, port 8448 closed, every signed request answered `401 Failed to find any key`).
//!
//! What this proves, in order:
//!
//! 1. Started with `public_baseurl: https://example.org`, the server answers
//!    `/.well-known/matrix/server` with `{"m.server": "example.org:443"}` and
//!    `/.well-known/matrix/client` with the base URL, and its boot log says the server document
//!    was derived from `server.public_baseurl`.
//! 2. The derived value follows a hot change of `public_baseurl` (to one with an explicit port),
//!    the same way the client document does.
//! 3. An explicit `well_known_server` wins over the derivation; the empty string turns the
//!    document off; clearing it (null) derives again.
//! 4. An `http://` base URL publishes the client document and no server document.

use reqwest::StatusCode;
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

    /// The log line, among those already read, that contains every one of `needles`.
    fn said(&self, needles: &[&str]) -> Option<&str> {
        self.seen
            .iter()
            .map(String::as_str)
            .find(|line| needles.iter().all(|needle| line.contains(needle)))
    }
}

impl Drop for HsProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config(dir: &std::path::Path, port: u16, public_baseurl: &str) -> std::path::PathBuf {
    let path = dir.join("hs.yaml");
    std::fs::write(
        &path,
        format!(
            "server:\n  server_name: example.org\n  public_baseurl: {public_baseurl}\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health]\n\
             storage:\n  backend: embedded\n  data_dir: {:?}\n\
             media:\n  storage:\n    backend: local\n    path: {:?}\n",
            dir.join("db"),
            dir.join("media"),
        ),
    )
    .unwrap();
    path
}

async fn get(base: &str, path: &str) -> (StatusCode, Value) {
    let response = reqwest::get(format!("{base}{path}")).await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn well_known_server(base: &str) -> (StatusCode, Value) {
    get(base, "/.well-known/matrix/server").await
}

/// Saves `patch` to the `server` section as the first administrator and checks it was applied
/// at once.
async fn patch_server(base: &str, token: &str, patch: Value) {
    let response = reqwest::Client::new()
        .patch(format!("{base}/api/v1/config/server"))
        .bearer_auth(token)
        .json(&patch)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{patch}: {body}");
    assert_eq!(
        body["applied"]["reloaded_sections"],
        json!(["server"]),
        "{body}"
    );
    assert_eq!(body["applied"]["errors"], json!([]), "{body}");
}

/// Claims the server through its setup link and returns the first administrator's token.
async fn first_admin_token(hs: &mut HsProcess, base: &str) -> String {
    let setup_line = hs.wait_for(&["setup_link="]);
    let setup_token = setup_line
        .rsplit_once("#token=")
        .unwrap()
        .1
        .trim()
        .to_owned();
    let response = reqwest::Client::new()
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let session: Value = response.json().await.unwrap();
    session["access_token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_public_base_url_publishes_the_server_document_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let config_path = config(dir.path(), port, "https://example.org");

    let mut hs = HsProcess::serve(&config_path);
    let token = first_admin_token(&mut hs, &base).await;

    // 1. Derived at boot, and the log says so.
    let (status, body) = well_known_server(&base).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"m.server": "example.org:443"}));
    let (status, body) = get(&base, "/.well-known/matrix/client").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"m.homeserver": {"base_url": "https://example.org"}})
    );
    let boot = hs
        .said(&["publishing .well-known discovery documents"])
        .unwrap_or_else(|| {
            panic!(
                "no boot line about .well-known; the log said:\n{}",
                hs.seen.join("\n")
            )
        });
    assert!(
        boot.contains("example.org:443") && boot.contains("derived from server.public_baseurl"),
        "the boot line should say what is published and where it came from: {boot}"
    );

    // 2. The derived value follows a hot change of the base URL.
    patch_server(
        &base,
        &token,
        json!({"public_baseurl": "https://matrix.example.org:8448"}),
    )
    .await;
    let (status, body) = well_known_server(&base).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"m.server": "matrix.example.org:8448"}));
    let (_, body) = get(&base, "/.well-known/matrix/client").await;
    assert_eq!(
        body["m.homeserver"]["base_url"],
        "https://matrix.example.org:8448"
    );
    let in_force = hs.wait_for(&["the server settings are now in force"]);
    assert!(
        in_force.contains("matrix.example.org:8448")
            && in_force.contains("derived from server.public_baseurl"),
        "the reload line should name the derived document: {in_force}"
    );

    // 3. Explicit wins; the empty string turns it off; null derives again.
    patch_server(
        &base,
        &token,
        json!({"well_known_server": "federation.example.org:8448"}),
    )
    .await;
    let (status, body) = well_known_server(&base).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"m.server": "federation.example.org:8448"}));

    patch_server(&base, &token, json!({"well_known_server": ""})).await;
    let (status, body) = well_known_server(&base).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["errcode"], "M_NOT_FOUND");
    let (status, _) = get(&base, "/.well-known/matrix/client").await;
    assert_eq!(status, StatusCode::OK, "the client document is unaffected");

    patch_server(&base, &token, json!({"well_known_server": null})).await;
    let (status, body) = well_known_server(&base).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"m.server": "matrix.example.org:8448"}));

    // 4. An http:// base URL derives nothing: federation needs TLS.
    patch_server(
        &base,
        &token,
        json!({"public_baseurl": "http://localhost:8008"}),
    )
    .await;
    let (status, _) = well_known_server(&base).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = get(&base, "/.well-known/matrix/client").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["m.homeserver"]["base_url"], "http://localhost:8008");
    let in_force = hs.wait_for(&[
        "the server settings are now in force",
        "http://localhost:8008",
    ]);
    assert!(
        in_force.contains("federation needs TLS"),
        "the reload line should say why nothing is derived: {in_force}"
    );
}

#[tokio::test]
async fn an_http_public_base_url_publishes_no_server_document_at_boot() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let config_path = config(dir.path(), port, "http://localhost:8008");

    let mut hs = HsProcess::serve(&config_path);
    hs.wait_for(&["setup_link="]);
    let (status, _) = well_known_server(&base).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = get(&base, "/.well-known/matrix/client").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["m.homeserver"]["base_url"], "http://localhost:8008");
    let boot = hs
        .said(&["publishing .well-known discovery documents"])
        .unwrap_or_else(|| {
            panic!(
                "no boot line about .well-known; the log said:\n{}",
                hs.seen.join("\n")
            )
        });
    assert!(
        boot.contains("server=None") && boot.contains("federation needs TLS"),
        "the boot line should say why no server document is published: {boot}"
    );
}
