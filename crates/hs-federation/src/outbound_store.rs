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
//! - `hs_federation.outbound_lengths`: `(destination,) -> i64` (an `atomic_add` counter): how
//!   many rows `outbound_queue` holds for the destination, kept in the same transactions that
//!   add and remove them, so the queue bound is checked with one read rather than a scan. A
//!   store opened over data written before the counter existed counts the queue once, at open
//!   (`outbound_meta`'s `lengths_v1` key says that has been done).
//! - `hs_federation.outbound_room_queued`: `(destination, room_id) -> u64` (big-endian): the
//!   sequence number of the newest PDU of that room this server meant the destination to have
//!   -- written by every enqueue, whether a queue row was written for it or not.
//! - `hs_federation.outbound_room_sent`: `(destination, room_id) -> u64`: the sequence number of
//!   the newest PDU of that room the destination accepted (or that catch-up has accounted for).
//!   Together with the previous one this is Synapse's `destination_rooms` plus
//!   `destinations.last_successful_stream_ordering`, split per room: a room is *behind* for a
//!   destination when its queued position is past its sent one.
//! - `hs_federation.outbound_catch_up`: `(destination,) -> CatchUpMark` JSON, present while the
//!   destination is in catch-up mode (see [`CatchUpMark`] and `crate::sender`'s module docs).
//!
//! - `hs_federation.outbound_edus`: `(destination, seq) -> {"edu": EDU JSON, "key": coalescing
//!   key or null}`: the **durable EDUs** ([`OutboundStore::enqueue_durable_edu`], RFC 0023):
//!   to-device messages and device-list updates, which unlike typing or presence must survive a
//!   restart. Numbered from the same `seq` counter as the PDUs; deleted when the transaction
//!   carrying them is accepted ([`OutboundStore::ack_durable_edus`]).
//! - `hs_federation.outbound_edu_keys`: `(destination, key) -> seq`: the unsent durable EDU of
//!   each coalescing key, so a newer one replaces it with one read.
//! - `hs_federation.outbound_edu_lengths`: `(destination,) -> i64` (an `atomic_add` counter): how
//!   many durable EDUs are waiting for the destination, for the bound and for the worker's "is
//!   anything waiting" check.
//! - `hs_federation.outbound_meta`'s `cursor:{name}` keys: positions a follower of another
//!   stream has handed over to the sender ([`OutboundStore::cursor`]), so what it had not yet
//!   handed over when the process stopped is handed over at the next start (`hs-cli`'s
//!   device-list announcer).
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

/// A durable EDU queued for a destination ([`OutboundStore::enqueue_durable_edu`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedEdu {
    /// Store-wide, monotonic (the PDUs' counter): a destination's durable EDUs in `seq` order
    /// are the order they were queued in.
    pub seq: u64,
    /// The EDU as it goes in a transaction: `{"edu_type": ..., "content": ...}`.
    pub edu: Value,
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

/// Why a destination is in catch-up mode, and since when. Present in the store from the moment
/// the destination's queue overflowed (or something else said what was queued for it is not the
/// whole story) until the sender has sent it the latest event of every room it is behind in.
/// While a destination has one, nothing more is written to its queue: each new PDU only moves
/// its room's queued position forward, and catch-up sends the room's latest event instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpMark {
    /// When the destination entered catch-up mode, in milliseconds since the epoch.
    pub since_ms: u64,
    /// The store's sequence counter when the mark was set: every queue row for the destination
    /// is older than this, so a copy of one still on its way to the worker can be recognised
    /// as superseded and skipped.
    pub from_seq: u64,
    /// `queue_full` (the queue reached the configured bound) or `requested` (a caller asked,
    /// [`OutboundStore::mark_catch_up`]).
    pub reason: String,
    /// How many PDUs the queue held when the mark was set: what catch-up replaces.
    #[serde(default)]
    pub queued_when_marked: u64,
}

/// [`CatchUpMark::reason`] when the queue reached its bound.
pub const CATCH_UP_QUEUE_FULL: &str = "queue_full";
/// [`CatchUpMark::reason`] when a caller asked for catch-up.
pub const CATCH_UP_REQUESTED: &str = "requested";

/// What [`OutboundStore::enqueue`] did with one PDU.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Enqueued {
    /// The sequence number the PDU was given (the same for every destination).
    pub seq: u64,
    /// The destinations a queue row was written for, in the order given.
    pub queued: Vec<String>,
    /// The destinations whose queue was full: this call put them in catch-up mode and wrote no
    /// row for them.
    pub newly_catching_up: Vec<String>,
    /// The destinations already in catch-up mode: no row was written; only the room's queued
    /// position moved.
    pub catching_up: Vec<String>,
}

