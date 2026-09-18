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
//! `txnId` is its highest sequence number, so retrying the *same* failed batch (nothing in it has
//! changed status yet) reuses the same `txnId`, honoring the spec's idempotency contract; a
//! different composition (because delivery succeeded and moved on, or more entries arrived)
//! legitimately gets a new one.
//!
//! # Persistence and restart
//!
//! There is no in-memory queue: every entry [`crate::registry::Registry::add`]-adjacent enqueue
//! calls create lives in [`crate::store::AppserviceStore`]'s `hs_appservice.txn_queue` keyspace
//! from the moment it is created, with its status, attempt count and next-retry time all part of
//! the same row. A fresh [`Scheduler`] opened against the same backend after a restart sees
//! exactly the same pending and dead-lettered entries with exactly the same backoff clocks — there
//! is no separate "resume" step to get wrong, because there was never a volatile copy to lose. See
//! [`tests::restart_resumes_pending_delivery`].

use std::sync::Arc;

use async_trait::async_trait;
use hs_auth::clock::Clock;
use hs_kv::KvBackend;
use serde_json::Value;

use crate::error::AppserviceError;
use crate::registry::Registry;
use crate::store::{QueueStatus, QueuedTransaction};
use crate::transaction::Transaction;

/// Tuning for [`Scheduler::drain`].
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    /// Maximum number of ready, consecutive pending entries merged into one HTTP transaction.
    pub max_batch: usize,
    /// Delivery attempts (including the first) before an entry is dead-lettered.
    pub max_attempts: u32,
    /// Backoff before the second attempt; doubles each subsequent retry up to `max_backoff_ms`.
    pub base_backoff_ms: u64,
    /// Backoff never grows past this.
    pub max_backoff_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_batch: 20,
            max_attempts: 10,
            base_backoff_ms: 500,
            max_backoff_ms: 5 * 60 * 1000,
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

/// Delivers a transaction body to one appservice over HTTP. A trait so tests (and
/// `hs-bridge-conformance`) can substitute an in-process double for [`HttpTransactionSender`].
#[async_trait]
pub trait TransactionSender: Send + Sync {
    /// Sends `body` as transaction `txn_id` to `url`, authenticating with `hs_token`
    /// (`Authorization: Bearer <hs_token>`, matching the spec's appservice push auth).
    ///
    /// # Errors
    /// Returns a human-readable error describing the failure (network, non-2xx status, timeout).
    async fn send(
        &self,
        url: &str,
        hs_token: &str,
        txn_id: &str,
        body: &Value,
    ) -> Result<(), String>;
}

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
    /// A sender with a 30-second request timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
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
    ) -> Result<(), String> {
        let base = url.trim_end_matches('/');
        let encoded_txn = urlencoding_minimal(txn_id);
        let full_url = format!("{base}/_matrix/app/v1/transactions/{encoded_txn}");
        let response = self
            .client
            .put(&full_url)
            .bearer_auth(hs_token)
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            Err(format!("{status}: {text}"))
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
        /// How many of them were dead-lettered by this attempt (reached `max_attempts`).
        dead_lettered: usize,
        /// The delivery error.
        error: String,
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
    fn matches(&self, entry: &QueuedTransaction) -> bool {
        let id_ok = self.transaction_ids.is_empty()
            || self.transaction_ids.contains(&entry.seq.to_string());
        let since_ok = self
            .since_ms
            .is_none_or(|since| entry.enqueued_at_ms >= since);
        id_ok && since_ok
    }
}

