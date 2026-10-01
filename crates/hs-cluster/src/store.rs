//! The cluster store: replica registry, shard ownership rows and the shard layout, built
//! directly on [`hs_kv::KvBackend`].
//!
//! There is deliberately no separate `LeaseStore` trait here. Track 01's `hs-kv` guarantees full
//! serializable snapshot isolation on every backend (point reads, multi-gets and range scans all
//! extend a transaction's read set, and a concurrent write to anything in that set aborts the
//! commit). That is exactly the fencing primitive the day-one design of this crate expected to
//! need to build itself: a shard's ownership row is changed by an ordinary read-modify-write
//! transaction, and it is impossible for two transactions to both believe they changed it from
//! the same starting state, because the loser's commit conflicts. `hs_kv::transact` retries by
//! calling the closure again from scratch, which re-reads the row and re-decides -- exactly the
//! "acquire" and "release" state machines below. See `docs/rfcs/0001-cluster-ownership.md`
//! section 6 and the reconciliation note at its top.

use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};

use crate::error::ClusterError;
use crate::types::{
    DrainRequest, Epoch, Generation, ReplicaId, ReplicaRecord, ShardId, ShardLayout, ShardRecord,
};

const REPLICAS_KEYSPACE: &str = "cluster_replicas";
const SHARDS_KEYSPACE: &str = "cluster_shards";
const LAYOUT_KEYSPACE: &str = "cluster_layout";
const LAYOUT_KEY: &[u8] = b"layout";
const REPLICA_PREFIX: &str = "replica/";
const SHARD_PREFIX: &str = "shard/";
/// Drain requests live in the replicas keyspace under their own prefix, so
/// [`ClusterStore::list_replicas`]'s `replica/` range never sees them.
const DRAIN_PREFIX: &str = "drain/";
/// The last `heartbeat_seq` a deregistered replica wrote, kept under its own prefix after
/// [`ClusterStore::remove_replica`] deletes the registry row, so the replica's next process
/// continues above it ([`ClusterStore::heartbeat_seq_floor`]).
const SEQ_PREFIX: &str = "seq/";

fn seq_key(id: &ReplicaId) -> Vec<u8> {
    format!("{SEQ_PREFIX}{}", id.as_str()).into_bytes()
}

fn drain_key(id: &ReplicaId) -> Vec<u8> {
    format!("{DRAIN_PREFIX}{}", id.as_str()).into_bytes()
}

fn replica_key(id: &ReplicaId) -> Vec<u8> {
    format!("{REPLICA_PREFIX}{}", id.as_str()).into_bytes()
}

/// The stable, byte-ordered key for a shard's ownership row. Zero-padded so byte order matches
/// numeric order within a kind, which is not load-bearing today (shards are enumerated by prefix,
/// not ranged) but is cheap to keep true.
pub(crate) fn shard_key(shard: ShardId) -> Vec<u8> {
    format!("{SHARD_PREFIX}{}/{:010}", shard.kind.as_str(), shard.index).into_bytes()
}

fn decode<T: serde::de::DeserializeOwned>(
    what: &'static str,
    bytes: &[u8],
) -> Result<T, ClusterError> {
    serde_json::from_slice(bytes).map_err(|source| ClusterError::Decode { what, source })
}

/// Same decode as [`decode`], but for use inside an `hs_kv::transact` closure, whose error type
/// is fixed by the `hs-kv` API to [`hs_kv::KvError`]. The `transact`/`.map_err(ClusterError::Store)`
/// wrapping at each call site turns this back into a `ClusterError` for the caller; the decode
/// failure reason is preserved in the error message either way.
fn decode_kv<T: serde::de::DeserializeOwned>(
    what: &'static str,
    bytes: &[u8],
) -> Result<T, hs_kv::KvError> {
    serde_json::from_slice(bytes)
        .map_err(|source| hs_kv::KvError::backend(JsonDecodeError { what, source }))
}

#[derive(Debug)]
struct JsonDecodeError {
    what: &'static str,
    source: serde_json::Error,
}

impl std::fmt::Display for JsonDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to decode {}: {}", self.what, self.source)
    }
}

