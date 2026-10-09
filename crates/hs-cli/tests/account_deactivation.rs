//! A person deactivating their own account leaves every room they are in, through the real
//! server: `POST /account/deactivate` (with and without `erase`), checked from the other side
//! of each room. Synapse parts a deactivated account from all its rooms
//! (`DeactivateAccountHandler`); until this hook (`hs_cli::room_departure`, answering
//! `hs_auth::state::RoomDeparture`) a self-deactivated account stayed joined everywhere, and only
//! an administrator's `users.deactivate` with `erase: true` left rooms.
//!
//! Alice and bob register; alice makes a room bob joins, bob makes a room and invites alice
//! (unanswered). Alice deactivates herself with her password. Then: the first room's state says
//! she left and bob's `/sync` carries the leave; the invite is rejected (her membership in bob's
//! room is `leave`); the admin API lists both as `leave`; and her access token is refused.

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
const BOB: &str = "@bob:example.org";

/// The server, its first administrator, alice and bob, a public room alice made and bob joined,
/// and a room bob made and invited alice to.
async fn boot(
    dir: &std::path::Path,
) -> (
    hs_cli::serve::ServeHandle,
    Caller,
    Caller,
    Caller,
    String,
    String,
) {
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
    let lobby = alice
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
        &format!("/_matrix/client/v3/join/{}", escape(&lobby)),
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    let bobs = bob
        .expect(
            Method::POST,
            "/_matrix/client/v3/createRoom",
            Some(json!({"preset": "private_chat", "name": "Bob's", "invite": [ALICE]})),
            StatusCode::OK,
        )
        .await["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    (handle, admin, alice, bob, lobby, bobs)
}

async fn membership(caller: &Caller, room: &str, user: &str) -> Value {
    caller
        .get(&format!(
            "/_matrix/client/v3/rooms/{}/state/m.room.member/{}",
            escape(room),
            escape(user)
        ))
        .await
}

async fn deactivate(alice: &Caller, extra: Value) -> Value {
    let mut body = json!({
        "auth": {
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": "alice"},
            "password": "hunter2-alice",
        }
    });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    alice
        .expect(
            Method::POST,
            "/_matrix/client/v3/account/deactivate",
            Some(body),
            StatusCode::OK,
        )
        .await
}

#[tokio::test]
async fn a_self_deactivated_account_leaves_its_rooms_and_rejects_its_invites() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, bob, lobby, bobs) = boot(dir.path()).await;
    alice
        .expect(
            Method::PUT,
            &format!("/_matrix/client/v3/profile/{}/displayname", escape(ALICE)),
            Some(json!({"displayname": "Alice Liddell"})),
            StatusCode::OK,
        )
        .await;
    // Bob's sync position before the deactivation, so the leave shows up as new.
    let since = bob.get("/_matrix/client/v3/sync?timeout=0").await["next_batch"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(membership(&bob, &lobby, ALICE).await["membership"], "join");
    assert_eq!(membership(&bob, &bobs, ALICE).await["membership"], "invite");

    let answer = deactivate(&alice, json!({"erase": true})).await;
    assert_eq!(answer["id_server_unbind_result"], "success", "{answer}");

    // The lobby's state says she left.
    let left = membership(&bob, &lobby, ALICE).await;
    assert_eq!(left["membership"], "leave", "{left}");
    // The unanswered invite is rejected.
    assert_eq!(
        membership(&bob, &bobs, ALICE).await["membership"],
        "leave",
        "an invite is rejected at deactivation"
    );
    // Bob's next sync carries the leave in the lobby.
    let sync = bob
        .get(&format!(
            "/_matrix/client/v3/sync?timeout=5000&since={since}"
        ))
        .await;
    let leaves: Vec<&Value> = sync["rooms"]["join"][&lobby]["timeline"]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter(|e| {
                    e["type"] == "m.room.member"
                        && e["state_key"] == ALICE
                        && e["content"]["membership"] == "leave"
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(leaves.len(), 1, "{sync}");
    assert_eq!(leaves[0]["sender"], ALICE, "the leave is her own");

    // The admin API lists both memberships as left, the account as deactivated and erased.
    let memberships = admin
        .get(&format!("/api/v1/users/{}/memberships", escape(ALICE)))
        .await;
    let items = memberships["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{memberships}");
    assert!(
        items.iter().all(|m| m["membership"] == "leave"),
        "{memberships}"
    );
    let record = admin.get(&format!("/api/v1/users/{}", escape(ALICE))).await;
    assert_eq!(record["deactivated"], true, "{record}");
    assert_eq!(record["erased"], true, "{record}");

    // Her token is gone.
    let (status, body) = alice
        .call(Method::GET, "/_matrix/client/v3/account/whoami", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // Bob is untouched.
    assert_eq!(membership(&bob, &lobby, BOB).await["membership"], "join");
}

#[tokio::test]
async fn a_plain_deactivation_leaves_rooms_too() {
    let dir = tempfile::tempdir().unwrap();
    let (_handle, admin, alice, bob, lobby, bobs) = boot(dir.path()).await;

    deactivate(&alice, json!({})).await;

    assert_eq!(membership(&bob, &lobby, ALICE).await["membership"], "leave");
    assert_eq!(membership(&bob, &bobs, ALICE).await["membership"], "leave");
    let record = admin.get(&format!("/api/v1/users/{}", escape(ALICE))).await;
    assert_eq!(record["deactivated"], true, "{record}");
    assert_eq!(record["erased"], false, "{record}");
}
