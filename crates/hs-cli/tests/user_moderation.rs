//! The moderation and activity half of the Users area, through the real server: each
//! operation's *effect*, not only its answer.
//!
//! - Suspension (MSC3823): a suspended user's send is refused `403 M_USER_SUSPENDED` while their
//!   reads still work; unsuspended, the same send goes through.
//! - Shadow-ban: the banned user's message is answered with an event ID and reaches nobody;
//!   lifted, the next one arrives.
//! - A rate-limit override: over it, `429 M_LIMIT_EXCEEDED`; cleared, sending works again.
//! - Login-as: the minted token acts as the user, is listed among their sessions as a support
//!   session, and appears in no audit entry.
//! - Redacting a user's events runs as a task whose result counts them, and the other member
//!   then sees them redacted.
//! - Memberships, statistics and media: the room, the counts and the upload are there, and
//!   deleting the user's media (a task) removes the upload.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn test_config(data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n",
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
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

    async fn get(&self, path: &str) -> Value {
        self.expect(Method::GET, path, None, StatusCode::OK).await
    }

    /// `PUT /send` of an `m.room.message`; the status and body.
    async fn send(&self, room: &str, txn: &str, text: &str) -> (StatusCode, Value) {
        self.call(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                escape(room)
            ),
            Some(json!({"msgtype": "m.text", "body": text})),
        )
        .await
    }

    /// Every message body in the room, oldest first, as this caller sees it (`None` for a
    /// redacted one).
    async fn bodies(&self, room: &str) -> Vec<Option<String>> {
        let page = self
            .get(&format!(
                "/_matrix/client/v3/rooms/{}/messages?dir=b&limit=100",
                escape(room)
            ))
            .await;
        let mut out: Vec<Option<String>> = page["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "m.room.message")
            .map(|e| e["content"]["body"].as_str().map(str::to_owned))
            .collect();
        out.reverse();
        out
    }
}

/// The value of `hs_room_moderated_writes_total{outcome}` on `/metrics`. The counter is
/// process-wide and this binary's tests share a process, so callers assert a lower bound.
async fn moderated_writes(base: &str, outcome: &str) -> u64 {
    let text = reqwest::get(format!("{base}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let prefix = format!("hs_room_moderated_writes_total{{outcome=\"{outcome}\"}} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map_or(0, |n| n.trim().parse().unwrap())
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('$', "%24")
}

/// The server, its first administrator, and `alice` and `bob` in one public room alice made.
async fn boot(
    dir: &std::path::Path,
) -> (hs_cli::serve::ServeHandle, Caller, Caller, Caller, String) {
    let handle =
        hs_cli::serve::spawn_serve(test_config(dir), hs_cli::serve::ServeOptions::default())
            .await
            .expect("server should boot");
    let nobody = Caller {
        base: handle.base_url(),
        token: None,
    };
    let link = handle
        .setup_link
        .clone()
        .expect("a fresh server offers setup");
    let setup_token = link.split_once("#token=").unwrap().1.to_owned();
    let (status, session) = nobody
        .call(
            Method::POST,
            "/api/v1/setup",
            Some(json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"})),
        )
        .await;
    assert!(status.is_success(), "{status}: {session}");
    let admin = nobody.with_token(session["access_token"].as_str().unwrap());
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        let registered = nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}})),
                StatusCode::OK,
            )
            .await;
        users.push(nobody.with_token(registered["access_token"].as_str().unwrap()));
    }
    let (alice, bob) = (users[0].clone(), users[1].clone());
    let room = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "public_chat", "name": "Lobby"})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/join/{}", escape(&room)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    (handle, admin, alice, bob, room)
}

const ALICE: &str = "%40alice%3Aexample.org";

