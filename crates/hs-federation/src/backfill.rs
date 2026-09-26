//! Resolving `RoomError::MissingAncestors`: when an inbound event cites a `prev_events`/
//! `auth_events` entry this server does not hold, fetch the gap from the server that told us
//! about the event, verify each fetched event the same way any inbound PDU is verified, persist
//! them in dependency order, then let the caller retry the event that triggered this. See
//! `crate::inbound::process_transaction` for the one caller today.
//!
//! # Two requests for the same gap
//!
//! The first attempt is the gap-shaped one, `POST /get_missing_events/{roomId}`: "here are my
//! forward extremities (`earliest_events`), here is the event you just sent me
//! (`latest_events`), give me what lies between" -- which is what a server that has just
//! received an event with unknown ancestors is meant to ask, and the one request Complement's
//! reference federation server answers for this purpose (`TestGetMissingEventsGapFilling`:
//! it checks the two lists name exactly those events and serves the missing ones; it has no
//! `/backfill` handler at all, so a resolver that only knows `/backfill` never closes the gap).
//! When that closes the gap, nothing else is asked. When it does not -- the gap is deeper than
//! one response, the peer does not answer that endpoint, or the caller could not say what it
//! holds ([`GapContext`] empty) -- the rounds of `GET /backfill/{roomId}?v=<missing>` below
//! walk the rest, under the same limits.
//!
//! # What this defends against
//!
//! A hostile or merely broken federation peer can dangle an unbounded ancestor chain: claim event
//! E's parent is missing, hand over a parent whose own parent is *also* missing, forever.
//! [`BackfillLimits`] bounds every dimension of that:
//!
//! - [`BackfillLimits::max_events_per_fetch`] bounds how many events one HTTP round-trip may
//!   contribute, regardless of what the peer's response actually contains (a peer that ignores
//!   the `limit` it was asked for and sends more is still cut off here, not trusted).
//! - [`BackfillLimits::max_rounds`] bounds how many round-trips one resolution attempt may make:
//!   this is the recursion-depth bound, since each round can surface a new, deeper gap.
//! - [`BackfillLimits::max_total_events`] bounds the total number of signature verifications (an
//!   asymmetric-crypto operation) one hostile transaction can force, independent of how few or
//!   many rounds it took to reach that count.
//! - [`BackfillLimits::max_duration`] bounds wall-clock time end to end, so a slow or stalling
//!   peer cannot hold a task open indefinitely.
//!
//! Hitting any limit ends the attempt with a [`BackfillGiveUpReason`] -- cleanly, not by
//! continuing to loop -- and the triggering event is reported exactly as it would have been with
//! no backfill at all: a per-event error, never a fatal transaction failure. See
//! `docs/status/06-federation.md` for the exact numbers chosen, the mutation test that confirms
//! [`BackfillLimits::max_rounds`] is load-bearing, and a precise statement of what an attacker can
//! and cannot cost this server through this path.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use ruma::RoomVersionId;
use serde_json::Value;

use crate::client::{ClientError, FederationClient};
use crate::inbound::{RoomWriteSink, event_json, verify_pdu};
use crate::keys::DynRemoteKeyCache;

/// Bounds applied to one [`resolve_missing_ancestors`] attempt. Deliberately conservative: a
/// legitimate gap after a real join is normally a handful of events at most (the common case is
/// exactly one -- an event that raced its own predecessor over the wire), so these numbers cost a
/// well-behaved peer nothing and cost a hostile one very little.
#[derive(Debug, Clone, Copy)]
pub struct BackfillLimits {
    /// The `limit` query parameter sent on each `/backfill` request, and the most events accepted
    /// from a single response regardless of what the peer actually sends. Matches
    /// `crate::transport::read_routes::MAX_BACKFILL_LIMIT`, this server's own server-side clamp on
    /// the same endpoint, so this server never asks a peer for more than it would itself agree to
    /// answer.
    pub max_events_per_fetch: usize,
    /// The most `/backfill` round-trips one resolution attempt will make to the same peer. Each
    /// round can surface a *new*, deeper missing ancestor (a fetched event's own `prev_events` may
    /// themselves be missing), so this is the recursion-depth bound: a chain deeper than this is
    /// given up on, not chased further.
    pub max_rounds: usize,
    /// The total number of distinct events this attempt will fetch and verify across every round,
    /// regardless of how few rounds that takes.
    pub max_total_events: usize,
    /// The wall-clock ceiling for the whole attempt, across every round-trip.
    pub max_duration: Duration,
}

