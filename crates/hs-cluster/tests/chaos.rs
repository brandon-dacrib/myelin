//! The in-process chaos harness (`docs/rfcs/0001-cluster-ownership.md` section 14).
//!
//! A toy replicated-log actor (`ChaosLog`) that any number of `KvOwnership<MemoryBackend>`
//! replicas share, plus fault injection (replica death via aborting its background task, a
//! partitioned/stalled store via [`FaultyBackend`]), running on the `tokio` paused clock for
//! deterministic timing. Every `ChaosLog::append` call is a real `hs_kv` transaction that calls
//! [`hs_cluster::Fence::check`] exactly as a production actor (track 04's room actor, for
//! example) is required to -- this is what makes the safety checks below meaningful rather than
//! assumed.
//!
//! What Phase 0's definition of done asks this file to demonstrate:
//! - no two replicas ever commit writes for the same shard at the same epoch (the epoch makes a
//!   stale owner's commit abort);
//! - failover completes within the configured lease TTL;
//! - no lost or duplicated writes under retries (the idempotency key, persisted in the same
//!   transaction as the effect, per RFC 0001 section 8's durable rule);
//! - a graceful handoff hands shards to a live peer before the draining replica stops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hs_cluster::ownership::{Drainable, KvOwnership, Ownership};
use hs_cluster::store::ClusterStore;
use hs_cluster::{ClusterConfig, Fence, ReplicaId, ShardId, ShardKind, ShardLayout};
use hs_kv::memory::MemoryBackend;
use hs_kv::{Conflict, KvBackend, KvError, KvRead, KvWrite, TransactConfig, transact};
use serde::{Deserialize, Serialize};

/// A backend wrapper that can be "cut" (partitioned): while cut, `begin`/`commit` fail, exactly
/// as a replica that has lost its connection to a shared PostgreSQL cluster would see connection
/// errors. `snapshot`/`keyspace`/`watch` keep working, which is deliberate -- it models a
/// one-way partition (a replica that can still be reached for reads but cannot commit), which is
/// enough to exercise the failure-detection and fencing paths without needing to also fake read
/// unavailability.
#[derive(Clone)]
struct FaultyBackend<B: KvBackend> {
    inner: B,
    cut: Arc<AtomicBool>,
}

impl<B: KvBackend> FaultyBackend<B> {
    fn new(inner: B) -> Self {
        Self {
            inner,
            cut: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set_cut(&self, cut: bool) {
        self.cut.store(cut, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct PartitionedError;
impl std::fmt::Display for PartitionedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store unreachable (simulated partition)")
    }
}
impl std::error::Error for PartitionedError {}

impl<B: KvBackend> KvBackend for FaultyBackend<B> {
    type Keyspace = B::Keyspace;
    type Snapshot = B::Snapshot;
    type Txn = B::Txn;

    fn keyspace(&self, name: &str) -> Result<Self::Keyspace, KvError> {
        self.inner.keyspace(name)
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.inner.snapshot()
    }

    fn begin(&self) -> Result<Self::Txn, KvError> {
        if self.cut.load(Ordering::SeqCst) {
            return Err(KvError::backend(PartitionedError));
        }
        self.inner.begin()
    }

    fn commit(&self, txn: Self::Txn) -> Result<Result<(), Conflict>, KvError> {
        if self.cut.load(Ordering::SeqCst) {
            return Err(KvError::backend(PartitionedError));
        }
        self.inner.commit(txn)
    }

    fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> hs_kv::Watch {
        self.inner.watch(keyspace, key)
    }
}

/// One committed record in the toy replicated log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LogEntry {
    shard: String,
    epoch: u64,
    writer: String,
    seq: u64,
}

/// The toy replicated-log actor: appends are ordinary `hs_kv` transactions that fence on the
/// shard's epoch and de-duplicate by idempotency key, exactly per RFC 0001 sections 6 and 8.
struct ChaosLog<B: KvBackend> {
    backend: B,
    entries: B::Keyspace,
    idem: B::Keyspace,
}

impl<B: KvBackend> ChaosLog<B> {
    fn open(backend: B) -> Self {
        let entries = backend
            .keyspace("chaos_entries")
            .expect("open entries keyspace");
        let idem = backend.keyspace("chaos_idem").expect("open idem keyspace");
        Self {
            backend,
            entries,
            idem,
        }
    }

    /// Attempts to append one record for `shard`, fenced by `fence`. Returns `Ok((entry,
    /// was_duplicate))` on success (`was_duplicate` true if `idem_key` had already been
    /// committed, in which case the *original* entry is returned, never a new one), or `Err` if
    /// the fence rejected the write (a stale owner) or the store itself failed.
    fn append(
        &self,
        fence: &hs_cluster::Fence,
        shard_keyspace: &B::Keyspace,
        shard: ShardId,
        writer: &ReplicaId,
        idem_key: u128,
    ) -> Result<(LogEntry, bool), String> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            fence.check(txn, shard_keyspace).map_err(KvError::backend)?;

            let idem_bytes = format!("idem/{shard}/{idem_key:032x}").into_bytes();
            if let Some(existing) = txn.get(&self.idem, &idem_bytes)? {
                let entry: LogEntry =
                    serde_json::from_slice(&existing).map_err(KvError::backend)?;
                return Ok((entry, true));
            }

            let seq_key = format!("seq/{shard}").into_bytes();
            let seq = txn.atomic_add(&self.entries, &seq_key, 1)?;
            let entry = LogEntry {
                shard: shard.to_string(),
                epoch: fence.epoch.map(|e| e.0).unwrap_or(0),
                writer: writer.to_string(),
                seq: seq as u64,
            };
            let entry_bytes = serde_json::to_vec(&entry).map_err(KvError::backend)?;
            let entry_key = format!("entry/{shard}/{seq:010}").into_bytes();
            txn.put(&self.entries, &entry_key, &entry_bytes)?;
            txn.put(&self.idem, &idem_bytes, &entry_bytes)?;
            Ok((entry, false))
        })
        .map_err(|e| e.to_string())
    }

    /// Every committed entry for `shard`, in commit order.
    fn entries_for(&self, shard: ShardId) -> Vec<LogEntry> {
        let snap = self.backend.snapshot();
        let prefix = format!("entry/{shard}/").into_bytes();
        let mut out = Vec::new();
        for item in snap.range(&self.entries, hs_kv::RangeSpec::prefix(prefix)) {
            let (_, value) = item.expect("range read");
            out.push(serde_json::from_slice(&value).expect("decode entry"));
        }
        out
    }
}

