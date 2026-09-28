//! The outbound federation sender: one queue per destination server, drained into
//! `PUT /_matrix/federation/v1/send/{txnId}` transactions through [`FederationClient`].
//!
//! # What this closes
//!
//! Before this module existed, nothing on this server ever sent a locally created event to
//! another server. The inbound half of `/send` (`crate::inbound`), the resident side of the join
//! handshake (`crate::join`) and the client side of it (`crate::outbound_join`) were all real, and
//! a remote server that joined a room here was then told nothing about anything that happened in
//! it afterwards. [`FederationSender`] is the missing half: hand it a PDU and the servers that
//! should receive it, and each of those servers gets it in a signed transaction, in order, retried
//! until it is accepted -- across restarts of this server.
//!
//! # Shape
//!
//! - **The store is the source of truth; the channels are the fast path.** A PDU is written to
//!   the [`OutboundStore`] (one row per destination, one sequence number) before it is handed to
//!   any worker, and deleted only once its destination has accepted it or this server's own
//!   policy has refused it. With [`crate::outbound_store::KvOutboundStore`] (what `hs serve`
//!   uses) that is the same
//!   backend the room's events live in; with [`InMemoryOutboundStore`] (what
//!   [`FederationSender::new`] uses) the sender is as volatile as it was before persistence.
//! - **One worker per destination**, spawned the first time anything is queued for it -- or by
//!   [`FederationSender::resume`] at start, for every destination the store still holds a queue
//!   for. A worker first drains what the store holds for its destination (the previous run's
//!   backlog, oldest first), then follows its channel; a PDU it already delivered from the store
//!   is recognised by its sequence number when its channel copy arrives, and skipped. It drains
//!   into transactions of at most [`MAX_PDUS_PER_TRANSACTION`] PDUs (the spec's resource limit,
//!   the same constant `crate::inbound` enforces on receipt), each sent through
//!   [`FederationClient::send`] -- so discovery, TLS and CA trust, `X-Matrix` signing, the
//!   per-destination concurrency limit and the client's destination backoff records all apply
//!   exactly as they do to every other outbound call.
//! - **Per-destination ordering is preserved.** A transaction is retried, with the same
//!   transaction ID (so a receiver that did process it but whose response was lost replays its
//!   cached answer -- `crate::inbound::TransactionStore`'s contract), until it succeeds; nothing
//!   queued behind it is sent first. Destinations are independent: a failing destination delays
//!   nothing but its own queue. Across a restart the transaction ID is new (the receiver may
//!   see the same PDUs twice, which is harmless: an event it holds is not applied again).
//! - **The wait between attempts is persisted too.** Every failed attempt records, per
//!   destination, how many times in a row the head transaction has failed, what went wrong and
//!   when the next attempt is due ([`OutboundDestinationState`]); a worker that starts after a
//!   restart waits out what is left of that rather than trying at once, and its backoff carries
//!   on doubling from where it was. On [`ClientError::Backoff`] -- the client's own
//!   connection-level judgement, kept in `crate::destination_store` -- the worker sleeps until
//!   that says the destination may be tried again. Every wait is in slices of at most
//!   [`BACKOFF_POLL_INTERVAL`], re-reading the store between slices, so an administrator's reset
//!   ([`FederationSender::reset_destination`], `federation.destinations.reset`) takes effect
//!   promptly. The doubling delay itself is [`SenderConfig`]; jitter is left to the client's
//!   store, which jitters the connection-level backoff.
//! - **A per-PDU `error` in a 200 response is final.** The receiver looked at that event and
//!   rejected it; sending it again would get the same answer. It is logged at `warn` and not
//!   retried, matching what the spec says a receiver's per-PDU result means.
//! - **Nothing is ever queued for this server's own name**, whatever a caller passes.
//!
//! # What this is not, said loudly
//!
//! **No catch-up from the room.** What survives a restart is what was queued: a PDU the feeder
//! (`hs_cli::federation_sender`) never handed over -- because the process died between the
//! event's persistence and the queueing, or because the feeder fell behind the update stream --
//! is not sent. Synapse's `destination_rooms` table (the last stream position successfully sent
//! per destination, so a whole outage is caught up from the room's own history) is the
//! behavioural reference for closing that, and is the next step, not this one.
//!
//! # EDUs
//!
//! [`FederationSender::enqueue_edu`] queues an EDU (typing, receipts, presence, device-list
//! updates) for each destination alongside its PDUs: every transaction carries up to
//! [`MAX_EDUS_PER_TRANSACTION`] of them (the spec's limit, and Synapse's), with whatever PDUs are
//! waiting, and a destination with only EDUs waiting gets a transaction of only EDUs. Unlike PDUs
//! they are **in memory only** -- an EDU describes a moment, and one delivered after a restart
//! would mostly describe a moment that has passed -- and each destination keeps at most
//! [`MAX_QUEUED_EDUS_PER_DESTINATION`], dropping the oldest, so a destination that is down for a
//! day does not hold a day of typing notices. An EDU queued with a coalescing key replaces the
//! unsent one with the same key (a typing or presence update supersedes the previous one). An
//! EDU for a destination another replica sends for is dropped, not stored: the replica that owns
//! it has its own users' EDUs to send, and this one's are not worth a shared table.
//! To-device messages, which must not be dropped, are not sent this way.
//!
//! **Only `/send`.** Invites (`PUT /invite`), leaves and knocks against a remote resident
//! (`make_leave`/`send_leave`, `make_knock`/`send_knock`) are separate handshakes, not
//! transactions, and are not initiated here.
//!
//! # In a cluster
//!
//! A [`SendGate`] says which destinations *this* process sends for ([`SendsEverywhere`] unless
//! [`FederationSender::set_gate`] is given another; `hs-cli` gives it one over `hs-cluster`
//! ownership of `ShardLayout::federation_shard(destination)`). A PDU for a destination the gate
//! refuses is still written to the store -- in a cluster the store is shared, and the replica
//! that owns the destination's shard sends from it -- but no worker is started here, so two
//! replicas never drain one queue. [`FederationSender::resume`] starts workers for the
//! destinations the gate allows (the caller runs it again on acquiring a shard), and
//! [`FederationSender::stop_workers_not_sent_here`] stops those it no longer does (on releasing
//! or losing one), leaving their rows in the store for the new owner. A worker that is idle
//! looks at the store again every [`SenderConfig::store_rescan_interval`], if one is set, which
//! is how rows another replica wrote for a destination this one already sends for are found;
//! without one (single-node, the default) the store only ever holds what this process wrote
//! and the channels are enough. The pending counts are this process's workers' alone.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::client::{ClientError, FederationClient};
use crate::inbound::{MAX_EDUS_PER_TRANSACTION, MAX_PDUS_PER_TRANSACTION};
use crate::outbound_store::{
    InMemoryOutboundStore, OutboundDestinationState, OutboundStore, OutboundStoreError,
};

/// Where an accepted event goes to reach other servers. Implemented by [`FederationSender`];
/// defined as a trait so the transport server (`crate::transport::FederationState`) and the
/// resident side of the join handshake (`crate::join::send_join`) can be tested with a recording
/// sink and no real client.
pub trait OutboundPduSink: Send + Sync {
    /// Queues `pdu` for delivery to every server in `destinations`. Never blocks and never
    /// fails: a destination this server must not or cannot reach is dealt with by the worker
    /// that owns its queue, and logged there.
    fn enqueue_pdu(&self, destinations: Vec<String>, pdu: Value);
}

/// The most EDUs a destination's queue holds; past it the oldest is dropped. See the module
/// docs' "EDUs".
pub const MAX_QUEUED_EDUS_PER_DESTINATION: usize = 5_000;

/// How a destination's worker waits between failed attempts at one transaction. See the module
/// docs for which failures this applies to and which are governed by the destination store
/// instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderConfig {
    /// The wait after the first failure; each further failure doubles it.
    pub initial_backoff: Duration,
    /// The longest wait between attempts. [`FederationSender::new`] sets this to the client's own
    /// `max_retry_backoff` (`hs-config::FederationConfig::max_retry_backoff`), so the two backoffs
    /// an operator can observe on a destination have the same ceiling.
    pub max_backoff: Duration,
    /// How often a worker that is waiting out a backoff re-reads the store to see whether an
    /// administrator has reset it. [`BACKOFF_POLL_INTERVAL`] by default; tests shorten it.
    pub reset_poll_interval: Duration,
    /// How long an idle worker waits on its channel before looking at the store again for rows
    /// it was not handed -- what another replica wrote for its destination, in a cluster over a
    /// shared store. `None` (the default) never looks: a single process's store holds only what
    /// its own channels already carried.
    pub store_rescan_interval: Option<Duration>,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(3600),
            reset_poll_interval: BACKOFF_POLL_INTERVAL,
            store_rescan_interval: None,
        }
    }
}

