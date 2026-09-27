//! Where the outbound sender's queues live: per destination, the PDUs not yet accepted by that
//! server, in order, and the state of the sender's own retrying of the head of that queue.
//!
//! [`crate::sender::FederationSender`] treats the store as the source of truth and its channels
//! as a fast path over it: a PDU is written here before it is handed to the destination's
//! worker, and deleted here only once the destination has accepted it (or this server's own
//! policy has refused it). So a restart, a crash or a shutdown loses nothing: on the next start
//! the sender reads back every destination with a non-empty queue, restores its worker and
//! resumes in order ([`crate::sender::FederationSender::resume`]). `PLAN.md` section 5.2 item 6
//! asks for exactly this ("each shard persists its queue state so failover resumes").
//!
//! Two stores implement [`OutboundStore`]: [`KvOutboundStore`] over any `hs-kv` backend (what
//! `hs serve` uses, on the same backend as everything else) and [`InMemoryOutboundStore`], for
//! tests and for a sender that was built without a backend, which is then exactly as volatile as
//! the sender was before this module existed. [`OutboundStore::durable`] tells them apart, so the
//! sender's shutdown message can say whether what is left queued is kept or lost.
//!
//! # Layout (`KvOutboundStore`)
//!
//! - `hs_federation.outbound_queue`: `(destination, seq) -> PDU JSON`. `seq` is one counter for
//!   the whole store, so a PDU queued for several destinations has one number everywhere, and a
//!   destination's queue in key order is its send order. The counter itself is
//!   `hs_federation.outbound_meta`'s `seq` key, advanced with `atomic_add` in the same
//!   transaction as the rows it numbers.
//! - `hs_federation.outbound_destinations`: `(destination,) -> OutboundDestinationState` JSON:
//!   how the sender's retrying of that destination's head transaction is going. Distinct from
//!   `hs_federation.destinations` ([`crate::destination_store`]), which is the *client's*
//!   connection-level backoff and applies to every outbound call, not only transactions; the
//!   admin API merges the two into one row per destination (`crate::admin_source`).
//!
//! Sequence numbers are `u64` big-endian in the key, so a range over one destination's prefix
//! is its queue oldest-first, and a `u64` never wraps in practice.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};
use hs_tables::TableError;
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The longest `last_error` kept per destination. An error string is a receiver's response
/// body or a client error's display; either can be large, and one line is what an operator
/// reads.
const MAX_ERROR_LEN: usize = 512;

/// A PDU queued for a destination, with the sequence number that fixes its place in that
/// destination's queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPdu {
    /// Store-wide, monotonic; a destination's queue is its PDUs in `seq` order.
    pub seq: u64,
    /// The event exactly as it will be sent (federation format: `hashes`, `signatures`, no
    /// `event_id`).
    pub pdu: Value,
}

/// How the sender's retrying of one destination's head transaction is going. Persisted, so a
/// restart resumes the backoff where it was rather than hammering a destination that was failing
/// a moment ago.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutboundDestinationState {
    /// Consecutive failed attempts at the current head transaction; zero once one is accepted.
    #[serde(default)]
    pub failures: u32,
    /// Milliseconds since the epoch before which the next attempt should not be made; `None`
    /// when the destination is not being waited on.
    #[serde(default)]
    pub next_attempt_ms: Option<u64>,
    /// What the last failed attempt said, for an operator; kept across a success until the next
    /// failure overwrites it.
    #[serde(default)]
    pub last_error: Option<String>,
    /// When the current run of failures began; `None` while the destination is not failing.
    #[serde(default)]
    pub failing_since_ms: Option<u64>,
    /// When a transaction was last attempted, success or failure.
    #[serde(default)]
    pub last_attempt_ms: Option<u64>,
    /// When a transaction was last accepted.
    #[serde(default)]
    pub last_success_ms: Option<u64>,
}

impl OutboundDestinationState {
    /// Records a failed attempt at `now`, to be retried at `next_attempt_ms`.
    #[must_use]
    pub fn on_failure(mut self, now: u64, error: &str, next_attempt_ms: u64) -> Self {
        self.failures = self.failures.saturating_add(1);
        self.next_attempt_ms = Some(next_attempt_ms);
        self.last_error = Some(truncate(error));
        self.failing_since_ms.get_or_insert(now);
        self.last_attempt_ms = Some(now);
        self
    }