impl std::error::Error for JsonDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn encode<T: serde::Serialize>(value: &T) -> Vec<u8> {
    // Every type this module serializes (`ReplicaRecord`, `ShardRecord`, `ShardLayout`) is a
    // plain, finite struct of primitives and strings with a derived `Serialize` -- it cannot fail
    // to encode. See the workspace's "no unwrap outside tests" rule: this is the documented
    // justification for the one place that would otherwise need one.
    serde_json::to_vec(value)
        .unwrap_or_else(|_| unreachable!("cluster record types always serialize"))
}

/// Decodes a shard row's bytes into its epoch only, for the fencing read path
/// ([`crate::fence::Fence::check`]), which runs inside an arbitrary caller's `hs-kv` transaction
/// and so must report errors as [`hs_kv::KvError`] like every other in-transaction read.
pub(crate) fn decode_epoch(bytes: &[u8]) -> Result<Epoch, hs_kv::KvError> {
    decode_kv::<ShardRecord>("ShardRecord", bytes).map(|r| r.epoch)
}

/// Replica registry, shard rows and the shard layout, over one `hs-kv` backend. Cheap to clone
/// (the keyspace handles are cheap handles and `B` itself is `Clone`).
#[derive(Clone)]
pub struct ClusterStore<B: KvBackend> {
    backend: B,
    replicas: B::Keyspace,
    shards: B::Keyspace,
    layout: B::Keyspace,
}

