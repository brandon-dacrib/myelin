//! Candidate A: snapshot plus delta chains, `PLAN.md` section 6.3's model of Synapse's state
//! groups.
//!
//! A state group is a delta from its parent group (added/removed `(StateKeyId, EventSn)`
//! entries), with a full snapshot written every [`SNAPSHOT_INTERVAL`] hops so no lookup ever
//! walks more than that many groups. This is the representation `PLAN.md` names as having the
//! "state-group blowup" failure mode: a [`SnapshotDeltaRepr::diff`] between two states that are
//! not on the same delta chain (a fork, or a resolved state that is not a direct descendant of
//! either input) has no shortcut and must fully materialize both sides
//! ([`SnapshotDeltaRepr::full_state`]) before computing a set difference.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use hs_kv::{KvBackend, KvError, KvRead, KvWrite};
use hs_model::ids::{EventSn, StateKeyId};
use hs_tables::TupleKey;
use thiserror::Error;

use super::repr::{BakeoffStats, StateRepr, set_diff};
use crate::api::StateDiff;

/// How many hops a delta chain may grow before the next `apply` writes a full snapshot instead
/// of another delta. Bake-off scale (see `docs/decisions/0005-state-bakeoff-methodology.md`,
/// "corpus sizes are scaled down"); Synapse tunes this per deployment.
pub const SNAPSHOT_INTERVAL: u32 = 50;

const COUNTER_KEY: &[u8] = b"next_group";

/// Candidate A's [`StateRepr::Root`]: a state-group id, or `0` for the empty state (never
/// stored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootA(pub u64);

/// Errors from [`SnapshotDeltaRepr`].
#[derive(Debug, Error)]
pub enum Error {
    /// `root` does not name a group this representation has stored.
    #[error("state group {0} not known to this store")]
    UnknownRoot(u64),
    /// The underlying `hs_kv` backend failed.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A write lost a (should-be-impossible, since every group id is written exactly once)
    /// serializability race.
    #[error("unexpected write conflict writing a new state group")]
    Conflict,
    /// A stored group record could not be decoded.
    #[error("state group record was corrupt")]
    Corrupt,
}

struct GroupRecord {
    is_snapshot: bool,
    parent: u64,
    depth: u32,
    /// The added set (delta record) or the full state map (snapshot record), sorted by key.
    entries: Vec<(StateKeyId, EventSn)>,
    /// The removed set. Always empty for a snapshot record.
    removed: Vec<StateKeyId>,
}

fn encode_group(rec: &GroupRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(17 + rec.entries.len() * 12 + rec.removed.len() * 4);
    out.push(u8::from(rec.is_snapshot));
    out.extend_from_slice(&rec.parent.to_be_bytes());
    out.extend_from_slice(&rec.depth.to_be_bytes());
    out.extend_from_slice(&(rec.entries.len() as u32).to_be_bytes());
    for (k, v) in &rec.entries {
        out.extend_from_slice(&k.to_be_bytes());
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&(rec.removed.len() as u32).to_be_bytes());
    for k in &rec.removed {
        out.extend_from_slice(&k.to_be_bytes());
    }
    out
}

fn take<'a>(p: &mut &'a [u8], n: usize) -> Result<&'a [u8], Error> {
    if p.len() < n {
        return Err(Error::Corrupt);
    }
    let (head, rest) = p.split_at(n);
    *p = rest;
    Ok(head)
}

fn decode_group(bytes: &[u8]) -> Result<GroupRecord, Error> {
    let mut p = bytes;
    let is_snapshot = take(&mut p, 1)?[0] != 0;
    let parent = u64::from_be_bytes(take(&mut p, 8)?.try_into().map_err(|_| Error::Corrupt)?);
    let depth = u32::from_be_bytes(take(&mut p, 4)?.try_into().map_err(|_| Error::Corrupt)?);
    let n_entries =
        u32::from_be_bytes(take(&mut p, 4)?.try_into().map_err(|_| Error::Corrupt)?) as usize;
    let mut entries = Vec::with_capacity(n_entries);
    for _ in 0..n_entries {
        let k = u32::from_be_bytes(take(&mut p, 4)?.try_into().map_err(|_| Error::Corrupt)?);
        let v = u64::from_be_bytes(take(&mut p, 8)?.try_into().map_err(|_| Error::Corrupt)?);
        entries.push((StateKeyId::new(k), EventSn::new(v)));
    }
    let n_removed =
        u32::from_be_bytes(take(&mut p, 4)?.try_into().map_err(|_| Error::Corrupt)?) as usize;
    let mut removed = Vec::with_capacity(n_removed);
    for _ in 0..n_removed {
        let k = u32::from_be_bytes(take(&mut p, 4)?.try_into().map_err(|_| Error::Corrupt)?);
        removed.push(StateKeyId::new(k));
    }
    Ok(GroupRecord {
        is_snapshot,
        parent,
        depth,
        entries,
        removed,
    })
}

