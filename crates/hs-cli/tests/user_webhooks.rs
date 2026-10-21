//! Decision 0057 against the real server: any user adds a webhook to a room of theirs through
//! `/_matrix/client/unstable/io.myelin.webhooks`, with their ordinary access token. One
//! in-process server; the shared Hookshot offering runs `elsewhere` (no bridge is started: the
//! server acts as the bot once the instance is registered, and the bridge would pick the state
//! event up when it ran). Alice, who made her room, may add one there: the bot is invited as
//! her, joins, is raised to the level the state event takes, and the connection is written
//! with her name in it; Bob, a plain member, is refused with the level it takes; a new room
//! the bot makes is joined by Alice with power 100; her webhooks are listed, removed, audited
//! with her as actor, counted as `via="user"`, and rate-limited; an encrypted room is refused;
//! without the offering the API says so.

use reqwest::Method;
use serde_json::{Value, json};

const PREFIX: &str = "/_matrix/client/unstable/io.myelin.webhooks";
const HOOK_TYPE: &str = "uk.half-shot.matrix-hookshot.generic.hook";
const BOT: &str = "@hookshot:example.org";

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
        method: Method,
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

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.call(method, &format!("/api/v1{path}"), body).await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    async fn matrix(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self
            .call(method, &format!("/_matrix/client/v3{path}"), body)
            .await;
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }

    /// A call to the user webhooks API: the status and the body.
    async fn webhooks(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (reqwest::StatusCode, Value) {
        self.call(method, &format!("{PREFIX}{path}"), body).await
    }

    async fn state(&self, room_id: &str, event_type: &str, state_key: &str) -> Value {
        let (status, body) = self
            .call(
                Method::GET,
                &format!(
                    "/_matrix/client/v3/rooms/{}/state/{event_type}/{state_key}",
                    escape(room_id)
                ),
                None,
            )
            .await;
        assert!(
            status.is_success(),
            "{event_type}/{state_key}: {status} {body}"
        );
        body
    }

    async fn create_room(&self, body: Value) -> String {
        let made = self.matrix(Method::POST, "/createRoom", Some(body)).await;
        made["room_id"].as_str().unwrap().to_owned()
    }

    async fn joined_rooms(&self) -> Vec<String> {
        let rooms = self.matrix(Method::GET, "/joined_rooms", None).await;
        rooms["joined_rooms"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn any_user_adds_a_webhook_to_a_room_of_theirs_through_the_user_api() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), &["alice", "bob"]).await;
    let alice = &users[0];
    let bob = &users[1];
    let alice_id = "@alice:example.org";

    // Without a token: refused as any client endpoint is.
    let anon = Caller {
        token: String::new(),
        ..alice.clone()
    };
    let (status, body) = anon.webhooks(Method::GET, "/sources", None).await;
    assert_eq!(status, 401, "{body}");

    // Before the offering exists: a clear not-found, not an empty list.
    let (status, body) = alice.webhooks(Method::GET, "/sources", None).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["errcode"], "M_NOT_FOUND");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("ask an administrator"),
        "{body}"
    );

    // The administrator offers Hookshot with a public URL; the shared instance registers.
    admin
        .admin(
            Method::PUT,
            "/bridge-offerings/matrix-hookshot",
            Some(json!({"runtime": "elsewhere", "options": {"public_url": "https://hooks.example.org"}})),
        )
        .await;
    until("the shared instance registered", || async {
        let instance = admin
            .admin(
                Method::GET,
                "/bridge-offerings/matrix-hookshot/instances/_",
                None,
            )
            .await;
        instance["appservice_id"].is_string() && instance["state"] != "requested"
    })
    .await;
    until("the user API sees the bridge", || async {
        alice.webhooks(Method::GET, "/sources", None).await.0 == 200
    })
    .await;
    let (status, sources) = alice.webhooks(Method::GET, "/sources", None).await;
    assert_eq!(status, 200, "{sources}");
    let ids: Vec<&str> = sources["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"datadog") && ids.contains(&"generic"),
        "{ids:?}"
    );

    // Alice's room, with Bob a plain member. The bot is not in it.
    let deploys = alice
        .create_room(
            json!({"name": "Deploys", "preset": "private_chat", "invite": ["@bob:example.org"]}),
        )
        .await;
    bob.matrix(
        Method::POST,
        &format!("/join/{}", escape(&deploys)),
        Some(json!({})),
    )
    .await;
    let (status, rooms) = alice.webhooks(Method::GET, "/rooms", None).await;
    assert_eq!(status, 200, "{rooms}");
    let room = rooms["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["room_id"] == deploys.as_str())
        .unwrap_or_else(|| panic!("{rooms}"))
        .clone();
    assert_eq!(room["name"], "Deploys", "{room}");
    assert_eq!(room["may_add"], true, "{room}");
    assert_eq!(room["bot_joined"], false, "{room}");
    let (_, bobs) = bob.webhooks(Method::GET, "/rooms", None).await;
    let room = bobs["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["room_id"] == deploys.as_str())
        .unwrap_or_else(|| panic!("{bobs}"))
        .clone();
    assert_eq!(room["may_add"], false, "{room}");
    assert!(
        room["reason"].as_str().unwrap().contains("power level 50"),
        "{room}"
    );

    // Bob may not; Alice may, and the server invites the bot as her, the bot joins, is raised
    // to 50, and the connection is written with her name in it.
    let (status, body) = bob
        .webhooks(
            Method::POST,
            "/webhooks",
            Some(json!({"source": "generic", "name": "deploys", "room": {"id": deploys}})),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["errcode"], "M_FORBIDDEN");
    let (status, made) = alice
        .webhooks(
            Method::POST,
            "/webhooks",
            Some(json!({"source": "generic", "name": "deploys", "room": {"id": deploys}})),
        )
        .await;
    assert_eq!(status, 201, "{made}");
    let url = made["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://hooks.example.org/webhook/") && url.len() == 34 + 36,
        "{url}"
    );
    assert_eq!(made["room_id"], deploys.as_str());
    assert_eq!(made["source"], "generic");
    assert!(made["setup"].as_str().unwrap().contains(url), "{made}");
    let members = alice
        .matrix(
            Method::GET,
            &format!("/rooms/{}/joined_members", escape(&deploys)),
            None,
        )
        .await;
    assert!(members["joined"].get(BOT).is_some(), "{members}");
    let levels = alice.state(&deploys, "m.room.power_levels", "").await;
    assert_eq!(levels["users"][BOT], 50, "{levels}");
    assert_eq!(levels["users"][alice_id], 100, "{levels}");
    let event = alice.state(&deploys, HOOK_TYPE, "deploys").await;
    assert_eq!(event["name"], "deploys", "{event}");
    assert_eq!(event["io.myelin.source"], "generic", "{event}");
    assert_eq!(event["io.myelin.created_by"], alice_id, "{event}");
    assert!(
        event["io.myelin.created_at"].as_u64().unwrap() > 0,
        "{event}"
    );
    assert!(event.get("transformationFunction").is_none(), "{event}");
    // The account data the bridge reads the hook id from: the bot's, with the URL's last part.
    let hook_id = url.rsplit('/').next().unwrap();
    assert_eq!(hook_id.len(), 36);
    // The notice in the room names Alice, not the URL.
    until("the bot's notice", || async {
        let page = alice
            .matrix(
                Method::GET,
                &format!("/rooms/{}/messages?dir=b&limit=20", escape(&deploys)),
                None,
            )
            .await;
        page["chunk"].as_array().unwrap().iter().any(|e| {
            e["sender"] == BOT
                && e["content"]["body"].as_str().is_some_and(|b| {
                    b.contains("deploys") && b.contains(alice_id) && !b.contains("/webhook/")
                })
        })
    })
    .await;
    // The same name again in the same room is a conflict.
    let (status, body) = alice
        .webhooks(
            Method::POST,
            "/webhooks",
            Some(json!({"source": "generic", "name": "deploys", "room": {"id": deploys}})),
        )
        .await;
    assert_eq!(status, 409, "{body}");

    // A new room: the bot makes it, Alice is joined to it at once and is its admin.
    let (status, alerts) = alice
        .webhooks(
            Method::POST,
            "/webhooks",
            Some(json!({"source": "datadog", "name": "datadog-alerts", "room": {"create": {"name": "Alerts", "topic": "Datadog monitors"}}})),
        )
        .await;
    assert_eq!(status, 201, "{alerts}");
    let alerts_room = alerts["room_id"].as_str().unwrap().to_owned();
    assert!(alice.joined_rooms().await.contains(&alerts_room));
    assert_eq!(
        alice.state(&alerts_room, "m.room.name", "").await["name"],
        "Alerts"
    );
    assert_eq!(
        alice.state(&alerts_room, "m.room.topic", "").await["topic"],
        "Datadog monitors"
    );
    let levels = alice.state(&alerts_room, "m.room.power_levels", "").await;
    assert_eq!(levels["users"][alice_id], 100, "{levels}");
    assert_eq!(levels["users"][BOT], 100, "{levels}");
    let event = alice.state(&alerts_room, HOOK_TYPE, "datadog-alerts").await;
    assert!(
        event["transformationFunction"]
            .as_str()
            .is_some_and(|f| f.contains("result")),
        "{event}"
    );
    assert!(alerts["files"].as_object().unwrap().is_empty(), "{alerts}");

    // Listed: Alice's two, with who made them; Bob sees only the one in his room.
    let (status, mine) = alice.webhooks(Method::GET, "/webhooks", None).await;
    assert_eq!(status, 200, "{mine}");
    let items = mine["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{mine}");
    let listed = items
        .iter()
        .find(|w| w["name"] == "datadog-alerts")
        .unwrap();
    assert_eq!(listed["room_id"], alerts_room.as_str());
    assert_eq!(listed["room_name"], "Alerts");
    assert_eq!(listed["source"], "datadog");
    assert_eq!(listed["created_by"], alice_id);
    assert!(listed["created_at"].as_u64().unwrap() > 0);
    assert!(
        !mine.to_string().contains("/webhook/"),
        "the listing carries a URL: {mine}"
    );
    let (_, bobs) = bob.webhooks(Method::GET, "/webhooks", None).await;
    let names: Vec<&str> = bobs["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["name"].as_str())
        .collect();
    assert_eq!(names, vec!["deploys"], "{bobs}");

    // Audited with Alice as the actor, without the URL.
    let log = admin
        .admin(
            Method::GET,
            "/audit-log?action=bridge_connection.created",
            None,
        )
        .await;
    let entries = log["items"].as_array().unwrap_or_else(|| panic!("{log}"));
    assert_eq!(entries.len(), 2, "{log}");
    for entry in entries {
        assert_eq!(entry["actor"]["id"], alice_id, "{entry}");
        assert_eq!(entry["actor"]["kind"], "user", "{entry}");
        assert_eq!(entry["target"]["type"], "bridge_connection", "{entry}");
    }
    assert!(
        !log.to_string().contains(hook_id),
        "the audit log carries the secret"
    );

    // Counted as the user path.
    let metrics = alice
        .client
        .get(format!("{}/metrics", alice.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains(
            "hs_bridge_webhook_connections_total{bridge_type=\"matrix-hookshot\",source=\"datadog\",via=\"user\",outcome=\"created\"} 1"
        ),
        "{metrics}"
    );

    // Bob may not remove Alice's; Alice removes hers, which empties the event; again is 404.
    let (status, body) = bob
        .webhooks(
            Method::DELETE,
            &format!("/webhooks/{}/deploys", escape(&deploys)),
            None,
        )
        .await;
    assert_eq!(status, 403, "{body}");
    let (status, body) = alice
        .webhooks(
            Method::DELETE,
            &format!("/webhooks/{}/deploys", escape(&deploys)),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let event = alice.state(&deploys, HOOK_TYPE, "deploys").await;
    assert_eq!(event, json!({}), "{event}");
    let (status, body) = alice
        .webhooks(
            Method::DELETE,
            &format!("/webhooks/{}/deploys", escape(&deploys)),
            None,
        )
        .await;
    assert_eq!(status, 404, "{body}");
    let (_, mine) = alice.webhooks(Method::GET, "/webhooks", None).await;
    assert_eq!(mine["items"].as_array().unwrap().len(), 1, "{mine}");
    let log = admin
        .admin(
            Method::GET,
            "/audit-log?action=bridge_connection.deleted",
            None,
        )
        .await;
    assert_eq!(log["items"].as_array().unwrap().len(), 1, "{log}");
    assert_eq!(log["items"][0]["actor"]["id"], alice_id);

    // An encrypted room is refused: the bridge would post in the clear.
    let secret = alice
        .create_room(json!({
            "name": "Secret",
            "preset": "private_chat",
            "initial_state": [{"type": "m.room.encryption", "state_key": "", "content": {"algorithm": "m.megolm.v1.aes-sha2"}}]
        }))
        .await;
    let (status, body) = alice
        .webhooks(
            Method::POST,
            "/webhooks",
            Some(json!({"source": "generic", "name": "secret", "room": {"id": secret}})),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("encrypted"),
        "{body}"
    );

    // A bad body is M_INVALID_PARAM, at the field.
    let (status, body) = alice
        .webhooks(
            Method::POST,
            "/webhooks",
            Some(json!({"source": "nope", "name": "x", "room": {"id": deploys}})),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["errcode"], "M_INVALID_PARAM");
    assert!(body["error"].as_str().unwrap().contains("/name"), "{body}");

    // The limit: ten at once, then M_LIMIT_EXCEEDED with a retry.
    let mut refused = None;
    for n in 0..12 {
        let (status, body) = alice
            .webhooks(
                Method::POST,
                "/webhooks",
                Some(json!({"source": "generic", "name": format!("burst-{n}"), "room": {"id": deploys}})),
            )
            .await;
        if status == 429 {
            refused = Some(body);
            break;
        }
        assert_eq!(status, 201, "{body}");
    }
    let refused = refused.expect("the eleventh creation within the hour is refused");
    assert_eq!(refused["errcode"], "M_LIMIT_EXCEEDED", "{refused}");
    assert!(refused["retry_after_ms"].as_u64().unwrap() > 0, "{refused}");

    // The page is served by the binary (the built interface, or a word when none is embedded).
    let page = alice
        .client
        .get(format!("{}/integrations/", alice.base))
        .send()
        .await
        .unwrap();
    let status = page.status();
    let text = page.text().await.unwrap();
    assert!(
        (status == 200 && text.contains("<div id=\"root\">"))
            || (status == 404 && text.contains("no integrations page")),
        "{status}: {text}"
    );

    handle.shutdown().await;
}