/// Which destinations this process sends for. See the module docs' "In a cluster".
pub trait SendGate: Send + Sync {
    /// Whether a worker for `destination` belongs in this process right now.
    fn sends_here(&self, destination: &str) -> bool;
}

/// The single-process gate: every destination is sent for here.
pub struct SendsEverywhere;

impl SendGate for SendsEverywhere {
    fn sends_here(&self, _destination: &str) -> bool {
        true
    }
}

impl SenderConfig {
    /// The default policy with its ceiling taken from `client`'s `max_retry_backoff`, which is
    /// what [`FederationSender::new`] uses.
    #[must_use]
    pub fn for_client(client: &FederationClient) -> Self {
        Self {
            max_backoff: client.max_retry_backoff(),
            ..Self::default()
        }
    }
}

/// The default [`SenderConfig::reset_poll_interval`]: the longest a worker sleeps in one go
/// while waiting to retry, whatever the wait is. The stores are re-read after each slice: an
/// administrator who resets a destination's backoff (`hs_federation::admin_source`) gets a retry
/// within this long, not at the end of a possibly hour-long wait. Re-reading costs store lookups
/// and no network traffic (`FederationClient::send` refuses before resolving anything).
pub const BACKOFF_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// The outbound sender. See the module docs. Cheap to share behind an `Arc`; every method takes
/// `&self`.
pub struct FederationSender {
    shared: Arc<Shared>,
    queues: Mutex<HashMap<String, DestinationQueue>>,
}

/// What every destination worker holds on to: everything but the queue map, so that dropping the
/// [`FederationSender`] (which owns the map, and with it every queue's sending half) ends each
/// worker's `recv` loop instead of keeping the sender alive through a reference cycle.
struct Shared {
    client: Arc<FederationClient>,
    own_server_name: String,
    config: SenderConfig,
    store: Arc<dyn OutboundStore>,
    gate: RwLock<Arc<dyn SendGate>>,
    /// Transaction IDs are `{started_ms}-{counter}`: unique across restarts (a later start has a
    /// later prefix) and monotonic within one (the counter only grows), which is what the spec
    /// asks of a `txnId` per `(origin, destination)` pair.
    started_ms: u64,
    txn_counter: AtomicU64,
    /// PDUs queued and not yet accepted or dropped, across every destination.
    pending_total: AtomicUsize,
    shut_down: AtomicBool,
    /// Where accepted EDUs are counted, once installed ([`FederationSender::install_edu_metrics`]).
    edu_metrics: std::sync::OnceLock<crate::metrics::EduMetrics>,
}

struct DestinationQueue {
    tx: mpsc::UnboundedSender<Queued>,
    pending: Arc<AtomicUsize>,
    edus: Arc<EduQueue>,
    worker: tokio::task::AbortHandle,
}

/// What a destination's channel carries.
enum Queued {
    /// A PDU, with the sequence number the store gave it (what lets the worker tell a copy of
    /// something it already sent from the store).
    Pdu { seq: u64, pdu: Arc<Value> },
    /// "There are EDUs in the queue": the doorbell for an idle worker. The EDUs themselves are
    /// in [`EduQueue`], where a newer one can replace an older one before either is sent.
    Edus,
}

/// One destination's unsent EDUs, oldest first, each with its coalescing key.
#[derive(Default)]
struct EduQueue {
    queue: Mutex<std::collections::VecDeque<(Option<String>, Arc<Value>)>>,
}

impl EduQueue {
    /// Adds `edu`, replacing an unsent one with the same key, and dropping the oldest past
    /// [`MAX_QUEUED_EDUS_PER_DESTINATION`]. Returns how many were dropped that way (0 or 1).
    fn push(&self, key: Option<String>, edu: Arc<Value>) -> usize {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = &key
            && let Some(position) = queue
                .iter()
                .position(|(existing, _)| existing.as_ref() == Some(key))
        {
            queue.remove(position);
        }
        queue.push_back((key, edu));
        let mut dropped = 0;
        while queue.len() > MAX_QUEUED_EDUS_PER_DESTINATION {
            queue.pop_front();
            dropped += 1;
        }
        dropped
    }

    /// Takes up to [`MAX_EDUS_PER_TRANSACTION`] of the oldest.
    fn take(&self) -> Vec<Arc<Value>> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let n = queue.len().min(MAX_EDUS_PER_TRANSACTION);
        queue.drain(..n).map(|(_, edu)| edu).collect()
    }

    fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// What a failed attempt asks the worker to do before the next one.
enum Wait {
    /// Sleep this long, then try again: the client's own backoff said no, and it is the client
    /// that will say when (so the sleep is one slice and the attempt is the check).
    For(Duration),
    /// Wait until this time, watching the store for a reset meanwhile.
    Until(u64),
}

/// How one transaction's delivery ended.
enum Delivery {
    /// The destination answered 2xx (per-PDU rejections, if any, have been logged).
    Delivered,
    /// This server's own policy forbids the destination (federation disabled, domain not in the
    /// allowlist, resolved address in a blocked range); retrying cannot change that, so the
    /// transaction was dropped and logged.
    Dropped,
    /// [`FederationSender::shutdown`] was called while retrying.
    ShutDown,
}

impl FederationSender {
    /// A sender for `own_server_name` over `client`, waiting between failed attempts with
    /// [`SenderConfig::for_client`], and keeping its queues in memory only: what is queued is
    /// lost with the process. `hs serve` uses [`FederationSender::with_store`] instead.
    #[must_use]
    pub fn new(client: Arc<FederationClient>, own_server_name: impl Into<String>) -> Self {
        let config = SenderConfig::for_client(&client);
        Self::with_config(client, own_server_name, config)
    }

    /// [`FederationSender::new`] with an explicit retry policy. Tests use this to make a failing
    /// destination retry in milliseconds rather than seconds.
    #[must_use]
    pub fn with_config(
        client: Arc<FederationClient>,
        own_server_name: impl Into<String>,
        config: SenderConfig,
    ) -> Self {
        Self::with_store(
            client,
            own_server_name,
            config,
            Arc::new(InMemoryOutboundStore::new()),
        )
    }

