//! A bridge's double puppet is not rate-limited when its appservice is not (decision 0053),
//! through the real server.
//!
//! A mautrix bridge double puppets a real user by logging in as them with
//! `m.login.application_service` and its own `as_token`, then sending on the ordinary access
//! token that login returns. That session is attributed to the appservice: with the
//! registration's `rate_limited: false` it is exempt from `rate_limits.message` as the
//! appservice's own requests are, while the same user's own (password) session stays limited.
//! An administrator's per-user override still applies to the puppet, and a registration edited to
//! `rate_limited: true` limits it from the next request. The admin API's sessions list names the
//! appservice the puppet's session was issued to.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const AS_TOKEN: &str = "as_token_for_the_puppet_rate_limit_test_00000000000000000000000";
const HS_TOKEN: &str = "hs_token_for_the_puppet_rate_limit_test_00000000000000000000000";
/// `rate_limits.message.burst_count`: a sender's first this-many messages pass, the next is
/// refused (the rate refills one every 20 s, far slower than the test).
const BURST: usize = 3;

fn test_config(data_dir: &std::path::Path, registration: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, admin, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         rate_limits:\n  message:\n    per_second: 0.05\n    burst_count: {BURST}\n\
         appservices:\n  registration_files: [{registration:?}]\n",
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
}

/// A double-puppeting registration as the Bridges area writes one: no `url`, a non-exclusive
/// claim on the real user, `rate_limited: false`.
fn registration_yaml() -> String {
    format!(
        "id: puppeteer\nas_token: {AS_TOKEN}\nhs_token: {HS_TOKEN}\n\
         sender_localpart: puppeteer\nrate_limited: false\n\
         namespaces:\n  users:\n    - regex: '@alice:example\\.org'\n      exclusive: false\n"
    )
}

#[derive(Clone)]
struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
    fn with_token(&self, token: &str) -> Caller {
        Caller {
            base: self.base.clone(),
            token: Some(token.to_owned()),
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = reqwest::Client::new().request(method, format!("{}{path}", self.base));
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

    /// `PUT /send` of an `m.room.message`; the status and body.
    async fn send(&self, room: &str, txn: &str) -> (StatusCode, Value) {
        self.call(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                escape(room)
            ),
            Some(json!({"msgtype": "m.text", "body": txn})),
        )
        .await
    }

    /// Sends until one is refused; how many passed first. Panics if `most` all pass.
    async fn sends_before_429(&self, room: &str, prefix: &str, most: usize) -> usize {
        for n in 0..most {
            let (status, body) = self.send(room, &format!("{prefix}{n}")).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                assert_eq!(body["errcode"], "M_LIMIT_EXCEEDED", "{body}");
                return n;
            }
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        panic!("{most} sends ({prefix}) and none was rate-limited");
    }
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace(':', "%3A")
        .replace('@', "%40")
}

/// A counter's value on `/metrics` (0 when absent).
async fn metric(base: &str, name: &str) -> u64 {
    let text = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let prefix = format!("{name} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map_or(0, |n| n.trim().parse().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_double_puppet_of_an_unlimited_appservice_is_not_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let registration = dir.path().join("puppeteer.yaml");
    std::fs::write(&registration, registration_yaml()).unwrap();
    let handle = hs_cli::serve::spawn_serve(
        test_config(dir.path(), &registration),
        hs_cli::serve::ServeOptions::default(),
    )
    .await
    .expect("server should boot");
    let base = handle.base_url();
    let nobody = Caller {
        base: base.clone(),
        token: None,
    };

    // The first administrator, for the override and the registration edit.
    let setup_token = handle
        .setup_link
        .clone()
        .expect("a fresh server offers setup")
        .split_once("#token=")
        .unwrap()
        .1
        .to_owned();
    let session = nobody
        .expect(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
            StatusCode::CREATED,
        )
        .await;
    let admin = nobody.with_token(session["access_token"].as_str().unwrap());

    // Alice, a person, with her own password session and a room.
    nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(json!({"username": "alice", "password": "hunter2-alice", "auth": {"type": "m.login.dummy"}, "inhibit_login": true})),
            StatusCode::OK,
        )
        .await;
    let own = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "hunter2-alice"})),
            StatusCode::OK,
        )
        .await;
    let alice = nobody.with_token(own["access_token"].as_str().unwrap());
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "private_chat"})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The bridge double puppets her, as mautrix does with `login_shared_secret: appservice`.
    let puppet_login = nobody
        .with_token(AS_TOKEN)
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({"type": "m.login.application_service", "identifier": {"type": "m.id.user", "user": "alice"}})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(puppet_login["user_id"], "@alice:example.org");
    let puppet = nobody.with_token(puppet_login["access_token"].as_str().unwrap());
    let whoami = puppet
        .expect(
            Method::GET,
            "/_matrix/client/v3/account/whoami",
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(whoami["user_id"], "@alice:example.org");

    // A catch-up: many times the burst on the puppet's token, none refused.
    let exempt_before = metric(&base, "hs_room_puppet_rate_limit_exempt_writes_total").await;
    for n in 0..(BURST * 4) {
        let (status, body) = puppet.send(&room, &format!("catch-up-{n}")).await;
        assert_eq!(status, StatusCode::OK, "puppet send {n}: {body}");
    }
    assert!(
        metric(&base, "hs_room_puppet_rate_limit_exempt_writes_total").await
            >= exempt_before + (BURST * 4) as u64
    );

    // Her own session is limited as before, with the whole burst still hers: the puppet's sends
    // took nothing from it.
    let limited_before = metric(&base, "hs_room_server_rate_limited_writes_total").await;
    assert_eq!(
        alice.sends_before_429(&room, "own-", BURST + 2).await,
        BURST
    );
    assert!(metric(&base, "hs_room_server_rate_limited_writes_total").await > limited_before);
    // ... while the puppet still sends.
    let (status, body) = puppet.send(&room, "puppet-after-own-429").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The Users area's sessions list says which of her sessions is the bridge's.
    let sessions = admin
        .expect(
            Method::GET,
            "/api/v1/users/%40alice%3Aexample.org/sessions",
            None,
            StatusCode::OK,
        )
        .await;
    let issued: Vec<(&str, Option<&str>)> = sessions["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["device_id"].as_str().unwrap(),
                s["issued_by_appservice"].as_str(),
            )
        })
        .collect();
    assert!(
        issued.contains(&(
            puppet_login["device_id"].as_str().unwrap(),
            Some("puppeteer")
        )),
        "{sessions}"
    );
    assert!(
        issued.contains(&(own["device_id"].as_str().unwrap(), None)),
        "{sessions}"
    );

    // An administrator's override on the person applies to the puppet too.
    let override_path = "/api/v1/users/%40alice%3Aexample.org/rate-limit";
    admin
        .expect(
            Method::PUT,
            override_path,
            Some(json!({"messages_per_second": 0.01, "burst_count": 1})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(puppet.sends_before_429(&room, "overridden-", 3).await, 1);
    admin
        .expect(Method::DELETE, override_path, None, StatusCode::NO_CONTENT)
        .await;
    let (status, body) = puppet.send(&room, "override-cleared").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The registration now says `rate_limited: true`: the puppet is limited from the next request.
    admin
        .expect(
            Method::PATCH,
            "/api/v1/appservices/puppeteer",
            Some(json!({"rate_limited": true})),
            StatusCode::OK,
        )
        .await;
    let passed = puppet.sends_before_429(&room, "limited-", BURST + 2).await;
    assert!(passed <= BURST, "{passed}");

    let _ = handle.shutdown().await;
}