/// Conservative production defaults. See each field's doc for the reasoning; see this crate's
/// status file for the mutation test that confirms [`BackfillLimits::max_rounds`] is load-bearing.
impl Default for BackfillLimits {
    fn default() -> Self {
        Self {
            max_events_per_fetch: 100,
            max_rounds: 10,
            max_total_events: 500,
            max_duration: Duration::from_secs(20),
        }
    }
}

/// Why one HTTP round-trip to fetch ancestor events failed.
#[derive(Debug, Clone)]
pub struct AncestorFetchError(pub String);

impl std::fmt::Display for AncestorFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AncestorFetchError {}

/// What the caller knows about the gap beyond the missing IDs themselves: the event whose
/// arrival exposed it, and the forward extremities this server holds in the room. Both are what
/// `POST /get_missing_events` asks for (`latest_events` and `earliest_events`); with neither
/// (`Default`), [`resolve_missing_ancestors`] goes straight to `/backfill`.
#[derive(Debug, Clone, Default)]
pub struct GapContext {
    /// The verified event that cited the missing ancestors: the newest end of the gap, which the
    /// remote holds (it sent it).
    pub latest_event_id: Option<String>,
    /// This server's current forward extremities in the room: the oldest end of the gap, which
    /// the remote is asked to walk back to and not past.
    pub earliest_events: Vec<String>,
}

/// The outbound half of backfill: fetches events from a remote server. [`FederationClient`]
/// implements this over the real `/backfill` endpoint (reusing its signed-request machinery, not
/// a second X-Matrix client); tests can supply a fake to control exactly what a "remote" hands
/// back, including a hostile one that never converges.
#[async_trait]
pub trait AncestorFetcher: Send + Sync {
    /// `GET /backfill/{roomId}?v=<from_event_ids>&limit=<limit>` against `destination`: up to
    /// `limit` raw (unverified) PDU JSON values, walking backwards from `from_event_ids`.
    /// Verifying them is the caller's job ([`resolve_missing_ancestors`] does this) -- an
    /// implementation must not be trusted to have checked anything about what it returns.
    async fn fetch_backfill(
        &self,
        destination: &str,
        room_id: &str,
        from_event_ids: &[String],
        limit: usize,
    ) -> Result<Vec<Value>, AncestorFetchError>;

    /// `POST /get_missing_events/{roomId}` against `destination`: up to `limit` raw (unverified)
    /// PDU JSON values on the paths from `latest_events` back to, not including,
    /// `earliest_events`, oldest first, none with a `depth` below `min_depth`. The same trust
    /// contract as [`AncestorFetcher::fetch_backfill`]: nothing returned has been checked.
    ///
    /// The default answers an error, which [`resolve_missing_ancestors`] treats as "ask
    /// `/backfill` instead": a fetcher that only knows `/backfill` (this crate's own test
    /// fetchers) keeps working as it did.
    async fn fetch_missing_events(
        &self,
        destination: &str,
        room_id: &str,
        earliest_events: &[String],
        latest_events: &[String],
        limit: usize,
        min_depth: i64,
    ) -> Result<Vec<Value>, AncestorFetchError> {
        let _ = (
            destination,
            room_id,
            earliest_events,
            latest_events,
            limit,
            min_depth,
        );
        Err(AncestorFetchError(
            "this fetcher does not implement /get_missing_events".to_owned(),
        ))
    }
}

#[async_trait]
impl AncestorFetcher for FederationClient {
    async fn fetch_backfill(
        &self,
        destination: &str,
        room_id: &str,
        from_event_ids: &[String],
        limit: usize,
    ) -> Result<Vec<Value>, AncestorFetchError> {
        self.backfill(destination, room_id, from_event_ids, limit)
            .await
            .map_err(|e: ClientError| AncestorFetchError(e.to_string()))
    }

