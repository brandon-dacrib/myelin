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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::signing::{SigningKeyPair, sign_object};
use ruma::{RoomVersionId, ServerName};
use serde_json::Value;
use tokio::time::Instant;

use crate::client::{ClientError, FederationClient};
use crate::inbound::{PduError, verify_pdu};
use crate::keys::{DynRemoteKeyCache, WantedKey};

/// Why a membership handshake this server started ([`join_room`], and the leave, knock and
/// invite in `crate::outbound_membership`) could not complete. The messages name no handshake;
/// `Rejected::step` does.
#[derive(Debug, thiserror::Error)]
pub enum OutboundJoinError {
    /// The outbound request itself failed (network, TLS, discovery, backoff, ...).
    #[error("federation request to {destination} failed: {source}")]
    Client {
        destination: String,
        #[source]
        source: ClientError,
    },
    /// The other server answered one step of the handshake with a non-2xx status.
    #[error("{destination} rejected {step} with HTTP {status}: {body}")]
    Rejected {
        destination: String,
        step: &'static str,
        status: u16,
        body: Value,
    },
    /// The template (`make_join`, `make_leave`, `make_knock`) was not shaped as the spec requires.
    #[error("the membership template from {0} was malformed: {1}")]
    MalformedTemplate(String, String),
    /// The membership event could not be hashed, redacted or signed.
    #[error("could not sign the membership event: {0}")]
    Signing(String),
    /// The answer to the second step (`send_*`, `invite`) was not shaped as the spec requires.
    #[error("the answer from {0} was malformed: {1}")]
    MalformedResponse(String, String),
    /// The event the other server answered with is not canonical JSON under the room version's
    /// rules (a float in a version-6 room): the answer is bad JSON, which the client is told
    /// (`400 M_BAD_JSON`; Sytest's "Outbound federation rejects invite response which include
    /// invalid JSON for room version 6"), not a server that could not be reached.
    #[error("the answer from {destination} is not canonical JSON: {reason}")]
    NotCanonicalJson { destination: String, reason: String },
    /// An event in the returned `state` or `auth_chain` failed the same verification any inbound
    /// PDU gets -- content hash or signature. Carries the failing event's raw JSON for logging;
    /// never trusted further than that.
    #[error("the answer from {destination} included an event that failed verification: {source}")]
    UnverifiedEvent {
        destination: String,
        #[source]
        source: PduError,
    },
    /// The room's `m.room.create` event in the `state` did not verify, so the room cannot be
    /// joined: nothing else in the snapshot can be authorised without it. Names the key it is
    /// signed with and why it could not be verified -- typically a key the room's server
    /// rotated out years ago and no longer publishes, and that no notary
    /// (`federation.trusted_key_servers`) had either -- instead of the "state must hold
    /// exactly one m.room.create" the room layer answered until 2026-10-10 once the create
    /// event had been silently dropped with the rest of the unverifiable ones.
    #[error(
        "the room's m.room.create event, signed by {sender} with {key_id}, could not be \
         verified, so the room cannot be joined through {destination}: {reason}"
    )]
    UnverifiedCreateEvent {
        destination: String,
        /// The server that signed it (the room's creator's).
        sender: String,
        /// The key id its signature names, or `?` when it carries none.
        key_id: String,
        /// Why: what the server and each notary answered for the key.
        reason: String,
    },
}

/// An event of a `send_join` answer that [`verify_array`] dropped, and why.
#[derive(Debug)]
pub struct DroppedEvent {
    /// Its `type`, or `?`.
    pub event_type: String,
    /// The server of its `sender`, or `?`.
    pub sender_server: String,
    /// The key id its sender's signature names, or `?`.
    pub key_id: String,
    /// What failed.
    pub failure: PduError,
}

/// What [`verify_array`] answered: the events that verified, in the answer's order, and the
/// ones dropped.
#[derive(Debug)]
pub struct VerifiedArray {
    /// Every event that verified, in the order it came.
    pub events: Vec<Event>,
    /// Every event that did not, with why.
    pub dropped: Vec<DroppedEvent>,
}

impl VerifiedArray {
    /// The error for a dropped `m.room.create` ([`OutboundJoinError::UnverifiedCreateEvent`]):
    /// a join cannot proceed without it, and the reason should name the key, not the hole it
    /// left in the state.
    fn dropped_create_event(&self, destination: &str) -> Option<OutboundJoinError> {
        self.dropped
            .iter()
            .find(|dropped| dropped.event_type == "m.room.create")
            .map(|dropped| OutboundJoinError::UnverifiedCreateEvent {
                destination: destination.to_owned(),
                sender: dropped.sender_server.clone(),
                key_id: dropped.key_id.clone(),
                reason: dropped.failure.to_string(),
            })
    }
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
/// doc); `PUT /_matrix/federation/v2/send_join/{room_id}/{event_id}` (the v1 spelling when the
/// resident does not answer v2: `put_v2_falling_back_to_v1`); verification of every event
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
    join_room_with_content(
        client,
        key_cache,
        destination,
        room_id,
        user_id,
        own_server_name,
        signing_key,
        None,
    )
    .await
}

/// [`join_room`], with `content` -- the fields the joining user's client asked to put into its
/// own `m.room.member` event beyond `membership` (`displayname`, `avatar_url`, `reason`) --
/// merged into the template the resident hands back before it is signed. `membership` itself is
/// never overridden: the template's own value stands. This is how a user's profile reaches a
/// room hosted elsewhere, exactly as `hs-room`'s local join carries it.
///
/// `make_join` is asked with every room version this server supports (`?ver=`), as the spec
/// requires: a resident that hosts a room in a version the joiner cannot handle answers
/// `M_INCOMPATIBLE_ROOM_VERSION` up front, and Synapse in particular treats a request with no
/// `ver` at all as "version 1 only" and refuses every modern room.
///
/// # Errors
/// See [`OutboundJoinError`].
#[allow(clippy::too_many_arguments)]
pub async fn join_room_with_content(
    client: &FederationClient,
    key_cache: &DynRemoteKeyCache,
    destination: &str,
    room_id: &str,
    user_id: &str,
    own_server_name: &ServerName,
    signing_key: &SigningKeyPair,
    content: Option<&Value>,
) -> Result<RemoteJoinOutcome, OutboundJoinError> {
    let SignedTemplate {
        value: signed_value,
        event: join_event,
        room_version,
    } = make_and_sign(
        client,
        destination,
        "make_join",
        true,
        room_id,
        user_id,
        own_server_name,
        signing_key,
        content,
    )
    .await?;
    let event_id = join_event.event_id().to_string();

    let send_join_response = put_v2_falling_back_to_v1(
        client,
        destination,
        "send_join",
        &format!(
            "send_join/{}/{}",
            crate::client::encode_path_segment(room_id),
            crate::client::encode_path_segment(&event_id)
        ),
        &signed_value,
    )
    .await?;

    let progress = VerifyProgress::new(destination, room_id);
    let state = verify_array(
        &send_join_response.body,
        "state",
        &room_version,
        key_cache,
        destination,
        &progress,
    )
    .await?;
    if let Some(error) = state.dropped_create_event(destination) {
        progress.finish();
        return Err(error);
    }
    let state = state.events;
    let auth_chain = verify_array(
        &send_join_response.body,
        "auth_chain",
        &room_version,
        key_cache,
        destination,
        &progress,
    )
    .await?
    .events;
    progress.finish();
    let members_omitted = send_join_response
        .body
        .get("members_omitted")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // A restricted join authorised by a user of another server (the resident, whose template
    // named them) is only valid with that server's signature as well as this one's; the
    // resident adds it at `send_join` and answers with the co-signed event, which is the one
    // this server keeps.
    let join_event = match crate::inbound::join_authoriser_server(&join_event) {
        Some(authoriser) if authoriser != own_server_name.as_str() => {
            let returned = send_join_response.body.get("event").ok_or_else(|| {
                OutboundJoinError::MalformedResponse(
                    destination.to_owned(),
                    format!(
                        "the join names an authoriser on {authoriser} but no co-signed `event` came back"
                    ),
                )
            })?;
            let cosigned = verify_pdu(returned, &room_version, key_cache)
                .await
                .map_err(|source| OutboundJoinError::UnverifiedEvent {
                    destination: destination.to_owned(),
                    source,
                })?;
            if cosigned.event_id() != join_event.event_id() {
                return Err(OutboundJoinError::MalformedResponse(
                    destination.to_owned(),
                    "the co-signed join is not the join this server sent".to_owned(),
                ));
            }
            cosigned
        }
        _ => join_event,
    };

    Ok(RemoteJoinOutcome {
        room_id: room_id.to_owned(),
        room_version,
        join_event,
        state,
        auth_chain,
        members_omitted,
    })
}