    /// Records an accepted transaction at `now`: the run of failures is over.
    #[must_use]
    pub fn on_success(mut self, now: u64) -> Self {
        self.failures = 0;
        self.next_attempt_ms = None;
        self.failing_since_ms = None;
        self.last_attempt_ms = Some(now);
        self.last_success_ms = Some(now);
        self
    }

    /// Forgets the backoff (an administrator's reset): the next attempt may be made at once.
    /// `last_error` and the attempt times are history and stay.
    #[must_use]
    pub fn reset(mut self) -> Self {
        self.failures = 0;
        self.next_attempt_ms = None;
        self.failing_since_ms = None;
        self
    }

    /// Whether an attempt may be made at `now_ms`.
    #[must_use]
    pub fn is_ready(&self, now_ms: u64) -> bool {
        self.next_attempt_ms.is_none_or(|at| now_ms >= at)
    }

    /// The interval the current wait spans, in milliseconds, if there is one.
    #[must_use]
    pub fn retry_interval_ms(&self) -> Option<u64> {
        match (self.next_attempt_ms, self.last_attempt_ms) {
            (Some(next), Some(last)) => Some(next.saturating_sub(last)),
            _ => None,
        }
    }
}

fn truncate(error: &str) -> String {
    if error.len() <= MAX_ERROR_LEN {
        return error.to_owned();
    }
    let mut end = MAX_ERROR_LEN;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &error[..end])
}

/// What can go wrong talking to an [`OutboundStore`].
#[derive(Debug, thiserror::Error)]
pub enum OutboundStoreError {
    /// The backend refused or failed.
    #[error(transparent)]
    Kv(#[from] hs_kv::KvError),
    /// A stored row could not be encoded or decoded.
    #[error("outbound queue row could not be encoded or decoded: {0}")]
    Codec(String),
}

impl From<TableError> for OutboundStoreError {
    fn from(error: TableError) -> Self {
        match error {
            TableError::Kv(kv) => Self::Kv(kv),
            other => Self::Codec(other.to_string()),
        }
    }
}

impl From<serde_json::Error> for OutboundStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Codec(error.to_string())
    }
}

/// The sender's persisted queues and per-destination retry state. See the module docs. Every
/// method is synchronous: a call is one short transaction or snapshot read on the backend, the
/// same shape as [`crate::destination_store::DestinationStore`]'s.
pub trait OutboundStore: Send + Sync {
    /// Whether what this store holds outlives the process. Decides how the sender describes
    /// what is still queued at shutdown: kept for the next start, or lost.
    fn durable(&self) -> bool;

    /// Appends `pdu` to every destination's queue in one transaction and returns the sequence
    /// number it was given (the same for each destination).
    ///
    /// # Errors
    /// Returns the backend's error; nothing was queued anywhere then.
    fn enqueue(&self, destinations: &[String], pdu: &Value) -> Result<u64, OutboundStoreError>;

    /// The oldest `limit` PDUs queued for `destination`, oldest first. A row that no longer
    /// decodes is logged and skipped rather than blocking everything behind it.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn peek(&self, destination: &str, limit: usize) -> Result<Vec<QueuedPdu>, OutboundStoreError>;

    /// Removes every PDU queued for `destination` with a sequence number up to and including
    /// `through_seq`; returns how many rows that was.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn ack(&self, destination: &str, through_seq: u64) -> Result<usize, OutboundStoreError>;

    /// Every destination with a non-empty queue, with the queue's length, sorted by name.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn queued(&self) -> Result<Vec<(String, usize)>, OutboundStoreError>;

    /// How many PDUs are queued for `destination`.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn queue_len(&self, destination: &str) -> Result<usize, OutboundStoreError>;

