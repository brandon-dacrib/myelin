//! Client-role join initiation: **this** server's own user joining a room hosted by a remote,
//! resident server, via the real `make_join`/`send_join` handshake -- the mirror image of
//! `crate::join`, which is the *resident* side of the same handshake (this server answering a
//! remote's `GET /make_join`/`PUT /send_join` for a room *it* hosts).
//!
//! # Why this exists, and what it does not close
//!
//! Before this module, `crate::join` (the resident/target side) and `crate::transport::join` (its
//! HTTP mount) were real and tested -- but nothing in this workspace ever *called out* to another
//! server's `/make_join`/`/send_join` to join a room this server does not host. Every one of this
//! crate's own join tests, and `hs-cli`'s `federation_writes.rs`, exercises the responder only. A
//! session spent putting two live instances of this server in front of each other
//! (`crates/hs-federation/scripts/two-server-federation.sh`) found this gap directly: there was no
//! code path at all for "join a room on that other server", client-side.
//!
//! This module is that client-side orchestration: resolve the resident server (via the caller's
//! [`crate::client::FederationClient`], so discovery, TLS/CA trust, X-Matrix request signing,
//! per-destination backoff and concurrency all come from the one client every other outbound call
//! already uses), fetch an unsigned join template (`GET make_join`), sign it exactly the way a
//! conformant sender must (hash the full event, redact, sign the *redacted* form, copy the
//! signature back onto the full event -- see `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`,
//! which this module follows to the letter rather than re-deriving), submit it (`PUT send_join`,
//! v2), and verify every event the resident hands back (`state`, `auth_chain`) the same way any
//! other inbound PDU is verified ([`crate::inbound::verify_pdu`]).
//!
//! **What this does not do: persist the joined room locally.** `hs-room`'s `RoomActor`/
//! `RoomRegistry` has a real API for applying an already-verified *foreign* event to a room this
//! server already has (`accept_remote_event`, used by `crate::inbound` and `crate::join`'s
//! resident-side `send_join`) -- but no API for *creating* a room from nothing but a federation
//! join response's state snapshot. `RoomRegistry::get_or_load` returns `RoomNotFound` for a room
//! ID this server has never created, and `RoomActor::create_room` only ever originates a brand new
//! room this server itself creates (a fresh `m.room.create` this server signs), not one whose
//! `m.room.create` was authored somewhere else entirely. Closing that needs a new `hs-room` entry
//! point; see `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`, addressed to track 04.
//! Until that lands, [`join_room`] returns a fully verified [`RemoteJoinOutcome`] -- proof the
//! handshake, the signing and the verification all happened for real, live, between two
//! processes -- and stops there: the resident server genuinely persists the new member (this is
//! real and observable on its side, e.g. via `GET /_matrix/client/v3/rooms/{roomId}/members`), but
//! the joining server cannot yet represent the room for its own user to read or post into.

use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::signing::{SigningKeyPair, sign_object};
use ruma::{RoomVersionId, ServerName};
use serde_json::Value;

use crate::client::{ClientError, FederationClient};
use crate::inbound::{PduError, verify_pdu};
use crate::keys::DynRemoteKeyCache;

/// Why [`join_room`] could not complete.
#[derive(Debug, thiserror::Error)]
pub enum OutboundJoinError {
    /// The outbound request itself failed (network, TLS, discovery, backoff, ...).
    #[error("federation request to {destination} failed: {source}")]
    Client {
        destination: String,
        #[source]
        source: ClientError,
    },
    /// The resident server answered `make_join` or `send_join` with a non-2xx status.
    #[error("{destination} rejected {step} with HTTP {status}: {body}")]
    Rejected {
        destination: String,
        step: &'static str,
        status: u16,
        body: Value,
    },
    /// `make_join`'s response body was not shaped the way the spec requires.
    #[error("make_join response from {0} was malformed: {1}")]
    MalformedTemplate(String, String),
    /// The join template could not be hashed, redacted or signed.
    #[error("could not sign the join event: {0}")]
    Signing(String),
    /// `send_join`'s response body was not shaped the way the spec requires.
    #[error("send_join response from {0} was malformed: {1}")]
    MalformedResponse(String, String),
    /// An event in the returned `state` or `auth_chain` failed the same verification any inbound
    /// PDU gets -- content hash or signature. Carries the failing event's raw JSON for logging;
    /// never trusted further than that.
    #[error("send_join from {destination} included an event that failed verification: {source}")]
    UnverifiedEvent {
        destination: String,
        #[source]
        source: PduError,
    },
}

