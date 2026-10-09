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
pub(crate) mod tests {
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

    /// The check inside the write itself for a copy of the room that is behind the store
    /// (`RoomActor::persist`): two copies of one room with the same, valid fence -- what a
    /// resident copy kept across losing and regaining a shard amounted to before the registry
    /// dropped such copies (2026-10-09) -- and the one that fell behind is refused, with the
    /// room, the position, the event there and who wrote it in the error, and the store left
    /// as it was.
    #[test]
    fn a_copy_behind_the_store_is_refused_rather_than_writing_over_a_row() {
        let backend = MemoryBackend::new();
        let cluster_store = ClusterStore::open(backend.clone()).unwrap();
        let shard = ShardId::new(hs_cluster::ShardKind::Room, 0);
        let record = cluster_store
            .acquire_shard(shard, &ReplicaId::new("hs-a"), Generation(1), |_| false)
            .unwrap()
            .expect("acquiring a free shard must succeed");
        let fence = Some(Fence::clustered(shard, record.epoch));
        let mut stale = room_with_fencing(backend.clone(), fence);
        let room_id = stale.room_id().to_owned();
        let identity = HomeserverIdentity::for_tests("hs1");
        let mut current = crate::actor::RoomActor::load(
            backend.clone(),
            Tables::open(&backend).unwrap(),
            identity.clone(),
            &room_id,
        )
        .unwrap()
        .unwrap();
        current.set_fencing(Some(Arc::new(RoomFencing {
            ownership: Arc::new(FixedFence {
                me: ReplicaId::new("hs-b"),
                fence,
            }),
            layout: hs_cluster::ShardLayout::small(4),
            cluster_store: ClusterStore::open(backend.clone()).unwrap(),
        })));
        let message = |body: &str| serde_json::json!({"msgtype": "m.text", "body": body});
        let theirs = current
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                message("written by the current copy"),
                None,
                2,
            )
            .unwrap();
        let position = current.timeline_position(theirs.event_id()).unwrap();

