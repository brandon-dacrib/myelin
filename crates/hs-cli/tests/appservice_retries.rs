//! A failed transaction is sent again under the same id with the same contents, against the
//! real binary; while it waits, the admin API says what it carries and why the bridge is not
//! healthy, and the metrics count the failure, its reason and the retry.
//!
//! The demo on 2026-10-15: Discord's dead letters were replayed and the bridge timed out on
//! every transaction, the Bridges page said "Degraded" with no reason, and the metrics had only
//! `hs_appservice_transactions_total{outcome="failed"}`. One bridge here, `flaky`, hears every
//! room (`!.*`) and answers `503` while the test says so. The bridge is an axum listener in this
//! process, as in `appservice_isolation.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

    /// Reads the log until a line contains `needle` (a cold debug boot under load takes up to a
    /// minute).
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

fn token(kind: &str) -> String {
    format!("{kind}_token_for_the_flaky_bridge_{}", "0".repeat(40))
}

/// One transaction as the bridge received it.
#[derive(Clone, Debug)]
struct Received {
    txn_id: String,
    body: Value,
    answered_ok: bool,
}

#[derive(Clone)]
struct Bridge {
    failing: Arc<AtomicBool>,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Bridge {
    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    /// The bodies of the `m.room.message` events it accepted.
    fn accepted_messages(&self) -> Vec<String> {
        self.received()
            .iter()
            .filter(|r| r.answered_ok)
            .flat_map(|r| r.body["events"].as_array().cloned().unwrap_or_default())
            .filter(|e| e["type"] == "m.room.message")
            .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
            .collect()
    }
}

async fn start_bridge() -> (String, Bridge) {
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};