/// The verified result of successfully joining a room hosted by another server.
///
/// Every event in [`Self::state`] and [`Self::auth_chain`] has already passed
/// [`crate::inbound::verify_pdu`] -- content hash and signature, checked against that *event's
/// own* sender's server (which need not be `destination`: a resident server relays events from
/// every domain that has ever participated in the room, exactly as `crate::inbound::verify_pdu`'s
/// own doc comment notes for `/send`). Nothing here has been persisted anywhere; see the module
/// doc for why.
#[derive(Debug)]
pub struct RemoteJoinOutcome {
    /// The room this join was for, as submitted (echoed back, not re-derived).
    pub room_id: String,
    /// The room version `make_join` reported, and every event was parsed and verified against.
    pub room_version: RoomVersionId,
    /// This server's own join event, signed by `own_server_name` and accepted by `destination`.
    pub join_event: Event,
    /// The room's full state at the point of the join, verified.
    pub state: Vec<Event>,
    /// That state's auth chain, verified.
    pub auth_chain: Vec<Event>,
    /// Always `false` in practice: `destination`'s own `send_join` (`crate::join::send_join`)
    /// never omits members (faster joins are out of scope on the resident side too), but this is
    /// read from the response rather than assumed, so a resident that ever does start omitting
    /// members makes that visible here rather than silently mis-verifying a partial state.
    pub members_omitted: bool,
}

/// Joins `room_id` as `user_id`, asking `destination` (a server presumed to already be a member of
/// the room, i.e. its `via`) to sponsor the join.
///
/// Performs, in order: `GET /_matrix/federation/v1/make_join/{room_id}/{user_id}`; local
/// hash-redact-sign of the returned template (per the spec's real signing order -- see the module
/// doc); `PUT /_matrix/federation/v2/send_join/{room_id}/{event_id}`; verification of every event
/// the response returns. `client` provides discovery, TLS/CA trust and outbound `X-Matrix` request
/// signing (the *transport* layer's signature, distinct from the *event*'s own signature this
/// function computes); `own_server_name`/`signing_key` are the joining user's own homeserver's
/// identity, used to sign the join event itself, exactly as `crates/hs-room/src/pipeline.rs` signs
/// any other locally-originated event.
///
/// # Errors
/// See [`OutboundJoinError`].
pub async fn join_room(
    client: &FederationClient,
    key_cache: &DynRemoteKeyCache,
    destination: &str,
    room_id: &str,
    user_id: &str,
    own_server_name: &ServerName,
    signing_key: &SigningKeyPair,
) -> Result<RemoteJoinOutcome, OutboundJoinError> {
    let template_path = format!("/_matrix/federation/v1/make_join/{room_id}/{user_id}");
    let make_join_response = client
        .send(destination, "GET", &template_path, None)
        .await
        .map_err(|source| OutboundJoinError::Client {
            destination: destination.to_owned(),
            source,
        })?;
    if make_join_response.status / 100 != 2 {
        return Err(OutboundJoinError::Rejected {
            destination: destination.to_owned(),
            step: "make_join",
            status: make_join_response.status,
            body: make_join_response.body,
        });
    }

    let template = make_join_response
        .body
        .get("event")
        .cloned()
        .ok_or_else(|| {
            OutboundJoinError::MalformedTemplate(
                destination.to_owned(),
                "missing `event`".to_owned(),
            )
        })?;
    let room_version_str = make_join_response
        .body
        .get("room_version")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OutboundJoinError::MalformedTemplate(
                destination.to_owned(),
                "missing `room_version`".to_owned(),
            )
        })?
        .to_owned();
    let room_version = RoomVersionId::try_from(room_version_str.as_str()).map_err(|_| {
        OutboundJoinError::MalformedTemplate(
            destination.to_owned(),
            format!("unrecognized room_version {room_version_str}"),
        )
    })?;
    let rules = hs_model::room_version::rules_for(&room_version).ok_or_else(|| {
        OutboundJoinError::MalformedTemplate(
            destination.to_owned(),
            format!("unsupported room_version {room_version_str}"),
        )
    })?;

    let signed_value = sign_join_template(&template, &rules, own_server_name, signing_key)
        .map_err(OutboundJoinError::Signing)?;
    let join_event = Event::parse(&signed_value, room_version.clone()).map_err(|e| {
        OutboundJoinError::Signing(format!("signed join event does not parse: {e}"))
    })?;
    let event_id = join_event.event_id().to_string();

    let send_join_path = format!("/_matrix/federation/v2/send_join/{room_id}/{event_id}");
    let send_join_response = client
        .send(destination, "PUT", &send_join_path, Some(&signed_value))
        .await
        .map_err(|source| OutboundJoinError::Client {
            destination: destination.to_owned(),
            source,
        })?;
    if send_join_response.status / 100 != 2 {
        return Err(OutboundJoinError::Rejected {
            destination: destination.to_owned(),
            step: "send_join",
            status: send_join_response.status,
            body: send_join_response.body,
        });
    }

    let state = verify_array(
        &send_join_response.body,
        "state",
        &room_version,
        key_cache,
        destination,
    )
    .await?;
    let auth_chain = verify_array(
        &send_join_response.body,
        "auth_chain",
        &room_version,
        key_cache,
        destination,
    )
    .await?;
    let members_omitted = send_join_response
        .body
        .get("members_omitted")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(RemoteJoinOutcome {
        room_id: room_id.to_owned(),
        room_version,
        join_event,
        state,
        auth_chain,
        members_omitted,
    })
}

