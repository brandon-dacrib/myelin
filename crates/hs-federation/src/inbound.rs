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
pub struct PduError(pub String);

impl std::fmt::Display for PduError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PduError {}

fn reject(message: impl Into<String>) -> PduError {
    PduError(message.into())
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

/// Parses, hash-checks and signature-verifies one PDU against the room version it claims. On
/// success, returns the parsed [`Event`] -- callers still owe it an authorization check
/// (`crate::join` does this for `send_join`; `/send`'s own PDUs are not authorized against room
/// state this session, see the module doc) before treating it as accepted.
///
/// # Errors
/// Returns [`PduError`] if the PDU is malformed, oversized, has a content hash that does not match
/// its declared one, is not signed by its sender's server, or that server's key cannot be resolved
/// or does not verify.
pub async fn verify_pdu(
    raw: &Value,
    room_version: &RoomVersionId,
    key_cache: &DynRemoteKeyCache,
) -> Result<Event, PduError> {
    let event = Event::parse(raw, room_version.clone())
        .map_err(|e| reject(format!("malformed event: {e}")))?;

    if !content_hash_matches(&event) {
        return Err(reject(
            "content hash does not match the event's declared hashes.sha256",
        ));
    }

    let sender_server = event.header().sender.server_name().as_str();
    let key_id = signature_key_id(event.json(), sender_server)
        .ok_or_else(|| reject(format!("no signature from sender's server {sender_server}")))?;

    let signed_at = u64::try_from(event.header().origin_server_ts).unwrap_or(0);
    let verifying_key = key_cache
        .get_valid_at(sender_server, &key_id, signed_at)
        .await
        .map_err(|e| {
            reject(format!(
                "key lookup for {sender_server}/{key_id} failed: {}",
                key_lookup_reason(&e)
            ))
        })?;

    hs_model::signing::verify_object(event.json(), sender_server, &key_id, &verifying_key)
        .map_err(|_| {
            reject(format!(
                "signature from {sender_server}/{key_id} does not verify"
            ))
        })?;

    Ok(event)
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
        Err(WriteRejected {
            error: self.reject_message.clone(),
        })
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
/// EDUs are parsed for structural validity ([`crate::edu::parse_edu`]) and otherwise ignored: no
/// EDU handler (presence, typing, receipts, device lists, to-device, signing-key updates) exists
/// yet.
///
/// # Errors
/// Returns [`TransactionError::TooManyPdus`]/[`TransactionError::TooManyEdus`] if the transaction
/// exceeds the resource-limits table; never fails for a problem with an individual PDU, which is
/// reported per-event in the returned map instead.
pub async fn process_transaction(
    origin: &str,
    txn_id: &str,
    body: &Value,
    rooms: &dyn crate::room_source::RoomDataSource,
    sink: &dyn RoomWriteSink,
    key_cache: &DynRemoteKeyCache,
    transactions: &dyn TransactionStore,
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
            Err(rejected) => {
                results.insert(event_id, serde_json::json!({"error": rejected.error}));
            }
        }
    }

    for edu in &edus {
        // Structural validation only -- see the module and function docs.
        let _ = crate::edu::parse_edu(edu);
    }

    let response = serde_json::json!({ "pdus": Value::Object(results) });
    transactions.put(origin, txn_id, response.clone()).await;
    Ok(response)
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
        sign_object(&mut object, &server, keys.primary()).unwrap();
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

    /// **Mutation test 1** (see the status file): a tampered body must fail verification. If this
    /// ever passes, `verify_pdu`'s signature check is not doing anything.
    #[tokio::test]
    async fn verify_pdu_rejects_a_tampered_body() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let mut raw = signed_event(&keys, "!r:origin.example.org", "@alice:origin.example.org");
        raw["content"]["body"] = serde_json::json!("tampered");

        let err = verify_pdu(&raw, &RoomVersionId::V11, &cache)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("hash"));
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
        )
        .await
        .unwrap();
        let pdus = response["pdus"].as_object().unwrap();
        assert_eq!(pdus.len(), 1);
        let (_, result) = pdus.iter().next().unwrap();
        assert!(result.get("error").is_some());
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
        )
        .await
        .unwrap();
        assert_eq!(first, second);
    }
}