    /// The retry state kept for `destination`, if any attempt has been recorded.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn state(
        &self,
        destination: &str,
    ) -> Result<Option<OutboundDestinationState>, OutboundStoreError>;

    /// Every destination a retry state is kept for, sorted by name.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn states(&self) -> Result<Vec<(String, OutboundDestinationState)>, OutboundStoreError>;

    /// Records a failed attempt at `destination`'s head transaction: what went wrong and when
    /// the next attempt is due.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn record_failure(
        &self,
        destination: &str,
        error: &str,
        next_attempt_ms: u64,
    ) -> Result<(), OutboundStoreError>;

    /// Records an accepted transaction for `destination`.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn record_success(&self, destination: &str) -> Result<(), OutboundStoreError>;

    /// Forgets `destination`'s backoff so its next attempt may be made at once
    /// ([`OutboundDestinationState::reset`]). A destination with no state is left without one.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn reset(&self, destination: &str) -> Result<(), OutboundStoreError>;
}

/// An [`OutboundStore`] that lives and dies with the process. What a sender built without a
/// backend uses; exactly as volatile as the sender was before persistence existed.
#[derive(Default)]
pub struct InMemoryOutboundStore {
    inner: Mutex<InMemoryInner>,
}

#[derive(Default)]
struct InMemoryInner {
    next_seq: u64,
    queues: HashMap<String, BTreeMap<u64, Value>>,
    states: HashMap<String, OutboundDestinationState>,
}

impl InMemoryOutboundStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InMemoryInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl OutboundStore for InMemoryOutboundStore {
    fn durable(&self) -> bool {
        false
    }

    fn enqueue(&self, destinations: &[String], pdu: &Value) -> Result<u64, OutboundStoreError> {
        let mut inner = self.lock();
        inner.next_seq += 1;
        let seq = inner.next_seq;
        for destination in destinations {
            inner
                .queues
                .entry(destination.clone())
                .or_default()
                .insert(seq, pdu.clone());
        }
        Ok(seq)
    }

