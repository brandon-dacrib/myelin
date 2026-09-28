//! Invite links, reports and server notices through the real server, as the management
//! interface's Users page and Reports page drive them: the cases `invites_and_notices.rs` and
//! `reports_tasks_statistics.rs` leave out. Ported from the superseded
//! `worktree-agent-aafb071194d2144c6` branch's `admin_areas.rs` and fitted to main's handlers.
//!
//! - An invite link (a registration token) is on the audit record when made and when withdrawn,
//!   the person it admits signs in with the password they chose, a withdrawn link admits nobody,
//!   and a registration attempt against a session that never existed does not use up a place.
//! - A report from somebody who cannot see the message, or about a user who does not exist, is
//!   `404`; the kind filter separates event and user reports; a decision moves the Overview's
//!   count at once, `no_action` dismisses, and a deleted report is gone.
//! - A server notice sent the way the interface sends it (`POST /api/v1/server-notices`, from a
//!   user's page) invites the person into a room named "Server Notices" from the server-notices
//!   user, where they read it, and the send is audited against the notice.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn test_config(data_dir: &std::path::Path, registration: bool) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: {registration}\n",
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
}

/// A party that calls the server: the administrator, a user, or nobody (`token: None`).
#[derive(Clone)]
struct Caller {
    base: String,
    token: Option<String>,
}

impl Caller {
    /// The status and the JSON body (`Null` for none).
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

    /// `path`, expecting `expected`; the body.
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

    /// `GET path`, expecting `200`.
    async fn get(&self, path: &str) -> Value {
        self.expect(Method::GET, path, None, StatusCode::OK).await
    }

    /// `GET /sync` until `wanted` holds of the response (an initial sync each time, so a
    /// change in membership shows as the room's current section).
    async fn sync_until(&self, wanted: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        for _ in 0..80 {
            let response = self.get("/_matrix/client/v3/sync?timeout=0").await;
            if wanted(&response) {
                return response;
            }
            last = response;
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        panic!("the sync never said what was expected; the last one said: {last}");
    }
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('$', "%24")
}

/// The server, its first administrator (`@ops`), and `users` registered through the client
/// API (so `registration` has to be open when any are asked for).
async fn boot(
    dir: &std::path::Path,
    registration: bool,
    users: &[&str],
) -> (hs_cli::serve::ServeHandle, Caller, Vec<Caller>) {
    let handle = hs_cli::serve::spawn_serve(
        test_config(dir, registration),
        hs_cli::serve::ServeOptions::default(),
    )
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
    let admin = Caller {
        base: handle.base_url(),
        token: Some(session["access_token"].as_str().unwrap().to_owned()),
    };
    let mut callers = Vec::new();
    for name in users {
        let registered = nobody
            .expect(
                Method::POST,
                "/_matrix/client/v3/register",
                Some(json!({"username": name, "password": format!("hunter2-{name}"), "auth": {"type": "m.login.dummy"}})),
                StatusCode::OK,
            )
            .await;
        callers.push(Caller {
            base: handle.base_url(),
            token: Some(registered["access_token"].as_str().unwrap().to_owned()),
        });
    }
    (handle, admin, callers)
}

fn register_with(username: &str, token: &str) -> Value {
    json!({
        "username": username,
        "password": format!("hunter2-{username}"),
        "auth": {"type": "m.login.registration_token", "token": token},
    })
}

async fn is_valid(nobody: &Caller, token: &str) -> bool {
    let body = nobody
        .get(&format!(
            "/_matrix/client/v1/register/m.login.registration_token/validity?token={token}"
        ))
        .await;
    body["valid"].as_bool().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invite_link_is_on_the_record_signs_its_person_in_and_can_be_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, _) = boot(dir.path(), false, &[]).await;
    let nobody = Caller {
        base: handle.base_url(),
        token: None,
    };

    // What "Invite by link" on the Users page makes: one use, generated.
    let created = admin
        .expect(
            Method::POST,
            "/api/v1/registration-tokens",
            Some(json!({"uses_allowed": 1})),
            StatusCode::CREATED,
        )
        .await;
    let token = created["token"].as_str().unwrap().to_owned();
    assert_eq!(created["pending"], 0, "{created}");
    assert_eq!(created["completed"], 0, "{created}");

    // Made, and on the record: who made which token.
    let audit = admin
        .get("/api/v1/audit-log?action=registration_tokens.create")
        .await;
    assert_eq!(audit["items"][0]["target"]["id"], token.as_str(), "{audit}");
    assert_eq!(audit["items"][0]["actor"]["id"], "@ops:example.org");