/// Polls a task until it ends; the task.
async fn finished(admin: &Caller, location: &str) -> Value {
    for _ in 0..200 {
        let task = admin.get(location).await;
        if matches!(
            task["status"].as_str(),
            Some("succeeded" | "failed" | "cancelled")
        ) {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("task {location} never finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suspended_user_reads_but_cannot_send_until_unsuspended() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, bob, room) = boot(dir.path()).await;

    let user = admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE}/suspend"),
            Some(json!({"reason": "spam wave"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(user["suspended"], true, "{user}");

    let (status, refused) = alice.send(&room, "t1", "while suspended").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["errcode"], "M_USER_SUSPENDED");
    assert!(moderated_writes(&admin.base, "suspended").await >= 1);
    // Other writes are refused too, and reads keep working.
    let (status, refused) = alice
        .call(
            Method::PUT,
            "/_matrix/client/v3/profile/%40alice%3Aexample.org/displayname",
            Some(json!({"displayname": "Spam"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let (status, _) = alice
        .call(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(alice.bodies(&room).await.is_empty());

    let audit = admin.get("/api/v1/audit-log?action=users.suspend").await;
    assert_eq!(
        audit["items"][0]["target"]["id"], "@alice:example.org",
        "{audit}"
    );
    assert_eq!(
        admin.get(&format!("/api/v1/users/{ALICE}")).await["suspended"],
        true
    );

    admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE}/unsuspend"),
            None,
            StatusCode::OK,
        )
        .await;
    let (status, sent) = alice.send(&room, "t2", "back again").await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(bob.bodies(&room).await, vec![Some("back again".to_owned())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shadow_banned_users_message_reaches_nobody() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, bob, room) = boot(dir.path()).await;

    admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE}/shadow-ban"),
            None,
            StatusCode::OK,
        )
        .await;
    let (status, sent) = alice.send(&room, "t1", "nobody hears this").await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert!(sent["event_id"].as_str().unwrap().starts_with('$'));
    assert!(
        bob.bodies(&room).await.is_empty(),
        "bob saw a shadow-banned message"
    );
    assert!(moderated_writes(&admin.base, "shadow_banned").await >= 1);
    // An invitation from a shadow-banned user is answered as made, and is not.
    let (status, _) = alice
        .call(
            Method::POST,
            &format!("/_matrix/client/v3/rooms/{}/invite", escape(&room)),
            Some(json!({"user_id": "@ops:example.org"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE}/unshadow-ban"),
            None,
            StatusCode::OK,
        )
        .await;
    let (status, _) = alice.send(&room, "t2", "everybody hears this").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        bob.bodies(&room).await,
        vec![Some("everybody hears this".to_owned())]
    );
    let audit = admin.get("/api/v1/audit-log?action=users.shadow_ban").await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1, "{audit}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limit_override_throttles_until_it_is_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, _bob, room) = boot(dir.path()).await;
    let path = format!("/api/v1/users/{ALICE}/rate-limit");
    assert_eq!(admin.get(&path).await, json!({}));

    let set = admin
        .expect(
            Method::PUT,
            &path,
            Some(json!({"messages_per_second": 0.01, "burst_count": 2})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(set, json!({"messages_per_second": 0.01, "burst_count": 2}));
    assert_eq!(admin.get(&path).await, set);
    for txn in ["a", "b"] {
        let (status, body) = alice.send(&room, txn, txn).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, limited) = alice.send(&room, "c", "one too many").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{limited}");
    assert_eq!(limited["errcode"], "M_LIMIT_EXCEEDED");
    assert!(limited["retry_after_ms"].as_u64().unwrap() > 1000);
    assert!(moderated_writes(&admin.base, "rate_limited").await >= 1);

    admin
        .expect(Method::DELETE, &path, None, StatusCode::NO_CONTENT)
        .await;
    let (status, body) = alice.send(&room, "d", "free again").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let audit = admin
        .get("/api/v1/audit-log?action=users.rate_limit.put")
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1, "{audit}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_as_acts_as_the_user_and_is_on_the_record_without_its_token() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, _bob, _room) = boot(dir.path()).await;
    let minted = admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{ALICE}/login-as"),
            Some(json!({"reason": "ticket 42", "valid_for_seconds": 600})),
            StatusCode::CREATED,
        )
        .await;
    let token = minted["access_token"].as_str().unwrap();
    let support = alice.with_token(token);
    let whoami = support.get("/_matrix/client/v3/account/whoami").await;
    assert_eq!(whoami["user_id"], "@alice:example.org");
    assert_eq!(whoami["device_id"], minted["device_id"]);

    let sessions = admin.get(&format!("/api/v1/users/{ALICE}/sessions")).await;
    let support_sessions: Vec<&Value> = sessions["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["support_session"] == true)
        .collect();
    assert_eq!(support_sessions.len(), 1, "{sessions}");
    assert_eq!(support_sessions[0]["device_id"], minted["device_id"]);

    let audit = admin.get("/api/v1/audit-log?action=users.login_as").await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1, "{audit}");
    assert!(
        !audit.to_string().contains(token),
        "the audit log holds the token"
    );

    // Signing the support session out is signing out a device like any other.
    admin
        .expect(
            Method::DELETE,
            &format!(
                "/api/v1/users/{ALICE}/devices/{}",
                minted["device_id"].as_str().unwrap()
            ),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    let (status, _) = support
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redacting_a_users_events_is_a_task_and_the_room_sees_them_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, bob, room) = boot(dir.path()).await;
    for (txn, text) in [("a", "spam one"), ("b", "spam two"), ("c", "spam three")] {
        let (status, _) = alice.send(&room, txn, text).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _) = bob.send(&room, "b1", "bob's own").await;
    assert_eq!(status, StatusCode::OK);

    let reqwest_response = reqwest::Client::new()
        .post(format!("{}/api/v1/users/{ALICE}/redact-events", admin.base))
        .bearer_auth(admin.token.as_deref().unwrap())
        .json(&json!({"reason": "spam"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest_response.status(), StatusCode::ACCEPTED);
    let location = reqwest_response.headers()["location"]
        .to_str()
        .unwrap()
        .to_owned();
    let task = finished(&admin, &location).await;
    assert_eq!(task["status"], "succeeded", "{task}");
    assert_eq!(task["result"]["redacted"], 3, "{task}");
    assert_eq!(task["progress"]["current"], 3);

    assert_eq!(
        bob.bodies(&room).await,
        vec![None, None, None, Some("bob's own".to_owned())]
    );
    let audit = admin
        .get("/api/v1/audit-log?action=users.redact_events")
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1, "{audit}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memberships_statistics_and_media_describe_the_user_and_media_can_be_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, bob, room) = boot(dir.path()).await;
    let (status, _) = alice.send(&room, "a", "hello").await;
    assert_eq!(status, StatusCode::OK);
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/rooms/{}/leave", escape(&room)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;

    let memberships = admin
        .get(&format!("/api/v1/users/{ALICE}/memberships"))
        .await;
    assert_eq!(
        memberships["items"][0]["room_id"],
        room.as_str(),
        "{memberships}"
    );
    assert_eq!(memberships["items"][0]["membership"], "join");
    assert_eq!(memberships["items"][0]["room_name"], "Lobby");
    let bobs = admin
        .get("/api/v1/users/%40bob%3Aexample.org/memberships?membership=leave")
        .await;
    assert_eq!(bobs["items"].as_array().unwrap().len(), 1, "{bobs}");

    let uploaded: Value = reqwest::Client::new()
        .post(format!(
            "{}/_matrix/media/v3/upload?filename=a.txt",
            alice.base
        ))
        .bearer_auth(alice.token.as_deref().unwrap())
        .header("content-type", "text/plain")
        .body("hello, media")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let media_id = uploaded["content_uri"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();

    let stats = admin
        .get(&format!("/api/v1/users/{ALICE}/statistics"))
        .await;
    assert_eq!(stats["joins_count"], 1, "{stats}");
    assert_eq!(stats["rooms_created_count"], 1);
    assert!(stats["events_sent_count"].as_u64().unwrap() >= 2);
    assert_eq!(stats["media_count"], 1);
    assert_eq!(stats["media_bytes"], 12);
    assert_eq!(stats["session_count"], 1);

    let media = admin.get(&format!("/api/v1/users/{ALICE}/media")).await;
    assert_eq!(media["items"][0]["media_id"], media_id.as_str(), "{media}");

    let response = reqwest::Client::new()
        .delete(format!("{}/api/v1/users/{ALICE}/media", admin.base))
        .bearer_auth(admin.token.as_deref().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    let task = finished(&admin, &location).await;
    assert_eq!(task["result"]["deleted"], 1, "{task}");
    let media = admin.get(&format!("/api/v1/users/{ALICE}/media")).await;
    assert!(media["items"].as_array().unwrap().is_empty(), "{media}");
    let (status, _) = alice
        .call(
            Method::GET,
            &format!("/_matrix/client/v1/media/download/example.org/{media_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
