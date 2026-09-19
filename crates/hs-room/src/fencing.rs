//! Cluster-fencing wiring for [`crate::actor::RoomActor::persist`] --
//! `docs/status/03-cluster.md` item 4, "the belt-and-braces against a stale ownership read racing
//! a real handoff."
//!
//! Track 03's routing gate (`hs-cli`'s `RoomShardGate`, `crates/hs-cli/src/cluster.rs`) is the
//! *primary* defense against the two-replica split-brain that same file's history describes: it
//! stops a non-owning replica from ever constructing a `RoomActor` for a room at all, by
//! forwarding the request over the mesh to whichever replica does own it. This module is the
//! secondary, belt-and-braces defense inside the owner's own commit, for the narrow window where
//! a real ownership handoff happens *between* the gate's check and this transaction's commit (a
//! network partition, a rolling update): without it, nothing stops a replica that has just lost
//! (or never held) the shard from still successfully writing to it if the gate's read of
//! `is_mine` happened to be racing a concurrent handoff.
//!
//! **This module alone does nothing.** `RoomActor::fencing` defaults to `None` on every
//! construction path in this crate, and `None` means `persist` behaves exactly as it did before
//! this file existed (see this crate's status file for why that default was chosen: `hs-cli`
//! holds the `hs_cluster::Cluster`/`Ownership` this needs, and wiring it in is explicitly that
//! track's line to add, not this crate's).

use std::sync::Arc;

use hs_cluster::store::ClusterStore;
use hs_cluster::{Ownership, ShardLayout};
use hs_kv::KvBackend;

/// Everything [`crate::actor::RoomActor::persist`] needs to check its cluster fence as the last
/// read before committing: which shard a room belongs to (`layout`), who currently owns it
/// (`ownership`), and the keyspace `Fence::check` reads its epoch row from inside the transaction
/// (`cluster_store`, which must be opened over the exact same backend as the
/// [`crate::registry::RoomRegistry`] this is installed on -- see
/// [`crate::registry::RoomRegistry::install_fencing`]).
pub struct RoomFencing<B: KvBackend> {
    /// Read-side ownership: `ownership.fence(shard)` is what this replica currently believes it
    /// holds for `shard`, freshly computed on every call (see `hs_cluster::ownership`'s
    /// `KvOwnership::fence`) -- not a value captured once and reused.
    pub ownership: Arc<dyn Ownership>,
    /// This cluster's shard counts, needed to compute which shard a room ID hashes to
    /// (`ShardLayout::room_shard`).
    pub layout: ShardLayout,
    /// The keyspace `hs_cluster::Fence::check` reads its epoch row from, opened over the same
    /// backend this room registry stores rooms in.
    pub cluster_store: ClusterStore<B>,
}

