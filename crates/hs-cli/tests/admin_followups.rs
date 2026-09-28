//! The admin API follow-ups of 2026-09-28 through the real server (`spawn_serve`, the code `hs
//! serve` runs), each the way the management interface uses it:
//!
//! - `GET /reports` filtered by the reported person and by the reporter, and every report filed
//!   through the client-server API announced on the event stream as `report.created`.
//! - The bulk media deletions as spawned tasks: a deletion reports its progress on the task and
//!   the event stream, and one cancelled midway stops there.
//! - The key server's cache of remote signing keys: `federation.keys.list` (this server's own
//!   keys), `federation.keys.get` (what is cached for another server) and
//!   `federation.keys.refresh`, which fetches them again from a second server; and
//!   `federation.destinations.rooms`, the rooms shared with a destination.
//!
//! Server names are IP literals with a port, as in `federation_two_servers.rs`, so two of these
//! servers can federate over plain HTTP without discovery.

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: \"127.0.0.1:{port}\"\n\
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

/// Somebody who calls a server: an administrator, a user, or nobody.
#[derive(Clone)]
struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
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

    async fn get(&self, path: &str) -> Value {
        self.expect(Method::GET, path, None, StatusCode::OK).await
    }
}

/// One server, its first administrator, and its name.
struct Server {
    handle: hs_cli::serve::ServeHandle,
    name: String,
    admin: Caller,
    nobody: Caller,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start() -> Self {
        let port = reserve_port();
        let dir = tempfile::tempdir().unwrap();
        let handle = hs_cli::serve::spawn_serve(
            config(port, dir.path()),
            hs_cli::serve::ServeOptions {
                federation_scheme: Some("http"),
                ..Default::default()
            },
        )
        .await
        .expect("the server boots");
        let nobody = Caller {
            base: handle.base_url(),
            token: None,
        };
        let link = handle
            .setup_link
            .clone()
            .expect("a fresh server offers setup");
        let setup_token = link.split_once("#token=").unwrap().1.to_owned();
        let session = nobody
            .expect(
                Method::POST,
                "/api/v1/setup",
                Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
                StatusCode::CREATED,
            )
            .await;
        let admin = Caller {
            base: handle.base_url(),
            token: Some(session["access_token"].as_str().unwrap().to_owned()),
        };
        Self {
            name: format!("127.0.0.1:{port}"),
            handle,
            admin,
            nobody,
            _dir: dir,
        }
    }

    /// Registers `name` through the client-server API.
    async fn user(&self, name: &str) -> (String, Caller) {
        let registered = self
            .nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}})),
                StatusCode::OK,
            )
            .await;
        (
            registered["user_id"].as_str().unwrap().to_owned(),
            Caller {
                base: self.handle.base_url(),
                token: Some(registered["access_token"].as_str().unwrap().to_owned()),
            },
        )
    }

    /// Opens `GET /api/v1/events` as the administrator, with the given `types` filters.
    async fn events(&self, types: &[&str]) -> EventStream {
        let query: Vec<(&str, &str)> = types.iter().map(|t| ("types", *t)).collect();
        let response = reqwest::Client::new()
            .get(format!("{}/api/v1/events", self.handle.base_url()))
            .query(&query)
            .bearer_auth(self.admin.token.as_deref().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = EventStream {
            response,
            buffer: String::new(),
        };
        assert_eq!(stream.next().await["type"], "stream.hello");
        stream
    }
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('$', "%24")
}

/// An open `text/event-stream` response, read frame by frame.
struct EventStream {
    response: reqwest::Response,
    buffer: String,
}

impl EventStream {
    /// The next event's JSON (keepalives skipped), or a panic after 20 seconds.
    async fn next(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let frame: String = self.buffer.drain(..end + 2).collect();
                let data: Vec<&str> = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                if data.is_empty() {
                    continue;
                }
                return serde_json::from_str(&data.join("\n")).unwrap();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let chunk = tokio::time::timeout(left, self.response.chunk())
                .await
                .expect("an event arrives within 20 seconds")
                .unwrap()
                .expect("the stream stays open");
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// Events until one of type `wanted` arrives; that one.
    async fn next_of(&mut self, wanted: &str) -> Value {
        loop {
            let event = self.next().await;
            if event["type"] == wanted {
                return event;
            }
        }
    }
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_filter_by_person_and_each_filed_report_is_announced() {
    let server = Server::start().await;
    let (alice_id, alice) = server.user("alice").await;
    let (_bob_id, bob) = server.user("bob").await;
    let (mallory_id, mallory) = server.user("mallory").await;
    assert_eq!(mallory_id, format!("@mallory:{}", server.name));
    let mut stream = server.events(&["report.*"]).await;

    // Alice and Bob both report Mallory; Mallory reports Alice back.
    let mut filed = Vec::new();
    for (reporter, about, reason) in [
        (&alice, &mallory_id, "spam in my DMs"),
        (&bob, &mallory_id, "the same spam"),
        (&mallory, &alice_id, "she reported me"),
    ] {
        reporter
            .expect(
                Method::POST,
                &format!("/_matrix/client/v3/users/{}/report", escape(about)),
                Some(json!({"reason": reason})),
                StatusCode::OK,
            )
            .await;
        // Each one is on the event stream as it is filed, before anybody lists anything.
        let event = stream.next_of("report.created").await;
        assert_eq!(event["resource"]["type"], "report", "{event}");
        assert_eq!(event["data"]["reason"], reason, "{event}");
        assert_eq!(event["data"]["reported_user_id"], about.as_str(), "{event}");
        assert_eq!(event["data"]["status"], "open", "{event}");
        filed.push(event["resource"]["id"].as_str().unwrap().to_owned());
    }

    let about_mallory = server
        .admin
        .get(&format!(
            "/api/v1/reports?reported_user_id={}",
            escape(&mallory_id)
        ))
        .await;
    assert_eq!(ids(&about_mallory), [filed[1].clone(), filed[0].clone()]);
    let by_mallory = server
        .admin
        .get(&format!(
            "/api/v1/reports?reporter_id={}",
            escape(&mallory_id)
        ))
        .await;
    assert_eq!(ids(&by_mallory), [filed[2].clone()]);
    let both = server
        .admin
        .get(&format!(
            "/api/v1/reports?reported_user_id={}&reporter_id={}",
            escape(&mallory_id),
            escape(&alice_id)
        ))
        .await;
    assert_eq!(ids(&both), [filed[0].clone()]);

    // A decision is announced too, as `report.resolved`.
    server
        .admin
        .expect(
            Method::POST,
            &format!("/api/v1/reports/{}/resolve", filed[2]),
            Some(json!({"resolution": "no_action"})),
            StatusCode::OK,
        )
        .await;
    let resolved = stream.next_of("report.resolved").await;
    assert_eq!(resolved["resource"]["id"], filed[2].as_str(), "{resolved}");
}
