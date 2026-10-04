//! An administrator changing how a user appears, through the real server: `users.update` with
//! `display_name`, `avatar_url` and `user_type`, checked by what the person and the people in
//! their rooms see, not only by what the admin API answers.
//!
//! Alice registers, names herself, makes a room and bob joins it. An administrator renames her,
//! gives her an avatar and marks the account a bot. Then: her `/profile` says the new name and
//! avatar; the room's `m.room.member` event for her, as bob reads it, carries them (the same
//! re-send her own `PUT /profile` would cause, so bob's `/sync` and other servers learn of it);
//! the admin API says `bot`; the audit log names each field. Clearing the name with `null`
//! empties her profile and her membership event. A bad avatar or kind is refused by field,
//! `users.availability` answers for a taken, free and impossible name, and `users.create` with a
//! kind records it.

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
}

fn escape(id: &str) -> String {
    id.replace('!', "%21")
        .replace('#', "%23")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('$', "%24")
}

const ALICE: &str = "@alice:example.org";

/// The server, its first administrator, alice (named "Alice" by herself) and bob, both in a
/// public room alice made.
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
                Some(json!({
                    "username": name,
                    "password": format!("hunter2-{name}"),
                    "auth": {"type": "m.login.dummy"},
                })),
                StatusCode::OK,
            )
            .await;
        users.push(nobody.with_token(registered["access_token"].as_str().unwrap()));
    }
    let (alice, bob) = (users[0].clone(), users[1].clone());
    alice
        .expect(
            Method::PUT,
            &format!("/_matrix/client/v3/profile/{}/displayname", escape(ALICE)),
            Some(json!({"displayname": "Alice"})),
            StatusCode::OK,
        )
        .await;
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

/// Alice's `m.room.member` event in `room` as bob reads it, once its `displayname` is
/// `expected` (the re-send after a profile change is asynchronous), or the last one seen after
/// ten seconds.
async fn member_once_named(bob: &Caller, room: &str, expected: Option<&str>) -> Value {
    let path = format!(
        "/_matrix/client/v3/rooms/{}/state/m.room.member/{}",
        escape(room),
        escape(ALICE)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let member = bob.get(&path).await;
        if member["displayname"].as_str() == expected || std::time::Instant::now() > deadline {
            return member;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_administrators_profile_change_reaches_the_user_and_their_rooms() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, admin, _alice, bob, room) = boot(dir.path()).await;
    let alice_path = format!("/api/v1/users/{}", escape(ALICE));

    // Before: her own name, in her profile and in the room.
    let member = member_once_named(&bob, &room, Some("Alice")).await;
    assert_eq!(member["displayname"], "Alice", "{member}");
    assert!(member.get("avatar_url").is_none(), "{member}");

    // ---- users.update: name, avatar and kind ----
    let updated = admin
        .expect(
            Method::PATCH,
            &alice_path,
            Some(json!({
                "display_name": "Alice Liddell",
                "avatar_url": "mxc://example.org/alice",
                "user_type": "bot",
            })),
            StatusCode::OK,
        )
        .await;
    assert_eq!(updated["display_name"], "Alice Liddell", "{updated}");
    assert_eq!(
        updated["avatar_url"], "mxc://example.org/alice",
        "{updated}"
    );
    assert_eq!(updated["user_type"], "bot", "{updated}");

    // Her profile, as any client reads it.
    let profile = bob
        .get(&format!("/_matrix/client/v3/profile/{}", escape(ALICE)))
        .await;
    assert_eq!(profile["displayname"], "Alice Liddell", "{profile}");
    assert_eq!(
        profile["avatar_url"], "mxc://example.org/alice",
        "{profile}"
    );

    // The room: her membership event was re-sent with the new name and avatar, which is what
    // bob's /sync and other servers see.
    let member = member_once_named(&bob, &room, Some("Alice Liddell")).await;
    assert_eq!(member["displayname"], "Alice Liddell", "{member}");
    assert_eq!(member["avatar_url"], "mxc://example.org/alice", "{member}");
    assert_eq!(member["membership"], "join", "{member}");

    // The admin API, read back.
    let user = admin.get(&alice_path).await;
    assert_eq!(user["user_type"], "bot", "{user}");
    assert_eq!(user["display_name"], "Alice Liddell", "{user}");

    // On the record, each field by name.
    let audit = admin.get("/api/v1/audit-log?action=users.update").await;
    let entry = &audit["items"][0];
    assert_eq!(entry["target"]["id"], ALICE, "{audit}");
    assert_eq!(entry["actor"]["id"], "@ops:example.org", "{audit}");
    let mut pointers: Vec<&str> = entry["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointer"].as_str().unwrap())
        .collect();
    pointers.sort_unstable();
    assert_eq!(
        pointers,
        ["/avatar_url", "/display_name", "/user_type"],
        "{audit}"
    );

    // ---- clearing the name with null ----
    let cleared = admin
        .expect(
            Method::PATCH,
            &alice_path,
            Some(json!({"display_name": null})),
            StatusCode::OK,
        )
        .await;
    assert!(cleared["display_name"].is_null(), "{cleared}");
    assert_eq!(
        cleared["avatar_url"], "mxc://example.org/alice",
        "{cleared}"
    );
    let profile = bob
        .get(&format!("/_matrix/client/v3/profile/{}", escape(ALICE)))
        .await;
    assert!(profile.get("displayname").is_none(), "{profile}");
    let member = member_once_named(&bob, &room, None).await;
    assert!(member.get("displayname").is_none(), "{member}");
    assert_eq!(member["avatar_url"], "mxc://example.org/alice", "{member}");

    // ---- refusals, by field, before anything is written ----
    let (status, problem) = admin
        .call(
            Method::PATCH,
            &alice_path,
            Some(json!({"avatar_url": "https://example.org/alice.png", "user_type": "bot"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["errors"][0]["pointer"], "/avatar_url", "{problem}");
    let (status, problem) = admin
        .call(
            Method::PATCH,
            &alice_path,
            Some(json!({"user_type": "wizard", "display_name": "Wiz"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["errors"][0]["pointer"], "/user_type", "{problem}");
    let user = admin.get(&alice_path).await;
    assert!(user["display_name"].is_null(), "{user}");
    assert_eq!(user["user_type"], "bot", "{user}");

    // ---- users.availability ----
    for (localpart, available) in [("alice", false), ("Alice", false), ("carol", true)] {
        let body = admin
            .get(&format!("/api/v1/users/availability?localpart={localpart}"))
            .await;
        assert_eq!(body["available"], available, "{localpart}: {body}");
    }
    let (status, problem) = admin
        .call(
            Method::GET,
            "/api/v1/users/availability?localpart=not%20a%20name",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["errors"][0]["pointer"], "param:localpart",
        "{problem}"
    );
    assert!(
        problem["errors"][0]["detail"].as_str().unwrap_or("").len() > 10,
        "{problem}"
    );

    // ---- users.create records the kind ----
    let created = admin
        .expect(
            Method::POST,
            "/api/v1/users",
            Some(json!({"localpart": "helpdesk", "password": "hunter2-helpdesk", "user_type": "support"})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(created["user_type"], "support", "{created}");
    let helpdesk = admin
        .get(&format!(
            "/api/v1/users/{}",
            escape("@helpdesk:example.org")
        ))
        .await;
    assert_eq!(helpdesk["user_type"], "support", "{helpdesk}");
    let body = admin
        .get("/api/v1/users/availability?localpart=helpdesk")
        .await;
    assert_eq!(body["available"], false, "{body}");

    drop(handle);
}
