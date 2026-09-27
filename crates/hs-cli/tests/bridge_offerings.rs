//! RFC 0017 against the real binary: an administrator offers a bridge, the manager takes an
//! instance from `requested` to `ready`, a person gets one by messaging its front door, and the
//! manager bot answers commands. Everything goes over the bound socket: the admin API as the
//! interface uses it, the client API as a Matrix client uses it, and the appservice API the
//! server itself delivers to. The bridge is stood in for by an axum listener that answers
//! `/_matrix/app/v1/ping`, which is exactly what the manager waits for; the shared heisenbridge
//! offering is run for real when `heisenbridge` is installed (`pip install heisenbridge`).
//!
//! No Kubernetes here, so every offering runs `elsewhere`: the instance's files are what an
//! administrator would download, and the stand-in is registered at the instance's `url` by
//! patching its registration, as an administrator does for a bridge that runs somewhere the
//! server could not have guessed.

use std::sync::Arc;
use std::sync::Mutex;

use serde_json::{Value, json};

fn test_config(data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  enabled: false\n",
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
}

/// A signed-in party: an administrator on `/api/v1`, or a user on the client API.
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
            .request(method, format!("{}{path}", self.base))
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

    /// `path` under `/api/v1`, expecting success.
    async fn admin(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, &format!("/api/v1{path}"), body).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    /// `path` under `/api/v1`, expecting `expected`.
    async fn admin_expect(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        expected: reqwest::StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, &format!("/api/v1{path}"), body).await;
        assert_eq!(status, expected, "{path}: {body}");
        body
    }

    /// `path` under `/_matrix/client/v3`, expecting success.
    async fn matrix(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self
            .call(method, &format!("/_matrix/client/v3{path}"), body)
            .await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    async fn say(&self, room_id: &str, text: &str) {
        let txn = format!("t{}", rand_suffix());
        self.matrix(
            reqwest::Method::PUT,
            &format!("/rooms/{}/send/m.room.message/{txn}", escape(room_id)),
            Some(json!({"msgtype": "m.text", "body": text})),
        )
        .await;
    }

    /// `GET /sync` until `wanted` is true of the response, or panics with the last one. A
    /// condition, not a duration: an incremental sync long-polls with its token carried
    /// forward; an initial one answers at once, so it is asked again after a moment.
    async fn sync_until(
        &self,
        mut since: Option<String>,
        wanted: impl Fn(&Value) -> bool,
    ) -> Value {
        let mut last = Value::Null;
        for _ in 0..240 {
            let mut path = "/sync?timeout=1000".to_owned();
            if let Some(since) = &since {
                path.push_str(&format!("&since={since}"));
            }
            let response = self.matrix(reqwest::Method::GET, &path, None).await;
            if wanted(&response) {
                return response;
            }
            if since.is_some() {
                since = response["next_batch"].as_str().map(str::to_owned);
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            last = response;
        }
        panic!("the sync never said what was expected; the last one said: {last}");
    }
}

fn rand_suffix() -> String {
    format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
}

/// `GET path` until its `state` is `state`, returning it; a condition, not a duration.
async fn until_state(admin: &Caller, path: &str, state: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..240 {
        last = admin.admin(GET, path, None).await;
        if last["state"] == state {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("{path} never reached {state}; the last look said: {last}");
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
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("never happened: {what}");
}

/// With `RUST_LOG` set, the server's own log goes to the test's output (`--nocapture`): the
/// manager says what it could not do at `warn`, and at `hs_bridges=debug` what it is doing.
fn log_if_asked() -> Option<hs_telemetry::Guard> {
    std::env::var_os("RUST_LOG")?;
    hs_telemetry::init(&hs_telemetry::Options::default()).ok()
}

/// The server, its first administrator, and users registered through the client API.
async fn boot(
    dir: &std::path::Path,
    users: &[&str],
) -> (hs_cli::serve::ServeHandle, Caller, Vec<Caller>) {
    let _log = log_if_asked();
    let handle =
        hs_cli::serve::spawn_serve(test_config(dir), hs_cli::serve::ServeOptions::default())
            .await
            .expect("server should boot");
    let base = handle.base_url();
    let client = reqwest::Client::new();
    let link = handle
        .setup_link
        .clone()
        .expect("a fresh server offers setup");
    let token = link.split_once("#token=").unwrap().1.to_owned();
    let session: Value = client
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({"setup_token": token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin = Caller {
        client: client.clone(),
        base: base.clone(),
        token: session["access_token"].as_str().unwrap().to_owned(),
    };
    let mut callers = Vec::new();
    for name in users {
        let registered: Value = client
            .post(format!("{base}/_matrix/client/v3/register"))
            .json(&json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        callers.push(Caller {
            client: client.clone(),
            base: base.clone(),
            token: registered["access_token"]
                .as_str()
                .unwrap_or_else(|| panic!("{name}: {registered}"))
                .to_owned(),
        });
    }
    (handle, admin, callers)
}

/// A bridge, as far as the server can tell: it answers a ping carrying its `hs_token`, and
/// takes transactions. Returns where it listens and what it was sent.
#[derive(Clone, Default)]
struct StandIn {
    hs_token: Arc<Mutex<String>>,
    transactions: Arc<Mutex<Vec<Value>>>,
}

async fn stand_in(hs_token: &str) -> (String, StandIn) {
    async fn ping(
        axum::extract::State(bridge): axum::extract::State<StandIn>,
        headers: axum::http::HeaderMap,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if auth == format!("Bearer {}", bridge.hs_token.lock().unwrap()) {
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
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        bridge.transactions.lock().unwrap().push(body);
        axum::Json(json!({}))
    }
    let bridge = StandIn {
        hs_token: Arc::new(Mutex::new(hs_token.to_owned())),
        transactions: Arc::default(),
    };
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

/// Every account on the server, with the appservice each belongs to: the witness for which
/// accounts an offering makes. (The overview's `users_count` is the same number, cached for a
/// minute, so it cannot be read before and after within one test.)
async fn accounts(admin: &Caller) -> Vec<(String, Option<String>)> {
    let listed = admin.admin(GET, "/users?limit=100", None).await;
    listed["items"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"))
        .iter()
        .map(|u| {
            (
                u["user_id"].as_str().unwrap().to_owned(),
                u["appservice_id"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn user_namespaces(appservice: &Value) -> Vec<String> {
    appservice["namespaces"]["users"]
        .as_array()
        .unwrap_or_else(|| panic!("{appservice}"))
        .iter()
        .map(|n| n["regex"].as_str().unwrap().to_owned())
        .collect()
}

fn notices_from<'a>(timeline: &'a Value, sender: &str) -> Vec<&'a str> {
    timeline["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter(|e| e["type"] == "m.room.message" && e["sender"] == sender)
                .filter_map(|e| e["content"]["body"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

/// A running client: one incremental sync after another, each waiting for what comes next.
/// Like a real client it remembers the event ids it has shown, so an event the server repeats
/// across two batches (seen here: a bot's reply that arrived while the batch carrying the
/// message it answers was being assembled) is not taken for a new one.
struct Watch {
    caller: Caller,
    since: Option<String>,
    seen: std::collections::HashSet<String>,
}

impl Watch {
    async fn start(caller: &Caller) -> Self {
        let first = caller.matrix(GET, "/sync?timeout=0", None).await;
        Self {
            caller: caller.clone(),
            since: first["next_batch"].as_str().map(str::to_owned),
            seen: std::collections::HashSet::new(),
        }
    }

    /// The next batch for which `wanted` holds; batches before it are passed over.
    async fn next(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        let batch = self.caller.sync_until(self.since.clone(), wanted).await;
        self.since = batch["next_batch"].as_str().map(str::to_owned);
        batch
    }

    /// The next thing `bot` says in `room` that this client has not shown before.
    async fn next_notice(&mut self, room: &str, bot: &str) -> String {
        let seen = self.seen.clone();
        let fresh = |timeline: &Value| -> Vec<(String, String)> {
            timeline["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter(|e| e["type"] == "m.room.message" && e["sender"] == bot)
                        .filter(|e| !seen.contains(e["event_id"].as_str().unwrap_or_default()))
                        .filter_map(|e| {
                            Some((
                                e["event_id"].as_str()?.to_owned(),
                                e["content"]["body"].as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let batch = self
            .next(|s| !fresh(&s["rooms"]["join"][room]["timeline"]).is_empty())
            .await;
        for (_, joined) in batch["rooms"]["join"].as_object().into_iter().flatten() {
            for event in joined["timeline"]["events"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(id) = event["event_id"].as_str() {
                    self.seen.insert(id.to_owned());
                }
            }
        }
        fresh(&batch["rooms"]["join"][room]["timeline"])
            .first()
            .map(|(_, body)| body.clone())
            .unwrap_or_default()
    }
}

/// Everything `bot` has said in `room`, oldest first, from the room's history rather than a
/// sync's window.
async fn room_notices(caller: &Caller, room: &str, bot: &str) -> Vec<String> {
    let page = caller
        .matrix(
            GET,
            &format!("/rooms/{}/messages?dir=b&limit=100", escape(room)),
            None,
        )
        .await;
    let mut out: Vec<String> = page["chunk"]
        .as_array()
        .unwrap_or_else(|| panic!("{page}"))
        .iter()
        .filter(|e| e["type"] == "m.room.message" && e["sender"] == bot)
        .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
        .collect();
    out.reverse();
    out
}

const GET: reqwest::Method = reqwest::Method::GET;
const PUT: reqwest::Method = reqwest::Method::PUT;
const POST: reqwest::Method = reqwest::Method::POST;
const PATCH: reqwest::Method = reqwest::Method::PATCH;
const DELETE: reqwest::Method = reqwest::Method::DELETE;

#[tokio::test]
async fn an_offering_takes_an_instance_from_requested_to_ready_and_removes_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), &["alice"]).await;
    let alice = &users[0];
    let alice_id = "@alice:example.org";

    // This server has no cluster: it says so, and an offering cannot ask for one.
    let target = admin.admin(GET, "/bridge-deployment-target", None).await;
    assert_eq!(target["available"], false, "{target}");
    assert!(
        target["reason"]
            .as_str()
            .is_some_and(|r| r.contains("Kubernetes")),
        "{target}"
    );
    let refused = admin
        .admin_expect(
            PUT,
            "/bridge-offerings/mautrix-whatsapp",
            Some(json!({"runtime": "cluster"})),
            reqwest::StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(refused["errors"][0]["pointer"], "/runtime", "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .is_some_and(|d| d.contains("cannot deploy")),
        "{refused}"
    );
    let offerings = admin.admin(GET, "/bridge-offerings", None).await;
    assert_eq!(offerings["items"], json!([]), "{offerings}");

    // Before the first offering: the manager's registration reserves its namespace, and no bot
    // account exists (the administrator and alice, nobody else).
    assert_eq!(
        accounts(&admin).await,
        vec![
            ("@alice:example.org".to_owned(), None),
            ("@ops:example.org".to_owned(), None)
        ]
    );
    let me = admin.admin(GET, "/appservices/myelin-bridges", None).await;
    assert_eq!(me["sender_localpart"], "bridges");
    assert_eq!(user_namespaces(&me), vec!["@bridges:example\\.org"]);
    assert!(
        me["url"]
            .as_str()
            .is_some_and(|u| u.ends_with("/_myelin/bridges")),
        "{me}"
    );

    // The offering: WhatsApp, run elsewhere, for everyone here.
    let offering = admin
        .admin(
            PUT,
            "/bridge-offerings/mautrix-whatsapp",
            Some(json!({"runtime": "elsewhere", "access": {"all_local_users": true}})),
        )
        .await;
    assert_eq!(offering["type"], "mautrix-whatsapp");
    assert_eq!(offering["mode"], "per_user");
    assert_eq!(offering["runtime"], "elsewhere");
    assert_eq!(offering["front_door"], "@whatsappbot:example.org");
    assert_eq!(offering["image"], "dock.mau.dev/mautrix/whatsapp:latest");
    assert_eq!(offering["image_tag"], "latest");
    assert_eq!(offering["instances"], json!({}));
    let offerings = admin.admin(GET, "/bridge-offerings", None).await;
    assert_eq!(
        offerings["items"][0]["type"], "mautrix-whatsapp",
        "{offerings}"
    );
    let me = admin.admin(GET, "/appservices/myelin-bridges", None).await;
    assert_eq!(
        user_namespaces(&me),
        vec!["@bridges:example\\.org", "@whatsappbot:example\\.org"]
    );
    // Now the bots exist, and not before: `@bridges` and `@whatsappbot`, the manager's.
    assert_eq!(
        accounts(&admin).await,
        vec![
            ("@alice:example.org".to_owned(), None),
            (
                "@bridges:example.org".to_owned(),
                Some("myelin-bridges".to_owned())
            ),
            ("@ops:example.org".to_owned(), None),
            (
                "@whatsappbot:example.org".to_owned(),
                Some("myelin-bridges".to_owned())
            ),
        ]
    );

    // An instance for alice. Only a local user of a per-user type can have one.
    for (user, pointer) in [("@alice:elsewhere.net", "not a user"), ("_", "per user")] {
        let refused = admin
            .admin_expect(
                PUT,
                &format!(
                    "/bridge-offerings/mautrix-whatsapp/instances/{}",
                    escape(user)
                ),
                None,
                reqwest::StatusCode::BAD_REQUEST,
            )
            .await;
        assert!(
            refused["detail"]
                .as_str()
                .is_some_and(|d| d.contains(pointer)),
            "{user}: {refused}"
        );
    }
    let path = format!(
        "/bridge-offerings/mautrix-whatsapp/instances/{}",
        escape(alice_id)
    );
    let instance = admin.admin(PUT, &path, None).await;
    assert_eq!(instance["user_id"], alice_id);
    assert!(
        ["requested", "registered", "starting"].contains(&instance["state"].as_str().unwrap()),
        "{instance}"
    );
    // Registered (its own appservice, tagged as alice's instance of the offering) and then,
    // with nothing here to run it, waiting for someone to.
    let seen = until_state(&admin, &path, "starting").await;
    assert_eq!(seen["appservice_id"], "whatsapp-alice", "{seen}");
    assert_eq!(seen["bot"], "@whatsappbot_alice:example.org");
    assert!(
        seen["reason"].as_str().is_some_and(|r| r.contains("files")),
        "{seen}"
    );
    assert!(seen["deployment"].is_null());
    let listed = admin.admin(GET, "/appservices", None).await;
    let mine = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "whatsapp-alice")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(mine["bridge_type"], "mautrix-whatsapp", "{mine}");
    assert_eq!(mine["sender_localpart"], "whatsappbot_alice");
    assert_eq!(
        user_namespaces(mine),
        vec![
            "@whatsapp_alice_.*:example\\.org",
            "@whatsappbot_alice:example\\.org",
            "@alice:example\\.org"
        ]
    );
    let registration = admin
        .admin(GET, "/appservices/whatsapp-alice/registration", None)
        .await;
    assert_eq!(
        registration["io.myelin.bridge_instance"], alice_id,
        "{registration}"
    );
    assert_eq!(registration["io.myelin.bridge_type"], "mautrix-whatsapp");
    let hs_token = registration["hs_token"].as_str().unwrap().to_owned();
    let as_token = registration["as_token"].as_str().unwrap().to_owned();
    let counted = admin
        .admin(GET, "/bridge-offerings/mautrix-whatsapp", None)
        .await;
    assert_eq!(counted["instances"], json!({"starting": 1}), "{counted}");
    let instances = admin
        .admin(GET, "/bridge-offerings/mautrix-whatsapp/instances", None)
        .await;
    assert_eq!(instances["items"][0]["user_id"], alice_id, "{instances}");

    // The files an administrator downloads to run it: the instance's own tokens, and this
    // server's address for a bridge outside it (the bound one, since no public base URL is
    // configured).
    let files = admin.admin(POST, &format!("{path}/files"), None).await;
    let config = files["config_yaml"].as_str().unwrap();
    assert!(config.contains(&as_token), "{config}");
    assert!(config.contains(&hs_token));
    assert!(
        config.contains(&format!("address: {}", handle.base_url())),
        "{config}"
    );
    assert!(config.contains("\"@alice:example.org\": admin"), "{config}");
    let registration_yaml = files["registration_yaml"].as_str().unwrap();
    assert!(registration_yaml.contains("id: whatsapp-alice"));
    assert!(registration_yaml.contains(&format!("hs_token: {hs_token}")));
    assert!(
        files["compose_yaml"]
            .as_str()
            .unwrap()
            .contains("dock.mau.dev/mautrix/whatsapp:latest")
    );
    assert!(
        files["manifest_yaml"]
            .as_str()
            .unwrap()
            .contains("kind: Bridge")
    );

    // The bridge comes up, somewhere: an administrator tells the registry where, and the
    // manager's next ping finds it. Ready, and alice hears from its bot.
    let (url, bridge) = stand_in(&hs_token).await;
    let patched = admin
        .admin(
            PATCH,
            "/appservices/whatsapp-alice",
            Some(json!({"url": url})),
        )
        .await;
    assert_eq!(patched["url"], url, "{patched}");
    let seen = until_state(&admin, &path, "ready").await;
    assert!(seen["ready_at"].is_string(), "{seen}");
    assert_eq!(seen["health"], "healthy", "{seen}");
    assert!(seen["last_ping_at"].is_string());
    assert!(seen["last_error"].is_null());
    assert!(seen["reason"].is_null());

    let bot = "@whatsappbot_alice:example.org";
    let synced = alice
        .sync_until(None, |s| {
            s["rooms"]["invite"]
                .as_object()
                .is_some_and(|rooms| !rooms.is_empty())
        })
        .await;
    let (dm, invite) = synced["rooms"]["invite"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let member = invite["invite_state"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "m.room.member" && e["state_key"] == alice_id)
        .unwrap_or_else(|| panic!("{invite}"));
    assert_eq!(member["sender"], bot, "{member}");
    assert_eq!(member["content"]["is_direct"], true, "{member}");
    alice
        .matrix(POST, &format!("/join/{}", escape(dm)), Some(json!({})))
        .await;
    let synced = alice
        .sync_until(None, |s| {
            !notices_from(&s["rooms"]["join"][dm]["timeline"], bot).is_empty()
        })
        .await;
    let said = notices_from(&synced["rooms"]["join"][dm]["timeline"], bot).join("\n");
    assert!(said.contains("This is your own WhatsApp bridge"), "{said}");
    assert!(said.contains("login qr"), "{said}");
    // Double puppeting lets the instance act as alice, and it used that to mark the chat as
    // direct on her side too, without dropping anything else she had there.
    let direct = alice
        .matrix(
            GET,
            &format!("/user/{}/account_data/m.direct", escape(alice_id)),
            None,
        )
        .await;
    assert_eq!(direct[bot], json!([dm]), "{direct}");
    // And the bridge itself was told what happened in that room: the events reached its
    // transactions endpoint, carrying the instance's own hs_token.
    until("the bridge is sent the room", || async {
        bridge
            .transactions
            .lock()
            .unwrap()
            .iter()
            .flat_map(|t| t["events"].as_array().cloned().unwrap_or_default())
            .any(|e| e["type"] == "m.room.member" && e["state_key"] == alice_id)
    })
    .await;
    // Ready is where it rests: two more ticks change nothing.
    let now = admin.admin(GET, &path, None).await;
    assert_eq!(now["state"], "ready");
    assert_eq!(now["ready_at"], seen["ready_at"]);

    // Removed: the registration goes with it; the account it made stays an account, since a
    // server keeps its accounts, but nobody can act as it any more.
    admin
        .admin_expect(DELETE, &path, None, reqwest::StatusCode::NO_CONTENT)
        .await;
    admin
        .admin_expect(GET, &path, None, reqwest::StatusCode::NOT_FOUND)
        .await;
    admin
        .admin_expect(
            GET,
            "/appservices/whatsapp-alice",
            None,
            reqwest::StatusCode::NOT_FOUND,
        )
        .await;
    let counted = admin
        .admin(GET, "/bridge-offerings/mautrix-whatsapp", None)
        .await;
    assert_eq!(counted["instances"], json!({}), "{counted}");
    let as_bridge = Caller {
        token: as_token.clone(),
        ..alice.clone()
    };
    let (status, body) = as_bridge
        .call(
            PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/after?user_id={}",
                escape(dm),
                escape(bot)
            ),
            Some(json!({"msgtype": "m.notice", "body": "still here?"})),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "the instance's tokens are dead with its registration: {body}"
    );

    // Stop offering: the front door leaves the namespace, and the offering is gone.
    admin
        .admin_expect(
            DELETE,
            "/bridge-offerings/mautrix-whatsapp",
            None,
            reqwest::StatusCode::NO_CONTENT,
        )
        .await;
    admin
        .admin_expect(
            GET,
            "/bridge-offerings/mautrix-whatsapp",
            None,
            reqwest::StatusCode::NOT_FOUND,
        )
        .await;
    let offerings = admin.admin(GET, "/bridge-offerings", None).await;
    assert_eq!(offerings["items"], json!([]), "{offerings}");
    let me = admin.admin(GET, "/appservices/myelin-bridges", None).await;
    assert_eq!(user_namespaces(&me), vec!["@bridges:example\\.org"]);
    // The instance's bot account remains, still attributed to the registration that made it
    // (the user directory records who made an account; the registration's absence is the
    // registry's to report), and the manager's two bots stay the manager's.
    let now: Vec<String> = accounts(&admin)
        .await
        .into_iter()
        .map(|(u, a)| format!("{u}={}", a.unwrap_or_default()))
        .collect();
    assert_eq!(
        now,
        vec![
            "@alice:example.org=",
            "@bridges:example.org=myelin-bridges",
            "@ops:example.org=",
            "@whatsappbot:example.org=myelin-bridges",
            "@whatsappbot_alice:example.org=whatsapp-alice",
        ]
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn a_person_gets_a_bridge_by_messaging_its_front_door_and_the_manager_bot_takes_commands() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), &["alice", "bob"]).await;
    let (alice, bob) = (&users[0], &users[1]);
    let alice_id = "@alice:example.org";
    let door = "@whatsappbot:example.org";
    let manager_bot = "@bridges:example.org";
    let instance_path = format!(
        "/bridge-offerings/mautrix-whatsapp/instances/{}",
        escape(alice_id)
    );
    // Their clients are running, as they would be: each sync from here on is incremental, and
    // waits for what comes next rather than re-reading a truncated timeline.
    let mut alice_watch = Watch::start(alice).await;
    let mut bob_watch = Watch::start(bob).await;

    // Offered to alice, and not to bob.
    admin
        .admin(
            PUT,
            "/bridge-offerings/mautrix-whatsapp",
            Some(json!({"runtime": "elsewhere", "access": {"all_local_users": false, "users": [alice_id]}})),
        )
        .await;

    // Alice invites the front door to a direct chat. The server delivers that to the manager's
    // own appservice endpoint over loopback; the bot joins and says what it is doing.
    let created = alice
        .matrix(
            POST,
            "/createRoom",
            Some(json!({"preset": "trusted_private_chat", "is_direct": true, "invite": [door]})),
        )
        .await;
    let front = created["room_id"].as_str().unwrap().to_owned();
    let said = alice_watch.next_notice(&front, door).await;
    assert!(
        said.contains("An administrator runs WhatsApp bridges on this server by hand"),
        "{said}"
    );
    assert!(said.contains("asked for one for you"), "{said}");
    let membership = alice
        .matrix(
            GET,
            &format!(
                "/rooms/{}/state/m.room.member/{}",
                escape(&front),
                escape(door)
            ),
            None,
        )
        .await;
    assert_eq!(membership["membership"], "join", "{membership}");
    let instance = admin.admin(GET, &instance_path, None).await;
    assert_eq!(instance["user_id"], alice_id, "{instance}");
    let me = admin
        .admin(GET, "/appservices/myelin-bridges/health", None)
        .await;
    assert_eq!(me["status"], "healthy", "delivered to: {me}");

    // Asking again while it is on its way says where it stands, and makes no second instance.
    alice.say(&front, "hello?").await;
    let said = alice_watch.next_notice(&front, door).await;
    assert!(said.contains("still being set up"), "{said}");
    let instances = admin
        .admin(GET, "/bridge-offerings/mautrix-whatsapp/instances", None)
        .await;
    assert_eq!(
        instances["items"].as_array().unwrap().len(),
        1,
        "{instances}"
    );

    // Bob is not on the list: told so once, politely, then left alone.
    let created = bob
        .matrix(
            POST,
            "/createRoom",
            Some(json!({"preset": "trusted_private_chat", "is_direct": true, "invite": [door]})),
        )
        .await;
    let bobs = created["room_id"].as_str().unwrap().to_owned();
    let said = bob_watch.next_notice(&bobs, door).await;
    assert!(said.contains("isn't available to your account"), "{said}");
    assert!(said.contains("administrator can change that"), "{said}");
    bob.say(&bobs, "please?").await;
    bob.say(&bobs, "pretty please?").await;
    admin
        .admin_expect(
            GET,
            "/bridge-offerings/mautrix-whatsapp/instances/%40bob%3Aexample.org",
            None,
            reqwest::StatusCode::NOT_FOUND,
        )
        .await;

    // Alice's bridge comes up. Her bot invites her, and the front door says so where she asked.
    until_state(&admin, &instance_path, "starting").await;
    let registration = admin
        .admin(GET, "/appservices/whatsapp-alice/registration", None)
        .await;
    let (url, _bridge) = stand_in(registration["hs_token"].as_str().unwrap()).await;
    admin
        .admin(
            PATCH,
            "/appservices/whatsapp-alice",
            Some(json!({"url": url})),
        )
        .await;
    let invited = alice_watch
        .next(|s| {
            s["rooms"]["invite"]
                .as_object()
                .is_some_and(|rooms| !rooms.is_empty())
        })
        .await;
    let dm = invited["rooms"]["invite"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .to_owned();
    let said = alice_watch.next_notice(&front, door).await;
    assert!(said.contains("Your WhatsApp bridge is ready"), "{said}");
    assert!(
        said.contains("invited you to a chat with @whatsappbot_alice:example.org"),
        "{said}"
    );
    assert_ne!(dm, front);
    // By now bob's two further messages have long been handled: one refusal, not three.
    assert_eq!(room_notices(bob, &bobs, door).await.len(), 1);

    // The manager bot: invited, it explains itself; then commands.
    let created = alice
        .matrix(
            POST,
            "/createRoom",
            Some(json!({"preset": "trusted_private_chat", "is_direct": true, "invite": [manager_bot]})),
        )
        .await;
    let control = created["room_id"].as_str().unwrap().to_owned();
    let help = alice_watch.next_notice(&control, manager_bot).await;
    assert!(help.contains("I set up bridges"), "{help}");
    assert!(help.contains("`start <bridge>`"), "{help}");
    assert!(help.contains(door), "{help}");
    let mut ask = async |text: &str| {
        alice.say(&control, text).await;
        alice_watch.next_notice(&control, manager_bot).await
    };

    let listed = ask("list").await;
    assert!(
        listed.contains("- WhatsApp: `start whatsapp` (yours: ready)"),
        "{listed}"
    );
    let status = ask("status").await;
    assert!(status.contains("- WhatsApp: ready"), "{status}");
    let asked = ask("stop whatsapp").await;
    assert!(asked.contains("Send `stop whatsapp confirm`"), "{asked}");
    assert_eq!(
        admin.admin(GET, &instance_path, None).await["state"],
        "ready",
        "asking is not confirming"
    );
    let stopped = ask("stop whatsapp confirm").await;
    assert_eq!(stopped, "Your WhatsApp bridge is removed.");
    admin
        .admin_expect(GET, &instance_path, None, reqwest::StatusCode::NOT_FOUND)
        .await;
    admin
        .admin_expect(
            GET,
            "/appservices/whatsapp-alice",
            None,
            reqwest::StatusCode::NOT_FOUND,
        )
        .await;
    let status = ask("status").await;
    assert!(status.contains("don't have any bridges yet"), "{status}");
    let started = ask("start whatsapp").await;
    assert!(started.contains("asked for one for you"), "{started}");
    assert_eq!(
        admin.admin(GET, &instance_path, None).await["user_id"],
        alice_id,
        "started again from the manager bot"
    );
    let unknown = ask("start pigeons").await;
    assert!(unknown.contains("Which one?"), "{unknown}");
    let nothing = ask("stop signal").await;
    assert!(nothing.contains("Which one?"), "{nothing}");

    // Bob asks the manager bot and is offered nothing.
    let created = bob
        .matrix(
            POST,
            "/createRoom",
            Some(json!({"preset": "trusted_private_chat", "is_direct": true, "invite": [manager_bot]})),
        )
        .await;
    let bobs_control = created["room_id"].as_str().unwrap().to_owned();
    bob_watch.next_notice(&bobs_control, manager_bot).await;
    bob.say(&bobs_control, "list").await;
    let said = bob_watch.next_notice(&bobs_control, manager_bot).await;
    assert!(said.contains("no bridges you can set up"), "{said}");

    handle.shutdown().await;
}

#[tokio::test]
async fn a_shared_offering_has_its_one_instance_from_the_start_and_heisenbridge_runs_from_its_files()
 {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, _) = boot(dir.path(), &[]).await;

    let offering = admin
        .admin(
            PUT,
            "/bridge-offerings/heisenbridge",
            Some(json!({"runtime": "elsewhere"})),
        )
        .await;
    assert_eq!(offering["mode"], "shared", "{offering}");
    assert!(offering["front_door"].is_null(), "{offering}");
    let instances = admin
        .admin(GET, "/bridge-offerings/heisenbridge/instances", None)
        .await;
    assert_eq!(
        instances["items"].as_array().unwrap().len(),
        1,
        "{instances}"
    );
    assert!(instances["items"][0]["user_id"].is_null(), "{instances}");
    let path = "/bridge-offerings/heisenbridge/instances/_";
    let seen = until_state(&admin, path, "starting").await;
    assert_eq!(seen["appservice_id"], "heisenbridge", "{seen}");
    assert_eq!(seen["bot"], "@heisenbridge:example.org");
    let files = admin.admin(POST, &format!("{path}/files"), None).await;
    assert!(files["config_yaml"].is_null(), "{files}");
    let compose = files["compose_yaml"].as_str().unwrap();
    assert!(compose.contains("hif1/heisenbridge:latest"), "{compose}");
    assert!(
        !compose.contains("\"-o\""),
        "no owner for a shared bouncer: {compose}"
    );
    let registration_yaml = files["registration_yaml"].as_str().unwrap().to_owned();
    let registration: Value = serde_yaml_ng::from_str(&registration_yaml).unwrap();
    assert_eq!(registration["id"], "heisenbridge");
    assert_eq!(registration["sender_localpart"], "heisenbridge");
    assert_eq!(registration["io.myelin.bridge_instance"], "_");

    // Run it for real when it is installed; otherwise a stand-in, so the machine still turns.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let installed = std::process::Command::new("heisenbridge")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    eprintln!(
        "heisenbridge: {}",
        if installed {
            "installed, running it for real"
        } else {
            "not installed, a stand-in answers its pings"
        }
    );
    let mut child = None;
    let _stand_in;
    let url = if installed {
        let registration_path = dir.path().join("heisenbridge.yaml");
        std::fs::write(&registration_path, &registration_yaml).unwrap();
        let log = std::fs::File::create(dir.path().join("heisenbridge.log")).unwrap();
        child = Some(
            std::process::Command::new("heisenbridge")
                .args(["-c"])
                .arg(&registration_path)
                .args(["-l", "127.0.0.1", "-p", &port.to_string(), "-vv"])
                .arg(handle.base_url())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("heisenbridge starts"),
        );
        format!("http://127.0.0.1:{port}")
    } else {
        eprintln!("heisenbridge is not installed: standing in for it");
        let (url, bridge) = stand_in(registration["hs_token"].as_str().unwrap()).await;
        _stand_in = bridge;
        url
    };
    admin
        .admin(
            PATCH,
            "/appservices/heisenbridge",
            Some(json!({"url": url})),
        )
        .await;
    let mut seen = Value::Null;
    for _ in 0..240 {
        seen = admin.admin(GET, path, None).await;
        if seen["state"] == "ready" {
            break;
        }
        if let Some(child) = child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            panic!(
                "heisenbridge exited with {status}:\n{}",
                std::fs::read_to_string(dir.path().join("heisenbridge.log")).unwrap_or_default()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(seen["state"], "ready", "{seen}");
    assert_eq!(seen["health"], "healthy", "{seen}");
    if let Some(mut child) = child {
        // The real bridge registered its bot through this server with its instance's token.
        let listed = admin.admin(GET, "/users?limit=50", None).await;
        let bot = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["user_id"] == "@heisenbridge:example.org")
            .unwrap_or_else(|| panic!("{listed}"));
        assert_eq!(bot["appservice_id"], "heisenbridge", "{bot}");
        let _ = child.kill();
        let _ = child.wait();
        let log = std::fs::read_to_string(dir.path().join("heisenbridge.log")).unwrap_or_default();
        assert!(
            log.contains("bridge is now running") || log.contains("Appservice user registration"),
            "{log}"
        );
    }

    let refused = admin
        .admin_expect(
            DELETE,
            "/bridge-offerings/heisenbridge",
            None,
            reqwest::StatusCode::CONFLICT,
        )
        .await;
    assert!(
        refused["detail"].as_str().is_some_and(|d| d.contains("1 ")),
        "{refused}"
    );
    admin
        .admin_expect(
            DELETE,
            "/bridge-offerings/heisenbridge?remove_instances=true",
            None,
            reqwest::StatusCode::NO_CONTENT,
        )
        .await;
    admin
        .admin_expect(
            GET,
            "/appservices/heisenbridge",
            None,
            reqwest::StatusCode::NOT_FOUND,
        )
        .await;

    handle.shutdown().await;
}
