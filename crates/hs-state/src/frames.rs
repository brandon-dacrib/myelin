//! Deduplicated frames with layered diffs: the production [`StateRepr`] implementation, modeled
//! on Conduit's and Palpo's state frames (`PLAN.md` section 6.3).
//!
//! This is the bake-off's winning candidate (candidate B,
//! `docs/decisions/0006-state-bakeoff-results.md`), promoted here out of `crate::bakeoff` per that
//! decision's "next-owner work" note; see `crate::kv_store` for the [`crate::api::StateStore`]
//! built on top of it. The two losing candidates remain benchmark-only under `crate::bakeoff`.
//!
//! A frame is a sorted set of `(StateKeyId, EventSn)` pairs expressed as a delta from a parent
//! frame: an `appended` list and a `disposed` list, both delta-varint compressed
//! (`crate::varint`). A frame's storage key is the content hash of `(parent, appended,
//! disposed)`, so two applications that happen to produce the identical delta from the identical
//! parent are automatically deduplicated -- no extra bookkeeping, just a `get`-before-`put`.
//! Layer depth is bounded the same way candidate A bounded delta-chain depth: every
//! [`REBASE_INTERVAL`] hops, `apply` writes a "base" frame containing the full materialized
//! state instead of another layer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hs_kv::{KvBackend, KvError, KvRead, KvWrite};
use hs_model::ids::{EventSn, StateKeyId};
use sha1::{Digest, Sha1};
use thiserror::Error;

use crate::api::StateDiff;
use crate::repr::{ReprStats, StateRepr, set_diff};
use crate::varint::{read_uvarint, write_uvarint};

/// How many layers may stack before `apply` writes a full "base" frame instead of another delta
/// layer. Bake-off scale (50), matching `SNAPSHOT_INTERVAL` in `crate::bakeoff::snapshot_delta`;
/// see `docs/decisions/0005-state-bakeoff-methodology.md`. A real deployment should probably tune
/// this per room size (`docs/status/02-state-and-model.md`'s "Implications for tracks 04 and 06"
/// makes the same point) rather than leave it at the bake-off's default -- not changed here since
/// no real-room-size data exists yet to tune it against.
pub const REBASE_INTERVAL: u32 = 50;

const EMPTY: [u8; 16] = [0; 16];

/// Candidate B's [`StateRepr::Root`]: a frame's content hash, or the all-zero sentinel for the
/// empty state (never stored -- an all-zero SHA-1 prefix from real content is astronomically
/// unlikely for the synthetic corpus this bake-off runs; a production implementation would use an
/// explicit `Option` instead of a sentinel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootB(pub [u8; 16]);

/// Errors from [`FrameRepr`].
#[derive(Debug, Error)]
pub enum Error {
    /// `root` does not name a frame this representation has stored.
    #[error("state frame {0:x?} not known to this store")]
    UnknownRoot([u8; 16]),
    /// The underlying `hs_kv` backend failed.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A frame record could not be decoded.
    #[error("state frame record was corrupt")]
    Corrupt,
}

struct FrameRecord {
    is_base: bool,
    parent: [u8; 16],
    depth: u32,
    appended: Vec<(StateKeyId, EventSn)>,
    disposed: Vec<StateKeyId>,
}

fn encode_delta_pairs(out: &mut Vec<u8>, pairs: &[(StateKeyId, EventSn)]) {
    write_uvarint(out, pairs.len() as u64);
    let mut prev = 0u32;
    for (k, v) in pairs {
        write_uvarint(out, u64::from(k.get() - prev));
        prev = k.get();
        write_uvarint(out, v.get());
    }
}

fn decode_delta_pairs(input: &mut &[u8]) -> Result<Vec<(StateKeyId, EventSn)>, Error> {
    let n = read_uvarint(input).map_err(|()| Error::Corrupt)? as usize;
    let mut out = Vec::with_capacity(n);
    let mut prev = 0u32;
    for _ in 0..n {
        let delta = read_uvarint(input).map_err(|()| Error::Corrupt)? as u32;
        prev += delta;
        let v = read_uvarint(input).map_err(|()| Error::Corrupt)?;
        out.push((StateKeyId::new(prev), EventSn::new(v)));
    }
    Ok(out)
}