    async fn fetch_missing_events(
        &self,
        destination: &str,
        room_id: &str,
        earliest_events: &[String],
        latest_events: &[String],
        limit: usize,
        min_depth: i64,
    ) -> Result<Vec<Value>, AncestorFetchError> {
        self.get_missing_events(
            destination,
            room_id,
            earliest_events,
            latest_events,
            limit,
            min_depth,
        )
        .await
        .map_err(|e: ClientError| AncestorFetchError(e.to_string()))
    }
}

/// Why [`resolve_missing_ancestors`] gave up before closing the gap. Every variant means the
/// triggering event stays unresolved -- reported as an ordinary per-event `/send` error, exactly
/// as it would be with no backfill at all, never a crash or an indefinite hang.
#[derive(Debug, Clone)]
pub enum BackfillGiveUpReason {
    /// [`BackfillLimits::max_rounds`] round-trips happened and the gap still was not closed.
    TooManyRounds,
    /// [`BackfillLimits::max_total_events`] events were fetched and the gap still was not closed.
    TooManyEvents,
    /// [`BackfillLimits::max_duration`] elapsed before the gap was closed.
    TimedOut,
    /// The remote could not be reached, or answered with something other than a usable response.
    RemoteUnavailable(String),
    /// The remote answered but did not supply anything that makes further progress: an empty
    /// response, or a response containing only events already seen in an earlier round of this
    /// same attempt.
    StillMissing(Vec<String>),
}

impl std::fmt::Display for BackfillGiveUpReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyRounds => write!(f, "gave up after too many backfill round-trips"),
            Self::TooManyEvents => write!(f, "gave up after fetching too many backfill events"),
            Self::TimedOut => write!(f, "timed out"),
            Self::RemoteUnavailable(e) => write!(f, "could not fetch ancestor events: {e}"),
            Self::StillMissing(ids) => write!(
                f,
                "remote did not supply {} missing ancestor(s): {}",
                ids.len(),
                ids.join(", ")
            ),
        }
    }
}

/// Closes a `RoomError::MissingAncestors` gap: fetches the missing events (and, recursively,
/// whatever *their* `prev_events`/`auth_events` turn out to need) from `origin`, verifies each one
/// the same way [`crate::inbound::verify_pdu`] verifies any inbound PDU, and persists them through
/// `sink` in dependency order (shallowest `depth` first). Does **not** retry the event that
/// originally reported the gap -- that is the caller's job once this returns `Ok(())`.
///
/// The first request is the gap-shaped `/get_missing_events` when `context` can describe the
/// gap (see the module docs); the `/backfill` rounds follow only if that did not close it.
///
/// # Errors
/// Returns [`BackfillGiveUpReason`] if any limit in `limits` is hit, the remote could not be
/// reached, or the remote's response does not close the gap. Never blocks past
/// [`BackfillLimits::max_duration`] wall-clock time, enforced by wrapping the whole attempt in
/// [`tokio::time::timeout`].
#[allow(clippy::too_many_arguments)]
pub async fn resolve_missing_ancestors(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    missing: Vec<String>,
    context: &GapContext,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
    limits: &BackfillLimits,
) -> Result<(), BackfillGiveUpReason> {
    tokio::time::timeout(
        limits.max_duration,
        resolve_inner(
            origin,
            room_id,
            room_version,
            missing,
            context,
            fetcher,
            key_cache,
            sink,
            limits,
        ),
    )
    .await
    .unwrap_or(Err(BackfillGiveUpReason::TimedOut))
}

/// The bookkeeping one resolution attempt carries across its rounds, whichever endpoint a round
/// asked.
struct Attempt {
    /// Every event ID this attempt has already turned into a verified `Event`, across every
    /// round -- so a peer that keeps re-sending the same event (whether by mistake or to burn
    /// cycles) does not get re-verified or re-counted against `max_total_events` a second time.
    already_fetched: HashSet<String>,
    fetched_total: usize,
    /// Verified events fetched in *any* round that could not yet be persisted (their own
    /// ancestors were still missing at the time). Carried across rounds so that when a later
    /// fetch closes what *they* were waiting on, they get retried in the same round that
    /// unblocks them rather than being abandoned the moment their first attempt fails.
    pending: Vec<hs_model::Event>,
}