    fn peek(&self, destination: &str, limit: usize) -> Result<Vec<QueuedPdu>, OutboundStoreError> {
        Ok(self
            .lock()
            .queues
            .get(destination)
            .map(|queue| {
                queue
                    .iter()
                    .take(limit)
                    .map(|(seq, pdu)| QueuedPdu {
                        seq: *seq,
                        pdu: pdu.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn ack(&self, destination: &str, through_seq: u64) -> Result<usize, OutboundStoreError> {
        let mut inner = self.lock();
        let Some(queue) = inner.queues.get_mut(destination) else {
            return Ok(0);
        };
        let kept = queue.split_off(&(through_seq + 1));
        let removed = queue.len();
        *queue = kept;
        if queue.is_empty() {
            inner.queues.remove(destination);
        }
        Ok(removed)
    }

    fn queued(&self) -> Result<Vec<(String, usize)>, OutboundStoreError> {
        let inner = self.lock();
        let mut all: Vec<(String, usize)> = inner
            .queues
            .iter()
            .filter(|(_, queue)| !queue.is_empty())
            .map(|(name, queue)| (name.clone(), queue.len()))
            .collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all)
    }

    fn queue_len(&self, destination: &str) -> Result<usize, OutboundStoreError> {
        Ok(self.lock().queues.get(destination).map_or(0, BTreeMap::len))
    }

    fn state(
        &self,
        destination: &str,
    ) -> Result<Option<OutboundDestinationState>, OutboundStoreError> {
        Ok(self.lock().states.get(destination).cloned())
    }

    fn states(&self) -> Result<Vec<(String, OutboundDestinationState)>, OutboundStoreError> {
        let inner = self.lock();
        let mut all: Vec<(String, OutboundDestinationState)> = inner
            .states
            .iter()
            .map(|(name, state)| (name.clone(), state.clone()))
            .collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all)
    }

    fn record_failure(
        &self,
        destination: &str,
        error: &str,
        next_attempt_ms: u64,
    ) -> Result<(), OutboundStoreError> {
        let mut inner = self.lock();
        let current = inner.states.remove(destination).unwrap_or_default();
        inner.states.insert(
            destination.to_owned(),
            current.on_failure(now_ms(), error, next_attempt_ms),
        );
        Ok(())
    }

    fn record_success(&self, destination: &str) -> Result<(), OutboundStoreError> {
        let mut inner = self.lock();
        let current = inner.states.remove(destination).unwrap_or_default();
        inner
            .states
            .insert(destination.to_owned(), current.on_success(now_ms()));
        Ok(())
    }

    fn reset(&self, destination: &str) -> Result<(), OutboundStoreError> {
        let mut inner = self.lock();
        if let Some(current) = inner.states.remove(destination) {
            inner.states.insert(destination.to_owned(), current.reset());
        }
        Ok(())
    }
}

/// The `hs-kv`-backed [`OutboundStore`]: see the module docs for the layout. What `hs serve`
/// gives its sender, on the same backend as every other table, so a queued PDU and the room
/// event it carries are durable together.
pub struct KvOutboundStore<B: KvBackend> {
    backend: B,
    queue: TypedKeyspace<B::Keyspace, (String, u64)>,
    destinations: TypedKeyspace<B::Keyspace, (String,)>,
    meta: B::Keyspace,
}

const SEQ_KEY: &[u8] = b"seq";

impl<B: KvBackend> KvOutboundStore<B> {
    /// Opens (creating if necessary) the three keyspaces on `backend`.
    ///
    /// # Errors
    /// Returns the backend's error if a keyspace cannot be opened.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        let queue = TypedKeyspace::new(backend.keyspace("hs_federation.outbound_queue")?);
        let destinations =
            TypedKeyspace::new(backend.keyspace("hs_federation.outbound_destinations")?);
        let meta = backend.keyspace("hs_federation.outbound_meta")?;
        Ok(Self {
            backend,
            queue,
            destinations,
            meta,
        })
    }

    fn read_state<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        reader: &R,
        key: &(String,),
    ) -> Result<Option<OutboundDestinationState>, OutboundStoreError> {
        match self.destinations.get(reader, key)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    fn write_state<W: KvWrite<Keyspace = B::Keyspace>>(
        &self,
        txn: &mut W,
        key: &(String,),
        state: &OutboundDestinationState,
    ) -> Result<(), OutboundStoreError> {
        let bytes = serde_json::to_vec(state)?;
        self.destinations.put(txn, key, &bytes)?;
        Ok(())
    }

    /// Runs `update` over `destination`'s current state (default when absent) in one
    /// transaction and stores the result. `update` returning `None` leaves the store untouched.
    fn update_state(
        &self,
        destination: &str,
        update: impl Fn(Option<OutboundDestinationState>) -> Option<OutboundDestinationState>,
    ) -> Result<(), OutboundStoreError> {
        let key = (destination.to_owned(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let current = self.read_state(txn, &key).map_err(into_kv)?;
            if let Some(next) = update(current) {
                self.write_state(txn, &key, &next).map_err(into_kv)?;
            }
            Ok(())
        })?;
        Ok(())
    }
}

/// `transact`'s closure speaks `KvError`; this store's own errors travel through it as backend
/// errors and come back out as [`OutboundStoreError::Kv`].
fn into_kv(error: OutboundStoreError) -> hs_kv::KvError {
    match error {
        OutboundStoreError::Kv(kv) => kv,
        other => hs_kv::KvError::backend(other),
    }
}

impl<B: KvBackend> OutboundStore for KvOutboundStore<B> {
    fn durable(&self) -> bool {
        true
    }

    fn enqueue(&self, destinations: &[String], pdu: &Value) -> Result<u64, OutboundStoreError> {
        let bytes = serde_json::to_vec(pdu)?;
        let seq = transact(&self.backend, TransactConfig::default(), |txn| {
            let seq = txn.atomic_add(&self.meta, SEQ_KEY, 1)?;
            // The counter is an `i64` by `atomic_add`'s contract and never negative here.
            let seq = u64::try_from(seq).unwrap_or(0);
            for destination in destinations {
                self.queue
                    .put(txn, &(destination.clone(), seq), &bytes)
                    .map_err(|e| into_kv(e.into()))?;
            }
            Ok(seq)
        })?;
        Ok(seq)
    }

    fn peek(&self, destination: &str, limit: usize) -> Result<Vec<QueuedPdu>, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(destination.to_owned(),))
            .limit(limit);
        let mut out = Vec::new();
        for item in self.queue.range(&snapshot, spec) {
            let ((_, seq), bytes) = item?;
            match serde_json::from_slice(&bytes) {
                Ok(pdu) => out.push(QueuedPdu { seq, pdu }),
                Err(error) => tracing::error!(
                    destination,
                    seq,
                    %error,
                    "a queued outbound PDU no longer decodes; skipping it"
                ),
            }
        }
        Ok(out)
    }

    fn ack(&self, destination: &str, through_seq: u64) -> Result<usize, OutboundStoreError> {
        let removed = transact(&self.backend, TransactConfig::default(), |txn| {
            let spec =
                TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(destination.to_owned(),));
            let keys: Vec<(String, u64)> = self
                .queue
                .range(txn, spec)
                .map(|item| item.map(|(key, _)| key).map_err(|e| into_kv(e.into())))
                .collect::<Result<_, _>>()?;
            let mut removed = 0usize;
            for key in keys.into_iter().filter(|(_, seq)| *seq <= through_seq) {
                self.queue
                    .delete(txn, &key)
                    .map_err(|e| into_kv(e.into()))?;
                removed += 1;
            }
            Ok(removed)
        })?;
        Ok(removed)
    }

