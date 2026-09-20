//! Ownership: the replica registry, heartbeats, failure detection, rendezvous convergence and the
//! [`Ownership`] API every actor is written against, whether clustered ([`KvOwnership`]) or
//! single-node ([`SingleNode`]). See `docs/rfcs/0001-cluster-ownership.md` sections 4, 5, 7, 10
//! and 12.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
        match tokio::task::spawn_blocking(move || store_for_init.init_layout(layout)).await {
            Ok(inner) => {
                inner?;
            }
            Err(join_err) => return Err(ClusterError::Store(hs_kv::KvError::backend(join_err))),
        }

        let (shard_map_tx, shard_map_rx) = watch::channel(Arc::new(ShardMap::default()));
        let (events, _) = broadcast::channel(1024);
        let (stop, stop_rx) = watch::channel(false);

        let manager = Arc::new(Self {
            me: config.me.clone(),
            generation: Generation::fresh(None),
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
            heartbeat_seq: now, // monotonically increasing enough for liveness purposes
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
            if *stop.borrow() {
                return;
            }
            self.tick().await;
        }
    }

    async fn tick(&self) {
        let state = if self.draining.load(Ordering::SeqCst) {
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

        self.converge(&live, self_heartbeat_fresh).await;
    }

    async fn converge(&self, live: &[ReplicaId], self_heartbeat_fresh: bool) {
        let am_i_live = live.contains(&self.me);
        let store = self.store.clone();
        let rows = tokio::task::spawn_blocking(move || store.list_shards()).await;
        let Ok(Ok(rows)) = rows else { return };
        let rows_by_shard: HashMap<ShardId, ShardRecord> = rows.into_iter().collect();

        let draining = self.draining.load(Ordering::SeqCst);
        let mut new_map = HashMap::new();

        for shard in self.config.layout.all_shards() {
            let row = rows_by_shard
                .get(&shard)
                .cloned()
                .unwrap_or_else(ShardRecord::initial);
            if let Some((owner, _)) = &row.owner {
                new_map.insert(shard, owner.clone());
            }

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

            if i_want_it && !i_hold_it {
                self.try_acquire(shard).await;
            } else if !i_want_it && i_hold_it {
                self.release(shard).await;
            }
        }

        let _ = self
            .shard_map_tx
            .send(Arc::new(ShardMap { owners: new_map }));
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
        self.owned
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&shard);
        self.metrics
            .record_ownership_change(shard.kind.as_str(), ChurnReason::Lost);
        self.metrics.record_fenced(shard.kind.as_str());
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
        let store = self.store.clone();
        let me = self.me.clone();
        let generation = self.generation;
        let _ = tokio::task::spawn_blocking(move || store.remove_replica(&me, generation)).await;
        let _ = self.stop.send(true);

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
    use tokio::time::advance;

    use super::*;
    use crate::types::ShardLayout;

    fn config(me: &str) -> ClusterConfig {
        let mut c = ClusterConfig::new(ReplicaId::new(me), "127.0.0.1:0", ShardLayout::small(4));
        c.heartbeat_interval = Duration::from_millis(50);
        c.lease_ttl = Duration::from_millis(150);
        c
    }

    /// Lets every background task (including ones parked behind a `spawn_blocking` join, which
    /// resolves on a real OS thread and needs a few real executor polls to be noticed) run to a
    /// fixed point, then advances the paused virtual clock by `step` and repeats `rounds` times.
    /// `spawn_blocking`'s completion is a real-time event layered under tokio's virtual clock, so
    /// a single `advance()` is not enough to observe it -- both are needed together.
    /// Advances virtual time until `condition` holds, up to `max_rounds`, and reports whether it
    /// ever did.
    ///
    /// This replaced a helper that advanced a fixed number of rounds and then hoped the
    /// background loop had got far enough — which depends on how the runtime happened to schedule
    /// 64 yields. That is generous on an idle laptop and not on a contended CI runner, where
    /// three of these tests failed while passing locally every single time. Waiting for the
    /// condition itself removes the guess rather than enlarging it, so use this before asserting
    /// on anything a background task produces.
    pub(crate) async fn settle_until(
        step: Duration,
        max_rounds: u32,
        mut condition: impl FnMut() -> bool,
    ) -> bool {
        for _ in 0..max_rounds {
            if condition() {
                return true;
            }
            advance(step).await;
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            // Yielding under paused virtual time lets *async* tasks run, but this crate's
            // acquisition path parks on `spawn_blocking`, which finishes on a real OS thread and
            // therefore needs real wall-clock time. Sleeping on the blocking pool gives it some
            // without blocking the runtime — the difference between a test that passes on an idle
            // machine and one that also passes on a contended CI runner, which is where this
            // failed on arm64 even with fifty rounds of virtual time.
            let _ =
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(2))).await;
        }
        condition()
    }

    #[tokio::test(start_paused = true)]
    async fn single_replica_acquires_every_shard() {
        let backend = MemoryBackend::new();
        let (mgr, _handle) = KvOwnership::start(config("hs-0"), backend).await.unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 50, || {
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
    async fn two_replicas_partition_the_shard_space_without_overlap() {
        let backend = MemoryBackend::new();
        let (a, _ha) = KvOwnership::start(config("hs-0"), backend.clone())
            .await
            .unwrap();
        let (b, _hb) = KvOwnership::start(config("hs-1"), backend).await.unwrap();
        assert!(
            settle_until(Duration::from_millis(60), 50, || {
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
            settle_until(Duration::from_millis(60), 50, || {
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
}