/// Candidate A over any `hs_kv::KvBackend`.
///
/// Cheap to clone: cloning shares the underlying backend, keyspace handles and instrumentation
/// counters (via `Rc`), exactly like cloning `KV` itself. This lets the bake-off harness create
/// one store per room in a many-small-rooms scenario while still measuring aggregate bytes
/// written and disk usage across every room sharing one physical backend, matching how a real
/// deployment shares one keyspace across many rooms.
#[derive(Clone)]
pub struct SnapshotDeltaRepr<KV: KvBackend> {
    backend: KV,
    groups: KV::Keyspace,
    meta: KV::Keyspace,
    bytes_written: Rc<Cell<u64>>,
    snapshot_count: Rc<Cell<u64>>,
}

impl<KV: KvBackend> SnapshotDeltaRepr<KV> {
    /// Opens (creating if necessary) candidate A's keyspaces on `backend`.
    ///
    /// # Errors
    /// Returns [`Error::Kv`] if the keyspaces could not be opened.
    pub fn new(backend: KV) -> Result<Self, Error> {
        let groups = backend.keyspace("state_a_groups")?;
        let meta = backend.keyspace("state_a_counter")?;
        Ok(Self {
            backend,
            groups,
            meta,
            bytes_written: Rc::new(Cell::new(0)),
            snapshot_count: Rc::new(Cell::new(0)),
        })
    }

    /// Total bytes ever passed to `KvWrite::put` for this representation's keyspaces: the
    /// numerator for the bake-off's write-amplification measurement.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.get()
    }

    /// How many `apply` calls wrote a full snapshot rather than a delta.
    #[must_use]
    pub fn snapshot_count(&self) -> u64 {
        self.snapshot_count.get()
    }

    /// Sums key and value bytes actually resident in this representation's keyspace: the
    /// bake-off's "bytes on disk" measurement.
    ///
    /// # Errors
    /// Returns [`Error::Kv`] on a backend failure.
    pub fn bytes_on_disk(&self) -> Result<u64, Error> {
        let snap = self.backend.snapshot();
        let mut total = 0u64;
        for item in snap.range(&self.groups, hs_kv::RangeSpec::full()) {
            let (k, v) = item?;
            total += (k.len() + v.len()) as u64;
        }
        Ok(total)
    }

    fn load(&self, id: u64) -> Result<GroupRecord, Error> {
        let snap = self.backend.snapshot();
        let bytes = snap
            .get(&self.groups, &(id,).encode())?
            .ok_or(Error::UnknownRoot(id))?;
        decode_group(&bytes)
    }

    fn store(&self, id: u64, rec: &GroupRecord) -> Result<(), Error> {
        let encoded = encode_group(rec);
        self.bytes_written
            .set(self.bytes_written.get() + encoded.len() as u64 + 8);
        let mut txn = self.backend.begin()?;
        txn.put(&self.groups, &(id,).encode(), &encoded)?;
        match self.backend.commit(txn)? {
            Ok(()) => Ok(()),
            Err(_conflict) => Err(Error::Conflict),
        }
    }

    fn next_group_id(&self) -> Result<u64, Error> {
        let mut txn = self.backend.begin()?;
        let n = txn.atomic_add(&self.meta, COUNTER_KEY, 1)?;
        match self.backend.commit(txn)? {
            #[allow(clippy::cast_sign_loss, reason = "counter is always non-negative")]
            Ok(()) => Ok(n as u64),
            Err(_conflict) => Err(Error::Conflict),
        }
    }

    /// Walks from `root` back to the nearest snapshot (or the empty state), rebuilding the full
    /// state map. Bounded to at most [`SNAPSHOT_INTERVAL`] hops by construction (every group's
    /// `depth` resets to 0 at a snapshot).
    fn materialize_full(&self, root: RootA) -> Result<BTreeMap<StateKeyId, EventSn>, Error> {
        if root.0 == 0 {
            return Ok(BTreeMap::new());
        }
        let mut stack = Vec::new();
        let mut cur = root.0;
        let base = loop {
            if cur == 0 {
                break BTreeMap::new();
            }
            let rec = self.load(cur)?;
            if rec.is_snapshot {
                break rec.entries.iter().copied().collect();
            }
            let parent = rec.parent;
            stack.push(rec);
            cur = parent;
        };
        let mut map = base;
        for rec in stack.into_iter().rev() {
            for k in &rec.removed {
                map.remove(k);
            }
            for (k, v) in &rec.entries {
                map.insert(*k, *v);
            }
        }
        Ok(map)
    }

    /// If `from` is an ancestor of `to` along the delta chain (within one snapshot epoch),
    /// returns the composed diff without materializing either side in full. `None` means "not
    /// on the same chain within the walk" -- the caller falls back to full materialization.
    fn try_ancestor_diff(&self, from: RootA, to: RootA) -> Result<Option<StateDiff>, Error> {
        let mut hops = Vec::new();
        let mut cur = to.0;
        loop {
            if cur == from.0 {
                let mut added = BTreeMap::new();
                let mut removed = BTreeSet::new();
                for rec in hops.into_iter().rev() {
                    let rec: GroupRecord = rec;
                    for k in &rec.removed {
                        removed.insert(*k);
                        added.remove(k);
                    }
                    for (k, v) in &rec.entries {
                        added.insert(*k, *v);
                        removed.remove(k);
                    }
                }
                return Ok(Some(StateDiff { added, removed }));
            }
            if cur == 0 {
                return Ok(None);
            }
            let rec = self.load(cur)?;
            let is_snapshot = rec.is_snapshot;
            let parent = rec.parent;
            hops.push(rec);
            if is_snapshot {
                return Ok(None);
            }
            cur = parent;
        }
    }
}

