//! Inbound federation writes: PDU verification shared by `PUT /send/{txnId}` and `send_join`, plus
//! the transaction-idempotency store `/send` needs.
//!
//! # What this closes, and what it does not
//!
//! [`verify_pdu`] is real: it parses a PDU against the room's own version, checks its content hash
//! (threat model: a tampered-but-still-signed body must not pass), and verifies its signature
//! against the *sender's* server (not the transmitting `origin` -- a resident server relays events
//! from every domain in a room, not just its own). [`InMemoryTransactionStore`] makes a retried
//! transaction idempotent for real: replaying `(origin, txn_id)` returns the cached response
//! without reprocessing a single PDU.
//!
//! What this does **not** close: actually *persisting* a newly-received event into this server's
//! own room store. [`RoomWriteSink`] is the seam for that, and every implementation this session
//! ships (`crate::room_source::InMemoryRoomSource`'s test-only sink here, and
//! `hs-cli`'s real one) can only report success for an event this server already holds --
//! `hs-room`'s `RoomActor` has no API to accept an already-built, foreign-signed [`hs_model::Event`]
//! (`send_event`/`send_event_citing` only build and sign *new* locally-originated events). See
//! `docs/status/06-federation.md` for the RFC this gap needs from track 04.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use ruma::RoomVersionId;
use serde_json::Value;

use crate::keys::{DynRemoteKeyCache, KeyLookupError};

/// The spec's resource-limits table: no more than 50 PDUs or 100 EDUs in one transaction. A
/// transaction over either limit is rejected outright (not truncated) -- a sender that cannot keep
/// to its own transactions' shape is not a sender this server should try to partially trust.
pub const MAX_PDUS_PER_TRANSACTION: usize = 50;
/// See [`MAX_PDUS_PER_TRANSACTION`].
pub const MAX_EDUS_PER_TRANSACTION: usize = 100;

/// Why [`verify_pdu`] rejected a PDU.
#[derive(Debug, Clone)]
pub struct PduError {
    /// What failed, for logs and error bodies.
    pub message: String,
    /// The PDU is not signed as it must be: no signature from a server that must sign it, that
    /// server's key cannot be found, or the signature does not verify. An endpoint answering for
    /// one PDU (`send_join`) answers this `403 M_FORBIDDEN`, as Synapse does, and anything else
    /// (a PDU that does not parse) `400`.
    pub unsigned: bool,
}

impl std::fmt::Display for PduError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PduError {}

fn reject(message: impl Into<String>) -> PduError {
    PduError {
        message: message.into(),
        unsigned: false,
    }
}

fn unsigned(message: impl Into<String>) -> PduError {
    PduError {
        message: message.into(),
        unsigned: true,
    }
}

/// Finds the key ID `server_name` signed `object` under, if any -- there may be several (key
/// rotation mid-flight); the first one found is used, matching how `crate::xmatrix` picks a single
/// key ID from a header rather than trying every one a server has ever published.
fn signature_key_id(
    object: &hs_model::canonical::CanonicalJsonObject,
    server_name: &str,
) -> Option<String> {
    object
        .get("signatures")?
        .as_object()?
        .get(server_name)?
        .as_object()?
        .keys()
        .next()
        .cloned()
}

/// Recomputes an event's content hash and compares it against the `hashes.sha256` field it
/// declares. `false` for a missing or malformed `hashes` field, which is exactly as much "tampered
/// with" as a mismatched one.
fn content_hash_matches(event: &Event) -> bool {
    let Some(declared) = event
        .json()
        .get("hashes")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|h| h.get("sha256"))
        .and_then(CanonicalJsonValue::as_str)
    else {
        return false;
    };
    declared == hs_model::hash::content_hash_base64(event.json())
}

/// Parses, signature-verifies and hash-checks one PDU against the room version it claims. On
/// success, returns the parsed [`Event`] -- in its redacted form when its content hash does not
/// match, as the spec has it -- and callers still owe it an authorization check (`crate::join`
/// does this for `send_join`; `/send`'s own PDUs are not authorized against room state this
/// session, see the module doc) before treating it as accepted.
///
/// # Errors
/// Returns [`PduError`] if the PDU is malformed, oversized, is not signed by its sender's server,
/// or that server's key cannot be resolved or does not verify.
pub async fn verify_pdu(
    raw: &Value,
    room_version: &RoomVersionId,
    key_cache: &DynRemoteKeyCache,
) -> Result<Event, PduError> {
    verify_pdu_to_authorise(raw, room_version, key_cache, None).await
}