    /// A sender whose queues and retry state live in `store`. With a durable store (a
    /// [`crate::outbound_store::KvOutboundStore`]) what is queued survives a restart: call
    /// [`FederationSender::resume`] on the new sender to pick it up.
    #[must_use]
    pub fn with_store(
        client: Arc<FederationClient>,
        own_server_name: impl Into<String>,
        config: SenderConfig,
        store: Arc<dyn OutboundStore>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                client,
                own_server_name: own_server_name.into(),
                config,
                store,
                gate: RwLock::new(Arc::new(SendsEverywhere)),
                started_ms: now_ms(),
                txn_counter: AtomicU64::new(0),
                pending_total: AtomicUsize::new(0),
                shut_down: AtomicBool::new(false),
                edu_metrics: std::sync::OnceLock::new(),
            }),
            queues: Mutex::new(HashMap::new()),
        }
    }

    /// Counts every EDU in a transaction a destination accepts into `metrics`
    /// (`hs_federation_edus_sent_total`). A second install is ignored.
    pub fn install_edu_metrics(&self, metrics: crate::metrics::EduMetrics) {
        if self.shared.edu_metrics.set(metrics).is_err() {
            tracing::warn!("EDU metrics were already installed on this sender; ignoring");
        }
    }

    /// The server name every transaction this sender builds carries as `origin`.
    #[must_use]
    pub fn own_server_name(&self) -> &str {
        &self.shared.own_server_name
    }

    /// Replaces the gate that says which destinations this process sends for (see the module
    /// docs' "In a cluster"). Takes effect for workers started from now on; the caller runs
    /// [`FederationSender::resume`] and [`FederationSender::stop_workers_not_sent_here`] to
    /// bring the existing ones in line.
    pub fn set_gate(&self, gate: Arc<dyn SendGate>) {
        *self
            .shared
            .gate
            .write()
            .unwrap_or_else(PoisonError::into_inner) = gate;
    }

    /// Stops the worker of every destination the gate no longer allows here, leaving what they
    /// had queued in the store for whichever replica sends for it now. Returns those
    /// destinations. What `hs-cli` does on releasing or losing a federation shard.
    pub fn stop_workers_not_sent_here(&self) -> Vec<String> {
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        let released: Vec<String> = queues
            .keys()
            .filter(|destination| !self.shared.sends_here(destination))
            .cloned()
            .collect();
        for destination in &released {
            if let Some(queue) = queues.remove(destination) {
                queue.worker.abort();
                let left = queue.pending.swap(0, Ordering::AcqRel);
                sub_saturating(&self.shared.pending_total, left);
                tracing::info!(
                    destination,
                    left_in_store = left,
                    "this replica no longer sends for the destination; its worker is stopped"
                );
            }
        }
        released
    }

    /// Whether what this sender queues outlives the process ([`OutboundStore::durable`]).
    #[must_use]
    pub fn is_durable(&self) -> bool {
        self.shared.store.durable()
    }

    /// Restores a worker for every destination the store still holds a queue for and the gate
    /// allows here, so a previous run's unsent PDUs go out, in order, from here. Returns how
    /// many PDUs were waiting. Idempotent: a destination that already has a worker is left
    /// alone. Must be called from within a Tokio runtime, like
    /// [`FederationSender::enqueue_pdu`].
    ///
    /// # Errors
    /// Returns the store's error if the queues cannot be read; nothing was started then.
    pub fn resume(&self) -> Result<usize, OutboundStoreError> {
        if self.shared.shut_down.load(Ordering::Acquire) {
            return Ok(0);
        }
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        let backlog = self.shared.store.queued()?;
        let mut resumed = 0usize;
        for (destination, count) in backlog {
            if destination == self.shared.own_server_name
                || queues.contains_key(&destination)
                || !self.shared.sends_here(&destination)
            {
                continue;
            }
            if let Some(queue) = spawn_worker(&self.shared, &destination, count) {
                tracing::info!(
                    destination,
                    pdus = count,
                    "resuming an outbound federation queue left by a previous run"
                );
                resumed += count;
                queues.insert(destination, queue);
            }
        }
        Ok(resumed)
    }

    /// Queues `pdu` for each server in `destinations` (deduplicated; this server's own name is
    /// always skipped), starting a destination's worker the first time it is named -- if the
    /// gate allows the destination here; otherwise the PDU is only written to the store, for
    /// the replica that sends for it. The PDU is in the store before this returns.
    ///
    /// Must be called from within a Tokio runtime, since a new destination's worker is spawned on
    /// the current one; outside a runtime the PDU is logged and dropped rather than panicking.
    /// After [`FederationSender::shutdown`] every call is a logged no-op, as is a store that
    /// refuses the write (logged at `error`: that is a failing disk, not a normal path).
    pub fn enqueue_pdu(&self, destinations: impl IntoIterator<Item = String>, pdu: Value) {
        if self.shared.shut_down.load(Ordering::Acquire) {
            tracing::debug!("outbound federation sender is shut down; dropping a PDU");
            return;
        }
        let mut seen = HashSet::new();
        let destinations: Vec<String> = destinations
            .into_iter()
            .filter(|destination| {
                !destination.is_empty()
                    && *destination != self.shared.own_server_name
                    && seen.insert(destination.clone())
            })
            .collect();
        if destinations.is_empty() {
            return;
        }
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        // Workers first, counting whatever the store already holds for a destination that has
        // none yet (a queue `resume` was not asked about), and only then the write: under the
        // one lock, so a PDU is counted exactly once, either as backlog or as this enqueue.
        let mut targets = Vec::with_capacity(destinations.len());
        for destination in destinations {
            if !queues.contains_key(&destination) && self.shared.sends_here(&destination) {
                let backlog = match self.shared.store.queue_len(&destination) {
                    Ok(count) => count,
                    Err(error) => {
                        tracing::error!(destination, %error, "cannot read the outbound queue");
                        0
                    }
                };
                match spawn_worker(&self.shared, &destination, backlog) {
                    Some(queue) => {
                        queues.insert(destination.clone(), queue);
                    }
                    None => continue,
                }
            }
            targets.push(destination);
        }
        if targets.is_empty() {
            return;
        }
        let seq = match self.shared.store.enqueue(&targets, &pdu) {
            Ok(seq) => seq,
            Err(error) => {
                tracing::error!(
                    destinations = ?targets,
                    %error,
                    "could not persist an outbound PDU; it will not be sent"
                );
                return;
            }
        };
        let pdu = Arc::new(pdu);
        for destination in targets {
            let Some(queue) = queues.get(&destination) else {
                tracing::debug!(
                    destination,
                    seq,
                    "queued a PDU for a destination another replica sends for"
                );
                continue;
            };
            queue.pending.fetch_add(1, Ordering::AcqRel);
            self.shared.pending_total.fetch_add(1, Ordering::AcqRel);
            let queued = Queued::Pdu {
                seq,
                pdu: pdu.clone(),
            };
            if queue.tx.send(queued).is_err() {
                // The worker is gone (aborted by `shutdown`, racing this call). Undo the count;
                // the shut-down check at the top makes this a narrow window, not a normal path.
                // The row stays in the store for the next start.
                queue.pending.fetch_sub(1, Ordering::AcqRel);
                self.shared.pending_total.fetch_sub(1, Ordering::AcqRel);
                tracing::debug!(
                    destination,
                    "outbound federation worker is gone; the PDU waits in the store"
                );
            }
        }
    }

    /// Queues an EDU (`{"edu_type": edu_type, "content": content}`) for each server in
    /// `destinations` (deduplicated; this server's own name is always skipped), to go out with
    /// the next transaction to each. With a `coalesce_key`, it replaces an unsent EDU with the same
    /// key for the same destination. In memory only; see the module docs' "EDUs" for what is kept,
    /// what is dropped, and why. Like [`FederationSender::enqueue_pdu`] it must be called from
    /// within a Tokio runtime and is a no-op after [`FederationSender::shutdown`].
    pub fn enqueue_edu(
        &self,
        destinations: impl IntoIterator<Item = String>,
        edu_type: &str,
        content: Value,
        coalesce_key: Option<String>,
    ) {
        if self.shared.shut_down.load(Ordering::Acquire) {
            tracing::debug!(
                edu_type,
                "outbound federation sender is shut down; dropping an EDU"
            );
            return;
        }
        let edu = Arc::new(serde_json::json!({"edu_type": edu_type, "content": content}));
        let mut seen = HashSet::new();
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        for destination in destinations {
            if destination.is_empty()
                || destination == self.shared.own_server_name
                || !seen.insert(destination.clone())
            {
                continue;
            }
            if !queues.contains_key(&destination) {
                if !self.shared.sends_here(&destination) {
                    tracing::debug!(
                        destination,
                        edu_type,
                        "dropping an EDU for a destination another replica sends for"
                    );
                    continue;
                }
                let backlog = match self.shared.store.queue_len(&destination) {
                    Ok(count) => count,
                    Err(error) => {
                        tracing::error!(destination, %error, "cannot read the outbound queue");
                        0
                    }
                };
                match spawn_worker(&self.shared, &destination, backlog) {
                    Some(queue) => {
                        queues.insert(destination.clone(), queue);
                    }
                    None => continue,
                }
            }
            let Some(queue) = queues.get(&destination) else {
                continue;
            };
            let dropped = queue.edus.push(coalesce_key.clone(), edu.clone());
            if dropped > 0 {
                tracing::warn!(
                    destination,
                    dropped,
                    "a destination's EDU queue is full; dropped the oldest"
                );
            }
            if queue.tx.send(Queued::Edus).is_err() {
                tracing::debug!(
                    destination,
                    "outbound federation worker is gone; EDU not sent"
                );
            }
        }
    }

    /// EDUs queued for `destination` and not yet in a transaction it accepted. Zero for a
    /// destination nothing has been queued for.
    #[must_use]
    pub fn pending_edus_for(&self, destination: &str) -> usize {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(destination)
            .map_or(0, |queue| queue.edus.len())
    }

    /// PDUs queued and not yet accepted by (or dropped for) their destination, summed over every
    /// destination. What the admin overview reports as pending.
    #[must_use]
    pub fn pending_pdus(&self) -> usize {
        self.shared.pending_total.load(Ordering::Acquire)
    }

    /// PDUs queued for one destination and not yet accepted or dropped. Zero for a destination
    /// nothing has been queued for.
    #[must_use]
    pub fn pending_pdus_for(&self, destination: &str) -> usize {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(destination)
            .map_or(0, |queue| queue.pending.load(Ordering::Acquire))
    }

    /// Every destination a worker exists for, with its pending count, sorted by name.
    #[must_use]
    pub fn pending_by_destination(&self) -> Vec<(String, usize)> {
        let mut all: Vec<(String, usize)> = self
            .queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(name, queue)| (name.clone(), queue.pending.load(Ordering::Acquire)))
            .collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all
    }

    /// The persisted retry state of every destination a transaction was ever attempted for,
    /// sorted by name. What the admin API shows alongside the client's own backoff records.
    ///
    /// # Errors
    /// Returns the store's error.
    pub fn destination_states(
        &self,
    ) -> Result<Vec<(String, OutboundDestinationState)>, OutboundStoreError> {
        self.shared.store.states()
    }

    /// One destination's persisted retry state, if a transaction was ever attempted for it.
    ///
    /// # Errors
    /// Returns the store's error.
    pub fn destination_state(
        &self,
        destination: &str,
    ) -> Result<Option<OutboundDestinationState>, OutboundStoreError> {
        self.shared.store.state(destination)
    }

    /// Forgets `destination`'s persisted backoff, so its worker -- which re-reads the store
    /// between slices of its wait -- tries again within [`BACKOFF_POLL_INTERVAL`]. What
    /// `federation.destinations.reset` does, together with the client's own store.
    ///
    /// # Errors
    /// Returns the store's error.
    pub fn reset_destination(&self, destination: &str) -> Result<(), OutboundStoreError> {
        self.shared.store.reset(destination)
    }

    /// Stops every worker at once. What is still queued stays in a durable store for the next
    /// start (logged at `info` with the count); in an in-memory store it is lost (logged at
    /// `warn`). Idempotent; `enqueue_pdu` is a no-op afterwards.
    pub fn shutdown(&self) {
        self.shared.shut_down.store(true, Ordering::Release);
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        for (_, queue) in queues.drain() {
            queue.worker.abort();
        }
        let left = self.shared.pending_total.swap(0, Ordering::AcqRel);
        if left > 0 {
            if self.shared.store.durable() {
                tracing::info!(
                    queued = left,
                    "outbound federation sender stopped with PDUs still queued; they are kept \
                     for the next start"
                );
            } else {
                tracing::warn!(
                    lost = left,
                    "outbound federation sender stopped with PDUs still queued; they are lost \
                     (this sender's queue is in memory only)"
                );
            }
        }
    }
}

