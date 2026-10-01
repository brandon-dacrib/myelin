//! Ownership: the replica registry, heartbeats, failure detection, rendezvous convergence and the
//! [`Ownership`] API every actor is written against, whether clustered ([`KvOwnership`]) or
//! single-node ([`SingleNode`]). See `docs/rfcs/0001-cluster-ownership.md` sections 4, 5, 7, 10
//! and 12.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use hs_kv::KvBackend;
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
// `tokio::time::Instant`, not `std::time::Instant`: every timing decision in this module
// (failure detection, self-suspicion, drain deadlines) must respect the paused/advanced clock
// `#[tokio::test(start_paused = true)]` uses, or the chaos harness (which relies on exactly that)
// cannot simulate lease expiry deterministically.
use tokio::time::Instant;

use crate::config::ClusterConfig;
use crate::error::ClusterError;
use crate::fence::Fence;
use crate::hash;
use crate::metrics::{ChurnReason, ClusterMetrics};
use crate::store::ClusterStore;
use crate::types::{
    Epoch, Generation, ReplicaId, ReplicaRecord, ReplicaState, ShardId, ShardLayout, ShardRecord,
};

/// The owner of every shard, as this replica currently believes it (from the last row scan plus
/// best-effort mesh announcements). A forwarding hint, not a guarantee -- the fencing epoch is
/// what actually protects data; this map only saves a forward from guessing wrong.
#[derive(Debug, Clone, Default)]
pub struct ShardMap {
    owners: HashMap<ShardId, ReplicaId>,
}

impl ShardMap {
    /// The believed owner of `shard`, if any.
    #[must_use]
    pub fn owner_of(&self, shard: ShardId) -> Option<&ReplicaId> {
        self.owners.get(&shard)
    }

    /// How many shards this map believes are owned by somebody.
    #[must_use]
    pub fn owned_shard_count(&self) -> usize {
        self.owners.len()
    }

    /// How many distinct replicas own at least one shard: the nearest thing this map has to
    /// "how many replicas are serving", and what an operator's overview shows. A replica that
    /// is up but owns nothing yet is not counted.
    #[must_use]
    pub fn owning_replica_count(&self) -> usize {
        self.owners
            .values()
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// Every distinct replica this map believes owns at least one shard: the live membership of
    /// the cluster as far as ownership can tell. What a room owner fans a `/sync` wake out to,
    /// and who a replica asks for their stream positions before it reads (`hs-user`'s session
    /// cluster). A replica that is up but owns nothing yet is not listed; it also cannot have
    /// written anything a `/sync` could be waiting for.
    #[must_use]
    pub fn replicas(&self) -> std::collections::BTreeSet<ReplicaId> {
        self.owners.values().cloned().collect()
    }
}

/// Ownership convergence events, emitted on [`Ownership::subscribe`].
#[derive(Debug, Clone)]
pub enum OwnershipEvent {
    /// This replica acquired a shard; `fence` is the proof of ownership to pass to the actor.
    Acquired(Fence),
    /// This replica released a shard because it is no longer the desired owner.
    Released(ShardId),
    /// A transaction on `shard` was rejected as stale: the actor must drop its in-memory state.
    Lost {
        /// The shard.
        shard: ShardId,
        /// The epoch the rejection was reported at.
        epoch: Epoch,
    },
    /// The live replica set changed (a peer joined, left or was judged dead).
    MembershipChanged,
}

/// Whether a replica is ready to serve, and why not if it is not. Consumed by track 12's
/// `/health/ready`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Ready: joined the mesh (or running single-node), heartbeated, and ownership has converged.
    Ready,
    /// Not yet ready, with a short human-readable reason.
    NotReady(String),
}

/// The result of a [`Drainable::drain`] call.
#[derive(Debug, Clone, Default)]
pub struct DrainReport {
    /// Shards released and handed off to a new owner before the deadline.
    pub handed_off: usize,
    /// Shards released but with no confirmed new owner before the deadline (still correct: a
    /// live peer acquires them on its next tick, so nothing is lost, only latency).
    pub released_unclaimed: usize,
    /// How long the drain took.
    pub elapsed: Duration,
}

/// The ownership API every actor is written against: [`SingleNode`] and [`KvOwnership`] both
/// implement it, so actor code never needs to know which mode it is running under.
pub trait Ownership: Send + Sync {
    /// This replica's identity.
    fn me(&self) -> &ReplicaId;
    /// The believed owner of `shard` (the row owner if known, else the desired owner).
    fn owner_of(&self, shard: ShardId) -> Option<ReplicaId>;
    /// Whether this replica currently owns `shard` with a fresh lease (self-suspicion: a replica
    /// whose own heartbeats are failing answers `false` even if its last-known row still names
    /// it, per RFC 0001 section 4).
    fn is_mine(&self, shard: ShardId) -> bool;
    /// The fence for `shard`, if this replica owns it. `Some` iff [`Ownership::is_mine`] is true.
    fn fence(&self, shard: ShardId) -> Option<Fence>;
    /// Subscribes to ownership events.
    fn subscribe(&self) -> broadcast::Receiver<OwnershipEvent>;
    /// A live view of the current shard map.
    fn shard_map(&self) -> watch::Receiver<Arc<ShardMap>>;
}