impl<KV: KvBackend> BakeoffStats for SnapshotDeltaRepr<KV> {
    fn bytes_on_disk(&self) -> Result<u64, Error> {
        SnapshotDeltaRepr::bytes_on_disk(self)
    }

    fn bytes_written(&self) -> u64 {
        SnapshotDeltaRepr::bytes_written(self)
    }

    fn compaction_events(&self) -> Option<u64> {
        Some(self.snapshot_count())
    }

    fn dedup_hits(&self) -> Option<u64> {
        None
    }
}

impl<KV: KvBackend> StateRepr for SnapshotDeltaRepr<KV> {
    type Root = RootA;
    type Error = Error;

    fn empty_root(&self) -> RootA {
        RootA(0)
    }

    fn get(&self, root: RootA, key: StateKeyId) -> Result<Option<EventSn>, Error> {
        let mut cur = root.0;
        loop {
            if cur == 0 {
                return Ok(None);
            }
            let rec = self.load(cur)?;
            if rec.removed.binary_search(&key).is_ok() {
                return Ok(None);
            }
            if let Ok(idx) = rec.entries.binary_search_by_key(&key, |(k, _)| *k) {
                return Ok(Some(rec.entries[idx].1));
            }
            if rec.is_snapshot {
                return Ok(None);
            }
            cur = rec.parent;
        }
    }

    fn full_state(&self, root: RootA) -> Result<BTreeMap<StateKeyId, EventSn>, Error> {
        self.materialize_full(root)
    }

    fn diff(&self, from: RootA, to: RootA) -> Result<StateDiff, Error> {
        if from == to {
            return Ok(StateDiff::default());
        }
        if let Some(d) = self.try_ancestor_diff(from, to)? {
            return Ok(d);
        }
        let a = self.materialize_full(from)?;
        let b = self.materialize_full(to)?;
        Ok(set_diff(&a, &b))
    }

