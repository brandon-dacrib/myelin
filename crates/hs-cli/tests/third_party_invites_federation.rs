//! Third-party (3PID) invites across two real `hs serve`s, as Sytest's "Can invite unbound 3pid
//! over federation" and "... with users from both servers" (`30rooms/12thirdpartyinvite.pl`)
//! drive them: alice of server A invites an address nobody has bound; the identity server keeps
//! the invitation; bob of server B binds the address, and the identity server tells *B*
//! (`/3pid/onbind`). B is not in the room (or is, through carol, but the invitation is alice's
//! to make), so it hands the invitation to A (`PUT /exchange_third_party_invite/{roomId}`); A
//! makes the invite, sends it to B over `/invite`, and bob sees it and joins. Before
//! 2026-10-04, B answered the identity server `404 room not found`.
//!
//! One test in this binary, run on a runtime it builds itself: the fake identity server's
//! certificate is self-signed, as Sytest's is, and the servers are told so through the
//! environment before anything else runs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[path = "support/fake_identity.rs"]
mod fake_identity;
use fake_identity::{FakeIdentityServer, serve_tls};

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
         auth:\n  enable_registration: true\n  identity_servers: [localhost]\n\
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

async fn sync_until(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let response: Value = client
            .get(format!("{base}/_matrix/client/v3/sync?timeout=500"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if wanted(&response) {
            return response;
        }
        last = response;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the sync never said what was expected; the last one said: {last}");
}

async fn member(
    client: &reqwest::Client,
    server: &Server,
    token: &str,
    room_id: &str,
    user: &str,
) -> Option<Value> {
    let response = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/state/m.room.member/{user}",
            server.base
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    response.json().await.ok()
}

/// One unbound invitation of alice's, claimed by `invitee` of server B: B hands it to A, which
/// invites them over federation, and they join.
#[allow(clippy::too_many_arguments)]
async fn unbound_invite_is_claimed(
    client: &reqwest::Client,
    ids: &FakeIdentityServer,
    a: &Server,
    b: &Server,
    alice: &str,
    room_id: &str,
    address: &str,
    invitee: (&str, &str),
    watcher: Option<&str>,
) {
    let (invitee_id, invitee_token) = invitee;
    let id_server = ids.base.trim_start_matches("https://").to_owned();
    // Where the watcher's legacy event stream stands before the invitation, as Sytest's
    // `await_event_for` reads it.
    let status = client
        .post(format!("{}/_matrix/client/v3/rooms/{room_id}/invite", a.base))
        .bearer_auth(alice)
        .json(&json!({"id_server": id_server, "id_access_token": "t", "medium": "email", "address": address}))
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success(), "{status}");
    // A member on B sees the invitation arrive, as Sytest's joiner waits for it: on the legacy
    // event stream, and in `/sync`.
    if let Some(watcher) = watcher {
        let mut from: Option<String> = None;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let from_param = from
                .as_deref()
                .map(|from| format!("&from={from}"))
                .unwrap_or_default();
            let page: Value = client
                .get(format!(
                    "{}/_matrix/client/v3/events?timeout=500{from_param}",
                    b.base
                ))
                .bearer_auth(watcher)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if page["chunk"].as_array().is_some_and(|chunk| {
                chunk
                    .iter()
                    .any(|e| e["type"] == "m.room.third_party_invite" && e["room_id"] == room_id)
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the invitation never reached the event stream; the last page: {page}"
            );
            from = page["end"].as_str().map(str::to_owned);
        }
        let wanted = room_id.to_owned();
        sync_until(client, &b.base, watcher, move |sync| {
            sync["rooms"]["join"][&wanted]["timeline"]["events"]
                .as_array()
                .is_some_and(|events| {
                    events
                        .iter()
                        .any(|e| e["type"] == "m.room.third_party_invite")
                })
        })
        .await;
    }
    let token = "tok1";
    let inviter = format!("@alice:{}", a.name);
    let response = client
        .post(format!("{}/_matrix/federation/v1/3pid/onbind", b.base))
        .json(&json!({
            "medium": "email",
            "address": address,
            "mxid": invitee_id,
            "invites": [{
                "medium": "email",
                "address": address,
                "mxid": invitee_id,
                "room_id": room_id,
                "sender": inviter,
                "signed": ids.sign(invitee_id, token),
            }],
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "onbind: {status} {body}");

    let wanted = room_id.to_owned();
    sync_until(client, &b.base, invitee_token, move |sync| {
        sync["rooms"]["invite"][&wanted].is_object()
    })
    .await;
    let invited = member(client, a, alice, room_id, invitee_id)
        .await
        .expect("the invite is in the room on A");
    assert_eq!(invited["membership"], "invite", "{invited}");
    assert_eq!(
        invited["third_party_invite"]["display_name"], "c...@e...",
        "{invited}"
    );

    let joined = client
        .post(format!("{}/_matrix/client/v3/join/{room_id}", b.base))
        .bearer_auth(invitee_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(joined.status().is_success(), "{:?}", joined.text().await);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let state = member(client, a, alice, room_id, invitee_id).await;
        if state.as_ref().is_some_and(|m| m["membership"] == "join") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the join never reached A: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[test]
fn an_unbound_invitation_claimed_on_another_server_becomes_an_invite_there() {
    // SAFETY: `set_var` is unsound only while another thread reads or writes the environment.
    // This is the one test in this binary and it runs this before it builds the runtime, so no
    // thread of this test has started; the harness's own threads do not touch the environment.
    unsafe {
        std::env::set_var(hs_cli::identity_service::INSECURE_TLS_ENV, "1");
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario());
}

async fn scenario() {
    let key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]));
    let stored = Arc::new(Mutex::new(Vec::new()));
    let ids_cell: Arc<Mutex<Option<FakeIdentityServer>>> = Arc::default();
    let (key2, stored2, cell2) = (key.clone(), stored.clone(), ids_cell.clone());
    serve_tls(move |base| {
        let ids = FakeIdentityServer {
            key: key2,
            base,
            stored: stored2,
        };
        *cell2.lock().unwrap() = Some(ids.clone());
        ids.router()
    })
    .await;
    let ids = ids_cell.lock().unwrap().clone().unwrap();

    let a = start().await;
    let b = start().await;
    let client = reqwest::Client::new();
    let (_alice_id, alice) = register(&client, &a.base, "alice").await;
    let (bob_id, bob) = register(&client, &b.base, "bob").await;
    let (carol_id, carol) = register(&client, &b.base, "carol").await;
    let (dave_id, dave) = register(&client, &b.base, "dave").await;

    // "Can invite unbound 3pid over federation": B has nobody in the room.
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice)
        .json(&json!({"preset": "private_chat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room = created["room_id"].as_str().unwrap().to_owned();
    unbound_invite_is_claimed(
        &client,
        &ids,
        &a,
        &b,
        &alice,
        &room,
        "lemurs@monkeyworld.org",
        (&bob_id, &bob),
        None,
    )
    .await;

    // "... with users from both servers": carol of B is in the room already -- a private one,
    // invited and then joined, as Sytest's `matrix_create_and_join_room(with_invite => 1)` --
    // and the invitation is still alice's, made on A.
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", a.base))
        .bearer_auth(&alice)
        .json(&json!({"preset": "private_chat"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let shared = created["room_id"].as_str().unwrap().to_owned();
    let invited = client
        .post(format!(
            "{}/_matrix/client/v3/rooms/{shared}/invite",
            a.base
        ))
        .bearer_auth(&alice)
        .json(&json!({"user_id": carol_id}))
        .send()
        .await
        .unwrap();
    assert!(invited.status().is_success(), "{:?}", invited.text().await);
    let joined = client
        .post(format!(
            "{}/_matrix/client/v3/join/{shared}?server_name={}",
            b.base, a.name
        ))
        .bearer_auth(&carol)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(joined.status().is_success(), "{:?}", joined.text().await);
    unbound_invite_is_claimed(
        &client,
        &ids,
        &a,
        &b,
        &alice,
        &shared,
        "dave@example.org",
        (&dave_id, &dave),
        Some(&carol),
    )
    .await;

    let metrics = client
        .get(format!("{}/metrics", b.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("hs_room_third_party_invites_total{outcome=\"forwarded\"} 2"),
        "{metrics}"
    );
    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