fn test_config(me: &str, layout: ShardLayout) -> ClusterConfig {
    let mut c = ClusterConfig::new(ReplicaId::new(me), format!("{me}:0"), layout);
    c.heartbeat_interval = Duration::from_millis(50);
    c.lease_ttl = Duration::from_millis(150);
    c
}

/// Lets background tasks (including ones parked behind a `spawn_blocking` join, which resolves
/// on a real OS thread) make progress, then advances the paused virtual clock and repeats.
async fn settle(step: Duration, rounds: u32) {
    for _ in 0..rounds {
        tokio::time::advance(step).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }
}

/// Like [`settle`], but stops as soon as `condition` holds and reports whether it ever did.
///
/// Use this before asserting on anything a background task produces. A fixed round count assumes
/// the acquisition loop gets far enough within however many times the runtime chooses to poll it,
/// which holds on an idle machine and does not on a contended CI runner — three of the tests in
/// this file failed there while passing locally every time, all of them waiting for the same
/// thing: the initial claim of a share of the shards.
async fn settle_until(
    step: Duration,
    max_rounds: u32,
    mut condition: impl FnMut() -> bool,
) -> bool {
    for _ in 0..max_rounds {
        if condition() {
            return true;
        }
        tokio::time::advance(step).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        // See the note in `hs_cluster::ownership`'s copy: yields under paused time run async
        // tasks, but the acquisition path finishes on a blocking thread that needs real time.
        let _ = tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(2))).await;
    }
    condition()
}

/// Tries to append to `shard` on whichever of `replicas` currently believes it owns it. Returns
/// `None` if no replica currently claims ownership (a transient gap during failover).
fn append_via_current_owner<B: KvBackend>(
    replicas: &[Arc<KvOwnership<B>>],
    log: &ChaosLog<B>,
    shard: ShardId,
    idem_key: u128,
) -> Option<Result<(LogEntry, bool), String>> {
    let owner = replicas.iter().find(|r| r.is_mine(shard))?;
    let fence = owner.fence(shard)?;
    Some(log.append(
        &fence,
        owner.store().shard_keyspace(),
        shard,
        owner.me(),
        idem_key,
    ))
}

