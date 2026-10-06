//! Guest access withdrawn over federation, across two real `hs serve`s, as Sytest's "Guest users
//! are kicked from guest_access rooms on revocation of guest_access over federation"
//! (`30rooms/13guestaccess.pl`) drives it: alice of server A makes a room, bob of server B joins
//! it and is given power, alice lets guests in, bob joins again, a guest of A joins; then *bob*
//! withdraws guest access, and A must make its guest leave.

use std::time::{Duration, Instant};

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
         auth:\n  enable_registration: true\n  allow_guest_access: true\n\
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

struct Server {
    handle: hs_cli::serve::ServeHandle,
    base: String,
    name: String,
    _dir: tempfile::TempDir,
}

async fn start() -> Server {
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
    Server {
        base: handle.base_url(),
        name: format!("127.0.0.1:{port}"),
        handle,
        _dir: dir,
    }
}

async fn register(client: &reqwest::Client, base: &str, username: &str) -> (String, String) {
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy"},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

/// `POST` (or `PUT`) `body` to `path` until it succeeds, as Sytest's `retry_until_success`.
async fn until_ok(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Value,
    what: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let response = client
            .request(method.clone(), &url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return answer;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never succeeded; the last answer: {status} {answer}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Where the user's `/sync` stands, as Sytest's `matrix_do_and_wait_for_sync` asks before its
/// action: an initial sync that asks for no rooms, setting the user offline.
async fn sync_position(client: &reqwest::Client, base: &str, token: &str) -> String {
    let filter = r#"{"room":{"rooms":[]},"account_data":{},"presence":{"types":[]}}"#;
    let response: Value = client
        .get(format!("{base}/_matrix/client/v3/sync"))
        .query(&[("filter", filter), ("set_presence", "offline")])
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    response["next_batch"].as_str().unwrap().to_owned()
}

/// `/sync` from `since` until `room_id` is among the joined rooms.
async fn sync_until_joined(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    room_id: &str,
    since: String,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut since = Some(since);
    loop {
        let since_param = since
            .as_deref()
            .map(|s| format!("&since={s}"))
            .unwrap_or_default();
        let response: Value = client
            .get(format!(
                "{base}/_matrix/client/v3/sync?timeout=500{since_param}"
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if response["rooms"]["join"][room_id].is_object() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{room_id} never showed as joined; the last sync: {response}"
        );
        since = response["next_batch"].as_str().map(str::to_owned);
    }
}

async fn membership(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    room_id: &str,
    user: &str,
) -> Option<String> {
    let response = client
        .get(format!(
            "{base}/_matrix/client/v3/rooms/{room_id}/state/m.room.member/{user}"
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.ok()?;
    body["membership"].as_str().map(str::to_owned)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_is_made_to_leave_when_a_member_of_another_server_withdraws_guest_access() {
    let a = start().await;
    let b = start().await;
    let client = reqwest::Client::new();
    let (_alice_id, alice) = register(&client, &a.base, "alice").await;
    let (bob_id, bob) = register(&client, &b.base, "bob").await;

    for round in 0..3 {
        // `matrix_create_and_join_room([alice, bob])`: a public room with an alias, bob joining
        // it by the alias.
        let alias_name = format!("guests-{round}");
        let created: Value = client
            .post(format!("{}/_matrix/client/v3/createRoom", a.base))
            .bearer_auth(&alice)
            .json(&json!({"visibility": "private", "preset": "public_chat", "room_alias_name": alias_name}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let room = created["room_id"]
            .as_str()
            .unwrap_or_else(|| panic!("createRoom: {created}"))
            .to_owned();
        let alias = format!("%23{alias_name}:{}", a.name);
        let since = sync_position(&client, &b.base, &bob).await;
        until_ok(
            &client,
            reqwest::Method::POST,
            format!("{}/_matrix/client/v3/join/{alias}", b.base),
            &bob,
            json!({}),
            "bob's join by alias",
        )
        .await;
        sync_until_joined(&client, &b.base, &bob, &room, since).await;

        // Bob gets power; guests may join.
        let mut levels: Value = client
            .get(format!(
                "{}/_matrix/client/v3/rooms/{room}/state/m.room.power_levels/",
                a.base
            ))
            .bearer_auth(&alice)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        levels["users"][&bob_id] = json!(50);
        until_ok(
            &client,
            reqwest::Method::PUT,
            format!(
                "{}/_matrix/client/v3/rooms/{room}/state/m.room.power_levels/",
                a.base
            ),
            &alice,
            levels,
            "the power levels",
        )
        .await;
        until_ok(
            &client,
            reqwest::Method::PUT,
            format!(
                "{}/_matrix/client/v3/rooms/{room}/state/m.room.guest_access/",
                a.base
            ),
            &alice,
            json!({"guest_access": "can_join"}),
            "guest access on",
        )
        .await;

        // Bob joins again; a guest of A joins. B holds the guest access first: what made
        // Sytest's version of this flaky was B having it before Sytest took bob's sync
        // position, so that only a new join event could show bob's sync the room.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let held = client
                .get(format!(
                    "{}/_matrix/client/v3/rooms/{room}/state/m.room.guest_access/",
                    b.base
                ))
                .bearer_auth(&bob)
                .send()
                .await
                .unwrap();
            if held.status().is_success() {
                break;
            }
            assert!(Instant::now() < deadline, "B never got the guest access");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let since = sync_position(&client, &b.base, &bob).await;
        until_ok(
            &client,
            reqwest::Method::POST,
            format!("{}/_matrix/client/v3/join/{room}", b.base),
            &bob,
            json!({}),
            "bob's second join",
        )
        .await;
        sync_until_joined(&client, &b.base, &bob, &room, since).await;
        let guest: Value = client
            .post(format!("{}/_matrix/client/v3/register?kind=guest", a.base))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let guest_id = guest["user_id"].as_str().unwrap().to_owned();
        let guest_token = guest["access_token"].as_str().unwrap().to_owned();
        let since = sync_position(&client, &a.base, &guest_token).await;
        until_ok(
            &client,
            reqwest::Method::POST,
            format!("{}/_matrix/client/v3/join/{room}", a.base),
            &guest_token,
            json!({}),
            "the guest's join",
        )
        .await;
        sync_until_joined(&client, &a.base, &guest_token, &room, since).await;
        assert_eq!(
            membership(&client, &a.base, &alice, &room, &guest_id)
                .await
                .as_deref(),
            Some("join")
        );

        // Bob, on B, withdraws guest access (retried: his power may not have reached B yet).
        until_ok(
            &client,
            reqwest::Method::PUT,
            format!(
                "{}/_matrix/client/v3/rooms/{room}/state/m.room.guest_access/",
                b.base
            ),
            &bob,
            json!({"guest_access": "forbidden"}),
            "bob withdrawing guest access",
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let now = membership(&client, &a.base, &alice, &room, &guest_id).await;
            if now.as_deref() == Some("leave") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "round {round}: the guest was never made to leave on A: {now:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