impl<B: KvBackend> ClusterStore<B> {
    /// Opens the three cluster keyspaces on `backend`, creating them if necessary.
    ///
    /// # Errors
    /// Returns [`ClusterError::Store`] if a keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, ClusterError> {
        let replicas = backend.keyspace(REPLICAS_KEYSPACE)?;
        let shards = backend.keyspace(SHARDS_KEYSPACE)?;
        let layout = backend.keyspace(LAYOUT_KEYSPACE)?;
        Ok(Self {
            backend,
            replicas,
            shards,
            layout,
        })
    }

    /// The keyspace handle shard rows live in, for callers (actors in other crates) that need to
    /// call [`crate::fence::Fence::check`] inside their own transactions on this same backend.
    pub fn shard_keyspace(&self) -> &B::Keyspace {
        &self.shards
    }

    /// A handle to the underlying backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Registers the layout at cluster creation, or confirms it matches an already-recorded one.
    /// A replica that boots with a different layout must refuse to start (RFC 0001 section 3).
    ///
    /// # Errors
    /// Returns [`ClusterError::LayoutMismatch`] if a different layout is already recorded, or a
    /// store error.
    pub fn init_layout(&self, wanted: ShardLayout) -> Result<ShardLayout, ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            match txn.get(&self.layout, LAYOUT_KEY)? {
                None => {
                    txn.put(&self.layout, LAYOUT_KEY, &encode(&wanted))?;
                    Ok(wanted)
                }
                Some(bytes) => Ok(decode_kv::<ShardLayout>("ShardLayout", &bytes)?),
            }
        })
        .map_err(ClusterError::Store)
        .and_then(|recorded| {
            if recorded == wanted {
                Ok(recorded)
            } else {
                Err(ClusterError::LayoutMismatch {
                    requested: wanted,
                    recorded,
                })
            }
        })
    }

    /// Reads the recorded layout, if the cluster has been initialized.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn get_layout(&self) -> Result<Option<ShardLayout>, ClusterError> {
        let snap = self.backend.snapshot();
        match snap.get(&self.layout, LAYOUT_KEY)? {
            None => Ok(None),
            Some(bytes) => decode("ShardLayout", &bytes).map(Some),
        }
    }

    /// Writes this replica's heartbeat row, unless a row for the same id with a higher
    /// generation is already present (RFC 0001 section 4: "a row whose generation is lower than
    /// another row for the same id is ignored").
    ///
    /// # Errors
    /// Returns a store error.
    pub fn heartbeat(&self, rec: &ReplicaRecord) -> Result<(), ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let key = replica_key(&rec.id);
            if let Some(existing) = txn.get(&self.replicas, &key)? {
                let existing: ReplicaRecord = decode_kv("ReplicaRecord", &existing)?;
                if existing.generation > rec.generation {
                    return Ok(());
                }
            }
            txn.put(&self.replicas, &key, &encode(rec))?;
            Ok(())
        })
        .map_err(ClusterError::Store)
    }

    /// Removes this replica's row, but only if the stored generation still matches -- a newer
    /// registration (this process having restarted again, or a `Left` row already garbage
    /// collected and recreated) is left alone.
    ///
    /// The row's last `heartbeat_seq` is kept under its own key in the same transaction, so the
    /// replica's next process continues above it ([`ClusterStore::heartbeat_seq_floor`]) even
    /// though its registry row is gone.
    ///
    /// # Errors
    /// Returns a store error.
    pub fn remove_replica(
        &self,
        id: &ReplicaId,
        generation: Generation,
    ) -> Result<(), ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let key = replica_key(id);
            if let Some(existing) = txn.get(&self.replicas, &key)? {
                let existing: ReplicaRecord = decode_kv("ReplicaRecord", &existing)?;
                if existing.generation == generation {
                    let kept = match txn.get(&self.replicas, &seq_key(id))? {
                        Some(bytes) => decode_kv::<u64>("heartbeat seq", &bytes)?,
                        None => 0,
                    };
                    txn.put(
                        &self.replicas,
                        &seq_key(id),
                        &encode(&kept.max(existing.heartbeat_seq)),
                    )?;
                    txn.delete(&self.replicas, &key)?;
                }
            }
            Ok(())
        })
        .map_err(ClusterError::Store)
    }

    /// The highest `heartbeat_seq` any earlier process of replica `id` is known to have
    /// written: its registry row's (a process that stopped without draining leaves one) or the
    /// value [`ClusterStore::remove_replica`] kept when a drained process deregistered,
    /// whichever is higher; `0` if neither exists. A starting replica numbers its heartbeats
    /// from one more than this, so a restart never goes backwards. A snapshot read.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn heartbeat_seq_floor(&self, id: &ReplicaId) -> Result<u64, ClusterError> {
        let snap = self.backend.snapshot();
        let row = match snap.get(&self.replicas, &replica_key(id))? {
            Some(bytes) => decode::<ReplicaRecord>("ReplicaRecord", &bytes)?.heartbeat_seq,
            None => 0,
        };
        let kept = match snap.get(&self.replicas, &seq_key(id))? {
            Some(bytes) => decode::<u64>("heartbeat seq", &bytes)?,
            None => 0,
        };
        Ok(row.max(kept))
    }

    /// Lists every replica row. A snapshot read: does not participate in any transaction's
    /// conflict set.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn list_replicas(&self) -> Result<Vec<ReplicaRecord>, ClusterError> {
        let snap = self.backend.snapshot();
        let mut out = Vec::new();
        for item in snap.range(&self.replicas, RangeSpec::prefix(REPLICA_PREFIX.as_bytes())) {
            let (_, value) = item?;
            out.push(decode("ReplicaRecord", &value)?);
        }
        Ok(out)
    }

    /// Records that an administrator wants replica `id` to drain (RFC 0001 section 10, started
    /// from the admin API rather than by `SIGTERM`). Any replica may write it; the named
    /// replica notices at its next heartbeat, stops taking part in hashing and hands its shards
    /// off, and keeps doing so until the request is withdrawn with
    /// [`ClusterStore::withdraw_drain`] -- across restarts too, since the request is a row in the
    /// shared store rather than state in the drained process.
    ///
    /// An existing request is kept, not replaced: the first request's time, author and task
    /// stay the record of the drain in progress. Returns the request now in force and whether
    /// this call created it.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn request_drain(
        &self,
        id: &ReplicaId,
        request: &DrainRequest,
    ) -> Result<(DrainRequest, bool), ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let key = drain_key(id);
            if let Some(existing) = txn.get(&self.replicas, &key)? {
                return decode_kv::<DrainRequest>("DrainRequest", &existing).map(|r| (r, false));
            }
            txn.put(&self.replicas, &key, &encode(request))?;
            Ok((request.clone(), true))
        })
        .map_err(ClusterError::Store)
    }

    /// Replaces the drain request for replica `id` if one is in force (to attach the task that
    /// follows it, say); does nothing if there is none, so it can never resurrect a request
    /// that was withdrawn meanwhile.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn update_drain(&self, id: &ReplicaId, request: &DrainRequest) -> Result<(), ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let key = drain_key(id);
            if txn.get(&self.replicas, &key)?.is_some() {
                txn.put(&self.replicas, &key, &encode(request))?;
            }
            Ok(())
        })
        .map_err(ClusterError::Store)
    }

    /// Withdraws a drain request for replica `id`, returning the request that was in force (or
    /// `None` if there was none, which is not an error).
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn withdraw_drain(&self, id: &ReplicaId) -> Result<Option<DrainRequest>, ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let key = drain_key(id);
            let Some(existing) = txn.get(&self.replicas, &key)? else {
                return Ok(None);
            };
            txn.delete(&self.replicas, &key)?;
            decode_kv::<DrainRequest>("DrainRequest", &existing).map(Some)
        })
        .map_err(ClusterError::Store)
    }

    /// The drain request for replica `id`, if there is one. A snapshot read.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn drain_request(&self, id: &ReplicaId) -> Result<Option<DrainRequest>, ClusterError> {
        let snap = self.backend.snapshot();
        match snap.get(&self.replicas, &drain_key(id))? {
            None => Ok(None),
            Some(bytes) => decode("DrainRequest", &bytes).map(Some),
        }
    }

    /// Every drain request in force, by replica. A snapshot read.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn list_drain_requests(&self) -> Result<Vec<(ReplicaId, DrainRequest)>, ClusterError> {
        let snap = self.backend.snapshot();
        let mut out = Vec::new();
        for item in snap.range(&self.replicas, RangeSpec::prefix(DRAIN_PREFIX.as_bytes())) {
            let (key, value) = item?;
            let Some(id) = std::str::from_utf8(&key)
                .ok()
                .and_then(|k| k.strip_prefix(DRAIN_PREFIX))
            else {
                continue;
            };
            out.push((ReplicaId::new(id), decode("DrainRequest", &value)?));
        }
        Ok(out)
    }

    /// Reads a shard's row, defaulting to [`ShardRecord::initial`] if it has never been acquired.
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn get_shard(&self, shard: ShardId) -> Result<ShardRecord, ClusterError> {
        let snap = self.backend.snapshot();
        match snap.get(&self.shards, &shard_key(shard))? {
            None => Ok(ShardRecord::initial()),
            Some(bytes) => decode("ShardRecord", &bytes),
        }
    }

    /// Lists every shard row that has ever been written (unwritten shards are implicitly
    /// [`ShardRecord::initial`] and are not returned; callers iterate the layout for the full
    /// shard space).
    ///
    /// # Errors
    /// Returns a store or decode error.
    pub fn list_shards(&self) -> Result<Vec<(ShardId, ShardRecord)>, ClusterError> {
        let snap = self.backend.snapshot();
        let mut out = Vec::new();
        for item in snap.range(&self.shards, RangeSpec::prefix(SHARD_PREFIX.as_bytes())) {
            let (key, value) = item?;
            let key_str = std::str::from_utf8(&key).unwrap_or_default();
            let Some(rest) = key_str.strip_prefix(SHARD_PREFIX) else {
                continue;
            };
            let Some(shard) = parse_shard_key(rest) else {
                continue;
            };
            out.push((shard, decode("ShardRecord", &value)?));
        }
        Ok(out)
    }

    /// Attempts to acquire `shard` for `(me, my_generation)`.
    ///
    /// `owner_is_dead` decides, from the caller's own view of replica liveness (RFC 0001 section
    /// 4: this is the observer's judgement, not a store-side check, which is why the epoch
    /// exists), whether a currently-recorded owner should be treated as gone. Returns the new
    /// row on success, or `None` if the shard is held by a live owner other than `me`.
    ///
    /// # Errors
    /// Returns a store error.
    pub fn acquire_shard(
        &self,
        shard: ShardId,
        me: &ReplicaId,
        my_generation: Generation,
        owner_is_dead: impl Fn(&ReplicaId) -> bool,
    ) -> Result<Option<ShardRecord>, ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let key = shard_key(shard);
            let current = match txn.get(&self.shards, &key)? {
                None => ShardRecord::initial(),
                Some(bytes) => decode_kv::<ShardRecord>("ShardRecord", &bytes)?,
            };
            let can_acquire = match &current.owner {
                None => true,
                Some((owner, _)) if owner == me => true,
                Some((owner, _)) => owner_is_dead(owner),
            };
            if !can_acquire {
                return Ok(None);
            }
            let new = ShardRecord {
                epoch: current.epoch.next(),
                owner: Some((me.clone(), my_generation)),
            };
            txn.put(&self.shards, &key, &encode(&new))?;
            Ok(Some(new))
        })
        .map_err(ClusterError::Store)
    }

    /// Releases `shard` and advances its epoch, but only if `me` at `my_generation` is still the
    /// recorded owner. Returns the released row's new epoch, or `None` (changing nothing) when
    /// `me` was not the owner.
    ///
    /// The epoch advances on release, not only on the next acquisition (decision 0023): a
    /// fence issued to `me` for the shard fails from the moment the release commits, so a write
    /// the old owner still has in flight cannot land while the shard has no owner. A handoff
    /// therefore moves the epoch twice, once here and once when the new owner acquires; epochs
    /// only need to grow.
    ///
    /// # Errors
    /// Returns a store error.
    pub fn release_shard(
        &self,
        shard: ShardId,
        me: &ReplicaId,
        my_generation: Generation,
    ) -> Result<Option<Epoch>, ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let key = shard_key(shard);
            let current = match txn.get(&self.shards, &key)? {
                None => return Ok(None),
                Some(bytes) => decode_kv::<ShardRecord>("ShardRecord", &bytes)?,
            };
            if current.owner.as_ref().map(|(o, g)| (o, *g)) != Some((me, my_generation)) {
                return Ok(None);
            }
            let released = ShardRecord {
                epoch: current.epoch.next(),
                owner: None,
            };
            txn.put(&self.shards, &key, &encode(&released))?;
            Ok(Some(released.epoch))
        })
        .map_err(ClusterError::Store)
    }

    /// Releases `shard` and advances its epoch, as a peer taking it from a lost owner would:
    /// a fence issued to `me` for it fails from now on, even though nobody has acquired it.
    /// What a draining replica does when no other replica is live to claim its shards.
    ///
    /// Applies when `me` at `my_generation` owns the shard or nobody does (it may already have
    /// been released without the epoch advancing). Returns `false`, changing nothing, when
    /// another replica owns it.
    ///
    /// # Errors
    /// Returns a store error.
    pub fn release_shard_fenced(
        &self,
        shard: ShardId,
        me: &ReplicaId,
        my_generation: Generation,
    ) -> Result<bool, ClusterError> {
        self.release_shards_fenced(&[shard], me, my_generation)
            .map(|released| released.first().copied().unwrap_or(false))
    }

    /// [`ClusterStore::release_shard_fenced`] for many shards in one transaction, answering
    /// for each in order. One transaction rather than one per shard: on PostgreSQL under load
    /// a transaction can take tens of milliseconds, and the last replica of a cluster
    /// releasing hundreds of shards one at a time spent seconds of its shutdown doing it.
    ///
    /// # Errors
    /// Returns a store error; then nothing was released.
    pub fn release_shards_fenced(
        &self,
        shards: &[ShardId],
        me: &ReplicaId,
        my_generation: Generation,
    ) -> Result<Vec<bool>, ClusterError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let mut out = Vec::with_capacity(shards.len());
            for shard in shards {
                let key = shard_key(*shard);
                let current = match txn.get(&self.shards, &key)? {
                    None => ShardRecord::initial(),
                    Some(bytes) => decode_kv::<ShardRecord>("ShardRecord", &bytes)?,
                };
                let mine_or_nobodys = match &current.owner {
                    None => true,
                    Some((owner, generation)) => owner == me && *generation == my_generation,
                };
                if mine_or_nobodys {
                    let released = ShardRecord {
                        epoch: current.epoch.next(),
                        owner: None,
                    };
                    txn.put(&self.shards, &key, &encode(&released))?;
                }
                out.push(mine_or_nobodys);
            }
            Ok(out)
        })
        .map_err(ClusterError::Store)
    }
}