#[tokio::test(start_paused = true)]
async fn no_two_replicas_ever_commit_the_same_shard_epoch() {
    let backend = MemoryBackend::new();
    let layout = ShardLayout::small(6);
    let log = ChaosLog::open(backend.clone());
    // An observer store, independent of any replica's ownership, used only to fetch the shard
    // keyspace handle for the adversarial stale-fence writes below.
    let observer_store = ClusterStore::open(backend.clone()).unwrap();

    let mut replicas = Vec::new();
    let mut handles = Vec::new();
    for i in 0..4 {
        let (mgr, handle) =
            KvOwnership::start(test_config(&format!("hs-{i}"), layout), backend.clone())
                .await
                .unwrap();
        replicas.push(mgr);
        handles.push(handle);
    }
    settle(Duration::from_millis(60), 6).await;

    // Chaos: repeatedly append to random shards, occasionally killing a replica (simulating a
    // crash) and letting the survivors converge, across many rounds.
    let shards: Vec<ShardId> = layout.all_shards().collect();
    let mut idem = 0u128;

    // Captured at the moment of the kill below: the doomed replica's identity and its fence for
    // every shard it owned at that instant. A *graceful* kill (abort the task, stop using it, as
    // this test did before) never exercises the fence adversarially, because nothing ever
    // attempts another write with a stale fence once the replica is out of rotation -- so a
    // mutation that disabled `Fence::check` entirely would sail through undetected here, even
    // though this is the test whose name claims to guard exactly that property. Keeping the
    // stale fence around and retrying writes with it after ownership has moved on (an
    // *ungraceful* loss: the process is gone, but a caller who was mid-retry with its last-known
    // fence is not) is what actually exercises the guarantee.
    let mut stale_writer: Option<ReplicaId> = None;
    let mut stale_fences: Vec<(ShardId, Fence)> = Vec::new();
    let mut adversarial_checks = 0u32;

    for round in 0..40u32 {
        for &shard in &shards {
            idem += 1;
            let _ = append_via_current_owner(&replicas, &log, shard, idem);
        }

        if round == 15 && !replicas.is_empty() {
            let doomed = &replicas[0];
            stale_fences = shards
                .iter()
                .filter_map(|&s| doomed.fence(s).map(|f| (s, f)))
                .collect();
            assert!(
                !stale_fences.is_empty(),
                "the doomed replica should own at least one shard for this test to be meaningful"
            );
            stale_writer = Some(doomed.me().clone());
            // Ungraceful loss: abort the task so it can never heartbeat or re-converge (there is
            // no drain, no release), but keep its captured identity and fences and keep using
            // them below.
            handles.remove(0).abort();
            replicas.remove(0);
        }

        // Once the doomed replica is gone, keep attacking every shard it used to own with its
        // stale, pre-kill fence on every remaining round. A fence is only "stale" once some
        // other replica has actually re-acquired the shard (bumping its epoch) -- until then the
        // captured fence is still the current one and a write with it is legitimate, not
        // adversarial, so the check below only fires once the store shows the epoch has moved
        // past what the doomed replica held. Every attempt made *after* that point must be
        // rejected; since `ChaosLog::append` only mutates the store on success, this also proves
        // no entry from the stale writer's post-failover attempts ever reaches the log.
        if let Some(writer) = &stale_writer {
            for (shard, fence) in &stale_fences {
                let Some(held) = fence.epoch else { continue };
                let current = observer_store.get_shard(*shard).expect("read shard row");
                if current.epoch <= held {
                    continue; // not fenced yet: no other replica has taken this shard over
                }
                idem += 1;
                let result =
                    log.append(fence, observer_store.shard_keyspace(), *shard, writer, idem);
                assert!(
                    result.is_err(),
                    "shard {shard}: a write using the stale, pre-failover fence must be rejected once a new owner has taken over, got {result:?}"
                );
                adversarial_checks += 1;
            }
        }

        settle(Duration::from_millis(40), 1).await;
    }
    settle(Duration::from_millis(200), 6).await;

    assert!(
        adversarial_checks > 0,
        "the stale-fence check never actually fired (no shard the doomed replica owned was ever \
         re-acquired by a survivor) -- this test would pass vacuously without checking anything"
    );

    // Safety invariant: for every shard, every committed epoch has exactly one writer, and
    // epochs are non-decreasing in commit order. This is the property the fencing epoch exists
    // to guarantee (RFC 0001 section 6): a stale owner's transaction reads the epoch row, a new
    // owner's acquire bumps it, and the two cannot both commit in an order where the stale write
    // lands after the takeover.
    for &shard in &shards {
        let entries = log.entries_for(shard);
        let mut epoch_writers: HashMap<u64, HashSet<String>> = HashMap::new();
        let mut last_epoch = 0u64;
        for e in &entries {
            assert!(
                e.epoch >= last_epoch,
                "shard {shard}: epoch went backwards ({e:?})"
            );
            last_epoch = e.epoch;
            epoch_writers
                .entry(e.epoch)
                .or_default()
                .insert(e.writer.clone());
        }
        for (epoch, writers) in &epoch_writers {
            assert_eq!(
                writers.len(),
                1,
                "shard {shard} epoch {epoch} was written by more than one replica: {writers:?}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn failover_completes_within_configured_ttl() {
    let backend = MemoryBackend::new();
    let layout = ShardLayout::small(2);
    let config_a = test_config("hs-0", layout);
    let lease_ttl = config_a.lease_ttl;
    let heartbeat_interval = config_a.heartbeat_interval;

    let (a, handle_a) = KvOwnership::start(config_a, backend.clone()).await.unwrap();
    let (b, _handle_b) = KvOwnership::start(test_config("hs-1", layout), backend.clone())
        .await
        .unwrap();
    assert!(
        settle_until(Duration::from_millis(60), 60, || layout
            .all_shards()
            .any(|s| a.is_mine(s)))
        .await,
        "hs-0 never claimed a shard"
    );

    let shard = layout
        .all_shards()
        .find(|s| a.is_mine(*s))
        .expect("hs-0 should own something");
    assert!(!b.is_mine(shard));

    // Kill the owner and measure how long the survivor takes to notice and take over.
    handle_a.abort();
    let start = tokio::time::Instant::now();
    let deadline = lease_ttl + heartbeat_interval * 3; // detection window plus one tick's slack
    loop {
        settle(Duration::from_millis(20), 1).await;
        if b.is_mine(shard) {
            break;
        }
        assert!(
            tokio::time::Instant::now() - start < deadline * 3,
            "failover did not complete within a generous multiple of the configured TTL"
        );
    }
    let elapsed = tokio::time::Instant::now() - start;
    assert!(
        elapsed <= deadline * 3,
        "failover took {elapsed:?}, expected within roughly {deadline:?}"
    );
    assert!(b.fence(shard).is_some());
}

#[tokio::test(start_paused = true)]
async fn retried_append_is_not_duplicated() {
    let backend = MemoryBackend::new();
    let layout = ShardLayout::small(2);
    let log = ChaosLog::open(backend.clone());
    let (a, _handle) = KvOwnership::start(test_config("hs-0", layout), backend.clone())
        .await
        .unwrap();
    let shard = ShardId::new(ShardKind::Room, 0);
    assert!(
        settle_until(Duration::from_millis(60), 60, || a.is_mine(shard)).await,
        "hs-0 never took ownership of {shard}"
    );
    let fence = a.fence(shard).unwrap();
    let key = 0xDEADBEEFu128;

    let (first, dup1) = log
        .append(&fence, a.store().shard_keyspace(), shard, a.me(), key)
        .unwrap();
    assert!(!dup1);
    // Simulate a client retry after a lost reply: same idempotency key, called again.
    let (second, dup2) = log
        .append(&fence, a.store().shard_keyspace(), shard, a.me(), key)
        .unwrap();
    assert!(dup2, "the retry should have been recognized as a duplicate");
    assert_eq!(
        first, second,
        "a retried append must return the original effect, not a new one"
    );

    let committed = log.entries_for(shard);
    assert_eq!(
        committed.len(),
        1,
        "exactly one entry should have been committed for the one idempotency key"
    );
}

#[tokio::test(start_paused = true)]
async fn drain_hands_off_to_a_live_peer_before_stopping() {
    let backend = MemoryBackend::new();
    let layout = ShardLayout::small(8);
    let (a, _handle_a) = KvOwnership::start(test_config("hs-0", layout), backend.clone())
        .await
        .unwrap();
    let (b, _handle_b) = KvOwnership::start(test_config("hs-1", layout), backend.clone())
        .await
        .unwrap();
    assert!(
        settle_until(Duration::from_millis(60), 60, || layout
            .all_shards()
            .any(|s| a.is_mine(s)))
        .await,
        "hs-0 never claimed a share of the shards"
    );

    let a_owned_before: Vec<_> = layout.all_shards().filter(|s| a.is_mine(*s)).collect();
    assert!(
        !a_owned_before.is_empty(),
        "hs-0 should own a share of the shards"
    );

    // Run `b`'s convergence loop concurrently with `a`'s drain, so `b` can actually claim what
    // `a` releases -- this is what "handoff completes before a replica stops" means with a live
    // peer, as opposed to the single-replica case (covered in `ownership.rs`'s unit tests) where
    // there is nobody to hand off to.
    let settler = tokio::spawn(async move {
        settle(Duration::from_millis(50), 200).await;
    });
    let report = a.drain(Duration::from_secs(10)).await;
    settler.await.unwrap();

    assert!(
        report.handed_off > 0,
        "at least some shards should have been handed off to the live peer"
    );
    for shard in &a_owned_before {
        assert!(
            !a.is_mine(*shard),
            "hs-0 must not still own {shard} after draining"
        );
    }
    // Every shard hs-0 used to own has now converged onto `b` (the only other live replica).
    assert!(
        settle_until(Duration::from_millis(50), 100, || a_owned_before
            .iter()
            .all(|s| b.is_mine(*s)))
        .await,
        "the survivor never took over every shard the drained replica held"
    );
    for shard in &a_owned_before {
        assert!(
            b.is_mine(*shard),
            "shard {shard} should have converged onto hs-1"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_partitioned_replica_cannot_write_after_being_fenced() {
    let real_backend = MemoryBackend::new();
    let faulty = FaultyBackend::new(real_backend.clone());
    let layout = ShardLayout::small(2);
    let log = ChaosLog::open(real_backend.clone());

    let (a, _handle_a) = KvOwnership::start(test_config("hs-0", layout), faulty.clone())
        .await
        .unwrap();
    let (b, _handle_b) = KvOwnership::start(test_config("hs-1", layout), real_backend.clone())
        .await
        .unwrap();
    assert!(
        settle_until(Duration::from_millis(60), 60, || layout
            .all_shards()
            .any(|s| a.is_mine(s)))
        .await,
        "hs-0 never claimed a shard to be fenced from"
    );

    let shard = layout
        .all_shards()
        .find(|s| a.is_mine(*s))
        .expect("hs-0 should own something");
    let fence_a = a.fence(shard).unwrap();

    // Partition hs-0 from the store: it stops being able to heartbeat or commit, exactly like a
    // stalled connection to a shared PostgreSQL cluster (RFC 0001's risk: "store stalls
    // masquerading as owner death").
    faulty.set_cut(true);

    // hs-1 eventually judges hs-0 dead (its heartbeat_seq stops advancing) and takes the shard.
    let mut took_over = false;
    for _ in 0..30 {
        settle(Duration::from_millis(20), 1).await;
        if b.is_mine(shard) {
            took_over = true;
            break;
        }
    }
    assert!(
        took_over,
        "hs-1 should have taken over the partitioned replica's shard"
    );

    // hs-0's old fence is now stale. Even though hs-0 still believes (from its last successful
    // read) that it might own the shard, an attempted write with the old fence must be rejected
    // -- this is the fencing epoch doing its job, not a liveness check.
    let result = log.append(&fence_a, a.store().shard_keyspace(), shard, a.me(), 999);
    assert!(
        result.is_err(),
        "a write fenced against the old epoch must not commit"
    );

    // And the new owner keeps making progress for as long as hs-0 stays stalled -- a partition
    // of one replica must not stall the shard, only move it (RFC 0001's risk paragraph again:
    // fencing makes the stall safe, but the rest of the cluster is not supposed to wait on it).
    // hs-0 remains cut for every one of these; each write re-derives hs-1's current fence rather
    // than reusing one value, since a real caller would too.
    for i in 0..5u128 {
        let fence_b = b
            .fence(shard)
            .expect("hs-1 should hold a fresh fence while hs-0 stays partitioned");
        let ok = log.append(
            &fence_b,
            b.store().shard_keyspace(),
            shard,
            b.me(),
            1000 + i,
        );
        assert!(
            ok.is_ok(),
            "hs-1 should keep committing while hs-0 is partitioned: {ok:?}"
        );
    }

    // hs-0's stale fence is still rejected after hs-1 has made further progress, not just at the
    // moment of takeover -- the rejection is a property of the epoch, not a one-shot check.
    let still_rejected = log.append(&fence_a, a.store().shard_keyspace(), shard, a.me(), 2000);
    assert!(
        still_rejected.is_err(),
        "hs-0's stale fence must remain rejected even after hs-1 has committed further writes"
    );
}
