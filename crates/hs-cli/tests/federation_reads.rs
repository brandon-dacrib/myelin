//! The federation read endpoints, driven against **real room data** through the **real**
//! `X-Matrix` verification layer.
//!
//! `hs-federation`'s own handler tests run against its in-memory fakes, which is the right place
//! to test the handlers; what they cannot test is whether the adapter onto `hs-room`
//! (`hs_cli::federation::RegistryRoomSource`) answers those handlers correctly. That is what this
//! file does: it creates a room through the same `RoomRegistry` `hs serve` uses, writes events
//! into it, and then asks for them the way another homeserver would -- over the composed
//! federation router, with a signed `Authorization: X-Matrix` header that the layer verifies
//! against the remote's published key.
//!
//! The "remote server" here is a signing key plus a name. Its `/_matrix/key/v2/server` response
//! is served by a fixed fetcher rather than over the network, which is the only part of the path
//! that is faked: the signature the layer checks is a real Ed25519 signature over the real
//! canonical signing object, and flipping one byte of it fails the request.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hs_kv::memory::MemoryBackend;
use hs_model::signing::SigningKeyPair;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use serde_json::Value;
use tower::ServiceExt as _;

const US: &str = "local.example";
const REMOTE: &str = "remote.example";

/// Serves the remote's own key response to the verifier, in place of a network fetch.
struct FixedKeys {
    server_name: String,
    keys: hs_federation::keys::OwnSigningKeys,
}

#[async_trait]
impl hs_federation::keys::KeyServerFetcher for FixedKeys {
    async fn fetch_server_key(&self, server_name: &str) -> Option<Value> {
        if server_name != self.server_name {
            return None;
        }
        hs_federation::keys::build_server_key_response(&self.server_name, &self.keys, &[], 3600)
            .ok()
    }
}

struct Harness {
    router: axum::Router,
    remote_key: SigningKeyPair,
    rooms: Arc<RoomRegistry<MemoryBackend>>,
}

