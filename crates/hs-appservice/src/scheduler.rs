//! The per-appservice transaction scheduler: ordered delivery, batching, retry with backoff,
//! dead-letter and replay (`PLAN.md` section 8.2, Appendix B).
//!
//! # Ordering
//!
//! Delivery is strictly ordered per appservice: [`Scheduler::drain`] only ever considers a
//! contiguous prefix of the pending queue starting at the lowest sequence number, and stops at
//! the first entry whose backoff has not elapsed rather than skip ahead to a later, ready entry.
//! This is what the spec's `txnId` reuse-on-retry contract assumes (a bridge that sees `txnId=5`
//! after `txnId=3` failed and is still backing off would have no way to know whether 4 was lost),
//! and it means one appservice's failures never affect another's — each appservice's queue and
//! backoff state is independent.
//!
//! # Batching
//!
//! Up to [`SchedulerConfig::max_batch`] ready, consecutive pending entries are merged into a
//! single HTTP transaction via [`merge_json`], which concatenates array-valued fields (`events`,
//! `ephemeral`, `to_device`) and deep-merges object-valued fields (`device_lists`, one-time-key
//! counts, fallback key types), with the later entry's leaf values winning on a scalar collision
//! (a newer one-time-key count is exactly what should supersede an older one). The batch's own
//! `txnId` is its highest sequence number.
//!
//! A new transaction also stops before its items (timeline events, ephemeral events and
//! to-device messages) would pass the appservice's item limit: [`SchedulerConfig::max_items`]
//! (100, Synapse's cap), halved each time the appservice times out and doubled back on each
//! delivery. An entry is one room's page of up to a hundred events, so twenty of them made
//! transactions of over a thousand events: on the demo on 2026-10-15 mautrix-discord took one of
//! 624 events 91 seconds to dispatch, because its handler does not answer until every event fits
//! in its event channel, and the 30-second request timeout fired on every transaction.
//!
//! # Retries
//!
//! When an attempt fails, every entry of the batch records the batch's `txnId`
//! ([`QueuedTransaction::txn_id`]), and the retry sends exactly those entries again under it,
//! whatever has been queued since: the spec's idempotency contract. Before 2026-10-15 a retry
//! re-formed the batch, so a failed batch of three that had grown to five by its retry went
//! under a new id, and a bridge that had taken the first attempt was sent its events twice.
//!
//! What a failure costs depends on what it says about the appservice ([`SendErrorKind`]):
//!
//! - **Transient** (no connection, timed out, a 5xx, 408 or 429): the appservice is down,
//!   restarting or busy. The transaction is retried, backing off to
//!   [`SchedulerConfig::max_backoff_ms`], until it has been failing for
//!   [`SchedulerConfig::give_up_after_ms`] (a day). A timeout waits at least
//!   [`SchedulerConfig::timeout_backoff_ms`] before its retry: the appservice took the request
//!   and is probably still working through it (mautrix-go goes on dispatching the events after
//!   the server hangs up), so an immediate retry only queues the same events behind them again.
//!   Until 2026-10-15 every failure counted against ten attempts, about four minutes: a Slack
//!   bridge restarting for five minutes lost a transaction to the dead letters.
//! - **Rejected** (any other status): the appservice answered and refused this transaction, so
//!   retrying for a day would hold up everything behind it. It is dead-lettered after
//!   [`SchedulerConfig::max_attempts`] attempts.
//!
//! # Dead letters
//!
//! A dead-lettered entry is kept, with its body, until an operator replays it
//! ([`Scheduler::replay`], which gives it a fresh retry budget) or discards it
//! ([`Scheduler::discard`]). The admin API lists them (`GET /appservices/{id}/backlog`), with
//! what each carried, so the choice can be made knowing what they are.
//!
//! # Persistence and restart
//!
//! There is no in-memory queue: every entry [`crate::registry::Registry::add`]-adjacent enqueue
//! calls create lives in [`crate::store::AppserviceStore`]'s `hs_appservice.txn_queue` keyspace
//! from the moment it is created, with its status, attempt count and next-retry time all part of
//! the same row. A fresh [`Scheduler`] opened against the same backend after a restart sees
//! exactly the same pending and dead-lettered entries with exactly the same backoff clocks — there
//! is no separate "resume" step to get wrong, because there was never a volatile copy to lose. See
//! [`tests::restart_resumes_pending_delivery`]. The item limit alone is kept in memory, and
//! starts again at [`SchedulerConfig::max_items`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use hs_auth::clock::Clock;
use hs_kv::KvBackend;
use serde_json::Value;

use crate::error::AppserviceError;
use crate::known_users::UserQueries;
use crate::metrics::AppserviceMetrics;
use crate::registry::Registry;
use crate::store::{QueueStatus, QueuedTransaction};
use crate::transaction::{Transaction, body_counts};

/// Tuning for [`Scheduler::drain`].
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    /// Maximum number of ready, consecutive pending entries merged into one HTTP transaction.
    pub max_batch: usize,
    /// Maximum items (timeline events, ephemeral events, to-device messages) a new transaction
    /// carries while the appservice keeps up; the first entry always goes, however large. See
    /// the module docs.
    pub max_items: usize,
    /// Attempts (including the first) before an entry the appservice rejected is
    /// dead-lettered; also the fewest attempts before a transiently failing one is.
    pub max_attempts: u32,
    /// How long an entry may keep failing transiently (unreachable, timed out, 5xx, 408, 429)
    /// before it is dead-lettered.
    pub give_up_after_ms: u64,
    /// Backoff before the second attempt; doubles each subsequent retry up to `max_backoff_ms`.
    pub base_backoff_ms: u64,
    /// Backoff never grows past this.
    pub max_backoff_ms: u64,
    /// The least wait after an attempt that timed out: the appservice may still be working on
    /// the transaction it did not answer.
    pub timeout_backoff_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_batch: 20,
            max_items: 100,
            max_attempts: 10,
            give_up_after_ms: 24 * 60 * 60 * 1000,
            base_backoff_ms: 500,
            max_backoff_ms: 5 * 60 * 1000,
            timeout_backoff_ms: 60 * 1000,
        }
    }
}

fn backoff_ms(config: &SchedulerConfig, attempts: u32) -> u64 {
    let scale = 1u64 << attempts.saturating_sub(1).min(20);
    config
        .base_backoff_ms
        .saturating_mul(scale)
        .min(config.max_backoff_ms)
}

/// How many items `body` carries toward [`SchedulerConfig::max_items`]: its timeline events,
/// ephemeral events and to-device messages.
fn items_in(body: &Value) -> usize {
    let counts = body_counts(body);
    counts.events + counts.typing + counts.receipts + counts.presence + counts.to_device
}

/// Why an attempt to send a transaction failed, which decides how long it is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendErrorKind {
    /// The appservice took the request and did not answer within the request timeout. It may
    /// still be working on it.
    TimedOut,
    /// No connection could be made, or it broke before an answer.
    Unreachable,
    /// The appservice answered 5xx, 408 or 429: busy or broken for now.
    Unavailable,
    /// The appservice answered with any other non-2xx status: it refused this transaction.
    Rejected,
}

impl SendErrorKind {
    /// The word for it in metrics and logs: `timed_out`, `unreachable`, `unavailable` or
    /// `rejected`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimedOut => "timed_out",
            Self::Unreachable => "unreachable",
            Self::Unavailable => "unavailable",
            Self::Rejected => "rejected",
        }
    }

    /// Whether the failure says something about the appservice now rather than about this
    /// transaction: everything but [`SendErrorKind::Rejected`].
    #[must_use]
    pub fn is_transient(self) -> bool {
        !matches!(self, Self::Rejected)
    }

    /// The kind of a non-2xx `status`.
    #[must_use]
    pub fn of_status(status: u16) -> Self {
        if status >= 500 || status == 408 || status == 429 {
            Self::Unavailable
        } else {
            Self::Rejected
        }
    }
}