/// Hashes, redacts and signs `template` the way the spec's "Adding hashes and signatures to
/// outgoing events" requires: content hash of the full event, then redact, then sign the
/// *redacted* object, then copy the resulting signature back onto the original, unredacted event.
/// Mirrors `crates/hs-room/src/pipeline.rs`'s `build_and_authorize` (fixed by
/// `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`) and this crate's own
/// `inbound`/`backfill` test helpers, which sign exactly this way for the same reason: a
/// spec-compliant verifier always redacts before checking, so signing the full object instead
/// produces a signature that mismatches for any event whose content redaction does not fully
/// retain.
fn sign_join_template(
    template: &Value,
    rules: &hs_model::room_version::RoomVersionRules,
    own_server_name: &ServerName,
    signing_key: &SigningKeyPair,
) -> Result<Value, String> {
    let mut canonical = to_canonical_object(template, rules.strict_canonical_json)
        .map_err(|e| format!("join template is not valid canonical JSON: {e}"))?;
    let content_hash = hs_model::hash::content_hash_base64(&canonical);
    canonical.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(CanonicalJsonObject::from([(
            "sha256".to_owned(),
            CanonicalJsonValue::String(content_hash),
        )])),
    );
    let mut redacted = hs_model::redaction::redact(&canonical, &rules.redaction)
        .map_err(|e| format!("could not redact the join template before signing: {e}"))?;
    sign_object(&mut redacted, own_server_name, signing_key)
        .map_err(|e| format!("could not sign the redacted join event: {e}"))?;
    canonical.insert(
        "signatures".to_owned(),
        redacted
            .remove("signatures")
            .expect("sign_object always inserts a signature"),
    );
    serde_json::from_slice(&CanonicalJsonValue::Object(canonical).to_canonical_bytes())
        .map_err(|e| format!("signed join event did not round-trip to JSON: {e}"))
}