/// Lifecycle operations only the process's own `main`/`hs-cli` calls: draining on shutdown and
/// readiness for probes. Kept separate from [`Ownership`] so actor code, which only ever needs
/// the read-side API, cannot accidentally drain the cluster it is running in.
#[async_trait::async_trait]
pub trait Drainable: Send + Sync {
    /// Whether this replica is ready to serve.
    fn ready(&self) -> Readiness;
    /// Runs the graceful handoff sequence (RFC 0001 section 10), releasing every owned shard and
    /// nudging its desired owner, up to `deadline`.
    async fn drain(&self, deadline: Duration) -> DrainReport;
}

/// The single-node ownership manager: `owner_of` is always me, `is_mine` is always true,
/// [`Fence::check`] on the fences it hands out is always a no-op, there is no registry, no
/// heartbeat task and no mesh listener. `forward` must never be reached in this mode (RFC 0001
/// section 12).
pub struct SingleNode {
    me: ReplicaId,
    events: broadcast::Sender<OwnershipEvent>,
    shard_map: watch::Sender<Arc<ShardMap>>,
}

impl SingleNode {
    /// Builds the inert ownership manager for `hs serve --single-node`.
    #[must_use]
    pub fn new(me: ReplicaId) -> Arc<Self> {
        let (events, _) = broadcast::channel(16);
        let (shard_map, _) = watch::channel(Arc::new(ShardMap::default()));
        Arc::new(Self {
            me,
            events,
            shard_map,
        })
    }
}

impl Ownership for SingleNode {
    fn me(&self) -> &ReplicaId {
        &self.me
    }

    fn owner_of(&self, _shard: ShardId) -> Option<ReplicaId> {
        Some(self.me.clone())
    }

    fn is_mine(&self, _shard: ShardId) -> bool {
        true
    }

    fn fence(&self, shard: ShardId) -> Option<Fence> {
        Some(Fence::inert(shard))
    }

    fn subscribe(&self) -> broadcast::Receiver<OwnershipEvent> {
        self.events.subscribe()
    }

    fn shard_map(&self) -> watch::Receiver<Arc<ShardMap>> {
        self.shard_map.subscribe()
    }
}

#[async_trait::async_trait]
impl Drainable for SingleNode {
    fn ready(&self) -> Readiness {
        Readiness::Ready
    }

    async fn drain(&self, _deadline: Duration) -> DrainReport {
        // Nothing to hand off: single-node mode has no peers.
        DrainReport::default()
    }
}

struct PeerLiveness {
    last_seq: u64,
    last_change: Instant,
}

/// Per-shard ownership state this replica currently believes it holds.
struct Owned {
    epoch: Epoch,
}

/// The clustered ownership manager, built directly on an `hs-kv` [`hs_kv::KvBackend`] `B`. See the
/// module docs.
pub struct KvOwnership<B: KvBackend> {
    me: ReplicaId,
    generation: Generation,
    config: ClusterConfig,
    store: ClusterStore<B>,
    metrics: Arc<ClusterMetrics>,
    owned: RwLock<HashMap<ShardId, Owned>>,
    shard_map_tx: watch::Sender<Arc<ShardMap>>,
    shard_map_rx: watch::Receiver<Arc<ShardMap>>,
    events: broadcast::Sender<OwnershipEvent>,
    peers: RwLock<HashMap<ReplicaId, PeerLiveness>>,
    last_heartbeat_ok: RwLock<Option<Instant>>,
    draining: AtomicBool,
    /// Whether an administrator has asked this replica to drain
    /// ([`ClusterStore::request_drain`]), as of the last heartbeat that could read the request.
    /// Like `draining`, it takes this replica out of hashing and releases its shards; unlike
    /// it, it is withdrawn by [`ClusterStore::withdraw_drain`], after which the replica takes
    /// shards again, and it neither makes the replica unready nor stops the manager.
    admin_drained: AtomicBool,
    /// Held by the heartbeat loop for each tick, and by [`Drainable::drain`] while it stops the
    /// loop and deregisters (see there).
    tick_lock: tokio::sync::Mutex<()>,
    /// The `heartbeat_seq` the next heartbeat row carries: a counter, one step per heartbeat
    /// written, started above every value an earlier process of this replica wrote
    /// ([`ClusterStore::heartbeat_seq_floor`]). Peers judge liveness by seeing it change, so it
    /// must never repeat; until 2026-09-30 it was the wall clock in milliseconds, and two
    /// heartbeats in one millisecond (or a clock stepped back) read as no progress.
    next_heartbeat_seq: AtomicU64,
    nudge: Notify,
    stop: watch::Sender<bool>,
}