fn parse_shard_key(rest: &str) -> Option<ShardId> {
    let (kind, index) = rest.split_once('/')?;
    Some(ShardId::new(
        crate::types::ShardKind::parse(kind)?,
        index.parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Generation, ReplicaState, ShardKind};
    use hs_kv::memory::MemoryBackend;

    fn store() -> ClusterStore<MemoryBackend> {
        ClusterStore::open(MemoryBackend::new()).expect("open")
    }

    fn replica(id: &str, generation: u64) -> ReplicaRecord {
        ReplicaRecord {
            id: ReplicaId::new(id),
            generation: Generation(generation),
            mesh_addr: "127.0.0.1:0".into(),
            zone: None,
            version: "test".into(),
            state: ReplicaState::Active,
            heartbeat_seq: 0,
            heartbeat_unix_ms: 0,
        }
    }

    #[test]
    fn layout_is_recorded_once_and_confirmed_after() {
        let s = store();
        let layout = ShardLayout::small(4);
        assert_eq!(s.init_layout(layout).unwrap(), layout);
        assert_eq!(s.init_layout(layout).unwrap(), layout);
        assert_eq!(s.get_layout().unwrap(), Some(layout));
    }

    #[test]
    fn mismatched_layout_is_rejected() {
        let s = store();
        s.init_layout(ShardLayout::small(4)).unwrap();
        let err = s.init_layout(ShardLayout::small(8)).unwrap_err();
        assert!(matches!(err, ClusterError::LayoutMismatch { .. }));
    }

    #[test]
    fn heartbeat_ignores_stale_generation() {
        let s = store();
        s.heartbeat(&replica("hs-0", 5)).unwrap();
        s.heartbeat(&replica("hs-0", 3)).unwrap(); // older generation, ignored
        let rows = s.list_replicas().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].generation, Generation(5));
    }

    #[test]
    fn remove_replica_requires_matching_generation() {
        let s = store();
        s.heartbeat(&replica("hs-0", 5)).unwrap();
        s.remove_replica(&ReplicaId::new("hs-0"), Generation(3))
            .unwrap();
        assert_eq!(
            s.list_replicas().unwrap().len(),
            1,
            "stale removal must not delete a newer row"
        );
        s.remove_replica(&ReplicaId::new("hs-0"), Generation(5))
            .unwrap();
        assert_eq!(s.list_replicas().unwrap().len(), 0);
    }

    #[test]
    fn the_heartbeat_seq_floor_survives_deregistration() {
        let s = store();
        let id = ReplicaId::new("hs-0");
        assert_eq!(s.heartbeat_seq_floor(&id).unwrap(), 0);
        let mut row = replica("hs-0", 5);
        row.heartbeat_seq = 41;
        s.heartbeat(&row).unwrap();
        assert_eq!(s.heartbeat_seq_floor(&id).unwrap(), 41, "from the live row");
        s.remove_replica(&id, Generation(5)).unwrap();
        assert!(s.list_replicas().unwrap().is_empty());
        assert_eq!(
            s.heartbeat_seq_floor(&id).unwrap(),
            41,
            "kept after removal"
        );
        // A later, lower row (an older binary's) never lowers it.
        row.heartbeat_seq = 7;
        row.generation = Generation(6);
        s.heartbeat(&row).unwrap();
        assert_eq!(s.heartbeat_seq_floor(&id).unwrap(), 41);
        s.remove_replica(&id, Generation(6)).unwrap();
        assert_eq!(s.heartbeat_seq_floor(&id).unwrap(), 41);
        // The kept value is not a drain request.
        assert!(s.list_drain_requests().unwrap().is_empty());
        assert_eq!(s.heartbeat_seq_floor(&ReplicaId::new("hs-1")).unwrap(), 0);
    }

    #[test]
    fn acquire_then_release_round_trips_epoch() {
        let s = store();
        let shard = ShardId::new(ShardKind::Room, 1);
        let me = ReplicaId::new("hs-0");
        let acquired = s
            .acquire_shard(shard, &me, Generation(1), |_| false)
            .unwrap()
            .unwrap();
        assert_eq!(acquired.epoch, Epoch(1));
        assert_eq!(s.get_shard(shard).unwrap(), acquired);

        // A different replica cannot acquire while the owner is considered alive.
        let other = ReplicaId::new("hs-1");
        assert!(
            s.acquire_shard(shard, &other, Generation(1), |_| false)
                .unwrap()
                .is_none()
        );

        // But can once the owner is judged dead, and the epoch advances.
        let taken = s
            .acquire_shard(shard, &other, Generation(1), |_| true)
            .unwrap()
            .unwrap();
        assert_eq!(taken.epoch, Epoch(2));
        assert_eq!(taken.owner.unwrap().0, other);

        // Releasing advances the epoch too (decision 0023), so a fence held from before the
        // release fails while the shard has no owner, not only once the next owner acquires.
        let new_epoch = s.release_shard(shard, &other, Generation(1)).unwrap();
        let released = s.get_shard(shard).unwrap();
        assert_eq!(released.owner, None);
        assert_eq!(released.epoch, Epoch(3), "release advances the epoch");
        assert_eq!(new_epoch, Some(Epoch(3)), "and answers the new one");

        // The next acquisition advances it again: epochs only grow.
        let again = s
            .acquire_shard(shard, &me, Generation(2), |_| false)
            .unwrap()
            .unwrap();
        assert_eq!(again.epoch, Epoch(4));
    }

    #[test]
    fn a_fenced_release_advances_the_epoch_unless_another_replica_owns_the_shard() {
        let s = store();
        let shard = ShardId::new(ShardKind::Room, 1);
        let me = ReplicaId::new("hs-0");
        let held = s
            .acquire_shard(shard, &me, Generation(1), |_| false)
            .unwrap()
            .unwrap();
        assert!(s.release_shard_fenced(shard, &me, Generation(1)).unwrap());
        let row = s.get_shard(shard).unwrap();
        assert_eq!(row.owner, None);
        assert_eq!(row.epoch, held.epoch.next());
        // Already ownerless: advanced again (harmless; epochs only need to grow).
        assert!(s.release_shard_fenced(shard, &me, Generation(1)).unwrap());
        assert_eq!(s.get_shard(shard).unwrap().epoch, held.epoch.next().next());
        // Owned by somebody else: left alone.
        let other = ReplicaId::new("hs-1");
        let theirs = s
            .acquire_shard(shard, &other, Generation(1), |_| false)
            .unwrap()
            .unwrap();
        assert!(!s.release_shard_fenced(shard, &me, Generation(1)).unwrap());
        assert_eq!(s.get_shard(shard).unwrap(), theirs);

        // Many at once, answered in order.
        let mine = ShardId::new(ShardKind::Room, 2);
        let held = s
            .acquire_shard(mine, &me, Generation(1), |_| false)
            .unwrap()
            .unwrap();
        assert_eq!(
            s.release_shards_fenced(&[shard, mine], &me, Generation(1))
                .unwrap(),
            vec![false, true]
        );
        assert_eq!(s.get_shard(shard).unwrap(), theirs);
        assert_eq!(s.get_shard(mine).unwrap().epoch, held.epoch.next());
    }

    #[test]
    fn release_by_a_non_owner_is_a_no_op() {
        let s = store();
        let shard = ShardId::new(ShardKind::Room, 1);
        let me = ReplicaId::new("hs-0");
        let held = s
            .acquire_shard(shard, &me, Generation(1), |_| false)
            .unwrap()
            .unwrap();
        let released = s
            .release_shard(shard, &ReplicaId::new("hs-1"), Generation(1))
            .unwrap();
        assert_eq!(released, None, "nothing was released");
        assert_eq!(
            s.get_shard(shard).unwrap(),
            held,
            "wrong replica must not release, nor move the epoch"
        );
        // Nor the right replica at an older generation (a previous process of it).
        assert_eq!(s.release_shard(shard, &me, Generation(0)).unwrap(), None);
        assert_eq!(s.get_shard(shard).unwrap(), held);
    }

    #[test]
    fn list_shards_only_returns_written_rows() {
        let s = store();
        assert!(s.list_shards().unwrap().is_empty());
        let shard = ShardId::new(ShardKind::User, 9);
        s.acquire_shard(shard, &ReplicaId::new("hs-0"), Generation(1), |_| false)
            .unwrap();
        let rows = s.list_shards().unwrap();
        assert_eq!(rows, vec![(shard, s.get_shard(shard).unwrap())]);
    }
}
