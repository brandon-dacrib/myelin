//! What a bridge's namespaces and protocols mean to the rest of the server, against the real
//! binary: Sytest's `tests/60app-services/` (01as-create, 03passive, 05lookup3pe,
//! 06publicroomlist, 07deactivate) as one story.
//!
//! - A person cannot register a user ID, or create an alias, in the bridge's exclusive
//!   namespaces (`400 M_EXCLUSIVE`).
//! - A local alias nobody has made, in the bridge's alias namespace, is asked of the bridge
//!   (`GET /_matrix/app/v1/rooms/{alias}`), which makes it; and the room it names is then the
//!   bridge's to hear (the room's alias is in its namespace).
//! - Inviting a user of the bridge's who has no account asks the bridge
//!   (`GET /_matrix/app/v1/users/{userId}`), which registers them, before it hears of the invite.
//! - `/thirdparty/protocols` and `/thirdparty/protocol/{p}` are the bridge's answers, with each
//!   network's `instance_id`; a lookup is passed to it with the client's fields.
//! - The bridge publishes a room in its network's directory; `/publicRooms` lists it for that
//!   network and with `include_all_networks`, never in the server's own list.
//! - The bridge deactivates one of its users without being asked for a password.
//!
//! One real `hs serve` process from a configuration file that imports the registration; the
//! bridge is an axum listener in this process (as in `appservice_ephemeral.rs`). Every check
//! waits for a condition, never a duration.

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

const AS_TOKEN: &str = "as_token_for_the_query_bridge_test_00000000000000000000000000000";
const HS_TOKEN: &str = "hs_token_for_the_query_bridge_test_00000000000000000000000000000";

/// Sytest's first appservice, give or take the server name: exclusive `@astest-` users and
/// `#astest-` aliases, protocol `ymca`.
fn registration_yaml(url: &str) -> String {
    format!(
        "id: ymca-bridge\nurl: {url}\nas_token: {AS_TOKEN}\nhs_token: {HS_TOKEN}\n\
         sender_localpart: ymcabot\nrate_limited: false\nprotocols: [ymca]\n\
         namespaces:\n  users:\n    - regex: '@astest-.*:example\\.org'\n      exclusive: true\n\
         \x20 aliases:\n    - regex: '#astest-.*:example\\.org'\n      exclusive: true\n"
    )
}

// ---------------------------------------------------------------------------------------------
// The bridge.
// ---------------------------------------------------------------------------------------------

/// What the bridge was asked and sent, and what it needs to answer: the server's address, and
/// the room the alias it is asked about should name.
#[derive(Clone, Default)]
struct Bridge {
    transactions: Arc<Mutex<Vec<Value>>>,
    asked: Arc<Mutex<Vec<String>>>,
    homeserver: Arc<Mutex<Option<String>>>,
    alias_room: Arc<Mutex<Option<String>>>,
}

impl Bridge {
    fn events(&self) -> Vec<Value> {
        self.transactions
            .lock()
            .unwrap()
            .iter()
            .flat_map(|txn| txn["events"].as_array().cloned().unwrap_or_default())
            .collect()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }

    fn homeserver(&self) -> String {
        self.homeserver.lock().unwrap().clone().unwrap()
    }
}

fn authorized(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {HS_TOKEN}"))
}

type Answer = (axum::http::StatusCode, axum::Json<Value>);

fn not_found() -> Answer {
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(json!({"errcode": "M_NOT_FOUND"})),
    )
}

fn ok(body: Value) -> Answer {
    (axum::http::StatusCode::OK, axum::Json(body))
}