/// The per-appservice transaction scheduler.
pub struct Scheduler<B: KvBackend> {
    registry: Arc<Registry<B>>,
    clock: Arc<dyn Clock>,
    sender: Arc<dyn TransactionSender>,
    config: SchedulerConfig,
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
        }
    }

    /// Overrides the default [`SchedulerConfig`].
    #[must_use]
    pub fn with_config(mut self, config: SchedulerConfig) -> Self {
        self.config = config;
        self
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
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
        if row.url.is_none() || txn.is_empty() {
            return Ok(None);
        }
        let body = txn.to_wire_json(
            row.receive_ephemeral,
            row.push_ephemeral_legacy,
            row.msc3202,
        );
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
    pub async fn drain(&self, appservice_id: &str) -> Result<DrainOutcome, AppserviceError> {
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

        let mut pending: Vec<QueuedTransaction> = self
            .registry
            .store()
            .queue_for(appservice_id)?
            .into_iter()
            .filter(|e| e.status == QueueStatus::Pending)
            .collect();
        pending.sort_by_key(|e| e.seq);

        if pending.is_empty() {
            return Ok(DrainOutcome::Empty);
        }

        let now = self.now_ms();
        if pending[0].next_attempt_at_ms > now {
            return Ok(DrainOutcome::Waiting);
        }

        // A contiguous prefix, stopping at the first not-yet-ready entry (ordering rule) or at
        // `max_batch`.
        let mut batch = Vec::new();
        for entry in pending {
            if entry.next_attempt_at_ms > now || batch.len() >= self.config.max_batch {
                break;
            }
            batch.push(entry);
        }

        let through_seq = batch.last().map(|e| e.seq).unwrap_or(0);
        let merged = batch
            .iter()
            .map(|e| e.body.clone())
            .reduce(|a, b| merge_json(&a, &b))
            .unwrap_or(Value::Object(serde_json::Map::new()));

        let txn_id = through_seq.to_string();
        let outcome = self
            .sender
            .send(&url, &row.hs_token, &txn_id, &merged)
            .await;

        match outcome {
            Ok(()) => {
                for entry in &batch {
                    self.registry
                        .store()
                        .delete_queue_entry(appservice_id, entry.seq)?;
                }
                let mut health = self.registry.store().health(appservice_id)?;
                health.consecutive_failures = 0;
                health.last_error = None;
                health.last_success_at_ms = Some(now);
                health.last_success_seq = Some(through_seq);
                self.registry.store().put_health(appservice_id, &health)?;
                Ok(DrainOutcome::Delivered {
                    count: batch.len(),
                    through_seq,
                })
            }
            Err(err) => {
                let mut dead_lettered = 0usize;
                for mut entry in batch.clone() {
                    entry.attempts += 1;
                    entry.last_error = Some(err.clone());
                    if entry.attempts >= self.config.max_attempts {
                        entry.status = QueueStatus::DeadLettered;
                        dead_lettered += 1;
                    } else {
                        entry.next_attempt_at_ms = now + backoff_ms(&self.config, entry.attempts);
                    }
                    self.registry
                        .store()
                        .put_queue_entry(appservice_id, &entry)?;
                }
                let mut health = self.registry.store().health(appservice_id)?;
                health.consecutive_failures = health.consecutive_failures.saturating_add(1);
                health.last_error = Some(err.clone());
                self.registry.store().put_health(appservice_id, &health)?;
                Ok(DrainOutcome::Failed {
                    count: batch.len(),
                    dead_lettered,
                    error: err,
                })
            }
        }
    }

    /// Resets matching dead-lettered (and still-pending) entries for immediate retry: clears
    /// their backoff and error, keeping their attempt count (so a replayed entry that fails again
    /// still respects `max_attempts` rather than retrying forever). Matches the admin API's
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
        let now = self.now_ms();
        let mut replayed = 0usize;
        for mut entry in self.registry.store().queue_for(appservice_id)? {
            if entry.status != QueueStatus::DeadLettered || !request.matches(&entry) {
                continue;
            }
            entry.status = QueueStatus::Pending;
            entry.next_attempt_at_ms = now;
            entry.last_error = None;
            self.registry
                .store()
                .put_queue_entry(appservice_id, &entry)?;
            replayed += 1;
        }
        Ok(replayed)
    }
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
}

impl MockSender {
    /// A sender that always succeeds until told otherwise.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next `n` `send` calls fail.
    pub fn fail_next(&self, n: usize) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fail_next = n;
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
    ) -> Result<(), String> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .calls
            .push((url.to_string(), txn_id.to_string(), body.clone()));
        if state.fail_next > 0 {
            state.fail_next -= 1;
            Err("mock failure".to_string())
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
        assert_eq!(
            outcome,
            DrainOutcome::Failed {
                count: 1,
                dead_lettered: 0,
                error: "mock failure".to_string()
            }
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
        assert_eq!(
            outcome,
            DrainOutcome::Failed {
                count: 1,
                dead_lettered: 1,
                error: "mock failure".to_string()
            }
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

    #[test]
    fn merge_json_scalar_collision_takes_the_later_value() {
        let a = json!({"counts": {"alice": {"DEV": {"signed_curve25519": 3}}}});
        let b = json!({"counts": {"alice": {"DEV": {"signed_curve25519": 9}}}});
        let merged = merge_json(&a, &b);
        assert_eq!(merged["counts"]["alice"]["DEV"]["signed_curve25519"], 9);
    }
}
