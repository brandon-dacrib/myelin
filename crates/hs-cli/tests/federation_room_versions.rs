//! Two real `hs serve` instances over plain HTTP (as `federation_two_servers.rs`), for what
//! Sytest's first run found missing between servers: a room of version 1 could not be joined
//! over federation (`make_join` refused to cite events by hash), and a redaction that arrived
//! over federation was stored and never applied.

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
        done["user_id"]
            .as_str()
            .unwrap_or_else(|| panic!("registration failed: {done}"))
            .to_owned(),
        done["access_token"].as_str().unwrap().to_owned(),
    )
}

/// Asks `url` until `wanted` holds of the answer, or fifteen seconds pass: federation delivery
/// runs off background tasks.
async fn get_until(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let response: Value = client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or(Value::Null);
        if wanted(&response) {
            return response;
        }
        last = response;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("{url} never answered what was expected; the last answer: {last}");
}

struct Joined {
    a: Server,
    b: Server,
    client: reqwest::Client,
    alice_token: String,
    bob_token: String,
    room_id: String,
}

/// Alice makes a public room of `version` on A; bob, on B, joins it through A.
async fn join_across(version: &str) -> Joined {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let (_, alice_token) = register(&client, &a.base, "alice").await;
    let (_, bob_token) = register(&client, &b.base, "bob").await;
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "public_chat", "room_version": version}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"]
        .as_str()
        .unwrap_or_else(|| panic!("createRoom failed: {created}"))
        .to_owned();
    let join = client
        .post(format!(
            "{}/_matrix/client/v3/join/{room_id}?server_name={}",
            b.base, a.name
        ))
        .bearer_auth(&bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = join.status();
    let body: Value = join.json().await.unwrap();
    assert_eq!(
        status, 200,
        "joining a version {version} room failed: {body}"
    );
    Joined {
        a,
        b,
        client,
        alice_token,
        bob_token,
        room_id,
    }
}

