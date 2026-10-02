//! Erasing a user through the real server: `users.deactivate` with `erase: true`, checked by
//! what it does to the person on the other side.
//!
//! Alice registers, names herself, uploads device keys, makes a room and bob joins it. An
//! administrator deactivates her with `erase`. Then: the admin API says deactivated and erased
//! with no display name; her devices are gone; `/keys/query` from bob's account has no keys for
//! her; the room's state says she left; her old access token is refused; her profile is empty;
//! the audit log holds the erasure under `users.deactivate` with the `/erased` change; and
//! `users.reactivate` answers `409`.

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

/// The server, its first administrator, alice (with a named device and uploaded keys) and bob,
/// both in a public room alice made.
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
                    "device_id": format!("{}_PHONE", name.to_uppercase()),
                    "auth": {"type": "m.login.dummy"},
                })),
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

async fn upload_keys(session: &Caller, device_id: &str) {
    session
        .expect(
            Method::POST,
            "/_matrix/client/v3/keys/upload",
            Some(json!({
                "device_keys": {
                    "user_id": ALICE,
                    "device_id": device_id,
                    "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                    "keys": {
                        format!("curve25519:{device_id}"): format!("curve-{device_id}"),
                        format!("ed25519:{device_id}"): format!("ed-{device_id}"),
                    },
                    "signatures": {ALICE: {format!("ed25519:{device_id}"): "sig"}},
                }
            })),
            StatusCode::OK,
        )
        .await;
}

/// Alice's devices with keys, as `session` sees them through `/keys/query`.
async fn keyed_devices(session: &Caller) -> Vec<String> {
    let body = session
        .expect(
            Method::POST,
            "/_matrix/client/v3/keys/query",
            Some(json!({"device_keys": {ALICE: []}})),
            StatusCode::OK,
        )
        .await;
    let mut devices: Vec<String> = body["device_keys"][ALICE]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    devices.sort();
    devices
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_erased_user_is_gone_from_everywhere_but_the_audit_log() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, bob, room) = boot(dir.path()).await;
    let alice_path = escape(ALICE);

    alice
        .expect(
            Method::PUT,
            &format!("/_matrix/client/v3/profile/{alice_path}/displayname"),
            Some(json!({"displayname": "Alice Liddell"})),
            StatusCode::OK,
        )
        .await;
    upload_keys(&alice, "ALICE_PHONE").await;
    assert_eq!(keyed_devices(&bob).await, vec!["ALICE_PHONE".to_owned()]);

    // Before: the admin API sees a named, active account with one device, joined to the room.
    let before = admin.get(&format!("/api/v1/users/{alice_path}")).await;
    assert_eq!(before["display_name"], "Alice Liddell", "{before}");
    assert_eq!(before["deactivated"], false);
    assert_eq!(before["erased"], false);
    assert_eq!(before["device_count"], 1);

    let erased = admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{alice_path}/deactivate"),
            Some(json!({"erase": true, "reason": "asked to be forgotten"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(erased["deactivated"], true, "{erased}");
    assert_eq!(erased["erased"], true, "{erased}");
    assert!(erased["display_name"].is_null(), "{erased}");
    assert_eq!(erased["device_count"], 0, "{erased}");

    // The record read back says the same.
    let after = admin.get(&format!("/api/v1/users/{alice_path}")).await;
    assert_eq!(after["erased"], true, "{after}");
    assert!(after["display_name"].is_null(), "{after}");

    // Her devices are gone, and so are their keys for everybody else.
    let devices = admin
        .get(&format!("/api/v1/users/{alice_path}/devices"))
        .await;
    assert!(devices["items"].as_array().unwrap().is_empty(), "{devices}");
    assert!(keyed_devices(&bob).await.is_empty());

    // She left the room: its state says so, and bob sees the leave.
    let member = bob
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/state/m.room.member/{alice_path}",
            escape(&room)
        ))
        .await;
    assert_eq!(member["membership"], "leave", "{member}");
    let memberships = admin
        .get(&format!("/api/v1/users/{alice_path}/memberships"))
        .await;
    assert_eq!(
        memberships["items"][0]["membership"], "leave",
        "{memberships}"
    );

    // Her old access token is refused.
    let (status, body) = alice
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["errcode"], "M_UNKNOWN_TOKEN", "{body}");

    // Her profile is empty for everybody, and she cannot be found in the directory.
    let profile = bob
        .get(&format!("/_matrix/client/v3/profile/{alice_path}"))
        .await;
    assert!(profile.get("displayname").is_none(), "{profile}");
    let search = bob
        .expect(
            Method::POST,
            "/_matrix/client/v3/user_directory/search",
            Some(json!({"search_term": "alice"})),
            StatusCode::OK,
        )
        .await;
    assert!(search["results"].as_array().unwrap().is_empty(), "{search}");

    // The audit log holds the erasure: a `users.deactivate` entry with the `/erased` change,
    // after the one with `/deactivated`.
    let audit = admin.get("/api/v1/audit-log?action=users.deactivate").await;
    let entries = audit["items"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{audit}");
    let pointers: Vec<&str> = entries
        .iter()
        .flat_map(|e| e["changes"].as_array().unwrap())
        .map(|c| c["pointer"].as_str().unwrap())
        .collect();
    assert!(pointers.contains(&"/erased"), "{audit}");
    assert!(pointers.contains(&"/deactivated"), "{audit}");
    assert_eq!(entries[0]["actor"]["id"], "@ops:example.org");
    assert_eq!(entries[0]["target"]["id"], ALICE);

    // There is no way back: reactivation and a new password are refused.
    let (status, problem) = admin
        .call(
            Method::POST,
            &format!("/api/v1/users/{alice_path}/reactivate"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(
        problem["detail"].as_str().unwrap().contains("erased"),
        "{problem}"
    );
    let (status, problem) = admin
        .call(
            Method::POST,
            &format!("/api/v1/users/{alice_path}/reset-password"),
            Some(json!({"password": "a-brand-new-password"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    let (status, body) = alice
        .call(
            Method::POST,
            "/_matrix/client/v3/login",
            Some(json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "hunter2-alice"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Erasing again changes nothing and is not audited again.
    let again = admin
        .expect(
            Method::POST,
            &format!("/api/v1/users/{alice_path}/deactivate"),
            Some(json!({"erase": true})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(again["erased"], true);
    let audit = admin.get("/api/v1/audit-log?action=users.deactivate").await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 2, "{audit}");

    // Bob erases himself: the spec's `erase` on `/account/deactivate` is the same erasure.
    bob.expect(
        Method::POST,
        "/_matrix/client/v3/account/deactivate",
        Some(json!({
            "erase": true,
            "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "bob"}, "password": "hunter2-bob"},
        })),
        StatusCode::OK,
    )
    .await;
    let bob_record = admin
        .get(&format!("/api/v1/users/{}", escape("@bob:example.org")))
        .await;
    assert_eq!(bob_record["erased"], true, "{bob_record}");
    assert_eq!(bob_record["device_count"], 0, "{bob_record}");
}