async fn start_bridge() -> (String, Bridge) {
    use axum::extract::{Path, RawQuery, State};
    use axum::http::HeaderMap;

    async fn transaction(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> Answer {
        if !authorized(&headers) {
            return not_found();
        }
        bridge.transactions.lock().unwrap().push(body);
        ok(json!({}))
    }

    /// Provides `@astest-invited`, registering them first, as a bridge does.
    async fn user(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        Path(user): Path<String>,
    ) -> Answer {
        assert!(authorized(&headers));
        bridge.asked.lock().unwrap().push(format!("user {user}"));
        if user != "@astest-invited:example.org" {
            return not_found();
        }
        let registered = reqwest::Client::new()
            .post(format!(
                "{}/_matrix/client/v3/register",
                bridge.homeserver()
            ))
            .bearer_auth(AS_TOKEN)
            .json(&json!({"type": "m.login.application_service", "username": "astest-invited"}))
            .send()
            .await
            .unwrap();
        assert!(
            registered.status().is_success(),
            "{:?}",
            registered.text().await
        );
        ok(json!({}))
    }

    /// Provides `#astest-bridged`, pointing it at the room the test chose.
    async fn room(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        Path(alias): Path<String>,
    ) -> Answer {
        assert!(authorized(&headers));
        bridge.asked.lock().unwrap().push(format!("room {alias}"));
        if alias != "#astest-bridged:example.org" {
            return not_found();
        }
        let room_id = bridge.alias_room.lock().unwrap().clone().unwrap();
        let made = reqwest::Client::new()
            .put(format!(
                "{}/_matrix/client/v3/directory/room/%23astest-bridged%3Aexample.org",
                bridge.homeserver()
            ))
            .bearer_auth(AS_TOKEN)
            .json(&json!({"room_id": room_id}))
            .send()
            .await
            .unwrap();
        assert!(made.status().is_success(), "{:?}", made.text().await);
        ok(json!({}))
    }

    async fn protocol(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        Path(protocol): Path<String>,
    ) -> Answer {
        assert!(authorized(&headers));
        bridge
            .asked
            .lock()
            .unwrap()
            .push(format!("protocol {protocol}"));
        if protocol != "ymca" {
            return not_found();
        }
        ok(json!({
            "user_fields": ["nick"],
            "location_fields": ["channel"],
            "icon": "mxc://example.org/ymca",
            "field_types": {},
            "instances": [{"desc": "Libera", "network_id": "libera", "fields": {}}],
        }))
    }

    async fn thirdparty_user(
        State(bridge): State<Bridge>,
        headers: HeaderMap,
        Path(protocol): Path<String>,
        RawQuery(query): RawQuery,
    ) -> Answer {
        assert!(authorized(&headers));
        let query = query.unwrap_or_default();
        bridge
            .asked
            .lock()
            .unwrap()
            .push(format!("thirdparty user {protocol}?{query}"));
        ok(json!([{
            "protocol": protocol,
            "fields": {"nick": "alice"},
            "userid": "@astest-alice:example.org",
        }]))
    }

    let bridge = Bridge::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn}",
            axum::routing::put(transaction),
        )
        .route("/_matrix/app/v1/users/{user}", axum::routing::get(user))
        .route("/_matrix/app/v1/rooms/{alias}", axum::routing::get(room))
        .route(
            "/_matrix/app/v1/thirdparty/protocol/{protocol}",
            axum::routing::get(protocol),
        )
        .route(
            "/_matrix/app/v1/thirdparty/user/{protocol}",
            axum::routing::get(thirdparty_user),
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
}

