//! Federation *writes* -- `PUT /_matrix/federation/v1/send/{txnId}` and the `make_join`/`send_join`
//! handshake (v1 and v2) -- driven the same way `federation_reads.rs` drives the read side: a real
//! signed `X-Matrix` request against the real composed router (`hs_federation::transport::router`
//! *and* `router_v2`, mounted at their real prefixes), over a real room created through the same
//! `RoomRegistry` `hs serve` uses.
//!
//! What this proves, and what it does not:
//!
//! - **Real, end-to-end**: transaction-envelope shape, the 50-PDU limit, per-event
//!   accept/reject results, idempotent replay by `(origin, txnId)`, content-hash and
//!   signature verification (including against a room's *own* locally-signed events, replayed as
//!   if relayed by a remote), and `make_join` building a genuine template against this server's
//!   real current state (`prev_events`, `depth`, `auth_events` all read off the real room actor).
//! - **The one honest gap, proven rather than assumed**: a PDU or `send_join` event this server has
//!   never seen before passes every real check (signature, hash, authorization) and then fails at
//!   the last step with a distinct, typed error -- because `hs-room`'s `RoomActor` has no API to
//!   accept an already-signed foreign event. See `crates/hs-federation/src/inbound.rs` and
//!   `crates/hs-federation/src/join.rs`'s module docs, and `docs/status/06-federation.md`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hs_kv::memory::MemoryBackend;
use hs_model::canonical::{CanonicalJsonValue, to_canonical_object};
use hs_model::room_version::EventsReferenceFormat;
use hs_model::signing::SigningKeyPair;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const US: &str = "local.example";
const REMOTE: &str = "remote.example";

/// Serves both this server's own key (as it would answer its own `/_matrix/key/v2/server`, which
/// a real deployment can always do for itself) and the fake remote's, so PDUs "relayed" by
/// `REMOTE` but actually signed by `US` (a room's own locally-created events, replayed as if a
/// federation partner forwarded them) verify correctly, alongside PDUs genuinely signed by
/// `REMOTE`.
struct TwoServerKeys {
    us_name: String,
    us_keys: hs_federation::keys::OwnSigningKeys,
    remote_name: String,
    remote_keys: hs_federation::keys::OwnSigningKeys,
}

#[async_trait]
impl hs_federation::keys::KeyServerFetcher for TwoServerKeys {
    async fn fetch_server_key(&self, server_name: &str) -> Option<Value> {
        if server_name == self.us_name {
            hs_federation::keys::build_server_key_response(&self.us_name, &self.us_keys, &[], 3600)
                .ok()
        } else if server_name == self.remote_name {
            hs_federation::keys::build_server_key_response(
                &self.remote_name,
                &self.remote_keys,
                &[],
                3600,
            )
            .ok()
        } else {
            None
        }
    }
}

struct Harness {
    router_v1: axum::Router,
    router_v2: axum::Router,
    remote_name: String,
    remote_key: SigningKeyPair,
    us_signing_key: SigningKeyPair,
    rooms: Arc<RoomRegistry<MemoryBackend>>,
}

impl Harness {
    async fn new() -> Self {
        Self::build(REMOTE.to_string(), None).await
    }

    /// Like [`Harness::new`], but wires a real [`hs_federation::client::FederationClient`] as the
    /// state's `ancestor_fetcher`, pointed (via the explicit-port destination form, exactly as
    /// `crates/hs-federation/src/client.rs`'s own tests do) at a "remote" server the caller binds
    /// to `127.0.0.1:{port}` -- for the backfill tests below, which need this server to actually
    /// make an outbound `/backfill` call, not just accept a `/send` transaction.
    ///
    /// The remote's identity is `localhost:{port}` (a valid, explicit-port Matrix server name):
    /// this both bypasses well-known/SRV discovery (there is no real DNS for it to resolve) and
    /// is what every signed request in these tests uses as `origin`, so the transaction's origin
    /// and the backfill client's destination are, correctly, the same string a real deployment
    /// would use.
    async fn with_backfill_peer(port: u16) -> Self {
        Self::build(format!("localhost:{port}"), Some(port)).await
    }

