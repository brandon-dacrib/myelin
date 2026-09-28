//! Two replicas on a store whose every commit is slow, as one PostgreSQL round trip per shard
//! is: converging the whole layout takes longer than a lease. Found by two real `hs serve`
//! processes on one PostgreSQL (`crates/hs-cli/tests/cluster_admin.rs`), where a replica's
//! first convergence of 137 shards took 19 s against a 3 s lease: its heartbeat stopped for
//! the whole of it, and it compared its peer's liveness against an observation taken at the
//! start, so each side took shards from a live peer that still believed it held them.
//!
//! Runs on the real clock (the delay is a real `std::thread::sleep` inside the store), so it is
//! kept short.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hs_cluster::ownership::{KvOwnership, Ownership, OwnershipEvent};
use hs_cluster::store::ClusterStore;
use hs_cluster::{ClusterConfig, ReplicaId, ShardLayout};
use hs_kv::memory::MemoryBackend;
use hs_kv::{Conflict, KvBackend, KvError};

/// A backend whose every commit takes `delay` of real time.
#[derive(Clone)]
struct SlowBackend {
    inner: MemoryBackend,
    delay: Duration,
    commits: Arc<AtomicU64>,
}

impl KvBackend for SlowBackend {
    type Keyspace = <MemoryBackend as KvBackend>::Keyspace;
    type Snapshot = <MemoryBackend as KvBackend>::Snapshot;
    type Txn = <MemoryBackend as KvBackend>::Txn;

    fn keyspace(&self, name: &str) -> Result<Self::Keyspace, KvError> {
        self.inner.keyspace(name)
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.inner.snapshot()
    }

    fn begin(&self) -> Result<Self::Txn, KvError> {
        self.inner.begin()
    }

    fn commit(&self, txn: Self::Txn) -> Result<Result<(), Conflict>, KvError> {
        std::thread::sleep(self.delay);
        self.commits.fetch_add(1, Ordering::Relaxed);
        self.inner.commit(txn)
    }

    fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> hs_kv::Watch {
        self.inner.watch(keyspace, key)
    }
}

fn config(me: &str) -> ClusterConfig {
    let mut c = ClusterConfig::new(ReplicaId::new(me), "127.0.0.1:0", ShardLayout::small(4));
    c.heartbeat_interval = Duration::from_millis(50);
    c.lease_ttl = Duration::from_millis(150);
    c
}

/// Every `Lost` event received so far.
fn losses(events: &mut tokio::sync::broadcast::Receiver<OwnershipEvent>) -> usize {
    let mut n = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, OwnershipEvent::Lost { .. }) {
            n += 1;
        }
    }
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_convergence_longer_than_a_lease_takes_nothing_from_a_live_peer() {
    let backend = SlowBackend {
        inner: MemoryBackend::new(),
        delay: Duration::from_millis(20),
        commits: Arc::new(AtomicU64::new(0)),
    };
    let layout = ShardLayout::small(4);
    let shards: Vec<_> = layout.all_shards().collect();
    // One commit per shard is longer than a lease: the premise of this test.
    assert!(backend.delay * shards.len() as u32 > config("x").lease_ttl * 2);

    let (a, _ha) = KvOwnership::start(config("hs-0"), backend.clone())
        .await
        .unwrap();
    let mut a_events = a.subscribe();
    // hs-0 alone, converging on everything...
    tokio::time::sleep(Duration::from_millis(150)).await;
    // ...and hs-1 arriving in the middle of it.
    let (b, _hb) = KvOwnership::start(config("hs-1"), backend.clone())
        .await
        .unwrap();
    let mut b_events = b.subscribe();

    let store = ClusterStore::open(backend.clone()).unwrap();
    let converged = || {
        let rows = store.list_shards().unwrap();
        shards.iter().all(|s| {
            let row_owner = rows
                .iter()
                .find(|(id, _)| id == s)
                .and_then(|(_, r)| r.owner.as_ref().map(|(o, _)| o.as_str().to_owned()));
            match (a.is_mine(*s), b.is_mine(*s)) {
                (true, false) => row_owner.as_deref() == Some("hs-0"),
                (false, true) => row_owner.as_deref() == Some("hs-1"),
                _ => false,
            }
        }) && shards.iter().any(|s| a.is_mine(*s))
            && shards.iter().any(|s| b.is_mine(*s))
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !converged() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the two replicas never agreed with the store on a partition"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Stays converged: nothing flaps once agreed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(converged(), "the partition did not hold");

    assert_eq!(
        losses(&mut a_events),
        0,
        "hs-1 took a shard from hs-0 while hs-0 was alive"
    );
    assert_eq!(
        losses(&mut b_events),
        0,
        "hs-0 took a shard from hs-1 while hs-1 was alive"
    );
}