/// [`verify_pdu`], for a restricted join this server is being asked to authorise (`send_join`
/// naming one of `own_server_name`'s users in `join_authorised_via_users_server`): the
/// authoriser's signature is the one this server is about to add, so it is not required yet.
/// Every other check is the same.
///
/// # Errors
/// As [`verify_pdu`].
pub async fn verify_pdu_to_authorise(
    raw: &Value,
    room_version: &RoomVersionId,
    key_cache: &DynRemoteKeyCache,
    own_server_name: Option<&str>,
) -> Result<Event, PduError> {
    let event = Event::parse(raw, room_version.clone())
        .map_err(|e| reject(format!("malformed event: {e}")))?;

    // Per the spec ("Validating hashes and signatures on received events", server-server API):
    // "the event is redacted following the redaction algorithm, and the resultant object is
    // checked for signatures ... Note that this step should succeed whether we have been sent
    // the full event or a redacted copy." A conformant sender signs the *redacted* form of the
    // event (see the same spec's "Adding hashes and signatures to outgoing events": hash, then
    // redact, then sign) -- redaction is deterministic from `type` alone, so checking the
    // signature against the unredacted object instead would reject any legitimately-signed event
    // whose full content carries anything redaction would strip (almost every event with more
    // than the bare minimum required content -- a join with a profile, a message with a body,
    // ...). This was confirmed as a real bug via Complement (`docs/status/06-federation.md`):
    // `send_join` was rejecting a genuinely, correctly signed join event with `M_BAD_JSON:
    // signature ... does not verify` because it checked the full event instead of the redacted
    // one.
    let sender_server = event.header().sender.server_name().as_str().to_owned();
    verify_server_signature(&event, &sender_server, key_cache).await?;
    if let Some(authoriser) = join_authoriser_server(&event)
        && authoriser != sender_server
        && Some(authoriser.as_str()) != own_server_name
    {
        verify_server_signature(&event, &authoriser, key_cache).await?;
    }
    // Then the content hash: "If the hash check fails, the event is redacted before processing
    // further" (the same spec section). The signatures cover the redacted form, so an event
    // whose content was stripped or changed on the way -- a server serving one it redacted, as
    // Synapse and this server do -- is still the event its sender signed, and is taken in its
    // redacted form, not refused. Until 2026-10-01 it was refused ("content hash does not
    // match"; Sytest's "Inbound federation can receive redacted events").
    if !content_hash_matches(&event) {
        tracing::info!(
            event_id = %event.event_id(),
            sender = %event.header().sender,
            "a received event's content does not match its hash; taking it redacted"
        );
        let redacted = event
            .redacted_json()
            .map_err(|e| reject(format!("could not redact an event whose hash fails: {e}")))?;
        let value: Value =
            serde_json::from_slice(&CanonicalJsonValue::Object(redacted).to_canonical_bytes())
                .map_err(|e| reject(format!("could not redact an event whose hash fails: {e}")))?;
        let redacted = Event::parse(&value, room_version.clone())
            .map_err(|e| reject(format!("the redacted event does not parse: {e}")))?;
        return Ok(redacted);
    }
    Ok(event)
}

/// The server of the user a restricted join names as its authoriser
/// (`content.join_authorised_via_users_server`), in a room version that checks it (8 and up):
/// the spec's "Validating hashes and signatures on received events" requires that server's
/// signature on the join as well as the sender's. `None` for any other event, and for a value
/// that is not a user ID (event authorization rejects that).
#[must_use]
pub fn join_authoriser_server(event: &Event) -> Option<String> {
    let rules = hs_model::room_version::rules_for(&event.header().room_version)?;
    if !rules.check_join_authorised_via_users_server || event.header().event_type != "m.room.member"
    {
        return None;
    }
    let content = event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)?;
    if content
        .get("membership")
        .and_then(CanonicalJsonValue::as_str)
        != Some("join")
    {
        return None;
    }
    let via = content
        .get("join_authorised_via_users_server")
        .and_then(CanonicalJsonValue::as_str)?;
    ruma::UserId::parse(via)
        .ok()
        .map(|user| user.server_name().to_string())
}

/// Checks that `event` carries a valid signature from `server` over its redacted form -- the
/// signature half of [`verify_pdu`], for a server other than the sender's: the invitee's server
/// co-signing an invite (`crate::invite`) is the case that needs it.
///
/// # Errors
/// Returns [`PduError`] if the event cannot be redacted, carries no signature from `server`,
/// `server`'s key cannot be resolved, or the signature does not verify.
pub async fn verify_server_signature(
    event: &Event,
    server: &str,
    key_cache: &DynRemoteKeyCache,
) -> Result<(), PduError> {
    let redacted = event.redacted_json().map_err(|e| {
        reject(format!(
            "cannot redact event for signature verification: {e}"
        ))
    })?;

    let key_id = signature_key_id(&redacted, server)
        .ok_or_else(|| unsigned(format!("no signature from {server}")))?;

    let signed_at = u64::try_from(event.header().origin_server_ts).unwrap_or(0);
    let verifying_key = key_cache
        .get_valid_at(server, &key_id, signed_at)
        .await
        .map_err(|e| {
            unsigned(format!(
                "key lookup for {server}/{key_id} failed: {}",
                key_lookup_reason(&e)
            ))
        })?;

    hs_model::signing::verify_object(&redacted, server, &key_id, &verifying_key)
        .map_err(|_| unsigned(format!("signature from {server}/{key_id} does not verify")))
}

fn key_lookup_reason(e: &KeyLookupError) -> String {
    e.to_string()
}

/// The full, serialized event JSON of an already-verified [`Event`], for handing to
/// [`RoomWriteSink`] or embedding in a `send_join` response. Round-trips through the same
/// canonical bytes [`Event::parse`] already validated, so this cannot disagree with what was
/// verified.
#[must_use]
pub fn event_json(event: &Event) -> Value {
    serde_json::from_slice(event.canonical_bytes())
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}

/// What happened when a [`RoomWriteSink`] was asked to apply an already-verified event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// This server already had the event (by ID); applying it again was a no-op. Real and
    /// idempotent: this is the one case this session's sinks can actually promise.
    AlreadyKnown,
    /// Newly and durably stored.
    Stored,
}

/// Why a [`RoomWriteSink`] could not apply an event.
#[derive(Debug, Clone)]
pub struct WriteRejected {
    pub error: String,
    /// The event IDs of ancestors (`prev_events`/`auth_events`) this server does not hold, if
    /// that is why the event was rejected. **Empty** for every other kind of rejection (bad
    /// signature, failed authorization, unknown room, ...) -- callers (`crate::backfill`) use
    /// this, not string-matching on [`WriteRejected::error`], to decide whether a gap is worth
    /// trying to close.
    pub missing_ancestors: Vec<String>,
    /// Whether event authorization refused the event (as opposed to it being unusable, or
    /// unplaceable for now). Such an event was received and processed: `/send` answers `{}` for
    /// it, as the spec's "a rejected event is still processed" and Synapse do, rather than an
    /// error the sending server would read as a delivery failure.
    pub auth_rejected: bool,
}

