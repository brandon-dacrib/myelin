//! The fencing epoch: how a shard owner proves, inside its own transaction, that it still holds
//! the shard.
//!
//! This is built entirely on `hs-kv`'s serializable snapshot isolation (see
//! `crates/hs-kv/src/lib.rs`'s crate docs, "Reads: snapshots and transactions"): a transaction
//! that reads a key and later commits successfully is a guarantee the key did not change out from
//! under it. [`Fence::check`] reads the shard's ownership row inside the *caller's* transaction
//! (adding it to that transaction's read set) and compares the epoch to the one this [`Fence`]
//! was issued for. If a different replica has since acquired the shard, [`crate::store::ClusterStore::acquire_shard`]
//! already wrote a new epoch to that row, so either:
//!
//! - the comparison here already sees the new epoch and fails immediately, or
//! - the two transactions race and this one's commit conflicts (`hs_kv::Conflict`), because the
//!   acquire's write and this read are on the same key.
//!
//! Either way the stale owner cannot commit a write against data it no longer owns. No separate
//! fencing primitive exists, and none is needed: `docs/rfcs/0001-cluster-ownership.md` section 6.

use hs_kv::KvRead;

use crate::error::FenceError;
use crate::store;
use crate::types::{Epoch, ShardId};

/// Proof of ownership of one shard as of one epoch. Handed to an actor when its shard is
/// acquired ([`crate::ownership::OwnershipEvent::Acquired`]); the actor calls [`Fence::check`]
/// inside every transaction it runs against that shard's data, before committing.
///
/// `epoch` is `None` in single-node mode, where there is no other writer to fence against and
/// [`Fence::check`] is always a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fence {
    /// The shard this fence covers.
    pub shard: ShardId,
    /// The epoch held, or `None` in single-node mode.
    pub epoch: Option<Epoch>,
}

impl Fence {
    /// Builds a fence for a clustered acquisition.
    #[must_use]
    pub fn clustered(shard: ShardId, epoch: Epoch) -> Self {
        Self {
            shard,
            epoch: Some(epoch),
        }
    }

    /// Builds the always-valid fence single-node mode hands out.
    #[must_use]
    pub fn inert(shard: ShardId) -> Self {
        Self { shard, epoch: None }
    }

    /// Reads the shard's ownership row inside `txn` (via `keyspace`, the same keyspace handle
    /// [`crate::store::ClusterStore::shard_keyspace`] returns for the backend `txn` belongs to)
    /// and checks it still shows this fence's epoch.
    ///
    /// Callers must do this as the *last* read before committing every transaction that touches
    /// the shard's data, and must commit `txn` (not a copy, not a transaction built later) so the
    /// read genuinely participates in that commit's conflict check. A cached check performed
    /// outside the transaction is not fencing.
    ///
    /// # Errors
    /// Returns [`FenceError::Fenced`] if the epoch no longer matches (a new owner has since
    /// acquired the shard), or a store error if the read itself failed. This method does not
    /// commit or roll back `txn`; the caller still must call the backend's commit and handle
    /// [`hs_kv::Conflict`] there as it would for any other transaction.
    pub fn check<K, T>(&self, txn: &T, keyspace: &K) -> Result<(), FenceError>
    where
        T: KvRead<Keyspace = K>,
    {
        let Some(held) = self.epoch else {
            // Single-node mode: there is no other writer, so there is nothing to fence against.
            return Ok(());
        };
        let current = read_epoch(txn, keyspace, self.shard).map_err(FenceError::Store)?;
        if current == Some(held) {
            Ok(())
        } else {
            Err(FenceError::Fenced {
                shard: self.shard,
                held,
                current,
            })
        }
    }
}

/// Reads a shard's current epoch inside `txn`, extending its read set the same way any other
/// `KvRead::get` would. `None` means the shard has never been acquired.
///
/// # Errors
/// Returns a store or decode error.
pub fn read_epoch<K, T>(
    txn: &T,
    keyspace: &K,
    shard: ShardId,
) -> Result<Option<Epoch>, hs_kv::KvError>
where
    T: KvRead<Keyspace = K>,
{
    match txn.get(keyspace, &store::shard_key(shard))? {
        None => Ok(None),
        Some(bytes) => Ok(Some(store::decode_epoch(&bytes)?)),
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use hs_kv::{KvBackend, TransactConfig, transact};

    use super::*;
    use crate::store::ClusterStore;
    use crate::types::{Generation, ReplicaId, ShardKind};

    #[test]
    fn fence_passes_while_epoch_is_unchanged() {
        let backend = MemoryBackend::new();
        let store = ClusterStore::open(backend.clone()).unwrap();
        let shard = ShardId::new(ShardKind::Room, 0);
        let rec = store
            .acquire_shard(shard, &ReplicaId::new("hs-0"), Generation(1), |_| false)
            .unwrap()
            .unwrap();
        let fence = Fence::clustered(shard, rec.epoch);

        let ok = transact(&backend, TransactConfig::default(), |txn| {
            fence
                .check(txn, store.shard_keyspace())
                .map_err(hs_kv::KvError::backend)
        });
        assert!(ok.is_ok());
    }

    #[test]
    fn fence_fails_after_another_replica_acquires() {
        let backend = MemoryBackend::new();
        let store = ClusterStore::open(backend.clone()).unwrap();
        let shard = ShardId::new(ShardKind::Room, 0);
        let rec = store
            .acquire_shard(shard, &ReplicaId::new("hs-0"), Generation(1), |_| false)
            .unwrap()
            .unwrap();
        let fence = Fence::clustered(shard, rec.epoch);

        // A second replica takes over (the first is judged dead).
        store
            .acquire_shard(shard, &ReplicaId::new("hs-1"), Generation(1), |_| true)
            .unwrap();

        let snap = backend.snapshot();
        let err = fence.check(&snap, store.shard_keyspace()).unwrap_err();
        assert!(matches!(err, FenceError::Fenced { .. }));
    }

    #[test]
    fn inert_fence_always_passes() {
        let backend = MemoryBackend::new();
        let store = ClusterStore::open(backend.clone()).unwrap();
        let fence = Fence::inert(ShardId::new(ShardKind::Room, 0));
        let snap = backend.snapshot();
        assert!(fence.check(&snap, store.shard_keyspace()).is_ok());
    }
}
