//! A real `hs serve` and a stand-in for Sytest's federation server, over plain HTTP: the
//! `/state_ids` fallback for a prev event this server cannot walk back to
//! (`hs_federation::state_fallback`, `hs_room::actor::fetched_state`) and soft failure
//! (`hs_room::actor::soft_fail`), as Sytest's `50federation/36state.pl` and `52soft-fail.pl`
//! drive them.
//!
//! The peer answers what Sytest's does: its key, `/get_missing_events` with what the test
//! queues, `/backfill` and `/state` with `404`, `/state_ids` at one event, `/event` for the
//! events it holds, and `{}` for anything else (the server under test sends it its own
//! events). It signs its own requests and events with its own key, which the server under
//! test fetches from it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use hs_model::canonical::{CanonicalJsonValue, to_canonical_object};
use hs_model::signing::SigningKeyPair;
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

/// Registers `username` through the real UIA dance and returns `(user_id, access_token)`.
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

/// Initial syncs until `wanted` is true of the response, or 20 seconds have passed.
async fn sync_until(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
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

fn timeline_bodies(sync: &Value, room_id: &str) -> Vec<String> {
    sync["rooms"]["join"][room_id]["timeline"]["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The value of the metric line in `exposition` that starts with `prefix`, `0` if absent.
fn counter(exposition: &str, prefix: &str) -> u64 {
    exposition
        .lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

async fn metrics(client: &reqwest::Client, base: &str) -> String {
    client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// The stand-in for Sytest's server.
struct Peer {
    name: String,
    key: SigningKeyPair,
    /// The room version of every room this peer joins.
    version: ruma::RoomVersionId,
    /// What `/get_missing_events` answers.
    missing_events: Mutex<Vec<Value>>,
    /// `(event_id, answer)`: the one event `/state_ids` is answered at.
    state_ids: Mutex<Option<(String, Value)>>,
    /// What `/event` serves, by ID.
    events: Mutex<HashMap<String, Value>>,
    /// What `/backfill` serves; `404` while empty.
    backfill: Mutex<Vec<Value>>,
    /// Every request: `"{method} {path}?{query}"`.
    requests: Mutex<Vec<String>>,
}

impl Peer {
    fn requested(&self, needle: &str) -> bool {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(needle))
    }
}

async fn spawn_peer() -> Arc<Peer> {
    spawn_peer_for(ruma::RoomVersionId::V11).await
}

async fn spawn_peer_for(version: ruma::RoomVersionId) -> Arc<Peer> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let name = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let peer = Arc::new(Peer {
        name,
        key: SigningKeyPair::generate("1"),
        version,
        missing_events: Mutex::new(Vec::new()),
        state_ids: Mutex::new(None),
        events: Mutex::new(HashMap::new()),
        backfill: Mutex::new(Vec::new()),
        requests: Mutex::new(Vec::new()),
    });

    async fn key(State(peer): State<Arc<Peer>>) -> axum::Json<Value> {
        let keys = hs_federation::keys::OwnSigningKeys::from_keys(vec![peer.key.clone()]);
        axum::Json(
            hs_federation::keys::build_server_key_response(&peer.name, &keys, &[], 3_600_000)
                .unwrap(),
        )
    }
    async fn missing(State(peer): State<Arc<Peer>>) -> axum::Json<Value> {
        let events = peer.missing_events.lock().unwrap().clone();
        axum::Json(json!({ "events": events }))
    }
    async fn state_ids(
        State(peer): State<Arc<Peer>>,
        Query(query): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let at = query.get("event_id").cloned().unwrap_or_default();
        match peer.state_ids.lock().unwrap().clone() {
            Some((event_id, answer)) if event_id == at => axum::Json(answer).into_response(),
            _ => not_found(),
        }
    }
    async fn event(
        State(peer): State<Arc<Peer>>,
        Path(event_id): Path<String>,
    ) -> axum::response::Response {
        match peer.events.lock().unwrap().get(&event_id).cloned() {
            Some(pdu) => axum::Json(json!({
                "origin": peer.name, "origin_server_ts": 1, "pdus": [pdu],
            }))
            .into_response(),
            None => not_found(),
        }
    }
    async fn backfill(State(peer): State<Arc<Peer>>) -> axum::response::Response {
        let pdus = peer.backfill.lock().unwrap().clone();
        if pdus.is_empty() {
            return not_found();
        }
        axum::Json(json!({"origin": peer.name, "origin_server_ts": 1, "pdus": pdus}))
            .into_response()
    }
    fn not_found() -> axum::response::Response {
        (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"errcode": "M_NOT_FOUND", "error": "not here"})),
        )
            .into_response()
    }
    async fn anything() -> axum::Json<Value> {
        axum::Json(json!({}))
    }

    let recorder = peer.clone();
    let app = axum::Router::new()
        .route("/_matrix/key/v2/server", axum::routing::get(key))
        .route("/_matrix/key/v2/server/{key_id}", axum::routing::get(key))
        .route(
            "/_matrix/federation/v1/get_missing_events/{room_id}",
            axum::routing::post(missing),
        )
        .route(
            "/_matrix/federation/v1/state_ids/{room_id}",
            axum::routing::get(state_ids),
        )
        .route(
            "/_matrix/federation/v1/backfill/{room_id}",
            axum::routing::get(backfill),
        )
        .route(
            "/_matrix/federation/v1/state/{room_id}",
            axum::routing::get(|| async { not_found() }),
        )
        .route(
            "/_matrix/federation/v1/event/{event_id}",
            axum::routing::get(event),
        )
        .fallback(anything)
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let recorder = recorder.clone();
                async move {
                    let line = format!(
                        "{} {}",
                        request.method(),
                        request
                            .uri()
                            .path_and_query()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    );
                    recorder.requests.lock().unwrap().push(line);
                    next.run(request).await
                }
            },
        ))
        .with_state(peer.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    peer
}