    async fn transaction(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        Path(txn_id): Path<String>,
        axum::Json(body): axum::Json<Value>,
    ) -> (StatusCode, axum::Json<Value>) {
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {}", token("hs")).as_str())
        );
        let failing = bridge.failing.load(Ordering::SeqCst);
        bridge.received.lock().unwrap().push(Received {
            txn_id,
            body,
            answered_ok: !failing,
        });
        if failing {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"errcode": "M_UNKNOWN", "error": "restarting"})),
            )
        } else {
            (StatusCode::OK, axum::Json(json!({})))
        }
    }

    async fn user() -> (StatusCode, axum::Json<Value>) {
        (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"errcode": "M_NOT_FOUND"})),
        )
    }

    let bridge = Bridge {
        failing: Arc::new(AtomicBool::new(false)),
        received: Arc::default(),
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

async fn call(
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

#[tokio::test]
async fn a_failed_transaction_is_retried_under_its_own_id_and_the_admin_api_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge_url, bridge) = start_bridge().await;
    let registration = dir.path().join("flaky.yaml");
    std::fs::write(
        &registration,
        format!(
            "id: flaky\nurl: {bridge_url}\nas_token: {}\nhs_token: {}\n\
             sender_localpart: flakybot\nrate_limited: false\n\
             namespaces:\n  users:\n    - regex: '@flaky_.*:example\\.org'\n      exclusive: true\n\
             \x20 rooms:\n    - regex: '!.*'\n      exclusive: false\n",
            token("as"),
            token("hs"),
        ),
    )
    .unwrap();
    let listener = hs_testkit::ports::reserve();
    let port = listener.port();
    let data_dir = dir.path().join("data");
    let media_dir = data_dir.join("media");
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
             storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
             media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
             auth:\n  enable_registration: true\n\
             rate_limits:\n  enabled: false\n\
             appservices:\n  registration_files: [{registration:?}]\n"
        ),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut server = HsProcess::serve(&config_path, listener);
    let setup_line = server.wait_for("setup_link=");
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
    let admin = session["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("{session}"))
        .to_owned();

    let alice = call(
        &client,
        reqwest::Method::POST,
        format!("{base}/_matrix/client/v3/register"),
        "",
        Some(json!({"username": "alice", "password": "hunter2-alice-long", "auth": {"type": "m.login.dummy"}})),
    )
    .await["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let room_id = call(
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
    until("the bridge hears the room's creation", || async {
        bridge.received().iter().any(|r| r.answered_ok)
    })
    .await;

    // The bridge goes down (503, as a restarting pod behind a Service answers), and a message
    // is sent.
    bridge.failing.store(true, Ordering::SeqCst);
    call(
        &client,
        reqwest::Method::PUT,
        format!("{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/t1"),
        &alice,
        Some(json!({"msgtype": "m.text", "body": "while it was down"})),
    )
    .await;
    until("the message is refused three times", || async {
        bridge.received().iter().filter(|r| !r.answered_ok).count() >= 3
    })
    .await;

    // While it waits, the admin API says what it carries and why the bridge is not healthy.
    let backlog = call(
        &client,
        reqwest::Method::GET,
        format!("{base}/api/v1/appservices/flaky/backlog"),
        &admin,
        None,
    )
    .await;
    let waiting = &backlog["items"][0];
    assert_eq!(waiting["dead_lettered"], false, "{backlog}");
    assert!(waiting["events"].as_u64().unwrap() >= 1, "{backlog}");
    assert_eq!(waiting["room_ids"], json!([room_id]), "{backlog}");
    assert!(waiting["attempts"].as_u64().unwrap() >= 3, "{backlog}");
    assert!(waiting["enqueued_at"].is_string(), "{backlog}");
    let row = call(
        &client,
        reqwest::Method::GET,
        format!("{base}/api/v1/appservices/flaky"),
        &admin,
        None,
    )
    .await;
    assert_eq!(row["health"], "degraded", "{row}");
    let reason = row["health_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("transactions to it failed") && reason.contains("503"),
        "{row}"
    );

    // Nothing to discard: the entry waits, it is not dead-lettered.
    let discarded = call(
        &client,
        reqwest::Method::POST,
        format!("{base}/api/v1/appservices/flaky/discard"),
        &admin,
        Some(json!({})),
    )
    .await;
    assert_eq!(discarded["discarded"], 0);

    // The bridge comes back: the message is delivered once.
    bridge.failing.store(false, Ordering::SeqCst);
    until("the message is delivered", || async {
        bridge
            .accepted_messages()
            .contains(&"while it was down".to_owned())
    })
    .await;
    assert_eq!(bridge.accepted_messages(), vec!["while it was down"]);

    // Every attempt at the message's transaction went under one id, with the same contents.
    let received = bridge.received();
    let carrying: Vec<&Received> = received
        .iter()
        .filter(|r| {
            r.body["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|e| e["type"] == "m.room.message"))
        })
        .collect();
    assert!(carrying.len() >= 4, "{carrying:?}");
    assert!(
        carrying.iter().all(|r| r.txn_id == carrying[0].txn_id),
        "one id: {:?}",
        carrying.iter().map(|r| &r.txn_id).collect::<Vec<_>>()
    );
    assert!(
        carrying.iter().all(|r| r.body == carrying[0].body),
        "the same contents"
    );

    let row = call(
        &client,
        reqwest::Method::GET,
        format!("{base}/api/v1/appservices/flaky"),
        &admin,
        None,
    )
    .await;
    assert_eq!(row["health"], "healthy", "{row}");
    assert_eq!(row["health_reason"], Value::Null, "{row}");

    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for (series, at_least) in [
        (
            "hs_appservice_transaction_failures_total{appservice=\"flaky\",reason=\"unavailable\"}",
            3.0,
        ),
        (
            "hs_appservice_transaction_retries_total{appservice=\"flaky\"}",
            3.0,
        ),
        (
            "hs_appservice_transaction_seconds_count{appservice=\"flaky\",outcome=\"delivered\"}",
            2.0,
        ),
        (
            "hs_appservice_transaction_seconds_count{appservice=\"flaky\",outcome=\"failed\"}",
            3.0,
        ),
    ] {
        assert!(
            sample(&metrics, series).is_some_and(|v| v >= at_least),
            "{series} >= {at_least}: {}",
            metrics
                .lines()
                .filter(|l| l.contains("flaky"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    assert_eq!(
        sample(
            &metrics,
            "hs_appservice_dead_lettered_total{appservice=\"flaky\"}"
        ),
        None,
        "nothing dead-lettered"
    );

    let log = server.log();
    let failure = log
        .lines()
        .find(|l| l.contains("could not deliver to this appservice"))
        .unwrap_or_else(|| panic!("no failure logged:\n{log}"));
    assert!(failure.contains("reason=\"unavailable\""), "{failure}");
    assert!(
        failure.contains(&format!("txn_id={} ", carrying[0].txn_id)),
        "{failure}"
    );
}