    fn queued(&self) -> Result<Vec<(String, usize)>, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for item in self.queue.range(&snapshot, RangeSpec::full()) {
            let ((destination, _), _) = item?;
            *counts.entry(destination).or_default() += 1;
        }
        Ok(counts.into_iter().collect())
    }

    fn queue_len(&self, destination: &str) -> Result<usize, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(destination.to_owned(),));
        let mut count = 0usize;
        for item in self.queue.range(&snapshot, spec) {
            item?;
            count += 1;
        }
        Ok(count)
    }

    fn state(
        &self,
        destination: &str,
    ) -> Result<Option<OutboundDestinationState>, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        self.read_state(&snapshot, &(destination.to_owned(),))
    }

    fn states(&self) -> Result<Vec<(String, OutboundDestinationState)>, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        let mut all = Vec::new();
        for item in self.destinations.range(&snapshot, RangeSpec::full()) {
            let ((destination,), bytes) = item?;
            match serde_json::from_slice::<OutboundDestinationState>(&bytes) {
                Ok(state) => all.push((destination, state)),
                Err(error) => tracing::error!(
                    destination,
                    %error,
                    "an outbound destination's retry state no longer decodes; ignoring it"
                ),
            }
        }
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all)
    }

    fn record_failure(
        &self,
        destination: &str,
        error: &str,
        next_attempt_ms: u64,
    ) -> Result<(), OutboundStoreError> {
        let now = now_ms();
        self.update_state(destination, |current| {
            Some(
                current
                    .unwrap_or_default()
                    .on_failure(now, error, next_attempt_ms),
            )
        })
    }

    fn record_success(&self, destination: &str) -> Result<(), OutboundStoreError> {
        let now = now_ms();
        self.update_state(destination, |current| {
            Some(current.unwrap_or_default().on_success(now))
        })
    }

    fn reset(&self, destination: &str) -> Result<(), OutboundStoreError> {
        self.update_state(destination, |current| {
            current.map(OutboundDestinationState::reset)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn pdu(i: u64) -> Value {
        serde_json::json!({ "type": "m.room.message", "i": i })
    }

    fn stores() -> Vec<(&'static str, Box<dyn OutboundStore>)> {
        vec![
            ("memory", Box::new(InMemoryOutboundStore::new())),
            (
                "kv",
                Box::new(KvOutboundStore::open(MemoryBackend::new()).unwrap()),
            ),
        ]
    }

    #[test]
    fn a_queue_is_read_back_in_order_and_acked_through_a_sequence_number() {
        for (name, store) in stores() {
            let a = "a.example".to_owned();
            let b = "b.example".to_owned();
            let s1 = store.enqueue(&[a.clone(), b.clone()], &pdu(1)).unwrap();
            let s2 = store.enqueue(std::slice::from_ref(&a), &pdu(2)).unwrap();
            let s3 = store.enqueue(&[a.clone(), b.clone()], &pdu(3)).unwrap();
            assert!(s1 < s2 && s2 < s3, "{name}: {s1} {s2} {s3}");

            let head = store.peek(&a, 2).unwrap();
            assert_eq!(head.len(), 2, "{name}");
            assert_eq!(head[0].seq, s1);
            assert_eq!(head[0].pdu, pdu(1));
            assert_eq!(head[1].seq, s2);
            assert_eq!(
                store.queued().unwrap(),
                vec![(a.clone(), 3), (b.clone(), 2)],
                "{name}"
            );
            assert_eq!(store.queue_len(&a).unwrap(), 3);
            assert_eq!(store.queue_len("nobody.example").unwrap(), 0);

            assert_eq!(store.ack(&a, s2).unwrap(), 2, "{name}");
            let rest = store.peek(&a, 50).unwrap();
            assert_eq!(rest.len(), 1, "{name}");
            assert_eq!(rest[0].seq, s3);
            assert_eq!(rest[0].pdu, pdu(3));
            // b's queue was not touched by a's ack.
            assert_eq!(store.peek(&b, 50).unwrap().len(), 2, "{name}");
            assert_eq!(store.ack(&a, s3).unwrap(), 1);
            assert!(store.peek(&a, 50).unwrap().is_empty());
            assert_eq!(store.queued().unwrap(), vec![(b.clone(), 2)], "{name}");
            // Acking what is already gone is nothing, not an error.
            assert_eq!(store.ack(&a, s3).unwrap(), 0);
        }
    }

    #[test]
    fn retry_state_is_recorded_reset_and_listed() {
        for (name, store) in stores() {
            assert!(store.state("x.example").unwrap().is_none(), "{name}");
            store
                .record_failure("x.example", "connection refused", 5_000_000_000_000)
                .unwrap();
            store
                .record_failure("x.example", "HTTP 502", 5_000_000_000_001)
                .unwrap();
            let state = store.state("x.example").unwrap().unwrap();
            assert_eq!(state.failures, 2, "{name}");
            assert_eq!(state.next_attempt_ms, Some(5_000_000_000_001));
            assert_eq!(state.last_error.as_deref(), Some("HTTP 502"));
            assert!(state.failing_since_ms.is_some());
            assert!(!state.is_ready(now_ms()));
            assert!(state.retry_interval_ms().is_some());

            store.reset("x.example").unwrap();
            let state = store.state("x.example").unwrap().unwrap();
            assert_eq!(state.failures, 0, "{name}");
            assert_eq!(state.next_attempt_ms, None);
            assert_eq!(state.failing_since_ms, None);
            assert_eq!(state.last_error.as_deref(), Some("HTTP 502"));
            assert!(state.is_ready(now_ms()));
            // Resetting a destination nothing is known about records nothing.
            store.reset("unknown.example").unwrap();
            assert!(store.state("unknown.example").unwrap().is_none());

            store.record_success("x.example").unwrap();
            let state = store.state("x.example").unwrap().unwrap();
            assert!(state.last_success_ms.is_some(), "{name}");
            assert_eq!(state.failures, 0);
            assert_eq!(store.states().unwrap().len(), 1);
            assert_eq!(store.states().unwrap()[0].0, "x.example");
        }
    }

    #[test]
    fn the_kv_store_keeps_its_queue_and_sequence_across_a_reopen() {
        // A `MemoryBackend` clone shares its data, the way a reopened `FjallBackend` shares its
        // files: opening a second store over it is the restart.
        let backend = MemoryBackend::new();
        let first = KvOutboundStore::open(backend.clone()).unwrap();
        let s1 = first
            .enqueue(&["peer.example".to_owned()], &pdu(1))
            .unwrap();
        first
            .record_failure("peer.example", "connection refused", u64::MAX)
            .unwrap();
        drop(first);

        let second = KvOutboundStore::open(backend).unwrap();
        assert!(second.durable());
        assert_eq!(
            second.queued().unwrap(),
            vec![("peer.example".to_owned(), 1)]
        );
        let s2 = second
            .enqueue(&["peer.example".to_owned()], &pdu(2))
            .unwrap();
        assert!(
            s2 > s1,
            "the counter continues, so order is kept: {s1} {s2}"
        );
        let queue = second.peek("peer.example", 50).unwrap();
        assert_eq!(
            queue.iter().map(|q| q.seq).collect::<Vec<_>>(),
            vec![s1, s2]
        );
        let state = second.state("peer.example").unwrap().unwrap();
        assert_eq!(state.failures, 1);
        assert_eq!(state.last_error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn a_long_error_is_kept_to_one_line_of_reasonable_length() {
        let long = "x".repeat(10_000);
        let state = OutboundDestinationState::default().on_failure(1, &long, 2);
        assert!(state.last_error.unwrap().len() < 600);
        assert!(!InMemoryOutboundStore::new().durable());
    }
}