    async fn build(remote_name: String, backfill_port: Option<u16>) -> Self {
        let backend = MemoryBackend::new();
        let identity = HomeserverIdentity::for_tests(US);
        let us_signing_key = (*identity.signing_key).clone();
        let us_keys = hs_federation::keys::OwnSigningKeys::from_keys(vec![us_signing_key.clone()]);
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

        let remote_key = SigningKeyPair::generate("a_remote");
        let remote_keys = hs_federation::keys::OwnSigningKeys::from_keys(vec![remote_key.clone()]);

        let ancestor_fetcher: Option<Arc<dyn hs_federation::backfill::AncestorFetcher>> =
            backfill_port.map(|_port| {
                let client = hs_federation::client::FederationClient::new(
                    US,
                    us_signing_key.clone(),
                    hs_federation::client::ClientConfig {
                        scheme: "http",
                        ..hs_federation::client::ClientConfig::default()
                    },
                    Arc::new(hs_federation::destination_store::InMemoryDestinationStore::new()),
                    Arc::new(NoWellKnown),
                    Arc::new(NoSrv),
                    Arc::new(FixedAddr(std::net::IpAddr::V4(
                        std::net::Ipv4Addr::LOCALHOST,
                    ))),
                );
                Arc::new(client) as Arc<dyn hs_federation::backfill::AncestorFetcher>
            });

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
            ancestor_fetcher,
            backfill_limits: hs_federation::backfill::BackfillLimits::default(),
        };

        let fetcher = TwoServerKeys {
            us_name: US.to_string(),
            us_keys,
            remote_name: remote_name.clone(),
            remote_keys,
        };
        let key_cache: Arc<hs_federation::keys::DynRemoteKeyCache> =
            Arc::new(hs_federation::keys::RemoteKeyCache::new(
                Box::new(fetcher) as Box<dyn hs_federation::keys::KeyServerFetcher>
            ));
        let ctx = Arc::new(hs_federation::xmatrix::XMatrixContext {
            own_server_name: US.to_string(),
            key_cache,
        });

        let (v1, _manifest) = hs_federation::transport::router(state.clone(), ctx.clone());
        let (v2, _manifest) = hs_federation::transport::router_v2(state, ctx);
        // Mounted exactly where `hs serve` mounts them (see this crate's status file for the
        // wiring line the integration lead needs to add for `router_v2`): the `X-Matrix` layer
        // signs over the *full*, prefixed path, and `axum::Router::nest` rewrites `req.uri()`
        // before inner layers run, so testing at the root would hide a whole class of bug (as
        // `federation_reads.rs` already documents).
        let router_v1 = axum::Router::new().nest("/_matrix/federation/v1", v1);
        let router_v2 = axum::Router::new().nest("/_matrix/federation/v2", v2);