impl<B: KvBackend> KvOwnership<B> {
    /// Opens the store, registers (or confirms) the shard layout, and starts the heartbeat and
    /// convergence background task. Returns the manager and a handle to that task (for tests and
    /// for orderly shutdown); production callers generally only need the manager.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the layout could not be confirmed or the store could not be
    /// reached.
    pub async fn start(
        config: ClusterConfig,
        backend: B,
    ) -> Result<(Arc<Self>, JoinHandle<()>), ClusterError> {
        let store = ClusterStore::open(backend)?;
        let layout = config.layout;
        let store_for_init = store.clone();
        let me = config.me.clone();
        let seq_floor = match tokio::task::spawn_blocking(move || {
            store_for_init.init_layout(layout)?;
            store_for_init.heartbeat_seq_floor(&me)
        })
        .await
        {
            Ok(inner) => inner?,
            Err(join_err) => return Err(ClusterError::Store(hs_kv::KvError::backend(join_err))),
        };
        let generation = Generation::fresh(None);
        tracing::info!(
            replica = %config.me,
            generation = generation.0,
            first_heartbeat_seq = seq_floor.saturating_add(1),
            "joining the cluster; heartbeats continue above every earlier process of this replica"
        );

        let (shard_map_tx, shard_map_rx) = watch::channel(Arc::new(ShardMap::default()));
        let (events, _) = broadcast::channel(1024);
        let (stop, stop_rx) = watch::channel(false);

        let manager = Arc::new(Self {
            me: config.me.clone(),
            generation,
            config,
            store,
            metrics: Arc::new(ClusterMetrics::new()),
            owned: RwLock::new(HashMap::new()),
            shard_map_tx,
            shard_map_rx,
            events,
            peers: RwLock::new(HashMap::new()),
            last_heartbeat_ok: RwLock::new(None),
            draining: AtomicBool::new(false),
            admin_drained: AtomicBool::new(false),
            tick_lock: tokio::sync::Mutex::new(()),
            next_heartbeat_seq: AtomicU64::new(seq_floor.saturating_add(1)),
            nudge: Notify::new(),
            stop,
        });

        let bg = manager.clone();
        let handle = tokio::spawn(async move { bg.run(stop_rx).await });
        Ok((manager, handle))
    }

    /// A snapshot of this manager's metrics.
    #[must_use]
    pub fn metrics(&self) -> Arc<ClusterMetrics> {
        self.metrics.clone()
    }

    /// The next heartbeat row: one more step of `heartbeat_seq` than the last one built (the
    /// liveness signal peers watch), and the wall clock in `heartbeat_unix_ms`, which is for
    /// operators only and never compared for liveness.
    fn heartbeat_row(&self, state: ReplicaState) -> ReplicaRecord {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        ReplicaRecord {
            id: self.me.clone(),
            generation: self.generation,
            mesh_addr: self.config.mesh_advertise_addr.clone(),
            zone: self.config.zone.clone(),
            version: self.config.version.clone(),
            state,
            heartbeat_seq: self.next_heartbeat_seq.fetch_add(1, Ordering::SeqCst),
            heartbeat_unix_ms: now,
        }
    }

