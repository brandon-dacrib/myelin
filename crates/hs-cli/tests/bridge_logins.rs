//! Who has signed in to a bridge, through the real binary: `GET /api/v1/appservices/{id}/logins`.
//!
//! One real `hs serve` process. A mautrix-whatsapp bridge is added the way the interface's
//! wizard adds one: rendered from the catalogue (which mints the provisioning secret and writes it
//! into both the bridge's `config.yaml` and the registration), then registered. The bridge is an
//! axum listener in this process serving mautrix `bridgev2`'s `/_matrix/provision/v3/whoami`,
//! which accepts only the secret from the rendered `config.yaml`, as a real bridge would: alice
//! has signed in with one WhatsApp account, bob with none. Then a heisenbridge (no provisioning
//! API: the answer says so, with a `200`) and a mautrix-signal whose bridge is not running
//! (an error in the answer, not a failed request), and the counter and log line for each.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

#[derive(Clone)]
struct Admin {
    base: String,
    token: Option<String>,
}

impl Admin {
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

/// A mautrix bridgev2 provisioning API, as far as `whoami` goes. Accepts only `secret`.
#[derive(Clone, Default)]
struct Bridge {
    secret: Arc<Mutex<String>>,
    asked: Arc<AtomicUsize>,
}

async fn whoami(
    axum::extract::State(bridge): axum::extract::State<Bridge>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (axum::http::StatusCode, axum::Json<Value>) {
    bridge.asked.fetch_add(1, Ordering::SeqCst);
    let expected = format!("Bearer {}", bridge.secret.lock().unwrap());
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(json!({"errcode": "M_UNKNOWN_TOKEN", "error": "Invalid auth token"})),
        );
    }
    let logins = if query.get("user_id").map(String::as_str) == Some("@alice:example.org") {
        json!([{
            "id": "15551234567",
            "name": "+1 555-123-4567",
            "state_event": "CONNECTED",
            "state_ts": 1_790_000_000,
            "profile": {"phone": "+15551234567"},
        }])
    } else {
        json!([])
    };
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({
            "network": {"displayname": "WhatsApp", "network_id": "whatsapp"},
            "bridge_bot": "@whatsappbot:example.org",
            "command_prefix": "!wa",
            "logins": logins,
        })),
    )
}

async fn start_bridge() -> (String, Bridge) {
    let bridge = Bridge::default();
    let app = axum::Router::new()
        .route("/_matrix/provision/v3/whoami", axum::routing::get(whoami))
        .with_state(bridge.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, bridge)
}

/// Renders `type_id` from the catalogue and registers what it rendered, as the wizard does.
/// Returns the render.
async fn add_bridge(admin: &Admin, type_id: &str, choices: Value) -> Value {
    let render = admin
        .expect(
            Method::POST,
            &format!("/api/v1/bridge-types/{type_id}/render"),
            Some(choices),
            StatusCode::OK,
        )
        .await;
    admin
        .expect(
            Method::POST,
            "/api/v1/appservices",
            Some(json!({"registration": render["registration"]})),
            StatusCode::CREATED,
        )
        .await;
    render
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_admin_api_says_who_has_signed_in_to_a_bridge_and_says_so_when_it_cannot() {
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

    // The catalogue says which types report sign-ins.
    let whatsapp_type = admin
        .expect(
            Method::GET,
            "/api/v1/bridge-types/mautrix-whatsapp",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(whatsapp_type["provisioning_api"], "mautrix_v3");

    // A mautrix-whatsapp bridge, added from the catalogue, running at the stand-in. The secret
    // it accepts is the one the render wrote into the config.yaml it would run with.
    let (bridge_url, bridge) = start_bridge().await;
    let render = add_bridge(
        &admin,
        "mautrix-whatsapp",
        json!({"id": "whatsapp", "bridgeAddress": bridge_url}),
    )
    .await;
    let config: Value = serde_yaml_ng::from_str(render["config_yaml"].as_str().unwrap()).unwrap();
    let secret = config["provisioning"]["shared_secret"]
        .as_str()
        .expect("the rendered config.yaml carries the provisioning secret");
    *bridge.secret.lock().unwrap() = secret.to_owned();

    let alice = admin
        .expect(
            Method::GET,
            "/api/v1/appservices/whatsapp/logins?user_id=%40alice%3Aexample.org",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(alice["supported"], true, "{alice}");
    assert_eq!(alice["signed_in"], true, "{alice}");
    assert_eq!(alice["cached"], false);
    assert_eq!(alice["logins"][0]["user_id"], "@alice:example.org");
    assert_eq!(alice["logins"][0]["remote_id"], "15551234567");
    assert_eq!(alice["logins"][0]["remote_name"], "+1 555-123-4567");
    assert_eq!(alice["logins"][0]["state"], "connected");
    assert_eq!(alice["logins"][0]["since"], "2026-09-21T14:13:20.000Z");

    let bob = admin
        .expect(
            Method::GET,
            "/api/v1/appservices/whatsapp/logins?user_id=%40bob%3Aexample.org",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(bob["signed_in"], false, "{bob}");
    assert_eq!(bob["logins"], json!([]));

    // Asked again within 30 seconds: the same answer, and the bridge was not asked again.
    let again = admin
        .expect(
            Method::GET,
            "/api/v1/appservices/whatsapp/logins?user_id=%40alice%3Aexample.org",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(again["cached"], true);
    assert_eq!(again["logins"], alice["logins"]);
    assert_eq!(bridge.asked.load(Ordering::SeqCst), 2);

    // A shared bridge has to be told whom to ask about.
    admin
        .expect(
            Method::GET,
            "/api/v1/appservices/whatsapp/logins",
            None,
            StatusCode::BAD_REQUEST,
        )
        .await;

    // heisenbridge has no provisioning API: a 200 that says so, never a 501.
    add_bridge(
        &admin,
        "heisenbridge",
        json!({"id": "irc", "bridgeAddress": bridge_url}),
    )
    .await;
    let irc = admin
        .expect(
            Method::GET,
            "/api/v1/appservices/irc/logins",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(irc["supported"], false, "{irc}");
    assert_eq!(irc["provisioning_api"], "none");
    assert!(irc["reason"].as_str().unwrap().contains("control room"));

    // A mautrix-signal bridge that is not running: an answer with the error in it.
    let closed = format!("http://127.0.0.1:{}", reserve_port());
    add_bridge(
        &admin,
        "mautrix-signal",
        json!({"id": "signal", "bridgeAddress": closed}),
    )
    .await;
    let signal = admin
        .expect(
            Method::GET,
            "/api/v1/appservices/signal/logins?user_id=%40alice%3Aexample.org",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(signal["supported"], true, "{signal}");
    assert_eq!(signal["signed_in"], Value::Null);
    assert_eq!(signal["error"]["status"], 502, "{signal}");
    assert_eq!(signal["error"]["reason"], "unreachable");
    hs.wait_for("could not ask a bridge who has signed in");

    // And every answer is counted.
    let metrics = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for series in [
        "hs_admin_bridge_login_queries_total{type=\"mautrix-whatsapp\",outcome=\"answered\"} 2",
        "hs_admin_bridge_login_queries_total{type=\"mautrix-whatsapp\",outcome=\"cached\"} 1",
        "hs_admin_bridge_login_queries_total{type=\"heisenbridge\",outcome=\"unsupported\"} 1",
        "hs_admin_bridge_login_queries_total{type=\"mautrix-signal\",outcome=\"unreachable\"} 1",
    ] {
        assert!(metrics.contains(series), "{series} not in:\n{metrics}");
    }
}