/// Reads `body[field]` as an array of raw PDUs and verifies each one, failing on the first that
/// does not verify (a resident server that hands back even one unverifiable event in its own
/// state snapshot is not a resident server worth trusting further).
async fn verify_array(
    body: &Value,
    field: &'static str,
    room_version: &RoomVersionId,
    key_cache: &DynRemoteKeyCache,
    destination: &str,
) -> Result<Vec<Event>, OutboundJoinError> {
    let raw_events = body.get(field).and_then(Value::as_array).ok_or_else(|| {
        OutboundJoinError::MalformedResponse(
            destination.to_owned(),
            format!("missing or non-array `{field}`"),
        )
    })?;
    let mut verified = Vec::with_capacity(raw_events.len());
    for raw in raw_events {
        let event = verify_pdu(raw, room_version, key_cache)
            .await
            .map_err(|source| OutboundJoinError::UnverifiedEvent {
                destination: destination.to_owned(),
                source,
            })?;
        verified.push(event);
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination_store::InMemoryDestinationStore;
    use crate::discovery::{AddrResolver, SrvResolver, WellKnownFetcher, WellKnownOutcome};
    use crate::inbound::{WriteOutcome, WriteRejected};
    use crate::keys::{
        DynRemoteKeyCache, KeyServerFetcher, OwnSigningKeys, RemoteKeyCache,
        build_server_key_response,
    };
    use crate::room_source::{FakeRoom, InMemoryRoomSource};
    use crate::transport::{FederationState, InMemoryQuerySource};
    use crate::xmatrix::XMatrixContext;
    use async_trait::async_trait;
    use std::net::IpAddr;
    use std::sync::Arc;

    /// A resolver that answers every hostname with `127.0.0.1` and never touches the network --
    /// this module's tests run a fake resident server on loopback instead of over TLS to a real
    /// remote, since discovery, TLS and CA trust are `crate::discovery`/`crate::client`'s own
    /// tests, not this module's.
    struct LoopbackResolver;
    #[async_trait]
    impl AddrResolver for LoopbackResolver {
        async fn resolve_addr(&self, _hostname: &str) -> Vec<IpAddr> {
            vec!["127.0.0.1".parse().unwrap()]
        }
    }
    #[async_trait]
    impl SrvResolver for LoopbackResolver {
        async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
            Vec::new()
        }
    }

    /// Every destination in this module's tests carries an explicit port
    /// (`resident.example.org:PORT`), which per `crate::discovery`'s own module doc resolves via
    /// a direct A/AAAA lookup and never consults `.well-known` at all -- so this fetcher only
    /// needs to exist to satisfy [`FederationClient::new`]'s signature, never to be called.
    struct UnusedWellKnown;
    #[async_trait]
    impl WellKnownFetcher for UnusedWellKnown {
        async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
            panic!("well-known should never be consulted for a destination with an explicit port")
        }
    }

    struct FixedFetcher(Value);
    #[async_trait]
    impl KeyServerFetcher for FixedFetcher {
        async fn fetch_server_key(&self, _server_name: &str) -> Option<Value> {
            Some(self.0.clone())
        }
    }

    fn key_cache(keys: &OwnSigningKeys, origin: &str) -> DynRemoteKeyCache {
        let doc = build_server_key_response(origin, keys, &[], 3600).unwrap();
        RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>)
    }

    fn own_signing_key() -> SigningKeyPair {
        SigningKeyPair::generate("a_1")
    }

    fn make_client(own_server_name: &str, signing_key: SigningKeyPair) -> FederationClient {
        FederationClient::new(
            own_server_name.to_owned(),
            signing_key,
            crate::client::ClientConfig {
                scheme: "http",
                ip_policy: crate::client::IpPolicy::from_cidrs(&[], &[]),
                ..Default::default()
            },
            Arc::new(InMemoryDestinationStore::default()),
            Arc::new(UnusedWellKnown),
            Arc::new(LoopbackResolver),
            Arc::new(LoopbackResolver),
        )
    }

    /// A [`RoomWriteSink`] that accepts any event as newly stored -- standing in, for this test
    /// only, for what `hs-cli`'s real `RegistryWriteSink` genuinely does when the resident server
    /// already hosts the room and the event is new and valid (exactly this test's scenario: see
    /// the module doc for why this crate cannot depend on `hs-room` directly to prove that with
    /// the real sink instead).
    struct AcceptingWriteSink;
    #[async_trait]
    impl crate::inbound::RoomWriteSink for AcceptingWriteSink {
        async fn accept_verified_event(
            &self,
            _room_id: &str,
            _event_id: &str,
            _event_json: &Value,
        ) -> Result<WriteOutcome, WriteRejected> {
            Ok(WriteOutcome::Stored)
        }
    }

    /// Hashes, redacts and signs a state event the same real way [`sign_join_template`] (and
    /// `crates/hs-room/src/pipeline.rs`) do, so events fed through this crate's own real
    /// `verify_pdu` (as [`join_room`]'s response verification does, for real, in the test below)
    /// actually pass -- unlike `crate::join::tests::room_with_creator`'s fixture, whose bare
    /// `serde_json::json!` events (no `hashes`, no `signatures`, no `origin_server_ts`) are never
    /// run through `verify_pdu` in that module's own tests.
    #[allow(clippy::too_many_arguments)]
    fn signed_state_event(
        keys: &OwnSigningKeys,
        room_id: &str,
        event_type: &str,
        sender: &str,
        state_key: &str,
        content: Value,
        prev_events: Vec<String>,
        auth_events: Vec<String>,
        depth: i64,
    ) -> Value {
        let mut object = to_canonical_object(
            &serde_json::json!({
                "type": event_type,
                "room_id": room_id,
                "sender": sender,
                "state_key": state_key,
                "origin_server_ts": depth * 1000,
                "depth": depth,
                "content": content,
                "prev_events": prev_events,
                "auth_events": auth_events,
            }),
            true,
        )
        .unwrap();
        let hash = hs_model::hash::content_hash_base64(&object);
        object.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(hash),
            )])),
        );
        let server = ruma::ServerName::parse(sender.split_once(':').unwrap().1).unwrap();
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
        let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
        sign_object(&mut redacted, &server, keys.primary()).unwrap();
        object.insert(
            "signatures".to_owned(),
            redacted.remove("signatures").unwrap(),
        );
        serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap()
    }

    fn event_id_of(value: &Value) -> String {
        Event::parse(value, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string()
    }

    /// A minimal, self-consistent, **really signed** room hosted by `resident.example.org:{port}`:
    /// one `m.room.create`, power levels, public join rules and the creator's own join -- enough
    /// for `crate::join::make_join`'s real auth checks to authorize a new member joining, and for
    /// [`join_room`]'s own real `verify_pdu` check on the way back to accept every one of them.
    fn resident_room(server_name: &str, keys: &OwnSigningKeys) -> (InMemoryRoomSource, String) {
        let room_id = format!("!r:{server_name}");
        let creator = format!("@creator:{server_name}");

        let create = signed_state_event(
            keys,
            &room_id,
            "m.room.create",
            &creator,
            "",
            serde_json::json!({"creator": creator, "room_version": "11"}),
            vec![],
            vec![],
            1,
        );
        let create_id = event_id_of(&create);

        let power_levels = signed_state_event(
            keys,
            &room_id,
            "m.room.power_levels",
            &creator,
            "",
            serde_json::json!({
                "users": {creator.clone(): 100}, "users_default": 0,
                "invite": 0, "kick": 50, "ban": 50, "redact": 50, "state_default": 50,
                "events_default": 0, "events": {}, "notifications": {"room": 50},
            }),
            vec![create_id.clone()],
            vec![create_id.clone()],
            2,
        );
        let power_id = event_id_of(&power_levels);

        let join_rules = signed_state_event(
            keys,
            &room_id,
            "m.room.join_rules",
            &creator,
            "",
            serde_json::json!({"join_rule": "public"}),
            vec![power_id.clone()],
            vec![create_id.clone(), power_id.clone()],
            3,
        );
        let join_rules_id = event_id_of(&join_rules);

        let creator_join = signed_state_event(
            keys,
            &room_id,
            "m.room.member",
            &creator,
            &creator,
            serde_json::json!({"membership": "join"}),
            vec![join_rules_id.clone()],
            vec![create_id.clone(), power_id.clone(), join_rules_id.clone()],
            4,
        );
        let creator_join_id = event_id_of(&creator_join);

        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            &room_id,
            FakeRoom {
                room_version: Some("11".to_owned()),
                extremities: vec![(creator_join_id, 4)],
                state: vec![
                    create.clone(),
                    power_levels.clone(),
                    join_rules.clone(),
                    creator_join,
                ],
                join_auth_chain: vec![create, power_levels, join_rules],
                ..FakeRoom::default()
            },
        );
        (rooms, room_id)
    }

    /// Boots a real resident federation server -- the actual `crate::transport::router`/
    /// `router_v2`, bound to a real loopback TCP socket via `axum::serve`, exactly as `hs serve`
    /// mounts them (`crates/hs-cli/src/serve.rs`) -- and returns its server name (with the port
    /// baked in, since this test has no DNS) and room ID.
    async fn spawn_resident(
        joiner_signing_key: &SigningKeyPair,
    ) -> (String, String, OwnSigningKeys) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_name = format!("resident.example.org:{port}");

        let dir = tempfile::tempdir().unwrap();
        let resident_keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let (rooms, room_id) = resident_room(&server_name, &resident_keys);
        // The joiner's key is fetched lazily by the resident's own X-Matrix verification layer;
        // seeded here (with the *same* key the test's own client signs with, not a freshly
        // generated one) since there is no real key server in this test.
        let joiner_keys = OwnSigningKeys::from_keys(vec![joiner_signing_key.clone()]);
        let joiner_doc =
            build_server_key_response("joiner.example.org", &joiner_keys, &[], 3600).unwrap();
        let key_cache: Arc<DynRemoteKeyCache> = Arc::new(RemoteKeyCache::new(
            Box::new(FixedFetcher(joiner_doc)) as Box<dyn KeyServerFetcher>,
        ));

        let state = FederationState {
            own_server_name: Arc::from(server_name.as_str()),
            rooms: Arc::new(rooms),
            queries: Arc::new(InMemoryQuerySource::default()),
            allow_public_rooms_over_federation: true,
            allow_device_name_lookup_over_federation: true,
            write_sink: Arc::new(AcceptingWriteSink),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
        };
        let ctx = Arc::new(XMatrixContext {
            own_server_name: server_name.clone(),
            key_cache,
        });

        let (v1, _) = crate::transport::router(state.clone(), ctx.clone());
        let (v2, _) = crate::transport::router_v2(state, ctx);
        let app = axum::Router::new()
            .nest("/_matrix/federation/v1", v1)
            .nest("/_matrix/federation/v2", v2);

        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });

        (server_name, room_id, resident_keys)
    }

    /// The deliverable this whole module exists for, proven end to end against a real HTTP
    /// server on a real loopback socket (no `tower::oneshot`, no mocked transport): a joiner
    /// asks a resident it has never talked to before for a join template, signs it itself, submits
    /// it, and gets back a fully verified room snapshot -- the same handshake
    /// `crates/hs-federation/scripts/two-server-federation.sh` runs between two real `hs serve`
    /// processes, exercised here as an automated regression instead of a manual script.
    #[tokio::test]
    async fn join_room_completes_the_real_handshake_against_a_live_resident() {
        let joiner_signing_key = own_signing_key();
        let (resident_name, room_id, resident_keys) = spawn_resident(&joiner_signing_key).await;
        let joiner_server_name = ruma::ServerName::parse("joiner.example.org").unwrap();
        let user_id = "@bob:joiner.example.org";

        let client = make_client("joiner.example.org", joiner_signing_key.clone());
        let key_cache = key_cache(&resident_keys, &resident_name);

        let outcome = join_room(
            &client,
            &key_cache,
            &resident_name,
            &room_id,
            user_id,
            &joiner_server_name,
            &joiner_signing_key,
        )
        .await
        .expect("a fresh join against a room that allows public joins must succeed");

        assert_eq!(outcome.room_id, room_id);
        assert_eq!(outcome.room_version, RoomVersionId::V11);
        assert!(!outcome.members_omitted);
        assert_eq!(outcome.join_event.header().sender.as_str(), user_id);
        assert_eq!(outcome.join_event.header().event_type, "m.room.member");
        // The resident's real state (create, power levels, join rules, creator's join) plus, once
        // persisted, the new join itself -- but `AcceptingWriteSink` does not actually mutate the
        // fixture's fake room store, so `state_for_join` still answers with the pre-join snapshot
        // (four events): this assertion is about what was verified, not about persistence, which
        // is the honest gap the module doc names.
        assert_eq!(outcome.state.len(), 4);
        assert_eq!(outcome.auth_chain.len(), 3);
    }

    #[tokio::test]
    async fn join_room_reports_a_clean_rejection_for_an_unknown_room() {
        let joiner_signing_key = own_signing_key();
        let (resident_name, _room_id, resident_keys) = spawn_resident(&joiner_signing_key).await;
        let joiner_server_name = ruma::ServerName::parse("joiner.example.org").unwrap();

        let client = make_client("joiner.example.org", joiner_signing_key.clone());
        let key_cache = key_cache(&resident_keys, &resident_name);

        let err = join_room(
            &client,
            &key_cache,
            &resident_name,
            &format!("!nope:{resident_name}"),
            "@bob:joiner.example.org",
            &joiner_server_name,
            &joiner_signing_key,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(
                err,
                OutboundJoinError::Rejected {
                    step: "make_join",
                    ..
                }
            ),
            "unexpected error: {err}"
        );
    }

    /// `sign_join_template` produces a signature `verify_pdu` accepts, and one that would be
    /// rejected by the *old*, wrong "sign the full event" order -- the same mutation test
    /// `docs/status/06-federation.md`'s sixth session ran on `verify_pdu` itself, run here against
    /// this module's own signing step so a regression back to the RFC-0014 bug would be caught
    /// locally, not only by whatever remote server next receives one of this server's joins.
    #[tokio::test]
    async fn sign_join_template_produces_a_verifiable_join_event() {
        let keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let server_name = ServerName::parse("joiner.example.org").unwrap();
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
        let template = serde_json::json!({
            "type": "m.room.member",
            "room_id": "!room:resident.example.org",
            "sender": "@alice:joiner.example.org",
            "state_key": "@alice:joiner.example.org",
            "content": {"membership": "join", "displayname": "Alice"},
            "origin_server_ts": 0,
            "prev_events": ["$prev"],
            "auth_events": ["$create", "$power_levels"],
            "depth": 4,
        });

        let signed = sign_join_template(&template, &rules, &server_name, keys.primary()).unwrap();
        let cache = key_cache(&keys, "joiner.example.org");
        let event = verify_pdu(&signed, &RoomVersionId::V11, &cache)
            .await
            .expect("a correctly (redacted-form) signed event must verify");
        assert_eq!(event.header().event_type, "m.room.member");

        // Mutation test: sign the *full*, unredacted object directly -- exactly RFC-0014's bug,
        // and exactly what a naive implementation of this function would do -- and confirm
        // `verify_pdu` (which always redacts before checking, per the spec) rejects it. `content`
        // here carries `displayname`, which `m.room.member` redaction strips
        // (`hs_model::redaction::redact_room_member_content`), so the two signing orders produce
        // different bytes and therefore different signatures. If `sign_join_template` ever
        // regresses to signing the full object, this assertion starts failing for the *fixed*
        // code path too, since both would then be identical.
        let mut wrongly_signed =
            to_canonical_object(&template, rules.strict_canonical_json).unwrap();
        let hash = hs_model::hash::content_hash_base64(&wrongly_signed);
        wrongly_signed.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(hash),
            )])),
        );
        sign_object(&mut wrongly_signed, &server_name, keys.primary()).unwrap();
        let wrongly_signed_value: Value = serde_json::from_slice(
            &CanonicalJsonValue::Object(wrongly_signed).to_canonical_bytes(),
        )
        .unwrap();
        let err = verify_pdu(&wrongly_signed_value, &RoomVersionId::V11, &cache)
            .await
            .expect_err(
                "a join event signed over its full, unredacted form must fail verify_pdu, which \
                 always redacts before checking a signature",
            );
        assert!(err.to_string().contains("does not verify"), "{err}");
    }

    #[test]
    fn client_error_display_names_the_destination() {
        let err = OutboundJoinError::Rejected {
            destination: "resident.example.org".to_owned(),
            step: "make_join",
            status: 404,
            body: serde_json::json!({"errcode": "M_NOT_FOUND"}),
        };
        let message = err.to_string();
        assert!(message.contains("resident.example.org"));
        assert!(message.contains("make_join"));
        assert!(message.contains("404"));
    }
}