impl Drop for FederationSender {
    fn drop(&mut self) {
        // A worker mid-retry holds only `Shared`, not this struct, so without this it would keep
        // retrying its current transaction after the sender that owned it was gone.
        for (_, queue) in self
            .queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
        {
            queue.worker.abort();
        }
    }
}

impl OutboundPduSink for FederationSender {
    fn enqueue_pdu(&self, destinations: Vec<String>, pdu: Value) {
        FederationSender::enqueue_pdu(self, destinations, pdu);
    }
}

/// Starts `destination`'s worker with `backlog` PDUs already in the store for it (counted as
/// pending from the start; the worker sends them first).
fn spawn_worker(
    shared: &Arc<Shared>,
    destination: &str,
    backlog: usize,
) -> Option<DestinationQueue> {
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => {
            tracing::error!(
                destination,
                "outbound federation sender used outside a Tokio runtime; dropping a PDU"
            );
            return None;
        }
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let pending = Arc::new(AtomicUsize::new(backlog));
    let edus = Arc::new(EduQueue::default());
    shared.pending_total.fetch_add(backlog, Ordering::AcqRel);
    let worker = runtime
        .spawn(run_worker(
            shared.clone(),
            destination.to_owned(),
            pending.clone(),
            edus.clone(),
            rx,
        ))
        .abort_handle();
    Some(DestinationQueue {
        tx,
        pending,
        edus,
        worker,
    })
}

/// One destination's loop: first what the store holds for it (a previous run's backlog, or
/// what was written before this worker's first look), then its channel; each batch delivered,
/// acknowledged in the store, repeat. With a [`SenderConfig::store_rescan_interval`], an idle
/// channel sends it back to the store that often. Ends when the queue's sending half is dropped
/// (the sender was dropped) or on [`Delivery::ShutDown`].
async fn run_worker(
    shared: Arc<Shared>,
    destination: String,
    pending: Arc<AtomicUsize>,
    edus: Arc<EduQueue>,
    mut rx: mpsc::UnboundedReceiver<Queued>,
) {
    // Everything with a sequence number up to here has left the store; a channel copy of it is
    // a duplicate.
    let mut acked_through: u64 = 0;
    loop {
        // What the store holds, oldest first, until it holds nothing.
        loop {
            let batch = match shared.store.peek(&destination, MAX_PDUS_PER_TRANSACTION) {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::error!(
                        destination,
                        %error,
                        "cannot read the outbound queue; sending what arrives from here on"
                    );
                    break;
                }
            };
            let Some(last) = batch.last() else {
                break;
            };
            let through = last.seq;
            let pdus: Vec<Arc<Value>> = batch.into_iter().map(|row| Arc::new(row.pdu)).collect();
            if let Delivery::ShutDown = shared.send_batch(&destination, &pdus, &edus.take()).await {
                return;
            }
            shared.settle(&destination, through, pdus.len(), &pending);
            acked_through = through;
        }
        // Then the channel, until it closes or has been quiet for a rescan interval.
        loop {
            // EDUs left over from a full transaction, or queued while one was being sent (their
            // doorbells may already have been consumed), go out before waiting for anything.
            if edus.len() > 0 {
                if let Delivery::ShutDown = shared.send_batch(&destination, &[], &edus.take()).await
                {
                    return;
                }
                continue;
            }
            let first = match shared.config.store_rescan_interval {
                None => rx.recv().await,
                Some(interval) => match tokio::time::timeout(interval, rx.recv()).await {
                    Ok(received) => received,
                    Err(_elapsed) => break,
                },
            };
            let Some(first) = first else {
                return;
            };
            // PDUs from the channel, up to a transaction's worth, skipping any already sent
            // from the store; EDU doorbells need nothing here (the queue is read below).
            let mut batch: Vec<(u64, Arc<Value>)> = Vec::new();
            let mut next = Some(first);
            while let Some(queued) = next.take() {
                if let Queued::Pdu { seq, pdu } = queued
                    && seq > acked_through
                {
                    batch.push((seq, pdu));
                }
                if batch.len() >= MAX_PDUS_PER_TRANSACTION {
                    break;
                }
                next = rx.try_recv().ok();
            }
            let edu_batch = edus.take();
            if batch.is_empty() && edu_batch.is_empty() {
                continue;
            }
            let through = batch.last().map_or(acked_through, |(seq, _)| *seq);
            let pdus: Vec<Arc<Value>> = batch.into_iter().map(|(_, pdu)| pdu).collect();
            if let Delivery::ShutDown = shared.send_batch(&destination, &pdus, &edu_batch).await {
                return;
            }
            if !pdus.is_empty() {
                shared.settle(&destination, through, pdus.len(), &pending);
                acked_through = through;
            }
        }
    }
}

impl Shared {
    fn sends_here(&self, destination: &str) -> bool {
        self.gate
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .sends_here(destination)
    }

    fn next_txn_id(&self) -> String {
        let n = self.txn_counter.fetch_add(1, Ordering::AcqRel) + 1;
        format!("{}-{n}", self.started_ms)
    }

    /// The wait after `failures` consecutive failed attempts at one transaction:
    /// `initial_backoff * 2^(failures - 1)`, capped at `max_backoff`.
    fn backoff(&self, failures: u32) -> Duration {
        let exponent = failures.saturating_sub(1).min(20);
        self.config
            .initial_backoff
            .saturating_mul(1u32 << exponent)
            .min(self.config.max_backoff)
    }

    /// One transaction, as a new transaction ID, until it is delivered or dropped.
    async fn send_batch(
        &self,
        destination: &str,
        pdus: &[Arc<Value>],
        edus: &[Arc<Value>],
    ) -> Delivery {
        let txn_id = self.next_txn_id();
        self.deliver(destination, &txn_id, pdus, edus).await
    }

    /// Takes a delivered (or dropped) batch out of the store and the pending counts.
    fn settle(&self, destination: &str, through: u64, count: usize, pending: &AtomicUsize) {
        let removed = match self.store.ack(destination, through) {
            Ok(removed) => removed,
            Err(error) => {
                tracing::error!(
                    destination,
                    through,
                    %error,
                    "could not remove delivered PDUs from the outbound queue; they may be sent \
                     again after a restart"
                );
                count
            }
        };
        // What the store had for this batch is what was counted pending for it: at enqueue, or
        // as backlog when the worker started. A row that no longer decoded was counted and
        // removed but never sent, and comes off here too.
        let settled = removed.max(count);
        sub_saturating(pending, settled);
        sub_saturating(&self.pending_total, settled);
    }

    /// The persisted retry state for `destination`, or the default; a store that cannot be read
    /// is logged and treated as empty.
    fn state_of(&self, destination: &str) -> OutboundDestinationState {
        match self.store.state(destination) {
            Ok(state) => state.unwrap_or_default(),
            Err(error) => {
                tracing::error!(destination, %error, "cannot read the outbound retry state");
                OutboundDestinationState::default()
            }
        }
    }