        Self {
            router_v1,
            router_v2,
            remote_name,
            remote_key,
            us_signing_key,
            rooms,
        }
    }

    async fn signed_put(&self, v2: bool, path: &str, body: &Value) -> (StatusCode, Value) {
        let prefix = if v2 {
            "/_matrix/federation/v2"
        } else {
            "/_matrix/federation/v1"
        };
        let uri = format!("{prefix}{path}");
        let auth = hs_federation::xmatrix::sign_request(
            "PUT",
            &uri,
            &self.remote_name,
            US,
            Some(body),
            &self.remote_key,
        )
        .expect("signing");
        let router = if v2 {
            self.router_v2.clone()
        } else {
            self.router_v1.clone()
        };
        let request = Request::builder()
            .method("PUT")
            .uri(&uri)
            .header("Authorization", auth)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    async fn signed_get(&self, path: &str) -> (StatusCode, Value) {
        let uri = format!("/_matrix/federation/v1{path}");
        let auth = hs_federation::xmatrix::sign_request(
            "GET",
            &uri,
            &self.remote_name,
            US,
            None,
            &self.remote_key,
        )
        .expect("signing");
        let request = Request::builder()
            .method("GET")
            .uri(&uri)
            .header("Authorization", auth)
            .body(Body::empty())
            .unwrap();
        let response = self.router_v1.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Creates a public, world-readable room owned by a local user with one message in it.
    /// Returns `(room_id, message_event_id)`.
    async fn room_with_a_message(&self) -> (String, String) {
        let (room_id, message) = self.room_with_a_message_event().await;
        (room_id, message.event_id().to_string())
    }

    /// Like [`Harness::room_with_a_message`], but also returns the message's real `depth` --
    /// needed to construct a well-formed synthetic descendant event for the backfill tests below.
    async fn room_with_a_message_and_depth(&self) -> (String, String, i64) {
        let (room_id, message) = self.room_with_a_message_event().await;
        (
            room_id,
            message.event_id().to_string(),
            message.header().depth,
        )
    }

    async fn room_with_a_message_event(&self) -> (String, hs_model::Event) {
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
        let message = handle
            .send_event(
                creator,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
                None,
                3_000,
            )
            .await
            .expect("message");
        let room_id = handle.query(|actor| actor.room_id().to_string()).await;
        (room_id, message)
    }

    /// The event IDs a plain `m.room.message` from alice must cite as `auth_events` for this
    /// server's default room version (11, where `room_create_event_id_as_room_id` is *false* --
    /// only room version 12 changed that -- so `m.room.create` is, perhaps counter-intuitively,
    /// still part of the selection alongside `m.room.power_levels` and alice's own
    /// `m.room.member`). Used by the backfill tests below to hand-construct synthetic descendant
    /// events that pass real authorization once persisted. Order: `(create, power_levels,
    /// member)`.
    async fn message_auth_event_ids(&self, room_id: &str) -> (String, String, String) {
        let parsed = ruma::RoomId::parse(room_id).unwrap();
        let handle = self.rooms.get_or_load(&parsed).await.unwrap();
        let alice = format!("@alice:{US}");
        handle
            .query(move |actor| {
                let state = actor.full_state().expect("room has state");
                let find = |event_type: &str, state_key: Option<&str>| {
                    state
                        .iter()
                        .find(|e| {
                            e.header().event_type == event_type
                                && state_key
                                    .is_none_or(|k| e.header().state_key.as_deref() == Some(k))
                        })
                        .unwrap_or_else(|| panic!("room has {event_type}"))
                        .event_id()
                        .to_string()
                };
                (
                    find("m.room.create", None),
                    find("m.room.power_levels", None),
                    find("m.room.member", Some(&alice)),
                )
            })
            .await
    }

    /// The exact wire-form PDU JSON for an event this server already holds, fetched the way a
    /// federation peer would receive it (full canonical form, no synthetic `event_id`).
    async fn full_pdu(&self, room_id: &str, event_id: &str) -> Value {
        let (status, body) = self.signed_get(&format!("/event/{event_id}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let _ = room_id;
        body["pdus"][0].clone()
    }
}

/// Always reports no `.well-known` -- the backfill tests' "remote" is addressed by its explicit
/// port, which bypasses discovery entirely, but a fetcher must still exist to build the client.
struct NoWellKnown;
#[async_trait]
impl hs_federation::discovery::WellKnownFetcher for NoWellKnown {
    async fn fetch(&self, _hostname: &str) -> hs_federation::discovery::WellKnownOutcome {
        hs_federation::discovery::WellKnownOutcome::Absent {
            cache_for: std::time::Duration::from_secs(60),
        }
    }
}

/// Always reports no SRV records, for the same reason as [`NoWellKnown`].
struct NoSrv;
#[async_trait]
impl hs_federation::discovery::SrvResolver for NoSrv {
    async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
        Vec::new()
    }
}

/// Resolves every hostname to the one fixed address the test's own axum server listens on.
struct FixedAddr(std::net::IpAddr);
#[async_trait]
impl hs_federation::discovery::AddrResolver for FixedAddr {
    async fn resolve_addr(&self, _hostname: &str) -> Vec<std::net::IpAddr> {
        vec![self.0]
    }
}

/// Builds and signs a synthetic `m.room.message`, exactly the way a real homeserver would: real
/// content hash, real signature, real canonical-JSON round-trip. Used by the backfill tests below
/// to construct events this server has never seen, with a real, checkable dependency chain.
#[allow(clippy::too_many_arguments)]
fn build_signed_message(
    signing_key: &SigningKeyPair,
    room_id: &str,
    sender: &str,
    prev_events: Vec<String>,
    auth_events: Vec<String>,
    depth: i64,
    origin_server_ts: i64,
    body: &str,
) -> Value {
    let mut object = to_canonical_object(
        &serde_json::json!({
            "type": "m.room.message",
            "room_id": room_id,
            "sender": sender,
            "origin_server_ts": origin_server_ts,
            "depth": depth,
            "content": {"msgtype": "m.text", "body": body},
            "prev_events": prev_events,
            "auth_events": auth_events,
        }),
        true,
    )
    .unwrap();
    let hash = hs_model::hash::content_hash_base64(&object);
    object.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(
            [("sha256".to_owned(), CanonicalJsonValue::String(hash))]
                .into_iter()
                .collect(),
        ),
    );
    let server = ruma::ServerName::parse(sender.split_once(':').unwrap().1).unwrap();
    hs_model::signing::sign_object(&mut object, &server, signing_key).unwrap();
    serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap()
}

#[tokio::test]
async fn send_accepts_an_event_it_already_holds_idempotently() {
    let harness = Harness::new().await;
    let (room_id, event_id) = harness.room_with_a_message().await;
    let pdu = harness.full_pdu(&room_id, &event_id).await;

    let body = serde_json::json!({ "pdus": [pdu], "edus": [] });
    let (status, response) = harness.signed_put(false, "/send/txn-1", &body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        response["pdus"][event_id.as_str()],
        serde_json::json!({}),
        "an already-known event must be accepted, not rejected: {response}"
    );
}

/// **Mutation test** (see the status file): flip a byte of the PDU's signature and confirm the
/// per-event result turns into an error. If this ever stays `{}`, `verify_pdu`'s signature check
/// is not wired into `/send` for real.
#[tokio::test]
async fn send_rejects_a_pdu_with_a_tampered_signature() {
    let harness = Harness::new().await;
    let (room_id, event_id) = harness.room_with_a_message().await;
    let mut pdu = harness.full_pdu(&room_id, &event_id).await;

    let sig_holder = pdu["signatures"][US]
        .as_object()
        .cloned()
        .expect("event is signed by its own server");
    let (key_id, sig) = sig_holder
        .into_iter()
        .next()
        .expect("at least one signature");
    let mut sig = sig.as_str().unwrap().to_string();
    let last = sig.pop().unwrap();
    sig.push(if last == 'A' { 'B' } else { 'A' });
    pdu["signatures"][US][key_id] = Value::String(sig);

    let body = serde_json::json!({ "pdus": [pdu], "edus": [] });
    let (status, response) = harness.signed_put(false, "/send/txn-tampered", &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the transaction itself is well-formed: {response}"
    );
    let pdus = response["pdus"].as_object().unwrap();
    assert_eq!(pdus.len(), 1);
    let (_, result) = pdus.iter().next().unwrap();
    assert!(
        result.get("error").is_some(),
        "a tampered signature must be rejected: {response}"
    );
}

#[tokio::test]
async fn send_rejects_a_new_event_whose_auth_events_do_not_authorize_it() {
    let harness = Harness::new().await;
    let (room_id, _event_id) = harness.room_with_a_message().await;

    // A brand-new event, never seen by this server, correctly hashed and signed by `REMOTE`.
    let remote_key = harness.remote_key.clone();
    let mut object = to_canonical_object(
        &serde_json::json!({
            "type": "m.room.message",
            "room_id": room_id,
            "sender": format!("@bob:{REMOTE}"),
            "origin_server_ts": 4_000,
            "depth": 5,
            "content": {"msgtype": "m.text", "body": "from a remote"},
            "prev_events": [],
            "auth_events": [],
        }),
        true,
    )
    .unwrap();
    let hash = hs_model::hash::content_hash_base64(&object);
    object.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(
            [("sha256".to_owned(), CanonicalJsonValue::String(hash))]
                .into_iter()
                .collect(),
        ),
    );
    let server = ruma::ServerName::parse(REMOTE).unwrap();
    hs_model::signing::sign_object(&mut object, &server, &remote_key).unwrap();
    let pdu: Value =
        serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap();

    let body = serde_json::json!({ "pdus": [pdu], "edus": [] });
    let (status, response) = harness
        .signed_put(false, "/send/txn-new-event", &body)
        .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let pdus = response["pdus"].as_object().unwrap();
    assert_eq!(pdus.len(), 1);
    let (_, result) = pdus.iter().next().unwrap();
    let error = result.get("error").and_then(Value::as_str).unwrap_or("");
    // This PDU is correctly hashed and signed, so it gets past `verify_pdu` -- and then fails for
    // the right reason. It cites no `m.room.create` in its `auth_events` (it cites nothing at
    // all), so the auth rules refuse it. Before `RoomActor::accept_remote_event` existed this
    // test asserted a "cannot yet persist" message instead, because nothing downstream of
    // verification ran at all; that wall is gone, and what a bad event now meets is the real
    // rules.
    assert!(
        error.contains("m.room.create") || error.contains("auth"),
        "expected an authorization failure naming what was wrong, got: {response}"
    );
    assert!(
        !error.contains("cannot yet persist"),
        "the persistence gap is closed; this message should no longer appear: {response}"
    );
}