impl Caller {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Value) {
        let mut request = self
            .client
            .request(method, format!("{}/_matrix/client/v3{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn ok(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, path, body).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    async fn public_room_ids(&self, body: Value) -> Vec<String> {
        let listed = self
            .ok(reqwest::Method::POST, "/publicRooms", Some(body))
            .await;
        listed["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .map(|room| room["room_id"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('|', "%7C")
}

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

async fn register(
    client: &reqwest::Client,
    base: &str,
    name: &str,
) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": name, "password": format!("hunter2-{name}-long"), "auth": {"type": "m.login.dummy"}}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

// ---------------------------------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_bridges_namespaces_protocols_and_directory_mean_what_sytest_says() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge_url, bridge) = start_bridge().await;
    let registration_path = dir.path().join("registration.yaml");
    std::fs::write(&registration_path, registration_yaml(&bridge_url)).unwrap();
    let port = reserve_port();
    let config_path = dir.path().join("hs.yaml");
    std::fs::write(
        &config_path,
        config_yaml(port, &dir.path().join("data"), &registration_path),
    )
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    *bridge.homeserver.lock().unwrap() = Some(base.clone());
    let client = reqwest::Client::new();
    let mut server = HsProcess::serve(&config_path);
    server.wait_for("setup_link=");

    // 01as-create: "Regular users cannot register within the AS namespace".
    let (status, body) = register(&client, &base, "astest-sneaky").await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["errcode"], "M_EXCLUSIVE", "{body}");
    let (status, body) = register(&client, &base, "alice").await;
    assert!(status.is_success(), "{body}");
    let alice = Caller {
        client: client.clone(),
        base: base.clone(),
        token: body["access_token"].as_str().unwrap().to_owned(),
    };
    let appservice = Caller {
        client: client.clone(),
        base: base.clone(),
        token: AS_TOKEN.to_owned(),
    };

    let room_id = alice
        .ok(
            reqwest::Method::POST,
            "/createRoom",
            Some(json!({"preset": "public_chat", "name": "Test Name"})),
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 01as-create: "Regular users cannot create room aliases within the AS namespace".
    let (status, body) = alice
        .call(
            reqwest::Method::PUT,
            "/directory/room/%23astest-mine%3Aexample.org",
            Some(json!({"room_id": room_id})),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["errcode"], "M_EXCLUSIVE", "{body}");

    // 03passive: "Accesing an AS-hosted room alias asks the AS server".
    *bridge.alias_room.lock().unwrap() = Some(room_id.clone());
    let resolved = alice
        .ok(
            reqwest::Method::GET,
            "/directory/room/%23astest-bridged%3Aexample.org",
            None,
        )
        .await;
    assert_eq!(resolved["room_id"], room_id.as_str(), "{resolved}");
    let (status, _) = alice
        .call(
            reqwest::Method::GET,
            "/directory/room/%23astest-nothing%3Aexample.org",
            None,
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    // 03passive: "Inviting an AS-hosted user asks the AS server", which registers them before
    // it hears of the invite.
    alice
        .ok(
            reqwest::Method::POST,
            &format!("/rooms/{}/invite", escape(&room_id)),
            Some(json!({"user_id": "@astest-invited:example.org"})),
        )
        .await;
    until("the bridge hears of the invite", || async {
        bridge.events().iter().any(|e| {
            e["type"] == "m.room.member"
                && e["state_key"] == "@astest-invited:example.org"
                && e["content"]["membership"] == "invite"
        })
    })
    .await;
    let asked = bridge.asked();
    let asked_user = asked
        .iter()
        .position(|a| a == "user @astest-invited:example.org")
        .unwrap_or_else(|| panic!("the bridge was not asked about the invited user: {asked:?}"));
    assert!(
        asked.contains(&"room #astest-bridged:example.org".to_owned()),
        "{asked:?}"
    );
    assert!(
        asked.contains(&"room #astest-nothing:example.org".to_owned()),
        "{asked:?}"
    );
    assert_eq!(
        asked
            .iter()
            .filter(|a| a.starts_with("user @astest-invited"))
            .count(),
        1,
        "asked once: {asked:?} (from position {asked_user})"
    );

    // 03passive: "Events in rooms with AS-hosted room aliases are sent to AS server": nobody in
    // the room is the bridge's (its invitee has not joined), but the room's alias is.
    alice
        .ok(
            reqwest::Method::PUT,
            &format!("/rooms/{}/send/m.room.message/t1", escape(&room_id)),
            Some(json!({"msgtype": "m.text", "body": "A message for the AS"})),
        )
        .await;
    until("the bridge hears the message", || async {
        bridge
            .events()
            .iter()
            .any(|e| e["content"]["body"] == "A message for the AS")
    })
    .await;

    // 05lookup3pe: protocol metadata, and a lookup passed through.
    let protocols = alice
        .ok(reqwest::Method::GET, "/thirdparty/protocols", None)
        .await;
    assert_eq!(
        protocols["ymca"]["user_fields"],
        json!(["nick"]),
        "{protocols}"
    );
    let instance = protocols["ymca"]["instances"][0]["instance_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{protocols}"))
        .to_owned();
    assert_eq!(instance, "ymca-bridge|libera");
    let one = alice
        .ok(reqwest::Method::GET, "/thirdparty/protocol/ymca", None)
        .await;
    assert_eq!(one, protocols["ymca"]);
    let (status, _) = alice
        .call(reqwest::Method::GET, "/thirdparty/protocol/gopher", None)
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    let users = alice
        .ok(
            reqwest::Method::GET,
            "/thirdparty/user/ymca?nick=alice",
            None,
        )
        .await;
    assert_eq!(users[0]["userid"], "@astest-alice:example.org", "{users}");
    assert!(
        bridge
            .asked()
            .contains(&"thirdparty user ymca?nick=alice".to_owned()),
        "the access token is not passed on: {:?}",
        bridge.asked()
    );

    // 06publicroomlist: the bridge's network directory is apart from the server's.
    let network = format!("/directory/list/appservice/libera/{}", escape(&room_id));
    let (status, body) = alice
        .call(
            reqwest::Method::PUT,
            &network,
            Some(json!({"visibility": "public"})),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "a person: {body}");
    appservice
        .ok(
            reqwest::Method::PUT,
            &network,
            Some(json!({"visibility": "public"})),
        )
        .await;
    assert!(!alice.public_room_ids(json!({})).await.contains(&room_id));
    assert!(
        alice
            .public_room_ids(json!({"third_party_instance_id": instance}))
            .await
            .contains(&room_id)
    );
    assert!(
        alice
            .public_room_ids(json!({"include_all_networks": true}))
            .await
            .contains(&room_id)
    );
    alice
        .ok(
            reqwest::Method::PUT,
            &format!("/directory/list/room/{}", escape(&room_id)),
            Some(json!({"visibility": "public"})),
        )
        .await;
    appservice.ok(reqwest::Method::DELETE, &network, None).await;
    assert!(
        !alice
            .public_room_ids(json!({"third_party_instance_id": instance}))
            .await
            .contains(&room_id)
    );
    assert!(alice.public_room_ids(json!({})).await.contains(&room_id));

    // 07deactivate: "AS can deactivate a user".
    appservice
        .ok(
            reqwest::Method::POST,
            "/account/deactivate?user_id=%40astest-invited%3Aexample.org",
            Some(json!({})),
        )
        .await;

    // Observable: the questions are counted, and the refusals and answers logged.
    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for series in [
        "hs_appservice_queries_total{appservice=\"ymca-bridge\",kind=\"user\",outcome=\"yes\"} 1",
        "hs_appservice_queries_total{appservice=\"ymca-bridge\",kind=\"room_alias\",outcome=\"yes\"} 1",
        "hs_appservice_queries_total{appservice=\"ymca-bridge\",kind=\"room_alias\",outcome=\"no\"} 1",
        "hs_appservice_queries_total{appservice=\"ymca-bridge\",kind=\"protocol\",outcome=\"cached\"}",
    ] {
        assert!(
            metrics.contains(series),
            "{series} missing from:\n{metrics}"
        );
    }
    let log = server.log();
    for line in [
        "refused a registration in an appservice's exclusive namespace",
        "refused an alias in an appservice's exclusive namespace",
        "an appservice provided a room alias the homeserver asked about",
        "an appservice provided a user the homeserver asked about",
        "an appservice changed its room directory",
        "an appservice deactivated one of its users",
    ] {
        assert!(log.contains(line), "the log never said {line:?}");
    }
}