/// A failed [`TransactionSender::send`]: why, and the error in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    /// Why it failed.
    pub kind: SendErrorKind,
    /// The error, as the delivery log and the appservice's `last_error` say it.
    pub message: String,
}

impl SendError {
    /// A failure of `kind`, said as `message`.
    #[must_use]
    pub fn new(kind: SendErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Delivers a transaction body to one appservice over HTTP. A trait so tests (and
/// `hs-bridge-conformance`) can substitute an in-process double for [`HttpTransactionSender`].
#[async_trait]
pub trait TransactionSender: Send + Sync {
    /// Sends `body` as transaction `txn_id` to `url`, authenticating with `hs_token`
    /// (`Authorization: Bearer <hs_token>`, matching the spec's appservice push auth).
    ///
    /// # Errors
    /// Returns why it failed ([`SendErrorKind`]) and a human-readable description (network,
    /// non-2xx status, timeout).
    async fn send(
        &self,
        url: &str,
        hs_token: &str,
        txn_id: &str,
        body: &Value,
    ) -> Result<(), SendError>;
}

/// How long [`HttpTransactionSender::new`] waits for an appservice to answer a transaction.
/// Sixty seconds, up from thirty on 2026-10-15: a bridge that is busy holds the request until
/// it has room for the events, and every timeout is a retry that queues them again.
pub const TRANSACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The real HTTP sender: `PUT {url}/_matrix/app/v1/transactions/{txnId}`.
pub struct HttpTransactionSender {
    client: reqwest::Client,
}

impl Default for HttpTransactionSender {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransactionSender {
    /// A sender with a [`TRANSACTION_TIMEOUT`] request timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(TRANSACTION_TIMEOUT)
    }

    /// A sender whose requests give up after `timeout`.
    #[must_use]
    pub fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            client: hs_http::client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl TransactionSender for HttpTransactionSender {
    async fn send(
        &self,
        url: &str,
        hs_token: &str,
        txn_id: &str,
        body: &Value,
    ) -> Result<(), SendError> {
        let base = url.trim_end_matches('/');
        let encoded_txn = urlencoding_minimal(txn_id);
        let full_url = format!("{base}/_matrix/app/v1/transactions/{encoded_txn}");
        // The exchange, timed by a span held around it; the request itself runs outside every
        // span (`hs_telemetry::outside_spans`, decision 0048), or a connection it opens would
        // hold the transaction's spans open for as long as the connection lives.
        let http_span = tracing::info_span!(
            "appservice.http",
            "otel.name" = "PUT /_matrix/app/v1/transactions",
            "otel.kind" = "client",
        );
        let response = hs_telemetry::outside_spans(
            self.client
                .put(&full_url)
                .bearer_auth(hs_token)
                .json(body)
                .send(),
        )
        .await;
        drop(http_span);
        let response = response.map_err(|e| {
            let kind = if e.is_timeout() {
                SendErrorKind::TimedOut
            } else {
                SendErrorKind::Unreachable
            };
            // The cause, not just "error sending request for url (...)": that is what the
            // delivery log and the appservice's `last_error` say.
            SendError::new(kind, crate::error::error_chain(&e))
        })?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            Err(SendError::new(
                SendErrorKind::of_status(status.as_u16()),
                format!("{status}: {text}"),
            ))
        }
    }
}

/// Percent-encodes a transaction id for use as a single path segment. Transaction ids here are
/// always plain decimal sequence numbers (see [`Scheduler::drain`]), so this only exists to avoid
/// pulling in a full URL-encoding dependency for the trivial case; it escapes anything outside
/// `[A-Za-z0-9._-]` defensively in case a future caller passes something else.
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Deep-merges two JSON values for batching: objects merge key by key (recursing), arrays
/// concatenate (`a`'s elements first), anything else takes `b` (the later entry wins on a scalar
/// collision — see the module docs on why that is the correct rule for one-time-key counts).
#[must_use]
pub fn merge_json(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(a_obj), Value::Object(b_obj)) => {
            let mut out = a_obj.clone();
            for (k, bv) in b_obj {
                let merged = match out.get(k) {
                    Some(av) => merge_json(av, bv),
                    None => bv.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Array(a_arr), Value::Array(b_arr)) => {
            let mut out = a_arr.clone();
            out.extend(b_arr.iter().cloned());
            Value::Array(out)
        }
        (_, b) => b.clone(),
    }
}

/// The outcome of one [`Scheduler::drain`] call for one appservice.
#[derive(Debug, Clone, PartialEq)]
pub enum DrainOutcome {
    /// Nothing pending.
    Empty,
    /// This appservice is paused; nothing was attempted.
    Paused,
    /// This appservice's registration has `url: null`; nothing was attempted (and nothing should
    /// ever have been enqueued for it — see [`Scheduler::enqueue`]).
    NoUrl,
    /// The oldest pending entry is still backing off; nothing was attempted (ordering rule).
    Waiting,
    /// A batch was delivered successfully.
    Delivered {
        /// How many queue entries were merged into the delivered batch.
        count: usize,
        /// The highest sequence number in the delivered batch.
        through_seq: u64,
    },
    /// A batch failed. Some or all of its entries may have been dead-lettered.
    Failed {
        /// How many queue entries were in the failed batch.
        count: usize,
        /// How many of them were dead-lettered by this attempt (their retry budget is spent).
        dead_lettered: usize,
        /// The delivery error.
        error: String,
        /// Why it failed.
        reason: SendErrorKind,
        /// The transaction id it was sent under; a retry uses it again.
        txn_id: u64,
        /// Which attempt this was (1 for the first).
        attempt: u32,
        /// How long the exchange took, in milliseconds.
        elapsed_ms: u64,
        /// When it is tried again, in milliseconds from now; `None` once it is dead-lettered.
        retry_in_ms: Option<u64>,
    },
}

/// Replay request: which dead-lettered (or still-pending) entries to reset for immediate retry.
/// Mirrors the admin API's `AppServiceReplayRequest`.
#[derive(Debug, Clone, Default)]
pub struct ReplayRequest {
    /// Specific transaction (queue sequence) ids to replay, as strings. Empty means "not
    /// filtering by id".
    pub transaction_ids: Vec<String>,
    /// Only replay entries enqueued at or after this time, in milliseconds since the epoch.
    pub since_ms: Option<u64>,
}

impl ReplayRequest {
    /// Whether the entry `seq`, queued at `enqueued_at_ms`, is one this request asks for.
    fn matches_head(&self, seq: u64, enqueued_at_ms: u64) -> bool {
        let id_ok =
            self.transaction_ids.is_empty() || self.transaction_ids.contains(&seq.to_string());
        let since_ok = self.since_ms.is_none_or(|since| enqueued_at_ms >= since);
        id_ok && since_ok
    }
}

/// Discard request: which dead-lettered entries to delete. Mirrors the admin API's
/// `AppServiceDiscardRequest`. With neither filter, every dead-lettered entry.
#[derive(Debug, Clone, Default)]
pub struct DiscardRequest {
    /// Specific transaction (queue sequence) ids, as strings. Empty means "not filtering by
    /// id".
    pub transaction_ids: Vec<String>,
    /// Only entries enqueued before this time, in milliseconds since the epoch.
    pub before_ms: Option<u64>,
}

impl DiscardRequest {
    /// Whether the entry `seq`, queued at `enqueued_at_ms`, is one this request asks for.
    fn matches_head(&self, seq: u64, enqueued_at_ms: u64) -> bool {
        let id_ok =
            self.transaction_ids.is_empty() || self.transaction_ids.contains(&seq.to_string());
        let before_ok = self.before_ms.is_none_or(|before| enqueued_at_ms < before);
        id_ok && before_ok
    }
}

/// `txn` as it would be sent to `row`'s appservice -- its spelling gated on that registration's
/// flags ([`Transaction::to_wire_json`]) -- or `None` if nothing should be queued for it at all:
/// the transaction is empty, or the registration has `url: null` and is never pushed to. See
/// [`Scheduler::enqueue`], which this is the deciding half of; [`crate::pump`] is the other
/// caller, because it queues for several appservices in one transaction of its own.
#[must_use]
pub fn wire_body(row: &crate::store::AppserviceRow, txn: &Transaction) -> Option<Value> {
    if row.url.is_none() || txn.is_empty() {
        return None;
    }
    Some(txn.to_wire_json(
        row.receive_ephemeral,
        row.push_ephemeral_legacy,
        row.msc3202,
    ))
}

/// The per-appservice transaction scheduler.
pub struct Scheduler<B: KvBackend> {
    registry: Arc<Registry<B>>,
    clock: Arc<dyn Clock>,
    sender: Arc<dyn TransactionSender>,
    config: SchedulerConfig,
    metrics: Option<AppserviceMetrics>,
    /// Asks an appservice about the unknown users of its namespace a batch names, before the
    /// batch is sent ([`crate::known_users`]). `None` asks nobody.
    user_queries: Option<Arc<UserQueries<B>>>,
    /// Each appservice's item limit, where it is below [`SchedulerConfig::max_items`] (see the
    /// module docs).
    item_limits: Mutex<HashMap<String, usize>>,
}

impl<B: KvBackend> Scheduler<B> {
    /// Builds a scheduler over `registry`, sending with `sender`.
    #[must_use]
    pub fn new(
        registry: Arc<Registry<B>>,
        clock: Arc<dyn Clock>,
        sender: Arc<dyn TransactionSender>,
    ) -> Self {
        Self {
            registry,
            clock,
            sender,
            config: SchedulerConfig::default(),
            metrics: None,
            user_queries: None,
            item_limits: Mutex::new(HashMap::new()),
        }
    }

    /// Before each batch is sent to an appservice, asks that appservice about each user of its
    /// namespace the batch's events name who has no account, and waits for the answers
    /// ([`crate::known_users`]). The asking is done by whoever calls [`Scheduler::drain`] for
    /// that appservice -- its own worker -- so a bridge that never answers holds only its own
    /// delivery.
    #[must_use]
    pub fn with_user_queries(mut self, user_queries: Arc<UserQueries<B>>) -> Self {
        self.user_queries = Some(user_queries);
        self
    }

    /// Overrides the default [`SchedulerConfig`].
    #[must_use]
    pub fn with_config(mut self, config: SchedulerConfig) -> Self {
        self.config = config;
        self
    }

    /// Counts every delivery into `metrics` ([`crate::metrics`]). Without this, nothing is
    /// counted, which is what tests and the conformance harness want.
    #[must_use]
    pub fn with_metrics(mut self, metrics: AppserviceMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// How many items `appservice_id`'s next new transaction may carry.
    #[must_use]
    pub fn item_limit(&self, appservice_id: &str) -> usize {
        self.item_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(appservice_id)
            .copied()
            .unwrap_or(self.config.max_items)
            .max(1)
    }

    /// Sets `appservice_id`'s item limit to what `change` makes of it, within
    /// `1..=max_items`, and reports it.
    fn change_item_limit(&self, appservice_id: &str, change: impl FnOnce(usize) -> usize) {
        let max = self.config.max_items.max(1);
        let limit = {
            let mut limits = self
                .item_limits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let current = limits.get(appservice_id).copied().unwrap_or(max);
            let next = change(current).clamp(1, max);
            if next == max {
                limits.remove(appservice_id);
            } else {
                limits.insert(appservice_id.to_owned(), next);
            }
            next
        };
        if let Some(metrics) = &self.metrics {
            metrics.set_batch_item_limit(appservice_id, limit);
        }
    }

    /// Runs `work` against the registry on tokio's blocking pool rather than on an async worker
    /// thread: a store read or write can take a while (an LSM store halts writers while it
    /// compacts), and an async worker held that long is one fewer to answer `/health/live`.
    async fn off_the_runtime<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Registry<B>) -> Result<T, AppserviceError> + Send + 'static,
    ) -> Result<T, AppserviceError>
    where
        B: 'static,
    {
        let registry = self.registry.clone();
        hs_telemetry::spans::spawn_blocking("appservice.store", move || work(&registry))
            .await
            .map_err(|e| AppserviceError::Store(format!("queue work did not finish: {e}")))?
    }

    /// Enqueues a transaction for `appservice_id`, gating its wire spelling on that appservice's
    /// registration flags (`crate::transaction::Transaction::to_wire_json`).
    ///
    /// Returns `Ok(None)`, enqueueing nothing, when either: the transaction has nothing in it
    /// ([`Transaction::is_empty`]), or the appservice's registration has `url: null` — `PLAN.md`
    /// section 8.2 requires these "never pushed to", and the simplest way to guarantee that
    /// absolutely is to never let anything accumulate for them in the first place, rather than
    /// enqueue-then-suppress-at-drain-time (which would grow an unbounded backlog for a
    /// double-puppet registration with a broad namespace that is nominally "interested" in every
    /// event, per Synapse's own interest routing).
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `appservice_id` is not registered.
    pub fn enqueue(
        &self,
        appservice_id: &str,
        txn: &Transaction,
    ) -> Result<Option<u64>, AppserviceError> {
        let row = self
            .registry
            .get(appservice_id)?
            .ok_or_else(|| AppserviceError::NotFound(appservice_id.to_string()))?;
        let Some(body) = wire_body(&row, txn) else {
            return Ok(None);
        };
        let seq = self
            .registry
            .store()
            .enqueue(appservice_id, body, self.now_ms())?;
        Ok(Some(seq))
    }

    /// Attempts one delivery cycle for `appservice_id`: batches and sends the oldest ready
    /// pending entries, applying success/failure/backoff/dead-letter bookkeeping. Call this
    /// repeatedly (a poll loop, or in response to an enqueue) to drive delivery; it does not spawn
    /// its own background task, since which driving strategy fits (per-appservice tokio tasks vs.
    /// a shared poll loop vs. cluster-sharded ownership per `PLAN.md` section 5.2) is a decision
    /// for whichever crate wires this scheduler into the running server, not this crate.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `appservice_id` is not registered, or a
    /// [`AppserviceError::Store`] on backend failure. A delivery failure itself is not a `Result`
    /// error — it is reported as `Ok(`[`DrainOutcome::Failed`]`)`, since "the appservice was
    /// unreachable" is an expected, routine outcome the caller is meant to handle by trying again
    /// later, not an operational failure of the scheduler itself.
    pub async fn drain(&self, appservice_id: &str) -> Result<DrainOutcome, AppserviceError>
    where
        B: 'static,
    {
        let row = self
            .registry
            .get(appservice_id)?
            .ok_or_else(|| AppserviceError::NotFound(appservice_id.to_string()))?;
        if row.paused {
            return Ok(DrainOutcome::Paused);
        }
        let Some(url) = row.url.clone() else {
            return Ok(DrainOutcome::NoUrl);
        };

        // Only the next batch is read, never the whole queue (`pending_batch`), and off the
        // async worker threads.
        let max_batch = self.config.max_batch.max(1);
        let id = appservice_id.to_owned();
        let pending: Vec<QueuedTransaction> = self
            .off_the_runtime(move |registry| registry.store().pending_batch(&id, max_batch))
            .await?;

        let Some(head) = pending.first() else {
            return Ok(DrainOutcome::Empty);
        };
        let now = self.now_ms();
        if head.next_attempt_at_ms > now {
            return Ok(DrainOutcome::Waiting);
        }

        let (batch, through_seq, retry) = match head.txn_id {
            // A retry: exactly the entries that failed together, under the id they failed
            // under, whatever has been queued since.
            Some(txn_id) => {
                let batch: Vec<QueuedTransaction> = pending
                    .into_iter()
                    .take_while(|e| e.txn_id == Some(txn_id))
                    .collect();
                (batch, txn_id, true)
            }
            // A new transaction: a contiguous prefix of entries never sent, stopping at the
            // first not-yet-ready entry (ordering rule), at `max_batch`, or before the items
            // pass the appservice's limit.
            None => {
                let item_limit = self.item_limit(appservice_id);
                let mut batch = Vec::new();
                let mut items = 0usize;
                for entry in pending {
                    if entry.next_attempt_at_ms > now
                        || entry.txn_id.is_some()
                        || batch.len() >= max_batch
                    {
                        break;
                    }
                    let n = items_in(&entry.body);
                    if !batch.is_empty() && items.saturating_add(n) > item_limit {
                        break;
                    }
                    items = items.saturating_add(n);
                    batch.push(entry);
                }
                let through_seq = batch.last().map_or(0, |e| e.seq);
                (batch, through_seq, false)
            }
        };
        let merged = batch
            .iter()
            .map(|e| e.body.clone())
            .reduce(|a, b| merge_json(&a, &b))
            .unwrap_or(Value::Object(serde_json::Map::new()));
        let attempt = batch.iter().map(|e| e.attempts).max().unwrap_or(0) + 1;

        // One delivery attempt is a trace of its own (a background pass, decision 0048): the
        // user queries made first, the HTTP exchange and the queue's bookkeeping. A drain that
        // finds nothing to send makes no span.
        let counts = body_counts(&merged);
        let span = tracing::info_span!(
            parent: None,
            "appservice.transaction",
            appservice = %appservice_id,
            txn_id = through_seq,
            attempt,
            retry,
            entries = batch.len(),
            events = counts.events,
            ephemeral = counts.typing + counts.receipts + counts.presence,
            to_device = counts.to_device,
            outcome = tracing::field::Empty,
            reason = tracing::field::Empty,
            "otel.status_code" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
        );
        let record_on = span.clone();
        let result = tracing::Instrument::instrument(
            self.deliver_batch(
                appservice_id,
                &row,
                &url,
                Attempt {
                    batch,
                    through_seq,
                    merged,
                    attempt,
                    retry,
                    now,
                },
            ),
            span,
        )
        .await;
        match &result {
            Ok(DrainOutcome::Delivered { .. }) => {
                record_on.record("outcome", "delivered");
            }
            Ok(DrainOutcome::Failed { error, reason, .. }) => {
                record_on.record("outcome", "failed");
                record_on.record("reason", reason.as_str());
                record_on.record("otel.status_code", "error");
                record_on.record("error.message", error.as_str());
            }
            Ok(_) => {}
            Err(error) => {
                record_on.record("outcome", "store_error");
                record_on.record("otel.status_code", "error");
                record_on.record("error.message", tracing::field::display(error));
            }
        };
        result
    }

    /// [`Scheduler::drain`]'s delivery of one attempt, and the queue's and health record's
    /// bookkeeping after it.
    async fn deliver_batch(
        &self,
        appservice_id: &str,
        row: &crate::store::AppserviceRow,
        url: &str,
        attempt: Attempt,
    ) -> Result<DrainOutcome, AppserviceError>
    where
        B: 'static,
    {
        let Attempt {
            batch,
            through_seq,
            merged,
            attempt,
            retry,
            now,
        } = attempt;
        if let Some(user_queries) = &self.user_queries {
            user_queries.before_delivery(row, &merged).await;
        }

        let txn_id = through_seq.to_string();
        if retry && let Some(metrics) = &self.metrics {
            metrics.record_retry(appservice_id);
        }
        let started = Instant::now();
        let outcome = self.sender.send(url, &row.hs_token, &txn_id, &merged).await;
        let elapsed = started.elapsed();

        match outcome {
            Ok(()) => {
                let seqs: Vec<u64> = batch.iter().map(|e| e.seq).collect();
                let id = appservice_id.to_owned();
                self.off_the_runtime(move |registry| {
                    registry.store().delete_queue_entries(&id, &seqs)
                })
                .await?;
                let counts = body_counts(&merged);
                tracing::info!(
                    appservice = %appservice_id,
                    txn_id = %txn_id,
                    attempt,
                    elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                    entries = batch.len(),
                    events = counts.events,
                    typing = counts.typing,
                    receipts = counts.receipts,
                    presence = counts.presence,
                    to_device = counts.to_device,
                    device_list_changes = counts.device_list_changes,
                    one_time_key_counts = counts.one_time_key_counts,
                    fallback_key_types = counts.fallback_key_types,
                    "delivered a transaction to an appservice"
                );
                self.change_item_limit(appservice_id, |limit| limit.saturating_mul(2));
                if let Some(metrics) = &self.metrics {
                    metrics.record_delivered(appservice_id, &counts);
                    metrics.record_attempt_seconds(
                        appservice_id,
                        "delivered",
                        elapsed.as_secs_f64(),
                    );
                }
                let mut health = self.registry.store().health(appservice_id)?;
                health.consecutive_failures = 0;
                health.last_error = None;
                health.last_success_at_ms = Some(now);
                health.last_success_seq = Some(through_seq);
                // A client refusing to share a room's keys with the bridge is the one thing
                // that makes a healthy bridge drop a person's message; keep the last one where
                // the admin API can show it, beside the bridge's own warning in the chat.
                for withheld in crate::transaction::key_withheld_in(&merged) {
                    tracing::warn!(
                        appservice = %appservice_id,
                        sender = %withheld.sender,
                        code = %withheld.code,
                        reason = withheld.reason.as_deref().unwrap_or_default(),
                        room_id = withheld.room_id.as_deref().unwrap_or_default(),
                        to_user_id = %withheld.to_user_id,
                        to_device_id = %withheld.to_device_id,
                        "a client withheld a room's keys from an appservice's device"
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.record_key_withheld(appservice_id, &withheld.code);
                    }
                    health.last_key_withheld = Some(crate::store::KeyWithheldRow {
                        at_ms: now,
                        sender: withheld.sender,
                        code: withheld.code,
                        reason: withheld.reason,
                        room_id: withheld.room_id,
                        to_user_id: withheld.to_user_id,
                        to_device_id: withheld.to_device_id,
                    });
                }
                self.registry.store().put_health(appservice_id, &health)?;
                Ok(DrainOutcome::Delivered {
                    count: batch.len(),
                    through_seq,
                })
            }
            Err(err) => {
                let reason = err.kind;
                if let Some(metrics) = &self.metrics {
                    metrics.record_failed(appservice_id, reason.as_str());
                    metrics.record_attempt_seconds(
                        appservice_id,
                        if reason == SendErrorKind::TimedOut {
                            "timed_out"
                        } else {
                            "failed"
                        },
                        elapsed.as_secs_f64(),
                    );
                }
                if reason == SendErrorKind::TimedOut {
                    self.change_item_limit(appservice_id, |limit| limit / 2);
                }
                // The clock after the attempt: a timeout is a minute after `now`.
                let after = self.now_ms();
                let first_failed_at_ms = batch
                    .iter()
                    .filter_map(|e| e.first_failed_at_ms)
                    .min()
                    .unwrap_or(after);
                let spent = attempt >= self.config.max_attempts;
                let give_up = if reason.is_transient() {
                    spent
                        && after.saturating_sub(first_failed_at_ms) >= self.config.give_up_after_ms
                } else {
                    spent
                };
                let mut wait = backoff_ms(&self.config, attempt);
                if reason == SendErrorKind::TimedOut {
                    wait = wait.max(self.config.timeout_backoff_ms);
                }
                // The entries of one transaction share its fate: one attempt count, one first
                // failure, one id, and dead-lettered together or not at all.
                let attempted: Vec<QueuedTransaction> = batch
                    .iter()
                    .cloned()
                    .map(|mut entry| {
                        entry.attempts = attempt;
                        entry.last_error = Some(err.message.clone());
                        entry.txn_id = Some(through_seq);
                        entry.first_failed_at_ms = Some(first_failed_at_ms);
                        if give_up {
                            entry.status = QueueStatus::DeadLettered;
                        } else {
                            entry.next_attempt_at_ms = after.saturating_add(wait);
                        }
                        entry
                    })
                    .collect();
                let dead_lettered = if give_up { attempted.len() } else { 0 };
                let id = appservice_id.to_owned();
                self.off_the_runtime(move |registry| {
                    registry.store().put_queue_entries(&id, &attempted)
                })
                .await?;
                if let Some(metrics) = &self.metrics {
                    metrics.record_dead_lettered(appservice_id, dead_lettered);
                }
                let mut health = self.registry.store().health(appservice_id)?;
                health.consecutive_failures = health.consecutive_failures.saturating_add(1);
                health.last_error = Some(err.message.clone());
                health.last_failure_at_ms = Some(after);
                self.registry.store().put_health(appservice_id, &health)?;
                Ok(DrainOutcome::Failed {
                    count: batch.len(),
                    dead_lettered,
                    error: err.message,
                    reason,
                    txn_id: through_seq,
                    attempt,
                    elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                    retry_in_ms: (!give_up).then_some(wait),
                })
            }
        }
    }

    /// Resets matching dead-lettered entries for immediate retry: clears their backoff, error
    /// and transaction id and gives them a fresh retry budget. Matches the admin API's
    /// `POST /appservices/{id}/replay`.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `appservice_id` is not registered.
    pub fn replay(
        &self,
        appservice_id: &str,
        request: &ReplayRequest,
    ) -> Result<usize, AppserviceError> {
        if self.registry.get(appservice_id)?.is_none() {
            return Err(AppserviceError::NotFound(appservice_id.to_string()));
        }
        // By heads, a bounded batch of bodies at a time: a bridge that has been off for a day
        // can have tens of thousands dead-lettered.
        let replayed = self.registry.store().requeue_dead_lettered(
            appservice_id,
            |seq, enqueued_at_ms| request.matches_head(seq, enqueued_at_ms),
            self.now_ms(),
        )?;
        tracing::info!(
            appservice = %appservice_id,
            replayed,
            transaction_ids = request.transaction_ids.len(),
            since_ms = request.since_ms,
            "dead-lettered transactions were put back in an appservice's queue"
        );
        Ok(replayed)
    }

    /// Deletes matching dead-lettered entries: what the bridge was never sent and an operator
    /// has decided it should not be. Entries still waiting are never touched. Matches the admin
    /// API's `POST /appservices/{id}/discard`.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `appservice_id` is not registered.
    pub fn discard(
        &self,
        appservice_id: &str,
        request: &DiscardRequest,
    ) -> Result<usize, AppserviceError> {
        if self.registry.get(appservice_id)?.is_none() {
            return Err(AppserviceError::NotFound(appservice_id.to_string()));
        }
        let discarded = self
            .registry
            .store()
            .discard_dead_lettered(appservice_id, |seq, enqueued_at_ms| {
                request.matches_head(seq, enqueued_at_ms)
            })?;
        tracing::info!(
            appservice = %appservice_id,
            discarded,
            transaction_ids = request.transaction_ids.len(),
            before_ms = request.before_ms,
            "dead-lettered transactions of an appservice were discarded"
        );
        Ok(discarded)
    }
}

/// One attempt [`Scheduler::drain`] hands [`Scheduler::deliver_batch`].
struct Attempt {
    batch: Vec<QueuedTransaction>,
    through_seq: u64,
    merged: Value,
    attempt: u32,
    retry: bool,
    now: u64,
}

/// A recording [`TransactionSender`] double for tests: always succeeds, or fails the next N
/// calls, configurable per call.
#[derive(Clone, Default)]
pub struct MockSender {
    inner: Arc<std::sync::Mutex<MockSenderState>>,
}

#[derive(Default)]
struct MockSenderState {
    calls: Vec<(String, String, Value)>,
    fail_next: usize,
    fail_kind: Option<SendErrorKind>,
}

impl MockSender {
    /// A sender that always succeeds until told otherwise.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next `n` `send` calls fail as [`SendErrorKind::Rejected`] ("mock failure"),
    /// which spends an entry's retry budget by attempts.
    pub fn fail_next(&self, n: usize) {
        self.fail_next_as(n, SendErrorKind::Rejected);
    }

    /// Makes the next `n` `send` calls fail as `kind` ("mock failure").
    pub fn fail_next_as(&self, n: usize, kind: SendErrorKind) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.fail_next = n;
        state.fail_kind = Some(kind);
    }

    /// Every call recorded so far, as `(url, txn_id, body)`.
    #[must_use]
    pub fn calls(&self) -> Vec<(String, String, Value)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .calls
            .clone()
    }
}

