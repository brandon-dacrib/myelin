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
        Self::start_with(hs_cli::serve::ServeOptions::default()).await
    }

    /// [`Server::start`] with `options` (federation over plain HTTP whatever they say).
    async fn start_with(options: hs_cli::serve::ServeOptions) -> Self {
        let port = reserve_port();
        let dir = tempfile::tempdir().unwrap();
        let handle = hs_cli::serve::spawn_serve(
            config(port, dir.path()),
            hs_cli::serve::ServeOptions {
                federation_scheme: Some("http"),
                ..options
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

// ---------------------------------------------------------------------------------------------
// Bulk media deletions as tasks
// ---------------------------------------------------------------------------------------------

/// How many of this server's own uploads the admin API lists.
async fn local_media(server: &Server) -> usize {
    server
        .admin
        .get("/api/v1/media?origin=local&limit=100")
        .await["items"]
        .as_array()
        .unwrap()
        .len()
}

/// The value of the metric line starting with `prefix` on `/metrics`, if there is one.
async fn metric(server: &Server, prefix: &str) -> Option<f64> {
    let text = reqwest::get(format!("{}/metrics", server.handle.base_url()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    text.lines()
        .find(|l| l.starts_with(prefix))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bulk_media_deletion_is_a_task_that_reports_progress_and_stops_when_cancelled() {
    // A pause after each item, so that there is a midway to cancel at.
    let server = Server::start_with(hs_cli::serve::ServeOptions {
        media_bulk_pause: Duration::from_millis(100),
        ..Default::default()
    })
    .await;
    let (_alice_id, alice) = server.user("alice").await;
    const UPLOADS: usize = 24;
    for n in 0..UPLOADS {
        let response = reqwest::Client::new()
            .post(format!(
                "{}/_matrix/client/v1/media/upload?filename=f{n}.txt",
                server.handle.base_url()
            ))
            .bearer_auth(alice.token.as_deref().unwrap())
            .header("content-type", "text/plain")
            .body(format!("upload number {n}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(local_media(&server).await, UPLOADS);
    // The Statistics page's media tiles count them.
    let overview = server.admin.get("/api/v1/statistics/overview").await;
    assert_eq!(overview["media_count"], UPLOADS, "{overview}");
    assert!(overview["media_bytes"].as_u64().unwrap() > 0, "{overview}");
    let mut stream = server.events(&["task.*", "media.*"]).await;

    // Everything unused since a date to come: every upload.
    let before = "2999-01-01T00:00:00Z";
    let task = server
        .admin
        .expect(
            Method::POST,
            "/api/v1/media/delete",
            Some(json!({"before": before})),
            StatusCode::ACCEPTED,
        )
        .await;
    assert_eq!(task["status"], "running", "{task}");
    assert_eq!(task["action"], "media.delete");
    let id = task["id"].as_str().unwrap().to_owned();
    let started = stream.next_of("media.deletion_started").await;
    assert_eq!(started["data"]["selected"], UPLOADS, "{started}");

    // Its progress is on the event stream as it goes; cancel it once some is done.
    loop {
        let event = stream.next_of("task.changed").await;
        assert_eq!(event["data"]["id"], id.as_str(), "{event}");
        assert_eq!(event["data"]["status"], "running", "{event}");
        if event["data"]["progress"]["current"].as_u64().unwrap_or(0) >= 1 {
            assert_eq!(event["data"]["progress"]["total"], UPLOADS, "{event}");
            assert_eq!(event["data"]["progress"]["unit"], "items", "{event}");
            break;
        }
    }
    let cancelled = server
        .admin
        .expect(
            Method::POST,
            &format!("/api/v1/tasks/{id}/cancel"),
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    let left = local_media(&server).await;
    assert!(left > 0 && left < UPLOADS, "stopped midway: {left} left");
    // It stays stopped, and stays cancelled.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(local_media(&server).await, left);
    let fetched = server.admin.get(&format!("/api/v1/tasks/{id}")).await;
    assert_eq!(fetched["status"], "cancelled", "{fetched}");

    // The same deletion again runs to the end and says what it did.
    let task = server
        .admin
        .expect(
            Method::POST,
            "/api/v1/media/delete",
            Some(json!({"before": before})),
            StatusCode::ACCEPTED,
        )
        .await;
    let second = task["id"].as_str().unwrap().to_owned();
    let deleted = stream.next_of("media.deleted").await;
    assert_eq!(deleted["resource"]["id"], second.as_str(), "{deleted}");
    assert_eq!(deleted["data"]["deleted_count"], left, "{deleted}");
    let finished = loop {
        let event = stream.next_of("task.changed").await;
        if event["data"]["id"] == second.as_str() && event["data"]["status"] != "running" {
            break event["data"].clone();
        }
    };
    assert_eq!(finished["status"], "succeeded", "{finished}");
    assert_eq!(finished["result"]["deleted_count"], left, "{finished}");
    assert_eq!(finished["progress"]["current"], left, "{finished}");
    assert_eq!(local_media(&server).await, 0);

    // Audited as the 202 each request was answered with.
    let audit = server
        .admin
        .get("/api/v1/audit-log?action=media.delete_bulk")
        .await;
    let entries = audit["items"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{audit}");
    assert!(
        entries.iter().all(|e| e["outcome"]["status"] == 202),
        "{audit}"
    );
    // And counted.
    assert_eq!(
        metric(
            &server,
            "hs_admin_tasks_total{action=\"media.delete\",status=\"cancelled\"}"
        )
        .await,
        Some(1.0)
    );
    assert_eq!(
        metric(
            &server,
            "hs_admin_tasks_total{action=\"media.delete\",status=\"succeeded\"}"
        )
        .await,
        Some(1.0)
    );
    assert_eq!(metric(&server, "hs_admin_tasks_running ").await, Some(0.0));
}

// ---------------------------------------------------------------------------------------------
// Federation: keys and shared rooms
// ---------------------------------------------------------------------------------------------

/// The task `id` on `server`, once it has ended (polled; panics after 20 seconds).
async fn settled_task(server: &Server, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let task = server.admin.get(&format!("/api/v1/tasks/{id}")).await;
        if task["status"] != "running" && task["status"] != "scheduled" {
            return task;
        }
        assert!(Instant::now() < deadline, "task {id} never ended: {task}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_refresh_fetches_from_the_other_server_and_shared_rooms_are_listed() {
    let a = Server::start().await;
    let b = Server::start().await;

    // B's own keys, as B's administrator sees them.
    let own = b.admin.get("/api/v1/federation/keys").await;
    let own = own.as_array().unwrap();
    assert_eq!(own.len(), 1, "{own:?}");
    let key_id = own[0]["key_id"].as_str().unwrap().to_owned();
    assert!(key_id.starts_with("ed25519:"), "{key_id}");
    assert_eq!(own[0]["old"], false);

    // A has not needed B's keys yet.
    let keys_of_b = format!("/api/v1/federation/keys/{}", escape(&b.name));
    a.admin
        .expect(Method::GET, &keys_of_b, None, StatusCode::NOT_FOUND)
        .await;

    // A refresh fetches them from B.
    let mut stream = a.events(&["task.*", "federation.*"]).await;
    let task = a
        .admin
        .expect(
            Method::POST,
            &format!("{keys_of_b}/refresh"),
            None,
            StatusCode::ACCEPTED,
        )
        .await;
    assert_eq!(task["action"], "federation.refetch_keys", "{task}");
    assert_eq!(task["resource"]["id"], b.name.as_str(), "{task}");
    let refreshed = stream.next_of("federation.keys_refreshed").await;
    assert_eq!(refreshed["resource"]["id"], b.name.as_str(), "{refreshed}");
    let task = settled_task(&a, task["id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "succeeded", "{task}");
    assert_eq!(
        task["result"]["keys"][0]["key_id"],
        key_id.as_str(),
        "{task}"
    );
    assert_eq!(
        task["result"]["keys"][0]["public_key"],
        own[0]["public_key"]
    );
    let cached = a.admin.get(&keys_of_b).await;
    assert_eq!(cached["server_name"], b.name.as_str());
    assert_eq!(cached["keys"][0]["key_id"], key_id.as_str(), "{cached}");
    assert!(cached["cached_at"].is_string(), "{cached}");
    assert!(cached["keys"][0]["valid_until_at"].is_string(), "{cached}");

    // A server nobody answers for: the task fails and says so.
    let nowhere = format!("127.0.0.1:{}", reserve_port());
    let task = a
        .admin
        .expect(
            Method::POST,
            &format!("/api/v1/federation/keys/{}/refresh", escape(&nowhere)),
            None,
            StatusCode::ACCEPTED,
        )
        .await;
    let task = settled_task(&a, task["id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "failed", "{task}");
    let audit = a
        .admin
        .get("/api/v1/audit-log?action=federation.keys.refresh")
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 2, "{audit}");

    // Alice on A makes a room; Bob on B joins it. A lists it as shared with B.
    let (_alice_id, alice) = a.user("alice").await;
    let (_bob_id, bob) = b.user("bob").await;
    let created = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "public_chat", "name": "across", "room_version": "11"})),
            StatusCode::OK,
        )
        .await;
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    bob.expect(
        Method::POST,
        &format!(
            "/_matrix/client/v3/join/{}?server_name={}",
            escape(&room_id),
            escape(&a.name)
        ),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    let rooms_of_b = format!(
        "/api/v1/federation/destinations/{}/rooms?include_total=true",
        escape(&b.name)
    );
    let shared = a.admin.get(&rooms_of_b).await;
    let items = shared["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{shared}");
    assert_eq!(items[0]["room_id"], room_id.as_str());
    assert_eq!(items[0]["name"], "across");
    assert_eq!(items[0]["joined_members_count"], 2, "{shared}");
    assert_eq!(items[0]["destination_members_count"], 1, "{shared}");
    // And a server A has never heard of shares nothing with it.
    a.admin
        .expect(
            Method::GET,
            "/api/v1/federation/destinations/nobody.example/rooms",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;

    assert_eq!(
        metric(
            &a,
            "hs_admin_tasks_total{action=\"federation.refetch_keys\",status=\"succeeded\"}"
        )
        .await,
        Some(1.0)
    );
    assert_eq!(
        metric(
            &a,
            "hs_admin_tasks_total{action=\"federation.refetch_keys\",status=\"failed\"}"
        )
        .await,
        Some(1.0)
    );
}