    fn apply(&self, root: RootA, changes: &StateDiff) -> Result<RootA, Error> {
        let parent_depth = if root.0 == 0 {
            0
        } else {
            self.load(root.0)?.depth
        };
        let new_depth = parent_depth + 1;
        let id = self.next_group_id()?;

        if new_depth % SNAPSHOT_INTERVAL == 0 {
            let mut full = self.materialize_full(root)?;
            for k in &changes.removed {
                full.remove(k);
            }
            for (k, v) in &changes.added {
                full.insert(*k, *v);
            }
            let mut entries: Vec<_> = full.into_iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            self.store(
                id,
                &GroupRecord {
                    is_snapshot: true,
                    parent: root.0,
                    depth: 0,
                    entries,
                    removed: Vec::new(),
                },
            )?;
            self.snapshot_count.set(self.snapshot_count.get() + 1);
        } else {
            let mut entries: Vec<_> = changes.added.iter().map(|(k, v)| (*k, *v)).collect();
            entries.sort_by_key(|(k, _)| *k);
            let mut removed: Vec<_> = changes.removed.iter().copied().collect();
            removed.sort_unstable();
            self.store(
                id,
                &GroupRecord {
                    is_snapshot: false,
                    parent: root.0,
                    depth: new_depth,
                    entries,
                    removed,
                },
            )?;
        }
        Ok(RootA(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn repr() -> SnapshotDeltaRepr<MemoryBackend> {
        SnapshotDeltaRepr::new(MemoryBackend::default()).unwrap()
    }

    #[test]
    fn set_then_get_round_trips() {
        let r = repr();
        let root0 = r.empty_root();
        let k = StateKeyId::new(1);
        let root1 = r.apply(root0, &StateDiff::set(k, EventSn::new(7))).unwrap();
        assert_eq!(r.get(root1, k).unwrap(), Some(EventSn::new(7)));
        assert_eq!(r.get(root0, k).unwrap(), None);
    }

    #[test]
    fn overwrite_and_remove() {
        let r = repr();
        let k = StateKeyId::new(1);
        let root0 = r.empty_root();
        let root1 = r.apply(root0, &StateDiff::set(k, EventSn::new(1))).unwrap();
        let root2 = r.apply(root1, &StateDiff::set(k, EventSn::new(2))).unwrap();
        assert_eq!(r.get(root2, k).unwrap(), Some(EventSn::new(2)));

        let mut removal = StateDiff::default();
        removal.removed.insert(k);
        let root3 = r.apply(root2, &removal).unwrap();
        assert_eq!(r.get(root3, k).unwrap(), None);
        // Earlier roots are unaffected (persistent update).
        assert_eq!(r.get(root2, k).unwrap(), Some(EventSn::new(2)));
    }

    #[test]
    fn diff_and_apply_round_trip() {
        let r = repr();
        let root0 = r.empty_root();
        let mut root = root0;
        for i in 0..5u32 {
            root = r
                .apply(
                    root,
                    &StateDiff::set(StateKeyId::new(i), EventSn::new(u64::from(i))),
                )
                .unwrap();
        }
        let d = r.diff(root0, root).unwrap();
        assert_eq!(d.added.len(), 5);
        let applied = r.apply(root0, &d).unwrap();
        assert_eq!(r.full_state(applied).unwrap(), r.full_state(root).unwrap());
    }

    #[test]
    fn snapshot_written_past_interval_and_full_state_still_correct() {
        let r = repr();
        let mut root = r.empty_root();
        let mut expected = BTreeMap::new();
        for i in 0..(SNAPSHOT_INTERVAL * 3) {
            let k = StateKeyId::new(i % 10); // heavy overwrite, like membership churn
            let v = EventSn::new(u64::from(i));
            root = r.apply(root, &StateDiff::set(k, v)).unwrap();
            expected.insert(k, v);
        }
        assert!(r.snapshot_count() > 0);
        assert_eq!(r.full_state(root).unwrap(), expected);
    }

    #[test]
    fn diff_across_a_fork_matches_full_materialization() {
        let r = repr();
        let root0 = r.empty_root();
        let base = r
            .apply(root0, &StateDiff::set(StateKeyId::new(0), EventSn::new(1)))
            .unwrap();
        let branch_a = r
            .apply(base, &StateDiff::set(StateKeyId::new(1), EventSn::new(2)))
            .unwrap();
        let branch_b = r
            .apply(base, &StateDiff::set(StateKeyId::new(2), EventSn::new(3)))
            .unwrap();
        // Not on the same chain: exercises the full-materialization fallback path.
        let d = r.diff(branch_a, branch_b).unwrap();
        let applied = r.apply(branch_a, &d).unwrap();
        assert_eq!(
            r.full_state(applied).unwrap(),
            r.full_state(branch_b).unwrap()
        );
    }
}