impl Attempt {
    /// Verifies one response's events into `pending`, within `limits`. Returns how many were new
    /// to this attempt.
    async fn absorb(
        &mut self,
        fetched: &[Value],
        room_version: &RoomVersionId,
        key_cache: &DynRemoteKeyCache,
        limits: &BackfillLimits,
    ) -> usize {
        let mut new_this_round = 0usize;
        for raw in fetched.iter().take(limits.max_events_per_fetch) {
            let Ok(event) = verify_pdu(raw, room_version, key_cache).await else {
                continue;
            };
            if self.already_fetched.insert(event.event_id().to_string()) {
                self.fetched_total += 1;
                new_this_round += 1;
                self.pending.push(event);
                if self.fetched_total >= limits.max_total_events {
                    break;
                }
            }
        }
        new_this_round
    }

    /// Persists every pending event whose ancestors are now held, ancestors first, and says what
    /// is still missing.
    async fn persist(&mut self, room_id: &str, sink: &dyn RoomWriteSink) -> Persisted {
        // Retry every event still waiting on something, old and new together, ancestors first: a
        // lower `depth` cannot depend on a higher one in an acyclic DAG, so one ascending pass
        // gives an event fetched several rounds ago its chance to succeed in the very round that
        // finally supplies what it was missing.
        self.pending.sort_by_key(|e| e.header().depth);

        let mut still_pending = Vec::new();
        let mut next_frontier: Vec<String> = Vec::new();
        for event in self.pending.drain(..) {
            let event_id = event.event_id().to_string();
            let value = event_json(&event);
            match sink.accept_verified_event(room_id, &event_id, &value).await {
                Ok(_) => {}
                Err(rejected) if !rejected.missing_ancestors.is_empty() => {
                    for id in &rejected.missing_ancestors {
                        if !next_frontier.contains(id) {
                            next_frontier.push(id.clone());
                        }
                    }
                    still_pending.push(event);
                }
                Err(_) => {
                    // A hard rejection (bad auth, malformed, ...): this branch of history cannot
                    // be persisted regardless of how much further back this resolves. Drop it
                    // rather than count it as progress or chase it further -- the caller's
                    // eventual retry of the original event reports whatever ancestor is still
                    // actually missing once this attempt returns.
                }
            }
        }
        self.pending = still_pending;

        if self.pending.is_empty() {
            Persisted::Closed
        } else if next_frontier.is_empty() {
            // Everything still pending was hard-dropped by other branches above, or otherwise
            // reported no forwarding address -- there is nothing left this loop can usefully ask
            // for next.
            Persisted::NoForwardingAddress(
                self.pending
                    .iter()
                    .map(|e| e.event_id().to_string())
                    .collect(),
            )
        } else {
            Persisted::Continue(next_frontier)
        }
    }
}

/// What one [`Attempt::persist`] pass left.
enum Persisted {
    /// Nothing is pending: the gap is closed.
    Closed,
    /// Events are still pending, and these are the ancestors they are waiting for.
    Continue(Vec<String>),
    /// Events are still pending and none of them named what it is waiting for.
    NoForwardingAddress(Vec<String>),
}