impl<B: KvBackend> RoomFencing<B> {
    /// Checks the fence for `room_id`'s shard inside `txn`, the exact transaction about to
    /// commit -- adding the epoch row to `txn`'s own read set, per `hs_cluster::Fence::check`'s
    /// contract, so a concurrent handoff either fails this check immediately or conflicts this
    /// transaction's commit.
    ///
    /// # Errors
    /// Returns a human-readable message on failure: either `hs_cluster::Fence::check`'s own
    /// error (a newer owner has since acquired the shard), or, if `ownership.fence(shard)` itself
    /// returns `None`, a message saying this replica no longer believes it owns the shard at all
    /// (treated identically -- either way this replica must not commit). Returned as a plain
    /// `String` rather than a richer type because the only caller
    /// ([`crate::actor::RoomActor::persist`]) must smuggle it through `hs_kv::KvError::Aborted`,
    /// which only carries a boxed `std::error::Error`; the message becomes
    /// [`crate::error::RoomError::Fenced`]'s payload.
    pub(crate) fn check<T>(&self, room_id: &str, txn: &T) -> Result<(), String>
    where
        T: hs_kv::KvRead<Keyspace = B::Keyspace>,
    {
        let shard = self.layout.room_shard(room_id);
        match self.ownership.fence(shard) {
            Some(fence) => fence
                .check(txn, self.cluster_store.shard_keyspace())
                .map_err(|e| e.to_string()),
            None => Err(format!(
                "this replica no longer owns shard {shard:?} for room {room_id}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use hs_cluster::store::ClusterStore;
    use hs_cluster::{Fence, Generation, OwnershipEvent, ReplicaId, ShardId, ShardMap};
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::actor::CreateRoomRequest;
    use crate::error::RoomError;
    use crate::identity::HomeserverIdentity;
    use crate::persist::Tables;

    /// A fake [`Ownership`] that always answers `fence()` with one fixed [`Fence`], regardless of
    /// which shard is asked about -- enough to prove `RoomFencing::check` (and, through it,
    /// `RoomActor::persist`) actually consults `Fence::check` against the real store, without
    /// needing the full async `KvOwnership` acquisition machinery in this crate's own tests.
    struct FixedFence {
        me: ReplicaId,
        fence: Option<Fence>,
    }

    impl Ownership for FixedFence {
        fn me(&self) -> &ReplicaId {
            &self.me
        }

        fn owner_of(&self, _shard: ShardId) -> Option<ReplicaId> {
            Some(self.me.clone())
        }

        fn is_mine(&self, _shard: ShardId) -> bool {
            true
        }

        fn fence(&self, _shard: ShardId) -> Option<Fence> {
            self.fence
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<OwnershipEvent> {
            tokio::sync::broadcast::channel(1).1
        }

        fn shard_map(&self) -> tokio::sync::watch::Receiver<Arc<ShardMap>> {
            tokio::sync::watch::channel(Arc::new(ShardMap::default())).1
        }
    }

    fn room_with_fencing(
        backend: MemoryBackend,
        fence: Option<Fence>,
    ) -> crate::actor::RoomActor<MemoryBackend> {
        let tables = Tables::open(&backend).unwrap();
        let identity = HomeserverIdentity::for_tests("hs1");
        let mut actor = crate::actor::RoomActor::create_room(
            backend.clone(),
            tables,
            identity,
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest::default(),
            1,
        )
        .unwrap();
        actor.set_fencing(Some(Arc::new(RoomFencing {
            ownership: Arc::new(FixedFence {
                me: ReplicaId::new("hs-a"),
                fence,
            }),
            layout: hs_cluster::ShardLayout::small(4),
            cluster_store: ClusterStore::open(backend).unwrap(),
        })));
        actor
    }

    /// A room actor with no fencing installed behaves exactly as before this feature existed --
    /// `persist` never even looks at `Fence::check`.
    #[test]
    fn no_fencing_installed_is_a_no_op() {
        let mut actor = crate::actor::RoomActor::create_room(
            MemoryBackend::new(),
            Tables::open(&MemoryBackend::new()).unwrap(),
            HomeserverIdentity::for_tests("hs1"),
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest::default(),
            1,
        )
        .unwrap();
        actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hi"}),
                None,
                2,
            )
            .expect("with no fencing installed, sending must succeed exactly as before");
    }

    /// A fence still holding the current epoch lets a write through.
    #[test]
    fn a_current_fence_allows_persist() {
        let backend = MemoryBackend::new();
        let cluster_store = ClusterStore::open(backend.clone()).unwrap();
        let shard = ShardId::new(hs_cluster::ShardKind::Room, 0);
        let record = cluster_store
            .acquire_shard(shard, &ReplicaId::new("hs-a"), Generation(1), |_| false)
            .unwrap()
            .expect("acquiring a free shard must succeed");
        let mut actor = room_with_fencing(backend, Some(Fence::clustered(shard, record.epoch)));
        actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hi"}),
                None,
                2,
            )
            .expect("a fence holding the current epoch must allow the write");
    }

    /// The belt-and-braces case this whole module exists for: a real ownership handoff (a second
    /// replica acquiring the same shard, bumping its epoch) happened after this actor's fence was
    /// issued. `RoomActor::persist` must refuse to commit with the stale fence, even though
    /// nothing about the event itself is invalid.
    #[test]
    fn a_stale_fence_after_a_real_handoff_rejects_the_write() {
        let backend = MemoryBackend::new();
        let cluster_store = ClusterStore::open(backend.clone()).unwrap();
        let shard = ShardId::new(hs_cluster::ShardKind::Room, 0);
        let stale = cluster_store
            .acquire_shard(shard, &ReplicaId::new("hs-a"), Generation(1), |_| false)
            .unwrap()
            .expect("acquiring a free shard must succeed");
        // Simulate a real handoff: a second replica takes the shard over, bumping the epoch.
        cluster_store
            .acquire_shard(shard, &ReplicaId::new("hs-b"), Generation(1), |_| true)
            .unwrap()
            .expect("a forced takeover must succeed");

        let mut actor = room_with_fencing(backend, Some(Fence::clustered(shard, stale.epoch)));
        let err = actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hi"}),
                None,
                2,
            )
            .expect_err("a stale fence after a real handoff must reject the write");
        assert!(matches!(err, RoomError::Fenced(_)), "got {err:?}");
    }

    /// `Ownership::fence` returning `None` (this replica's own view no longer includes the
    /// shard) is treated identically to a failed epoch check -- also a rejection, not a silent
    /// pass.
    #[test]
    fn ownership_reporting_no_fence_at_all_rejects_the_write() {
        let backend = MemoryBackend::new();
        let mut actor = room_with_fencing(backend, None);
        let err = actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hi"}),
                None,
                2,
            )
            .expect_err("no fence at all must reject the write, not pass silently");
        assert!(matches!(err, RoomError::Fenced(_)), "got {err:?}");
    }
}
