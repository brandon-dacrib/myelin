//! Invite links and server notices through the real server: a registration token made in the
//! admin API lets somebody register while open registration is off, and a server notice reaches
//! its recipient the way the Matrix specification (and Complement's `TestServerNotices`)
//! describes.

use serde_json::{Value, json};

fn test_config(data_dir: &std::path::Path) -> hs_config::Config {
    let media_dir = data_dir.join("media");
    let yaml = format!(
        "server:\n  server_name: example.org\n\
         listeners:\n  listeners:\n    - port: 1\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\n\
         storage:\n  backend: embedded\n  data_dir: {:?}\n\
         media:\n  storage:\n    backend: local\n    path: {:?}\n\
         auth:\n  enable_registration: false\n",
        data_dir, media_dir
    );
    let mut config = hs_config::Config::from_yaml(&yaml).unwrap();
    config.listeners.listeners[0].port = 0;
    config
}

fn setup_token_of(link: &str) -> &str {
    link.split_once("/admin/setup#token=")
        .unwrap_or_else(|| panic!("not a setup link: {link}"))
        .1
}

/// Calls `base` and answers the status and the JSON body (`Null` for none).
async fn call(
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let mut request = reqwest::Client::new().request(method, format!("{base}{path}"));
    if let Some(token) = token {
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

async fn first_admin(handle: &hs_cli::serve::ServeHandle) -> String {
    let token = setup_token_of(handle.setup_link.as_deref().unwrap()).to_owned();
    let (status, admin) = call(
        &handle.base_url(),
        reqwest::Method::POST,
        "/api/v1/setup",
        None,
        Some(json!({"setup_token": token, "username": "ops", "password": "hunter2-first-admin"})),
    )
    .await;
    assert!(status.is_success(), "{admin}");
    admin["access_token"].as_str().unwrap().to_owned()
}

fn register_with(username: &str, token: &str) -> Value {
    json!({
        "username": username,
        "password": format!("hunter2-{username}"),
        "auth": {"type": "m.login.registration_token", "token": token},
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invite_link_token_registers_as_many_people_as_it_allows_on_a_closed_server() {
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        test_config(dir.path()),
        hs_cli::serve::ServeOptions::default(),
    )
    .await
    .expect("server should boot");
    let base = handle.base_url();
    let admin = first_admin(&handle).await;
    use reqwest::Method;

    // Closed, and no token out: the front door is shut as it always was.
    let (status, _) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/register",
        None,
        Some(json!({"username": "dana", "password": "hunter2-dana"})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    let (status, created) = call(
        &base,
        Method::POST,
        "/api/v1/registration-tokens",
        Some(&admin),
        Some(json!({"uses_allowed": 1})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 16);

    let validity = |token: String| {
        let base = base.clone();
        async move {
            call(
                &base,
                Method::GET,
                &format!(
                    "/_matrix/client/v1/register/m.login.registration_token/validity?token={token}"
                ),
                None,
                None,
            )
            .await
        }
    };
    let (status, valid) = validity(token.clone()).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(valid, json!({"valid": true}));

    // With a token out, a client asking how to register is told to ask for one.
    let (status, flows) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/register",
        None,
        Some(json!({"username": "dana", "password": "hunter2-dana"})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{flows}");
    assert_eq!(
        flows["flows"][0]["stages"],
        json!(["m.login.registration_token"])
    );

    let (status, registered) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/register",
        None,
        Some(register_with("dana", &token)),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{registered}");
    assert_eq!(registered["user_id"], "@dana:example.org");
    assert!(registered["access_token"].as_str().is_some());

    // One use allowed, one used.
    let (_, after) = call(
        &base,
        Method::GET,
        &format!("/api/v1/registration-tokens/{token}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(after["completed"], 1, "{after}");
    assert_eq!(after["pending"], 0);
    assert_eq!(after["valid"], false);
    assert_eq!(validity(token.clone()).await.1, json!({"valid": false}));
    let (status, _) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/register",
        None,
        Some(register_with("eve", &token)),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);

    // Raising the limit reopens it; who did that is on the record.
    let (status, _) = call(
        &base,
        Method::PATCH,
        &format!("/api/v1/registration-tokens/{token}"),
        Some(&admin),
        Some(json!({"uses_allowed": 2})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (_, audit) = call(
        &base,
        Method::GET,
        "/api/v1/audit-log?action=registration_tokens.update",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(audit["items"][0]["actor"]["id"], "@ops:example.org");

    let (status, _) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/register",
        None,
        Some(register_with("eve", &token)),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (_, page) = call(
        &base,
        Method::GET,
        "/api/v1/registration-tokens",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(page["items"][0]["token"], token.as_str(), "{page}");
    assert_eq!(page["items"][0]["completed"], 2);
    assert_eq!(page["items"][0]["valid"], false);

    handle.shutdown().await;
}

/// `GET /sync` (incremental from `since`, if given) until `wanted` holds of the response.
async fn sync_until(
    base: &str,
    token: &str,
    mut since: Option<String>,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    for _ in 0..40 {
        let mut path = "/_matrix/client/v3/sync?timeout=1000".to_owned();
        if let Some(since) = &since {
            path.push_str(&format!("&since={since}"));
        }
        let (_, response) = call(base, reqwest::Method::GET, &path, Some(token), None).await;
        if wanted(&response) {
            return response;
        }
        if since.is_some() {
            since = response["next_batch"].as_str().map(str::to_owned);
        }
        last = response;
    }
    panic!("the sync never said what was expected; the last one said: {last}");
}

/// The invitation, if `response` has one from the server-notices user: Complement's
/// `syncUntilInvite` looks at `invite_state.events.0.sender`.
fn notices_invite(response: &Value) -> Option<String> {
    response["rooms"]["invite"]
        .as_object()?
        .iter()
        .find(|(_, room)| room["invite_state"]["events"][0]["sender"] == "@_server:example.org")
        .map(|(room_id, _)| room_id.clone())
}

/// Complement's `TestServerNotices`, step for step, through the Synapse-compatible endpoint it
/// uses, and then the native one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_notice_reaches_its_recipient_as_complement_expects() {
    use reqwest::Method;
    let dir = tempfile::tempdir().unwrap();
    let handle = hs_cli::serve::spawn_serve(
        test_config(dir.path()),
        hs_cli::serve::ServeOptions::default(),
    )
    .await
    .expect("server should boot");
    let base = handle.base_url();
    let admin = first_admin(&handle).await;
    let (_, created) = call(
        &base,
        Method::POST,
        "/api/v1/registration-tokens",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    let (status, alice) = call(
        &base,
        Method::POST,
        "/_matrix/client/v3/register",
        None,
        Some(register_with("alice", created["token"].as_str().unwrap())),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{alice}");
    let alice = alice["access_token"].as_str().unwrap().to_owned();

    let notice = json!({
        "user_id": "@alice:example.org",
        "content": {"msgtype": "m.text", "body": "hello from server notices!"},
    });
    let send = |token: String, txn: Option<&str>| {
        let (base, notice) = (base.clone(), notice.clone());
        let (method, path) = match txn {
            Some(txn) => (
                Method::PUT,
                format!("/_synapse/admin/v1/send_server_notice/{txn}"),
            ),
            None => (
                Method::POST,
                "/_synapse/admin/v1/send_server_notice".to_owned(),
            ),
        };
        async move { call(&base, method, &path, Some(&token), Some(notice)).await }
    };

    // "/send_server_notice is not allowed as normal user" (with no body at all).
    let (status, refused) = call(
        &base,
        Method::POST,
        "/_synapse/admin/v1/send_server_notice",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["errcode"], "M_FORBIDDEN");

    // "/send_server_notice as an admin is allowed".
    let (status, sent) = send(admin.clone(), None).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{sent}");
    let event_id = sent["event_id"].as_str().unwrap().to_owned();

    // "Alice is invited to the server alert room".
    let synced = sync_until(&base, &alice, None, |r| notices_invite(r).is_some()).await;
    let room_id = notices_invite(&synced).unwrap();

    // "Alice cannot reject the invite".
    let (status, refused) = call(
        &base,
        Method::POST,
        &format!("/_matrix/client/v3/rooms/{room_id}/leave"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["errcode"], "M_CANNOT_LEAVE_SERVER_NOTICE_ROOM");

    // "Alice can join the alert room", and the notice is in her timeline, and the room is
    // tagged as the server's.
    let since = synced["next_batch"].as_str().map(str::to_owned);
    let (status, _) = call(
        &base,
        Method::POST,
        &format!("/_matrix/client/v3/rooms/{room_id}/join"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let joined = sync_until(&base, &alice, since, |r| {
        r["rooms"]["join"][&room_id]["timeline"]["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|e| e["event_id"] == event_id.as_str()))
    })
    .await;
    let account_data = &joined["rooms"]["join"][&room_id]["account_data"]["events"];
    assert!(
        account_data.as_array().is_some_and(|events| {
            events.iter().any(|e| {
                e["type"] == "m.tag" && e["content"]["tags"].get("m.server_notice").is_some()
            })
        }),
        "{account_data}"
    );

    // "Alice can leave the alert room, after joining it".
    let (status, _) = call(
        &base,
        Method::POST,
        &format!("/_matrix/client/v3/rooms/{room_id}/leave"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);

    // "After leaving the alert room and on re-invitation, no new room is created".
    let (status, _) = send(admin.clone(), None).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let synced = sync_until(&base, &alice, None, |r| notices_invite(r).is_some()).await;
    assert_eq!(notices_invite(&synced).unwrap(), room_id);

    // "Sending a notice with a transactionID is idempotent".
    let (_, first) = send(admin.clone(), Some("1")).await;
    let (_, second) = send(admin.clone(), Some("1")).await;
    assert!(first["event_id"].as_str().is_some(), "{first}");
    assert_eq!(first["event_id"], second["event_id"]);

    // The native history has every send, newest first, and each is on the record.
    let (status, history) = call(
        &base,
        Method::GET,
        "/api/v1/server-notices",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{history}");
    let items = history["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "{history}");
    assert_eq!(items[0]["event_ids"][0], first["event_id"]);
    assert_eq!(items[2]["event_ids"][0], event_id.as_str());
    assert!(items.iter().all(|n| n["room_ids"][0] == room_id.as_str()));
    let (_, audit) = call(
        &base,
        Method::GET,
        "/api/v1/audit-log?action=server_notices.send",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 3, "{audit}");

    // A notice to somebody who does not exist sends nothing, and says who.
    let (status, problem) = call(
        &base,
        Method::POST,
        "/api/v1/server-notices",
        Some(&admin),
        Some(json!({
            "recipients": ["@alice:example.org", "@nobody:example.org"],
            "content": {"msgtype": "m.text", "body": "x"},
        })),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["errors"][0]["pointer"], "/recipients");

    handle.shutdown().await;
}
