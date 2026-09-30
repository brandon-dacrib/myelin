//! A bridge is sent typing, receipts, presence, to-device messages, device-list changes and
//! one-time-key counts by the real binary (MSC2409, MSC4203, MSC3202), once each, across a
//! restart, and not while it is paused.
//!
//! One real `hs serve` process over a data directory, started from a configuration file that
//! imports a registration with `receive_ephemeral` and `org.matrix.msc3202`, whose `url` is an
//! axum listener in this process standing in for the bridge (what `bridge_offerings.rs` does
//! for its bridges). Alice and bob are people; `@ghost_alice` is the bridge's, registered and
//! joined through the appservice API. Every check waits for a condition, never a duration.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// The real binary.
// ---------------------------------------------------------------------------------------------

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

    /// Reads the log until a line contains `needle`. A condition, not a duration: the timeout
    /// only bounds how long a broken server can hang the suite.
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

    /// Asks the server to stop the way `docker stop` or Kubernetes would, waits for it to, and
    /// returns everything it logged.
    fn stop(mut self) -> String {
        let pid = self.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let _ = self.child.wait();
        while let Ok(line) = self.lines.recv() {
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

/// A port for a server started from a configuration file, which has to name one.
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
    registration: &std::path::Path,
) -> String {
    let media_dir = data_dir.join("media");
    format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: {client_port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n\
         appservices:\n  registration_files: [{registration:?}]\n"
    )
}

const AS_TOKEN: &str = "as_token_for_the_ephemeral_bridge_test_0000000000000000000000000";
const HS_TOKEN: &str = "hs_token_for_the_ephemeral_bridge_test_0000000000000000000000000";

/// A mautrix-shaped registration: ephemeral events and MSC3202 both on, exclusive ghosts.
fn registration_yaml(url: &str) -> String {
    format!(
        "id: ephemeral-bridge\nurl: {url}\nas_token: {AS_TOKEN}\nhs_token: {HS_TOKEN}\n\
         sender_localpart: bridgebot\nrate_limited: false\nreceive_ephemeral: true\n\
         org.matrix.msc3202: true\nnamespaces:\n  users:\n    - regex: '@ghost_.*:example\\.org'\n      exclusive: true\n"
    )
}

// ---------------------------------------------------------------------------------------------
// The bridge, as far as the server can tell.
// ---------------------------------------------------------------------------------------------

/// Answers pings with the registration's `hs_token` and records every transaction, in order.
#[derive(Clone, Default)]
struct StandIn {
    transactions: Arc<Mutex<Vec<Value>>>,
}

impl StandIn {
    fn transactions(&self) -> Vec<Value> {
        self.transactions.lock().unwrap().clone()
    }

    /// Every ephemeral event of type `kind` sent so far, in order.
    fn ephemeral(&self, kind: &str) -> Vec<Value> {
        self.transactions()
            .iter()
            .flat_map(|txn| {
                txn["ephemeral"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
            })
            .filter(|e| e["type"] == kind)
            .collect()
    }

    fn to_device(&self) -> Vec<Value> {
        self.transactions()
            .iter()
            .flat_map(|txn| {
                txn["to_device"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
            })
            .collect()
    }

    fn device_list_changes(&self) -> Vec<String> {
        self.transactions()
            .iter()
            .flat_map(|txn| {
                txn["device_lists"]["changed"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
            })
            .filter_map(|u| u.as_str().map(str::to_owned))
            .collect()
    }

    fn events(&self) -> Vec<Value> {
        self.transactions()
            .iter()
            .flat_map(|txn| {
                txn["events"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
            })
            .collect()
    }

    /// How much of each kind has been sent, for "nothing more came".
    fn tally(&self) -> (usize, usize, usize, usize, usize, usize) {
        (
            self.events().len(),
            self.ephemeral("m.typing").len(),
            self.ephemeral("m.receipt").len(),
            self.ephemeral("m.presence").len(),
            self.to_device().len(),
            self.device_list_changes().len(),
        )
    }
}

async fn stand_in() -> (String, StandIn) {
    async fn ping(headers: axum::http::HeaderMap) -> (axum::http::StatusCode, axum::Json<Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth == format!("Bearer {HS_TOKEN}") {
            (axum::http::StatusCode::OK, axum::Json(json!({})))
        } else {
            (
                axum::http::StatusCode::FORBIDDEN,
                axum::Json(json!({"errcode": "M_FORBIDDEN"})),
            )
        }
    }
    async fn transaction(
        axum::extract::State(bridge): axum::extract::State<StandIn>,
        headers: axum::http::HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth != format!("Bearer {HS_TOKEN}") {
            return (
                axum::http::StatusCode::FORBIDDEN,
                axum::Json(json!({"errcode": "M_FORBIDDEN"})),
            );
        }
        bridge.transactions.lock().unwrap().push(body);
        (axum::http::StatusCode::OK, axum::Json(json!({})))
    }
    let bridge = StandIn::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route("/_matrix/app/v1/ping", axum::routing::post(ping))
        .route(
            "/_matrix/app/v1/transactions/{txn}",
            axum::routing::put(transaction),
        )
        .with_state(bridge.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, bridge)
}

// ---------------------------------------------------------------------------------------------
// Clients.
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Caller {
    client: reqwest::Client,
    base: String,
    token: String,
    /// `?user_id=` on every call: an appservice acting as one of its users.
    masquerade: Option<String>,
}

impl Caller {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Value) {
        let mut url = format!("{}{path}", self.base);
        if let Some(user) = &self.masquerade {
            url.push(if path.contains('?') { '&' } else { '?' });
            url.push_str(&format!("user_id={}", escape(user)));
        }
        let mut request = self
            .client
            .request(method, url)
            .header("accept", "application/json")
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn matrix(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self
            .call(method, &format!("/_matrix/client/v3{path}"), body)
            .await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    async fn admin(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, &format!("/api/v1{path}"), body).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    async fn say(&self, room_id: &str, text: &str) -> String {
        let txn = format!("t{}", nanos());
        let sent = self
            .matrix(
                reqwest::Method::PUT,
                &format!("/rooms/{}/send/m.room.message/{txn}", escape(room_id)),
                Some(json!({"msgtype": "m.text", "body": text})),
            )
            .await;
        sent["event_id"].as_str().unwrap().to_owned()
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
}

/// Polls `f` until it says yes, or panics after a bound.
async fn until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..240 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("never happened: {what}");
}

/// Registers `name` through the client API.
async fn register(client: &reqwest::Client, base: &str, name: &str) -> Caller {
    let registered: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    Caller {
        client: client.clone(),
        base: base.to_owned(),
        token: registered["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {registered}"))
            .to_owned(),
        masquerade: None,
    }
}

/// Registers `name` as the bridge does (`m.login.application_service`), returning the account's
/// own session (its device is what to-device messages are addressed to) and the appservice
/// masquerading as it.
async fn register_as_bridge(
    client: &reqwest::Client,
    base: &str,
    name: &str,
) -> (Caller, Caller, String) {
    let registered: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .bearer_auth(AS_TOKEN)
        .json(&json!({"type": "m.login.application_service", "username": name}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_id = format!("@{name}:example.org");
    assert_eq!(registered["user_id"], user_id, "{registered}");
    let own = Caller {
        client: client.clone(),
        base: base.to_owned(),
        token: registered["access_token"].as_str().unwrap().to_owned(),
        masquerade: None,
    };
    let masqueraded = Caller {
        client: client.clone(),
        base: base.to_owned(),
        token: AS_TOKEN.to_owned(),
        masquerade: Some(user_id),
    };
    let device_id = registered["device_id"].as_str().unwrap().to_owned();
    (own, masqueraded, device_id)
}

/// The first administrator, from the setup link `hs serve` logged.
async fn admin_token_of(server: &mut HsProcess, client: &reqwest::Client, base: &str) -> String {
    let setup_line = server.wait_for("setup_link=");
    let setup_token: String = setup_line
        .split_once("/admin/setup#token=")
        .unwrap_or_else(|| panic!("not a setup link: {setup_line}"))
        .1
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let session: Value = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-ops-ephemeral"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    session["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("{session}"))
        .to_owned()
}

/// Device keys as a client uploads them: the server distributes them and never reads inside.
fn device_keys(user_id: &str, device_id: &str) -> Value {
    json!({
        "user_id": user_id,
        "device_id": device_id,
        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        "keys": {
            format!("curve25519:{device_id}"): "3C5BFWi2Y8MaVvjM8M22DBmh24PmgR0nPvJOIArzgyI",
            format!("ed25519:{device_id}"): "Ed25519KeyOfThisDeviceXXXXXXXXXXXXXXXXXXXXX",
        },
        "signatures": {user_id: {format!("ed25519:{device_id}"): "sig"}},
    })
}

// ---------------------------------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_bridge_is_sent_ephemeral_data_once_across_a_restart_and_not_while_paused() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge_url, bridge) = stand_in().await;
    let registration_path = dir.path().join("registration.yaml");
    std::fs::write(&registration_path, registration_yaml(&bridge_url)).unwrap();
    let port = reserve_port();
    let data_dir = dir.path().join("data");
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        config_yaml(port, &data_dir, &registration_path),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let mut server = HsProcess::serve(&config_path);
    // The setup link is logged once the server is listening.
    let admin_token = admin_token_of(&mut server, &client, &base).await;
    let admin = Caller {
        client: client.clone(),
        base: base.clone(),
        token: admin_token,
        masquerade: None,
    };
    let listed = admin
        .admin(reqwest::Method::GET, "/appservices", None)
        .await;
    assert!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "ephemeral-bridge"),
        "the registration file was imported: {listed}"
    );

    let alice = register(&client, &base, "alice").await;
    let bob = register(&client, &base, "bob").await;
    let (bot_own, _bot, bot_device) = register_as_bridge(&client, &base, "bridgebot").await;
    let (_ghost_own, ghost, ghost_device) = register_as_bridge(&client, &base, "ghost_alice").await;
    let alice_id = "@alice:example.org";
    let ghost_id = "@ghost_alice:example.org";

    // A room with the ghost in it: from here on the bridge is interested in it.
    let created = alice
        .matrix(
            reqwest::Method::POST,
            "/createRoom",
            Some(json!({"preset": "private_chat", "invite": [ghost_id]})),
        )
        .await;
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    ghost
        .matrix(
            reqwest::Method::POST,
            &format!("/join/{}", escape(&room_id)),
            None,
        )
        .await;
    until("the ghost's join reached the bridge", || async {
        bridge
            .events()
            .iter()
            .any(|e| e["type"] == "m.room.member" && e["state_key"] == ghost_id)
    })
    .await;
    // A room without the ghost: nothing from here reaches the bridge.
    let private = bob
        .matrix(
            reqwest::Method::POST,
            "/createRoom",
            Some(json!({"preset": "private_chat", "invite": [alice_id]})),
        )
        .await;
    let private_room = private["room_id"].as_str().unwrap().to_owned();
    alice
        .matrix(
            reqwest::Method::POST,
            &format!("/join/{}", escape(&private_room)),
            None,
        )
        .await;

    // Typing: the room's current typing set, as `m.typing`.
    for room in [&private_room, &room_id] {
        alice
            .matrix(
                reqwest::Method::PUT,
                &format!("/rooms/{}/typing/{}", escape(room), escape(alice_id)),
                Some(json!({"typing": true, "timeout": 30000})),
            )
            .await;
    }
    until("alice's typing reached the bridge", || async {
        !bridge.ephemeral("m.typing").is_empty()
    })
    .await;
    let typing = bridge.ephemeral("m.typing");
    assert_eq!(
        typing[0],
        json!({"type": "m.typing", "room_id": room_id, "content": {"user_ids": [alice_id]}}),
        "{typing:?}"
    );
    assert!(
        typing.iter().all(|t| t["room_id"] == room_id),
        "the private room's typing is not the bridge's: {typing:?}"
    );

    // A receipt on a message: `m.receipt` in the spec's shape.
    let event_id = alice.say(&room_id, "read me").await;
    let private_event = alice.say(&private_room, "not for the bridge").await;
    alice
        .matrix(
            reqwest::Method::POST,
            &format!(
                "/rooms/{}/receipt/m.read/{}",
                escape(&room_id),
                escape(&event_id)
            ),
            Some(json!({})),
        )
        .await;
    alice
        .matrix(
            reqwest::Method::POST,
            &format!(
                "/rooms/{}/receipt/m.read/{}",
                escape(&private_room),
                escape(&private_event)
            ),
            Some(json!({})),
        )
        .await;
    until("alice's receipt reached the bridge", || async {
        !bridge.ephemeral("m.receipt").is_empty()
    })
    .await;
    let receipts = bridge.ephemeral("m.receipt");
    assert_eq!(receipts[0]["room_id"], room_id, "{receipts:?}");
    assert!(
        receipts[0]["content"][&event_id]["m.read"][alice_id]["ts"].is_u64(),
        "{receipts:?}"
    );
    assert!(
        receipts.iter().all(|r| r["room_id"] == room_id),
        "{receipts:?}"
    );

    // Presence: `m.presence` from a room-mate of the ghost.
    alice
        .matrix(
            reqwest::Method::PUT,
            &format!("/presence/{}/status", escape(alice_id)),
            Some(json!({"presence": "unavailable", "status_msg": "bridging"})),
        )
        .await;
    until("alice's presence reached the bridge", || async {
        bridge
            .ephemeral("m.presence")
            .iter()
            .any(|p| p["sender"] == alice_id && p["content"]["presence"] == "unavailable")
    })
    .await;
    let presence = bridge
        .ephemeral("m.presence")
        .into_iter()
        .find(|p| p["content"]["presence"] == "unavailable")
        .unwrap();
    assert_eq!(presence["content"]["status_msg"], "bridging", "{presence}");
    assert!(
        presence["content"]["last_active_ago"].is_u64(),
        "{presence}"
    );
    assert!(
        presence["content"].get("user_id").is_none(),
        "no user_id inside, as Synapse: {presence}"
    );

    // A to-device message to the ghost's device: the event with its addressing keys.
    alice
        .matrix(
            reqwest::Method::PUT,
            &format!("/sendToDevice/m.room_key/td{}", nanos()),
            Some(json!({"messages": {ghost_id: {&ghost_device: {"algorithm": "m.megolm.v1.aes-sha2", "session_key": "s"}}}})),
        )
        .await;
    // ...and one to bob, which is nobody's business but bob's.
    alice
        .matrix(
            reqwest::Method::PUT,
            &format!("/sendToDevice/m.room_key/td{}", nanos()),
            Some(json!({"messages": {"@bob:example.org": {"*": {"algorithm": "x"}}}})),
        )
        .await;
    until(
        "the ghost's to-device message reached the bridge",
        || async { !bridge.to_device().is_empty() },
    )
    .await;
    let to_device = bridge.to_device();
    assert_eq!(
        to_device[0],
        json!({
            "type": "m.room_key",
            "sender": alice_id,
            "content": {"algorithm": "m.megolm.v1.aes-sha2", "session_key": "s"},
            "to_user_id": ghost_id,
            "to_device_id": ghost_device,
        }),
        "{to_device:?}"
    );
    let carrying = bridge
        .transactions()
        .into_iter()
        .find(|t| !t["to_device"].as_array().is_none_or(Vec::is_empty))
        .unwrap();
    assert_eq!(
        carrying["de.sorunome.msc2409.to_device"], carrying["to_device"],
        "both spellings: {carrying}"
    );
    assert!(
        to_device.iter().all(|m| m["to_user_id"] == ghost_id),
        "bob's message is not the bridge's: {to_device:?}"
    );

    // Alice uploads device keys: she shares a room with the ghost, so the bridge is told her
    // device list changed. The bot uploads one-time keys: every transaction from then on says
    // how many it has left.
    let alice_device = alice
        .matrix(reqwest::Method::GET, "/account/whoami", None)
        .await["device_id"]
        .as_str()
        .unwrap()
        .to_owned();
    alice
        .matrix(
            reqwest::Method::POST,
            "/keys/upload",
            Some(json!({"device_keys": device_keys(alice_id, &alice_device)})),
        )
        .await;
    until("alice's device-list change reached the bridge", || async {
        bridge.device_list_changes().iter().any(|u| u == alice_id)
    })
    .await;
    let bot_id = "@bridgebot:example.org";
    bot_own
        .matrix(
            reqwest::Method::POST,
            "/keys/upload",
            Some(json!({
                "device_keys": device_keys(bot_id, &bot_device),
                "one_time_keys": {
                    "signed_curve25519:AAAAAQ": {"key": "k1", "signatures": {}},
                    "signed_curve25519:AAAAAg": {"key": "k2", "signatures": {}},
                },
            })),
        )
        .await;
    alice
        .matrix(
            reqwest::Method::PUT,
            &format!("/rooms/{}/typing/{}", escape(&room_id), escape(alice_id)),
            Some(json!({"typing": false})),
        )
        .await;
    until(
        "a transaction carried the bot's one-time-key count",
        || async {
            bridge.transactions().iter().any(|t| {
                t["device_one_time_keys_count"][bot_id][&bot_device]["signed_curve25519"] == 2
            })
        },
    )
    .await;
    let counted = bridge
        .transactions()
        .into_iter()
        .find(|t| t["device_one_time_keys_count"][bot_id][&bot_device]["signed_curve25519"] == 2)
        .unwrap();
    assert_eq!(
        counted["org.matrix.msc3202.device_one_time_keys_count"],
        counted["device_one_time_keys_count"],
        "{counted}"
    );
    assert_eq!(
        counted["org.matrix.msc3202.device_one_time_key_counts"],
        counted["device_one_time_keys_count"],
        "{counted}"
    );

    // The bridge's own metrics and log line.
    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for kind in [
        "events",
        "typing",
        "receipts",
        "presence",
        "to_device",
        "device_list_changes",
        "one_time_key_counts",
    ] {
        assert!(
            metrics.contains(&format!(
                "hs_appservice_delivered_items_total{{appservice=\"ephemeral-bridge\",kind=\"{kind}\"}}"
            )),
            "{kind} is counted:\n{metrics}"
        );
    }
    assert!(
        metrics.contains(
            "hs_appservice_transactions_total{appservice=\"ephemeral-bridge\",outcome=\"delivered\"}"
        ),
        "{metrics}"
    );
    let log = server.log();
    assert!(
        log.contains("delivered a transaction to an appservice") && log.contains("to_device=1"),
        "the delivery log line says what was carried:\n{log}"
    );

    // A restart over the same data directory: nothing is sent twice, and what happens next is
    // sent once.
    let before = bridge.tally();
    let first_log = server.stop();
    assert!(
        first_log.contains("delivered a transaction to an appservice"),
        "{first_log}"
    );
    let mut server = HsProcess::serve(&config_path);
    server.wait_for("listening");
    // Long enough for a pump that resends to have done so: the poll is every 250 ms.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        bridge.tally(),
        before,
        "the restarted server resent nothing: {:?}",
        bridge.transactions()
    );
    let second = alice.say(&room_id, "after the restart").await;
    alice
        .matrix(
            reqwest::Method::POST,
            &format!(
                "/rooms/{}/receipt/m.read/{}",
                escape(&room_id),
                escape(&second)
            ),
            Some(json!({})),
        )
        .await;
    until(
        "the receipt after the restart reached the bridge",
        || async {
            bridge
                .ephemeral("m.receipt")
                .iter()
                .any(|r| r["content"][&second].is_object())
        },
    )
    .await;
    assert_eq!(
        bridge
            .ephemeral("m.receipt")
            .iter()
            .filter(|r| r["content"][&second].is_object())
            .count(),
        1
    );

    // Paused: held; resumed: delivered, once.
    admin
        .admin(
            reqwest::Method::POST,
            "/appservices/ephemeral-bridge/pause",
            Some(json!({})),
        )
        .await;
    let while_paused = bridge.tally();
    alice
        .matrix(
            reqwest::Method::PUT,
            &format!("/presence/{}/status", escape(alice_id)),
            Some(json!({"presence": "online", "status_msg": "paused"})),
        )
        .await;
    alice
        .matrix(
            reqwest::Method::PUT,
            &format!("/sendToDevice/m.room_key/td{}", nanos()),
            Some(json!({"messages": {ghost_id: {&ghost_device: {"while": "paused"}}}})),
        )
        .await;
    until("the paused appservice's backlog holds it", || async {
        let backlog = admin
            .admin(
                reqwest::Method::GET,
                "/appservices/ephemeral-bridge/backlog",
                None,
            )
            .await;
        !backlog["items"].as_array().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(bridge.tally(), while_paused, "held while paused");
    admin
        .admin(
            reqwest::Method::POST,
            "/appservices/ephemeral-bridge/resume",
            Some(json!({})),
        )
        .await;
    until("what was held is delivered on resume", || async {
        bridge
            .to_device()
            .iter()
            .any(|m| m["content"]["while"] == "paused")
    })
    .await;
    assert_eq!(
        bridge
            .to_device()
            .iter()
            .filter(|m| m["content"]["while"] == "paused")
            .count(),
        1
    );
    assert!(
        bridge
            .ephemeral("m.presence")
            .iter()
            .any(|p| p["content"]["status_msg"] == "paused")
    );
    // The bridge's health is what the admin API says it is: healthy, with an ephemeral-only
    // transaction as its last success.
    let health = admin
        .admin(
            reqwest::Method::GET,
            "/appservices/ephemeral-bridge/health",
            None,
        )
        .await;
    assert_eq!(health["status"], "healthy", "{health}");
    let _ = server.stop();
}