/// `PUT /_matrix/federation/v2/{path}` with `body`, and when `destination` does not know the v2
/// spelling -- `404`, or `400 M_UNRECOGNIZED` -- the same request to `/_matrix/federation/v1/{path}`,
/// whose `[200, {...}]` answer is unwrapped to its object. Synapse falls back the same way for
/// `send_join` and `send_leave`. The answer to whichever was asked last is returned when it is a
/// 2xx; [`OutboundJoinError::Rejected`] naming `step` otherwise.
///
/// Until 2026-10-01 only v2 was asked, and a server without it (Sytest's own server answers v2
/// `404` in its "Outbound federation can query v1 /send_join") could not be joined through.
///
/// # Errors
/// [`OutboundJoinError::Client`] if a request fails, [`OutboundJoinError::Rejected`] for a
/// non-2xx answer.
pub(crate) async fn put_v2_falling_back_to_v1(
    client: &FederationClient,
    destination: &str,
    step: &'static str,
    path: &str,
    body: &Value,
) -> Result<crate::client::FederationResponse, OutboundJoinError> {
    let put = |version: &'static str| {
        let full = format!("/_matrix/federation/{version}/{path}");
        async move {
            client
                .send(destination, "PUT", &full, Some(body))
                .await
                .map_err(|source| OutboundJoinError::Client {
                    destination: destination.to_owned(),
                    source,
                })
        }
    };
    let mut response = put("v2").await?;
    let unrecognized = response.status == 404
        || (response.status == 400
            && response.body.get("errcode").and_then(Value::as_str) == Some("M_UNRECOGNIZED"));
    if unrecognized {
        tracing::info!(
            destination,
            step,
            status = response.status,
            "the other server does not answer the v2 spelling; asking v1"
        );
        response = put("v1").await?;
        if response.status / 100 == 2
            && let Value::Array(pair) = &mut response.body
            && pair.len() == 2
        {
            let inner = pair.pop().unwrap_or(Value::Null);
            response.body = inner;
        }
    }
    if response.status / 100 != 2 {
        return Err(OutboundJoinError::Rejected {
            destination: destination.to_owned(),
            step,
            status: response.status,
            body: response.body,
        });
    }
    Ok(response)
}

/// A membership template a resident handed out, signed by this server: the JSON to submit, the
/// parsed event and the room version it was parsed under.
pub(crate) struct SignedTemplate {
    pub(crate) value: Value,
    pub(crate) event: Event,
    pub(crate) room_version: RoomVersionId,
}