impl WriteRejected {
    /// An ordinary rejection with nothing to backfill.
    #[must_use]
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
            missing_ancestors: Vec::new(),
            auth_rejected: false,
        }
    }

    /// A rejection by event authorization: the event was processed and refused
    /// ([`WriteRejected::auth_rejected`]).
    #[must_use]
    pub fn auth(message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
            missing_ancestors: Vec::new(),
            auth_rejected: true,
        }
    }

    /// A rejection because `missing` ancestors are not held by this server yet -- the one kind of
    /// rejection [`crate::backfill::resolve_missing_ancestors`] can act on.
    #[must_use]
    pub fn missing_ancestors(missing: Vec<String>, message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
            missing_ancestors: missing,
            auth_rejected: false,
        }
    }
}

/// The write half of "accept a federation event", shared by `/send`'s per-PDU processing and
/// `send_join`'s persistence step. See the module doc for what today's implementations can and
/// cannot promise.
#[async_trait]
pub trait RoomWriteSink: Send + Sync {
    /// Applies `event_json` (already hash- and signature-verified by [`verify_pdu`]) to `room_id`.
    /// `event_id` is passed explicitly rather than read back out of `event_json`: for room version
    /// 3 and later the wire form of a PDU does not carry `event_id` at all (it is derived from the
    /// reference hash), so the caller -- which parsed the event and therefore already knows its
    /// ID -- hands it over rather than making every implementation re-derive it.
    async fn accept_verified_event(
        &self,
        room_id: &str,
        event_id: &str,
        event_json: &Value,
    ) -> Result<WriteOutcome, WriteRejected>;

    /// Which of `event_ids` this server does not hold in `room_id` in any form (in the
    /// timeline, as an outlier, or as rejected): what the `/state_ids` fallback
    /// (`crate::state_fallback`) must fetch. The default knows nothing, so everything is
    /// fetched; a sink over a real room store answers from its index.
    async fn unknown_events(&self, room_id: &str, event_ids: &[String]) -> Vec<String> {
        let _ = room_id;
        event_ids.to_vec()
    }

    /// Holds `prev_event` -- a prev event of a received event, which this server could not walk
    /// back to -- with the state before it as another server answered `/state_ids`
    /// (`state_before`, event IDs), and the events of that state, its auth chain and
    /// `prev_event`'s own auth events that this server lacked (`fetched`). Every event handed
    /// over is already hash- and signature-verified ([`verify_pdu`]); what the sink does with
    /// them (authorise each against its own auth events, hold what passes as outliers, store
    /// what fails as rejected, hold `prev_event` with the state) is `hs-room`'s
    /// `RoomActor::accept_prev_event_with_state`. See `crate::state_fallback`.
    ///
    /// The default refuses: a sink that cannot hold an event with a fetched state.
    ///
    /// # Errors
    /// [`WriteRejected::auth`] when `prev_event` fails authorisation at that state (it is then
    /// stored rejected, and an event citing it is judged at the state before it);
    /// [`WriteRejected::other`] when it cannot be held at all.
    async fn accept_prev_event_with_state(
        &self,
        room_id: &str,
        prev_event_id: &str,
        prev_event: &Value,
        state_before: &[String],
        fetched: &[Value],
    ) -> Result<WriteOutcome, WriteRejected> {
        let _ = (room_id, prev_event_id, prev_event, state_before, fetched);
        Err(WriteRejected::other(
            "this sink cannot hold a prev event with a fetched state",
        ))
    }

    /// Holds `events` -- auth events of a received event that this server lacked, fetched by
    /// `/event` and already hash- and signature-verified -- as outliers, each judged by its own
    /// `auth_events` (one that fails is stored rejected, so an event citing it is rejected in
    /// turn). See `crate::state_fallback::fetch_missing_auth_events`. Returns how many were
    /// held, rejected ones included.
    ///
    /// The default refuses.
    ///
    /// # Errors
    /// [`WriteRejected::other`] when they cannot be held at all.
    async fn accept_auth_outliers(
        &self,
        room_id: &str,
        events: &[Value],
    ) -> Result<usize, WriteRejected> {
        let _ = (room_id, events);
        Err(WriteRejected::other(
            "this sink cannot hold fetched auth events",
        ))
    }
}

/// A [`RoomWriteSink`] that already knows every event it will ever be asked about (for this
/// crate's own tests) or that knows none (the honest default for a room source with no events at
/// all).
pub struct StaticWriteSink {
    known: std::collections::HashSet<String>,
    reject_message: String,
}

impl StaticWriteSink {
    /// A sink that already knows `known_event_ids` and rejects everything else with
    /// `reject_message`.
    #[must_use]
    pub fn new(
        known_event_ids: impl IntoIterator<Item = String>,
        reject_message: impl Into<String>,
    ) -> Self {
        Self {
            known: known_event_ids.into_iter().collect(),
            reject_message: reject_message.into(),
        }
    }
}

#[async_trait]
impl RoomWriteSink for StaticWriteSink {
    async fn accept_verified_event(
        &self,
        _room_id: &str,
        event_id: &str,
        _event_json: &Value,
    ) -> Result<WriteOutcome, WriteRejected> {
        if self.known.contains(event_id) {
            return Ok(WriteOutcome::AlreadyKnown);
        }
        Err(WriteRejected::other(self.reject_message.clone()))
    }
}

/// Caches a transaction's response by `(origin, txn_id)` so a retried transaction (the same
/// sender, the same transaction ID, arriving again -- a normal consequence of a timed-out response
/// whose request nonetheless succeeded) is answered from cache rather than reprocessed. Scoped to
/// this process's lifetime: the realistic threat this defends is a network-level retry within a
/// connection's lifetime, not a replay after a server restart -- see this crate's status file for
/// why a `KvBackend`-backed version was not built this session.
#[async_trait]
pub trait TransactionStore: Send + Sync {
    /// The cached response for `(origin, txn_id)`, if this transaction was already processed.
    async fn get(&self, origin: &str, txn_id: &str) -> Option<Value>;
    /// Records the response this transaction produced.
    async fn put(&self, origin: &str, txn_id: &str, response: Value);
}