        let err = stale
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                message("from a copy that is behind"),
                None,
                3,
            )
            .expect_err("a copy behind the store must not write over the current copy's row");
        let text = err.to_string();
        assert!(matches!(err, RoomError::Internal(_)), "got {err:?}");
        for needle in [
            "behind the store",
            room_id.as_str(),
            "replica hs-a (epoch",
            &format!("position {position} already holds event"),
            theirs.event_id().as_str(),
            "written by hs-b (epoch",
        ] {
            assert!(text.contains(needle), "{needle:?} is not in {text:?}");
        }

        // The store is as the current copy left it: a fresh load holds its event where it
        // was, and nothing of the refused one.
        let fresh = crate::actor::RoomActor::load(
            backend.clone(),
            Tables::open(&backend).unwrap(),
            identity,
            &room_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(fresh.timeline_position(theirs.event_id()), Some(position));
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

    /// A scripted [`Ownership`]: this replica owns exactly the room shards in `owned` (an inert
    /// fence for each, so writes to them pass) and nothing else.
    struct OwnsShards {
        me: ReplicaId,
        owned: Vec<u32>,
    }

    impl Ownership for OwnsShards {
        fn me(&self) -> &ReplicaId {
            &self.me
        }

        fn owner_of(&self, shard: ShardId) -> Option<ReplicaId> {
            if self.is_mine(shard) {
                Some(self.me.clone())
            } else {
                Some(ReplicaId::new("elsewhere"))
            }
        }

        fn is_mine(&self, shard: ShardId) -> bool {
            shard.kind == hs_cluster::ShardKind::Room && self.owned.contains(&shard.index)
        }

        fn fence(&self, shard: ShardId) -> Option<Fence> {
            self.is_mine(shard).then(|| Fence::inert(shard))
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<OwnershipEvent> {
            tokio::sync::broadcast::channel(1).1
        }

        fn shard_map(&self) -> tokio::sync::watch::Receiver<Arc<ShardMap>> {
            tokio::sync::watch::channel(Arc::new(ShardMap::default())).1
        }
    }

    pub(crate) const LAYOUT: hs_cluster::ShardLayout = hs_cluster::ShardLayout::small(4);

    pub(crate) fn owning(
        backend: &MemoryBackend,
        owned: Vec<u32>,
    ) -> Arc<RoomFencing<MemoryBackend>> {
        Arc::new(RoomFencing {
            ownership: Arc::new(OwnsShards {
                me: ReplicaId::new("hs-a"),
                owned,
            }),
            layout: LAYOUT,
            cluster_store: ClusterStore::open(backend.clone()).unwrap(),
        })
    }

    fn v12() -> CreateRoomRequest {
        CreateRoomRequest {
            room_version: Some(ruma::RoomVersionId::try_from("12").unwrap()),
            ..CreateRoomRequest::default()
        }
    }

    fn create_ts(actor: &crate::actor::RoomActor<MemoryBackend>) -> i64 {
        actor
            .state_event("m.room.create", "")
            .unwrap()
            .expect("a created room has a create event")
            .json()
            .get("origin_server_ts")
            .and_then(|v| match v {
                hs_model::canonical::CanonicalJsonValue::Integer(i) => Some(*i),
                _ => None,
            })
            .expect("a create event has an integer origin_server_ts")
    }

    /// RFC 0019's retry for hash-derived ids: a version-12 room whose first create event hashes
    /// to a room shard another replica owns is rebuilt, one millisecond earlier at a time, until
    /// its id lands on a shard this replica owns -- and the room then works under that id.
    /// Before the retry existed the room was created under the first id, on the wrong replica.
    #[test]
    fn a_v12_create_whose_first_id_hashes_elsewhere_is_rebuilt_until_it_lands_here() {
        const NOW: i64 = 1_700_000_000_000;
        let alice = user_id!("@alice:hs1").to_owned();
        let unplaced = |backend: MemoryBackend| {
            crate::actor::RoomActor::create_room(
                backend.clone(),
                Tables::open(&backend).unwrap(),
                HomeserverIdentity::for_tests("hs1"),
                alice.clone(),
                v12(),
                NOW,
            )
            .unwrap()
            .room_id()
            .to_owned()
        };
        // A hash-derived id is a function of the create event alone, so the first attempt's id
        // is known ahead: the one a create with no placement at all produces.
        let first = unplaced(MemoryBackend::new());
        assert_eq!(
            first,
            unplaced(MemoryBackend::new()),
            "v12 ids are deterministic"
        );
        let foreign = LAYOUT.room_shard(first.as_str()).index;
        let owned: Vec<u32> = (0..LAYOUT.rooms).filter(|i| *i != foreign).collect();

        let backend = MemoryBackend::new();
        let mut actor = crate::actor::RoomActor::create_room_placed(
            backend.clone(),
            Tables::open(&backend).unwrap(),
            HomeserverIdentity::for_tests("hs1"),
            alice.clone(),
            v12(),
            NOW,
            Some(owning(&backend, owned.clone())),
        )
        .expect("a replica owning three of four shards places a room on one of them");

        let placed = actor.room_id().to_owned();
        assert_ne!(placed, first, "the first id hashed to a foreign shard");
        assert!(
            owned.contains(&LAYOUT.room_shard(placed.as_str()).index),
            "{placed} must hash to a shard this replica owns"
        );
        let ts = create_ts(&actor);
        assert!(
            ts < NOW && ts > NOW - i64::from(crate::actor::MAX_ID_ATTEMPTS_PER_SHARD * 4),
            "the create event moved earlier, within the bound: {ts}"
        );
        actor
            .send_event(
                alice,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hi"}),
                None,
                NOW + 1,
            )
            .expect("the placed room takes writes under its fence");
    }

    /// The same placement through the registry `/createRoom` calls, once `hs-cli` has installed
    /// fencing: every one of twenty version-12 rooms lands on the one shard of four this
    /// replica owns.
    #[tokio::test]
    async fn the_registry_places_every_v12_room_on_an_owned_shard() {
        let backend = MemoryBackend::new();
        let registry = crate::registry::RoomRegistry::open(
            backend.clone(),
            HomeserverIdentity::for_tests("hs1"),
        )
        .unwrap();
        registry.install_fencing(owning(&backend, vec![2]));
        for i in 0..20 {
            let handle = registry
                .create_room(
                    user_id!("@alice:hs1").to_owned(),
                    v12(),
                    1_700_000_000_000 + i,
                )
                .await
                .unwrap();
            let room_id = handle.query(|a| a.room_id().to_owned()).await;
            assert_eq!(LAYOUT.room_shard(room_id.as_str()).index, 2, "{room_id}");
        }
    }

    /// An opaque id the handler mints itself (no pre-assigned one: an admin-created room, a
    /// server-notices room) is minted again until it hashes to an owned shard; one the caller
    /// chose (the gate's pre-assigned id) is used as given.
    #[test]
    fn an_opaque_id_is_minted_on_an_owned_shard_and_a_chosen_one_is_kept() {
        for _ in 0..20 {
            let backend = MemoryBackend::new();
            let actor = crate::actor::RoomActor::create_room_placed(
                backend.clone(),
                Tables::open(&backend).unwrap(),
                HomeserverIdentity::for_tests("hs1"),
                user_id!("@alice:hs1").to_owned(),
                CreateRoomRequest::default(),
                1,
                Some(owning(&backend, vec![1])),
            )
            .unwrap();
            assert_eq!(LAYOUT.room_shard(actor.room_id().as_str()).index, 1);
        }

        let chosen = ruma::RoomId::new_v1(ruma::server_name!("hs1"));
        let elsewhere: Vec<u32> = (0..LAYOUT.rooms)
            .filter(|i| *i != LAYOUT.room_shard(chosen.as_str()).index)
            .collect();
        let backend = MemoryBackend::new();
        // The create event of a chosen id on a shard not owned here is still fenced like every
        // write, so nothing is persisted: the chosen id is not replaced by another.
        let err = crate::actor::RoomActor::create_room_placed(
            backend.clone(),
            Tables::open(&backend).unwrap(),
            HomeserverIdentity::for_tests("hs1"),
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                room_id: Some(chosen.clone()),
                ..CreateRoomRequest::default()
            },
            1,
            Some(owning(&backend, elsewhere)),
        )
        .err()
        .expect("a chosen id on a foreign shard is fenced, not swapped for another");
        assert!(matches!(err, RoomError::Fenced(_)), "got {err:?}");
    }

    /// A replica that owns no room shard at all cannot place a room anywhere: the bounded retry
    /// runs out and the create is refused (`503`, which the client retries), rather than
    /// spinning or building the room on a shard somebody else owns.
    #[test]
    fn a_replica_owning_no_room_shard_refuses_the_create() {
        let backend = MemoryBackend::new();
        let err = crate::actor::RoomActor::create_room_placed(
            backend.clone(),
            Tables::open(&backend).unwrap(),
            HomeserverIdentity::for_tests("hs1"),
            user_id!("@alice:hs1").to_owned(),
            v12(),
            1_700_000_000_000,
            Some(owning(&backend, Vec::new())),
        )
        .err()
        .expect("no owned shard, no room");
        assert!(matches!(err, RoomError::Fenced(_)), "got {err:?}");
    }
}