#[allow(clippy::too_many_arguments)]
async fn resolve_inner(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    mut frontier: Vec<String>,
    context: &GapContext,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
    limits: &BackfillLimits,
) -> Result<(), BackfillGiveUpReason> {
    let mut attempt = Attempt {
        already_fetched: HashSet::new(),
        fetched_total: 0,
        pending: Vec::new(),
    };

    // The gap-shaped request first (module docs). Not a round against `max_rounds` -- it is the
    // request that should make the rounds unnecessary -- but every event it yields counts
    // against `max_total_events` like any other, and its failure is a reason to ask the other
    // way, not to give up.
    if let Some(latest) = &context.latest_event_id {
        let request_limit = limits.max_events_per_fetch.min(limits.max_total_events);
        match fetcher
            .fetch_missing_events(
                origin,
                room_id,
                &context.earliest_events,
                std::slice::from_ref(latest),
                request_limit,
                0,
            )
            .await
        {
            Ok(fetched) if !fetched.is_empty() => {
                let new = attempt
                    .absorb(&fetched, room_version, key_cache, limits)
                    .await;
                if new > 0 {
                    match attempt.persist(room_id, sink).await {
                        Persisted::Closed => return Ok(()),
                        Persisted::Continue(next) => frontier = next,
                        Persisted::NoForwardingAddress(ids) => {
                            return Err(BackfillGiveUpReason::StillMissing(ids));
                        }
                    }
                }
            }
            Ok(_) => {
                tracing::debug!(
                    origin,
                    room_id,
                    "/get_missing_events answered with nothing; asking /backfill"
                );
            }
            Err(error) => {
                tracing::debug!(origin, room_id, %error, "/get_missing_events could not be used; asking /backfill");
            }
        }
    }

    for _round in 0..limits.max_rounds {
        let remaining_budget = limits
            .max_total_events
            .saturating_sub(attempt.fetched_total);
        if remaining_budget == 0 {
            return Err(BackfillGiveUpReason::TooManyEvents);
        }
        let request_limit = limits.max_events_per_fetch.min(remaining_budget);
        // Bound the request itself, not just the response: a `frontier` that has grown large
        // (many independent gaps discovered in one round) must not turn into an unbounded query
        // string.
        let from_ids: Vec<String> = frontier
            .iter()
            .take(limits.max_events_per_fetch)
            .cloned()
            .collect();

        let fetched = fetcher
            .fetch_backfill(origin, room_id, &from_ids, request_limit)
            .await
            .map_err(|e| BackfillGiveUpReason::RemoteUnavailable(e.to_string()))?;

        if fetched.is_empty() {
            return Err(BackfillGiveUpReason::StillMissing(frontier));
        }

        let new_this_round = attempt
            .absorb(&fetched, room_version, key_cache, limits)
            .await;
        if new_this_round == 0 {
            // Every event in this response was either unverifiable or already seen: nothing about
            // the gap changed, so there is nothing left to try that a further round would not
            // simply repeat.
            return Err(BackfillGiveUpReason::StillMissing(frontier));
        }

        match attempt.persist(room_id, sink).await {
            Persisted::Closed => return Ok(()),
            Persisted::Continue(next) => frontier = next,
            Persisted::NoForwardingAddress(ids) => {
                return Err(BackfillGiveUpReason::StillMissing(ids));
            }
        }
    }

    Err(BackfillGiveUpReason::TooManyRounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::{WriteOutcome, WriteRejected};
    use crate::keys::{
        KeyServerFetcher, OwnSigningKeys, RemoteKeyCache, build_server_key_response,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn signed_message(
        keys: &OwnSigningKeys,
        room_id: &str,
        sender: &str,
        prev_events: Vec<String>,
        depth: i64,
    ) -> Value {
        let mut object = hs_model::canonical::to_canonical_object(
            &serde_json::json!({
                "type": "m.room.message",
                "room_id": room_id,
                "sender": sender,
                "origin_server_ts": depth * 1000,
                "depth": depth,
                "content": {"body": format!("event at depth {depth}")},
                "prev_events": prev_events,
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
        // Sign the *redacted* object and copy the signature back onto the full one, matching the
        // spec's real signing order and `verify_pdu`'s (equally real, since this session) matching
        // verification order -- see `crate::inbound::verify_pdu`'s doc comment. Signing the
        // unredacted `m.room.message` content directly would produce a signature `verify_pdu`
        // correctly rejects, since redaction strips all of a message event's content.
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();
        let mut redacted = hs_model::redaction::redact(&object, &rules.redaction).unwrap();
        hs_model::signing::sign_object(&mut redacted, &server, keys.primary()).unwrap();
        object.insert(
            "signatures".to_owned(),
            redacted.remove("signatures").unwrap(),
        );
        serde_json::from_slice(
            &hs_model::canonical::CanonicalJsonValue::Object(object).to_canonical_bytes(),
        )
        .unwrap()
    }

    /// A fetcher that hands back exactly the events queued for it, one response (a `Vec` of raw
    /// PDU JSON) per call, in order; a call past the end of the queue returns an empty `Vec` (the
    /// honest "the remote has nothing more to give" case).
    struct QueuedFetcher {
        responses: Mutex<std::collections::VecDeque<Vec<Value>>>,
        calls: AtomicUsize,
        /// What `/get_missing_events` answers, when this fetcher answers it at all: `None`
        /// leaves the trait's default (an error, so the resolver asks `/backfill`).
        missing_events: Option<Vec<Value>>,
        missing_events_calls: AtomicUsize,
        /// The `(earliest_events, latest_events)` of the last `/get_missing_events` request.
        last_gap: Mutex<Option<(Vec<String>, Vec<String>)>>,
    }

    impl QueuedFetcher {
        fn new(responses: Vec<Vec<Value>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                calls: AtomicUsize::new(0),
                missing_events: None,
                missing_events_calls: AtomicUsize::new(0),
                last_gap: Mutex::new(None),
            }
        }

        fn answering_missing_events(mut self, events: Vec<Value>) -> Self {
            self.missing_events = Some(events);
            self
        }
    }

    #[async_trait]
    impl AncestorFetcher for QueuedFetcher {
        async fn fetch_backfill(
            &self,
            _destination: &str,
            _room_id: &str,
            _from_event_ids: &[String],
            _limit: usize,
        ) -> Result<Vec<Value>, AncestorFetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default())
        }

        async fn fetch_missing_events(
            &self,
            _destination: &str,
            _room_id: &str,
            earliest_events: &[String],
            latest_events: &[String],
            _limit: usize,
            _min_depth: i64,
        ) -> Result<Vec<Value>, AncestorFetchError> {
            self.missing_events_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_gap.lock().unwrap() =
                Some((earliest_events.to_vec(), latest_events.to_vec()));
            match &self.missing_events {
                Some(events) => Ok(events.clone()),
                None => Err(AncestorFetchError("no /get_missing_events here".to_owned())),
            }
        }
    }

    /// A fetcher that never runs out: every call gets back one freshly-generated event whose own
    /// `prev_events` names yet another event nobody has ever heard of. Simulates a hostile or
    /// simply broken peer that dangles an endless chain rather than admitting it has nothing to
    /// offer.
    struct EndlessFetcher {
        keys: OwnSigningKeys,
        sender: String,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl AncestorFetcher for EndlessFetcher {
        async fn fetch_backfill(
            &self,
            _destination: &str,
            room_id: &str,
            _from_event_ids: &[String],
            _limit: usize,
        ) -> Result<Vec<Value>, AncestorFetchError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let next_missing = format!("$never-ends-{n}");
            let event = signed_message(
                &self.keys,
                room_id,
                &self.sender,
                vec![next_missing],
                -(n as i64),
            );
            Ok(vec![event])
        }
    }

    /// A sink that persists whatever it is asked to as long as its own `prev_events` are already
    /// in `known`, mirroring `RoomActor::accept_remote_event`'s real behaviour closely enough to
    /// exercise the resolution loop's dependency-order handling.
    struct DagSink {
        known: Mutex<HashSet<String>>,
    }

    impl DagSink {
        fn new(known: impl IntoIterator<Item = String>) -> Self {
            Self {
                known: Mutex::new(known.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl RoomWriteSink for DagSink {
        async fn accept_verified_event(
            &self,
            _room_id: &str,
            event_id: &str,
            event_json: &Value,
        ) -> Result<WriteOutcome, WriteRejected> {
            let mut known = self.known.lock().unwrap();
            if known.contains(event_id) {
                return Ok(WriteOutcome::AlreadyKnown);
            }
            let prev: Vec<String> = event_json
                .get("prev_events")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let missing: Vec<String> = prev.into_iter().filter(|p| !known.contains(p)).collect();
            if !missing.is_empty() {
                return Err(WriteRejected::missing_ancestors(
                    missing,
                    "missing ancestor",
                ));
            }
            known.insert(event_id.to_owned());
            Ok(WriteOutcome::Stored)
        }
    }

    #[tokio::test]
    async fn resolves_a_single_hop_gap() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let room_id = "!r:origin.example.org";
        let sender = "@alice:origin.example.org";

        // The sink already holds `$root` (its own root event); the event that triggered
        // resolution cites `$missing` as its `prev_events`, and `$missing` in turn cites `$root`.
        let root_id = "$root".to_owned();
        let missing_event = signed_message(&keys, room_id, sender, vec![root_id.clone()], 1);
        let missing_id = hs_model::Event::parse(&missing_event, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();

        let fetcher = QueuedFetcher::new(vec![vec![missing_event]]);
        let sink = DagSink::new(vec![root_id]);

        let result = resolve_missing_ancestors(
            "origin.example.org",
            room_id,
            &RoomVersionId::V11,
            vec![missing_id.clone()],
            &GapContext::default(),
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(sink.known.lock().unwrap().contains(&missing_id));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn resolves_a_multi_hop_gap_across_several_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let room_id = "!r:origin.example.org";
        let sender = "@alice:origin.example.org";

        let root_id = "$root".to_owned();
        let e1 = signed_message(&keys, room_id, sender, vec![root_id.clone()], 1);
        let e1_id = hs_model::Event::parse(&e1, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();
        let e2 = signed_message(&keys, room_id, sender, vec![e1_id.clone()], 2);
        let e2_id = hs_model::Event::parse(&e2, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();

        // The remote only ever hands back one hop per round -- proves the loop actually recurses
        // rather than only handling a single response.
        let fetcher = QueuedFetcher::new(vec![vec![e2], vec![e1]]);
        let sink = DagSink::new(vec![root_id]);

        let result = resolve_missing_ancestors(
            "origin.example.org",
            room_id,
            &RoomVersionId::V11,
            vec![e2_id.clone()],
            &GapContext::default(),
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(sink.known.lock().unwrap().contains(&e1_id));
        assert!(sink.known.lock().unwrap().contains(&e2_id));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let room_id = "!r:origin.example.org";
        let sender = "@alice:origin.example.org".to_owned();

        let fetcher = EndlessFetcher {
            keys,
            sender,
            calls: AtomicUsize::new(0),
        };
        let sink = DagSink::new(Vec::new());
        let limits = BackfillLimits::default();

        let result = resolve_missing_ancestors(
            "origin.example.org",
            room_id,
            &RoomVersionId::V11,
            vec!["$initial-gap".to_owned()],
            &GapContext::default(),
            &fetcher,
            &cache,
            &sink,
            &limits,
        )
        .await;
        assert!(
            matches!(result, Err(BackfillGiveUpReason::TooManyRounds)),
            "{result:?}"
        );
        // Bounded: exactly `max_rounds` round-trips happened, not one per hop of the chain the
        // peer was willing to keep offering.
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), limits.max_rounds);
    }

    #[tokio::test]
    async fn gives_up_when_the_remote_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");

        let fetcher = QueuedFetcher::new(vec![Vec::new()]);
        let sink = DagSink::new(Vec::new());

        let result = resolve_missing_ancestors(
            "origin.example.org",
            "!r:origin.example.org",
            &RoomVersionId::V11,
            vec!["$gap".to_owned()],
            &GapContext::default(),
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(
            matches!(result, Err(BackfillGiveUpReason::StillMissing(_))),
            "{result:?}"
        );
    }

    /// The gap-shaped request closes the gap on its own: `/backfill` is never asked, and the
    /// request named exactly this server's extremity and the event that exposed the gap --
    /// which is what Complement's reference server checks before it answers.
    #[tokio::test]
    async fn a_gap_is_closed_through_get_missing_events_before_backfill_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let room_id = "!r:origin.example.org";
        let sender = "@alice:origin.example.org";

        // Held: `$root`. Missing: e1 (cites $root) and e2 (cites e1). The event that exposed
        // the gap, e3, cites e2; the remote is asked for what lies between $root and e3.
        let root_id = "$root".to_owned();
        let e1 = signed_message(&keys, room_id, sender, vec![root_id.clone()], 1);
        let e1_id = event_id_of(&e1);
        let e2 = signed_message(&keys, room_id, sender, vec![e1_id.clone()], 2);
        let e2_id = event_id_of(&e2);
        let e3 = signed_message(&keys, room_id, sender, vec![e2_id.clone()], 3);
        let e3_id = event_id_of(&e3);

        let fetcher = QueuedFetcher::new(vec![vec![e1.clone(), e2.clone()]])
            .answering_missing_events(vec![e1, e2]);
        let sink = DagSink::new(vec![root_id.clone()]);
        let context = GapContext {
            latest_event_id: Some(e3_id.clone()),
            earliest_events: vec![root_id.clone()],
        };

        let result = resolve_missing_ancestors(
            "origin.example.org",
            room_id,
            &RoomVersionId::V11,
            vec![e2_id.clone()],
            &context,
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(sink.known.lock().unwrap().contains(&e1_id));
        assert!(sink.known.lock().unwrap().contains(&e2_id));
        assert_eq!(fetcher.missing_events_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            0,
            "/backfill must not be asked once /get_missing_events closed the gap"
        );
        assert_eq!(
            fetcher.last_gap.lock().unwrap().clone(),
            Some((vec![root_id], vec![e3_id]))
        );
    }

    /// A peer that does not answer `/get_missing_events` (or a fetcher that cannot ask it) is
    /// asked `/backfill` as before, with the context making no difference.
    #[tokio::test]
    async fn falls_back_to_backfill_when_get_missing_events_does_not_help() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let room_id = "!r:origin.example.org";
        let sender = "@alice:origin.example.org";

        let root_id = "$root".to_owned();
        let missing_event = signed_message(&keys, room_id, sender, vec![root_id.clone()], 1);
        let missing_id = event_id_of(&missing_event);

        // No `/get_missing_events` answer: the trait default's error.
        let fetcher = QueuedFetcher::new(vec![vec![missing_event]]);
        let sink = DagSink::new(vec![root_id.clone()]);
        let context = GapContext {
            latest_event_id: Some("$the-event-that-exposed-it".to_owned()),
            earliest_events: vec![root_id],
        };

        let result = resolve_missing_ancestors(
            "origin.example.org",
            room_id,
            &RoomVersionId::V11,
            vec![missing_id.clone()],
            &context,
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(sink.known.lock().unwrap().contains(&missing_id));
        assert_eq!(fetcher.missing_events_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    }

    /// A partial `/get_missing_events` answer (the gap is deeper than one response) is kept, and
    /// `/backfill` walks the rest from where it left off.
    #[tokio::test]
    async fn a_partial_get_missing_events_answer_is_continued_by_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, "origin.example.org");
        let room_id = "!r:origin.example.org";
        let sender = "@alice:origin.example.org";

        let root_id = "$root".to_owned();
        let e1 = signed_message(&keys, room_id, sender, vec![root_id.clone()], 1);
        let e1_id = event_id_of(&e1);
        let e2 = signed_message(&keys, room_id, sender, vec![e1_id.clone()], 2);
        let e2_id = event_id_of(&e2);

        // `/get_missing_events` hands back only the newer half; `/backfill` supplies e1.
        let fetcher = QueuedFetcher::new(vec![vec![e1]]).answering_missing_events(vec![e2]);
        let sink = DagSink::new(vec![root_id.clone()]);
        let context = GapContext {
            latest_event_id: Some("$exposing-event".to_owned()),
            earliest_events: vec![root_id],
        };

        let result = resolve_missing_ancestors(
            "origin.example.org",
            room_id,
            &RoomVersionId::V11,
            vec![e2_id.clone()],
            &context,
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(sink.known.lock().unwrap().contains(&e1_id));
        assert!(sink.known.lock().unwrap().contains(&e2_id));
        assert_eq!(fetcher.missing_events_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    }

    fn event_id_of(raw: &Value) -> String {
        hs_model::Event::parse(raw, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string()
    }

    // The mutation test for `BackfillLimits::max_rounds` (raise it to a very large number,
    // confirm `gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever` above fails, put
    // it back) was performed by hand against `BackfillLimits::default()` and is recorded, with its
    // result, in `docs/status/06-federation.md` rather than committed as a standing test -- a
    // standing test cannot mutate the default it is itself asserting against without either two
    // copies of the limit or a test that stops meaning what its name says.
}