    /// Sleeps until `until_ms`, in slices of at most the configured poll interval, giving up
    /// early when the store no longer says to wait (an administrator's reset). Returns `false`
    /// if the sender was shut down meanwhile.
    async fn wait_until(&self, destination: &str, until_ms: u64) -> bool {
        let slice = self.config.reset_poll_interval;
        loop {
            if self.shut_down.load(Ordering::Acquire) {
                return false;
            }
            let now = now_ms();
            if now >= until_ms {
                return true;
            }
            let remaining = Duration::from_millis(until_ms - now);
            tokio::time::sleep(remaining.min(slice)).await;
            if remaining <= slice {
                return !self.shut_down.load(Ordering::Acquire);
            }
            if self.state_of(destination).is_ready(now_ms()) {
                tracing::info!(
                    destination,
                    "outbound backoff was reset while waiting; trying now"
                );
                return true;
            }
        }
    }

    fn record_failure(&self, destination: &str, error: &str, next_attempt_ms: u64) {
        if let Err(store_error) = self
            .store
            .record_failure(destination, error, next_attempt_ms)
        {
            tracing::error!(destination, error = %store_error, "cannot record an outbound failure");
        }
    }

    /// Sends one transaction until the destination accepts it, this server's own policy refuses
    /// it, or the sender is shut down. See the module docs for the retry rules.
    async fn deliver(
        &self,
        destination: &str,
        txn_id: &str,
        pdus: &[Arc<Value>],
        edus: &[Arc<Value>],
    ) -> Delivery {
        let path = format!("/_matrix/federation/v1/send/{txn_id}");
        let body = serde_json::json!({
            "origin": self.own_server_name,
            "origin_server_ts": now_ms(),
            "pdus": pdus.iter().map(|pdu| (**pdu).clone()).collect::<Vec<Value>>(),
            "edus": edus.iter().map(|edu| (**edu).clone()).collect::<Vec<Value>>(),
        });
        // Where a previous run left this destination: its run of failures carries on from
        // there, and what is left of its wait is waited out first.
        let persisted = self.state_of(destination);
        let mut failures: u32 = persisted.failures;
        if let Some(next_attempt) = persisted.next_attempt_ms
            && next_attempt > now_ms()
        {
            tracing::info!(
                destination,
                txn_id,
                failures,
                next_attempt_ms = next_attempt,
                "destination was failing before this start; waiting out its backoff"
            );
            if !self.wait_until(destination, next_attempt).await {
                return Delivery::ShutDown;
            }
        }
        loop {
            if self.shut_down.load(Ordering::Acquire) {
                return Delivery::ShutDown;
            }
            let wait = match self
                .client
                .send(destination, "PUT", &path, Some(&body))
                .await
            {
                Ok(response) if (200..300).contains(&response.status) => {
                    self.log_rejections(destination, txn_id, &response.body);
                    tracing::debug!(
                        destination,
                        txn_id,
                        pdus = pdus.len(),
                        edus = edus.len(),
                        "federation transaction accepted"
                    );
                    self.record_edus_sent(destination, txn_id, edus);
                    if let Err(error) = self.store.record_success(destination) {
                        tracing::error!(destination, %error, "cannot record an outbound success");
                    }
                    return Delivery::Delivered;
                }
                Ok(response) => {
                    failures += 1;
                    let delay = self.backoff(failures);
                    tracing::warn!(
                        destination,
                        txn_id,
                        status = response.status,
                        failures,
                        retry_in_ms = delay.as_millis() as u64,
                        "federation transaction rejected; will retry"
                    );
                    let until = now_ms().saturating_add(delay.as_millis() as u64);
                    let error = format!("HTTP {}: {}", response.status, response.body);
                    self.record_failure(destination, &error, until);
                    Wait::Until(until)
                }
                Err(ClientError::Backoff { retry_at_ms, .. }) => {
                    // The client's destination store's judgement, not this loop's: wait it out
                    // (in slices, so a reset of that store is noticed) without counting it as
                    // another failure of this transaction.
                    let remaining = Duration::from_millis(retry_at_ms.saturating_sub(now_ms()));
                    let floor = self.config.initial_backoff;
                    let ceiling = self.config.reset_poll_interval.max(floor);
                    let delay = remaining.max(floor).min(ceiling);
                    tracing::debug!(
                        destination,
                        txn_id,
                        retry_at_ms,
                        sleeping_ms = delay.as_millis() as u64,
                        "destination is backing off; waiting"
                    );
                    Wait::For(delay)
                }
                Err(
                    error @ (ClientError::Disabled
                    | ClientError::DomainDenied(_)
                    | ClientError::IpDenied(_)),
                ) => {
                    tracing::error!(
                        destination,
                        txn_id,
                        pdus = pdus.len(),
                        %error,
                        "federation transaction dropped: this server's own policy forbids the \
                         destination"
                    );
                    return Delivery::Dropped;
                }
                Err(error) => {
                    failures += 1;
                    let delay = self.backoff(failures);
                    tracing::warn!(
                        destination,
                        txn_id,
                        failures,
                        %error,
                        retry_in_ms = delay.as_millis() as u64,
                        "federation transaction failed; will retry"
                    );
                    let until = now_ms().saturating_add(delay.as_millis() as u64);
                    self.record_failure(destination, &error.to_string(), until);
                    Wait::Until(until)
                }
            };
            let keep_going = match wait {
                Wait::For(delay) => {
                    tokio::time::sleep(delay).await;
                    !self.shut_down.load(Ordering::Acquire)
                }
                Wait::Until(until) => self.wait_until(destination, until).await,
            };
            if !keep_going {
                return Delivery::ShutDown;
            }
        }
    }

