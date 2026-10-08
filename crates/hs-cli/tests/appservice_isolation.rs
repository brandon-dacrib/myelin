//! One bridge's slowness never delays another's delivery, against the real binary.
//!
//! Two bridges are registered with one `hs serve`. `stuck` takes the connection when the server
//! asks it whether a user of its namespace exists (`GET /_matrix/app/v1/users/{userId}`) and
//! never answers; `fine` hears every room (its room namespace is `!.*`). A person invites a user
//! of `stuck`'s who has no account, and then says something. Until 2026-10-08 the server asked
//! that question from the one task that queues every room's events for every bridge, so `fine`
//! heard nothing for the question's ten-second timeout. Now `stuck`'s own delivery worker asks
//! it (`hs_appservice::known_users`, decision 0033): `fine` hears both events within a second,
//! `stuck` is sent the invitation once its question has timed out, and meanwhile the queue
//! gauges and the admin API's `AppService.queue` say which bridge is behind.
//!
//! The bridges are axum listeners in this process, as in `appservice_queries.rs`. Every check
//! waits for a condition, except the one that is about time.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

    /// Reads the log until a line contains `needle`. The timeout only bounds how long a broken
    /// server can hang the suite (a cold debug boot under load takes up to a minute).
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

    /// Everything logged so far, the lines not yet read included.
    fn log(&mut self) -> String {
        while let Ok(line) = self.lines.try_recv() {
            self.seen.push(line);
        }
        self.seen.join("\n")
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

fn config_yaml(
    client_port: u16,
    data_dir: &std::path::Path,
    registrations: &[std::path::PathBuf],
) -> String {
    let media_dir = data_dir.join("media");
    let files = registrations
        .iter()
        .map(|p| format!("{p:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {client_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n\
         appservices:\n  registration_files: [{files}]\n"
    )
}

fn token(kind: &str, id: &str) -> String {
    format!("{kind}_token_for_the_{id}_bridge_{}", "0".repeat(40))
}

/// `stuck`: exclusive `@stuck_` users. `fine`: exclusive `@fine_` users, and every room.
fn registration_yaml(id: &str, url: &str) -> String {
    let rooms = if id == "fine" {
        "\x20 rooms:\n    - regex: '!.*'\n      exclusive: false\n"
    } else {
        ""
    };
    format!(
        "id: {id}\nurl: {url}\nas_token: {}\nhs_token: {}\n\
         sender_localpart: {id}bot\nrate_limited: false\n\
         namespaces:\n  users:\n    - regex: '@{id}_.*:example\\.org'\n      exclusive: true\n{rooms}",
        token("as", id),
        token("hs", id),
    )
}

// ---------------------------------------------------------------------------------------------
// The bridges.
// ---------------------------------------------------------------------------------------------

/// What a bridge was sent and asked, and when each event arrived.
#[derive(Clone)]
struct Bridge {
    id: &'static str,
    events: Arc<Mutex<Vec<(Instant, Value)>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Bridge {
    fn bodies(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, e)| {
                e["content"]["body"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| e["state_key"].as_str().map(|s| format!("member {s}")))
            })
            .collect()
    }

    fn arrived(&self, what: &str) -> Option<Instant> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .find(|(_, e)| {
                e["content"]["body"] == what
                    || e["state_key"].as_str() == what.strip_prefix("member ")
            })
            .map(|(at, _)| *at)
    }
}

async fn start_bridge(id: &'static str) -> (String, Bridge) {
    use axum::extract::{Path, State};
    use axum::http::HeaderMap;

    type Answer = (axum::http::StatusCode, axum::Json<Value>);

    fn authorized(bridge: &Bridge, headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {}", token("hs", bridge.id)))
    }

    async fn transaction(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> Answer {
        assert!(authorized(&bridge, &headers));
        let now = Instant::now();
        let mut events = bridge.events.lock().unwrap();
        for event in body["events"].as_array().cloned().unwrap_or_default() {
            events.push((now, event));
        }
        (axum::http::StatusCode::OK, axum::Json(json!({})))
    }

    /// `stuck` takes the question and never answers; `fine` knows nobody.
    async fn user(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        Path(user): Path<String>,
    ) -> Answer {
        assert!(authorized(&bridge, &headers));
        bridge.asked.lock().unwrap().push(user);
        if bridge.id == "stuck" {
            std::future::pending::<()>().await;
        }
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"errcode": "M_NOT_FOUND"})),
        )
    }

    let bridge = Bridge {
        id,
        events: Arc::default(),
        asked: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn}",
            axum::routing::put(transaction),
        )
        .route("/_matrix/app/v1/users/{user}", axum::routing::get(user))
        .with_state(bridge.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, bridge)
}

// ---------------------------------------------------------------------------------------------
// Clients.
// ---------------------------------------------------------------------------------------------

async fn ok(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<Value>,
) -> Value {
    let mut request = client.request(method, &url).bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "{url}: {status} {body}");
    body
}

async fn until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..600 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never happened: {what}");
}

