//! Forgetting a federation destination against two real `hs serve` instances (decision 0042):
//! while a user of A is in a room B holds, A refuses to forget B (409 naming the shared room)
//! and a prune's dry run keeps it; once the user has left and the leave has reached B, the
//! dry run says B is unused and forgetting it answers 200, after which B is not a destination
//! at all -- until a room brings the two together again.
//!
//! Server names are IP literals with a port, as in `federation_two_servers.rs`, so no
//! discovery is involved.

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
         listeners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, federation, health]\n\
         storage:\n  backend: embedded\n  data_dir: {data_dir:?}\n\
         media:\n  storage:\n    backend: local\n    path: {media_dir:?}\n\
         auth:\n  enable_registration: true\n\
         federation:\n  ip_range_blocklist: []\n  allow_public_rooms_over_federation: true\n\
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

async fn start(port: u16) -> Server {
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

/// Registers `username` and returns `(user_id, access_token)`.
async fn register(client: &reqwest::Client, base: &str, username: &str) -> (String, String) {
    let first: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({"username": username, "password": "correct horse"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = first["session"].as_str().unwrap().to_owned();
    let done: Value = client
        .post(format!("{base}/_matrix/client/v3/register"))
        .json(&json!({
            "username": username,
            "password": "correct horse",
            "auth": {"type": "m.login.dummy", "session": session},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        done["user_id"].as_str().unwrap().to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

/// The first administrator's admin API token, through the setup link.
async fn admin_token(client: &reqwest::Client, server: &Server) -> String {
    let link = server
        .handle
        .setup_link
        .clone()
        .expect("a fresh server offers setup");
    let setup_token = link.split_once("#token=").unwrap().1.to_owned();
    let session: Value = client
        .post(format!("{}/api/v1/setup", server.base))
        .json(&json!({"setup_token": setup_token, "username": "ops", "password": "hunter2-first-admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    session["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("setup failed: {session}"))
        .to_owned()
}

async fn admin(
    client: &reqwest::Client,
    server: &Server,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = client
        .request(method, format!("{}{path}", server.base))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, value)
}

/// Polls `check` against a fresh `GET` of `path` for up to 30 s.
async fn admin_until(
    client: &reqwest::Client,
    server: &Server,
    token: &str,
    path: &str,
    check: impl Fn(u16, &Value) -> bool,
) -> (u16, Value) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = (0, Value::Null);
    while Instant::now() < deadline {
        last = admin(client, server, token, reqwest::Method::GET, path, None).await;
        if check(last.0, &last.1) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("gave up waiting on {path}: {} {}", last.0, last.1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_destination_is_forgotten_once_no_room_is_shared_and_refused_while_one_is() {
    let port_a = reserve_port();
    let port_b = reserve_port();
    let a = start(port_a).await;
    let b = start(port_b).await;
    let client = reqwest::Client::new();
    let ops = admin_token(&client, &a).await;

    let (_alice, alice_token) = register(&client, &a.base, "alice").await;
    let (_bob, bob_token) = register(&client, &b.base, "bob").await;

    // Bob, on B, makes a room; Alice, on A, joins it through B.
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", b.base))
        .bearer_auth(&bob_token)
        .json(&json!({"preset": "public_chat", "name": "forget me not", "room_version": "11"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    let join: Value = client
        .post(format!(
            "{}/_matrix/client/v3/join/{room_id}?server_name={}",
            a.base, b.name
        ))
        .bearer_auth(&alice_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(join["room_id"], room_id, "join failed: {join}");

    // A now has B as a destination, sharing the room.
    let destination = format!("/api/v1/federation/destinations/{}", b.name);
    let (status, row) = admin_until(&client, &a, &ops, &destination, |status, row| {
        status == 200 && row["shared_rooms_count"] == 1
    })
    .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["server_name"], b.name);
    let (status, page) = admin(
        &client,
        &a,
        &ops,
        reqwest::Method::GET,
        "/api/v1/federation/destinations?include_total=true&shares_room=true",
        None,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["total"], 1, "{page}");

    // Forgetting it is refused: a room is shared.
    let (status, problem) = admin(
        &client,
        &a,
        &ops,
        reqwest::Method::DELETE,
        &destination,
        None,
    )
    .await;
    assert_eq!(status, 409, "{problem}");
    assert_eq!(problem["type"], "urn:hs:problem:conflict");
    let detail = problem["detail"].as_str().unwrap();
    assert!(detail.contains("shares 1 room with"), "{detail}");
    assert!(detail.contains("force=true"), "{detail}");

    // And a prune's dry run keeps it, saying why.
    let (status, report) = admin(
        &client,
        &a,
        &ops,
        reqwest::Method::POST,
        "/api/v1/federation/destinations/prune?dry_run=true",
        Some(json!({"failing_for": "7d"})),
    )
    .await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["forgotten"]["count"], 0, "{report}");
    assert_eq!(report["kept"]["by_reason"]["shares_rooms"], 1, "{report}");
    assert_eq!(report["kept"]["servers"][0]["server_name"], b.name);
    assert_eq!(
        report["kept"]["servers"][0]["detail"],
        "shares 1 room with this server"
    );

    // Alice leaves; the leave goes to B, and then nothing is shared or queued.
    let left = client
        .post(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/leave",
            a.base
        ))
        .bearer_auth(&alice_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(left.status(), 200);
    let bob_sees_leave = |sync: &Value| {
        sync["rooms"]["join"][&room_id]["timeline"]["events"]
            .as_array()
            .is_some_and(|events| {
                events
                    .iter()
                    .any(|e| e["type"] == "m.room.member" && e["content"]["membership"] == "leave")
            })
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let sync: Value = client
            .get(format!("{}/_matrix/client/v3/sync?timeout=0", b.base))
            .bearer_auth(&bob_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if bob_sees_leave(&sync) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B never saw alice's leave: {sync}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // The dry run now says B is unused (the prune reads the rooms afresh, unlike the list,
    // which keeps a reading for a short while).
    let deadline = Instant::now() + Duration::from_secs(30);
    let report = loop {
        let (status, report) = admin(
            &client,
            &a,
            &ops,
            reqwest::Method::POST,
            "/api/v1/federation/destinations/prune?dry_run=true",
            None,
        )
        .await;
        assert_eq!(status, 200, "{report}");
        if report["forgotten"]["count"] == 1 {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "the prune never found B unused: {report}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(report["forgotten"]["by_reason"]["unused"], 1, "{report}");
    assert_eq!(report["forgotten"]["servers"][0]["server_name"], b.name);
    assert_eq!(report["kept"]["count"], 0, "{report}");
    let (status, row) = admin(&client, &a, &ops, reqwest::Method::GET, &destination, None).await;
    assert_eq!(status, 200, "a dry run forgets nothing: {row}");

    // Forgetting it answers what was dropped; it is then unknown.
    let (status, forgotten) = admin(
        &client,
        &a,
        &ops,
        reqwest::Method::DELETE,
        &destination,
        None,
    )
    .await;
    assert_eq!(status, 200, "{forgotten}");
    assert_eq!(forgotten["server_name"], b.name);
    assert_eq!(forgotten["shared_rooms_count"], 0);
    assert_eq!(forgotten["dropped_pdu_count"], 0);
    assert!(
        forgotten["dropped_key_count"].as_u64().unwrap() >= 1,
        "B's keys were held for its signatures: {forgotten}"
    );
    let (status, _) = admin(&client, &a, &ops, reqwest::Method::GET, &destination, None).await;
    assert_eq!(status, 404);
    let (status, keys) = admin(
        &client,
        &a,
        &ops,
        reqwest::Method::GET,
        &format!("/api/v1/federation/keys/{}", b.name),
        None,
    )
    .await;
    assert_eq!(status, 404, "its keys went with it: {keys}");
    let (status, _) = admin(
        &client,
        &a,
        &ops,
        reqwest::Method::DELETE,
        &destination,
        None,
    )
    .await;
    assert_eq!(status, 404, "forgetting it again");

    // The forget is on the record.
    let (status, audit) = admin(
        &client,
        &a,
        &ops,
        reqwest::Method::GET,
        "/api/v1/audit-log?action=federation.destinations.forget",
        None,
    )
    .await;
    assert_eq!(status, 200, "{audit}");
    assert_eq!(audit["items"][0]["target"]["id"], b.name, "{audit}");

    // A room brings them together again: B is a destination once more, from nothing.
    let rejoin: Value = client
        .post(format!(
            "{}/_matrix/client/v3/join/{room_id}?server_name={}",
            a.base, b.name
        ))
        .bearer_auth(&alice_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rejoin["room_id"], room_id, "rejoin failed: {rejoin}");
    let (status, row) =
        admin_until(&client, &a, &ops, &destination, |status, _| status == 200).await;
    assert_eq!(status, 200, "{row}");

    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