#[async_trait]
impl TransactionSender for MockSender {
    async fn send(
        &self,
        url: &str,
        _hs_token: &str,
        txn_id: &str,
        body: &Value,
    ) -> Result<(), SendError> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .calls
            .push((url.to_string(), txn_id.to_string(), body.clone()));
        if state.fail_next > 0 {
            state.fail_next -= 1;
            Err(SendError::new(
                state.fail_kind.unwrap_or(SendErrorKind::Rejected),
                "mock failure",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::{NamespaceRule, Namespaces};
    use crate::registration::Registration;
    use hs_auth::clock::FixedClock;
    use hs_kv::memory::MemoryBackend;
    use ruma::server_name;
    use serde_json::json;

    fn registry_with(
        id: &str,
        url: Option<&str>,
    ) -> (Arc<Registry<MemoryBackend>>, Arc<FixedClock>) {
        let clock = Arc::new(FixedClock::new(1_000_000));
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), server_name!("example.org"))
                .unwrap()
                .with_clock(clock.clone()),
        );
        let reg = Registration {
            id: id.to_string(),
            url: url.map(str::to_string),
            as_token: format!("as_{id}"),
            hs_token: format!("hs_{id}"),
            sender_localpart: format!("{id}bot"),
            rate_limited: true,
            namespaces: Namespaces {
                users: vec![
                    NamespaceRule::compile(&format!("^@{id}_.*:example\\.org$"), true).unwrap(),
                ],
                aliases: vec![],
                rooms: vec![],
            },
            protocols: vec![],
            receive_ephemeral: true,
            push_ephemeral_legacy: false,
            msc3202: true,
            msc4190: false,
            extra: Default::default(),
        };
        registry.add(&reg).unwrap();
        (registry, clock)
    }