/// The value of the series that starts with `series` (its full name and labels).
fn sample(metrics: &str, series: &str) -> Option<f64> {
    metrics
        .lines()
        .find(|l| l.starts_with(series))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

// ---------------------------------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_bridge_that_never_answers_holds_only_its_own_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let (stuck_url, stuck) = start_bridge("stuck").await;
    let (fine_url, fine) = start_bridge("fine").await;
    let mut registrations = Vec::new();
    for (id, url) in [("stuck", &stuck_url), ("fine", &fine_url)] {
        let path = dir.path().join(format!("{id}.yaml"));
        std::fs::write(&path, registration_yaml(id, url)).unwrap();
        registrations.push(path);
    }
    let port = reserve_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        config_yaml(port, &dir.path().join("data"), &registrations),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut server = HsProcess::serve(&config_path);
    let setup_line = server.wait_for("setup_link=");

    // The first administrator, for the admin API's view.
    let setup_token = setup_line
        .split("#token=")
        .nth(1)
        .unwrap_or_else(|| panic!("no setup token in {setup_line:?}"))
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect::<String>();
    let session: Value = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_token = session["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("{session}"))
        .to_owned();

    let registered = ok(
        &client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/register"),
        "",
        Some(json!({"username": "alice", "password": "hunter2-alice-long", "auth": {"type": "m.login.dummy"}})),
    )
    .await;
    let alice = registered["access_token"].as_str().unwrap().to_owned();
    let room_id = ok(
        &client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/createRoom"),
        &alice,
        Some(json!({"preset": "public_chat"})),
    )
    .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let room = room_id.replace('!', "%21").replace(':', "%3A");

    // An invitation to a user of `stuck`'s who has no account: `stuck` is asked about them and
    // never answers. Then a message.
    ok(
        &client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/rooms/{room}/invite"),
        &alice,
        Some(json!({"user_id": "@stuck_ghost:example.org"})),
    )
    .await;
    ok(
        &client,
        reqwest::Method::PUT,
        format!("{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/t1"),
        &alice,
        Some(json!({"msgtype": "m.text", "body": "hello, bridges"})),
    )
    .await;
    let sent = Instant::now();

    until("fine hears the invitation and the message", || async {
        fine.arrived("hello, bridges").is_some()
    })
    .await;
    let invite_at = fine.arrived("member @stuck_ghost:example.org").unwrap();
    let message_at = fine.arrived("hello, bridges").unwrap();
    assert!(
        invite_at.max(message_at).saturating_duration_since(sent) < Duration::from_secs(1),
        "fine heard the room {:?} after the message was sent, while stuck was being asked",
        invite_at.max(message_at).saturating_duration_since(sent)
    );
    until("stuck is asked about its ghost", || async {
        stuck
            .asked
            .lock()
            .unwrap()
            .contains(&"@stuck_ghost:example.org".to_owned())
    })
    .await;
    assert!(
        stuck.bodies().is_empty(),
        "stuck is sent nothing before its question is over: {:?}",
        stuck.bodies()
    );

    // While stuck's question is open, the gauges and the admin API say it is the one behind.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let stuck_depth = sample(&metrics, "hs_appservice_queue_depth{appservice=\"stuck\"}");
    assert!(stuck_depth.is_some_and(|d| d >= 1.0), "{metrics}");
    assert_eq!(
        sample(&metrics, "hs_appservice_queue_depth{appservice=\"fine\"}"),
        Some(0.0),
        "{metrics}"
    );
    assert!(
        sample(
            &metrics,
            "hs_appservice_queue_oldest_age_seconds{appservice=\"stuck\"}"
        )
        .is_some_and(|age| age >= 1.0),
        "{metrics}"
    );
    let listed = ok(
        &client,
        reqwest::Method::GET,
        format!("{base}/api/v1/appservices"),
        &admin_token,
        None,
    )
    .await;
    let row = |id: &str| {
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} not in {listed}"))
    };
    assert!(
        row("stuck")["queue"]["pending"].as_u64().unwrap() >= 1,
        "{listed}"
    );
    assert!(
        row("stuck")["queue"]["oldest_pending_age_ms"]
            .as_u64()
            .unwrap()
            >= 1_000,
        "{listed}"
    );
    assert_eq!(row("fine")["queue"]["pending"], 0, "{listed}");
    assert_eq!(
        row("fine")["queue"]["oldest_pending_age_ms"],
        Value::Null,
        "{listed}"
    );

    // Once the question times out (ten seconds), stuck is sent the invitation anyway. (Not the
    // message: nobody of stuck's is in the room, and an invitation is not membership.)
    until("stuck is sent the invitation", || async {
        !stuck.bodies().is_empty()
    })
    .await;
    assert_eq!(stuck.bodies(), vec!["member @stuck_ghost:example.org"]);
    assert!(
        sent.elapsed() >= Duration::from_secs(5),
        "stuck was sent its events before its question could have timed out"
    );
    until("stuck's queue is empty again", || async {
        let metrics = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        sample(&metrics, "hs_appservice_queue_depth{appservice=\"stuck\"}") == Some(0.0)
    })
    .await;

    let log = server.log();
    assert!(
        log.contains("an appservice did not answer the homeserver's question"),
        "the unanswered question is logged"
    );
}