impl Harness {
    async fn new() -> Self {
        let backend = MemoryBackend::new();
        let identity = HomeserverIdentity::for_tests(US);
        let rooms = Arc::new(RoomRegistry::open(backend.clone(), identity).expect("registry"));

        let user_store: hs_user::store::DynUserStore = Arc::new(
            hs_user::store::tables::TablesUserStore::open(backend.clone()).expect("user store"),
        );
        let auth_store: Arc<dyn hs_auth::store::AuthStore> = Arc::new(
            hs_auth::store::tables::TablesAuthStore::open(backend.clone()).expect("auth store"),
        );
        let e2e_store: Arc<dyn hs_e2e::store::E2eStore> = Arc::new(
            hs_e2e::store::tables::TablesE2eStore::open(backend.clone()).expect("e2e store"),
        );

        let state = hs_federation::transport::FederationState {
            own_server_name: Arc::from(US),
            rooms: Arc::new(hs_cli::federation::RegistryRoomSource::new(
                rooms.clone(),
                user_store,
            )),
            queries: Arc::new(hs_cli::federation::ServerQuerySource::new(
                auth_store,
                e2e_store,
                rooms.clone(),
                US,
            )),
            allow_public_rooms_over_federation: true,
            allow_device_name_lookup_over_federation: true,
            write_sink: Arc::new(hs_cli::federation::RegistryWriteSink::new(rooms.clone())),
            transactions: Arc::new(hs_federation::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: hs_federation::backfill::BackfillLimits::default(),
        };

        let remote_key = SigningKeyPair::generate("a_remote");
        let fetcher = FixedKeys {
            server_name: REMOTE.to_string(),
            keys: hs_federation::keys::OwnSigningKeys::from_keys(vec![remote_key.clone()]),
        };
        let key_cache: Arc<hs_federation::keys::DynRemoteKeyCache> =
            Arc::new(hs_federation::keys::RemoteKeyCache::new(
                Box::new(fetcher) as Box<dyn hs_federation::keys::KeyServerFetcher>
            ));
        let ctx = Arc::new(hs_federation::xmatrix::XMatrixContext {
            own_server_name: US.to_string(),
            key_cache,
        });

        let (router, _manifest) = hs_federation::transport::router(state, ctx);
        // Mounted exactly where `hs serve` mounts it, prefix and all. That is not incidental:
        // `axum::Router::nest` rewrites the URI the inner layers see, so a verifier that signs
        // over the rewritten path disagrees with every real sender. Driving the router at the
        // root here would hide that.
        let router = axum::Router::new().nest("/_matrix/federation/v1", router);
        Self {
            router,
            remote_key,
            rooms,
        }
    }

    /// Sends a GET as the remote server would: signed over the spec-relative URI, which is the
    /// full `/_matrix/federation/v1/...` path even though the router itself is mounted at the
    /// root here.
    async fn signed_get(&self, path: &str) -> (StatusCode, Value) {
        let uri = format!("/_matrix/federation/v1{path}");
        let auth =
            hs_federation::xmatrix::sign_request("GET", &uri, REMOTE, US, None, &self.remote_key)
                .expect("signing");
        let request = Request::builder()
            .method("GET")
            .uri(&uri)
            .header("Authorization", auth)
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    async fn signed_post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let uri = format!("/_matrix/federation/v1{path}");
        let auth = hs_federation::xmatrix::sign_request(
            "POST",
            &uri,
            REMOTE,
            US,
            Some(&body),
            &self.remote_key,
        )
        .expect("signing");
        let request = Request::builder()
            .method("POST")
            .uri(&uri)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    /// The room's `m.room.create` event ID, read from the room itself. It cannot be read off a
    /// federation response: a PDU for any room version this server creates carries no `event_id`
    /// field, by design -- the recipient computes it from the reference hash.
    async fn create_event_id(&self, room_id: &str) -> String {
        let parsed = ruma::RoomId::parse(room_id).unwrap();
        let handle = self.rooms.get_or_load(&parsed).await.unwrap();
        handle
            .query(|actor| {
                actor
                    .state_event("m.room.create", "")
                    .ok()
                    .flatten()
                    .map(|event| event.event_id().to_string())
            })
            .await
            .expect("every room has a create event")
    }

    /// Creates a room owned by a local user, with one message in it. Returns
    /// `(room_id, message_event_id)`.
    async fn room_with_a_message(&self, world_readable: bool) -> (String, String) {
        let creator = ruma::UserId::parse(format!("@alice:{US}")).unwrap();
        let handle = self
            .rooms
            .create_room(
                creator.clone(),
                hs_room::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1_000,
            )
            .await
            .expect("room creation");

        if world_readable {
            handle
                .send_event(
                    creator.clone(),
                    "m.room.history_visibility".to_owned(),
                    Some(String::new()),
                    serde_json::json!({ "history_visibility": "world_readable" }),
                    None,
                    2_000,
                )
                .await
                .expect("history visibility");
        }

        let message = handle
            .send_event(
                creator,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({ "msgtype": "m.text", "body": "over federation" }),
                None,
                3_000,
            )
            .await
            .expect("message");

        let room_id = handle.query(|actor| actor.room_id().to_string()).await;
        (room_id, message.event_id().to_string())
    }
}

#[tokio::test]
async fn an_unsigned_request_never_reaches_a_handler() {
    let harness = Harness::new().await;
    let request = Request::builder()
        .method("GET")
        .uri("/_matrix/federation/v1/version")
        .body(Body::empty())
        .unwrap();
    let response = harness.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_signed_request_is_accepted_and_a_tampered_one_is_not() {
    let harness = Harness::new().await;

    let (status, body) = harness.signed_get("/version").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The same request with one character of the signature changed must fail: this is what makes
    // the test above a signature check rather than a "header is present" check.
    let uri = "/_matrix/federation/v1/version";
    let auth =
        hs_federation::xmatrix::sign_request("GET", uri, REMOTE, US, None, &harness.remote_key)
            .unwrap();
    let tampered = if auth.contains("AAAA") {
        auth.replace("AAAA", "BBBB")
    } else {
        // Flip the first character of the base64 signature value.
        let (head, tail) = auth.split_at(auth.find("sig=\"").unwrap() + 5);
        let mut tail = tail.to_string();
        let first = tail.remove(0);
        format!("{head}{}{tail}", if first == 'A' { 'B' } else { 'A' })
    };
    let request = Request::builder()
        .method("GET")
        .uri("/_matrix/federation/v1/version")
        .header("Authorization", tampered)
        .body(Body::empty())
        .unwrap();
    let response = harness.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_room_with_no_remote_member_is_invisible_to_that_remote() {
    let harness = Harness::new().await;
    let (room_id, event_id) = harness.room_with_a_message(false).await;

    let (status, _) = harness.signed_get(&format!("/event/{event_id}")).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a server with no member in the room must not be able to read its events"
    );

    let (status, _) = harness
        .signed_get(&format!("/state/{room_id}?event_id={event_id}"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_world_readable_room_serves_its_events_as_full_pdus() {
    let harness = Harness::new().await;
    let (room_id, event_id) = harness.room_with_a_message(true).await;

    let (status, body) = harness.signed_get(&format!("/event/{event_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let pdu = &body["pdus"][0];
    assert_eq!(pdu["room_id"], room_id);
    assert_eq!(pdu["type"], "m.room.message");
    assert_eq!(pdu["content"]["body"], "over federation");
    // A federation PDU carries the material a remote needs to verify it -- which the
    // client-facing rendering strips.
    assert!(
        pdu.get("signatures").is_some(),
        "PDU must carry signatures: {pdu}"
    );
    assert!(pdu.get("hashes").is_some(), "PDU must carry hashes: {pdu}");
    assert!(
        pdu.get("auth_events").is_some(),
        "PDU must carry auth_events: {pdu}"
    );
    assert!(
        pdu.get("prev_events").is_some(),
        "PDU must carry prev_events: {pdu}"
    );
}

#[tokio::test]
async fn state_is_served_for_any_event_including_historical_ones() {
    let harness = Harness::new().await;
    let (room_id, newest) = harness.room_with_a_message(true).await;

    let (status, body) = harness
        .signed_get(&format!("/state/{room_id}?event_id={newest}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pdus = body["pdus"].as_array().expect("pdus");
    assert!(
        pdus.iter().any(|e| e["type"] == "m.room.create"),
        "the room's state must include its create event: {pdus:?}"
    );
    assert!(
        !body["auth_chain"]
            .as_array()
            .expect("auth_chain")
            .is_empty(),
        "state must come with the auth chain needed to check it"
    );

    // State at the create event is the room's state *then*, not now. This is the case the adapter
    // used to refuse outright rather than answer with current state; `RoomActor::state_at_event`
    // is what made answering it possible, and the assertion that the two differ is what proves
    // this is real history and not the current-state map wearing a different event ID.
    let create_id = harness.create_event_id(&room_id).await;

    let (status, early) = harness
        .signed_get(&format!("/state/{room_id}?event_id={create_id}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{early}");
    let early_pdus = early["pdus"].as_array().expect("pdus");
    assert!(
        early_pdus.len() < pdus.len(),
        "state at the create event must be smaller than current state: {} vs {}",
        early_pdus.len(),
        pdus.len()
    );
    assert!(
        !early_pdus
            .iter()
            .any(|e| e["type"] == "m.room.history_visibility"),
        "history_visibility was set after the create event, so it is not in the state then: {early_pdus:?}"
    );

    // An event this server does not have is still a 404, not an empty state map.
    let (status, _) = harness
        .signed_get(&format!("/state/{room_id}?event_id=$nonexistent"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn backfill_walks_the_timeline_backwards_from_the_live_end() {
    let harness = Harness::new().await;
    let (room_id, message_id) = harness.room_with_a_message(true).await;

    let (status, body) = harness
        .signed_get(&format!("/backfill/{room_id}?limit=10"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pdus = body["pdus"].as_array().expect("pdus");
    assert!(
        pdus.iter().any(|e| e["type"] == "m.room.message"),
        "backfill should reach the message: {pdus:?}"
    );

    // Asked to start from the message, backfill returns it and what came before, newest first.
    let (status, body) = harness
        .signed_get(&format!("/backfill/{room_id}?limit=3&v={message_id}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pdus = body["pdus"].as_array().expect("pdus");
    assert!(pdus.len() <= 3, "the server-side limit must be honoured");
}

#[tokio::test]
async fn the_auth_chain_of_an_event_is_the_events_it_transitively_cites() {
    let harness = Harness::new().await;
    let (room_id, message_id) = harness.room_with_a_message(true).await;

    let (status, body) = harness
        .signed_get(&format!("/event_auth/{room_id}/{message_id}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let chain = body["auth_chain"].as_array().expect("auth_chain");
    assert!(
        chain.iter().any(|e| e["type"] == "m.room.create"),
        "every event's auth chain reaches the create event: {chain:?}"
    );
    assert!(
        !chain.iter().any(|e| e["event_id"] == message_id.as_str()),
        "an event is not part of its own auth chain"
    );
}

/// `/get_missing_events` answers oldest-first, the order the events actually happened in.
///
/// The walk that finds them runs backwards from what the caller has, so the natural order is
/// newest-first — and a requesting server replays the batch into its own DAG assuming the
/// opposite. Complement reads `*ev.StateKey()` off the first entry: when that is a message rather
/// than a state event it dereferences a nil pointer, which kills the Go test binary and silently
/// discards every test scheduled after it. That is why the whole federation suite has been run
/// with `-skip TestInboundCanReturnMissingEvents`.
#[tokio::test]
async fn missing_events_come_back_oldest_first() {
    let harness = Harness::new().await;
    let (room_id, message_id) = harness.room_with_a_message(true).await;
    let create_id = harness.create_event_id(&room_id).await;

    let (status, body) = harness
        .signed_post(
            &format!("/get_missing_events/{room_id}"),
            serde_json::json!({
                "earliest_events": [create_id],
                "latest_events": [message_id],
                "limit": 10,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let events = body["events"].as_array().expect("events");
    assert!(
        !events.is_empty(),
        "the gap between the create event and the message is not empty: {body}"
    );

    let depths: Vec<i64> = events
        .iter()
        .map(|e| e["depth"].as_i64().expect("every PDU carries a depth"))
        .collect();
    let mut sorted = depths.clone();
    sorted.sort_unstable();
    assert_eq!(
        depths, sorted,
        "events must be ordered by depth ascending, oldest first: {depths:?}"
    );

    // The specific shape Complement asserts on: the first event out of the gap after
    // `m.room.create` is the creator's own join, a state event. A response whose first entry has
    // no state key is what crashes it.
    assert_eq!(
        events[0]["type"], "m.room.member",
        "the oldest event after the create is the creator's join: {events:?}"
    );
    assert!(
        events[0]["state_key"].is_string(),
        "the first event must have a state key, or Complement dereferences nil: {events:?}"
    );

    // Neither bound is echoed back: the caller said it has both.
    for event in events {
        assert_ne!(event["type"], "m.room.create", "{event}");
    }
}