/// One room a destination is behind in: what catch-up sends the room's latest event for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomBehind {
    /// The room.
    pub room_id: String,
    /// The sequence number of the newest PDU of the room meant for the destination.
    pub queued_seq: u64,
    /// The sequence number of the newest one it accepted, if any.
    pub sent_seq: Option<u64>,
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

    /// Gives `pdu` the next sequence number and, in one transaction, for every destination:
    /// moves `room_id`'s queued position to it (when there is a room), and appends it to the
    /// destination's queue -- unless the destination is in catch-up mode, or its queue already
    /// holds `max_queue_len` PDUs, in which case no row is written and the destination is (or
    /// stays) in catch-up mode. [`Enqueued`] says which destination went which way.
    ///
    /// # Errors
    /// Returns the backend's error; nothing was queued anywhere then.
    fn enqueue(
        &self,
        destinations: &[String],
        room_id: Option<&str>,
        pdu: &Value,
        max_queue_len: usize,
    ) -> Result<Enqueued, OutboundStoreError>;

    /// The oldest `limit` PDUs queued for `destination`, oldest first. A row that no longer
    /// decodes is logged and skipped rather than blocking everything behind it.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn peek(&self, destination: &str, limit: usize) -> Result<Vec<QueuedPdu>, OutboundStoreError>;

    /// Removes every PDU queued for `destination` with a sequence number up to and including
    /// `through_seq`, and records each `(room_id, seq)` in `sent` as that room's sent position
    /// for the destination, in one transaction; returns how many rows were removed. `sent` is
    /// empty for rows that are dropped rather than delivered.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn ack(
        &self,
        destination: &str,
        through_seq: u64,
        sent: &[(String, u64)],
    ) -> Result<usize, OutboundStoreError>;

    /// The catch-up mark of `destination`, if it is in catch-up mode.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn catch_up_mark(&self, destination: &str) -> Result<Option<CatchUpMark>, OutboundStoreError>;

    /// Every destination in catch-up mode, sorted by name.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn catch_up_marks(&self) -> Result<Vec<(String, CatchUpMark)>, OutboundStoreError>;

    /// Puts `destination` in catch-up mode for `reason`, unless it already is. Returns whether
    /// this call did it.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn mark_catch_up(&self, destination: &str, reason: &str) -> Result<bool, OutboundStoreError>;

    /// Moves each `(room_id, seq)`'s queued position for `destination` forward to `seq` where
    /// it is behind it (never back). What the sender does with rows it drops unsent, so that a
    /// row written before positions were recorded is still caught up.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn note_queued(
        &self,
        destination: &str,
        rooms: &[(String, u64)],
    ) -> Result<(), OutboundStoreError>;

    /// The rooms `destination` is behind in (queued position past sent position), oldest queued
    /// position first, at most `limit`.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn rooms_behind(
        &self,
        destination: &str,
        limit: usize,
    ) -> Result<Vec<RoomBehind>, OutboundStoreError>;

    /// Records each `(room_id, seq)` as that room's sent position for `destination`.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn record_sent(
        &self,
        destination: &str,
        rooms: &[(String, u64)],
    ) -> Result<(), OutboundStoreError>;

    /// Takes `destination` out of catch-up mode if, and only if, it is behind in no room, in
    /// one transaction (so an enqueue racing it either lands before, and is seen as a room
    /// behind, or after, and is queued normally). Returns whether it did.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn finish_catch_up(&self, destination: &str) -> Result<bool, OutboundStoreError>;

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

    /// Queues `edu` durably for every destination, in one transaction: with a `coalesce_key`,
    /// it replaces the destination's unsent durable EDU with the same key; past
    /// `max_per_destination` the destination's oldest is dropped. Returns how many were dropped
    /// that way, over every destination.
    ///
    /// # Errors
    /// Returns the backend's error; nothing was queued anywhere then.
    fn enqueue_durable_edu(
        &self,
        destinations: &[String],
        edu: &Value,
        coalesce_key: Option<&str>,
        max_per_destination: usize,
    ) -> Result<usize, OutboundStoreError>;

    /// The oldest `limit` durable EDUs queued for `destination`, oldest first. A row that no
    /// longer decodes is logged and skipped.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn peek_durable_edus(
        &self,
        destination: &str,
        limit: usize,
    ) -> Result<Vec<QueuedEdu>, OutboundStoreError>;

    /// Removes the durable EDUs `seqs` of `destination` (those a transaction it accepted
    /// carried, or that this server's policy refused). One already gone -- replaced by a newer
    /// one with its key meanwhile -- is skipped. Returns how many were removed.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn ack_durable_edus(
        &self,
        destination: &str,
        seqs: &[u64],
    ) -> Result<usize, OutboundStoreError>;

    /// How many durable EDUs are queued for `destination`.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn durable_edu_len(&self, destination: &str) -> Result<usize, OutboundStoreError>;

    /// Every destination with durable EDUs queued, with how many, sorted by name.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn durable_edus_queued(&self) -> Result<Vec<(String, usize)>, OutboundStoreError>;

    /// The position stored under `name` by [`OutboundStore::set_cursor`], if any.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn cursor(&self, name: &str) -> Result<Option<u64>, OutboundStoreError>;

    /// Stores `position` under `name`.
    ///
    /// # Errors
    /// Returns the backend's error.
    fn set_cursor(&self, name: &str, position: u64) -> Result<(), OutboundStoreError>;
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
    /// `(destination, room) -> seq`, ordered so one destination's rooms are a range.
    room_queued: BTreeMap<(String, String), u64>,
    room_sent: BTreeMap<(String, String), u64>,
    marks: HashMap<String, CatchUpMark>,
    /// Durable EDUs: `destination -> seq -> (key, edu)`.
    edus: HashMap<String, BTreeMap<u64, (Option<String>, Value)>>,
    cursors: HashMap<String, u64>,
}

impl InMemoryInner {
    fn behind(&self, destination: &str) -> Vec<RoomBehind> {
        let mut rooms: Vec<RoomBehind> = self
            .room_queued
            .range((destination.to_owned(), String::new())..)
            .take_while(|((d, _), _)| d == destination)
            .filter_map(|((d, room), queued)| {
                let sent = self.room_sent.get(&(d.clone(), room.clone())).copied();
                (sent.is_none_or(|sent| sent < *queued)).then(|| RoomBehind {
                    room_id: room.clone(),
                    queued_seq: *queued,
                    sent_seq: sent,
                })
            })
            .collect();
        rooms.sort_by_key(|room| room.queued_seq);
        rooms
    }
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