    async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(self.config.heartbeat_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.nudge.notified() => {}
                _ = stop.changed() => {
                    if *stop.borrow() {
                        return;
                    }
                }
            }
            // Held for the whole tick, and checked for `stop` under it, so that `drain` (which
            // takes it before deregistering) can never have a tick's heartbeat land after the
            // row is removed and register this replica again.
            let _ticking = self.tick_lock.lock().await;
            if *stop.borrow() {
                return;
            }
            self.tick().await;
        }
    }

    /// Whether this replica is out of hashing and handing its shards off: shutting down, or
    /// drained by an administrator.
    fn stepping_aside(&self) -> bool {
        self.draining.load(Ordering::SeqCst) || self.admin_drained.load(Ordering::SeqCst)
    }

    /// Whether an administrator's drain request for this replica was in force at the last
    /// heartbeat.
    #[must_use]
    pub fn is_admin_drained(&self) -> bool {
        self.admin_drained.load(Ordering::SeqCst)
    }

    /// Reads this replica's drain request and follows it. A failed read keeps the previous
    /// answer: a store hiccup must neither start nor stop a drain.
    async fn read_drain_request(&self) {
        let store = self.store.clone();
        let me = self.me.clone();
        let request = tokio::task::spawn_blocking(move || store.drain_request(&me)).await;
        let Ok(Ok(request)) = request else { return };
        let wanted = request.is_some();
        let was = self.admin_drained.swap(wanted, Ordering::SeqCst);
        match (was, &request) {
            (false, Some(request)) => tracing::info!(
                replica = %self.me,
                requested_by = %request.requested_by,
                owned = self.owned_count(),
                "an administrator asked this replica to drain: handing its shards to the others"
            ),
            (true, None) => tracing::info!(
                replica = %self.me,
                "this replica's drain was withdrawn: taking its share of the shards again"
            ),
            _ => {}
        }
    }

    fn owned_count(&self) -> usize {
        self.owned
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    async fn tick(&self) {
        let started = Instant::now();
        self.read_drain_request().await;
        let state = if self.stepping_aside() {
            ReplicaState::Draining
        } else {
            ReplicaState::Active
        };
        let row = self.heartbeat_row(state);
        let store = self.store.clone();
        let row_for_hb = row.clone();
        let hb_result = tokio::task::spawn_blocking(move || store.heartbeat(&row_for_hb)).await;
        let heartbeat_ok = matches!(hb_result, Ok(Ok(())));
        if heartbeat_ok {
            self.metrics.set_heartbeat_seq(row.heartbeat_seq);
            *self
                .last_heartbeat_ok
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        }

        // Failure detection: observed-change on this observer's monotonic clock (RFC 0001
        // section 4). Only re-evaluate peers when our own heartbeat succeeded recently --
        // otherwise a store stall looks like everyone else dying.
        let store = self.store.clone();
        let replicas = tokio::task::spawn_blocking(move || store.list_replicas()).await;
        let Ok(Ok(replicas)) = replicas else { return };

        let self_heartbeat_fresh = self
            .last_heartbeat_ok
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|t| t.elapsed() <= self.config.heartbeat_interval * 2)
            .unwrap_or(false);

        let mut live: Vec<ReplicaId> = Vec::new();
        {
            let mut peers = self
                .peers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            let seen: std::collections::HashSet<_> =
                replicas.iter().map(|r| r.id.clone()).collect();
            for rec in &replicas {
                let entry = peers.entry(rec.id.clone()).or_insert_with(|| PeerLiveness {
                    last_seq: rec.heartbeat_seq,
                    last_change: now,
                });
                if entry.last_seq != rec.heartbeat_seq {
                    entry.last_seq = rec.heartbeat_seq;
                    entry.last_change = now;
                }
            }
            peers.retain(|id, _| seen.contains(id) || *id == self.me);
            for rec in &replicas {
                if !rec.state.is_hashable() {
                    continue;
                }
                let dead = rec.id != self.me
                    && self_heartbeat_fresh
                    && peers
                        .get(&rec.id)
                        .map(|p| p.last_change.elapsed() >= self.config.lease_ttl)
                        .unwrap_or(false);
                if !dead {
                    live.push(rec.id.clone());
                }
            }
        }
        self.metrics.set_live_replicas(live.len() as u64);
        if let Some(age) = *self
            .last_heartbeat_ok
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            self.metrics.set_lease_age(age.elapsed());
        }

        self.converge(&live, self_heartbeat_fresh, started).await;
    }

    /// Moves this replica's holdings toward what rendezvous hashing over `live` wants, and
    /// publishes the shard map.
    ///
    /// Two rules keep it honest (both found by two real replicas on one PostgreSQL, where one
    /// store transaction per shard makes a full convergence of a 137-shard layout take far
    /// longer than a lease):
    ///
    /// - **The store row is the truth.** A shard this replica holds in memory whose row no
    ///   longer names it (at its generation and epoch) was taken by a peer that judged it dead;
    ///   it is dropped as [`OwnershipEvent::Lost`] and, if still wanted, acquired again. Without
    ///   this, a replica that lost a shard and saw it released later would believe it held it
    ///   forever while the row stayed ownerless.
    /// - **A tick is bounded.** Acquisitions and releases stop once the tick has run for one
    ///   `heartbeat_interval` and resume on the next, which starts at once. The heartbeat and
    ///   the peers' liveness are observed at the start of every tick, so a long convergence can
    ///   neither let this replica's own lease lapse nor make a live peer look dead by comparing
    ///   against an observation taken seconds ago.
    async fn converge(&self, live: &[ReplicaId], self_heartbeat_fresh: bool, started: Instant) {
        let am_i_live = live.contains(&self.me);
        let store = self.store.clone();
        let rows = tokio::task::spawn_blocking(move || store.list_shards()).await;
        let Ok(Ok(rows)) = rows else { return };
        let rows_by_shard: HashMap<ShardId, ShardRecord> = rows.into_iter().collect();

        let draining = self.stepping_aside();
        let mut new_map = HashMap::new();
        let mut work_left = false;

        for shard in self.config.layout.all_shards() {
            let row = rows_by_shard
                .get(&shard)
                .cloned()
                .unwrap_or_else(ShardRecord::initial);
            if let Some((owner, _)) = &row.owner {
                new_map.insert(shard, owner.clone());
            }
            self.reconcile_with_row(shard, &row);

            let desired = if draining || !am_i_live {
                hash::desired_owner(shard, live.iter().filter(|id| **id != self.me)).cloned()
            } else {
                hash::desired_owner(shard, live.iter()).cloned()
            };
            let i_want_it = desired.as_ref() == Some(&self.me)
                && !draining
                && am_i_live
                && self_heartbeat_fresh;
            let i_hold_it = self
                .owned
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&shard);

            let change = i_want_it != i_hold_it;
            if change && started.elapsed() >= self.config.heartbeat_interval {
                work_left = true;
            } else if i_want_it && !i_hold_it {
                self.try_acquire(shard).await;
            } else if !i_want_it && i_hold_it {
                self.release(shard).await;
            }
        }

        let _ = self
            .shard_map_tx
            .send(Arc::new(ShardMap { owners: new_map }));
        if work_left {
            tracing::debug!(
                replica = %self.me,
                "convergence continues next tick: this one used its time budget"
            );
            self.nudge();
        }
    }

    /// Drops `shard` from what this replica holds if the store `row` says it is no longer
    /// this replica's at the epoch it acquired (see [`Self::converge`]).
    fn reconcile_with_row(&self, shard: ShardId, row: &ShardRecord) {
        let held_epoch = self
            .owned
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&shard)
            .map(|o| o.epoch);
        let Some(held_epoch) = held_epoch else { return };
        let still_mine =
            row.owner.as_ref().is_some_and(|(owner, generation)| {
                *owner == self.me && *generation == self.generation
            }) && row.epoch == held_epoch;
        if still_mine {
            return;
        }
        tracing::warn!(
            replica = %self.me,
            %shard,
            held_epoch = held_epoch.0,
            row_epoch = row.epoch.0,
            row_owner = row.owner.as_ref().map(|(owner, _)| owner.as_str()).unwrap_or("none"),
            "a shard this replica held was taken by a peer that judged it dead; dropping it"
        );
        self.drop_lost(shard, row.epoch);
    }

    /// Replicas this observer currently judges dead (RFC 0001 section 4), as an owned set: kept
    /// in its own synchronous function so the `std::sync::RwLockReadGuard` it takes (not `Send`)
    /// never becomes part of an `async fn`'s generator state, even transiently.
    fn dead_peers(&self) -> std::collections::HashSet<ReplicaId> {
        let peers = self
            .peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ttl = self.config.lease_ttl;
        peers
            .iter()
            .filter(|(id, p)| **id != self.me && p.last_change.elapsed() >= ttl)
            .map(|(id, _)| id.clone())
            .collect()
    }

    async fn try_acquire(&self, shard: ShardId) {
        let store = self.store.clone();
        let me = self.me.clone();
        let generation = self.generation;
        let dead = self.dead_peers();
        let result = tokio::task::spawn_blocking(move || {
            store.acquire_shard(shard, &me, generation, |owner| dead.contains(owner))
        })
        .await;
        if let Ok(Ok(Some(rec))) = result {
            self.owned
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(shard, Owned { epoch: rec.epoch });
            self.metrics
                .record_ownership_change(shard.kind.as_str(), ChurnReason::Acquire);
            let _ = self
                .events
                .send(OwnershipEvent::Acquired(Fence::clustered(shard, rec.epoch)));
        }
    }

    async fn release(&self, shard: ShardId) {
        let store = self.store.clone();
        let me = self.me.clone();
        let generation = self.generation;
        let result =
            tokio::task::spawn_blocking(move || store.release_shard(shard, &me, generation)).await;
        if matches!(result, Ok(Ok(()))) {
            self.owned
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&shard);
            self.metrics
                .record_ownership_change(shard.kind.as_str(), ChurnReason::Release);
            let _ = self.events.send(OwnershipEvent::Released(shard));
        }
    }

    /// Reports that a transaction against `shard` was rejected as fenced (a stale write). The
    /// actor calls this after a [`crate::error::FenceError::Fenced`] so bookkeeping (owned set,
    /// metrics, the `Lost` event) stays consistent with what actually happened at the store.
    pub fn report_fenced(&self, shard: ShardId, epoch: Epoch) {
        self.metrics.record_fenced(shard.kind.as_str());
        self.drop_lost(shard, epoch);
    }

    /// Forgets `shard` as lost: out of the held set, counted, and announced as
    /// [`OwnershipEvent::Lost`] so actors drop their in-memory state. A shard not held is left
    /// alone.
    fn drop_lost(&self, shard: ShardId, epoch: Epoch) {
        let was_held = self
            .owned
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&shard)
            .is_some();
        if !was_held {
            return;
        }
        self.metrics
            .record_ownership_change(shard.kind.as_str(), ChurnReason::Lost);
        let _ = self.events.send(OwnershipEvent::Lost { shard, epoch });
    }

    /// Wakes the convergence loop immediately (used when a `/mesh/v1/released` nudge arrives, or
    /// in tests).
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }

    /// The current shard layout.
    #[must_use]
    pub fn layout(&self) -> ShardLayout {
        self.config.layout
    }

    /// Access to the underlying store, for callers (the mesh server, tests) that need the shard
    /// keyspace handle for [`Fence::check`].
    #[must_use]
    pub fn store(&self) -> &ClusterStore<B> {
        &self.store
    }
}