    fn event_txn(body: &str) -> Transaction {
        Transaction {
            events: vec![json!({"type": "m.room.message", "body": body})],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn drain_delivers_a_single_pending_transaction() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry.clone(), clock, Arc::new(sender.clone()));

        let seq = scheduler.enqueue("a", &event_txn("hi")).unwrap();
        assert_eq!(seq, Some(1));

        let outcome = scheduler.drain("a").await.unwrap();
        assert_eq!(
            outcome,
            DrainOutcome::Delivered {
                count: 1,
                through_seq: 1
            }
        );
        let calls = sender.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "1");
        assert_eq!(calls[0].2["events"][0]["body"], "hi");

        // Delivered entries are removed from the queue.
        assert!(registry.store().queue_for("a").unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_delivery_attempt_is_a_trace_of_its_own_and_an_empty_drain_is_not() {
        hs_telemetry::testing::install();
        let (registry, clock) = registry_with("traced", Some("http://bridge.local"));
        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry.clone(), clock, Arc::new(sender.clone()));
        scheduler.enqueue("traced", &event_txn("hi")).unwrap();
        scheduler.drain("traced").await.unwrap();
        let traces = hs_telemetry::testing::traces_rooted_at("appservice.transaction");
        let attempt = traces
            .iter()
            .filter_map(|t| t.find("appservice.transaction"))
            .find(|s| s.attr("appservice") == Some("traced"))
            .unwrap_or_else(|| panic!("no transaction span: {traces:?}"));
        assert_eq!(attempt.attr("outcome"), Some("delivered"));
        assert_eq!(attempt.attr("entries"), Some("1"));
        assert_eq!(attempt.attr("events"), Some("1"));
        assert_eq!(attempt.parent_span_id, "0000000000000000", "a root");

        sender.fail_next(1);
        scheduler.enqueue("traced", &event_txn("again")).unwrap();
        scheduler.drain("traced").await.unwrap();
        let failed = hs_telemetry::testing::traces_rooted_at("appservice.transaction")
            .iter()
            .filter_map(|t| t.find("appservice.transaction").cloned())
            .find(|s| s.attr("appservice") == Some("traced"))
            .expect("the failed attempt");
        assert_eq!(failed.attr("outcome"), Some("failed"));
        assert!(failed.failed);

        // Nothing queued: no trace.
        let _ = hs_telemetry::testing::traces_rooted_at("appservice.transaction");
        assert_eq!(
            scheduler.drain("traced").await.unwrap(),
            DrainOutcome::Waiting
        );
        assert!(
            hs_telemetry::testing::traces_rooted_at("appservice.transaction")
                .iter()
                .all(|t| t
                    .find("appservice.transaction")
                    .is_none_or(|s| s.attr("appservice") != Some("traced")))
        );
    }

