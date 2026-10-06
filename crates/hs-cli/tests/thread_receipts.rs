//! Threaded read receipts and per-thread notification counts (MSC3771, MSC3773, MSC4102)
//! through real `hs serve` instances in one process: Complement's `TestThreadedReceipts` and
//! `TestThreadReceiptsInSyncMSC4102` (`tests/csapi/thread_notifications_test.go`), checked more
//! strictly than Complement does: each `/sync` after an action must already be right, where
//! Complement keeps syncing until it is. Two servers federate over plain HTTP, as in
//! `federation_edus.rs`.

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
         federation:\n  ip_range_blocklist: []\n\
         rate_limits:\n  enabled: false\n"
    );
    hs_config::Config::from_yaml(&yaml).expect("the test configuration parses")
}

struct Server {
    _handle: hs_cli::serve::ServeHandle,
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
        _handle: handle,
        _dir: dir,
    }
}

struct User {
    id: String,
    token: String,
    base: String,
}

async fn register(client: &reqwest::Client, server: &Server, username: &str) -> User {
    let base = &server.base;
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
    User {
        id: done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        token: done["access_token"].as_str().unwrap().to_owned(),
        base: base.clone(),
    }
}

async fn call(
    client: &reqwest::Client,
    user: &User,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (reqwest::StatusCode, Value) {
    let response = client
        .request(method, format!("{}/_matrix/client/v3/{path}", user.base))
        .bearer_auth(&user.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn ok(
    client: &reqwest::Client,
    user: &User,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> Value {
    let (status, body) = call(client, user, method, path, body).await;
    assert!(status.is_success(), "{path} answered {status}: {body}");
    body
}

/// One `/sync`, from `since` if given, with `filter` inline if given, not waiting.
async fn sync(
    client: &reqwest::Client,
    user: &User,
    since: Option<&str>,
    filter: Option<&str>,
) -> Value {
    let mut query = vec![("timeout", "0".to_owned())];
    if let Some(since) = since {
        query.push(("since", since.to_owned()));
    }
    if let Some(filter) = filter {
        query.push(("filter", filter.to_owned()));
    }
    client
        .get(format!("{}/_matrix/client/v3/sync", user.base))
        .query(&query)
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Syncs until `wanted` is true of a response (each from the last one's `next_batch`), or
/// thirty seconds have passed.
async fn sync_until(
    client: &reqwest::Client,
    user: &User,
    what: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let response = sync(client, user, None, None).await;
        if wanted(&response) {
            return response;
        }
        last = response;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the sync never showed {what}; the last one said: {last}");
}

fn timeline_has(sync: &Value, room_id: &str, event_id: &str) -> bool {
    sync["rooms"]["join"][room_id]["timeline"]["events"]
        .as_array()
        .is_some_and(|events| events.iter().any(|e| e["event_id"] == event_id))
}

/// The receipt `user` has on `event_id` in `room_id`'s `m.receipt` events, if any.
fn receipt<'a>(sync: &'a Value, room_id: &str, event_id: &str, user: &str) -> Option<&'a Value> {
    sync["rooms"]["join"][room_id]["ephemeral"]["events"]
        .as_array()?
        .iter()
        .filter(|e| e["type"] == "m.receipt")
        .find_map(|e| e["content"][event_id]["m.read"].get(user))
}

/// Sends `content` as `event_type` and waits until the sender's own `/sync` has it, as
/// Complement's `SendEventSynced` does.
async fn send_synced(
    client: &reqwest::Client,
    user: &User,
    room_id: &str,
    event_type: &str,
    content: Value,
) -> String {
    let txn = format!("t{}", rand_suffix());
    let sent = ok(
        client,
        user,
        reqwest::Method::PUT,
        &format!("rooms/{room_id}/send/{event_type}/{txn}"),
        content,
    )
    .await;
    let event_id = sent["event_id"].as_str().unwrap().to_owned();
    sync_until(client, user, "the sent event", |s| {
        timeline_has(s, room_id, &event_id)
    })
    .await;
    event_id
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn counts(sync: &Value, room_id: &str) -> (u64, u64, Option<Value>) {
    let room = &sync["rooms"]["join"][room_id];
    let unread = &room["unread_notifications"];
    (
        unread["notification_count"]
            .as_u64()
            .unwrap_or_else(|| panic!("no counts: {room}")),
        unread["highlight_count"].as_u64().unwrap(),
        room.get("unread_thread_notifications").cloned(),
    )
}

const THREAD_FILTER: &str = r#"{"room":{"timeline":{"unread_thread_notifications":true}}}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn threaded_receipts_read_their_thread_and_counts_split_by_thread() {
    let server = start().await;
    let client = reqwest::Client::new();
    let alice = register(&client, &server, "alice").await;
    let bob = register(&client, &server, "bob").await;
    use reqwest::Method;

    // MSC4306's `postcontent` kind: Complement's `disableMsc4306PushRules` asks for its rules
    // first and moves on at a `404`.
    let (status, _) = call(
        &client,
        &bob,
        Method::GET,
        "pushrules/global/postcontent/.io.element.msc4306.rule.subscribed_thread",
        Value::Null,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    let room = ok(
        &client,
        &alice,
        Method::POST,
        "createRoom",
        json!({"preset": "public_chat"}),
    )
    .await;
    let room_id = room["room_id"].as_str().unwrap().to_owned();
    ok(
        &client,
        &bob,
        Method::POST,
        &format!("join/{room_id}"),
        json!({}),
    )
    .await;
    let joined = sync_until(&client, &bob, "bob's join", |s| {
        s["rooms"]["join"].get(&room_id).is_some()
    })
    .await;
    let since = joined["next_batch"].as_str().unwrap().to_owned();

    let message = |body: String| json!({"msgtype": "m.text", "body": body});
    let a = send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.message",
        message("Hello world!".into()),
    )
    .await;
    let threaded = |body: String, mention: Option<&str>| {
        let mut content = json!({
            "msgtype": "m.text",
            "body": body,
            "m.relates_to": {"event_id": a, "rel_type": "m.thread"},
        });
        if let Some(user) = mention {
            content["m.mentions"] = json!({"user_ids": [user]});
        }
        content
    };
    let b = send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.message",
        threaded("Start thread!".into(), None),
    )
    .await;
    send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.message",
        threaded(format!("Thread response {}!", bob.id), Some(&bob.id)),
    )
    .await;
    let mut mention = message(format!("Hello {}!", bob.id));
    mention["m.mentions"] = json!({"user_ids": [bob.id]});
    let d = send_synced(&client, &alice, &room_id, "m.room.message", mention).await;
    send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.message",
        threaded("End thread".into(), None),
    )
    .await;
    let f = send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.message",
        json!({"msgtype": "m.text", "body": "Reference!",
               "m.relates_to": {"event_id": a, "rel_type": "m.reference"}}),
    )
    .await;
    let g = send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.reaction",
        json!({"m.relates_to": {"event_id": f, "rel_type": "m.annotation", "key": "test"}}),
    )
    .await;

    // Each step: the unsplit counts, then the split ones; returns the thread's (highlight,
    // notification) counts from the split ones, `None` when the thread is left out.
    let check = |label: &'static str, whole: (u64, u64), main: (u64, u64)| {
        let (client, bob, room_id, since, a, d) = (
            client.clone(),
            &bob,
            room_id.clone(),
            since.clone(),
            a.clone(),
            d.clone(),
        );
        async move {
            let plain = sync(&client, bob, Some(&since), None).await;
            assert!(timeline_has(&plain, &room_id, &d), "{label}: {plain}");
            let (n, h, threads) = counts(&plain, &room_id);
            assert_eq!((h, n), whole, "{label}: unsplit counts");
            assert_eq!(threads, None, "{label}: no thread counts unless asked for");
            let split = sync(&client, bob, Some(&since), Some(THREAD_FILTER)).await;
            let (n, h, threads) = counts(&split, &room_id);
            assert_eq!((h, n), main, "{label}: main timeline counts");
            let thread = threads.as_ref().map(|t| {
                (
                    t[&a]["highlight_count"].as_u64().unwrap(),
                    t[&a]["notification_count"].as_u64().unwrap(),
                )
            });
            (plain, split, thread)
        }
    };
    let (_, _, t) = check("before any receipt", (2, 6), (1, 3)).await;
    assert_eq!(t, Some((1, 3)));

    let post_receipt = |event: String, body: Value| {
        let (client, bob, room_id) = (client.clone(), &bob, room_id.clone());
        async move {
            ok(
                &client,
                bob,
                Method::POST,
                &format!("rooms/{room_id}/receipt/m.read/{event}"),
                body,
            )
            .await;
        }
    };

    post_receipt(a.clone(), json!({"thread_id": "main"})).await;
    let (plain, _, t) = check("a main receipt on A", (2, 5), (1, 2)).await;
    assert_eq!(t, Some((1, 3)));
    assert_eq!(
        receipt(&plain, &room_id, &a, &bob.id).unwrap()["thread_id"],
        "main"
    );

    post_receipt(b.clone(), json!({"thread_id": a})).await;
    let (plain, _, t) = check("a thread receipt on B", (2, 4), (1, 2)).await;
    assert_eq!(t, Some((1, 2)));
    assert_eq!(
        receipt(&plain, &room_id, &b, &bob.id).unwrap()["thread_id"],
        a.as_str()
    );

    post_receipt(d.clone(), json!({})).await;
    let (plain, _, t) = check("an unthreaded receipt on D", (0, 2), (0, 1)).await;
    assert_eq!(t, Some((0, 1)));
    assert!(
        receipt(&plain, &room_id, &d, &bob.id)
            .unwrap()
            .get("thread_id")
            .is_none()
    );

    post_receipt(g.clone(), json!({"thread_id": a})).await;
    let (_, split, t) = check("the thread read through G", (0, 1), (0, 1)).await;
    assert_eq!(t, None, "a thread with nothing unread is left out: {split}");

    // A `thread_id` that is neither `main` nor an event ID is refused.
    let (status, body) = call(
        &client,
        &bob,
        Method::POST,
        &format!("rooms/{room_id}/receipt/m.read/{g}"),
        json!({"thread_id": "nonsense"}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unthreaded_receipt_wins_a_clash_here_and_over_federation() {
    let (hs1, hs2) = (start().await, start().await);
    let client = reqwest::Client::new();
    let alice = register(&client, &hs1, "alice").await;
    let bob = register(&client, &hs2, "bob").await;
    use reqwest::Method;

    let room = ok(
        &client,
        &alice,
        Method::POST,
        "createRoom",
        json!({"preset": "public_chat"}),
    )
    .await;
    let room_id = room["room_id"].as_str().unwrap().to_owned();
    ok(
        &client,
        &bob,
        Method::POST,
        &format!("join/{room_id}?server_name={}", hs1.name),
        json!({}),
    )
    .await;
    let a = send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.message",
        json!({"msgtype": "m.text", "body": "Hello world!"}),
    )
    .await;
    let b = send_synced(
        &client,
        &alice,
        &room_id,
        "m.room.message",
        json!({"msgtype": "m.text", "body": "Start thread!",
               "m.relates_to": {"event_id": a, "rel_type": "m.thread"}}),
    )
    .await;

    ok(
        &client,
        &alice,
        Method::POST,
        &format!("rooms/{room_id}/receipt/m.read/{b}"),
        json!({}),
    )
    .await;
    ok(
        &client,
        &alice,
        Method::POST,
        &format!("rooms/{room_id}/receipt/m.read/{b}"),
        json!({"thread_id": a}),
    )
    .await;

    let unthreaded = |s: &Value| {
        receipt(s, &room_id, &b, &alice.id).is_some_and(|r| r.get("thread_id").is_none())
    };
    let here = sync(&client, &alice, None, None).await;
    assert!(unthreaded(&here), "alice's own server: {here}");
    sync_until(
        &client,
        &bob,
        "alice's unthreaded receipt over federation",
        unthreaded,
    )
    .await;
}
