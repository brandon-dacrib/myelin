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

        let auth_store: Arc<dyn hs_auth::store::AuthStore> = Arc::new(
            hs_auth::store::tables::TablesAuthStore::open(backend.clone()).expect("auth store"),
        );
        let e2e_store: Arc<dyn hs_e2e::store::E2eStore> = Arc::new(
            hs_e2e::store::tables::TablesE2eStore::open(backend.clone()).expect("e2e store"),
        );

        let state = hs_federation::transport::FederationState {
            own_server_name: Arc::from(US),
            rooms: Arc::new(hs_cli::federation::RegistryRoomSource::new(rooms.clone())),
            queries: Arc::new(hs_cli::federation::ServerQuerySource::new(
                auth_store,
                e2e_store,
                rooms.clone(),
                US,
            )),
            policy: hs_federation::transport::InboundPolicy::new(true, true),
            write_sink: Arc::new(hs_cli::federation::RegistryWriteSink::new(rooms.clone())),
            transactions: Arc::new(hs_federation::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: hs_federation::backfill::BackfillLimits::default(),
            sender: None,
            invites: None,
            edu_sink: None,
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

        let (router, _manifest) = hs_federation::transport::router(state.clone(), ctx.clone());
        let (router_v2, _manifest) = hs_federation::transport::router_v2(state, ctx);
        // Mounted exactly where `hs serve` mounts it, prefix and all. That is not incidental:
        // `axum::Router::nest` rewrites the URI the inner layers see, so a verifier that signs
        // over the rewritten path disagrees with every real sender. Driving the router at the
        // root here would hide that.
        let router = axum::Router::new()
            .nest("/_matrix/federation/v1", router)
            .nest("/_matrix/federation/v2", router_v2);
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

    /// Sends a PUT as the remote server would, to `/_matrix/federation/{version}{path}`.
    async fn signed_put(&self, version: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let uri = format!("/_matrix/federation/{version}{path}");
        let auth = hs_federation::xmatrix::sign_request(
            "PUT",
            &uri,
            REMOTE,
            US,
            Some(&body),
            &self.remote_key,
        )
        .expect("signing");
        let request = Request::builder()
            .method("PUT")
            .uri(&uri)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    /// Signs a membership template the way the remote server would: `event_id` of its own making
    /// (rooms of version 1 and 2), the content hash, then the signature over the redacted form.
    fn sign_as_remote(&self, template: &Value, room_version: &ruma::RoomVersionId) -> Value {
        use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
        let rules = hs_model::room_version::rules_for(room_version).unwrap();
        let mut object = to_canonical_object(template, rules.strict_canonical_json).unwrap();
        let remote = ruma::ServerName::parse(REMOTE).unwrap();
        if rules.event_format_requires_event_id {
            object.insert(
                "event_id".to_owned(),
                CanonicalJsonValue::String(ruma::EventId::new_v1(&remote).to_string()),
            );
        }
        let hash = hs_model::hash::content_hash_base64(&object);
        object.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(hash),
            )])),
        );
        let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
        hs_model::signing::sign_object(&mut redacted, &remote, &self.remote_key).unwrap();
        object.insert(
            "signatures".to_owned(),
            redacted.remove("signatures").unwrap(),
        );
        serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap()
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
    // The spec's `Transaction`: who answered, and when (missing until 2026-10-01).
    assert_eq!(body["origin"], US, "{body}");
    assert!(body["origin_server_ts"].as_u64().is_some(), "{body}");

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

    // And it is the state *before* the event, as the spec and Synapse have it: before the
    // create event there is nothing at all.
    assert!(early_pdus.is_empty(), "{early_pdus:?}");

    // `/state_ids` names the same events, by ID -- which a PDU of room version 3 or later does
    // not carry, so they came back as two empty lists until 2026-09-30.
    let (status, ids) = harness
        .signed_get(&format!("/state_ids/{room_id}?event_id={newest}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{ids}");
    assert_eq!(
        ids["pdu_ids"].as_array().map(Vec::len),
        Some(pdus.len()),
        "{ids}"
    );
    assert!(
        ids["pdu_ids"]
            .as_array()
            .expect("pdu_ids")
            .iter()
            .all(|id| id.as_str().is_some_and(|id| id.starts_with('$'))),
        "{ids}"
    );
    assert!(
        !ids["auth_chain_ids"]
            .as_array()
            .expect("auth_chain_ids")
            .is_empty(),
        "{ids}"
    );
    assert!(
        !ids["pdu_ids"]
            .as_array()
            .expect("pdu_ids")
            .iter()
            .any(|id| id == newest.as_str()),
        "the state before an event does not include it: {ids}"
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
    assert_eq!(body["origin"], US, "{body}");
    assert!(body["origin_server_ts"].as_u64().is_some(), "{body}");
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

/// Sytest's "Inbound federation can get public room list" and "Federation publicRoom Name/topic
/// keys are correct": the federation directory lists the rooms published to it -- whatever
/// their join rule -- with the client-server entry shape (a name or topic never set is left
/// out), and nothing that is not published, public-join or not.
#[tokio::test]
async fn the_federation_directory_lists_the_published_rooms() {
    let harness = Harness::new().await;
    let (unpublished, _) = harness.room_with_a_message(false).await;
    let creator = ruma::UserId::parse(format!("@alice:{US}")).unwrap();
    let invite_only = harness
        .rooms
        .create_room(
            creator.clone(),
            hs_room::actor::CreateRoomRequest {
                preset: Some("private_chat".to_owned()),
                name: Some("Published, invite-only".to_owned()),
                ..Default::default()
            },
            1_000,
        )
        .await
        .expect("room creation");
    let published_id = invite_only.query(|actor| actor.room_id().to_owned()).await;
    harness
        .rooms
        .set_directory_visibility(&published_id, true)
        .expect("published");

    let (status, body) = harness.signed_get("/publicRooms").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let chunk = body["chunk"].as_array().expect("chunk");
    assert_eq!(chunk.len(), 1, "{body}");
    let entry = &chunk[0];
    assert_eq!(entry["room_id"], published_id.as_str(), "{body}");
    assert_eq!(entry["name"], "Published, invite-only", "{body}");
    assert!(entry.get("topic").is_none(), "{body}");
    assert_eq!(entry["num_joined_members"], 1, "{body}");
    assert_eq!(entry["world_readable"], false, "{body}");
    assert_eq!(entry["guest_can_join"], true, "{body}");
    assert_eq!(entry["join_rule"], "invite", "{body}");
    assert!(
        !chunk.iter().any(|room| room["room_id"] == unpublished),
        "an unpublished public-join room is not listed: {body}"
    );
}

/// Sytest's "Backfill checks the events requested belong to the room": asked to walk back from
/// an event of another room, backfill answers no events, not this room's newest history.
#[tokio::test]
async fn backfill_from_an_event_of_another_room_answers_nothing() {
    let harness = Harness::new().await;
    let (room_id, _) = harness.room_with_a_message(true).await;
    let (_other_room, other_message) = harness.room_with_a_message(true).await;

    let (status, body) = harness
        .signed_get(&format!("/backfill/{room_id}?limit=1&v={other_message}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["origin"], US, "{body}");
    assert_eq!(body["pdus"], serde_json::json!([]), "{body}");
}

/// A version-1 room joined the way Sytest's "Inbound federation can receive v1/v2 /send_join"
/// joins it: `make_join`, sign, `send_join`. Both spellings answer the room's state with an auth
/// chain that is the state's own -- every event reached through `auth_events`, the create event,
/// the power levels and the creator's join among them -- and from which every event of the chain
/// can be authorised in turn. The chain was empty until 2026-10-01: every one of those events is
/// current state, and the walk left the state events out.
#[tokio::test]
async fn send_join_answers_the_auth_chain_of_the_rooms_state() {
    let harness = Harness::new().await;
    let creator = ruma::UserId::parse(format!("@alice:{US}")).unwrap();
    let handle = harness
        .rooms
        .create_room(
            creator,
            hs_room::actor::CreateRoomRequest {
                room_version: Some(ruma::RoomVersionId::V1),
                preset: Some("public_chat".to_owned()),
                ..Default::default()
            },
            1_000,
        )
        .await
        .expect("room creation");
    let room_id = handle.query(|actor| actor.room_id().to_string()).await;

    for (version, user) in [("v1", "bob"), ("v2", "carol")] {
        let user_id = format!("@{user}:{REMOTE}");
        let (status, made) = harness
            .signed_get(&format!("/make_join/{room_id}/{user_id}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{made}");
        assert_eq!(made["room_version"], "1");
        let signed = harness.sign_as_remote(&made["event"], &ruma::RoomVersionId::V1);
        let event_id = signed["event_id"].as_str().unwrap().to_owned();
        let (status, answer) = harness
            .signed_put(version, &format!("/send_join/{room_id}/{event_id}"), signed)
            .await;
        assert_eq!(status, StatusCode::OK, "{version}: {answer}");
        let answer = if version == "v1" {
            assert_eq!(answer[0], 200, "{answer}");
            answer[1].clone()
        } else {
            answer
        };
        let chain = answer["auth_chain"].as_array().expect("auth_chain");
        for wanted in ["m.room.create", "m.room.power_levels", "m.room.member"] {
            assert!(
                chain.iter().any(|e| e["type"] == wanted),
                "{version}: the auth chain has no {wanted}: {chain:?}"
            );
        }
        // Closed under `auth_events`: every event a chain event cites is in the chain.
        let ids: std::collections::HashSet<&str> = chain
            .iter()
            .filter_map(|e| e["event_id"].as_str())
            .collect();
        for event in chain {
            for cited in event["auth_events"].as_array().unwrap() {
                let cited = cited[0].as_str().unwrap();
                assert!(
                    ids.contains(cited),
                    "{version}: {cited} is cited but not in the chain"
                );
            }
        }
        assert!(!answer["state"].as_array().unwrap().is_empty());
    }
}

/// `make_join` refuses a user of another server than the one asking -- this server's own, here
/// -- with `403 M_FORBIDDEN`, and a room no user of this server is in any more with
/// `404 M_NOT_FOUND`. Both were answered with a template until 2026-10-01 (Sytest's "Inbound
/// /v1/make_join rejects remote attempts to join local users to rooms" and "Inbound /make_join
/// rejects attempts to join rooms where all users have left").
#[tokio::test]
async fn make_join_refuses_another_servers_user_and_a_room_this_server_has_left() {
    let harness = Harness::new().await;
    let (room_id, _) = harness.room_with_a_message(false).await;

    let (status, body) = harness
        .signed_get(&format!("/make_join/{room_id}/@mallory:{US}?ver=11&ver=12"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["errcode"], "M_FORBIDDEN");

    let path = format!("/make_join/{room_id}/@bob:{REMOTE}?ver=11&ver=12");
    let (status, body) = harness.signed_get(&path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "before the last local member leaves: {body}"
    );

    let parsed = ruma::RoomId::parse(&room_id).unwrap();
    let alice = ruma::UserId::parse(format!("@alice:{US}")).unwrap();
    harness
        .rooms
        .get_or_load(&parsed)
        .await
        .unwrap()
        .membership(
            alice.clone(),
            hs_room::membership::Action::Leave,
            alice,
            serde_json::json!({}),
            9_000,
        )
        .await
        .expect("alice leaves");
    let (status, body) = harness.signed_get(&path).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["errcode"], "M_NOT_FOUND");
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
/// History a server was not in the room for is served to it redacted, not whole and not left
/// out: in a members-only room, a message from before that server's member joined comes back
/// through `/backfill` and `/get_missing_events` with its content stripped and its signatures
/// intact; a message from after the join comes back whole. `TestInboundCanReturnMissingEvents`
/// checks exactly this for the `joined` and `invited` visibilities, and until now both
/// endpoints applied only the room-level gate and served everything whole.
#[tokio::test]
async fn history_a_server_was_not_there_for_is_served_redacted() {
    let harness = Harness::new().await;
    let creator = ruma::UserId::parse(format!("@alice:{US}")).unwrap();
    let bob = ruma::UserId::parse(format!("@bob:{REMOTE}")).unwrap();
    let handle = harness
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
    handle
        .send_event(
            creator.clone(),
            "m.room.history_visibility".to_owned(),
            Some(String::new()),
            serde_json::json!({ "history_visibility": "joined" }),
            None,
            2_000,
        )
        .await
        .expect("members-only history");
    handle
        .send_event(
            creator.clone(),
            "m.room.message".to_owned(),
            None,
            serde_json::json!({ "msgtype": "m.text", "body": "before bob" }),
            None,
            3_000,
        )
        .await
        .expect("message before the join");
    handle
        .membership(
            bob.clone(),
            hs_room::membership::Action::Join,
            bob.clone(),
            serde_json::json!({}),
            4_000,
        )
        .await
        .expect("bob joins from the remote");
    handle
        .send_event(
            creator.clone(),
            "m.room.message".to_owned(),
            None,
            serde_json::json!({ "msgtype": "m.text", "body": "after bob" }),
            None,
            5_000,
        )
        .await
        .expect("message after the join");
    // One more, to ask from: the newest end of a `/get_missing_events` gap is not echoed back.
    let newest = handle
        .send_event(
            creator,
            "m.room.message".to_owned(),
            None,
            serde_json::json!({ "msgtype": "m.text", "body": "newest" }),
            None,
            6_000,
        )
        .await
        .expect("a newest message");
    let room_id = handle.query(|actor| actor.room_id().to_string()).await;
    let create_id = harness.create_event_id(&room_id).await;

    // A PDU on the wire carries no `event_id` at this room version; the two messages are told
    // apart by the timestamps they were sent with.
    let check = |pdus: &[Value], what: &str| {
        let find = |ts: i64| {
            pdus.iter()
                .find(|p| p["type"] == "m.room.message" && p["origin_server_ts"] == ts)
                .unwrap_or_else(|| {
                    panic!("{what}: the message sent at {ts} must be in the answer: {pdus:?}")
                })
        };
        let redacted = find(3_000);
        assert!(
            redacted["content"].get("body").is_none(),
            "{what}: the message before bob's join must be served redacted: {redacted}"
        );
        assert!(
            redacted.get("signatures").is_some() && redacted.get("hashes").is_some(),
            "{what}: a redacted PDU is still a PDU: {redacted}"
        );
        let whole = find(5_000);
        assert_eq!(
            whole["content"]["body"], "after bob",
            "{what}: the message after the join is served whole"
        );
    };

    let (status, body) = harness
        .signed_get(&format!(
            "/backfill/{room_id}?limit=20&v={}",
            newest.event_id()
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    check(body["pdus"].as_array().expect("pdus"), "/backfill");

    let (status, body) = harness
        .signed_post(
            &format!("/get_missing_events/{room_id}"),
            serde_json::json!({
                "earliest_events": [create_id],
                "latest_events": [newest.event_id()],
                "limit": 20,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    check(
        body["events"].as_array().expect("events"),
        "/get_missing_events",
    );
}

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

    // `min_depth` is a floor: nothing below it comes back, and the walk does not continue
    // past it. Asked from one below the newest gap event's depth, only that event and its
    // depth-mates answer.
    let deepest = *sorted.last().expect("at least one event");
    let (status, body) = harness
        .signed_post(
            &format!("/get_missing_events/{room_id}"),
            serde_json::json!({
                "earliest_events": [create_id],
                "latest_events": [message_id],
                "limit": 10,
                "min_depth": deepest,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let floored: Vec<i64> = body["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|e| e["depth"].as_i64().expect("depth"))
        .collect();
    assert!(
        !floored.is_empty(),
        "the newest gap event is at the floor: {body}"
    );
    assert!(
        floored.iter().all(|d| *d >= deepest),
        "nothing below min_depth {deepest} may come back: {floored:?}"
    );
    assert!(
        floored.len() < depths.len(),
        "the floor must have cut something: {floored:?} vs {depths:?}"
    );
}