/// `fields` hashed and signed by `peer`, as a room version 11 PDU, and its event ID.
fn pdu(peer: &Peer, fields: Value) -> (Value, String) {
    let mut object = to_canonical_object(&fields, true).unwrap();
    let hash = hs_model::hash::content_hash_base64(&object);
    object.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(
            [("sha256".to_owned(), CanonicalJsonValue::String(hash))]
                .into_iter()
                .collect(),
        ),
    );
    object.remove("signatures");
    let rules = hs_model::room_version::rules_for(&peer.version).unwrap();
    let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
    let server = ruma::ServerName::parse(&peer.name).unwrap();
    hs_model::signing::sign_object(&mut redacted, &server, &peer.key).unwrap();
    object.insert(
        "signatures".to_owned(),
        redacted.remove("signatures").unwrap(),
    );
    let value: Value =
        serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap();
    let event_id = hs_model::Event::parse(&value, peer.version.clone())
        .unwrap()
        .event_id()
        .to_string();
    (value, event_id)
}

/// A federation request from `peer` to `server`, signed.
async fn signed(
    client: &reqwest::Client,
    peer: &Peer,
    server: &Server,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let auth = hs_federation::xmatrix::sign_request(
        method,
        path,
        &peer.name,
        &server.name,
        body,
        &peer.key,
    )
    .unwrap();
    let url = format!("{}{path}", server.base);
    let request = match method {
        "GET" => client.get(url),
        "PUT" => client.put(url).json(body.unwrap()),
        other => panic!("{other}"),
    };
    let response = request.header("Authorization", auth).send().await.unwrap();
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// A room on `server` (version 11, public), created by a new user, with `bob` of `peer`
/// joined through `make_join`/`send_join`. `power` adjusts the power levels before the
/// join. Returns `(room_id, alice's token, bob's user ID, the join's ID and depth, the room's
/// state before the join as IDs, the create event's ID, the power levels' ID)`.
struct JoinedRoom {
    room_id: String,
    token: String,
    bob: String,
    join_id: String,
    join_depth: i64,
    state_before_join: Vec<String>,
    create_id: String,
    power_id: String,
    /// Whether events cite the create event (before room version 12, MSC4291).
    create_in_auth: bool,
}

impl JoinedRoom {
    /// The auth events of an ordinary event of bob's, given the power levels in force: the
    /// create event, the power levels and his join, less the create event from room version 12
    /// on (MSC4291: it is implied by the room ID).
    fn auth(&self, power: &str) -> Vec<String> {
        let mut ids = vec![
            self.create_id.clone(),
            power.to_owned(),
            self.join_id.clone(),
        ];
        if !self.create_in_auth {
            ids.remove(0);
        }
        ids
    }
}

async fn room_with_bob(
    client: &reqwest::Client,
    server: &Server,
    peer: &Peer,
    alice: &str,
    power: impl FnOnce(&mut Value, &str),
) -> JoinedRoom {
    let (_alice_id, token) = register(client, &server.base, alice).await;
    room_of_alices_with_bob(client, server, peer, token, power).await
}

/// [`room_with_bob`] for an alice already registered, by her access token.
async fn room_of_alices_with_bob(
    client: &reqwest::Client,
    server: &Server,
    peer: &Peer,
    token: String,
    power: impl FnOnce(&mut Value, &str),
) -> JoinedRoom {
    let created: Value = client
        .post(format!("{}/_matrix/client/v3/createRoom", server.base))
        .bearer_auth(&token)
        .json(&json!({"preset": "public_chat", "room_version": peer.version.as_str()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let room_id = created["room_id"].as_str().unwrap().to_owned();
    let bob = format!("@bob:{}", peer.name);
    let pl_url = format!(
        "{}/_matrix/client/v3/rooms/{room_id}/state/m.room.power_levels/",
        server.base
    );
    let mut levels: Value = client
        .get(&pl_url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    power(&mut levels, &bob);
    let status = client
        .put(&pl_url)
        .bearer_auth(&token)
        .json(&levels)
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success());

    let (status, template) = signed(
        client,
        peer,
        server,
        "GET",
        &format!(
            "/_matrix/federation/v1/make_join/{room_id}/{bob}?ver={}",
            peer.version
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{template}");
    let join_depth = template["event"]["depth"].as_i64().unwrap();
    let (join, join_id) = pdu(peer, template["event"].clone());
    let (status, answer) = signed(
        client,
        peer,
        server,
        "PUT",
        &format!("/_matrix/federation/v2/send_join/{room_id}/{join_id}"),
        Some(&join),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let mut state_before_join = Vec::new();
    let mut create_id = String::new();
    let mut power_id = String::new();
    for event in answer["state"].as_array().unwrap() {
        let parsed = hs_model::Event::parse(event, peer.version.clone()).unwrap();
        let id = parsed.event_id().to_string();
        match parsed.header().event_type.as_str() {
            "m.room.create" => create_id.clone_from(&id),
            "m.room.power_levels" => power_id.clone_from(&id),
            _ => {}
        }
        if parsed.event_id() != join_id.as_str() {
            state_before_join.push(id);
        }
    }
    peer.events.lock().unwrap().insert(join_id.clone(), join);
    // The join is in alice's sync before anything is built on it.
    let joined = room_id.clone();
    let bob_for_sync = bob.clone();
    sync_until(client, &server.base, &token, move |sync| {
        sync["rooms"]["join"][&joined]["timeline"]["events"]
            .as_array()
            .is_some_and(|events| {
                events.iter().any(|e| {
                    e["type"] == "m.room.member" && e["state_key"] == bob_for_sync.as_str()
                })
            })
    })
    .await;
    JoinedRoom {
        room_id,
        token,
        bob,
        join_id,
        join_depth,
        state_before_join,
        create_id,
        power_id,
        create_in_auth: !hs_model::room_version::rules_for(&peer.version)
            .unwrap()
            .room_create_event_id_as_room_id,
    }
}

/// Sytest's "Outbound federation requests missing prev_events and then asks for /state_ids and
/// resolves the state" (`36state.pl`), on the real server: C arrives citing X, which
/// `/get_missing_events` answers; X cites Y, which nobody sends and `/backfill` will not
/// serve. The server asks the state at Y, fetches Y and the made-up state event T it does not
/// hold, holds Y with that state, and takes X and C; alice's sync has both, the room's state
/// has T and Y, and the fallback is counted and logged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_prev_event_is_taken_with_the_state_the_sending_server_answers_for_it() {
    missing_prev_event_taken_with_state(ruma::RoomVersionId::V11).await;
}

/// The same in room version 12, where no event cites the create event (MSC4291): an outlier
/// there was refused for "no create event" before 2026-10-04.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_prev_event_is_taken_with_its_state_in_room_version_12() {
    missing_prev_event_taken_with_state(ruma::RoomVersionId::V12).await;
}

async fn missing_prev_event_taken_with_state(version: ruma::RoomVersionId) {
    let server = start().await;
    let peer = spawn_peer_for(version).await;
    let client = reqwest::Client::new();
    let room = room_with_bob(&client, &server, &peer, "alice", |levels, bob| {
        levels["users"][bob] = json!(100);
    })
    .await;
    let r = &room;
    let event = |fields: Value| pdu(&peer, fields);
    let base = |kind: &str, state_key: Option<&str>, prev: Vec<&str>, depth: i64, body: &str| {
        let mut fields = json!({
            "type": kind, "sender": r.bob, "room_id": r.room_id,
            "origin_server_ts": 1_000 + depth, "depth": depth,
            "content": {"body": body},
            "prev_events": prev,
            "auth_events": r.auth(&r.power_id),
        });
        if let Some(key) = state_key {
            fields["state_key"] = json!(key);
        }
        fields
    };
    let (y, y_id) = event(base(
        "test_state",
        Some("Y"),
        vec![&r.join_id],
        r.join_depth + 1,
        "event_y",
    ));
    let (t, t_id) = event(base(
        "test_state",
        Some("T"),
        vec![&r.join_id],
        r.join_depth + 1,
        "how now",
    ));
    let (x, x_id) = event(base(
        "m.room.message",
        None,
        vec![&r.join_id, &y_id],
        r.join_depth + 2,
        "event_x",
    ));
    let (c, c_id) = event(base(
        "m.room.message",
        None,
        vec![&r.join_id, &x_id],
        r.join_depth + 3,
        "event_c",
    ));

    let mut state_at_y = r.state_before_join.clone();
    state_at_y.push(r.join_id.clone());
    state_at_y.push(t_id.clone());
    *peer.missing_events.lock().unwrap() = vec![x.clone()];
    *peer.state_ids.lock().unwrap() = Some((
        y_id.clone(),
        json!({"pdu_ids": state_at_y, "auth_chain_ids": []}),
    ));
    peer.events
        .lock()
        .unwrap()
        .extend([(y_id.clone(), y), (t_id.clone(), t), (x_id.clone(), x)]);

    let before = counter(
        &metrics(&client, &server.base).await,
        "hs_federation_state_fallbacks_total{outcome=\"resolved\"}",
    );
    let (status, answer) = signed(
        &client,
        &peer,
        &server,
        "PUT",
        "/_matrix/federation/v1/send/txn-c",
        Some(&json!({"origin": peer.name, "origin_server_ts": 1, "pdus": [c], "edus": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["pdus"][&c_id], json!({}), "{answer}");
    assert!(peer.requested("/get_missing_events/"));
    assert!(
        peer.requested(&format!("/state_ids/{}?event_id=", r.room_id)),
        "the state at Y was asked for: {:?}",
        peer.requests.lock().unwrap()
    );
    assert!(peer.requested(&format!("/event/{y_id}")));
    assert!(peer.requested(&format!("/event/{t_id}")));

    let room_id = r.room_id.clone();
    sync_until(&client, &server.base, &r.token, move |sync| {
        let bodies = timeline_bodies(sync, &room_id);
        bodies.iter().any(|b| b == "event_x") && bodies.iter().any(|b| b == "event_c")
    })
    .await;

    let state: Vec<Value> = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{}/state",
            server.base, r.room_id
        ))
        .bearer_auth(&r.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id_of = |kind: &str, key: &str| {
        state
            .iter()
            .find(|e| e["type"] == kind && e["state_key"] == key)
            .and_then(|e| e["event_id"].as_str())
            .map(str::to_owned)
    };
    assert_eq!(id_of("test_state", "Y"), Some(y_id.clone()), "{state:?}");
    assert_eq!(id_of("test_state", "T"), Some(t_id.clone()), "{state:?}");
    assert_eq!(id_of("m.room.power_levels", ""), Some(r.power_id.clone()));

    // The state at X, as this server answers it, has Y and T.
    let (status, at_x) = signed(
        &client,
        &peer,
        &server,
        "GET",
        &format!(
            "/_matrix/federation/v1/state_ids/{}?event_id={x_id}",
            r.room_id
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{at_x}");
    let at_x: Vec<&str> = at_x["pdu_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        at_x.contains(&y_id.as_str()) && at_x.contains(&t_id.as_str()),
        "{at_x:?}"
    );

    // Y is an outlier: no state is served at it (Sytest's "/state[_ids] returns M_NOT_FOUND for
    // an outlier").
    for endpoint in ["state", "state_ids"] {
        let (status, body) = signed(
            &client,
            &peer,
            &server,
            "GET",
            &format!(
                "/_matrix/federation/v1/{endpoint}/{}?event_id={y_id}",
                r.room_id
            ),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "/{endpoint} at the outlier: {body}"
        );
    }

    let after = counter(
        &metrics(&client, &server.base).await,
        "hs_federation_state_fallbacks_total{outcome=\"resolved\"}",
    );
    assert!(
        after > before,
        "the fallback is counted: {before} -> {after}"
    );
    server.handle.shutdown().await;
}

/// Sytest's "Federation rejects inbound events where the prev_events cannot be found": an
/// event sent directly whose missing prev event `/get_missing_events` will not divulge is
/// refused, and the state at that prev event is never asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_whose_prev_event_nobody_divulges_is_refused_without_asking_the_state() {
    let server = start().await;
    let peer = spawn_peer().await;
    let client = reqwest::Client::new();
    let r = room_with_bob(&client, &server, &peer, "alice", |_, _| {}).await;
    let fields = |prev: &str, depth: i64, body: &str| {
        json!({
            "type": "m.room.message", "sender": r.bob, "room_id": r.room_id,
            "origin_server_ts": 1_000 + depth, "depth": depth,
            "content": {"body": body},
            "prev_events": [prev],
            "auth_events": r.auth(&r.power_id),
        })
    };
    let (_missing, missing_id) = pdu(&peer, fields(&r.join_id, r.join_depth + 1, "Message 1"));
    let (sent, sent_id) = pdu(&peer, fields(&missing_id, r.join_depth + 2, "Message 2"));
    let (status, answer) = signed(
        &client,
        &peer,
        &server,
        "PUT",
        "/_matrix/federation/v1/send/txn-orphan",
        Some(&json!({"origin": peer.name, "origin_server_ts": 1, "pdus": [sent], "edus": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(answer["pdus"][&sent_id]["error"].is_string(), "{answer}");
    assert!(peer.requested("/get_missing_events/"));
    assert!(
        !peer.requested("/state_ids/"),
        "the state at a prev event nobody divulged was asked for: {:?}",
        peer.requests.lock().unwrap()
    );
    server.handle.shutdown().await;
}

/// Sytest's "Inbound federation correctly soft fails events" (`52soft-fail.pl`), on the real
/// server: after alice raises `m.room.message` to 50, bob's message C citing the join (the
/// state before the change) is accepted (`{}`) and soft failed -- not in alice's sync, `404`
/// to her, served to federation -- and D, another type citing C and the power levels, is in
/// alice's sync. The soft failure is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_the_current_state_refuses_is_soft_failed_and_kept_from_clients() {
    let server = start().await;
    let peer = spawn_peer().await;
    let client = reqwest::Client::new();
    let r = room_with_bob(&client, &server, &peer, "alice", |_, _| {}).await;
    let pl_url = format!(
        "{}/_matrix/client/v3/rooms/{}/state/m.room.power_levels/",
        server.base, r.room_id
    );
    let mut levels: Value = client
        .get(&pl_url)
        .bearer_auth(&r.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    levels["events"]["m.room.message"] = json!(50);
    let changed: Value = client
        .put(&pl_url)
        .bearer_auth(&r.token)
        .json(&levels)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let new_power = changed["event_id"].as_str().unwrap().to_owned();
    let before = counter(
        &metrics(&client, &server.base).await,
        "hs_room_soft_failed_events_total",
    );

    let (c, c_id) = pdu(
        &peer,
        json!({
            "type": "m.room.message", "sender": r.bob, "room_id": r.room_id,
            "origin_server_ts": 2_000, "depth": r.join_depth + 1,
            "content": {"body": "Denied"},
            "prev_events": [r.join_id],
            "auth_events": r.auth(&r.power_id),
        }),
    );
    let (d, d_id) = pdu(
        &peer,
        json!({
            "type": "m.room.other_message_type", "sender": r.bob, "room_id": r.room_id,
            "origin_server_ts": 2_001, "depth": r.join_depth + 10,
            "content": {"body": "Allowed"},
            "prev_events": [c_id, new_power],
            "auth_events": r.auth(&new_power),
        }),
    );
    for (txn, event, id) in [("txn-c", c, &c_id), ("txn-d", d, &d_id)] {
        let (status, answer) = signed(
            &client,
            &peer,
            &server,
            "PUT",
            &format!("/_matrix/federation/v1/send/{txn}"),
            Some(&json!({"origin": peer.name, "origin_server_ts": 1, "pdus": [event], "edus": []})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["pdus"][id.as_str()], json!({}), "{answer}");
    }

    let room_id = r.room_id.clone();
    let sync = sync_until(&client, &server.base, &r.token, move |sync| {
        timeline_bodies(sync, &room_id)
            .iter()
            .any(|b| b == "Allowed")
    })
    .await;
    assert!(
        !timeline_bodies(&sync, &r.room_id)
            .iter()
            .any(|b| b == "Denied"),
        "the soft-failed message reached alice: {sync}"
    );
    let status = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{}/event/{c_id}",
            server.base, r.room_id
        ))
        .bearer_auth(&r.token)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 404);
    let (status, served) = signed(
        &client,
        &peer,
        &server,
        "GET",
        &format!("/_matrix/federation/v1/event/{c_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a soft-failed event is served to federation: {served}"
    );
    let after = counter(
        &metrics(&client, &server.base).await,
        "hs_room_soft_failed_events_total",
    );
    assert!(
        after > before,
        "the soft failure is counted: {before} -> {after}"
    );
    server.handle.shutdown().await;
}

/// Sytest's "Backfilled events whose prev_events are in a different room do not allow
/// cross-room back-pagination" (`34room-backfill.pl`), on the real server. Bob's P is in room
/// one; in room two, Q cites P as its prev event, R cites Q and S cites R. S is sent; R comes
/// from `/get_missing_events`, Q through the state fallback (its own prev event is nobody's
/// to give). A backward `/messages` page from S stops at the outlier Q and asks `/backfill`
/// for it -- before 2026-10-04 it walked straight from R to the join, asking nothing -- and
/// the page holds Q and never P, which this server holds in room one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_pagination_backfills_past_a_fetched_prev_event_and_never_crosses_rooms() {
    let server = start().await;
    let peer = spawn_peer().await;
    let client = reqwest::Client::new();
    let (_alice, token) = register(&client, &server.base, "alice").await;
    let one = room_of_alices_with_bob(&client, &server, &peer, token.clone(), |_, _| {}).await;
    let two = room_of_alices_with_bob(&client, &server, &peer, token.clone(), |_, _| {}).await;
    let message = |room: &JoinedRoom, prev: Vec<&str>, depth: i64, body: &str| {
        pdu(
            &peer,
            json!({
                "type": "m.room.message", "sender": room.bob, "room_id": room.room_id,
                "origin_server_ts": 1_000 + depth, "depth": depth,
                "content": {"body": body},
                "prev_events": prev,
                "auth_events": room.auth(&room.power_id),
            }),
        )
    };
    let (p, p_id) = message(&one, vec![&one.join_id], one.join_depth + 1, "event P");
    let depth = one.join_depth.max(two.join_depth);
    let (q, q_id) = message(&two, vec![&p_id], depth + 2, "event Q");
    let (r, r_id) = message(&two, vec![&q_id], depth + 3, "event R");
    let (s, s_id) = message(&two, vec![&r_id], depth + 4, "event S");

    let mut state_at_q = two.state_before_join.clone();
    state_at_q.push(two.join_id.clone());
    *peer.missing_events.lock().unwrap() = vec![r.clone()];
    *peer.state_ids.lock().unwrap() = Some((
        q_id.clone(),
        json!({"pdu_ids": state_at_q, "auth_chain_ids": []}),
    ));
    peer.events
        .lock()
        .unwrap()
        .extend([(q_id.clone(), q.clone()), (r_id.clone(), r)]);
    *peer.backfill.lock().unwrap() = vec![q];

    for (txn, event, id) in [("txn-s", s, &s_id), ("txn-p", p, &p_id)] {
        let (status, answer) = signed(
            &client,
            &peer,
            &server,
            "PUT",
            &format!("/_matrix/federation/v1/send/{txn}"),
            Some(&json!({"origin": peer.name, "origin_server_ts": 1, "pdus": [event], "edus": []})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["pdus"][id], json!({}), "{answer}");
    }
    assert!(peer.requested(&format!("/state_ids/{}?event_id=", two.room_id)));

    let room_id = two.room_id.clone();
    let filter = r#"{"room":{"timeline":{"limit":2}}}"#;
    let deadline = Instant::now() + Duration::from_secs(20);
    let prev_batch = loop {
        let sync: Value = client
            .get(format!("{}/_matrix/client/v3/sync", server.base))
            .query(&[("filter", filter)])
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let bodies = timeline_bodies(&sync, &room_id);
        if bodies.iter().any(|b| b == "event S") {
            break sync["rooms"]["join"][&room_id]["timeline"]["prev_batch"]
                .as_str()
                .unwrap()
                .to_owned();
        }
        assert!(Instant::now() < deadline, "S never reached alice's sync");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let page: Value = client
        .get(format!(
            "{}/_matrix/client/v3/rooms/{room_id}/messages",
            server.base
        ))
        .query(&[("dir", "b"), ("from", prev_batch.as_str())])
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        peer.requested(&format!("/backfill/{room_id}")),
        "the page did not ask for the history before Q: {:?}",
        peer.requests.lock().unwrap()
    );
    let chunk = page["chunk"].as_array().unwrap();
    assert!(chunk.len() >= 2, "{page}");
    let bodies: Vec<&str> = chunk
        .iter()
        .filter_map(|e| e["content"]["body"].as_str())
        .collect();
    assert!(bodies.contains(&"event Q"), "{page}");
    assert!(!bodies.contains(&"event P"), "{page}");
    assert!(
        chunk.iter().all(|e| e["room_id"] == room_id.as_str()),
        "{page}"
    );
    server.handle.shutdown().await;
}

/// Sytest's "Inbound federation redacts events from erased users" (`32room-getevent.pl`), on
/// the real server: another server fetching alice's message over `/event` gets it whole, and
/// once alice has deactivated her account with `erase`, gets it redacted -- still the same
/// event, with no `body`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_erased_accounts_events_are_served_to_other_servers_redacted() {
    let server = start().await;
    let peer = spawn_peer().await;
    let client = reqwest::Client::new();
    let r = room_with_bob(&client, &server, &peer, "alice", |_, _| {}).await;
    let sent: Value = client
        .put(format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/erase-1",
            server.base, r.room_id
        ))
        .bearer_auth(&r.token)
        .json(&json!({"msgtype": "m.text", "body": "body1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let event_id = sent["event_id"].as_str().unwrap().to_owned();
    let path = format!("/_matrix/federation/v1/event/{event_id}");
    let (status, before) = signed(&client, &peer, &server, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(before["pdus"][0]["content"]["body"], "body1", "{before}");

    let status = client
        .post(format!("{}/_matrix/client/v3/account/deactivate", server.base))
        .bearer_auth(&r.token)
        .json(&json!({
            "erase": true,
            "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "correct horse"},
        }))
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success(), "{status}");

    let (status, after) = signed(&client, &peer, &server, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    let pdu = &after["pdus"][0];
    assert_eq!(pdu["type"], "m.room.message", "{after}");
    assert!(
        pdu["content"].get("body").is_none(),
        "not redacted: {after}"
    );
    let parsed = hs_model::Event::parse(pdu, peer.version.clone()).unwrap();
    assert_eq!(parsed.event_id(), event_id.as_str(), "the same event");
    server.handle.shutdown().await;
}

/// Complement's `TestInboundFederationRejectsEventsWithRejectedAuthEvents`, on the real server:
/// bob's power levels are rejected (he has no power); an outlier O (his membership, citing the
/// rejected power levels among its auth events) is never sent, only served by `/event`; E1
/// cites O among its auth events. The server fetches O by `/event` -- not by
/// `/get_missing_events` or `/backfill`, which are for prev events -- holds it rejected, and
/// rejects E1 (`{}` to `/send`, `404` to alice); the sentinel after it reaches alice's sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_auth_event_is_fetched_by_id_and_its_rejection_carries_over() {
    let server = start().await;
    let peer = spawn_peer().await;
    let client = reqwest::Client::new();
    let r = room_with_bob(&client, &server, &peer, "alice", |_, _| {}).await;
    let join_rules = {
        let state: Vec<Value> = client
            .get(format!(
                "{}/_matrix/client/v3/rooms/{}/state",
                server.base, r.room_id
            ))
            .bearer_auth(&r.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        state
            .iter()
            .find(|e| e["type"] == "m.room.join_rules")
            .and_then(|e| e["event_id"].as_str())
            .unwrap()
            .to_owned()
    };
    let d = r.join_depth;
    let (pl, pl_id) = pdu(
        &peer,
        json!({
            "type": "m.room.power_levels", "state_key": "", "sender": r.bob,
            "room_id": r.room_id, "origin_server_ts": 1_001, "depth": d + 1,
            "content": {"users": {}},
            "prev_events": [r.join_id], "auth_events": r.auth(&r.power_id),
        }),
    );
    let (outlier, outlier_id) = pdu(
        &peer,
        json!({
            "type": "m.room.member", "state_key": r.bob, "sender": r.bob,
            "room_id": r.room_id, "origin_server_ts": 1_002, "depth": d + 1,
            "content": {"membership": "join", "test": 1},
            "prev_events": [r.join_id],
            "auth_events": [r.create_id, join_rules, pl_id, r.join_id],
        }),
    );
    let mut e1_auth = r.auth(&r.power_id);
    e1_auth.push(outlier_id.clone());
    let (e1, e1_id) = pdu(
        &peer,
        json!({
            "type": "m.room.message", "sender": r.bob, "room_id": r.room_id,
            "origin_server_ts": 1_003, "depth": d + 1, "content": {"body": "sent 1"},
            "prev_events": [r.join_id], "auth_events": e1_auth,
        }),
    );
    let (sentinel, sentinel_id) = pdu(
        &peer,
        json!({
            "type": "m.room.message", "sender": r.bob, "room_id": r.room_id,
            "origin_server_ts": 1_004, "depth": d + 2, "content": {"body": "sentinel"},
            "prev_events": [e1_id], "auth_events": r.auth(&r.power_id),
        }),
    );
    peer.events
        .lock()
        .unwrap()
        .insert(outlier_id.clone(), outlier);

    for (txn, pdus) in [("txn-pl", vec![pl]), ("txn-e", vec![e1, sentinel])] {
        let (status, answer) = signed(
            &client,
            &peer,
            &server,
            "PUT",
            &format!("/_matrix/federation/v1/send/{txn}"),
            Some(&json!({"origin": peer.name, "origin_server_ts": 1, "pdus": pdus, "edus": []})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        for (_, result) in answer["pdus"].as_object().unwrap() {
            assert_eq!(result, &json!({}), "{answer}");
        }
    }
    assert!(peer.requested(&format!("/event/{outlier_id}")));
    assert!(
        !peer.requested("/get_missing_events/") && !peer.requested("/backfill/"),
        "a missing auth event was walked for: {:?}",
        peer.requests.lock().unwrap()
    );
    let room_id = r.room_id.clone();
    sync_until(&client, &server.base, &r.token, move |sync| {
        timeline_bodies(sync, &room_id)
            .iter()
            .any(|b| b == "sentinel")
    })
    .await;
    for id in [&pl_id, &outlier_id, &e1_id] {
        let status = client
            .get(format!(
                "{}/_matrix/client/v3/rooms/{}/event/{id}",
                server.base, r.room_id
            ))
            .bearer_auth(&r.token)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status.as_u16(), 404, "{id} is visible");
    }
    let _ = sentinel_id;
    server.handle.shutdown().await;
}