impl<B: KvBackend> Ownership for KvOwnership<B> {
    fn me(&self) -> &ReplicaId {
        &self.me
    }

    fn owner_of(&self, shard: ShardId) -> Option<ReplicaId> {
        if self.is_mine(shard) {
            return Some(self.me.clone());
        }
        self.shard_map_rx.borrow().owner_of(shard).cloned()
    }

    fn is_mine(&self, shard: ShardId) -> bool {
        let held = self
            .owned
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&shard);
        if !held {
            return false;
        }
        // Self-suspicion (RFC 0001 section 4): if my own heartbeats have failed for `lease_ttl`,
        // stop claiming ownership for reads even though the store row has not changed yet.
        self.last_heartbeat_ok
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|t| t.elapsed() < self.config.lease_ttl)
            .unwrap_or(false)
    }

    fn fence(&self, shard: ShardId) -> Option<Fence> {
        if !self.is_mine(shard) {
            return None;
        }
        self.owned
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&shard)
            .map(|o| Fence::clustered(shard, o.epoch))
    }

    fn subscribe(&self) -> broadcast::Receiver<OwnershipEvent> {
        self.events.subscribe()
    }

    fn shard_map(&self) -> watch::Receiver<Arc<ShardMap>> {
        self.shard_map_rx.clone()
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> Drainable for KvOwnership<B> {
    fn ready(&self) -> Readiness {
        if self.draining.load(Ordering::SeqCst) {
            return Readiness::NotReady("draining".into());
        }
        let heartbeat_fresh = self
            .last_heartbeat_ok
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|t| t.elapsed() < self.config.lease_ttl)
            .unwrap_or(false);
        if !heartbeat_fresh {
            return Readiness::NotReady("no recent successful heartbeat".into());
        }
        Readiness::Ready
    }

    async fn drain(&self, deadline: Duration) -> DrainReport {
        let start = Instant::now();
        self.draining.store(true, Ordering::SeqCst);
        // Announce immediately: my heartbeat row flips to `Draining`, which excludes me from
        // every peer's next hash (RFC 0001 section 10 step 1).
        let row = self.heartbeat_row(ReplicaState::Draining);
        let store = self.store.clone();
        let _ = tokio::task::spawn_blocking(move || store.heartbeat(&row)).await;
        self.nudge();

        let deadline = deadline.saturating_sub(self.config.handoff.safety_margin);
        let deadline_at = start + deadline;

        // Release every shard we hold, in parallel batches (RFC 0001 section 10 step 3). Each
        // release also nudges the convergence loop of any replica that later notices via
        // `nudge()`; the mesh server wires the `/mesh/v1/released` HTTP nudge for real peers.
        let mut released_shards: Vec<ShardId> = Vec::new();
        loop {
            let owned: Vec<ShardId> = self
                .owned
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .keys()
                .copied()
                .collect();
            if owned.is_empty() || Instant::now() >= deadline_at {
                break;
            }
            let batch: Vec<_> = owned
                .into_iter()
                .take(self.config.handoff.parallelism.max(1))
                .collect();
            let mut handles = Vec::new();
            for shard in &batch {
                handles.push(self.release(*shard));
            }
            futures::future::join_all(handles).await;
            released_shards.extend(batch);
        }

        // Step 4: wait (up to what is left of the deadline) for each released shard to show a
        // new owner, so the report distinguishes a clean handoff from one that ran out of time.
        let mut handed_off = 0usize;
        let mut released_unclaimed = 0usize;
        for shard in &released_shards {
            loop {
                let store = self.store.clone();
                let s = *shard;
                let row = tokio::task::spawn_blocking(move || store.get_shard(s)).await;
                let claimed = matches!(&row, Ok(Ok(rec)) if rec.owner.is_some());
                if claimed {
                    handed_off += 1;
                    break;
                }
                if Instant::now() >= deadline_at {
                    released_unclaimed += 1;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        // Anything still marked owned (release itself failed, e.g. a store error) counts as
        // unclaimed too -- it is left `owner == me` in the store, which is safe (a live peer
        // will not acquire it while this row exists) but not a clean handoff.
        released_unclaimed += self
            .owned
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();

        // Deregister: remove our row entirely so we are not even counted as `Draining` overhead.
        // The heartbeat loop is stopped first, with no tick in flight (`tick_lock`): a tick
        // that was already running would otherwise write the row back after its removal, and
        // this replica, restarted within the lease, would be refused as a live duplicate of
        // itself.
        let _no_tick = self.tick_lock.lock().await;
        let _ = self.stop.send(true);
        let store = self.store.clone();
        let me = self.me.clone();
        let generation = self.generation;
        let _ = tokio::task::spawn_blocking(move || store.remove_replica(&me, generation)).await;

        DrainReport {
            handed_off,
            released_unclaimed,
            elapsed: start.elapsed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;

    use super::*;
    use crate::test_clock::settle_until;
    use crate::types::ShardLayout;

    fn config(me: &str) -> ClusterConfig {
        let mut c = ClusterConfig::new(ReplicaId::new(me), "127.0.0.1:0", ShardLayout::small(4));
        c.heartbeat_interval = Duration::from_millis(50);
        c.lease_ttl = Duration::from_millis(150);
        c
    }

    #[tokio::test(start_paused = true)]
    async fn single_replica_acquires_every_shard() {
        let backend = MemoryBackend::new();
        let (mgr, _handle) = KvOwnership::start(config("hs-0"), backend).await.unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 200, || {
                mgr.layout().all_shards().all(|s| mgr.is_mine(s))
            })
            .await,
            "the solo replica never acquired every shard"
        );
        for shard in mgr.layout().all_shards() {
            assert!(mgr.is_mine(shard), "{shard} not owned");
            assert!(mgr.fence(shard).is_some());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_shard_taken_while_held_is_dropped_and_taken_back_once_released() {
        let backend = MemoryBackend::new();
        let (mgr, _handle) = KvOwnership::start(config("hs-0"), backend.clone())
            .await
            .unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 200, || {
                mgr.layout().all_shards().all(|s| mgr.is_mine(s))
            })
            .await
        );
        let mut events = mgr.subscribe();
        let shard = ShardId::new(crate::types::ShardKind::Room, 1);
        let held = mgr.fence(shard).unwrap().epoch.unwrap();

        // A peer that judged hs-0 dead takes the shard, then lets it go, all between two of
        // hs-0's ticks: the row is ownerless at a later epoch.
        let store = ClusterStore::open(backend).unwrap();
        let thief = ReplicaId::new("hs-9");
        let stolen = store
            .acquire_shard(shard, &thief, Generation(1), |_| true)
            .unwrap()
            .unwrap();
        store.release_shard(shard, &thief, Generation(1)).unwrap();
        assert!(stolen.epoch > held);

        // hs-0 notices, reports the loss, and owns the shard again at a newer epoch -- in the
        // store, not only in its own memory.
        assert!(
            settle_until(Duration::from_millis(60), 200, || {
                let row = store.get_shard(shard).unwrap();
                row.owner
                    .as_ref()
                    .is_some_and(|(o, _)| o.as_str() == "hs-0")
                    && mgr.fence(shard).is_some_and(|f| f.epoch == Some(row.epoch))
            })
            .await,
            "hs-0 never took back the shard it lost"
        );
        let mut lost = false;
        while let Ok(event) = events.try_recv() {
            if let OwnershipEvent::Lost { shard: s, epoch } = event {
                assert_eq!((s, epoch), (shard, stolen.epoch));
                lost = true;
            }
        }
        assert!(lost, "the loss was never announced");
        assert!(mgr.fence(shard).unwrap().epoch.unwrap() > stolen.epoch);
    }

    #[tokio::test(start_paused = true)]
    async fn two_replicas_partition_the_shard_space_without_overlap() {
        let backend = MemoryBackend::new();
        let (a, _ha) = KvOwnership::start(config("hs-0"), backend.clone())
            .await
            .unwrap();
        let (b, _hb) = KvOwnership::start(config("hs-1"), backend).await.unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 200, || {
                a.layout()
                    .all_shards()
                    .all(|s| a.is_mine(s) || b.is_mine(s))
            })
            .await,
            "the two replicas never partitioned the shard space"
        );
        for shard in a.layout().all_shards() {
            let mine_a = a.is_mine(shard);
            let mine_b = b.is_mine(shard);
            assert!(!(mine_a && mine_b), "{shard} owned by both");
            assert!(mine_a || mine_b, "{shard} owned by neither");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drain_releases_every_owned_shard() {
        let backend = MemoryBackend::new();
        let (mgr, _handle) = KvOwnership::start(config("hs-0"), backend.clone())
            .await
            .unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 200, || {
                mgr.layout().all_shards().all(|s| mgr.is_mine(s))
            })
            .await,
            "the solo replica never acquired every shard"
        );
        let total = mgr.layout().all_shards().count();
        let report = mgr.drain(Duration::from_secs(5)).await;
        // Solo replica: every shard is released (nothing left `owner == me`), but none is
        // "handed off" in the report's sense, because there is no peer to claim it -- that is
        // still correct per RFC 0001 section 10 step 4 ("shards still unclaimed at the deadline
        // remain released ... nothing is lost, only latency").
        assert_eq!(
            report.released_unclaimed + report.handed_off,
            total,
            "every shard should have been released"
        );
        assert_eq!(report.handed_off, 0, "no peer exists to hand off to");
        for shard in mgr.layout().all_shards() {
            assert!(!mgr.is_mine(shard));
        }
    }

    /// Two heartbeats in the same millisecond are two steps of progress. When `heartbeat_seq`
    /// was the wall clock in milliseconds they carried the same value, which a peer reads as
    /// "no progress": a few hundred rows built back to back here held dozens of repeats.
    #[tokio::test(start_paused = true)]
    async fn heartbeats_in_one_millisecond_are_each_a_step_of_progress() {
        let (mgr, _handle) = KvOwnership::start(config("hs-0"), MemoryBackend::new())
            .await
            .unwrap();
        let rows: Vec<ReplicaRecord> = (0..500)
            .map(|_| mgr.heartbeat_row(ReplicaState::Active))
            .collect();
        for pair in rows.windows(2) {
            assert_eq!(
                pair[1].heartbeat_seq,
                pair[0].heartbeat_seq + 1,
                "two heartbeats (at {} and {} ms) are not two steps of progress",
                pair[0].heartbeat_unix_ms,
                pair[1].heartbeat_unix_ms
            );
        }
        // The wall clock is still there for operators.
        assert!(rows[0].heartbeat_unix_ms > 1_600_000_000_000);
    }

    /// The sequence a replica's process starts from is above every value an earlier process
    /// of the same replica wrote: after a drain (which removes the registry row), after a crash
    /// (which leaves it), and when the earlier value is far above the wall clock (a clock
    /// stepped back, or an earlier binary's millisecond sequence).
    #[tokio::test(start_paused = true)]
    async fn a_restart_continues_the_heartbeat_seq_above_the_previous_process() {
        let backend = MemoryBackend::new();
        let store = ClusterStore::open(backend.clone()).unwrap();
        let row_seq = || {
            store
                .list_replicas()
                .unwrap()
                .into_iter()
                .find(|r| r.id.as_str() == "hs-0")
                .map(|r| r.heartbeat_seq)
        };

        // A crashed earlier process left a row whose sequence is far above today's clock.
        let far_ahead = 10 * 1_800_000_000_000;
        store
            .heartbeat(&ReplicaRecord {
                id: ReplicaId::new("hs-0"),
                generation: Generation(1),
                mesh_addr: "127.0.0.1:0".into(),
                zone: None,
                version: "old".into(),
                state: ReplicaState::Active,
                heartbeat_seq: far_ahead,
                heartbeat_unix_ms: 1,
            })
            .unwrap();
        let (first, _h1) = KvOwnership::start(config("hs-0"), backend.clone())
            .await
            .unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 200, || row_seq()
                .is_some_and(|s| s > far_ahead + 3))
            .await,
            "the restarted replica's heartbeats went backwards: {:?} after {far_ahead}",
            row_seq()
        );

        // A drain removes the row; the next process still continues above it.
        first.drain(Duration::from_secs(1)).await;
        assert_eq!(row_seq(), None, "the drained replica deregistered");
        let last = store.heartbeat_seq_floor(&ReplicaId::new("hs-0")).unwrap();
        assert!(last > far_ahead + 3);
        let (second, _h2) = KvOwnership::start(config("hs-0"), backend.clone())
            .await
            .unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 200, || row_seq().is_some()).await,
            "the second process never heartbeated"
        );
        let resumed = row_seq().unwrap();
        assert!(
            resumed > last,
            "the second process restarted its sequence at {resumed}, not above {last}"
        );
        assert!(
            settle_until(Duration::from_millis(60), 200, || second
                .metrics()
                .snapshot()
                .heartbeat_seq
                > last)
            .await,
            "hs_cluster_heartbeat_seq does not show the sequence"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_drained_replica_stays_deregistered_so_it_can_restart_at_once() {
        let backend = MemoryBackend::new();
        let store = ClusterStore::open(backend.clone()).unwrap();
        for _ in 0..20 {
            let (mgr, _handle) = KvOwnership::start(config("hs-0"), backend.clone())
                .await
                .unwrap();
            assert!(
                settle_until(Duration::from_millis(7), 200, || {
                    store
                        .list_replicas()
                        .unwrap()
                        .iter()
                        .any(|r| r.id.as_str() == "hs-0")
                })
                .await
            );
            mgr.drain(Duration::from_secs(1)).await;
            // However the drain raced the heartbeat loop, no heartbeat lands after it.
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(
                store.list_replicas().unwrap().is_empty(),
                "the heartbeat loop registered the drained replica again"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_administrators_drain_hands_every_shard_to_the_peer_and_withdrawing_it_rebalances() {
        let backend = MemoryBackend::new();
        let (a, _ha) = KvOwnership::start(config("hs-0"), backend.clone())
            .await
            .unwrap();
        let (b, _hb) = KvOwnership::start(config("hs-1"), backend.clone())
            .await
            .unwrap();
        let layout = a.layout();
        assert!(
            settle_until(Duration::from_millis(60), 200, || {
                layout.all_shards().all(|s| a.is_mine(s) || b.is_mine(s))
                    && layout.all_shards().any(|s| a.is_mine(s))
            })
            .await,
            "the two replicas never partitioned the shard space"
        );

        // Any replica (here, through the store directly) may ask hs-0 to drain.
        let store = ClusterStore::open(backend.clone()).unwrap();
        let request = crate::types::DrainRequest {
            requested_unix_ms: 1,
            requested_by: "@ops:example.org".into(),
            task_id: None,
        };
        let (_, created) = store
            .request_drain(&ReplicaId::new("hs-0"), &request)
            .unwrap();
        assert!(created);
        assert!(
            settle_until(Duration::from_millis(60), 400, || {
                layout.all_shards().all(|s| b.is_mine(s) && !a.is_mine(s))
            })
            .await,
            "hs-1 never took every shard from the drained hs-0"
        );
        assert!(a.is_admin_drained());
        // Drained is not shut down: still ready, still heartbeating, just owning nothing.
        assert_eq!(a.ready(), Readiness::Ready);
        let row = store
            .list_replicas()
            .unwrap()
            .into_iter()
            .find(|r| r.id.as_str() == "hs-0")
            .unwrap();
        assert_eq!(row.state, ReplicaState::Draining);

        // A second request keeps the first one's record.
        let (kept, created) = store
            .request_drain(
                &ReplicaId::new("hs-0"),
                &crate::types::DrainRequest {
                    requested_by: "@someone-else:example.org".into(),
                    ..request.clone()
                },
            )
            .unwrap();
        assert!(!created);
        assert_eq!(kept.requested_by, "@ops:example.org");
        assert_eq!(store.list_drain_requests().unwrap().len(), 1);

        // Withdrawn: hs-0 is hashed again and takes its share back.
        assert_eq!(
            store.withdraw_drain(&ReplicaId::new("hs-0")).unwrap(),
            Some(request)
        );
        assert_eq!(store.withdraw_drain(&ReplicaId::new("hs-0")).unwrap(), None);
        assert!(
            settle_until(Duration::from_millis(60), 400, || {
                layout.all_shards().any(|s| a.is_mine(s))
                    && layout.all_shards().all(|s| a.is_mine(s) != b.is_mine(s))
            })
            .await,
            "hs-0 never took shards back after its drain was withdrawn"
        );
        assert!(!a.is_admin_drained());
    }
}