    // A registration naming a session that never existed is refused, and takes no place.
    let mut stale = register_with("frank", &token);
    stale["auth"]["session"] = json!("never-existed");
    let (status, body) = nobody
        .call(Method::POST, "/_matrix/client/v3/register", Some(stale))
        .await;
    assert!(status.is_client_error(), "{status}: {body}");
    assert!(
        is_valid(&nobody, &token).await,
        "the one place is still free"
    );
    let after = admin
        .get(&format!("/api/v1/registration-tokens/{token}"))
        .await;
    assert_eq!(after["pending"], 0, "{after}");
    assert_eq!(after["completed"], 0, "{after}");

    // The invited person registers with the password they chose, and signs in with it.
    let registered = nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(register_with("carol", &token)),
            StatusCode::OK,
        )
        .await;
    assert_eq!(registered["user_id"], "@carol:example.org");
    nobody
        .expect(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({
                "type": "m.login.password",
                "identifier": {"type": "m.id.user", "user": "carol"},
                "password": "hunter2-carol",
            })),
            StatusCode::OK,
        )
        .await;
    let carol = admin.get("/api/v1/users/@carol:example.org").await;
    assert_eq!(carol["user_id"], "@carol:example.org", "{carol}");

    // A second link, withdrawn before anybody used it: gone, invalid, and admits nobody.
    let withdrawn = admin
        .expect(
            Method::POST,
            "/api/v1/registration-tokens",
            Some(json!({"uses_allowed": 1})),
            StatusCode::CREATED,
        )
        .await;
    let withdrawn = withdrawn["token"].as_str().unwrap().to_owned();
    assert!(is_valid(&nobody, &withdrawn).await);
    admin
        .expect(
            Method::DELETE,
            &format!("/api/v1/registration-tokens/{withdrawn}"),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    admin
        .expect(
            Method::GET,
            &format!("/api/v1/registration-tokens/{withdrawn}"),
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    assert!(!is_valid(&nobody, &withdrawn).await);
    let (status, _) = nobody
        .call(
            Method::POST,
            "/_matrix/client/v3/register",
            Some(register_with("dave", &withdrawn)),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    admin
        .expect(
            Method::GET,
            "/api/v1/users/@dave:example.org",
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    let audit = admin
        .get("/api/v1/audit-log?action=registration_tokens.delete")
        .await;
    assert_eq!(
        audit["items"][0]["target"]["id"],
        withdrawn.as_str(),
        "{audit}"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_from_strangers_are_not_found_and_decisions_move_the_overview_count() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), true, &["alice", "bob", "carol"]).await;
    let (alice, bob, carol) = (&users[0], &users[1], &users[2]);

    let created = alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "private_chat", "invite": ["@bob:example.org"]})),
            StatusCode::OK,
        )
        .await;
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    bob.expect(
        Method::POST,
        &format!("/_matrix/client/v3/rooms/{}/join", escape(&room_id)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    let sent = alice
        .expect(
            Method::PUT,
            &format!(
                "/_matrix/client/v3/rooms/{}/send/m.room.message/t1",
                escape(&room_id)
            ),
            Some(json!({"msgtype": "m.text", "body": "buy my coin"})),
            StatusCode::OK,
        )
        .await;
    let event_id = sent["event_id"].as_str().unwrap().to_owned();
    let report_path = format!(
        "/_matrix/client/v3/rooms/{}/report/{}",
        escape(&room_id),
        escape(&event_id)
    );

    // Nothing reported yet: the Overview says zero, not nothing.
    let overview = admin.get("/api/v1/statistics/overview").await;
    assert_eq!(overview["pending_reports_count"], 0, "{overview}");

    // Carol is not in the room, so to her the message does not exist: the same 404 an unknown
    // event gets, so a report cannot be used to probe a private room.
    carol
        .expect(
            Method::POST,
            &report_path,
            Some(json!({"reason": "spam", "score": -80})),
            StatusCode::NOT_FOUND,
        )
        .await;
    // Nor can anybody report a local user who does not exist.
    bob.expect(
        Method::POST,
        "/_matrix/client/v3/users/%40nobody%3Aexample.org/report",
        Some(json!({"reason": "?"})),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert!(
        admin.get("/api/v1/reports").await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    bob.expect(
        Method::POST,
        &report_path,
        Some(json!({"reason": "Unsolicited crypto spam", "score": -80})),
        StatusCode::OK,
    )
    .await;
    bob.expect(
        Method::POST,
        "/_matrix/client/v3/users/%40alice%3Aexample.org/report",
        Some(json!({"reason": "Keeps doing it"})),
        StatusCode::OK,
    )
    .await;

    // The kind filter separates them.
    let events = admin.get("/api/v1/reports?kind=event").await;
    let events = events["items"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["event_id"], event_id.as_str());
    let event_report = events[0]["id"].as_str().unwrap().to_owned();
    let people = admin.get("/api/v1/reports?kind=user").await;
    let people = people["items"].as_array().unwrap();
    assert_eq!(people.len(), 1, "{people:?}");
    assert_eq!(people[0]["reported_user_id"], "@alice:example.org");
    let user_report = people[0]["id"].as_str().unwrap().to_owned();

    let overview = admin.get("/api/v1/statistics/overview").await;
    assert_eq!(overview["pending_reports_count"], 2, "{overview}");

    // One resolved, one dismissed: the count drops at once, nothing is left open.
    let resolved = admin
        .expect(
            Method::POST,
            &format!("/api/v1/reports/{event_report}/resolve"),
            Some(json!({"resolution": "redacted", "note": "Removed the message"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(resolved["status"], "resolved");
    let dismissed = admin
        .expect(
            Method::POST,
            &format!("/api/v1/reports/{user_report}/resolve"),
            Some(json!({"resolution": "no_action"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(dismissed["status"], "dismissed", "{dismissed}");
    let overview = admin.get("/api/v1/statistics/overview").await;
    assert_eq!(overview["pending_reports_count"], 0, "{overview}");
    assert!(
        admin.get("/api/v1/reports?status=open").await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let listed = admin.get("/api/v1/reports?status=dismissed").await;
    assert_eq!(listed["items"][0]["id"], user_report.as_str(), "{listed}");
    let audit = admin.get("/api/v1/audit-log?action=reports.resolve").await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 2, "{audit}");

    // Deleted is gone; the other keeps its note.
    admin
        .expect(
            Method::DELETE,
            &format!("/api/v1/reports/{user_report}"),
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    admin
        .expect(
            Method::GET,
            &format!("/api/v1/reports/{user_report}"),
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
    let audit = admin.get("/api/v1/audit-log?action=reports.delete").await;
    assert_eq!(
        audit["items"][0]["target"]["id"],
        user_report.as_str(),
        "{audit}"
    );
    let kept = admin.get(&format!("/api/v1/reports/{event_report}")).await;
    assert_eq!(kept["resolution_note"], "Removed the message");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notice_sent_as_the_interface_sends_it_reaches_the_person_and_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, users) = boot(dir.path(), true, &["alice"]).await;
    let alice = &users[0];

    // The body a user's "Send notice" posts: one recipient, a plain-text message.
    let sent = admin
        .expect(
            Method::POST,
            "/api/v1/server-notices",
            Some(json!({
                "recipients": ["@alice:example.org"],
                "content": {"msgtype": "m.text", "body": "Maintenance tonight at 22:00"},
            })),
            StatusCode::CREATED,
        )
        .await;
    let notice_id = sent["id"].as_str().unwrap().to_owned();
    assert_eq!(sent["recipients"], json!(["@alice:example.org"]), "{sent}");
    assert_eq!(sent["sender"], "@_server:example.org", "{sent}");
    let room_id = sent["room_ids"][0].as_str().unwrap().to_owned();
    let event_id = sent["event_ids"][0].as_str().unwrap().to_owned();

    // Alice's client sees an invitation to "Server Notices" from the server-notices user, in
    // the room the send answered with.
    let synced = alice
        .sync_until(|r| r["rooms"]["invite"][&room_id].is_object())
        .await;
    let invite_state = synced["rooms"]["invite"][&room_id]["invite_state"]["events"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        invite_state
            .iter()
            .any(|e| e["type"] == "m.room.name" && e["content"]["name"] == "Server Notices"),
        "{invite_state:?}"
    );
    assert!(
        invite_state
            .iter()
            .any(|e| e["type"] == "m.room.member" && e["sender"] == "@_server:example.org"),
        "{invite_state:?}"
    );

    // She joins and reads it.
    alice
        .expect(
            Method::POST,
            &format!("/_matrix/client/v3/rooms/{}/join", escape(&room_id)),
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
    let joined = alice
        .sync_until(|r| {
            r["rooms"]["join"][&room_id]["timeline"]["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|e| e["event_id"] == event_id.as_str()))
        })
        .await;
    let notice = joined["rooms"]["join"][&room_id]["timeline"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_id"] == event_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(notice["content"]["body"], "Maintenance tonight at 22:00");
    assert_eq!(notice["sender"], "@_server:example.org");

    // On the record: who sent which notice, and the notice in the history.
    let audit = admin
        .get("/api/v1/audit-log?action=server_notices.send")
        .await;
    let entry = &audit["items"][0];
    assert_eq!(entry["target"]["type"], "server_notice", "{audit}");
    assert_eq!(entry["target"]["id"], notice_id.as_str(), "{audit}");
    assert_eq!(entry["actor"]["id"], "@ops:example.org", "{audit}");
    let history = admin.get("/api/v1/server-notices").await;
    assert_eq!(history["items"][0]["id"], notice_id.as_str(), "{history}");

    handle.shutdown().await;
}
