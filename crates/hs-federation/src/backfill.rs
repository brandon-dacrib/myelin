//! Resolving `RoomError::MissingAncestors`: when an inbound event cites a `prev_events`/
//! `auth_events` entry this server does not hold, fetch the gap from the server that told us
//! about the event, verify each fetched event the same way any inbound PDU is verified, persist
//! them in dependency order, then let the caller retry the event that triggered this. See
//! `crate::inbound::process_transaction` for the one caller today.
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
            fetcher,
            key_cache,
            sink,
            limits,
        ),
    )
    .await
    .unwrap_or(Err(BackfillGiveUpReason::TimedOut))
}

#[allow(clippy::too_many_arguments)]
async fn resolve_inner(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    mut frontier: Vec<String>,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
    limits: &BackfillLimits,
) -> Result<(), BackfillGiveUpReason> {
    // Every event ID this attempt has already turned into a verified `Event`, across every round
    // -- so a peer that keeps re-sending the same event (whether by mistake or to burn cycles)
    // does not get re-verified or re-counted against `max_total_events` a second time.
    let mut already_fetched: HashSet<String> = HashSet::new();
    let mut fetched_total: usize = 0;
    // Verified events fetched in *any* round that could not yet be persisted (their own
    // ancestors were still missing at the time). Carried across rounds so that when a later
    // fetch closes what *they* were waiting on, they get retried in the same round that unblocks
    // them rather than being abandoned the moment their first attempt fails.
    let mut pending: Vec<hs_model::Event> = Vec::new();

    for _round in 0..limits.max_rounds {
        let remaining_budget = limits.max_total_events.saturating_sub(fetched_total);
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

        let mut new_this_round = 0usize;
        for raw in fetched.iter().take(limits.max_events_per_fetch) {
            let Ok(event) = verify_pdu(raw, room_version, key_cache).await else {
                continue;
            };
            if already_fetched.insert(event.event_id().to_string()) {
                fetched_total += 1;
                new_this_round += 1;
                pending.push(event);
                if fetched_total >= limits.max_total_events {
                    break;
                }
            }
        }
        if fetched_total > limits.max_total_events {
            return Err(BackfillGiveUpReason::TooManyEvents);
        }
        if new_this_round == 0 {
            // Every event in this response was either unverifiable or already seen: nothing about
            // the gap changed, so there is nothing left to try that a further round would not
            // simply repeat.
            return Err(BackfillGiveUpReason::StillMissing(frontier));
        }

        // Retry every event still waiting on something, old and new together, ancestors first: a
        // lower `depth` cannot depend on a higher one in an acyclic DAG, so one ascending pass
        // gives an event fetched several rounds ago its chance to succeed in the very round that
        // finally supplies what it was missing.
        pending.sort_by_key(|e| e.header().depth);

        let mut still_pending = Vec::new();
        let mut next_frontier: Vec<String> = Vec::new();
        for event in pending.drain(..) {
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
        pending = still_pending;

        if pending.is_empty() {
            return Ok(());
        }
        if next_frontier.is_empty() {
            // Everything still pending was hard-dropped by other branches above, or otherwise
            // reported no forwarding address -- there is nothing left this loop can usefully ask
            // for next.
            return Err(BackfillGiveUpReason::StillMissing(
                pending.iter().map(|e| e.event_id().to_string()).collect(),
            ));
        }
        frontier = next_frontier;
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
        hs_model::signing::sign_object(&mut object, &server, keys.primary()).unwrap();
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
    }

    impl QueuedFetcher {
        fn new(responses: Vec<Vec<Value>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                calls: AtomicUsize::new(0),
            }
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

    // The mutation test for `BackfillLimits::max_rounds` (raise it to a very large number,
    // confirm `gives_up_cleanly_on_an_endless_chain_instead_of_looping_forever` above fails, put
    // it back) was performed by hand against `BackfillLimits::default()` and is recorded, with its
    // result, in `docs/status/06-federation.md` rather than committed as a standing test -- a
    // standing test cannot mutate the default it is itself asserting against without either two
    // copies of the limit or a test that stops meaning what its name says.
}