#[tokio::test]
async fn send_over_the_pdu_limit_is_rejected_before_processing_anything() {
    let harness = Harness::new().await;
    let too_many: Vec<Value> = (0..51).map(|_| serde_json::json!({})).collect();
    let body = serde_json::json!({ "pdus": too_many, "edus": [] });
    let (status, _response) = harness.signed_put(false, "/send/txn-too-big", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn make_join_builds_a_real_template_against_the_real_room() {
    let harness = Harness::new().await;
    let (room_id, _event_id) = harness.room_with_a_message().await;

    let (status, body) = harness
        .signed_get(&format!("/make_join/{room_id}/@bob:{REMOTE}?ver=11"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["room_version"], "11");
    assert_eq!(body["event"]["type"], "m.room.member");
    assert_eq!(body["event"]["state_key"], format!("@bob:{REMOTE}"));
    assert_eq!(body["event"]["content"]["membership"], "join");
    // The event's real `prev_events` are this server's real forward extremities, not a stub.
    assert!(body["event"]["prev_events"].as_array().unwrap().len() == 1);
    assert!(!body["event"]["auth_events"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn send_join_v2_persists_the_join_and_it_is_readable_afterwards() {
    let harness = Harness::new().await;
    let (room_id, _event_id) = harness.room_with_a_message().await;

    let (status, template) = harness
        .signed_get(&format!("/make_join/{room_id}/@bob:{REMOTE}?ver=11"))
        .await;
    assert_eq!(status, StatusCode::OK, "{template}");

    // The joining server signs the template it was handed, exactly as a real one would.
    let mut object = to_canonical_object(&template["event"], true).unwrap();
    let hash = hs_model::hash::content_hash_base64(&object);
    object.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(
            [("sha256".to_owned(), CanonicalJsonValue::String(hash))]
                .into_iter()
                .collect(),
        ),
    );
    let server = ruma::ServerName::parse(REMOTE).unwrap();
    hs_model::signing::sign_object(&mut object, &server, &harness.remote_key).unwrap();
    let signed_bytes = CanonicalJsonValue::Object(object).to_canonical_bytes();
    let signed: Value = serde_json::from_slice(&signed_bytes).unwrap();

    let event = hs_model::Event::parse(&signed, ruma::RoomVersionId::V11).unwrap();
    assert_eq!(
        hs_model::room_version::rules_for(&ruma::RoomVersionId::V11)
            .unwrap()
            .events_reference_format,
        EventsReferenceFormat::V2IdOnly,
        "sanity check: this server's default room version derives event IDs from the reference hash"
    );

    let (status, response) = harness
        .signed_put(
            true,
            &format!("/send_join/{room_id}/{}", event.event_id()),
            &signed,
        )
        .await;
    // The whole path now runs: signature, content hash, shape, the sender's server matching the
    // requester, authorization against real current state, and -- new -- persistence through
    // `RoomActor::accept_remote_event`. This test used to assert a 501 with a distinct errcode
    // because the last step had nowhere to go.
    assert_eq!(status, StatusCode::OK, "{response}");

    // The join is not merely acknowledged: a remote's join is now real state on this server, and
    // the read side agrees. `/event/{id}` serves the event back, and it is the *same* event --
    // the bytes a remote signed, not a re-authored copy with our own signature on it.
    let joined_event_id = event.event_id().to_string();
    let (status, fetched) = harness
        .signed_get(&format!("/event/{joined_event_id}"))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the join should be readable: {fetched}"
    );
    let stored = &fetched["pdus"][0];
    assert_eq!(stored["type"], "m.room.member");
    assert_eq!(stored["state_key"], format!("@bob:{REMOTE}"));
    assert_eq!(stored["content"]["membership"], "join");
    assert!(
        stored["signatures"][REMOTE].is_object(),
        "the remote's own signature must survive storage byte-for-byte: {fetched}"
    );

    // And it is state, not just a timeline entry: the room's current state names bob as joined.
    let (status, state) = harness
        .signed_get(&format!("/state/{room_id}?event_id={joined_event_id}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{state}");
    let has_bob = state["pdus"].as_array().unwrap().iter().any(|pdu| {
        pdu["type"] == "m.room.member"
            && pdu["state_key"] == format!("@bob:{REMOTE}")
            && pdu["content"]["membership"] == "join"
    });
    assert!(has_bob, "bob's join should be in the room's state: {state}");
}

/// Binds a minimal "remote federation server" to a real loopback TCP port: an axum catch-all that
/// answers every request with whatever is currently in `body` (mutable after the peer starts, so
/// a test can spawn it before it knows the exact JSON it wants to serve), ignoring path, method,
/// and any `Authorization` header. Good enough to stand in for a peer this server's real
/// `hs_federation::client::FederationClient` dials over real HTTP -- what matters for these tests
/// is what *this* server does with the response, not that the peer itself is a faithful federation
/// implementation.
async fn spawn_configurable_backfill_peer() -> (u16, Arc<Mutex<Value>>, tokio::task::JoinHandle<()>)
{
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = Arc::new(Mutex::new(serde_json::json!({ "pdus": [] })));
    let body_for_app = body.clone();
    let app = axum::Router::new().route(
        "/{*rest}",
        axum::routing::any(move || {
            let body = body_for_app.clone();
            async move {
                let current = body.lock().unwrap().clone();
                axum::Json(current)
            }
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (port, body, handle)
}

/// **The backfill loop, end to end** (`docs/next-steps.md` item 4): a remote sends an event (`m2`)
/// whose `prev_events` cites an event (`m1`) this server has never seen. `RoomActor::persist`
/// reports `RoomError::MissingAncestors`, and this server fetches exactly the missing event from
/// the "remote" over a real, signed HTTP `/backfill` request, verifies it the same way any inbound
/// PDU is verified, persists it, then retries `m2` -- which now succeeds because its one missing
/// ancestor is no longer missing.
#[tokio::test]
async fn send_backfills_a_missing_ancestor_then_accepts_the_original_event() {
    let (port, peer_body, _peer_handle) = spawn_configurable_backfill_peer().await;
    let harness = Harness::with_backfill_peer(port).await;

    let (room_id, base_event_id, base_depth) = harness.room_with_a_message_and_depth().await;
    let (create_id, power_id, member_id) = harness.message_auth_event_ids(&room_id).await;
    let alice = format!("@alice:{US}");

    let m1 = build_signed_message(
        &harness.us_signing_key,
        &room_id,
        &alice,
        vec![base_event_id],
        vec![create_id.clone(), power_id.clone(), member_id.clone()],
        base_depth + 1,
        10_000,
        "m1 (fetched via backfill, never sent directly)",
    );
    let m1_id = hs_model::Event::parse(&m1, ruma::RoomVersionId::V11)
        .unwrap()
        .event_id()
        .to_string();

    let m2 = build_signed_message(
        &harness.us_signing_key,
        &room_id,
        &alice,
        vec![m1_id.clone()],
        vec![create_id, power_id, member_id],
        base_depth + 2,
        11_000,
        "m2 (cites m1, which this server has never seen)",
    );

    // The peer's canned response: exactly the one event that closes the gap, nothing more --
    // proves this server asks for, and is satisfied by, the minimum necessary, not a bulk history
    // dump.
    *peer_body.lock().unwrap() = serde_json::json!({ "pdus": [m1] });

    let body = serde_json::json!({ "pdus": [m2], "edus": [] });
    let (status, response) = harness.signed_put(false, "/send/txn-backfill", &body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let pdus = response["pdus"].as_object().unwrap();
    assert_eq!(pdus.len(), 1, "{response}");
    let (_, result) = pdus.iter().next().unwrap();
    assert_eq!(
        result,
        &serde_json::json!({}),
        "expected the original event to be accepted once backfill closed the gap: {response}"
    );

    // Not just acknowledged: `m1` -- fetched via backfill, never sent directly -- is itself now
    // real, durable state on this server.
    let (status, fetched_m1) = harness.signed_get(&format!("/event/{m1_id}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the backfilled ancestor should have been persisted: {fetched_m1}"
    );
    assert_eq!(
        fetched_m1["pdus"][0]["content"]["body"],
        "m1 (fetched via backfill, never sent directly)"
    );
}

/// A hostile (or merely broken) remote answers every `/backfill` call with exactly one
/// freshly-fabricated event whose own `prev_events` names a *new*, still-missing ancestor -- an
/// endless chain that never bottoms out in anything this server already holds. This server must
/// give up cleanly once `hs_federation::backfill::BackfillLimits::default().max_rounds`
/// round-trips have happened, not chase the chain forever.
#[tokio::test]
async fn send_gives_up_when_the_remote_serves_an_endless_backfill_chain() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let harness = Harness::with_backfill_peer(port).await;

    let (room_id, _base_event_id, base_depth) = harness.room_with_a_message_and_depth().await;
    let (create_id, power_id, member_id) = harness.message_auth_event_ids(&room_id).await;
    let alice = format!("@alice:{US}");

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_app = calls.clone();
    let signing_key_for_app = harness.us_signing_key.clone();
    let room_id_for_app = room_id.clone();
    let create_id_for_app = create_id.clone();
    let power_id_for_app = power_id.clone();
    let member_id_for_app = member_id.clone();
    let alice_for_app = alice.clone();
    let app = axum::Router::new().route(
        "/{*rest}",
        axum::routing::any(move || {
            let calls = calls_for_app.clone();
            let signing_key = signing_key_for_app.clone();
            let room_id = room_id_for_app.clone();
            let create_id = create_id_for_app.clone();
            let power_id = power_id_for_app.clone();
            let member_id = member_id_for_app.clone();
            let alice = alice_for_app.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                // Every response points to a brand-new, still-missing ancestor: this "remote"
                // never converges, no matter how many times it is asked. It never gets far enough
                // for authorization to even run (ancestor presence is checked first), so the
                // auth_events here do not need to be exhaustively correct -- only shaped like a
                // real event.
                let never_ends = format!("$never-ends-{n}");
                let event = build_signed_message(
                    &signing_key,
                    &room_id,
                    &alice,
                    vec![never_ends],
                    vec![create_id, power_id, member_id],
                    -(n as i64),
                    20_000 + n as i64,
                    "endless",
                );
                axum::Json(serde_json::json!({ "pdus": [event] }))
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let m2 = build_signed_message(
        &harness.us_signing_key,
        &room_id,
        &alice,
        vec!["$initial-gap".to_owned()],
        vec![create_id, power_id, member_id],
        base_depth + 1,
        30_000,
        "cites a gap that never closes",
    );

    let body = serde_json::json!({ "pdus": [m2], "edus": [] });
    let (status, response) = harness.signed_put(false, "/send/txn-runaway", &body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let pdus = response["pdus"].as_object().unwrap();
    assert_eq!(pdus.len(), 1, "{response}");
    let (_, result) = pdus.iter().next().unwrap();
    let error = result.get("error").and_then(Value::as_str).unwrap_or("");
    assert!(
        error.contains("backfill") || error.contains("round"),
        "expected the transaction to report a clean backfill give-up, got: {response}"
    );

    // Bounded: exactly `max_rounds` requests reached the hostile peer, not one per hop of the
    // (literally endless) chain it kept offering.
    let limits = hs_federation::backfill::BackfillLimits::default();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        limits.max_rounds,
        "expected exactly max_rounds requests to the hostile peer"
    );
}