/// The in-memory [`TransactionStore`] used both by this crate's own tests and by `hs-cli`'s
/// wiring.
#[derive(Default)]
pub struct InMemoryTransactionStore {
    seen: Mutex<HashMap<(String, String), Value>>,
}

impl InMemoryTransactionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TransactionStore for InMemoryTransactionStore {
    async fn get(&self, origin: &str, txn_id: &str) -> Option<Value> {
        self.seen
            .lock()
            .unwrap()
            .get(&(origin.to_owned(), txn_id.to_owned()))
            .cloned()
    }

    async fn put(&self, origin: &str, txn_id: &str, response: Value) {
        self.seen
            .lock()
            .unwrap()
            .insert((origin.to_owned(), txn_id.to_owned()), response);
    }
}

/// Processes one `/send` transaction body: enforces the PDU/EDU count limits, replays a
/// known-`(origin, txn_id)` transaction from cache, otherwise verifies and applies each PDU in
/// order and records the response for future replays.
///
/// A PDU rejected for missing ancestors (`WriteRejected::missing_ancestors` non-empty) is not
/// immediately reported as an error: if `ancestor_fetcher` is supplied, the gap is closed via
/// [`crate::backfill::resolve_missing_ancestors`] against `origin` (the server that sent us this
/// transaction) and the PDU is retried exactly once before falling back to reporting an error.
/// This is the loop described in `docs/status/06-federation.md`: it turns "an event arrived before
/// its history" into "an event arrived, and now so did its history" whenever the gap is small
/// enough and the peer cooperative enough to close within `backfill_limits`.
///
/// EDUs are parsed for structural validity ([`crate::edu::parse_edu`]) and each valid one is
/// handed to `edu_sink` after the PDUs, in order (`None`: they are dropped, as before any sink
/// existed), less what the rooms' server ACLs deny `origin` ([`crate::acl::filter_edu`]: typing
/// and receipts). A malformed EDU is logged and skipped; it never fails the transaction.
///
/// # Errors
/// Returns [`TransactionError::TooManyPdus`]/[`TransactionError::TooManyEdus`] if the transaction
/// exceeds the resource-limits table; never fails for a problem with an individual PDU (including
/// a backfill attempt that gives up), which is reported per-event in the returned map instead.
#[allow(clippy::too_many_arguments)]
pub async fn process_transaction(
    origin: &str,
    txn_id: &str,
    body: &Value,
    rooms: &dyn crate::room_source::RoomDataSource,
    sink: &dyn RoomWriteSink,
    key_cache: &DynRemoteKeyCache,
    transactions: &dyn TransactionStore,
    ancestor_fetcher: Option<&dyn crate::backfill::AncestorFetcher>,
    backfill_limits: &crate::backfill::BackfillLimits,
    edu_sink: Option<&dyn crate::edu::InboundEduSink>,
) -> Result<Value, TransactionError> {
    if let Some(cached) = transactions.get(origin, txn_id).await {
        return Ok(cached);
    }

    let pdus = body
        .get("pdus")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let edus = body
        .get("edus")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if pdus.len() > MAX_PDUS_PER_TRANSACTION {
        return Err(TransactionError::TooManyPdus);
    }
    if edus.len() > MAX_EDUS_PER_TRANSACTION {
        return Err(TransactionError::TooManyEdus);
    }

    let mut results = serde_json::Map::new();
    let mut acl_verdicts: HashMap<String, Result<(), String>> = HashMap::new();
    for pdu in &pdus {
        let fallback_id = pdu
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or("$unknown")
            .to_owned();

        let Some(room_id) = pdu.get("room_id").and_then(Value::as_str) else {
            results.insert(fallback_id, serde_json::json!({"error": "missing room_id"}));
            continue;
        };

        let Some(room_version_str) = rooms.room_version(room_id).await else {
            results.insert(fallback_id, serde_json::json!({"error": "unknown room"}));
            continue;
        };
        let Ok(room_version) = RoomVersionId::try_from(room_version_str.as_str()) else {
            results.insert(
                fallback_id,
                serde_json::json!({"error": "unsupported room version"}),
            );
            continue;
        };

        // The room's server ACL, once per room per transaction: a PDU from a server the room
        // bans is refused before it is even verified, as Synapse does.
        let acl = match acl_verdicts.get(room_id) {
            Some(verdict) => verdict.clone(),
            None => {
                let verdict = crate::acl::check_origin(rooms, room_id, origin).await;
                acl_verdicts.insert(room_id.to_owned(), verdict.clone());
                verdict
            }
        };
        if let Err(message) = acl {
            crate::metrics::record_acl_refusal("send");
            tracing::info!(
                origin,
                room_id,
                "refused a PDU: the room's server ACL denies the sending server"
            );
            let event_id = Event::parse(pdu, room_version.clone())
                .map_or(fallback_id, |event| event.event_id().to_string());
            results.insert(event_id, serde_json::json!({ "error": message }));
            continue;
        }

        let event = match verify_pdu(pdu, &room_version, key_cache).await {
            Ok(event) => event,
            Err(err) => {
                results.insert(fallback_id, serde_json::json!({"error": err.to_string()}));
                continue;
            }
        };
        let event_id = event.event_id().to_string();
        let value = event_json(&event);
        match sink.accept_verified_event(room_id, &event_id, &value).await {
            Ok(_) => {
                results.insert(event_id, serde_json::json!({}));
            }
            // Only auth events are missing: they are fetched one by one (`/event`), as Synapse
            // and Dendrite do, not walked for -- `/get_missing_events` and `/backfill` are for
            // prev events, and a server answering this one need not serve them (Complement's
            // `TestInboundFederationRejectsEventsWithRejectedAuthEvents` fails on either).
            Err(rejected)
                if !rejected.missing_ancestors.is_empty()
                    && ancestor_fetcher.is_some()
                    && only_auth_events_missing(&value, &rejected.missing_ancestors) =>
            {
                let fetcher = ancestor_fetcher.expect("checked Some above");
                match crate::state_fallback::fetch_missing_auth_events(
                    origin,
                    room_id,
                    &room_version,
                    rejected.missing_ancestors.clone(),
                    fetcher,
                    key_cache,
                    sink,
                )
                .await
                {
                    Ok(()) => match sink.accept_verified_event(room_id, &event_id, &value).await {
                        Ok(_) => {
                            results.insert(event_id, serde_json::json!({}));
                        }
                        Err(still_rejected) => {
                            let result = rejection_result(origin, &event_id, &still_rejected);
                            results.insert(event_id, result);
                        }
                    },
                    Err(reason) => {
                        results.insert(
                            event_id,
                            serde_json::json!({
                                "error": format!("{}; fetching them: {reason}", rejected.error)
                            }),
                        );
                    }
                }
            }
            Err(rejected)
                if !rejected.missing_ancestors.is_empty() && ancestor_fetcher.is_some() =>
            {
                let fetcher = ancestor_fetcher.expect("checked Some above");
                // What this server holds, for the gap-shaped request (`crate::backfill`'s
                // module docs): its forward extremities are the oldest end of the gap, this
                // event the newest.
                let context = crate::backfill::GapContext {
                    latest_event_id: Some(event_id.clone()),
                    earliest_events: rooms
                        .forward_extremities(room_id)
                        .await
                        .map(|extremities| extremities.into_iter().map(|(id, _)| id).collect())
                        .unwrap_or_default(),
                };
                let outcome = crate::backfill::resolve_missing_ancestors(
                    origin,
                    room_id,
                    &room_version,
                    rejected.missing_ancestors.clone(),
                    &context,
                    fetcher,
                    key_cache,
                    sink,
                    backfill_limits,
                )
                .await;
                match outcome {
                    Ok(()) => match sink.accept_verified_event(room_id, &event_id, &value).await {
                        Ok(_) => {
                            results.insert(event_id, serde_json::json!({}));
                        }
                        Err(still_rejected) => {
                            let result = rejection_result(origin, &event_id, &still_rejected);
                            results.insert(event_id, result);
                        }
                    },
                    Err(gave_up) => {
                        results.insert(
                            event_id,
                            serde_json::json!({
                                "error": format!(
                                    "{}; backfill attempt to close it {gave_up}",
                                    rejected.error
                                )
                            }),
                        );
                    }
                }
            }
            Err(rejected) => {
                let result = rejection_result(origin, &event_id, &rejected);
                results.insert(event_id, result);
            }
        }
    }

    for edu in &edus {
        match crate::edu::parse_edu(edu) {
            Ok(edu) => {
                if let Some(sink) = edu_sink
                    && let Some(edu) =
                        crate::acl::filter_edu(rooms, origin, edu, &mut acl_verdicts).await
                {
                    sink.receive_edu(origin, edu).await;
                }
            }
            Err(error) => {
                tracing::debug!(origin, %error, "dropping a malformed EDU");
            }
        }
    }

    let response = serde_json::json!({ "pdus": Value::Object(results) });
    transactions.put(origin, txn_id, response.clone()).await;
    Ok(response)
}