/// The first half of every membership handshake this server initiates: `GET {step}` from
/// `destination` (`make_join`, `make_leave` or `make_knock`, asked with every supported room
/// version when `with_versions`), `content` merged into the template (never its `membership`),
/// and the result hashed, redacted and signed as this server's event.
///
/// # Errors
/// See [`OutboundJoinError`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn make_and_sign(
    client: &FederationClient,
    destination: &str,
    step: &'static str,
    with_versions: bool,
    room_id: &str,
    user_id: &str,
    own_server_name: &ServerName,
    signing_key: &SigningKeyPair,
    content: Option<&Value>,
) -> Result<SignedTemplate, OutboundJoinError> {
    let template_path = if with_versions {
        let supported_versions: Vec<String> = hs_model::room_version::known_room_version_ids()
            .map(|v| format!("ver={v}"))
            .collect();
        format!(
            "/_matrix/federation/v1/{step}/{}/{}?{}",
            crate::client::encode_path_segment(room_id),
            crate::client::encode_path_segment(user_id),
            supported_versions.join("&")
        )
    } else {
        format!(
            "/_matrix/federation/v1/{step}/{}/{}",
            crate::client::encode_path_segment(room_id),
            crate::client::encode_path_segment(user_id)
        )
    };
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
            step,
            status: make_join_response.status,
            body: make_join_response.body,
        });
    }
    let mut template = make_join_response
        .body
        .get("event")
        .cloned()
        .ok_or_else(|| {
            OutboundJoinError::MalformedTemplate(
                destination.to_owned(),
                "missing `event`".to_owned(),
            )
        })?;
    if let Some(overlay) = content.and_then(Value::as_object) {
        let template_content = template
            .as_object_mut()
            .ok_or_else(|| {
                OutboundJoinError::MalformedTemplate(
                    destination.to_owned(),
                    "`event` is not an object".to_owned(),
                )
            })?
            .entry("content")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(map) = template_content.as_object_mut() {
            for (key, value) in overlay {
                // The resident's choice of who authorises a restricted join is the resident's:
                // it is the one it will co-sign.
                if key != "membership"
                    && !(key == "join_authorised_via_users_server" && map.contains_key(key))
                {
                    map.insert(key.clone(), value.clone());
                }
            }
        }
    }
    // "If not provided, the room version is assumed to be either "1" or "2"" (the spec's
    // `make_join`, `make_leave` and `make_knock` responses); the two share an event format, and
    // Synapse reads a missing one as "1". Refusing the template instead failed every join
    // through Sytest's own server, which leaves it out (502 to the client until 2026-10-01).
    let room_version_str = match make_join_response.body.get("room_version") {
        None | Some(Value::Null) => "1".to_owned(),
        Some(Value::String(version)) => version.clone(),
        Some(other) => {
            return Err(OutboundJoinError::MalformedTemplate(
                destination.to_owned(),
                format!("`room_version` is not a string: {other}"),
            ));
        }
    };
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
    let event = Event::parse(&signed_value, room_version.clone()).map_err(|e| {
        OutboundJoinError::Signing(format!("signed {step} event does not parse: {e}"))
    })?;
    Ok(SignedTemplate {
        value: signed_value,
        event,
        room_version,
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
///
/// The template is the resident's suggestion, not the event: `origin_server_ts` is always this
/// server's own clock (the join is this server's event, timestamped when it made it, the same
/// as Synapse's `make_membership_event`), and `origin` is this server's name when the template
/// left it out. Complement's reference federation server, for one, hands out a template with
/// neither, and an event without `origin_server_ts` does not even parse.
fn sign_join_template(
    template: &Value,
    rules: &hs_model::room_version::RoomVersionRules,
    own_server_name: &ServerName,
    signing_key: &SigningKeyPair,
) -> Result<Value, String> {
    let mut canonical = to_canonical_object(template, rules.strict_canonical_json)
        .map_err(|e| format!("join template is not valid canonical JSON: {e}"))?;
    canonical.insert(
        "origin_server_ts".to_owned(),
        CanonicalJsonValue::Integer(now_ms()),
    );
    canonical
        .entry("origin".to_owned())
        .or_insert_with(|| CanonicalJsonValue::String(own_server_name.to_string()));
    // Room versions 1 and 2 carry the event's ID in the event, chosen by the server that makes
    // it (`$<opaque>:<server>`), not derived from its hash; without one the joining event does
    // not even parse, and no version-1 or -2 room could be joined from here.
    if rules.event_format_requires_event_id {
        canonical.entry("event_id".to_owned()).or_insert_with(|| {
            CanonicalJsonValue::String(ruma::EventId::new_v1(own_server_name).to_string())
        });
    }
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

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// How many servers' events of a `send_join` answer are verified at once. Verifying is waiting
/// for key fetches, one per server the snapshot cites, and the servers are independent: the
/// events are grouped by their sender's server, the groups are verified this many at a time,
/// and within a group one after another (the first event fetches the key, or learns the
/// server is gone; the rest find it cached, or refused at once). A snapshot citing `n` servers
/// that are gone costs `ceil(n / 64)` fetch budgets ([`crate::keys::DEFAULT_KEY_FETCH_TIMEOUT`]),
/// not `n` request timeouts. Each slot is at most one key fetch in flight, so this also bounds
/// the connections a join opens at once.
pub const JOIN_VERIFY_CONCURRENCY: usize = 64;

/// How often a join that is still verifying says so in the log.
pub const JOIN_VERIFY_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// The verification of one join's `send_join` answer, as the log and the metrics see it: an
/// `info` line every [`JOIN_VERIFY_PROGRESS_INTERVAL`] while it runs (verified and dropped so
/// far, the servers whose events are in flight, the slowest server so far) and one at the end
/// with the totals and the elapsed time, observed into `hs_federation_join_verify_seconds`.
/// An operator reading the log sees that a slow join is alive and which server is making it
/// slow; before this, a join of a large room was silent for hours.
pub struct VerifyProgress {
    destination: String,
    room_id: String,
    started: Instant,
    inner: std::sync::Mutex<ProgressInner>,
}

#[derive(Default)]
struct ProgressInner {
    verified: usize,
    dropped: usize,
    /// The servers whose events are being verified now, with how many of their events are in
    /// flight and when the earliest of those started.
    in_flight: HashMap<String, (usize, Instant)>,
    /// The server whose event took longest to verify so far, and how long.
    slowest: Option<(String, Duration)>,
}

impl VerifyProgress {
    /// A fresh report for a join of `room_id` through `destination`, starting now.
    #[must_use]
    pub fn new(destination: &str, room_id: &str) -> Self {
        Self {
            destination: destination.to_owned(),
            room_id: room_id.to_owned(),
            started: Instant::now(),
            inner: std::sync::Mutex::new(ProgressInner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProgressInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One of `server`'s events starts verifying.
    fn begin(&self, server: &str) -> Instant {
        let now = Instant::now();
        let mut inner = self.lock();
        let entry = inner.in_flight.entry(server.to_owned()).or_insert((0, now));
        entry.0 += 1;
        now
    }

    /// One of `server`'s events, started at `began`, finished: verified or dropped.
    fn end(&self, server: &str, began: Instant, verified: bool) {
        let took = began.elapsed();
        let mut inner = self.lock();
        if verified {
            inner.verified += 1;
        } else {
            inner.dropped += 1;
        }
        if let Some(entry) = inner.in_flight.get_mut(server) {
            entry.0 = entry.0.saturating_sub(1);
            if entry.0 == 0 {
                inner.in_flight.remove(server);
            }
        }
        if inner
            .slowest
            .as_ref()
            .is_none_or(|(_, slowest)| took > *slowest)
        {
            inner.slowest = Some((server.to_owned(), took));
        }
    }

    /// The counts so far: `(verified, dropped)`.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.verified, inner.dropped)
    }

    /// The slowest server so far: the longest a completed event took, or the longest an event
    /// still in flight has been waiting, whichever is longer.
    fn slowest(&self, inner: &ProgressInner) -> Option<(String, Duration)> {
        let now = Instant::now();
        let mut slowest = inner.slowest.clone();
        for (server, (_, since)) in &inner.in_flight {
            let waiting = now.saturating_duration_since(*since);
            if slowest.as_ref().is_none_or(|(_, took)| waiting > *took) {
                slowest = Some((server.clone(), waiting));
            }
        }
        slowest
    }

    /// The periodic line: what has been verified, what is in flight and who is slow.
    fn report(&self, field: &'static str, total: usize) {
        let inner = self.lock();
        let slowest = self.slowest(&inner);
        let mut pending: Vec<&String> = inner.in_flight.keys().collect();
        pending.sort();
        let pending_shown: Vec<&str> = pending.iter().take(8).map(|s| s.as_str()).collect();
        tracing::info!(
            destination = %self.destination,
            room_id = %self.room_id,
            field,
            total,
            verified = inner.verified,
            dropped = inner.dropped,
            elapsed_secs = self.started.elapsed().as_secs(),
            servers_pending = pending.len(),
            pending = ?pending_shown,
            slowest_server = slowest.as_ref().map(|(server, _)| server.as_str()).unwrap_or("-"),
            slowest_secs = slowest.as_ref().map_or(0, |(_, took)| took.as_secs()),
            "still verifying the events a send_join answer carried"
        );
    }

    /// The final line, and the histogram observation.
    pub fn finish(&self) {
        let elapsed = self.started.elapsed();
        let inner = self.lock();
        let slowest = self.slowest(&inner);
        crate::metrics::record_join_verify_seconds(elapsed.as_secs_f64());
        tracing::info!(
            destination = %self.destination,
            room_id = %self.room_id,
            verified = inner.verified,
            dropped = inner.dropped,
            elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            slowest_server = slowest.as_ref().map(|(server, _)| server.as_str()).unwrap_or("-"),
            slowest_ms = slowest
                .as_ref()
                .map_or(0, |(_, took)| u64::try_from(took.as_millis()).unwrap_or(u64::MAX)),
            "verified the events a send_join answer carried"
        );
    }
}

/// The server of a raw event's `sender`, or `?` when it has none to speak of (the event is
/// then dropped as malformed by [`verify_pdu`], and grouped with the other malformed ones).
fn sender_server(raw: &Value) -> &str {
    raw.get("sender")
        .and_then(Value::as_str)
        .and_then(|sender| sender.split_once(':').map(|(_, server)| server))
        .unwrap_or("?")
}

/// The key id a raw event's signature by `server` names (the first, as [`verify_pdu`] checks
/// it), if it carries one.
fn signature_key_id(raw: &Value, server: &str) -> Option<String> {
    raw.get("signatures")?
        .get(server)?
        .as_object()?
        .keys()
        .next()
        .cloned()
}

/// The keys verifying `raw_events` will ask for ([`RemoteKeyCache::ensure_keys`]): each
/// event's sender's signing key, valid at its `origin_server_ts`, and for a restricted join
/// the authoriser's. An event that names neither a sender nor a key is left to
/// [`verify_pdu`] to refuse.
fn wanted_keys(raw_events: &[Value]) -> Vec<WantedKey> {
    let mut wanted: Vec<WantedKey> = Vec::new();
    for raw in raw_events {
        let signed_at_ts = raw
            .get("origin_server_ts")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut servers = vec![sender_server(raw).to_owned()];
        if let Some(authoriser) = raw
            .pointer("/content/join_authorised_via_users_server")
            .and_then(Value::as_str)
            .and_then(|user| user.split_once(':').map(|(_, server)| server.to_owned()))
        {
            servers.push(authoriser);
        }
        for server in servers {
            if server == "?" {
                continue;
            }
            let Some(key_id) = signature_key_id(raw, &server) else {
                continue;
            };
            let key = WantedKey {
                server_name: server,
                key_id,
                signed_at_ts,
            };
            if !wanted.contains(&key) {
                wanted.push(key);
            }
        }
    }
    wanted
}

/// One event of a `send_join` answer through [`verify_pdu`], told to `progress`, and logged
/// with its reason when it is dropped.
async fn verify_one(
    raw: &Value,
    field: &'static str,
    room_version: &RoomVersionId,
    key_cache: &DynRemoteKeyCache,
    destination: &str,
    progress: &VerifyProgress,
) -> Result<Event, PduError> {
    let sender = sender_server(raw);
    let began = progress.begin(sender);
    let result = verify_pdu(raw, room_version, key_cache).await;
    progress.end(sender, began, result.is_ok());
    if let Err(failure) = &result {
        tracing::warn!(
            destination,
            field,
            event_type = raw.get("type").and_then(serde_json::Value::as_str).unwrap_or("?"),
            sender = raw.get("sender").and_then(serde_json::Value::as_str).unwrap_or("?"),
            %failure,
            "dropping an event from a send_join response that did not verify"
        );
    }
    result
}

/// Reads `body[field]` as an array of raw PDUs and verifies each one, **dropping** any that do
/// not verify. A resident relays events from every server that was ever in the room, and one of
/// those servers' keys being unobtainable, or one event arriving with its signatures stripped,
/// says nothing about the rest: the spec's rule for a received event that fails verification is
/// to drop that event, and a join must not fail because of it (Complement's
/// `TestJoinFederatedRoomWithUnverifiableEvents`, which strips or corrupts the signature on one
/// state event and expects the join to succeed). What was dropped is logged with its reason, and
/// [`OutboundJoinError::UnverifiedEvent`] is kept for the one case that is not survivable: a
/// snapshot in which nothing verified at all.
///
/// The keys every event will need are fetched first, in one pass
/// ([`RemoteKeyCache::ensure_keys`]): the servers [`JOIN_VERIFY_CONCURRENCY`] at a time and,
/// for the keys they do not publish, the notaries once for the whole batch. Then the events
/// are grouped by their sender's server and the groups verified [`JOIN_VERIFY_CONCURRENCY`]
/// at a time (see that constant), each lookup now a cache hit or an immediate refusal, and
/// what verified is answered in the order it came, since the caller's auth chain is ordered.
/// Until 2026-10-10 the events were verified one after another, and a large room's snapshot,
/// citing thousands of servers, took hours. `progress` is told of every event, and reports
/// every [`JOIN_VERIFY_PROGRESS_INTERVAL`] while this runs.
async fn verify_array(
    body: &Value,
    field: &'static str,
    room_version: &RoomVersionId,
    key_cache: &DynRemoteKeyCache,
    destination: &str,
    progress: &VerifyProgress,
) -> Result<VerifiedArray, OutboundJoinError> {
    let raw_events = body.get(field).and_then(Value::as_array).ok_or_else(|| {
        OutboundJoinError::MalformedResponse(
            destination.to_owned(),
            format!("missing or non-array `{field}`"),
        )
    })?;
    let total = raw_events.len();
    let mut ticker = tokio::time::interval_at(
        Instant::now() + JOIN_VERIFY_PROGRESS_INTERVAL,
        JOIN_VERIFY_PROGRESS_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    {
        let wanted = wanted_keys(raw_events);
        tracing::debug!(
            destination,
            field,
            events = total,
            keys = wanted.len(),
            "fetching the keys a send_join answer's events are signed with"
        );
        let prefetch = key_cache.ensure_keys(&wanted);
        let mut prefetch = std::pin::pin!(prefetch);
        loop {
            tokio::select! {
                () = &mut prefetch => break,
                _ = ticker.tick() => progress.report(field, total),
            }
        }
    }
    // The events of each server, in the answer's order, the servers in order of first
    // appearance.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_of: HashMap<&str, usize> = HashMap::new();
    for (index, raw) in raw_events.iter().enumerate() {
        let server = sender_server(raw);
        let group = *group_of.entry(server).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[group].push(index);
    }
    let verify_group = |indexes: Vec<usize>| async move {
        let mut results = Vec::with_capacity(indexes.len());
        for index in indexes {
            let result = verify_one(
                &raw_events[index],
                field,
                room_version,
                key_cache,
                destination,
                progress,
            )
            .await;
            results.push((index, result));
        }
        results
    };
    let results = futures::stream::iter(groups)
        .map(verify_group)
        .buffer_unordered(JOIN_VERIFY_CONCURRENCY);
    let mut results = std::pin::pin!(results);

    let mut outcomes: Vec<Option<Result<Event, PduError>>> = (0..total).map(|_| None).collect();
    loop {
        tokio::select! {
            next = results.next() => match next {
                Some(group) => {
                    for (index, result) in group {
                        outcomes[index] = Some(result);
                    }
                }
                None => break,
            },
            _ = ticker.tick() => progress.report(field, total),
        }
    }
    let mut verified = Vec::with_capacity(total);
    let mut dropped: Vec<DroppedEvent> = Vec::new();
    for (raw, outcome) in raw_events.iter().zip(outcomes) {
        match outcome {
            Some(Ok(event)) => verified.push(event),
            Some(Err(failure)) => {
                let sender_server = sender_server(raw).to_owned();
                dropped.push(DroppedEvent {
                    event_type: raw
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_owned(),
                    key_id: signature_key_id(raw, &sender_server).unwrap_or_else(|| "?".to_owned()),
                    sender_server,
                    failure,
                });
            }
            None => {}
        }
    }
    if verified.is_empty()
        && let Some(first) = dropped.pop()
    {
        return Err(OutboundJoinError::UnverifiedEvent {
            destination: destination.to_owned(),
            source: first.failure,
        });
    }
    Ok(VerifiedArray {
        events: verified,
        dropped,
    })
}

// -------------------------------------------------------------------------------------------
// One join at a time per (room, user)
// -------------------------------------------------------------------------------------------

/// Whether a request started the join it was answered with, or attached to one already under
/// way ([`InFlightJoins::run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinShare {
    /// This request started the join.
    Started,
    /// A join of the same room for the same user was under way; this request waited for it.
    Attached,
}

impl JoinShare {
    /// The `share` label of `hs_federation_join_requests_total`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Attached => "attached",
        }
    }
}

/// The task running a join ended without an answer (it panicked, or the runtime shut down), so
/// the requests waiting on it have nothing to be answered with.
#[derive(Debug, thiserror::Error)]
#[error("the join of {room_id} for {user_id} ended without an answer")]
pub struct JoinTaskLost {
    pub room_id: String,
    pub user_id: String,
}

/// One join at a time per `(room, user)`: a second `/join` for the same pair while one is
/// running attaches to the running one and is answered with its outcome, instead of starting a
/// second `make_join`/`send_join` handshake that the room's server answers the same and that
/// verifies the same thousands of events again. Clients retry a `/join` that did not answer in
/// time (Element after its proxy's 100 s), and until 2026-10-10 each retry was a whole new join.
///
/// The join runs in its own task (`tokio::spawn`), so it outlives the request that started it:
/// a client that goes away leaves the join running, and its retry finds it and attaches. `T` is
/// the join's outcome, shared with every request that attached, so it must be `Clone`
/// (`Arc<Result<..>>` for an error that is not).
pub struct InFlightJoins<T: Clone + Send + Sync + 'static> {
    running: RunningJoins<T>,
}

/// The joins under way, by `(room_id, user_id)`, each with the channel its outcome is announced
/// on (`None` until it is).
type RunningJoins<T> =
    Arc<std::sync::Mutex<HashMap<(String, String), tokio::sync::watch::Receiver<Option<T>>>>>;

/// A join's entry in [`InFlightJoins`], removed when dropped: at the end of the task, or when
/// the task panics and unwinds, so a lost join does not leave an entry every later request
/// would attach to and never be answered from.
struct RunningEntry<T> {
    running: RunningJoins<T>,
    key: (String, String),
}

impl<T> Drop for RunningEntry<T> {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl<T: Clone + Send + Sync + 'static> Default for InFlightJoins<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Send + Sync + 'static> InFlightJoins<T> {
    /// Nothing under way.
    #[must_use]
    pub fn new() -> Self {
        Self {
            running: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// How many joins are under way.
    #[must_use]
    pub fn running(&self) -> usize {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// The outcome of the join of `room_id` for `user_id`: the one under way if there is one
    /// ([`JoinShare::Attached`]), else the one `start` makes, run in a task of its own
    /// ([`JoinShare::Started`]). `start` is called only when a join is started, under the lock
    /// that decides it, so two requests arriving together start one join.
    ///
    /// # Errors
    /// [`JoinTaskLost`] when the task ended without an outcome (it panicked).
    pub async fn run<F, Fut>(
        &self,
        room_id: &str,
        user_id: &str,
        start: F,
    ) -> Result<(T, JoinShare), JoinTaskLost>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        let key = (room_id.to_owned(), user_id.to_owned());
        let (mut receiver, share) = {
            let mut running = self
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match running.get(&key) {
                Some(receiver) => (receiver.clone(), JoinShare::Attached),
                None => {
                    let (sender, receiver) = tokio::sync::watch::channel(None);
                    running.insert(key.clone(), receiver.clone());
                    let future = start();
                    let entry = RunningEntry {
                        running: Arc::clone(&self.running),
                        key: key.clone(),
                    };
                    tokio::spawn(async move {
                        let outcome = future.await;
                        // Out of the map before the answer goes out, so a request arriving now
                        // starts a join of its own rather than attaching to a finished one.
                        drop(entry);
                        // Nobody waiting is fine: the join happened, which is what matters.
                        let _ = sender.send(Some(outcome));
                    });
                    (receiver, JoinShare::Started)
                }
            }
        };
        crate::metrics::record_join_request(share.as_str());
        if share == JoinShare::Attached {
            tracing::info!(
                room_id,
                user_id,
                "a join of this room for this user is already under way; answering with its \
                 outcome rather than starting another"
            );
        }
        let lost = || JoinTaskLost {
            room_id: room_id.to_owned(),
            user_id: user_id.to_owned(),
        };
        let outcome = receiver
            .wait_for(Option::is_some)
            .await
            .map_err(|_closed| lost())?
            .clone()
            .ok_or_else(lost)?;
        Ok((outcome, share))
    }
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
    use futures::FutureExt as _;
    use std::net::IpAddr;

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
            policy: crate::transport::InboundPolicy::new(true, true),
            write_sink: Arc::new(AcceptingWriteSink),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
            invites: None,
            edu_sink: None,
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

        let outcome = join_room_with_content(
            &client,
            &key_cache,
            &resident_name,
            &room_id,
            user_id,
            &joiner_server_name,
            &joiner_signing_key,
            Some(&serde_json::json!({"displayname": "Bob", "membership": "leave"})),
        )
        .await
        .expect("a fresh join against a room that allows public joins must succeed");

        assert_eq!(outcome.room_id, room_id);
        assert_eq!(outcome.room_version, RoomVersionId::V11);
        assert!(!outcome.members_omitted);
        assert_eq!(outcome.join_event.header().sender.as_str(), user_id);
        assert_eq!(outcome.join_event.header().event_type, "m.room.member");
        // The caller's profile rode along on the signed join; its attempt to override
        // `membership` did not.
        let content = outcome
            .join_event
            .json()
            .get("content")
            .and_then(CanonicalJsonValue::as_object)
            .cloned()
            .unwrap();
        assert_eq!(
            content
                .get("displayname")
                .and_then(CanonicalJsonValue::as_str),
            Some("Bob")
        );
        assert_eq!(
            content
                .get("membership")
                .and_then(CanonicalJsonValue::as_str),
            Some("join")
        );
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

    /// A resident shaped like Sytest's own federation server: its `make_join` answer has no
    /// `room_version` (the spec: then version 1 or 2) and it answers the v2 `send_join` `404`,
    /// so only v1 (`[200, {...}]`) completes the join. Both broke every join through it with a
    /// 502 until 2026-10-01: "the membership template ... was malformed: missing
    /// `room_version`", and with that fixed, a v2 `404`.
    #[tokio::test]
    async fn a_template_without_a_room_version_and_a_resident_without_v2_send_join_still_join() {
        use axum::routing::{get, put};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let resident = format!("sytest.example.org:{port}");
        let room_id = format!("!4:{resident}");
        let v2_asked = Arc::new(AtomicUsize::new(0));
        let v1_body: Arc<std::sync::Mutex<Option<Value>>> = Arc::default();

        let template = serde_json::json!({
            "type": "m.room.member",
            "room_id": room_id,
            "sender": "@bob:joiner.example.org",
            "state_key": "@bob:joiner.example.org",
            "content": {"membership": "join"},
            "depth": 3,
            "origin_server_ts": 1,
            "prev_events": [[format!("$10:{resident}"), {"sha256": "AQnpUtoiUgT0E3RsD9DZHUvje901wxHZgyt62fqexbE"}]],
            "auth_events": [[format!("$8:{resident}"), {"sha256": "nM8kHXePWZh+fKbo4qkHpTcLjz+8YsmXm1wjyFp0iB8"}]],
        });
        let app = axum::Router::new()
            .route(
                "/_matrix/federation/v1/make_join/{room}/{user}",
                get(move || {
                    let template = template.clone();
                    async move { axum::Json(serde_json::json!({ "event": template })) }
                }),
            )
            .route(
                "/_matrix/federation/v2/send_join/{room}/{event}",
                put({
                    let v2_asked = v2_asked.clone();
                    move || {
                        v2_asked.fetch_add(1, Ordering::SeqCst);
                        async { axum::http::StatusCode::NOT_FOUND }
                    }
                }),
            )
            .route(
                "/_matrix/federation/v1/send_join/{room}/{event}",
                put({
                    let v1_body = v1_body.clone();
                    move |axum::Json(body): axum::Json<Value>| {
                        *v1_body.lock().unwrap() = Some(body);
                        async {
                            axum::Json(serde_json::json!([200, {"state": [], "auth_chain": []}]))
                        }
                    }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });

        let joiner_signing_key = own_signing_key();
        let client = make_client("joiner.example.org", joiner_signing_key.clone());
        let unused_keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let outcome = join_room(
            &client,
            &key_cache(&unused_keys, &resident),
            &resident,
            &room_id,
            "@bob:joiner.example.org",
            &ServerName::parse("joiner.example.org").unwrap(),
            &joiner_signing_key,
        )
        .await
        .expect("a version-1 template and a v1-only send_join must still make a join");

        assert_eq!(outcome.room_version, RoomVersionId::V1);
        assert_eq!(v2_asked.load(Ordering::SeqCst), 1, "v2 is asked first");
        let submitted = v1_body
            .lock()
            .unwrap()
            .clone()
            .expect("v1 send_join was asked");
        // A version-1 event carries the ID its server chose for it.
        assert_eq!(
            submitted["event_id"].as_str(),
            Some(outcome.join_event.event_id().as_str())
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

    /// Complement's reference federation server answers `make_join` with a template that has
    /// neither `origin_server_ts` nor `origin`; the joiner supplies both, or the signed event
    /// does not even parse and every join through that server fails.
    #[test]
    fn a_template_without_a_timestamp_or_origin_is_completed_before_signing() {
        let keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let server_name = ServerName::parse("joiner.example.org").unwrap();
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
        let template = serde_json::json!({
            "type": "m.room.member",
            "room_id": "!room:resident.example.org",
            "sender": "@alice:joiner.example.org",
            "state_key": "@alice:joiner.example.org",
            "content": {"membership": "join"},
            "prev_events": ["$prev"],
            "auth_events": ["$create"],
            "depth": 4,
        });
        let before = now_ms();
        let signed = sign_join_template(&template, &rules, &server_name, keys.primary()).unwrap();
        let ts = signed["origin_server_ts"].as_i64().unwrap();
        assert!(
            ts >= before && ts <= now_ms(),
            "origin_server_ts is this server's clock"
        );
        assert_eq!(signed["origin"], "joiner.example.org");
        Event::parse(&signed, RoomVersionId::V11).expect("the completed event parses");
    }

    /// A template for a version-1 room gets an event ID of this server's making before it is
    /// hashed and signed: those versions carry it in the event, and without it the join event
    /// did not parse and no version-1 room could be joined from here.
    #[test]
    fn a_version_1_template_is_given_an_event_id_of_this_servers_making() {
        let keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let server_name = ServerName::parse("joiner.example.org").unwrap();
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V1).unwrap();
        let template = serde_json::json!({
            "type": "m.room.member",
            "room_id": "!room:resident.example.org",
            "sender": "@alice:joiner.example.org",
            "state_key": "@alice:joiner.example.org",
            "content": {"membership": "join"},
            "prev_events": [["$prev:resident.example.org", {"sha256": "aGFzaA"}]],
            "auth_events": [["$create:resident.example.org", {"sha256": "aGFzaA"}]],
            "depth": 4,
        });
        let signed = sign_join_template(&template, &rules, &server_name, keys.primary()).unwrap();
        let event_id = signed["event_id"].as_str().unwrap();
        assert!(event_id.starts_with('$') && event_id.ends_with(":joiner.example.org"));
        let event = Event::parse(&signed, RoomVersionId::V1).expect("the event parses");
        assert_eq!(event.event_id().as_str(), event_id);
    }

    /// One event with its signatures stripped does not sink the join: it is dropped, the rest
    /// are kept, and only a snapshot in which nothing verifies is an error.
    #[tokio::test]
    async fn verify_array_drops_what_does_not_verify_and_keeps_the_rest() {
        let keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let cache = key_cache(&keys, "resident.example.org");
        let good = signed_state_event(
            &keys,
            "!r:resident.example.org",
            "m.room.name",
            "@creator:resident.example.org",
            "",
            serde_json::json!({"name": "signed"}),
            vec![],
            vec![],
            2,
        );
        let mut stripped = signed_state_event(
            &keys,
            "!r:resident.example.org",
            "m.room.name",
            "@creator:resident.example.org",
            "",
            serde_json::json!({"name": "no signature"}),
            vec![],
            vec![],
            3,
        );
        stripped["signatures"] = serde_json::json!({});

        let progress = VerifyProgress::new("resident", "!r:resident.example.org");
        let body = serde_json::json!({"state": [good.clone(), stripped.clone()]});
        let kept = verify_array(
            &body,
            "state",
            &RoomVersionId::V11,
            &cache,
            "resident",
            &progress,
        )
        .await
        .unwrap();
        assert_eq!(kept.events.len(), 1);
        assert_eq!(kept.events[0].event_id().as_str(), event_id_of(&good));
        assert_eq!(kept.dropped.len(), 1);
        assert_eq!(kept.dropped[0].event_type, "m.room.name");
        assert_eq!(kept.dropped[0].sender_server, "resident.example.org");
        assert_eq!(kept.dropped[0].key_id, "?");

        assert_eq!(progress.counts(), (1, 1));

        let nothing = serde_json::json!({"state": [stripped]});
        let err = verify_array(
            &nothing,
            "state",
            &RoomVersionId::V11,
            &cache,
            "resident",
            &progress,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OutboundJoinError::UnverifiedEvent { .. }),
            "{err}"
        );

        let empty = serde_json::json!({"state": []});
        assert!(
            verify_array(
                &empty,
                "state",
                &RoomVersionId::V11,
                &cache,
                "resident",
                &progress
            )
            .await
            .unwrap()
            .events
            .is_empty()
        );
    }

    /// A fetcher with one key document per server, a delay before answering per server, and
    /// servers that never answer (gone: the connection hangs until a timeout).
    #[derive(Default)]
    struct ScenarioFetcher {
        docs: std::collections::HashMap<String, Value>,
        delays: std::collections::HashMap<String, std::time::Duration>,
        gone: Vec<String>,
        calls: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    }

    #[async_trait]
    impl KeyServerFetcher for ScenarioFetcher {
        async fn fetch_server_key(&self, server_name: &str) -> Option<Value> {
            *self
                .calls
                .lock()
                .unwrap()
                .entry(server_name.to_owned())
                .or_insert(0) += 1;
            if self.gone.iter().any(|s| s == server_name) {
                std::future::pending::<()>().await;
            }
            if let Some(delay) = self.delays.get(server_name) {
                tokio::time::sleep(*delay).await;
            }
            self.docs.get(server_name).cloned()
        }
    }

    fn scenario_cache(fetcher: ScenarioFetcher) -> Arc<DynRemoteKeyCache> {
        Arc::new(RemoteKeyCache::new(
            Box::new(fetcher) as Box<dyn KeyServerFetcher>
        ))
    }

    /// A state event of `!r:resident.example.org` sent by a user of `server`, signed by
    /// `keys` under that server's name.
    fn event_from(keys: &OwnSigningKeys, server: &str, depth: i64) -> Value {
        signed_state_event(
            keys,
            "!r:resident.example.org",
            "m.room.member",
            &format!("@u{depth}:{server}"),
            &format!("@u{depth}:{server}"),
            serde_json::json!({"membership": "join"}),
            vec![],
            vec![],
            depth,
        )
    }

    /// The measured fix for the join that took hours (status 06, 2026-10-10): a snapshot
    /// citing 20 servers that are gone, 10 events each, among 20 events from the resident.
    /// Before: one event after another, 30 s (the client's request timeout) per event from a
    /// gone server, 200 x 30 s = 100 minutes. After: the gone servers' fetches run in
    /// parallel and each costs the 10 s budget once, the other events from a gone server are
    /// refused at once by the backoff, and the whole snapshot verifies in 10 s. The kept events
    /// come back in the answer's order.
    #[tokio::test(start_paused = true)]
    async fn events_from_gone_servers_cost_one_budget_in_parallel_and_keep_their_order() {
        let keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let mut fetcher = ScenarioFetcher::default();
        fetcher.docs.insert(
            "resident.example.org".to_owned(),
            build_server_key_response("resident.example.org", &keys, &[], 3600).unwrap(),
        );
        let gone: Vec<String> = (0..20).map(|i| format!("gone-{i}.example.org")).collect();
        fetcher.gone = gone.clone();
        let calls = fetcher.calls.clone();
        let cache = scenario_cache(fetcher);

        let mut events = Vec::new();
        let mut depth = 1;
        for gone_server in &gone {
            events.push(event_from(&keys, "resident.example.org", depth));
            depth += 1;
            for _ in 0..10 {
                events.push(event_from(&keys, gone_server, depth));
                depth += 1;
            }
        }
        let expected: Vec<String> = events
            .iter()
            .filter(|e| {
                e["sender"]
                    .as_str()
                    .unwrap()
                    .ends_with(":resident.example.org")
            })
            .map(event_id_of)
            .collect();
        assert_eq!(events.len(), 220);

        let timeouts_before = crate::metrics::key_fetch_failures("timeout");
        let backoffs_before = crate::metrics::key_fetch_failures("backoff");
        let progress = VerifyProgress::new("resident.example.org", "!r:resident.example.org");
        let body = serde_json::json!({ "state": events });
        let started = tokio::time::Instant::now();
        let kept = verify_array(
            &body,
            "state",
            &RoomVersionId::V11,
            &cache,
            "resident.example.org",
            &progress,
        )
        .await
        .unwrap()
        .events;
        let elapsed = started.elapsed();
        progress.finish();

        let kept_ids: Vec<String> = kept.iter().map(|e| e.event_id().to_string()).collect();
        assert_eq!(
            kept_ids, expected,
            "the resident's events, in the answer's order"
        );
        assert_eq!(progress.counts(), (20, 200));
        assert_eq!(
            elapsed,
            crate::keys::DEFAULT_KEY_FETCH_TIMEOUT,
            "one fetch budget for the whole snapshot, not one per event"
        );
        let calls = calls.lock().unwrap().clone();
        for server in &gone {
            assert_eq!(calls.get(server), Some(&1), "{server} asked once");
        }
        assert_eq!(calls.get("resident.example.org"), Some(&1));
        // The counters are process-wide and other tests count into them at the same time.
        assert!(crate::metrics::key_fetch_failures("timeout") - timeouts_before >= 20);
        assert!(crate::metrics::key_fetch_failures("backoff") - backoffs_before >= 180);
    }

    /// Events are answered in the order they came, however their servers' fetches finish:
    /// three servers answering after 3 s, 1 s and at once, their events interleaved.
    #[tokio::test(start_paused = true)]
    async fn verification_keeps_the_answers_order_whatever_order_the_fetches_finish_in() {
        let keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let mut fetcher = ScenarioFetcher::default();
        let servers = ["slow.example.org", "medium.example.org", "fast.example.org"];
        for (server, delay) in servers.iter().zip([3u64, 1, 0]) {
            fetcher.docs.insert(
                (*server).to_owned(),
                build_server_key_response(server, &keys, &[], 3600).unwrap(),
            );
            fetcher
                .delays
                .insert((*server).to_owned(), std::time::Duration::from_secs(delay));
        }
        let calls = fetcher.calls.clone();
        let cache = scenario_cache(fetcher);
        let events: Vec<Value> = (1..=30)
            .map(|depth| event_from(&keys, servers[(depth as usize) % 3], depth))
            .collect();
        let expected: Vec<String> = events.iter().map(event_id_of).collect();

        let progress = VerifyProgress::new("resident.example.org", "!r:resident.example.org");
        let body = serde_json::json!({ "auth_chain": events });
        let started = tokio::time::Instant::now();
        let kept = verify_array(
            &body,
            "auth_chain",
            &RoomVersionId::V11,
            &cache,
            "resident.example.org",
            &progress,
        )
        .await
        .unwrap()
        .events;
        let kept_ids: Vec<String> = kept.iter().map(|e| e.event_id().to_string()).collect();
        assert_eq!(kept_ids, expected);
        assert_eq!(progress.counts(), (30, 0));
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(3));
        let calls = calls.lock().unwrap().clone();
        for server in servers {
            assert_eq!(calls.get(server), Some(&1), "{server} fetched once");
        }
    }

    /// More events than the pool is wide, from one gone server: still one budget.
    #[tokio::test(start_paused = true)]
    async fn a_gone_server_with_more_events_than_the_pool_still_costs_one_budget() {
        let keys = OwnSigningKeys::from_keys(vec![own_signing_key()]);
        let mut fetcher = ScenarioFetcher {
            gone: vec!["gone.example.org".to_owned()],
            ..Default::default()
        };
        fetcher.docs.insert(
            "resident.example.org".to_owned(),
            build_server_key_response("resident.example.org", &keys, &[], 3600).unwrap(),
        );
        let calls = fetcher.calls.clone();
        let cache = scenario_cache(fetcher);
        let mut events: Vec<Value> = (1..=(JOIN_VERIFY_CONCURRENCY as i64 * 3))
            .map(|depth| event_from(&keys, "gone.example.org", depth))
            .collect();
        events.push(event_from(&keys, "resident.example.org", 1000));

        let progress = VerifyProgress::new("resident.example.org", "!r:resident.example.org");
        let body = serde_json::json!({ "state": events });
        let started = tokio::time::Instant::now();
        let kept = verify_array(
            &body,
            "state",
            &RoomVersionId::V11,
            &cache,
            "resident.example.org",
            &progress,
        )
        .await
        .unwrap()
        .events;
        assert_eq!(kept.len(), 1);
        assert_eq!(progress.counts(), (1, JOIN_VERIFY_CONCURRENCY * 3));
        assert_eq!(started.elapsed(), crate::keys::DEFAULT_KEY_FETCH_TIMEOUT);
        assert_eq!(calls.lock().unwrap().get("gone.example.org"), Some(&1));
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

    // --- InFlightJoins ----------------------------------------------------------------------

    /// Two requests for the same (room, user) arriving together: one join runs, both are
    /// answered with its outcome, one as the starter and one attached, and nothing is left
    /// running afterwards.
    #[tokio::test]
    async fn two_requests_for_the_same_room_and_user_share_one_join() {
        let joins = Arc::new(InFlightJoins::<u32>::new());
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let released = released.map(|_| ()).shared();
        let start = {
            let starts = starts.clone();
            let released = released.clone();
            move || {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    released.await;
                    7u32
                }
            }
        };
        let first = tokio::spawn({
            let joins = joins.clone();
            async move { joins.run("!r:a", "@u:a", start).await.unwrap() }
        });
        tokio::task::yield_now().await;
        assert_eq!(joins.running(), 1);
        let attached_before = crate::metrics::join_requests("attached");
        let second = tokio::spawn({
            let joins = joins.clone();
            let starts = starts.clone();
            async move {
                joins
                    .run("!r:a", "@u:a", move || {
                        starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        async move { 99u32 }
                    })
                    .await
                    .unwrap()
            }
        });
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert_eq!(first, (7, JoinShare::Started));
        assert_eq!(second, (7, JoinShare::Attached));
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(crate::metrics::join_requests("attached") > attached_before);
        assert_eq!(joins.running(), 0);
        // Done: the next request starts a join of its own.
        let (again, share) = joins.run("!r:a", "@u:a", || async { 8u32 }).await.unwrap();
        assert_eq!((again, share), (8, JoinShare::Started));
    }

    /// A different room, or a different user of the same room, is a join of its own.
    #[tokio::test]
    async fn different_rooms_or_users_run_separately() {
        let joins = InFlightJoins::<&'static str>::new();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let released = released.map(|_| ()).shared();
        let gate = released.clone();
        let a = joins.run("!r:a", "@u:a", move || async move {
            gate.await;
            "a"
        });
        let b = joins.run("!r:a", "@v:a", || async { "b" });
        let c = joins.run("!s:a", "@u:a", || async { "c" });
        let (b, c) = futures::future::join(b, c).await;
        assert_eq!(b.unwrap(), ("b", JoinShare::Started));
        assert_eq!(c.unwrap(), ("c", JoinShare::Started));
        release.send(()).unwrap();
        assert_eq!(a.await.unwrap(), ("a", JoinShare::Started));
    }

    /// The request that started the join goes away (the client disconnected): the join goes on
    /// in its task, and the retry attaches to it and is answered when it is done.
    #[tokio::test]
    async fn a_dropped_request_leaves_the_join_running_for_the_retry_to_attach_to() {
        let joins = Arc::new(InFlightJoins::<u32>::new());
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let first = {
            let joins = joins.clone();
            tokio::spawn(async move {
                joins
                    .run("!r:a", "@u:a", move || async move {
                        let _ = released.await;
                        42u32
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        first.abort();
        let _ = first.await;
        assert_eq!(joins.running(), 1, "the join is still under way");
        let retry = {
            let joins = joins.clone();
            tokio::spawn(async move { joins.run("!r:a", "@u:a", || async { 0u32 }).await })
        };
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        assert_eq!(retry.await.unwrap().unwrap(), (42, JoinShare::Attached));
        assert_eq!(joins.running(), 0);
    }

    /// A join task that panics answers every request waiting on it with [`JoinTaskLost`], and
    /// is not left in the map.
    #[tokio::test]
    async fn a_join_task_that_panics_is_reported_to_every_waiter() {
        let joins = Arc::new(InFlightJoins::<u32>::new());
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let released = released.map(|_| ()).shared();
        let first = {
            let joins = joins.clone();
            let gate = released.clone();
            tokio::spawn(async move {
                joins
                    .run("!r:a", "@u:a", move || async move {
                        gate.await;
                        panic!("the join task fell over");
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        let second = {
            let joins = joins.clone();
            tokio::spawn(async move { joins.run("!r:a", "@u:a", || async { 1u32 }).await })
        };
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        let first = first.await.unwrap().unwrap_err();
        let second = second.await.unwrap().unwrap_err();
        assert_eq!(
            first.to_string(),
            "the join of !r:a for @u:a ended without an answer"
        );
        assert_eq!(second.room_id, "!r:a");
        // The entry went with the task (its removal guard ran while unwinding), so the next
        // request starts a join of its own instead of attaching to a closed channel.
        assert_eq!(joins.running(), 0);
        let (again, share) = joins.run("!r:a", "@u:a", || async { 5u32 }).await.unwrap();
        assert_eq!((again, share), (5, JoinShare::Started));
    }
}