async fn send(joined: &Joined, base: &str, token: &str, body: &str) -> String {
    let response: Value = joined
        .client
        .put(format!(
            "{base}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            joined.room_id,
            uuid_like()
        ))
        .bearer_auth(token)
        .json(&json!({"msgtype": "m.text", "body": body}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    response["event_id"]
        .as_str()
        .unwrap_or_else(|| panic!("send failed: {response}"))
        .to_owned()
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// A room of version 1 -- events cited by `[id, {sha256}]`, ids not derived from hashes -- is
/// joined over federation, and messages cross both ways after. Before the fix, `make_join`
/// answered `M_UNSUPPORTED_ROOM_VERSION` for versions 1 and 2 (12 Sytest failures).
#[tokio::test]
async fn a_version_1_room_is_joined_over_federation_and_messages_cross() {
    let joined = join_across("1").await;
    let from_bob = send(&joined, &joined.b.base, &joined.bob_token, "from bob").await;
    let at_a = get_until(
        &joined.client,
        &format!(
            "{}/_matrix/client/v3/rooms/{}/event/{from_bob}",
            joined.a.base, joined.room_id
        ),
        &joined.alice_token,
        |event| event["content"]["body"] == "from bob",
    )
    .await;
    assert_eq!(at_a["event_id"], from_bob.as_str());
    let from_alice = send(&joined, &joined.a.base, &joined.alice_token, "from alice").await;
    get_until(
        &joined.client,
        &format!(
            "{}/_matrix/client/v3/rooms/{}/event/{from_alice}",
            joined.b.base, joined.room_id
        ),
        &joined.bob_token,
        |event| event["content"]["body"] == "from alice",
    )
    .await;
    joined.a.handle.shutdown().await;
    joined.b.handle.shutdown().await;
}

/// A room of version 3, whose event IDs are standard base64 and so about half the time carry a
/// `/`: six invites from A to users of B all go through. Before the client percent-encoded the
/// IDs in its request paths, the `/` split the path, B answered `404 M_UNRECOGNIZED`, and each
/// invite failed with even odds (Sytest's "User can invite remote user to room with version 3"
/// passed or failed from one run to the next); six in a row all succeeding was about 1 in 60.
#[tokio::test]
async fn invites_in_a_version_3_room_reach_another_server_whatever_their_event_ids() {
    let a = start(reserve_port()).await;
    let b = start(reserve_port()).await;
    let client = reqwest::Client::new();
    let (_, alice_token) = register(&client, &a.base, "alice").await;
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice_token)
        .json(&json!({"preset": "private_chat", "room_version": "3"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"]
        .as_str()
        .unwrap_or_else(|| panic!("createRoom failed: {created}"))
        .to_owned();
    for i in 0..6 {
        let (invitee, _) = register(&client, &b.base, &format!("guest{i}")).await;
        let response = client
            .post(format!(
                "{}/_matrix/client/v3/rooms/{room_id}/invite",
                a.base
            ))
            .bearer_auth(&alice_token)
            .json(&json!({"user_id": invitee}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        assert_eq!(status, 200, "inviting {invitee} failed: {body}");
    }
    a.handle.shutdown().await;
    b.handle.shutdown().await;
}

/// `GET {base}/_matrix/client/v3/sync` as `token`, from `since` if given, answered within ten
/// seconds.
async fn sync(client: &reqwest::Client, base: &str, token: &str, since: Option<&str>) -> Value {
    let mut url = format!("{base}/_matrix/client/v3/sync?timeout=2000");
    if let Some(since) = since {
        url.push_str(&format!("&since={}", urlencode(since)));
    }
    client
        .get(url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn urlencode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Bob, on B, redacts his own message in a room of `version` made on A, where bob has no power.
/// On A, where the redaction arrives over federation, the message's content is gone; once
/// alice's `/sync` has the redaction, a backward `/messages` from that sync's `next_batch` starts
/// with it, then the message, which names it in `unsigned.redacted_by` and carries it whole in
/// `unsigned.redacted_because` -- exactly what Sytest's "Can receive redactions from regular
/// users over federation in room version N" asks of the receiving server. `/event` on both
/// servers carries the same.
///
/// What failed before 2026-10-01's fixes: in versions 1 and 2 bob's redaction was refused on B
/// (its ID was minted after the auth check, so the same-server rule never matched); in every
/// version no event rendered `redacted_because` or `redacted_by`; before the sixteenth session
/// the redaction was not applied on A at all.
async fn redaction_crosses(version: &str) {
    let joined = join_across(version).await;
    let message = send(&joined, &joined.b.base, &joined.bob_token, "regrettable").await;
    let event_url = format!(
        "{}/_matrix/client/v3/rooms/{}/event/{}",
        joined.a.base,
        joined.room_id,
        urlencode(&message)
    );
    get_until(&joined.client, &event_url, &joined.alice_token, |event| {
        event["content"]["body"] == "regrettable"
    })
    .await;
    let since = sync(&joined.client, &joined.a.base, &joined.alice_token, None).await["next_batch"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = joined
        .client
        .put(format!(
            "{}/_matrix/client/v3/rooms/{}/redact/{}/{}",
            joined.b.base,
            joined.room_id,
            urlencode(&message),
            uuid_like()
        ))
        .bearer_auth(&joined.bob_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let redaction: Value = response.json().await.unwrap();
    assert_eq!(
        status, 200,
        "version {version}: bob's redaction of his own message on B: {redaction}"
    );
    let redaction_id = redaction["event_id"].as_str().unwrap().to_owned();

    // Alice's sync on A, until its timeline has the redaction.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut since = since;
    loop {
        let body = sync(
            &joined.client,
            &joined.a.base,
            &joined.alice_token,
            Some(&since),
        )
        .await;
        since = body["next_batch"].as_str().unwrap().to_owned();
        let seen = body["rooms"]["join"][&joined.room_id]["timeline"]["events"]
            .as_array()
            .is_some_and(|events| {
                events
                    .iter()
                    .any(|e| e["event_id"] == redaction_id.as_str())
            });
        if seen {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "version {version}: the redaction never reached alice's sync on A"
        );
    }
    let page: Value = joined
        .client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{}/messages?dir=b&from={}",
            joined.a.base,
            joined.room_id,
            urlencode(&since)
        ))
        .bearer_auth(&joined.alice_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let chunk = page["chunk"].as_array().unwrap();
    assert_eq!(
        chunk[0]["event_id"], redaction_id,
        "version {version}: /messages on A does not start with the redaction: {page}"
    );
    assert_eq!(chunk[0]["redacts"], message.as_str(), "version {version}");
    assert_eq!(chunk[1]["event_id"], message.as_str(), "version {version}");
    assert_eq!(
        chunk[1]["unsigned"]["redacted_by"], redaction_id,
        "version {version}: {}",
        chunk[1]
    );
    assert_eq!(
        chunk[1]["unsigned"]["redacted_because"]["event_id"], redaction_id,
        "version {version}"
    );
    assert_eq!(
        chunk[1]["unsigned"]["redacted_because"]["sender"], chunk[0]["sender"],
        "version {version}"
    );
    assert!(
        chunk[1]["unsigned"]["redacted_because"]
            .get("signatures")
            .is_none(),
        "version {version}: redacted_because is shown as a client event"
    );
    assert!(
        chunk[1]["content"]
            .as_object()
            .is_some_and(|c| c.is_empty()),
        "version {version}"
    );

    for (base, token) in [
        (&joined.a.base, &joined.alice_token),
        (&joined.b.base, &joined.bob_token),
    ] {
        let event = get_until(
            &joined.client,
            &format!(
                "{base}/_matrix/client/v3/rooms/{}/event/{}",
                joined.room_id,
                urlencode(&message)
            ),
            token,
            |event| event["unsigned"]["redacted_by"] == redaction_id.as_str(),
        )
        .await;
        assert_eq!(
            event["unsigned"]["redacted_because"]["event_id"], redaction_id,
            "version {version}, {base}"
        );
    }
    joined.a.handle.shutdown().await;
    joined.b.handle.shutdown().await;
}

#[tokio::test]
async fn a_redaction_received_over_federation_is_applied_and_shown_in_version_1() {
    redaction_crosses("1").await;
}

#[tokio::test]
async fn a_redaction_received_over_federation_is_applied_and_shown_in_version_2() {
    redaction_crosses("2").await;
}

#[tokio::test]
async fn a_redaction_received_over_federation_is_applied_and_shown_in_version_11() {
    redaction_crosses("11").await;
}