fn encode_delta_keys(out: &mut Vec<u8>, keys: &[StateKeyId]) {
    write_uvarint(out, keys.len() as u64);
    let mut prev = 0u32;
    for k in keys {
        write_uvarint(out, u64::from(k.get() - prev));
        prev = k.get();
    }
}

fn decode_delta_keys(input: &mut &[u8]) -> Result<Vec<StateKeyId>, Error> {
    let n = read_uvarint(input).map_err(|()| Error::Corrupt)? as usize;
    let mut out = Vec::with_capacity(n);
    let mut prev = 0u32;
    for _ in 0..n {
        let delta = read_uvarint(input).map_err(|()| Error::Corrupt)? as u32;
        prev += delta;
        out.push(StateKeyId::new(prev));
    }
    Ok(out)
}

fn encode_frame(rec: &FrameRecord) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(u8::from(rec.is_base));
    out.extend_from_slice(&rec.parent);
    write_uvarint(&mut out, u64::from(rec.depth));
    encode_delta_pairs(&mut out, &rec.appended);
    encode_delta_keys(&mut out, &rec.disposed);
    out
}

fn decode_frame(bytes: &[u8]) -> Result<FrameRecord, Error> {
    let mut p = bytes;
    let (flag, rest) = p.split_first().ok_or(Error::Corrupt)?;
    let is_base = *flag != 0;
    p = rest;
    if p.len() < 16 {
        return Err(Error::Corrupt);
    }
    let (parent_bytes, rest) = p.split_at(16);
    let parent: [u8; 16] = parent_bytes.try_into().map_err(|_| Error::Corrupt)?;
    p = rest;
    let depth = read_uvarint(&mut p).map_err(|()| Error::Corrupt)? as u32;
    let appended = decode_delta_pairs(&mut p)?;
    let disposed = decode_delta_keys(&mut p)?;
    Ok(FrameRecord {
        is_base,
        parent,
        depth,
        appended,
        disposed,
    })
}