/// Whether every one of `missing` is an `auth_events` entry of `event` and none a `prev_events`
/// one.
fn only_auth_events_missing(event: &Value, missing: &[String]) -> bool {
    let ids = |field: &str| -> Vec<String> {
        event
            .get(field)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        entry
                            .as_str()
                            .or_else(|| entry.as_array()?.first()?.as_str())
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let prevs = ids("prev_events");
    let auths = ids("auth_events");
    missing
        .iter()
        .all(|id| auths.contains(id) && !prevs.contains(id))
}

/// What `/send` answers for a PDU the sink did not take: `{}` for one event authorization
/// rejected -- it was received and processed, and the spec and Synapse answer success for it
/// (an error there tells the sender its delivery failed, which it did not) -- and `{"error": ..}`
/// for everything else (a PDU that could not be parsed, placed or stored).
fn rejection_result(origin: &str, event_id: &str, rejected: &WriteRejected) -> Value {
    if rejected.auth_rejected {
        tracing::info!(
            origin,
            event_id,
            reason = %rejected.error,
            "a PDU received over federation was rejected by event authorization"
        );
        serde_json::json!({})
    } else {
        serde_json::json!({ "error": rejected.error })
    }
}

/// Why [`process_transaction`] refused a whole transaction outright (as opposed to one PDU within
/// it, which is reported per-event instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionError {
    TooManyPdus,
    TooManyEdus,
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyPdus => write!(
                f,
                "too many pdus in one transaction (max {MAX_PDUS_PER_TRANSACTION})"
            ),
            Self::TooManyEdus => write!(
                f,
                "too many edus in one transaction (max {MAX_EDUS_PER_TRANSACTION})"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{
        KeyServerFetcher, OwnSigningKeys, RemoteKeyCache, build_server_key_response,
    };
    use crate::room_source::{FakeRoom, InMemoryRoomSource};
    use hs_model::signing::sign_object;

    struct FixedFetcher(Value);
    #[async_trait]
    impl KeyServerFetcher for FixedFetcher {
        async fn fetch_server_key(&self, _server_name: &str) -> Option<Value> {
            Some(self.0.clone())
        }
    }

    fn signed_event(keys: &OwnSigningKeys, room_id: &str, sender: &str) -> Value {
        let mut object = hs_model::canonical::to_canonical_object(
            &serde_json::json!({
                "type": "m.room.message",
                "room_id": room_id,
                "sender": sender,
                "origin_server_ts": 1,
                "depth": 2,
                "content": {"body": "hi"},
                "prev_events": [],
                "auth_events": [],
            }),
            true,
        )
        .unwrap();
        let hash = hs_model::hash::content_hash_base64(&object);
        object.insert(
            "hashes".to_owned(),
            hs_model::canonical::CanonicalJsonValue::Object(
                [(
                    "sha256".to_owned(),
                    hs_model::canonical::CanonicalJsonValue::String(hash),
                )]
                .into_iter()
                .collect(),
            ),
        );
        let server = ruma::ServerName::parse(sender.split_once(':').unwrap().1).unwrap();
        // Per the spec's real signing order (hash, then redact, then sign the *redacted* object,
        // then copy the signature back onto the full one) -- matches `verify_pdu`'s equally real
        // verification order below. Signing the unredacted object directly (as a naive test would)
        // produces a signature `verify_pdu` correctly rejects whenever `content` carries anything
        // redaction would strip, which for `m.room.message` is everything.
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
        let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
        sign_object(&mut redacted, &server, keys.primary()).unwrap();
        object.insert(
            "signatures".to_owned(),
            redacted.remove("signatures").unwrap(),
        );
        serde_json::from_slice(
            &hs_model::canonical::CanonicalJsonValue::Object(object).to_canonical_bytes(),
        )
        .unwrap()
    }

    fn key_cache(keys: &OwnSigningKeys, origin: &str) -> DynRemoteKeyCache {
        let doc = build_server_key_response(origin, keys, &[], 3600).unwrap();
        RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>)
    }

    fn room_source(room_id: &str) -> InMemoryRoomSource {
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            room_id,
            FakeRoom {
                room_version: Some("11".to_owned()),
                ..FakeRoom::default()
            },
        );
        rooms
    }

    #[tokio::test]
    async fn verify_pdu_accepts_a_correctly_signed_event() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");

        let event = verify_pdu(&raw, &RoomVersionId::V11, &cache).await.unwrap();
        assert_eq!(event.header().event_type, "m.room.message");
    }

    /// A body changed after signing fails the content hash, and the event is taken in its
    /// redacted form -- the spec's "if the hash check fails, the event is redacted before
    /// processing further" -- keeping its ID (the signatures and the reference hash cover the
    /// redacted form). It was refused until 2026-10-01 (Sytest's "Inbound federation can receive
    /// redacted events"). That the signature check itself bites is
    /// `verify_pdu_rejects_a_tampered_signature_with_hash_intact`.
    #[tokio::test]
    async fn verify_pdu_takes_an_event_whose_hash_fails_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let mut raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");
        let original_id = Event::parse(&raw, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_owned();
        raw["content"]["body"] = serde_json::json!("tampered");

        let event = verify_pdu(&raw, &RoomVersionId::V11, &cache).await.unwrap();
        assert_eq!(event.event_id(), &*original_id);
        assert_eq!(
            event.json().get("content"),
            Some(&CanonicalJsonValue::Object(Default::default())),
            "taken redacted: a message keeps no content"
        );

        // Changed *and* re-signed by nobody: a stripped signature still fails.
        raw["signatures"] = serde_json::json!({});
        assert!(verify_pdu(&raw, &RoomVersionId::V11, &cache).await.is_err());
    }

    /// Isolates the signature check from the content-hash check (unlike
    /// `verify_pdu_rejects_a_tampered_body`, which tampers the content and is therefore caught by
    /// the *hash* check regardless of whether signature verification runs at all): the hash still
    /// matches, only the signature bytes are corrupted. This is the unit-level half of the
    /// mutation test recorded in the status file -- confirmed to fail when `verify_pdu`'s call to
    /// `signing::verify_object` is short-circuited to always succeed.
    #[tokio::test]
    async fn verify_pdu_rejects_a_tampered_signature_with_hash_intact() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let mut raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");

        let sig_obj = raw["signatures"]["origin.example.org"].as_object().unwrap();
        let (key_id, sig) = sig_obj.iter().next().unwrap();
        let key_id = key_id.clone();
        let mut sig = sig.as_str().unwrap().to_owned();
        let last = sig.pop().unwrap();
        sig.push(if last == 'A' { 'B' } else { 'A' });
        raw["signatures"]["origin.example.org"][key_id] = Value::String(sig);

        let err = verify_pdu(&raw, &RoomVersionId::V11, &cache)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("verify"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn verify_pdu_rejects_a_signature_from_the_wrong_key() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let other_keys = OwnSigningKeys::load_or_generate(dir2.path()).unwrap();
        // The cache trusts `keys`, but the event is signed with `other_keys`.
        let cache = key_cache(&keys, "origin.example.org");
        let raw = signed_event(
            &other_keys,
            "!r:origin.example.org",
            "@alice:origin.example.org",
        );

        let err = verify_pdu(&raw, &RoomVersionId::V11, &cache)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("verify") || err.to_string().contains("key"));
    }

    #[tokio::test]
    async fn transaction_over_the_pdu_limit_is_rejected() {
        let rooms = room_source("!r:origin.example.org");
        let sink = StaticWriteSink::new(Vec::new(), "not supported");
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let store = InMemoryTransactionStore::new();

        let too_many: Vec<Value> = (0..51).map(|_| serde_json::json!({})).collect();
        let body = serde_json::json!({"pdus": too_many, "edus": []});
        let err = process_transaction(
            "origin.example.org",
            "txn1",
            &body,
            &rooms,
            &sink,
            &cache,
            &store,
            None,
            &crate::backfill::BackfillLimits::default(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err, TransactionError::TooManyPdus);
    }

    #[tokio::test]
    async fn already_known_event_is_accepted_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");
        let event = Event::parse(&raw, RoomVersionId::V11).unwrap();

        let rooms = room_source("!r:origin.example.org");
        let sink = StaticWriteSink::new(vec![event.event_id().to_string()], "not supported");
        let store = InMemoryTransactionStore::new();

        let body = serde_json::json!({"pdus": [raw], "edus": []});
        let response = process_transaction(
            "origin.example.org",
            "txn1",
            &body,
            &rooms,
            &sink,
            &cache,
            &store,
            None,
            &crate::backfill::BackfillLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            response["pdus"][event.event_id().as_str()],
            serde_json::json!({})
        );
    }

    #[tokio::test]
    async fn an_unrecognized_new_event_is_rejected_per_event_not_the_whole_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");

        let rooms = room_source("!r:origin.example.org");
        let sink = StaticWriteSink::new(Vec::new(), "cannot yet persist this");
        let store = InMemoryTransactionStore::new();

        let body = serde_json::json!({"pdus": [raw], "edus": []});
        let response = process_transaction(
            "origin.example.org",
            "txn1",
            &body,
            &rooms,
            &sink,
            &cache,
            &store,
            None,
            &crate::backfill::BackfillLimits::default(),
            None,
        )
        .await
        .unwrap();
        let pdus = response["pdus"].as_object().unwrap();
        assert_eq!(pdus.len(), 1);
        let (_, result) = pdus.iter().next().unwrap();
        assert!(result.get("error").is_some());
    }

    /// A PDU event authorization rejects was received and processed: `/send` answers `{}` for
    /// it, as Synapse does and Sytest expects ("Unexpected response from /send" in ten tests
    /// before), while a PDU the sink could not take for any other reason is still an error.
    #[tokio::test]
    async fn a_pdu_rejected_by_auth_is_answered_with_an_empty_result() {
        struct Refusing(bool);
        #[async_trait]
        impl RoomWriteSink for Refusing {
            async fn accept_verified_event(
                &self,
                _room_id: &str,
                _event_id: &str,
                _event_json: &Value,
            ) -> Result<WriteOutcome, WriteRejected> {
                Err(if self.0 {
                    WriteRejected::auth("event rejected: not allowed")
                } else {
                    WriteRejected::other("the store is down")
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");
        let event_id = Event::parse(&raw, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();
        let rooms = room_source("!r:origin.example.org");
        let body = serde_json::json!({"pdus": [raw], "edus": []});
        for (auth, expected_error) in [(true, false), (false, true)] {
            let response = process_transaction(
                "origin.example.org",
                &format!("txn-{auth}"),
                &body,
                &rooms,
                &Refusing(auth),
                &cache,
                &InMemoryTransactionStore::new(),
                None,
                &crate::backfill::BackfillLimits::default(),
                None,
            )
            .await
            .unwrap();
            let result = &response["pdus"][event_id.as_str()];
            assert_eq!(result.get("error").is_some(), expected_error, "{response}");
            if auth {
                assert_eq!(*result, serde_json::json!({}));
            }
        }
    }

    /// A PDU in a room whose server ACL denies the transaction's origin is refused with an
    /// error, by event ID, and not handed to the sink at all.
    #[tokio::test]
    async fn a_pdu_from_a_server_the_room_acl_denies_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");
        let event_id = Event::parse(&raw, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            "!r:origin.example.org",
            FakeRoom {
                room_version: Some("11".to_owned()),
                state: vec![serde_json::json!({
                    "event_id": "$acl", "type": "m.room.server_acl", "state_key": "",
                    "room_id": "!r:origin.example.org", "sender": "@admin:us.example.org",
                    "content": {"allow": ["*"], "deny": ["origin.example.org"]},
                })],
                ..FakeRoom::default()
            },
        );
        // A sink that would accept it: the refusal has to come before it.
        let sink = StaticWriteSink::new(vec![event_id.clone()], "n/a");
        let before = crate::metrics::acl_refusals("send");
        let response = process_transaction(
            "origin.example.org",
            "txn-acl",
            &serde_json::json!({"pdus": [raw], "edus": []}),
            &rooms,
            &sink,
            &cache,
            &InMemoryTransactionStore::new(),
            None,
            &crate::backfill::BackfillLimits::default(),
            None,
        )
        .await
        .unwrap();
        let error = response["pdus"][event_id.as_str()]["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{response}"));
        assert!(error.contains("server ACL"), "{error}");
        assert!(crate::metrics::acl_refusals("send") > before);
    }

    /// **Mutation test 2** (see the status file): replaying the same `(origin, txn_id)` must not
    /// reprocess -- proven here by a sink that errors the *second* time it is called, which the
    /// idempotency cache must never reach.
    #[tokio::test]
    async fn replaying_a_transaction_id_does_not_reprocess() {
        struct FailOnSecondCall(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl RoomWriteSink for FailOnSecondCall {
            async fn accept_verified_event(
                &self,
                _room_id: &str,
                _event_id: &str,
                _event_json: &Value,
            ) -> Result<WriteOutcome, WriteRejected> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Ok(WriteOutcome::Stored)
                } else {
                    panic!("sink called a second time -- the transaction was reprocessed");
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");

        let rooms = room_source("!r:origin.example.org");
        let sink = FailOnSecondCall(std::sync::atomic::AtomicUsize::new(0));
        let store = InMemoryTransactionStore::new();

        let body = serde_json::json!({"pdus": [raw], "edus": []});
        let first = process_transaction(
            "origin.example.org",
            "txn-replay",
            &body,
            &rooms,
            &sink,
            &cache,
            &store,
            None,
            &crate::backfill::BackfillLimits::default(),
            None,
        )
        .await
        .unwrap();
        let second = process_transaction(
            "origin.example.org",
            "txn-replay",
            &body,
            &rooms,
            &sink,
            &cache,
            &store,
            None,
            &crate::backfill::BackfillLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(first, second);
    }

    /// Every well-formed EDU reaches the sink with the transaction's origin, after the PDUs; a
    /// malformed one is skipped without failing the transaction; a replayed transaction does
    /// not deliver them again.
    #[tokio::test]
    async fn edus_are_handed_to_the_sink_with_their_origin_once() {
        #[derive(Default)]
        struct Recording(Mutex<Vec<(String, crate::edu::Edu)>>);
        #[async_trait]
        impl crate::edu::InboundEduSink for Recording {
            async fn receive_edu(&self, origin: &str, edu: crate::edu::Edu) {
                self.0.lock().unwrap().push((origin.to_owned(), edu));
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let rooms = room_source("!r:origin.example.org");
        let sink = StaticWriteSink::new(Vec::new(), "not supported");
        let store = InMemoryTransactionStore::new();
        let edus = Recording::default();
        let body = serde_json::json!({"pdus": [], "edus": [
            {"edu_type": "m.typing", "content": {"room_id": "!r:origin.example.org",
                "user_id": "@alice:origin.example.org", "typing": true}},
            "not an edu",
            {"edu_type": "m.presence", "content": {"push": []}},
        ]});
        for _ in 0..2 {
            process_transaction(
                "origin.example.org",
                "txn-edus",
                &body,
                &rooms,
                &sink,
                &cache,
                &store,
                None,
                &crate::backfill::BackfillLimits::default(),
                Some(&edus),
            )
            .await
            .unwrap();
        }
        let received = edus.0.lock().unwrap().clone();
        let types: Vec<(&str, &str)> = received
            .iter()
            .map(|(origin, edu)| (origin.as_str(), edu.edu_type.as_str()))
            .collect();
        assert_eq!(
            types,
            vec![
                ("origin.example.org", "m.typing"),
                ("origin.example.org", "m.presence"),
            ]
        );
    }

    /// A room's server ACL applies to the EDUs that name it (MSC4163, as Synapse): a typing
    /// notice for a room that denies the sending server is dropped, and so is that room's part
    /// of a receipt EDU, while another room's receipts and an EDU naming no room pass. Both
    /// reached the sink until 2026-10-01.
    #[tokio::test]
    async fn typing_and_receipts_for_a_room_whose_acl_denies_the_origin_are_dropped() {
        #[derive(Default)]
        struct Recording(Mutex<Vec<crate::edu::Edu>>);
        #[async_trait]
        impl crate::edu::InboundEduSink for Recording {
            async fn receive_edu(&self, _origin: &str, edu: crate::edu::Edu) {
                self.0.lock().unwrap().push(edu);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            "!banned:us.example.org",
            FakeRoom {
                room_version: Some("11".to_owned()),
                state: vec![serde_json::json!({
                    "event_id": "$acl", "type": "m.room.server_acl", "state_key": "",
                    "room_id": "!banned:us.example.org", "sender": "@admin:us.example.org",
                    "content": {"allow": ["*"], "deny": ["origin.example.org"]},
                })],
                ..FakeRoom::default()
            },
        );
        rooms.insert_room(
            "!open:us.example.org",
            FakeRoom {
                room_version: Some("11".to_owned()),
                ..FakeRoom::default()
            },
        );
        let receipt = |room: &str| {
            serde_json::json!({"m.read": {"@alice:origin.example.org": {
                "event_ids": ["$e"], "data": {"ts": 1}}}})
            .as_object()
            .map(|r| (room.to_owned(), serde_json::Value::Object(r.clone())))
            .unwrap()
        };
        let receipts: serde_json::Map<String, Value> = [
            receipt("!banned:us.example.org"),
            receipt("!open:us.example.org"),
        ]
        .into_iter()
        .collect();
        let body = serde_json::json!({"pdus": [], "edus": [
            {"edu_type": "m.typing", "content": {"room_id": "!banned:us.example.org",
                "user_id": "@alice:origin.example.org", "typing": true}},
            {"edu_type": "m.typing", "content": {"room_id": "!open:us.example.org",
                "user_id": "@alice:origin.example.org", "typing": true}},
            {"edu_type": "m.receipt", "content": receipts},
            {"edu_type": "m.receipt", "content": {"!banned:us.example.org":
                receipt("x").1}},
            {"edu_type": "m.presence", "content": {"push": []}},
        ]});
        let typing_before = crate::metrics::acl_refusals("typing");
        let receipts_before = crate::metrics::acl_refusals("receipt");
        let edus = Recording::default();
        process_transaction(
            "origin.example.org",
            "txn-edu-acl",
            &body,
            &rooms,
            &StaticWriteSink::new(Vec::new(), "not supported"),
            &cache,
            &InMemoryTransactionStore::new(),
            None,
            &crate::backfill::BackfillLimits::default(),
            Some(&edus),
        )
        .await
        .unwrap();
        let received = edus.0.lock().unwrap().clone();
        let summary: Vec<(String, Value)> = received
            .iter()
            .map(|edu| {
                let what = match edu.edu_type.as_str() {
                    "m.typing" => edu.content["room_id"].clone(),
                    "m.receipt" => serde_json::json!(
                        edu.content.as_object().unwrap().keys().collect::<Vec<_>>()
                    ),
                    _ => Value::Null,
                };
                (edu.edu_type.clone(), what)
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "m.typing".to_owned(),
                    serde_json::json!("!open:us.example.org")
                ),
                (
                    "m.receipt".to_owned(),
                    serde_json::json!(["!open:us.example.org"])
                ),
                ("m.presence".to_owned(), Value::Null),
            ]
        );
        assert!(crate::metrics::acl_refusals("typing") > typing_before);
        assert!(crate::metrics::acl_refusals("receipt") >= receipts_before + 2);
    }
}