    fn enqueue(
        &self,
        destinations: &[String],
        room_id: Option<&str>,
        pdu: &Value,
        max_queue_len: usize,
    ) -> Result<Enqueued, OutboundStoreError> {
        let mut inner = self.lock();
        inner.next_seq += 1;
        let seq = inner.next_seq;
        let mut out = Enqueued {
            seq,
            ..Enqueued::default()
        };
        for destination in destinations {
            if let Some(room) = room_id {
                inner
                    .room_queued
                    .insert((destination.clone(), room.to_owned()), seq);
            }
            if inner.marks.contains_key(destination) {
                out.catching_up.push(destination.clone());
                continue;
            }
            let len = inner.queues.get(destination).map_or(0, BTreeMap::len);
            if len >= max_queue_len {
                inner.marks.insert(
                    destination.clone(),
                    CatchUpMark {
                        since_ms: now_ms(),
                        from_seq: seq,
                        reason: CATCH_UP_QUEUE_FULL.to_owned(),
                        queued_when_marked: len as u64,
                    },
                );
                out.newly_catching_up.push(destination.clone());
                continue;
            }
            inner
                .queues
                .entry(destination.clone())
                .or_default()
                .insert(seq, pdu.clone());
            out.queued.push(destination.clone());
        }
        Ok(out)
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

    fn ack(
        &self,
        destination: &str,
        through_seq: u64,
        sent: &[(String, u64)],
    ) -> Result<usize, OutboundStoreError> {
        let mut inner = self.lock();
        for (room, seq) in sent {
            inner
                .room_sent
                .insert((destination.to_owned(), room.clone()), *seq);
        }
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

    fn catch_up_mark(&self, destination: &str) -> Result<Option<CatchUpMark>, OutboundStoreError> {
        Ok(self.lock().marks.get(destination).cloned())
    }

    fn catch_up_marks(&self) -> Result<Vec<(String, CatchUpMark)>, OutboundStoreError> {
        let mut all: Vec<(String, CatchUpMark)> = self
            .lock()
            .marks
            .iter()
            .map(|(name, mark)| (name.clone(), mark.clone()))
            .collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all)
    }

    fn mark_catch_up(&self, destination: &str, reason: &str) -> Result<bool, OutboundStoreError> {
        let mut inner = self.lock();
        if inner.marks.contains_key(destination) {
            return Ok(false);
        }
        let mark = CatchUpMark {
            since_ms: now_ms(),
            from_seq: inner.next_seq + 1,
            reason: reason.to_owned(),
            queued_when_marked: inner.queues.get(destination).map_or(0, BTreeMap::len) as u64,
        };
        inner.marks.insert(destination.to_owned(), mark);
        Ok(true)
    }

    fn note_queued(
        &self,
        destination: &str,
        rooms: &[(String, u64)],
    ) -> Result<(), OutboundStoreError> {
        let mut inner = self.lock();
        for (room, seq) in rooms {
            let entry = inner
                .room_queued
                .entry((destination.to_owned(), room.clone()))
                .or_insert(*seq);
            *entry = (*entry).max(*seq);
        }
        Ok(())
    }

    fn rooms_behind(
        &self,
        destination: &str,
        limit: usize,
    ) -> Result<Vec<RoomBehind>, OutboundStoreError> {
        let mut rooms = self.lock().behind(destination);
        rooms.truncate(limit);
        Ok(rooms)
    }

    fn record_sent(
        &self,
        destination: &str,
        rooms: &[(String, u64)],
    ) -> Result<(), OutboundStoreError> {
        let mut inner = self.lock();
        for (room, seq) in rooms {
            inner
                .room_sent
                .insert((destination.to_owned(), room.clone()), *seq);
        }
        Ok(())
    }

    fn finish_catch_up(&self, destination: &str) -> Result<bool, OutboundStoreError> {
        let mut inner = self.lock();
        if !inner.behind(destination).is_empty() {
            return Ok(false);
        }
        inner.marks.remove(destination);
        Ok(true)
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

    fn enqueue_durable_edu(
        &self,
        destinations: &[String],
        edu: &Value,
        coalesce_key: Option<&str>,
        max_per_destination: usize,
    ) -> Result<usize, OutboundStoreError> {
        let mut inner = self.lock();
        inner.next_seq += 1;
        let seq = inner.next_seq;
        let mut dropped = 0usize;
        for destination in destinations {
            let queue = inner.edus.entry(destination.clone()).or_default();
            if let Some(key) = coalesce_key {
                queue.retain(|_, (existing, _)| existing.as_deref() != Some(key));
            }
            queue.insert(seq, (coalesce_key.map(str::to_owned), edu.clone()));
            while queue.len() > max_per_destination.max(1) {
                queue.pop_first();
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    fn peek_durable_edus(
        &self,
        destination: &str,
        limit: usize,
    ) -> Result<Vec<QueuedEdu>, OutboundStoreError> {
        Ok(self
            .lock()
            .edus
            .get(destination)
            .map(|queue| {
                queue
                    .iter()
                    .take(limit)
                    .map(|(seq, (_, edu))| QueuedEdu {
                        seq: *seq,
                        edu: edu.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn ack_durable_edus(
        &self,
        destination: &str,
        seqs: &[u64],
    ) -> Result<usize, OutboundStoreError> {
        let mut inner = self.lock();
        let Some(queue) = inner.edus.get_mut(destination) else {
            return Ok(0);
        };
        let removed = seqs
            .iter()
            .filter(|seq| queue.remove(seq).is_some())
            .count();
        if queue.is_empty() {
            inner.edus.remove(destination);
        }
        Ok(removed)
    }

    fn durable_edu_len(&self, destination: &str) -> Result<usize, OutboundStoreError> {
        Ok(self.lock().edus.get(destination).map_or(0, BTreeMap::len))
    }

    fn durable_edus_queued(&self) -> Result<Vec<(String, usize)>, OutboundStoreError> {
        let inner = self.lock();
        let mut all: Vec<(String, usize)> = inner
            .edus
            .iter()
            .filter(|(_, queue)| !queue.is_empty())
            .map(|(name, queue)| (name.clone(), queue.len()))
            .collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all)
    }

    fn cursor(&self, name: &str) -> Result<Option<u64>, OutboundStoreError> {
        Ok(self.lock().cursors.get(name).copied())
    }

    fn set_cursor(&self, name: &str, position: u64) -> Result<(), OutboundStoreError> {
        self.lock().cursors.insert(name.to_owned(), position);
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
    lengths: B::Keyspace,
    room_queued: TypedKeyspace<B::Keyspace, (String, String)>,
    room_sent: TypedKeyspace<B::Keyspace, (String, String)>,
    marks: TypedKeyspace<B::Keyspace, (String,)>,
    edus: TypedKeyspace<B::Keyspace, (String, u64)>,
    edu_keys: TypedKeyspace<B::Keyspace, (String, String)>,
    edu_lengths: B::Keyspace,
}

/// A durable EDU row of `hs_federation.outbound_edus`.
#[derive(Serialize, Deserialize)]
struct EduRow {
    edu: Value,
    #[serde(default)]
    key: Option<String>,
}

fn cursor_key(name: &str) -> Vec<u8> {
    format!("cursor:{name}").into_bytes()
}

const SEQ_KEY: &[u8] = b"seq";
/// Present in `outbound_meta` once `outbound_lengths` has been brought up to date with the
/// queue (see the module docs).
const LENGTHS_KEY: &[u8] = b"lengths_v1";

fn length_key(destination: &str) -> Vec<u8> {
    hs_tables::key::TupleKey::encode(&(destination.to_owned(),))
}

fn decode_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

impl<B: KvBackend> KvOutboundStore<B> {
    /// Opens (creating if necessary) the store's keyspaces on `backend`, and counts the queue
    /// once if it was written before queue lengths were kept.
    ///
    /// # Errors
    /// Returns the backend's error if a keyspace cannot be opened or the queue counted.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        let queue = TypedKeyspace::new(backend.keyspace("hs_federation.outbound_queue")?);
        let destinations =
            TypedKeyspace::new(backend.keyspace("hs_federation.outbound_destinations")?);
        let meta = backend.keyspace("hs_federation.outbound_meta")?;
        let lengths = backend.keyspace("hs_federation.outbound_lengths")?;
        let room_queued =
            TypedKeyspace::new(backend.keyspace("hs_federation.outbound_room_queued")?);
        let room_sent = TypedKeyspace::new(backend.keyspace("hs_federation.outbound_room_sent")?);
        let marks = TypedKeyspace::new(backend.keyspace("hs_federation.outbound_catch_up")?);
        let edus = TypedKeyspace::new(backend.keyspace("hs_federation.outbound_edus")?);
        let edu_keys = TypedKeyspace::new(backend.keyspace("hs_federation.outbound_edu_keys")?);
        let edu_lengths = backend.keyspace("hs_federation.outbound_edu_lengths")?;
        let store = Self {
            backend,
            queue,
            destinations,
            meta,
            lengths,
            room_queued,
            room_sent,
            marks,
            edus,
            edu_keys,
            edu_lengths,
        };
        store.count_lengths_once()?;
        Ok(store)
    }

    /// Brings `outbound_lengths` up to date with a queue written before it existed: one scan,
    /// once per store (in the transaction that records it was done, so two processes opening
    /// the same store at once do it once between them).
    fn count_lengths_once(&self) -> Result<(), hs_kv::KvError> {
        if self
            .backend
            .snapshot()
            .get(&self.meta, LENGTHS_KEY)?
            .is_some()
        {
            return Ok(());
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            if txn.get(&self.meta, LENGTHS_KEY)?.is_some() {
                return Ok(());
            }
            let mut counts: BTreeMap<String, i64> = BTreeMap::new();
            for item in self.queue.range(txn, RangeSpec::full()) {
                let ((destination, _), _) = item.map_err(|e| into_kv(e.into()))?;
                *counts.entry(destination).or_default() += 1;
            }
            for (destination, count) in counts {
                txn.put(
                    &self.lengths,
                    &length_key(&destination),
                    &count.to_be_bytes(),
                )?;
            }
            txn.put(&self.meta, LENGTHS_KEY, b"1")
        })
    }

    fn queue_length<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        reader: &R,
        destination: &str,
    ) -> Result<u64, hs_kv::KvError> {
        Ok(reader
            .get(&self.lengths, &length_key(destination))?
            .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_ref()).ok())
            .map_or(0, |bytes| {
                u64::try_from(i64::from_be_bytes(bytes)).unwrap_or(0)
            }))
    }

    fn edu_length<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        reader: &R,
        destination: &str,
    ) -> Result<u64, hs_kv::KvError> {
        Ok(reader
            .get(&self.edu_lengths, &length_key(destination))?
            .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_ref()).ok())
            .map_or(0, |bytes| {
                u64::try_from(i64::from_be_bytes(bytes)).unwrap_or(0)
            }))
    }

    /// Deletes `destination`'s durable EDU `seq`, and its key's index entry when that still
    /// names it. Returns whether the row was there.
    fn delete_edu<W: KvWrite<Keyspace = B::Keyspace> + KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &mut W,
        destination: &str,
        seq: u64,
    ) -> Result<bool, hs_kv::KvError> {
        let row_key = (destination.to_owned(), seq);
        let Some(bytes) = self
            .edus
            .get(txn, &row_key)
            .map_err(|e| into_kv(e.into()))?
        else {
            return Ok(false);
        };
        if let Ok(EduRow { key: Some(key), .. }) = serde_json::from_slice::<EduRow>(&bytes) {
            let index_key = (destination.to_owned(), key);
            let indexed = self
                .edu_keys
                .get(txn, &index_key)
                .map_err(|e| into_kv(e.into()))?
                .and_then(|bytes| decode_u64(&bytes));
            if indexed == Some(seq) {
                self.edu_keys
                    .delete(txn, &index_key)
                    .map_err(|e| into_kv(e.into()))?;
            }
        }
        self.edus
            .delete(txn, &row_key)
            .map_err(|e| into_kv(e.into()))?;
        txn.atomic_add(&self.edu_lengths, &length_key(destination), -1)?;
        Ok(true)
    }

    fn read_mark<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        reader: &R,
        destination: &str,
    ) -> Result<Option<CatchUpMark>, OutboundStoreError> {
        match self.marks.get(reader, &(destination.to_owned(),))? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// One destination's room positions, `room -> seq`, from `keyspace`.
    fn room_positions<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        reader: &R,
        keyspace: &TypedKeyspace<B::Keyspace, (String, String)>,
        destination: &str,
    ) -> Result<BTreeMap<String, u64>, OutboundStoreError> {
        let spec =
            TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(destination.to_owned(),));
        let mut out = BTreeMap::new();
        for item in keyspace.range(reader, spec) {
            let ((_, room), bytes) = item?;
            if let Some(seq) = decode_u64(&bytes) {
                out.insert(room, seq);
            }
        }
        Ok(out)
    }