fn content_hash(
    flag: u8,
    parent: [u8; 16],
    appended_bytes: &[u8],
    disposed_bytes: &[u8],
) -> [u8; 16] {
    let mut hasher = Sha1::new();
    hasher.update([flag]);
    hasher.update(parent);
    hasher.update(appended_bytes);
    hasher.update(disposed_bytes);
    let full: [u8; 20] = hasher.finalize().into();
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

/// Candidate B over any `hs_kv::KvBackend`. See `SnapshotDeltaRepr`'s docs on why this is cheap
/// to clone and what cloning shares (backend, keyspace, instrumentation counters).
#[derive(Clone)]
pub struct FrameRepr<KV: KvBackend> {
    backend: KV,
    frames: KV::Keyspace,
    bytes_written: Arc<AtomicU64>,
    rebase_count: Arc<AtomicU64>,
    dedup_hits: Arc<AtomicU64>,
}

impl<KV: KvBackend> FrameRepr<KV> {
    /// Opens (creating if necessary) candidate B's keyspace on `backend`.
    ///
    /// # Errors
    /// Returns [`Error::Kv`] if the keyspace could not be opened.
    pub fn new(backend: KV) -> Result<Self, Error> {
        let frames = backend.keyspace("state_b_frames")?;
        Ok(Self {
            backend,
            frames,
            bytes_written: Arc::new(AtomicU64::new(0)),
            rebase_count: Arc::new(AtomicU64::new(0)),
            dedup_hits: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Total bytes ever passed to `KvWrite::put` for this representation's keyspace.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// How many `apply` calls wrote a full "base" frame rather than a delta layer.
    #[must_use]
    pub fn rebase_count(&self) -> u64 {
        self.rebase_count.load(Ordering::Relaxed)
    }

    /// How many `apply` calls produced a frame whose content hash already existed (the delta was
    /// identical to one already stored from the same parent) and therefore wrote nothing.
    #[must_use]
    pub fn dedup_hits(&self) -> u64 {
        self.dedup_hits.load(Ordering::Relaxed)
    }

    /// Sums key and value bytes resident in this representation's keyspace.
    ///
    /// # Errors
    /// Returns [`Error::Kv`] on a backend failure.
    pub fn bytes_on_disk(&self) -> Result<u64, Error> {
        let snap = self.backend.snapshot();
        let mut total = 0u64;
        for item in snap.range(&self.frames, hs_kv::RangeSpec::full()) {
            let (k, v) = item?;
            total += (k.len() + v.len()) as u64;
        }
        Ok(total)
    }

    fn load(&self, hash: [u8; 16]) -> Result<FrameRecord, Error> {
        let snap = self.backend.snapshot();
        let bytes = snap
            .get(&self.frames, &hash)?
            .ok_or(Error::UnknownRoot(hash))?;
        decode_frame(&bytes)
    }

    fn exists(&self, hash: [u8; 16]) -> Result<bool, Error> {
        let snap = self.backend.snapshot();
        Ok(snap.get(&self.frames, &hash)?.is_some())
    }

    /// Writes `rec` under its content hash, returning that hash. If a frame with the same
    /// content already exists (dedup), skips the write.
    fn store(&self, rec: &FrameRecord) -> Result<[u8; 16], Error> {
        let mut appended_bytes = Vec::new();
        encode_delta_pairs(&mut appended_bytes, &rec.appended);
        let mut disposed_bytes = Vec::new();
        encode_delta_keys(&mut disposed_bytes, &rec.disposed);
        let hash = content_hash(
            u8::from(rec.is_base),
            rec.parent,
            &appended_bytes,
            &disposed_bytes,
        );

        if self.exists(hash)? {
            self.dedup_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hash);
        }

        let encoded = encode_frame(rec);
        self.bytes_written
            .fetch_add(encoded.len() as u64 + 16, Ordering::Relaxed);
        let mut txn = self.backend.begin()?;
        txn.put(&self.frames, &hash, &encoded)?;
        self.backend.commit(txn)?.map_err(|_conflict| {
            // Two writers racing to create the exact same content-addressed frame: the losing
            // side's content already exists under `hash` (that is what content-addressing
            // guarantees), so this is not actually a correctness problem, just treated as an
            // error here because the bake-off harness is single-threaded and should never hit
            // this branch.
            Error::Kv(KvError::backend(std::io::Error::other(
                "unexpected write conflict on a content-addressed frame",
            )))
        })?;
        Ok(hash)
    }

    fn materialize_full(&self, root: RootB) -> Result<BTreeMap<StateKeyId, EventSn>, Error> {
        if root.0 == EMPTY {
            return Ok(BTreeMap::new());
        }
        let mut stack = Vec::new();
        let mut cur = root.0;
        let base = loop {
            if cur == EMPTY {
                break BTreeMap::new();
            }
            let rec = self.load(cur)?;
            if rec.is_base {
                break rec.appended.iter().copied().collect();
            }
            let parent = rec.parent;
            stack.push(rec);
            cur = parent;
        };
        let mut map = base;
        for rec in stack.into_iter().rev() {
            for k in &rec.disposed {
                map.remove(k);
            }
            for (k, v) in &rec.appended {
                map.insert(*k, *v);
            }
        }
        Ok(map)
    }

    fn try_ancestor_diff(&self, from: RootB, to: RootB) -> Result<Option<StateDiff>, Error> {
        let mut hops = Vec::new();
        let mut cur = to.0;
        loop {
            if cur == from.0 {
                let mut added = BTreeMap::new();
                let mut removed = BTreeSet::new();
                for rec in hops.into_iter().rev() {
                    let rec: FrameRecord = rec;
                    for k in &rec.disposed {
                        removed.insert(*k);
                        added.remove(k);
                    }
                    for (k, v) in &rec.appended {
                        added.insert(*k, *v);
                        removed.remove(k);
                    }
                }
                return Ok(Some(StateDiff { added, removed }));
            }
            if cur == EMPTY {
                return Ok(None);
            }
            let rec = self.load(cur)?;
            let is_base = rec.is_base;
            let parent = rec.parent;
            hops.push(rec);
            if is_base {
                return Ok(None);
            }
            cur = parent;
        }
    }
}

impl<KV: KvBackend> ReprStats for FrameRepr<KV> {
    fn bytes_on_disk(&self) -> Result<u64, Error> {
        FrameRepr::bytes_on_disk(self)
    }

    fn bytes_written(&self) -> u64 {
        FrameRepr::bytes_written(self)
    }

    fn compaction_events(&self) -> Option<u64> {
        Some(self.rebase_count())
    }

    fn dedup_hits(&self) -> Option<u64> {
        Some(FrameRepr::dedup_hits(self))
    }
}

impl<KV: KvBackend> StateRepr for FrameRepr<KV> {
    type Root = RootB;
    type Error = Error;

    fn empty_root(&self) -> RootB {
        RootB(EMPTY)
    }

    fn get(&self, root: RootB, key: StateKeyId) -> Result<Option<EventSn>, Error> {
        let mut cur = root.0;
        loop {
            if cur == EMPTY {
                return Ok(None);
            }
            let rec = self.load(cur)?;
            if rec.disposed.binary_search(&key).is_ok() {
                return Ok(None);
            }
            if let Ok(idx) = rec.appended.binary_search_by_key(&key, |(k, _)| *k) {
                return Ok(Some(rec.appended[idx].1));
            }
            if rec.is_base {
                return Ok(None);
            }
            cur = rec.parent;
        }
    }

    fn full_state(&self, root: RootB) -> Result<BTreeMap<StateKeyId, EventSn>, Error> {
        self.materialize_full(root)
    }

    fn diff(&self, from: RootB, to: RootB) -> Result<StateDiff, Error> {
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

    fn apply(&self, root: RootB, changes: &StateDiff) -> Result<RootB, Error> {
        let parent_depth = if root.0 == EMPTY {
            0
        } else {
            self.load(root.0)?.depth
        };
        let new_depth = parent_depth + 1;

        if new_depth % REBASE_INTERVAL == 0 {
            let mut full = self.materialize_full(root)?;
            for k in &changes.removed {
                full.remove(k);
            }
            for (k, v) in &changes.added {
                full.insert(*k, *v);
            }
            let appended: Vec<_> = full.into_iter().collect();
            let hash = self.store(&FrameRecord {
                is_base: true,
                parent: EMPTY,
                depth: 0,
                appended,
                disposed: Vec::new(),
            })?;
            self.rebase_count.fetch_add(1, Ordering::Relaxed);
            Ok(RootB(hash))
        } else {
            let mut appended: Vec<_> = changes.added.iter().map(|(k, v)| (*k, *v)).collect();
            appended.sort_by_key(|(k, _)| *k);
            let mut disposed: Vec<_> = changes.removed.iter().copied().collect();
            disposed.sort_unstable();
            let hash = self.store(&FrameRecord {
                is_base: false,
                parent: root.0,
                depth: new_depth,
                appended,
                disposed,
            })?;
            Ok(RootB(hash))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn repr() -> FrameRepr<MemoryBackend> {
        FrameRepr::new(MemoryBackend::default()).unwrap()
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
    fn identical_delta_from_identical_parent_dedups() {
        let r = repr();
        let root0 = r.empty_root();
        let base = r
            .apply(root0, &StateDiff::set(StateKeyId::new(0), EventSn::new(1)))
            .unwrap();
        let a = r
            .apply(base, &StateDiff::set(StateKeyId::new(5), EventSn::new(9)))
            .unwrap();
        let b = r
            .apply(base, &StateDiff::set(StateKeyId::new(5), EventSn::new(9)))
            .unwrap();
        assert_eq!(
            a, b,
            "identical (parent, appended, disposed) must hash equal"
        );
        assert!(r.dedup_hits() >= 1);
    }

    #[test]
    fn rebase_written_past_interval_and_full_state_still_correct() {
        let r = repr();
        let mut root = r.empty_root();
        let mut expected = BTreeMap::new();
        for i in 0..(REBASE_INTERVAL * 3) {
            let k = StateKeyId::new(i % 10);
            let v = EventSn::new(u64::from(i));
            root = r.apply(root, &StateDiff::set(k, v)).unwrap();
            expected.insert(k, v);
        }
        assert!(r.rebase_count() > 0);
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
        let d = r.diff(branch_a, branch_b).unwrap();
        let applied = r.apply(branch_a, &d).unwrap();
        assert_eq!(
            r.full_state(applied).unwrap(),
            r.full_state(branch_b).unwrap()
        );
    }
}