    #[tokio::test]
    async fn null_url_registration_is_never_enqueued_or_pushed_to() {
        let (registry, clock) = registry_with("dp", None);
        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry.clone(), clock, Arc::new(sender.clone()));

        let seq = scheduler
            .enqueue("dp", &event_txn("should never be sent"))
            .unwrap();
        assert_eq!(seq, None);
        assert!(registry.store().queue_for("dp").unwrap().is_empty());

        let outcome = scheduler.drain("dp").await.unwrap();
        assert_eq!(outcome, DrainOutcome::NoUrl);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn ordered_delivery_batches_consecutive_ready_entries() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry, clock, Arc::new(sender.clone()));

        scheduler.enqueue("a", &event_txn("1")).unwrap();
        scheduler.enqueue("a", &event_txn("2")).unwrap();
        scheduler.enqueue("a", &event_txn("3")).unwrap();

        let outcome = scheduler.drain("a").await.unwrap();
        assert_eq!(
            outcome,
            DrainOutcome::Delivered {
                count: 3,
                through_seq: 3
            }
        );
        let calls = sender.calls();
        assert_eq!(calls.len(), 1);
        let events = calls[0].2["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["body"], "1");
        assert_eq!(events[2]["body"], "3");
    }

    #[tokio::test]
    async fn failure_schedules_backoff_and_does_not_dead_letter_early() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        sender.fail_next(1);
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender.clone()))
            .with_config(SchedulerConfig {
                max_attempts: 3,
                base_backoff_ms: 1000,
                max_backoff_ms: 60_000,
                ..Default::default()
            });

        scheduler.enqueue("a", &event_txn("1")).unwrap();
        let outcome = scheduler.drain("a").await.unwrap();
        assert!(
            matches!(
                &outcome,
                DrainOutcome::Failed {
                    count: 1,
                    dead_lettered: 0,
                    error,
                    reason: SendErrorKind::Rejected,
                    txn_id: 1,
                    attempt: 1,
                    retry_in_ms: Some(1000),
                    ..
                } if error == "mock failure"
            ),
            "{outcome:?}"
        );

        // Immediately draining again does nothing: still backing off.
        assert_eq!(scheduler.drain("a").await.unwrap(), DrainOutcome::Waiting);

        // After the backoff elapses, it retries and succeeds.
        clock.advance(1500);
        let outcome = scheduler.drain("a").await.unwrap();
        assert_eq!(
            outcome,
            DrainOutcome::Delivered {
                count: 1,
                through_seq: 1
            }
        );
        assert_eq!(sender.calls().len(), 2);
    }

    #[tokio::test]
    async fn exhausting_retries_dead_letters_and_replay_recovers() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        sender.fail_next(10);
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender.clone()))
            .with_config(SchedulerConfig {
                max_attempts: 2,
                base_backoff_ms: 1,
                max_backoff_ms: 1,
                ..Default::default()
            });

        scheduler.enqueue("a", &event_txn("1")).unwrap();
        scheduler.drain("a").await.unwrap(); // attempt 1: fails, retriable
        clock.advance(10);
        let outcome = scheduler.drain("a").await.unwrap(); // attempt 2: fails, dead-lettered
        assert!(
            matches!(
                outcome,
                DrainOutcome::Failed {
                    count: 1,
                    dead_lettered: 1,
                    attempt: 2,
                    retry_in_ms: None,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(scheduler.drain("a").await.unwrap(), DrainOutcome::Empty);

        let replayed = scheduler.replay("a", &ReplayRequest::default()).unwrap();
        assert_eq!(replayed, 1);

        sender.fail_next(0);
        let outcome = scheduler.drain("a").await.unwrap();
        assert!(matches!(outcome, DrainOutcome::Delivered { count: 1, .. }));
    }

    #[tokio::test]
    async fn paused_appservice_enqueues_but_never_drains() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        registry.pause("a").unwrap();
        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry.clone(), clock, Arc::new(sender.clone()));

        let seq = scheduler.enqueue("a", &event_txn("1")).unwrap();
        assert_eq!(seq, Some(1));
        assert_eq!(scheduler.drain("a").await.unwrap(), DrainOutcome::Paused);
        assert!(sender.calls().is_empty());

        registry.resume("a").unwrap();
        let outcome = scheduler.drain("a").await.unwrap();
        assert!(matches!(outcome, DrainOutcome::Delivered { .. }));
    }

    #[tokio::test]
    async fn restart_resumes_pending_delivery() {
        let backend = MemoryBackend::new();
        let clock = Arc::new(FixedClock::new(1_000_000));
        let registry = Arc::new(
            Registry::open(backend.clone(), server_name!("example.org"))
                .unwrap()
                .with_clock(clock.clone()),
        );
        let reg = Registration {
            id: "a".to_string(),
            url: Some("http://bridge.local".to_string()),
            as_token: "as_a".to_string(),
            hs_token: "hs_a".to_string(),
            sender_localpart: "abot".to_string(),
            rate_limited: true,
            namespaces: Namespaces::default(),
            protocols: vec![],
            receive_ephemeral: false,
            push_ephemeral_legacy: false,
            msc3202: false,
            msc4190: false,
            extra: Default::default(),
        };
        registry.add(&reg).unwrap();

        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry, clock.clone(), Arc::new(sender.clone()));
        scheduler
            .enqueue("a", &event_txn("survives a restart"))
            .unwrap();

        // Simulate a process restart: open a brand new Registry/Scheduler over the same
        // (cloned-handle) backend, as a fresh process would over the same on-disk store.
        let registry2 = Arc::new(
            Registry::open(backend, server_name!("example.org"))
                .unwrap()
                .with_clock(clock.clone()),
        );
        let sender2 = MockSender::new();
        let scheduler2 = Scheduler::new(registry2, clock, Arc::new(sender2.clone()));

        let outcome = scheduler2.drain("a").await.unwrap();
        assert!(matches!(outcome, DrainOutcome::Delivered { count: 1, .. }));
        assert_eq!(
            sender2.calls()[0].2["events"][0]["body"],
            "survives a restart"
        );
    }

    #[test]
    fn merge_json_concatenates_arrays_and_deep_merges_objects() {
        let a = json!({"events": [1, 2], "device_lists": {"changed": ["x"]}});
        let b = json!({"events": [3], "device_lists": {"changed": ["y"], "left": ["z"]}});
        let merged = merge_json(&a, &b);
        assert_eq!(merged["events"], json!([1, 2, 3]));
        assert_eq!(merged["device_lists"]["changed"], json!(["x", "y"]));
        assert_eq!(merged["device_lists"]["left"], json!(["z"]));
    }

    /// The demo on 2026-10-12: a bridge whose transaction handler is stuck accepts the
    /// connection and never answers. The request times out, and both the failure the drain
    /// reports (which the delivery worker logs) and the appservice's `last_error` say that it
    /// timed out, not only reqwest's "error sending request for url (...)".
    #[tokio::test]
    async fn a_bridge_that_never_answers_is_reported_as_timed_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        let (registry, clock) = registry_with("stuck", Some(&url));
        let sender = HttpTransactionSender::with_timeout(std::time::Duration::from_millis(300));
        let scheduler = Scheduler::new(registry.clone(), clock, Arc::new(sender));
        scheduler.enqueue("stuck", &event_txn("hello?")).unwrap();
        let outcome = scheduler.drain("stuck").await.unwrap();
        held.abort();
        let DrainOutcome::Failed { error, reason, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(reason, SendErrorKind::TimedOut, "{error}");
        assert!(
            error.starts_with("error sending request for url"),
            "{error}"
        );
        assert!(error.contains("timed out"), "{error}");
        let health = registry.health("stuck").unwrap();
        assert!(
            health
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("timed out")),
            "{:?}",
            health.last_error
        );
    }

    fn entry(registry: &Registry<MemoryBackend>, id: &str) -> Vec<QueuedTransaction> {
        registry.store().queue_for(id).unwrap()
    }

    /// The spec's idempotency contract: a failed transaction is sent again under the same
    /// `txnId` with exactly the same contents, even when more has been queued meanwhile. Before
    /// 2026-10-15 the retry re-formed the batch, and a batch that had grown went under a new id
    /// with the old events in it.
    #[tokio::test]
    async fn a_retry_sends_the_same_entries_under_the_same_txn_id_whatever_arrived_since() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        sender.fail_next_as(1, SendErrorKind::Unreachable);
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender.clone()))
            .with_config(SchedulerConfig {
                base_backoff_ms: 1000,
                ..Default::default()
            });
        scheduler.enqueue("a", &event_txn("1")).unwrap();
        scheduler.enqueue("a", &event_txn("2")).unwrap();
        let failed = scheduler.drain("a").await.unwrap();
        assert!(
            matches!(
                failed,
                DrainOutcome::Failed {
                    txn_id: 2,
                    count: 2,
                    ..
                }
            ),
            "{failed:?}"
        );
        // Two more arrive while it backs off.
        scheduler.enqueue("a", &event_txn("3")).unwrap();
        scheduler.enqueue("a", &event_txn("4")).unwrap();
        clock.advance(1000);
        assert_eq!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Delivered {
                count: 2,
                through_seq: 2
            }
        );
        // Then the new ones, under an id of their own.
        assert_eq!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Delivered {
                count: 2,
                through_seq: 4
            }
        );
        let calls = sender.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].1, "2");
        assert_eq!(calls[1].1, "2", "the retry reuses the id");
        assert_eq!(calls[0].2, calls[1].2, "and the contents");
        assert_eq!(calls[2].1, "4");
        assert_eq!(calls[2].2["events"][0]["body"], "3");
    }

    /// A bridge that is down, restarting or busy is retried for a day, not ten attempts: on
    /// 2026-10-15 the demo's Slack bridge was restarting for five minutes and its transaction
    /// was dead-lettered after four.
    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_has_failed_for_a_day() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        sender.fail_next_as(usize::MAX, SendErrorKind::Unreachable);
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender.clone()))
            .with_config(SchedulerConfig {
                max_attempts: 3,
                base_backoff_ms: 1000,
                max_backoff_ms: 60 * 60 * 1000,
                give_up_after_ms: 10 * 60 * 60 * 1000,
                ..Default::default()
            });
        scheduler.enqueue("a", &event_txn("1")).unwrap();
        let mut attempts = 0;
        loop {
            attempts += 1;
            match scheduler.drain("a").await.unwrap() {
                DrainOutcome::Failed {
                    dead_lettered: 0,
                    retry_in_ms: Some(wait),
                    reason: SendErrorKind::Unreachable,
                    ..
                } => clock.advance(wait),
                DrainOutcome::Failed {
                    dead_lettered: 1,
                    retry_in_ms: None,
                    ..
                } => break,
                other => panic!("{other:?}"),
            }
            assert!(attempts < 100, "never gave up");
        }
        assert!(attempts > 3, "past max_attempts: {attempts}");
        let entries = entry(&registry, "a");
        assert_eq!(entries[0].status, QueueStatus::DeadLettered);
        let failing_for = clock.now_ms() - entries[0].first_failed_at_ms.unwrap();
        assert!(failing_for >= 10 * 60 * 60 * 1000, "{failing_for}");
        // Every attempt was the same transaction.
        assert!(sender.calls().iter().all(|(_, id, _)| id == "1"));
    }

    /// A bridge that answers and refuses (a 400, say) is not going to change its mind about
    /// this transaction: it is dead-lettered after `max_attempts`, so as not to hold up the
    /// rest for a day.
    #[tokio::test]
    async fn a_rejected_transaction_is_dead_lettered_after_max_attempts() {
        assert_eq!(SendErrorKind::of_status(400), SendErrorKind::Rejected);
        assert_eq!(SendErrorKind::of_status(404), SendErrorKind::Rejected);
        for status in [408, 429, 500, 502, 503] {
            assert_eq!(SendErrorKind::of_status(status), SendErrorKind::Unavailable);
        }
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        sender.fail_next_as(usize::MAX, SendErrorKind::Rejected);
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender))
            .with_config(SchedulerConfig {
                max_attempts: 3,
                ..Default::default()
            });
        scheduler.enqueue("a", &event_txn("1")).unwrap();
        for attempt in 1..=3 {
            let outcome = scheduler.drain("a").await.unwrap();
            let DrainOutcome::Failed {
                dead_lettered,
                attempt: n,
                ..
            } = outcome
            else {
                panic!("{outcome:?}");
            };
            assert_eq!(n, attempt);
            assert_eq!(dead_lettered, usize::from(attempt == 3));
            clock.advance(60_000);
        }
    }

    /// A timeout means the bridge took the request and may still be working on it: the retry
    /// waits at least `timeout_backoff_ms`, and new transactions go out smaller until it keeps
    /// up again.
    #[tokio::test]
    async fn a_timeout_waits_before_its_retry_and_shrinks_new_transactions_until_a_delivery() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender.clone()))
            .with_config(SchedulerConfig {
                max_items: 8,
                timeout_backoff_ms: 60_000,
                ..Default::default()
            })
            .with_metrics(AppserviceMetrics::default());
        for n in 0..12 {
            scheduler.enqueue("a", &event_txn(&n.to_string())).unwrap();
        }
        sender.fail_next_as(1, SendErrorKind::TimedOut);
        let outcome = scheduler.drain("a").await.unwrap();
        let DrainOutcome::Failed {
            count,
            retry_in_ms,
            txn_id,
            ..
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(count, 8, "eight items at most");
        assert_eq!(retry_in_ms, Some(60_000), "not the 500 ms backoff");
        assert_eq!(scheduler.item_limit("a"), 4);
        clock.advance(59_000);
        assert_eq!(scheduler.drain("a").await.unwrap(), DrainOutcome::Waiting);
        clock.advance(1_000);
        // The retry is the same eight, under the same id.
        assert_eq!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Delivered {
                count: 8,
                through_seq: txn_id
            }
        );
        // The delivery doubled the limit back.
        assert_eq!(scheduler.item_limit("a"), 8);

        sender.fail_next_as(2, SendErrorKind::TimedOut);
        // The rest of the first twelve: four, failing twice.
        assert!(matches!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Failed { count: 4, .. }
        ));
        for n in 12..30 {
            scheduler.enqueue("a", &event_txn(&n.to_string())).unwrap();
        }
        clock.advance(60_000);
        assert!(matches!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Failed { count: 4, .. }
        ));
        assert_eq!(scheduler.item_limit("a"), 2);
        clock.advance(120_000);
        assert!(matches!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Delivered { count: 4, .. }
        ));
        // New transactions: four items (the limit doubled from two), then eight.
        assert!(matches!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Delivered { count: 4, .. }
        ));
        assert!(matches!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Delivered { count: 8, .. }
        ));
    }

    /// An entry is one room's page of up to a hundred events: twenty of them made transactions
    /// of over a thousand events (the demo, 2026-10-15). A new transaction stops before its
    /// items pass `max_items`; an entry larger than that goes alone.
    #[tokio::test]
    async fn a_transaction_carries_at_most_max_items_and_a_larger_entry_goes_alone() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        let scheduler = Scheduler::new(registry.clone(), clock, Arc::new(sender.clone()));
        let page = |n: usize| Transaction {
            events: (0..n)
                .map(|i| json!({"type": "m.room.message", "body": i}))
                .collect(),
            ..Default::default()
        };
        for n in [60, 30, 30, 150, 5] {
            scheduler.enqueue("a", &page(n)).unwrap();
        }
        while let DrainOutcome::Delivered { .. } = scheduler.drain("a").await.unwrap() {}
        let sizes: Vec<usize> = sender
            .calls()
            .iter()
            .map(|(_, _, body)| body["events"].as_array().map_or(0, Vec::len))
            .collect();
        assert_eq!(sizes, vec![90, 30, 150, 5]);
    }

    /// The demo on 2026-10-15: Discord's 260 dead letters were replayed, each kept its spent
    /// attempt count, so each batch was sent once, timed out while the bridge went on working
    /// through it, and was dead-lettered again. A replay now gives a fresh budget.
    #[tokio::test]
    async fn a_replayed_entry_gets_a_fresh_retry_budget() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        sender.fail_next(1);
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender.clone()))
            .with_config(SchedulerConfig {
                max_attempts: 1,
                ..Default::default()
            });
        scheduler.enqueue("a", &event_txn("1")).unwrap();
        assert!(matches!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Failed {
                dead_lettered: 1,
                ..
            }
        ));
        assert_eq!(scheduler.replay("a", &ReplayRequest::default()).unwrap(), 1);
        let replayed = &entry(&registry, "a")[0];
        assert_eq!(replayed.attempts, 0);
        assert_eq!(replayed.txn_id, None);
        assert_eq!(replayed.first_failed_at_ms, None);
        sender.fail_next_as(1, SendErrorKind::TimedOut);
        let outcome = scheduler.drain("a").await.unwrap();
        assert!(
            matches!(
                outcome,
                DrainOutcome::Failed {
                    dead_lettered: 0,
                    ..
                }
            ),
            "a timeout after a replay is retried: {outcome:?}"
        );
        clock.advance(60_000);
        assert!(matches!(
            scheduler.drain("a").await.unwrap(),
            DrainOutcome::Delivered { .. }
        ));
    }

    #[tokio::test]
    async fn discard_deletes_the_matching_dead_letters_and_nothing_that_waits() {
        let (registry, clock) = registry_with("a", Some("http://bridge.local"));
        let sender = MockSender::new();
        sender.fail_next(3);
        let scheduler = Scheduler::new(registry.clone(), clock.clone(), Arc::new(sender))
            .with_config(SchedulerConfig {
                max_attempts: 1,
                max_batch: 1,
                ..Default::default()
            });
        for n in 1..=4 {
            scheduler.enqueue("a", &event_txn(&n.to_string())).unwrap();
            clock.advance(1_000);
        }
        for _ in 0..3 {
            scheduler.drain("a").await.unwrap();
        }
        // 1, 2 and 3 dead-lettered; 4 waits.
        assert_eq!(
            registry.store().queue_summary("a").unwrap().dead_lettered,
            3
        );
        let discarded = scheduler
            .discard(
                "a",
                &DiscardRequest {
                    transaction_ids: vec!["2".to_owned()],
                    before_ms: None,
                },
            )
            .unwrap();
        assert_eq!(discarded, 1);
        // Everything queued before 4 was: 1 and 3.
        let discarded = scheduler
            .discard(
                "a",
                &DiscardRequest {
                    transaction_ids: vec![],
                    before_ms: Some(1_000_000 + 3_500),
                },
            )
            .unwrap();
        assert_eq!(discarded, 2);
        let left: Vec<(u64, QueueStatus)> = entry(&registry, "a")
            .iter()
            .map(|e| (e.seq, e.status))
            .collect();
        assert_eq!(left, vec![(4, QueueStatus::Pending)]);
        let summary = registry.store().queue_summary("a").unwrap();
        assert_eq!((summary.pending, summary.dead_lettered), (1, 0));
        assert!(matches!(
            scheduler.discard("nope", &DiscardRequest::default()),
            Err(AppserviceError::NotFound(_))
        ));
    }

    #[test]
    fn merge_json_scalar_collision_takes_the_later_value() {
        let a = json!({"counts": {"alice": {"DEV": {"signed_curve25519": 3}}}});
        let b = json!({"counts": {"alice": {"DEV": {"signed_curve25519": 9}}}});
        let merged = merge_json(&a, &b);
        assert_eq!(merged["counts"]["alice"]["DEV"]["signed_curve25519"], 9);
    }
}