    fn behind<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        reader: &R,
        destination: &str,
    ) -> Result<Vec<RoomBehind>, OutboundStoreError> {
        let queued = self.room_positions(reader, &self.room_queued, destination)?;
        let sent = self.room_positions(reader, &self.room_sent, destination)?;
        let mut rooms: Vec<RoomBehind> = queued
            .into_iter()
            .filter_map(|(room_id, queued_seq)| {
                let sent_seq = sent.get(&room_id).copied();
                sent_seq
                    .is_none_or(|sent| sent < queued_seq)
                    .then_some(RoomBehind {
                        room_id,
                        queued_seq,
                        sent_seq,
                    })
            })
            .collect();
        rooms.sort_by_key(|room| room.queued_seq);
        Ok(rooms)
    }

    fn put_positions<W: KvWrite<Keyspace = B::Keyspace>>(
        &self,
        txn: &mut W,
        keyspace: &TypedKeyspace<B::Keyspace, (String, String)>,
        destination: &str,
        rooms: &[(String, u64)],
    ) -> Result<(), hs_kv::KvError> {
        for (room, seq) in rooms {
            keyspace
                .put(
                    txn,
                    &(destination.to_owned(), room.clone()),
                    &seq.to_be_bytes(),
                )
                .map_err(|e| into_kv(e.into()))?;
        }
        Ok(())
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

    fn enqueue(
        &self,
        destinations: &[String],
        room_id: Option<&str>,
        pdu: &Value,
        max_queue_len: usize,
    ) -> Result<Enqueued, OutboundStoreError> {
        let bytes = serde_json::to_vec(pdu)?;
        let max_queue_len = u64::try_from(max_queue_len).unwrap_or(u64::MAX);
        let enqueued = transact(&self.backend, TransactConfig::default(), |txn| {
            let seq = txn.atomic_add(&self.meta, SEQ_KEY, 1)?;
            // The counter is an `i64` by `atomic_add`'s contract and never negative here.
            let seq = u64::try_from(seq).unwrap_or(0);
            let mut out = Enqueued {
                seq,
                ..Enqueued::default()
            };
            for destination in destinations {
                if let Some(room) = room_id {
                    self.room_queued
                        .put(
                            txn,
                            &(destination.clone(), room.to_owned()),
                            &seq.to_be_bytes(),
                        )
                        .map_err(|e| into_kv(e.into()))?;
                }
                if self.read_mark(txn, destination).map_err(into_kv)?.is_some() {
                    out.catching_up.push(destination.clone());
                    continue;
                }
                let len = self.queue_length(txn, destination)?;
                if len >= max_queue_len {
                    let mark = CatchUpMark {
                        since_ms: now_ms(),
                        from_seq: seq,
                        reason: CATCH_UP_QUEUE_FULL.to_owned(),
                        queued_when_marked: len,
                    };
                    let mark = serde_json::to_vec(&mark).map_err(|e| into_kv(e.into()))?;
                    self.marks
                        .put(txn, &(destination.clone(),), &mark)
                        .map_err(|e| into_kv(e.into()))?;
                    out.newly_catching_up.push(destination.clone());
                    continue;
                }
                self.queue
                    .put(txn, &(destination.clone(), seq), &bytes)
                    .map_err(|e| into_kv(e.into()))?;
                txn.atomic_add(&self.lengths, &length_key(destination), 1)?;
                out.queued.push(destination.clone());
            }
            Ok(out)
        })?;
        Ok(enqueued)
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

    fn ack(
        &self,
        destination: &str,
        through_seq: u64,
        sent: &[(String, u64)],
    ) -> Result<usize, OutboundStoreError> {
        let removed = transact(&self.backend, TransactConfig::default(), |txn| {
            let spec =
                TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(destination.to_owned(),));
            // The queue is in sequence order: stop at the first row past `through_seq` rather
            // than reading the whole of a long queue.
            let mut keys: Vec<(String, u64)> = Vec::new();
            for item in self.queue.range(txn, spec) {
                let (key, _) = item.map_err(|e| into_kv(e.into()))?;
                if key.1 > through_seq {
                    break;
                }
                keys.push(key);
            }
            let removed = keys.len();
            for key in keys {
                self.queue
                    .delete(txn, &key)
                    .map_err(|e| into_kv(e.into()))?;
            }
            if removed > 0 {
                txn.atomic_add(
                    &self.lengths,
                    &length_key(destination),
                    -i64::try_from(removed).unwrap_or(i64::MAX),
                )?;
            }
            self.put_positions(txn, &self.room_sent, destination, sent)?;
            Ok(removed)
        })?;
        Ok(removed)
    }

    fn catch_up_mark(&self, destination: &str) -> Result<Option<CatchUpMark>, OutboundStoreError> {
        self.read_mark(&self.backend.snapshot(), destination)
    }

    fn catch_up_marks(&self) -> Result<Vec<(String, CatchUpMark)>, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        let mut all = Vec::new();
        for item in self.marks.range(&snapshot, RangeSpec::full()) {
            let ((destination,), bytes) = item?;
            match serde_json::from_slice::<CatchUpMark>(&bytes) {
                Ok(mark) => all.push((destination, mark)),
                Err(error) => tracing::error!(
                    destination,
                    %error,
                    "a destination's catch-up mark no longer decodes; ignoring it"
                ),
            }
        }
        Ok(all)
    }

    fn mark_catch_up(&self, destination: &str, reason: &str) -> Result<bool, OutboundStoreError> {
        let marked = transact(&self.backend, TransactConfig::default(), |txn| {
            if self.read_mark(txn, destination).map_err(into_kv)?.is_some() {
                return Ok(false);
            }
            let next_seq = txn
                .get(&self.meta, SEQ_KEY)?
                .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_ref()).ok())
                .map_or(0, |bytes| {
                    u64::try_from(i64::from_be_bytes(bytes)).unwrap_or(0)
                })
                + 1;
            let mark = CatchUpMark {
                since_ms: now_ms(),
                from_seq: next_seq,
                reason: reason.to_owned(),
                queued_when_marked: self.queue_length(txn, destination)?,
            };
            let mark = serde_json::to_vec(&mark).map_err(|e| into_kv(e.into()))?;
            self.marks
                .put(txn, &(destination.to_owned(),), &mark)
                .map_err(|e| into_kv(e.into()))?;
            Ok(true)
        })?;
        Ok(marked)
    }

    fn note_queued(
        &self,
        destination: &str,
        rooms: &[(String, u64)],
    ) -> Result<(), OutboundStoreError> {
        if rooms.is_empty() {
            return Ok(());
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (room, seq) in rooms {
                let key = (destination.to_owned(), room.clone());
                let current = self
                    .room_queued
                    .get(txn, &key)
                    .map_err(|e| into_kv(e.into()))?
                    .and_then(|bytes| decode_u64(&bytes));
                if current.is_none_or(|current| current < *seq) {
                    self.room_queued
                        .put(txn, &key, &seq.to_be_bytes())
                        .map_err(|e| into_kv(e.into()))?;
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    fn rooms_behind(
        &self,
        destination: &str,
        limit: usize,
    ) -> Result<Vec<RoomBehind>, OutboundStoreError> {
        let mut rooms = self.behind(&self.backend.snapshot(), destination)?;
        rooms.truncate(limit);
        Ok(rooms)
    }

    fn record_sent(
        &self,
        destination: &str,
        rooms: &[(String, u64)],
    ) -> Result<(), OutboundStoreError> {
        if rooms.is_empty() {
            return Ok(());
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.put_positions(txn, &self.room_sent, destination, rooms)
        })?;
        Ok(())
    }

    fn finish_catch_up(&self, destination: &str) -> Result<bool, OutboundStoreError> {
        let finished = transact(&self.backend, TransactConfig::default(), |txn| {
            // Read inside the transaction: an enqueue that moves a room's queued position
            // between this read and the commit conflicts with it, and the retry sees it.
            if !self.behind(txn, destination).map_err(into_kv)?.is_empty() {
                return Ok(false);
            }
            self.marks
                .delete(txn, &(destination.to_owned(),))
                .map_err(|e| into_kv(e.into()))?;
            Ok(true)
        })?;
        Ok(finished)
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

    fn enqueue_durable_edu(
        &self,
        destinations: &[String],
        edu: &Value,
        coalesce_key: Option<&str>,
        max_per_destination: usize,
    ) -> Result<usize, OutboundStoreError> {
        let bytes = serde_json::to_vec(&EduRow {
            edu: edu.clone(),
            key: coalesce_key.map(str::to_owned),
        })?;
        let max = u64::try_from(max_per_destination.max(1)).unwrap_or(u64::MAX);
        let dropped = transact(&self.backend, TransactConfig::default(), |txn| {
            let seq = txn.atomic_add(&self.meta, SEQ_KEY, 1)?;
            let seq = u64::try_from(seq).unwrap_or(0);
            let mut dropped = 0usize;
            for destination in destinations {
                if let Some(key) = coalesce_key {
                    let index_key = (destination.clone(), key.to_owned());
                    if let Some(previous) = self
                        .edu_keys
                        .get(txn, &index_key)
                        .map_err(|e| into_kv(e.into()))?
                        .and_then(|bytes| decode_u64(&bytes))
                    {
                        self.delete_edu(txn, destination, previous)?;
                    }
                    self.edu_keys
                        .put(txn, &index_key, &seq.to_be_bytes())
                        .map_err(|e| into_kv(e.into()))?;
                }
                self.edus
                    .put(txn, &(destination.clone(), seq), &bytes)
                    .map_err(|e| into_kv(e.into()))?;
                txn.atomic_add(&self.edu_lengths, &length_key(destination), 1)?;
                // Past the bound, the oldest go.
                let mut len = self.edu_length(txn, destination)?;
                while len > max {
                    let spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(
                        destination.clone(),
                    ))
                    .limit(1);
                    let oldest = self
                        .edus
                        .range(txn, spec)
                        .next()
                        .transpose()
                        .map_err(|e| into_kv(e.into()))?
                        .map(|((_, oldest), _)| oldest);
                    let Some(oldest) = oldest else {
                        break;
                    };
                    self.delete_edu(txn, destination, oldest)?;
                    dropped += 1;
                    len -= 1;
                }
            }
            Ok(dropped)
        })?;
        Ok(dropped)
    }

    fn peek_durable_edus(
        &self,
        destination: &str,
        limit: usize,
    ) -> Result<Vec<QueuedEdu>, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(destination.to_owned(),))
            .limit(limit);
        let mut out = Vec::new();
        for item in self.edus.range(&snapshot, spec) {
            let ((_, seq), bytes) = item?;
            match serde_json::from_slice::<EduRow>(&bytes) {
                Ok(row) => out.push(QueuedEdu { seq, edu: row.edu }),
                Err(error) => tracing::error!(
                    destination,
                    seq,
                    %error,
                    "a queued durable EDU no longer decodes; skipping it"
                ),
            }
        }
        Ok(out)
    }

    fn ack_durable_edus(
        &self,
        destination: &str,
        seqs: &[u64],
    ) -> Result<usize, OutboundStoreError> {
        if seqs.is_empty() {
            return Ok(0);
        }
        let removed = transact(&self.backend, TransactConfig::default(), |txn| {
            let mut removed = 0usize;
            for seq in seqs {
                if self.delete_edu(txn, destination, *seq)? {
                    removed += 1;
                }
            }
            Ok(removed)
        })?;
        Ok(removed)
    }

    fn durable_edu_len(&self, destination: &str) -> Result<usize, OutboundStoreError> {
        let len = self.edu_length(&self.backend.snapshot(), destination)?;
        Ok(usize::try_from(len).unwrap_or(usize::MAX))
    }

    fn durable_edus_queued(&self) -> Result<Vec<(String, usize)>, OutboundStoreError> {
        let snapshot = self.backend.snapshot();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for item in self.edus.range(&snapshot, RangeSpec::full()) {
            let ((destination, _), _) = item?;
            *counts.entry(destination).or_default() += 1;
        }
        Ok(counts.into_iter().collect())
    }

    fn cursor(&self, name: &str) -> Result<Option<u64>, OutboundStoreError> {
        Ok(self
            .backend
            .snapshot()
            .get(&self.meta, &cursor_key(name))?
            .and_then(|bytes| decode_u64(&bytes)))
    }

    fn set_cursor(&self, name: &str, position: u64) -> Result<(), OutboundStoreError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            txn.put(&self.meta, &cursor_key(name), &position.to_be_bytes())
        })?;
        Ok(())
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
            let s1 = store
                .enqueue(&[a.clone(), b.clone()], None, &pdu(1), usize::MAX)
                .unwrap()
                .seq;
            let s2 = store
                .enqueue(std::slice::from_ref(&a), None, &pdu(2), usize::MAX)
                .unwrap()
                .seq;
            let s3 = store
                .enqueue(&[a.clone(), b.clone()], None, &pdu(3), usize::MAX)
                .unwrap()
                .seq;
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

            assert_eq!(store.ack(&a, s2, &[]).unwrap(), 2, "{name}");
            let rest = store.peek(&a, 50).unwrap();
            assert_eq!(rest.len(), 1, "{name}");
            assert_eq!(rest[0].seq, s3);
            assert_eq!(rest[0].pdu, pdu(3));
            // b's queue was not touched by a's ack.
            assert_eq!(store.peek(&b, 50).unwrap().len(), 2, "{name}");
            assert_eq!(store.ack(&a, s3, &[]).unwrap(), 1);
            assert!(store.peek(&a, 50).unwrap().is_empty());
            assert_eq!(store.queued().unwrap(), vec![(b.clone(), 2)], "{name}");
            // Acking what is already gone is nothing, not an error.
            assert_eq!(store.ack(&a, s3, &[]).unwrap(), 0);
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
            .enqueue(&["peer.example".to_owned()], None, &pdu(1), usize::MAX)
            .unwrap()
            .seq;
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
            .enqueue(&["peer.example".to_owned()], None, &pdu(2), usize::MAX)
            .unwrap()
            .seq;
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

    /// A queue at its bound takes no more rows: the destination is marked for catch-up, and
    /// from then on each PDU only moves its room's queued position. Delivered rows move the
    /// sent position; catch-up ends only once no room is behind.
    #[test]
    fn a_full_queue_marks_its_destination_and_positions_say_which_rooms_are_behind() {
        for (name, store) in stores() {
            let d = "down.example".to_owned();
            let up = "up.example".to_owned();
            let both = [d.clone(), up.clone()];
            let s1 = store.enqueue(&both, Some("!a"), &pdu(1), 2).unwrap();
            assert_eq!(s1.queued, both.to_vec(), "{name}");
            let s2 = store.enqueue(&both, Some("!b"), &pdu(2), 2).unwrap();
            assert_eq!(store.queue_len(&d).unwrap(), 2, "{name}");
            assert!(store.catch_up_mark(&d).unwrap().is_none());

            // The third is over the bound for both; `up` delivers its first first.
            assert_eq!(store.ack(&up, s1.seq, &[("!a".into(), s1.seq)]).unwrap(), 1);
            let s3 = store.enqueue(&both, Some("!a"), &pdu(3), 2).unwrap();
            assert_eq!(s3.queued, vec![up.clone()], "{name}");
            assert_eq!(s3.newly_catching_up, vec![d.clone()], "{name}");
            let mark = store.catch_up_mark(&d).unwrap().expect("marked");
            assert_eq!(mark.reason, CATCH_UP_QUEUE_FULL);
            assert_eq!(mark.from_seq, s3.seq);
            assert_eq!(mark.queued_when_marked, 2);
            assert_eq!(store.queue_len(&d).unwrap(), 2, "no row past the bound");
            // `up` delivers the rest of what it has.
            assert_eq!(
                store
                    .ack(&up, s3.seq, &[("!b".into(), s2.seq), ("!a".into(), s3.seq)])
                    .unwrap(),
                2
            );
            let s4 = store.enqueue(&both, Some("!c"), &pdu(4), 2).unwrap();
            assert_eq!(s4.queued, vec![up.clone()], "{name}");
            assert_eq!(s4.catching_up, vec![d.clone()], "{name}");
            assert_eq!(store.queue_len(&d).unwrap(), 2);
            assert_eq!(store.catch_up_marks().unwrap().len(), 1);

            // `d` has had nothing: every room is behind, oldest queued position first.
            let behind = store.rooms_behind(&d, 10).unwrap();
            let order: Vec<(&str, u64)> = behind
                .iter()
                .map(|r| (r.room_id.as_str(), r.queued_seq))
                .collect();
            assert_eq!(
                order,
                vec![("!b", s2.seq), ("!a", s3.seq), ("!c", s4.seq)],
                "{name}"
            );
            assert_eq!(store.rooms_behind(&d, 1).unwrap().len(), 1);
            // `up` has had everything but `!c`'s, which is still in its queue.
            assert_eq!(
                store
                    .rooms_behind(&up, 10)
                    .unwrap()
                    .iter()
                    .map(|r| r.room_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["!c"],
                "{name}"
            );

            assert!(!store.finish_catch_up(&d).unwrap(), "{name}: still behind");
            store
                .record_sent(&d, &[("!b".into(), s2.seq), ("!a".into(), s3.seq)])
                .unwrap();
            assert!(!store.finish_catch_up(&d).unwrap(), "{name}: !c is behind");
            store.record_sent(&d, &[("!c".into(), s4.seq)]).unwrap();
            assert!(store.finish_catch_up(&d).unwrap(), "{name}");
            assert!(store.catch_up_mark(&d).unwrap().is_none());
            // Out of catch-up, but the queue is still at its bound until it is drained.
            assert_eq!(store.ack(&d, s2.seq, &[]).unwrap(), 2, "{name}");
            assert_eq!(
                store.enqueue(&both, Some("!a"), &pdu(5), 2).unwrap().queued,
                both.to_vec()
            );

            // `note_queued` only ever moves a position forward.
            store.note_queued(&d, &[("!z".into(), 1)]).unwrap();
            store.note_queued(&d, &[("!z".into(), 0)]).unwrap();
            let z: Vec<RoomBehind> = store
                .rooms_behind(&d, 10)
                .unwrap()
                .into_iter()
                .filter(|r| r.room_id == "!z")
                .collect();
            assert_eq!(z[0].queued_seq, 1, "{name}");

            // Asked for, a mark is set once.
            assert!(store.mark_catch_up(&up, CATCH_UP_REQUESTED).unwrap());
            assert!(!store.mark_catch_up(&up, CATCH_UP_REQUESTED).unwrap());
            let mark = store.catch_up_mark(&up).unwrap().unwrap();
            assert_eq!(mark.reason, CATCH_UP_REQUESTED, "{name}");
            assert!(mark.from_seq > s4.seq, "{name}: {mark:?}");
        }
    }

    /// A store written before queue lengths were kept counts its queue once when opened, so
    /// the bound applies to what was already there.
    #[test]
    fn a_queue_from_before_lengths_were_kept_is_counted_at_open() {
        let backend = MemoryBackend::new();
        let store = KvOutboundStore::open(backend.clone()).unwrap();
        let d = "d.example".to_owned();
        for i in 0..3 {
            store
                .enqueue(std::slice::from_ref(&d), None, &pdu(i), usize::MAX)
                .unwrap();
        }
        // What an older binary would have left: rows, no lengths, no marker.
        transact(&backend, TransactConfig::default(), |txn| {
            txn.delete(&store.lengths, &length_key(&d))?;
            txn.delete(&store.meta, LENGTHS_KEY)
        })
        .unwrap();
        drop(store);

        let reopened = KvOutboundStore::open(backend).unwrap();
        let full = reopened
            .enqueue(std::slice::from_ref(&d), None, &pdu(9), 3)
            .unwrap();
        assert_eq!(full.newly_catching_up, vec![d.clone()]);
        assert_eq!(
            reopened
                .queue_length(&reopened.backend.snapshot(), &d)
                .unwrap(),
            3
        );
    }

    #[test]
    fn a_long_error_is_kept_to_one_line_of_reasonable_length() {
        let long = "x".repeat(10_000);
        let state = OutboundDestinationState::default().on_failure(1, &long, 2);
        assert!(state.last_error.unwrap().len() < 600);
        assert!(!InMemoryOutboundStore::new().durable());
    }
}