    /// Logs (at debug) and counts each EDU of a transaction `destination` accepted.
    fn record_edus_sent(&self, destination: &str, txn_id: &str, edus: &[Arc<Value>]) {
        for edu in edus {
            let edu_type = edu
                .get("edu_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            tracing::debug!(destination, txn_id, edu_type, "EDU sent");
            if let Some(metrics) = self.edu_metrics.get() {
                metrics.record_sent(edu_type);
            }
        }
    }

    /// Logs every per-PDU `error` in an accepted transaction's response. Final, not retried: the
    /// receiver examined the event and refused it.
    fn log_rejections(&self, destination: &str, txn_id: &str, response: &Value) {
        let Some(results) = response.get("pdus").and_then(Value::as_object) else {
            return;
        };
        for (event_id, result) in results {
            if let Some(error) = result.get("error").and_then(Value::as_str)
                && !error.is_empty()
            {
                tracing::warn!(
                    destination,
                    txn_id,
                    event_id,
                    error,
                    "destination rejected a PDU; it will not be sent to that server again"
                );
            }
        }
    }
}

/// `counter -= n`, stopping at zero: a count that is only ever an operator's number must never
/// wrap.
fn sub_saturating(counter: &AtomicUsize, n: usize) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(n))
    });
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientConfig;
    use crate::destination_store::{DestinationState, DestinationStore, InMemoryDestinationStore};
    use crate::discovery::{AddrResolver, SrvResolver, WellKnownFetcher, WellKnownOutcome};
    use crate::outbound_store::KvOutboundStore;
    use async_trait::async_trait;
    use axum::extract::{Request, State};
    use axum::middleware::Next;
    use axum::response::Response;
    use hs_kv::memory::MemoryBackend;
    use hs_model::signing::SigningKeyPair;
    use hs_testkit::fake_federation::{CannedResponse, FakeFederationPeer};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;
    use tokio::net::TcpListener;

    /// Answers every hostname with `127.0.0.1` and no SRV records, so an explicit-port
    /// destination (`localhost:{port}`) reaches a fake peer bound on loopback -- the same shape
    /// `crate::client` and `crate::outbound_join`'s tests use.
    struct Loopback;
    #[async_trait]
    impl AddrResolver for Loopback {
        async fn resolve_addr(&self, _hostname: &str) -> Vec<IpAddr> {
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        }
    }
    #[async_trait]
    impl SrvResolver for Loopback {
        async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
            Vec::new()
        }
    }
    struct NoWellKnown;
    #[async_trait]
    impl WellKnownFetcher for NoWellKnown {
        async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
            WellKnownOutcome::Absent {
                cache_for: Duration::from_secs(60),
            }
        }
    }

    const US: &str = "us.example.org";

    fn client_with(destinations: Arc<dyn DestinationStore>) -> Arc<FederationClient> {
        Arc::new(FederationClient::new(
            US,
            SigningKeyPair::generate("a_1"),
            ClientConfig {
                scheme: "http",
                ..ClientConfig::default()
            },
            destinations,
            Arc::new(NoWellKnown),
            Arc::new(Loopback),
            Arc::new(Loopback),
        ))
    }

    fn client() -> Arc<FederationClient> {
        client_with(Arc::new(InMemoryDestinationStore::new()))
    }

    fn fast() -> SenderConfig {
        SenderConfig {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
            reset_poll_interval: Duration::from_millis(50),
            store_rescan_interval: None,
        }
    }

    type AuthLog = Arc<Mutex<Vec<Option<String>>>>;

    /// `FakeFederationPeer` records method, path and body but not headers; this layer in front
    /// of it keeps every request's `Authorization` header so the tests can see the `X-Matrix`
    /// signature went out.
    async fn record_authorization(
        State(log): State<AuthLog>,
        request: Request,
        next: Next,
    ) -> Response {
        let header = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        log.lock().unwrap().push(header);
        next.run(request).await
    }

    /// Binds `peer` on a fresh loopback port; returns the destination string a sender should use
    /// for it and the recorded `Authorization` headers.
    async fn spawn_peer(peer: &FakeFederationPeer) -> (String, AuthLog) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        spawn_peer_on(peer, listener)
    }

    /// [`spawn_peer`] on a socket the test already bound, for a peer that has to come up on a
    /// port a sender was already given.
    fn spawn_peer_on(peer: &FakeFederationPeer, listener: TcpListener) -> (String, AuthLog) {
        let port = listener.local_addr().unwrap().port();
        let log: AuthLog = Arc::new(Mutex::new(Vec::new()));
        let app = peer.router().layer(axum::middleware::from_fn_with_state(
            log.clone(),
            record_authorization,
        ));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("localhost:{port}"), log)
    }

    /// Polls `condition` every few milliseconds until it holds or `deadline` passes.
    async fn wait_for(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while !condition() {
            if start.elapsed() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }

    fn pdu(i: usize) -> Value {
        serde_json::json!({ "type": "m.room.message", "room_id": "!r:us.example.org", "i": i })
    }

    fn txn_id_of(path: &str) -> &str {
        path.rsplit('/').next().unwrap()
    }

    #[tokio::test]
    async fn three_pdus_go_out_as_one_signed_transaction() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        for i in 0..3 {
            sender.enqueue_pdu([destination.clone()], pdu(i));
        }
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 1).await);
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);

        let requests = peer.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let request = &requests[0];
        assert_eq!(request.method, "PUT");
        assert!(
            request.path.starts_with("/_matrix/federation/v1/send/"),
            "{}",
            request.path
        );
        assert_eq!(request.body["origin"], US);
        assert!(request.body["origin_server_ts"].as_u64().unwrap() > 0);
        let pdus = request.body["pdus"].as_array().unwrap();
        assert_eq!(pdus.len(), 3);
        for (i, pdu) in pdus.iter().enumerate() {
            assert_eq!(pdu["i"], i);
        }
        assert_eq!(request.body["edus"], serde_json::json!([]));

        let headers = auth.lock().unwrap().clone();
        assert_eq!(headers.len(), 1);
        let header = headers[0].as_deref().expect("an Authorization header");
        assert!(header.starts_with("X-Matrix "), "{header}");
        assert!(header.contains(&format!("origin=\"{US}\"")), "{header}");
    }

    #[tokio::test]
    async fn sixty_pdus_split_into_transactions_of_fifty_then_ten_in_order() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        // A current-thread runtime (`#[tokio::test]`'s default) cannot run the worker until this
        // task yields, so all sixty are queued before the first transaction is built.
        for i in 0..60 {
            sender.enqueue_pdu([destination.clone()], pdu(i));
        }
        assert_eq!(sender.pending_pdus(), 60);
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);

        let requests = peer.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        let first = requests[0].body["pdus"].as_array().unwrap();
        let second = requests[1].body["pdus"].as_array().unwrap();
        assert_eq!(first.len(), MAX_PDUS_PER_TRANSACTION);
        assert_eq!(second.len(), 10);
        let order: Vec<u64> = first
            .iter()
            .chain(second.iter())
            .map(|p| p["i"].as_u64().unwrap())
            .collect();
        assert_eq!(order, (0..60).collect::<Vec<u64>>());
        assert_ne!(
            txn_id_of(&requests[0].path),
            txn_id_of(&requests[1].path),
            "transaction IDs must differ"
        );
        assert!(txn_id_of(&requests[0].path) < txn_id_of(&requests[1].path));
    }

    #[tokio::test]
    async fn a_failing_destination_is_retried_in_order_after_waiting() {
        let peer = FakeFederationPeer::new("peer.example.org");
        peer.queue_response(CannedResponse::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"errcode": "M_UNKNOWN"}),
        ));
        peer.queue_response(CannedResponse::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"errcode": "M_UNKNOWN"}),
        ));
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        let started = Instant::now();
        sender.enqueue_pdu([destination.clone()], pdu(0));
        sender.enqueue_pdu([destination.clone()], pdu(1));
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        let elapsed = started.elapsed();

        let requests = peer.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        // The same transaction, same ID, until accepted: a receiver that did process an earlier
        // attempt replays its cached answer rather than applying the PDUs twice.
        assert_eq!(requests[0].path, requests[1].path);
        assert_eq!(requests[1].path, requests[2].path);
        assert_eq!(requests[0].body, requests[2].body);
        let pdus = requests[2].body["pdus"].as_array().unwrap();
        assert_eq!(pdus.len(), 2);
        assert_eq!(pdus[0]["i"], 0);
        assert_eq!(pdus[1]["i"], 1);
        // Waited, not spun: 100ms after the first failure, 200ms after the second.
        assert!(
            elapsed >= Duration::from_millis(300),
            "third attempt came too soon: {elapsed:?}"
        );
        // And once accepted, nothing more.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(peer.request_count(), 3);
    }

    #[tokio::test]
    async fn destinations_are_served_independently() {
        let slow = FakeFederationPeer::new("slow.example.org");
        slow.queue_response(CannedResponse::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({}),
        ));
        let quick = FakeFederationPeer::new("quick.example.org");
        let (slow_dest, _) = spawn_peer(&slow).await;
        let (quick_dest, _) = spawn_peer(&quick).await;
        let sender = FederationSender::with_config(
            client(),
            US,
            SenderConfig {
                initial_backoff: Duration::from_millis(500),
                max_backoff: Duration::from_secs(1),
                ..fast()
            },
        );

        sender.enqueue_pdu([slow_dest.clone()], pdu(1));
        sender.enqueue_pdu([quick_dest.clone()], pdu(2));

        // The quick destination is done while the slow one is still waiting out its first
        // failure: one queue's trouble is not the other's.
        assert!(
            wait_for(Duration::from_secs(10), || sender
                .pending_pdus_for(&quick_dest)
                == 0)
            .await
        );
        assert_eq!(sender.pending_pdus_for(&slow_dest), 1);
        assert_eq!(quick.request_count(), 1);

        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        let slow_requests = slow.requests();
        assert_eq!(slow_requests.len(), 2, "{slow_requests:?}");
        assert_eq!(slow_requests[1].body["pdus"][0]["i"], 1);
        let quick_requests = quick.requests();
        assert_eq!(quick_requests.len(), 1, "{quick_requests:?}");
        assert_eq!(quick_requests[0].body["pdus"][0]["i"], 2);
        let mut expected = vec![(quick_dest.clone(), 0), (slow_dest.clone(), 0)];
        expected.sort();
        assert_eq!(sender.pending_by_destination(), expected);
    }

    #[tokio::test]
    async fn nothing_is_ever_sent_to_our_own_server_name() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        // Our own name alone: no queue, no worker, nothing pending.
        sender.enqueue_pdu([US.to_owned()], pdu(0));
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());

        // Our own name mixed in with (and duplicated alongside) a real destination: exactly one
        // copy goes to the real one and none anywhere else.
        sender.enqueue_pdu(
            [
                US.to_owned(),
                destination.clone(),
                US.to_owned(),
                destination.clone(),
            ],
            pdu(1),
        );
        assert_eq!(sender.pending_pdus(), 1);
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        assert_eq!(
            sender.pending_by_destination(),
            vec![(destination.clone(), 0)]
        );
        let requests = peer.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].body["pdus"].as_array().unwrap().len(), 1);
    }

    /// A destination store whose answer for one destination is "not before `retry_at_ms`", so
    /// the test can assert the worker honoured it without depending on the real store's jitter.
    struct FixedBackoff {
        destination: String,
        retry_at_ms: u64,
        inner: InMemoryDestinationStore,
    }
    #[async_trait]
    impl DestinationStore for FixedBackoff {
        async fn get(&self, destination: &str) -> DestinationState {
            if destination == self.destination && now_ms() < self.retry_at_ms {
                return DestinationState {
                    failure_count: 1,
                    retry_at_ms: Some(self.retry_at_ms),
                    ..DestinationState::default()
                };
            }
            self.inner.get(destination).await
        }
        async fn record_failure(&self, destination: &str, max_backoff_ms: u64) {
            self.inner.record_failure(destination, max_backoff_ms).await;
        }
        async fn record_success(&self, destination: &str) {
            self.inner.record_success(destination).await;
        }
        async fn list(&self) -> Vec<(String, DestinationState)> {
            self.inner.list().await
        }
        async fn reset(&self, destination: &str) {
            self.inner.reset(destination).await;
        }
    }

    #[tokio::test]
    async fn a_destination_in_backoff_is_not_contacted_before_its_retry_time() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let retry_at_ms = now_ms() + 400;
        let store = Arc::new(FixedBackoff {
            destination: destination.clone(),
            retry_at_ms,
            inner: InMemoryDestinationStore::new(),
        });
        let sender = FederationSender::with_config(
            client_with(store),
            US,
            SenderConfig {
                initial_backoff: Duration::from_millis(50),
                max_backoff: Duration::from_secs(1),
                ..fast()
            },
        );

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 1).await);
        assert!(
            now_ms() >= retry_at_ms,
            "the destination was contacted before its retry time"
        );
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        assert_eq!(peer.request_count(), 1);
    }

    #[tokio::test]
    async fn a_per_pdu_rejection_is_final_not_retried() {
        let peer = FakeFederationPeer::new("peer.example.org");
        peer.queue_response(CannedResponse::ok(serde_json::json!({
            "pdus": { "$rejected": { "error": "event failed authorization" }, "$fine": {} }
        })));
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(peer.request_count(), 1);
    }

    #[tokio::test]
    async fn shutdown_stops_the_workers_and_drops_the_queue() {
        // A port nothing listens on: every attempt fails at connect and the worker would retry
        // for as long as it lived.
        let unused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let destination = format!("localhost:{}", unused.local_addr().unwrap().port());
        drop(unused);
        let sender = FederationSender::with_config(client(), US, fast());

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert_eq!(sender.pending_pdus(), 1);
        sender.shutdown();
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());

        // Queuing after shutdown is a no-op, not a new worker.
        sender.enqueue_pdu([destination.clone()], pdu(1));
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());
    }

    #[test]
    fn backoff_doubles_from_the_initial_delay_and_is_capped() {
        let shared = Shared {
            client: client(),
            own_server_name: US.to_owned(),
            config: SenderConfig {
                initial_backoff: Duration::from_millis(100),
                max_backoff: Duration::from_millis(1000),
                reset_poll_interval: BACKOFF_POLL_INTERVAL,
                store_rescan_interval: None,
            },
            store: Arc::new(InMemoryOutboundStore::new()),
            gate: RwLock::new(Arc::new(SendsEverywhere)),
            started_ms: 0,
            txn_counter: AtomicU64::new(0),
            pending_total: AtomicUsize::new(0),
            shut_down: AtomicBool::new(false),
            edu_metrics: std::sync::OnceLock::new(),
        };
        assert_eq!(shared.backoff(1), Duration::from_millis(100));
        assert_eq!(shared.backoff(2), Duration::from_millis(200));
        assert_eq!(shared.backoff(4), Duration::from_millis(800));
        assert_eq!(shared.backoff(5), Duration::from_millis(1000));
        assert_eq!(shared.backoff(40), Duration::from_millis(1000));
        assert_eq!(shared.next_txn_id(), "0-1");
        assert_eq!(shared.next_txn_id(), "0-2");
    }

    /// The restart: a sender over a durable store fails to reach a destination and is shut
    /// down with PDUs queued; a new sender over the same backend resumes them and they arrive,
    /// in order, once the destination is up. The retry state is what the first sender left.
    #[tokio::test]
    async fn what_was_queued_is_sent_by_the_next_sender_over_the_same_store_in_order() {
        // A port nothing listens on yet: the first sender's attempts fail at connect.
        let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let destination = format!("localhost:{port}");
        let backend = MemoryBackend::new();
        let store: Arc<dyn OutboundStore> =
            Arc::new(KvOutboundStore::open(backend.clone()).unwrap());

        let first = FederationSender::with_store(client(), US, fast(), store.clone());
        assert!(first.is_durable());
        for i in 0..3 {
            first.enqueue_pdu([destination.clone()], pdu(i));
        }
        assert_eq!(first.pending_pdus(), 3);
        assert!(
            wait_for(Duration::from_secs(10), || {
                first
                    .destination_state(&destination)
                    .unwrap()
                    .is_some_and(|state| state.failures >= 1)
            })
            .await
        );
        let before = first.destination_state(&destination).unwrap().unwrap();
        assert!(before.last_error.is_some(), "{before:?}");
        assert!(before.failing_since_ms.is_some());
        assert!(before.next_attempt_ms.is_some());
        first.shutdown();
        assert_eq!(first.pending_pdus(), 0);
        assert_eq!(store.queued().unwrap(), vec![(destination.clone(), 3)]);
        drop(first);

        // The destination comes up; a new sender over the same backend is the restart.
        let peer = FakeFederationPeer::new("peer.example.org");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        spawn_peer_on(&peer, listener);
        let second = FederationSender::with_store(
            client(),
            US,
            fast(),
            Arc::new(KvOutboundStore::open(backend).unwrap()),
        );
        assert_eq!(second.pending_pdus(), 0);
        assert_eq!(second.resume().unwrap(), 3);
        assert_eq!(second.pending_pdus(), 3);
        assert_eq!(
            second.pending_by_destination(),
            vec![(destination.clone(), 3)]
        );
        assert_eq!(
            second.resume().unwrap(),
            0,
            "resuming twice starts nothing twice"
        );
        assert!(wait_for(Duration::from_secs(10), || second.pending_pdus() == 0).await);

        let requests = peer.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let pdus = requests[0].body["pdus"].as_array().unwrap();
        assert_eq!(pdus.len(), 3);
        for (i, pdu) in pdus.iter().enumerate() {
            assert_eq!(pdu["i"], i);
        }
        let after = second.destination_state(&destination).unwrap().unwrap();
        assert_eq!(after.failures, 0, "{after:?}");
        assert!(after.last_success_ms.is_some());
        assert!(after.failing_since_ms.is_none());
        assert!(store.queued().unwrap().is_empty());
        assert!(store.peek(&destination, 50).unwrap().is_empty());
    }

    /// What the previous run recorded as "not before then" is honoured by the next, and an
    /// administrator's reset ends the wait within one poll interval.
    #[tokio::test]
    async fn a_persisted_backoff_is_waited_out_after_a_restart_and_a_reset_ends_it_early() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let store: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        store
            .enqueue(std::slice::from_ref(&destination), &pdu(0))
            .unwrap();
        store
            .record_failure(&destination, "connection refused", now_ms() + 3_600_000)
            .unwrap();

        let sender = FederationSender::with_store(client(), US, fast(), store.clone());
        assert_eq!(sender.resume().unwrap(), 1);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            peer.requests().is_empty(),
            "sent during a backoff that was not over"
        );
        assert_eq!(sender.pending_pdus(), 1);

        sender.reset_destination(&destination).unwrap();
        assert!(wait_for(Duration::from_secs(5), || sender.pending_pdus() == 0).await);
        assert_eq!(peer.requests().len(), 1);
        let state = sender.destination_state(&destination).unwrap().unwrap();
        assert_eq!(state.failures, 0);
        assert!(state.last_success_ms.is_some());
        assert_eq!(
            state.last_error.as_deref(),
            Some("connection refused"),
            "the last error is history, not cleared by a reset"
        );
    }

    /// A queue in the store for a destination `resume` was never asked about (or that a
    /// previous worker left behind) goes out first, before what is queued now.
    #[tokio::test]
    async fn a_backlog_the_sender_was_not_told_about_goes_out_before_what_is_queued_now() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let store: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        store
            .enqueue(std::slice::from_ref(&destination), &pdu(0))
            .unwrap();
        store
            .enqueue(std::slice::from_ref(&destination), &pdu(1))
            .unwrap();

        let sender = FederationSender::with_store(client(), US, fast(), store.clone());
        sender.enqueue_pdu([destination.clone()], pdu(2));
        assert_eq!(
            sender.pending_pdus(),
            3,
            "the backlog counts as pending too"
        );
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);

        let sent: Vec<u64> = peer
            .requests()
            .iter()
            .flat_map(|request| request.body["pdus"].as_array().cloned().unwrap_or_default())
            .map(|pdu| pdu["i"].as_u64().unwrap())
            .collect();
        assert_eq!(sent, vec![0, 1, 2]);
        assert!(store.queued().unwrap().is_empty());
        assert_eq!(sender.pending_by_destination(), vec![(destination, 0)]);
    }

    /// A gate scripted by the test: the set of destinations sent for here.
    struct Only(Mutex<HashSet<String>>);
    impl SendGate for Only {
        fn sends_here(&self, destination: &str) -> bool {
            self.0.lock().unwrap().contains(destination)
        }
    }

    /// A destination the gate refuses here is written to the store and left there: no worker,
    /// nothing pending, nothing sent -- until the gate allows it and `resume` is run.
    #[tokio::test]
    async fn a_destination_another_replica_sends_for_is_stored_but_not_sent_from_here() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let store: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        let gate = Arc::new(Only(Mutex::new(HashSet::new())));
        let sender = FederationSender::with_store(client(), US, fast(), store.clone());
        sender.set_gate(gate.clone());

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());
        assert_eq!(store.queued().unwrap(), vec![(destination.clone(), 1)]);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            peer.requests().is_empty(),
            "sent from a replica that does not own it"
        );
        // Neither does resuming start it.
        assert_eq!(sender.resume().unwrap(), 0);

        // The shard is this replica's now.
        gate.0.lock().unwrap().insert(destination.clone());
        assert_eq!(sender.resume().unwrap(), 1);
        assert_eq!(sender.pending_pdus(), 1);
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        assert_eq!(peer.requests().len(), 1);
        assert!(store.queued().unwrap().is_empty());
    }

    /// Losing a destination stops its worker mid-retry and leaves its queue in the store.
    #[tokio::test]
    async fn losing_a_destination_stops_its_worker_and_leaves_its_queue_in_the_store() {
        let peer = FakeFederationPeer::new("peer.example.org");
        for _ in 0..50 {
            peer.queue_response(CannedResponse::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({}),
            ));
        }
        let (destination, _auth) = spawn_peer(&peer).await;
        let store: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        let gate = Arc::new(Only(Mutex::new(HashSet::from([destination.clone()]))));
        let sender = FederationSender::with_store(client(), US, fast(), store.clone());
        sender.set_gate(gate.clone());

        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert_eq!(sender.pending_pdus(), 1);
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 2).await);
        // The gate flips while the worker is retrying.
        gate.0.lock().unwrap().clear();
        assert_eq!(
            sender.stop_workers_not_sent_here(),
            vec![destination.clone()]
        );
        assert_eq!(sender.pending_pdus(), 0);
        assert!(sender.pending_by_destination().is_empty());
        let attempts = peer.request_count();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(peer.request_count(), attempts, "the worker kept retrying");
        assert_eq!(store.queued().unwrap(), vec![(destination.clone(), 1)]);
        assert!(sender.stop_workers_not_sent_here().is_empty());
    }

    /// A row another replica wrote for a destination this one already has a worker for is found
    /// by the rescan, not left until something local is queued for it.
    #[tokio::test]
    async fn a_row_written_behind_the_workers_back_is_found_by_the_rescan() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let store: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        let sender = FederationSender::with_store(
            client(),
            US,
            SenderConfig {
                store_rescan_interval: Some(Duration::from_millis(50)),
                ..fast()
            },
            store.clone(),
        );
        sender.enqueue_pdu([destination.clone()], pdu(0));
        assert!(wait_for(Duration::from_secs(10), || sender.pending_pdus() == 0).await);
        assert_eq!(peer.request_count(), 1);

        // The other replica's write: straight into the store, past this sender.
        store
            .enqueue(std::slice::from_ref(&destination), &pdu(1))
            .unwrap();
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() == 2).await);
        let second = &peer.requests()[1];
        assert_eq!(second.body["pdus"][0]["i"], 1);
        // The peer has the request before the worker has read its answer and deleted the row.
        assert!(
            wait_for(Duration::from_secs(10), || store
                .queued()
                .unwrap()
                .is_empty())
            .await
        );
        assert_eq!(
            sender.pending_pdus(),
            0,
            "never counted here, never negative"
        );
    }

    // ---- EDUs ----

    fn typing(i: usize) -> Value {
        serde_json::json!({"room_id": "!r:example.org", "user_id": format!("@u{i}:example.org"), "typing": true})
    }

    /// An EDU goes out in the same transaction as the PDUs waiting with it, in the spec's
    /// `{edu_type, content}` shape; one queued on its own gets a transaction of its own. Each is
    /// counted as sent, by type, once the destination has accepted it.
    #[tokio::test]
    async fn an_edu_rides_with_waiting_pdus_and_goes_alone_when_nothing_waits() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());
        let metrics = crate::metrics::EduMetrics::default();
        sender.install_edu_metrics(metrics.clone());

        // Nothing has yielded to the worker between these two, so it finds both.
        sender.enqueue_pdu([destination.clone()], pdu(0));
        sender.enqueue_edu([destination.clone()], "m.typing", typing(0), None);
        assert_eq!(sender.pending_edus_for(&destination), 1);
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 1).await);
        assert!(
            wait_for(Duration::from_secs(10), || sender
                .pending_edus_for(&destination)
                == 0)
            .await
        );

        sender.enqueue_edu(
            [destination.clone(), US.to_owned()],
            "m.presence",
            serde_json::json!({"push": []}),
            None,
        );
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 2).await);

        let requests = peer.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[0].body["pdus"].as_array().unwrap().len(), 1);
        assert_eq!(
            requests[0].body["edus"],
            serde_json::json!([{"edu_type": "m.typing", "content": typing(0)}])
        );
        assert_eq!(requests[1].body["pdus"], serde_json::json!([]));
        assert_eq!(
            requests[1].body["edus"],
            serde_json::json!([{"edu_type": "m.presence", "content": {"push": []}}])
        );
        assert!(
            wait_for(Duration::from_secs(10), || metrics.sent("m.presence") == 1).await,
            "the accepted presence EDU is counted"
        );
        assert_eq!(metrics.sent("m.typing"), 1);
        assert_ne!(
            requests[0].path, requests[1].path,
            "each is its own transaction"
        );
    }

    /// At most a hundred EDUs to a transaction, in the order queued; a newer EDU with the same
    /// coalescing key replaces the unsent one rather than following it.
    #[tokio::test]
    async fn edus_are_capped_per_transaction_and_a_newer_one_replaces_its_unsent_key() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let sender = FederationSender::with_config(client(), US, fast());

        sender.enqueue_edu(
            [destination.clone()],
            "m.typing",
            serde_json::json!({"stale": true}),
            Some("typing alice".to_owned()),
        );
        for i in 0..149 {
            sender.enqueue_edu(
                [destination.clone()],
                "m.typing",
                typing(i),
                Some(format!("k{i}")),
            );
        }
        sender.enqueue_edu(
            [destination.clone()],
            "m.typing",
            serde_json::json!({"stale": false}),
            Some("typing alice".to_owned()),
        );
        assert_eq!(sender.pending_edus_for(&destination), 150);
        assert!(wait_for(Duration::from_secs(10), || peer.request_count() >= 2).await);
        assert!(
            wait_for(Duration::from_secs(10), || sender
                .pending_edus_for(&destination)
                == 0)
            .await
        );

        let requests = peer.requests();
        let sizes: Vec<usize> = requests
            .iter()
            .map(|r| r.body["edus"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, vec![100, 50]);
        let contents: Vec<Value> = requests
            .iter()
            .flat_map(|r| r.body["edus"].as_array().unwrap().clone())
            .map(|edu| edu["content"].clone())
            .collect();
        assert_eq!(contents[0], typing(0), "order is kept");
        assert_eq!(contents[149], serde_json::json!({"stale": false}));
        assert!(!contents.contains(&serde_json::json!({"stale": true})));
    }

    /// An EDU for a destination another replica sends for is not sent from here, and -- unlike
    /// a PDU -- not stored for that replica either.
    #[tokio::test]
    async fn an_edu_for_a_destination_another_replica_sends_for_is_dropped() {
        let peer = FakeFederationPeer::new("peer.example.org");
        let (destination, _auth) = spawn_peer(&peer).await;
        let store: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        let sender = FederationSender::with_store(client(), US, fast(), store.clone());
        sender.set_gate(Arc::new(Only(Mutex::new(HashSet::new()))));

        sender.enqueue_edu([destination.clone()], "m.typing", typing(0), None);
        assert_eq!(sender.pending_edus_for(&destination), 0);
        assert!(store.queued().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(peer.requests().is_empty());
    }
}
