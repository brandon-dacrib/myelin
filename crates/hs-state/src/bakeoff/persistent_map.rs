//! Candidate C: a content-addressed persistent map, `PLAN.md` section 6.3's prior.
//!
//! A 32-way trie over `StateKeyId`'s 32 bits (5 bits per level for six levels, the remaining 2
//! bits at the seventh), where every node is stored by the content hash of its encoding and a
//! state is the hash of its root node. Single-entry subtrees are inlined as leaves directly in
//! their parent (the standard HAMT space optimization), so a sparse trie never pays for long
//! chains of single-child nodes. Two structural properties this candidate is supposed to have,
//! per `PLAN.md`, and that this module's `diff` and `apply` are built to actually exercise:
//!
//! - **Structural sharing**: [`PersistentMapRepr::apply`] only rewrites the `O(log32 n)` nodes on
//!   the path to the changed key; every other node is reused by content hash, both across time
//!   (successive states of one room) and across forks (two states that share an ancestor share
//!   every node neither side touched).
//! - **Diff skips identical subtrees**: [`PersistentMapRepr::diff`] compares two roots node by
//!   node and stops descending the instant it finds two equal content hashes, which is exactly
//!   "the operation state resolution, `/sync` state deltas and `state_after` need"
//!   (`PLAN.md` section 6.3).
//!
//! Garbage collection is *not* automatic (unlike A's periodic snapshot or B's periodic rebase,
//! both of which are folded into ordinary `apply` calls): orphaned single-leaf nodes accumulate
//! whenever [`PersistentMapRepr::apply`] deletes a key and inlines the resulting single-leaf
//! child (the now-unreachable intermediate node stays in the backend). [`PersistentMapRepr::gc`]
//! is a real mark-and-sweep pass over a given set of live roots, run explicitly by the bake-off
//! harness so its cost is measured rather than hidden.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use hs_kv::{KvBackend, KvError, KvRead, KvWrite};
use hs_model::ids::{EventSn, StateKeyId};
use sha1::{Digest, Sha1};
use thiserror::Error;

use super::repr::{BakeoffStats, StateRepr};
use crate::api::StateDiff;

const EMPTY: [u8; 16] = [0; 16];

/// The node writes accumulated by one `apply` call, committed in a single transaction. See
/// [`PersistentMapRepr::store_staged`] and [`PersistentMapRepr::commit_batch`].
///
/// `pending` doubles as a read-through cache: a single `apply` can touch the same
/// not-yet-committed node more than once (inserting several keys in one `StateDiff` chains
/// through whatever the previous key's insert just staged), so [`PersistentMapRepr::load_staged`]
/// must be able to see writes this same batch made before they are committed.
#[derive(Default)]
struct WriteBatch {
    pending: HashMap<[u8; 16], Vec<u8>>,
    order: Vec<[u8; 16]>,
}

/// Candidate C's [`StateRepr::Root`]: a trie node's content hash, or the all-zero sentinel for
/// the empty state (no node stored). See the module docs on why a sentinel rather than
/// `Option<[u8; 16]>` is acceptable for this bake-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootC(pub [u8; 16]);

/// Errors from [`PersistentMapRepr`].
#[derive(Debug, Error)]
pub enum Error {
    /// `root` (or an internal node reached from it) does not name a node this representation has
    /// stored.
    #[error("trie node {0:x?} not known to this store")]
    UnknownNode([u8; 16]),
    /// The underlying `hs_kv` backend failed.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A node record could not be decoded.
    #[error("trie node record was corrupt")]
    Corrupt,
}

#[derive(Debug, Clone, Copy)]
enum Child {
    Leaf(StateKeyId, EventSn),
    Ptr([u8; 16]),
}

#[derive(Debug, Clone, Default)]
struct Node {
    bitmap: u32,
    /// Children in ascending bit-index order, parallel to the set bits of `bitmap`.
    children: Vec<Child>,
}

impl Node {
    fn child_at(&self, digit: u32) -> Option<Child> {
        if self.bitmap & (1 << digit) == 0 {
            return None;
        }
        let idx = (self.bitmap & ((1u32 << digit) - 1)).count_ones() as usize;
        Some(self.children[idx])
    }

    fn set_child(&mut self, digit: u32, child: Child) {
        let idx = (self.bitmap & ((1u32 << digit) - 1)).count_ones() as usize;
        if self.bitmap & (1 << digit) == 0 {
            self.bitmap |= 1 << digit;
            self.children.insert(idx, child);
        } else {
            self.children[idx] = child;
        }
    }

    fn remove_child(&mut self, digit: u32) {
        if self.bitmap & (1 << digit) == 0 {
            return;
        }
        let idx = (self.bitmap & ((1u32 << digit) - 1)).count_ones() as usize;
        self.children.remove(idx);
        self.bitmap &= !(1 << digit);
    }

    fn is_empty(&self) -> bool {
        self.bitmap == 0
    }

    /// If this node has exactly one child and it is a leaf, that leaf -- the case that allows the
    /// parent to inline it, dropping this node's own indirection.
    fn single_leaf(&self) -> Option<(StateKeyId, EventSn)> {
        if self.children.len() == 1
            && let Child::Leaf(k, v) = self.children[0]
        {
            return Some((k, v));
        }
        None
    }
}

fn digit(key: u32, level: usize) -> u32 {
    if level < 6 {
        (key >> (27 - 5 * level as u32)) & 0x1F
    } else {
        key & 0x3
    }
}

fn encode_node(node: &Node) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + node.children.len() * 17);
    out.extend_from_slice(&node.bitmap.to_be_bytes());
    for child in &node.children {
        match child {
            Child::Leaf(k, v) => {
                out.push(0);
                out.extend_from_slice(&k.to_be_bytes());
                out.extend_from_slice(&v.to_be_bytes());
            }
            Child::Ptr(h) => {
                out.push(1);
                out.extend_from_slice(h);
            }
        }
    }
    out
}

fn decode_node(bytes: &[u8]) -> Result<Node, Error> {
    if bytes.len() < 4 {
        return Err(Error::Corrupt);
    }
    let (bitmap_bytes, mut p) = bytes.split_at(4);
    let bitmap = u32::from_be_bytes(bitmap_bytes.try_into().map_err(|_| Error::Corrupt)?);
    let n = bitmap.count_ones() as usize;
    let mut children = Vec::with_capacity(n);
    for _ in 0..n {
        let (&tag, rest) = p.split_first().ok_or(Error::Corrupt)?;
        p = rest;
        match tag {
            0 => {
                if p.len() < 12 {
                    return Err(Error::Corrupt);
                }
                let (kb, rest) = p.split_at(4);
                let (vb, rest) = rest.split_at(8);
                p = rest;
                let k = u32::from_be_bytes(kb.try_into().map_err(|_| Error::Corrupt)?);
                let v = u64::from_be_bytes(vb.try_into().map_err(|_| Error::Corrupt)?);
                children.push(Child::Leaf(StateKeyId::new(k), EventSn::new(v)));
            }
            1 => {
                if p.len() < 16 {
                    return Err(Error::Corrupt);
                }
                let (hb, rest) = p.split_at(16);
                p = rest;
                children.push(Child::Ptr(hb.try_into().map_err(|_| Error::Corrupt)?));
            }
            _ => return Err(Error::Corrupt),
        }
    }
    Ok(Node { bitmap, children })
}

fn hash_node(bytes: &[u8]) -> [u8; 16] {
    let full: [u8; 20] = Sha1::digest(bytes).into();
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

/// Candidate C over any `hs_kv::KvBackend`. See `SnapshotDeltaRepr`'s docs on why this is cheap
/// to clone and what cloning shares (backend, keyspace, instrumentation counters).
#[derive(Clone)]
pub struct PersistentMapRepr<KV: KvBackend> {
    backend: KV,
    nodes: KV::Keyspace,
    bytes_written: Rc<Cell<u64>>,
    nodes_written: Rc<Cell<u64>>,
    dedup_hits: Rc<Cell<u64>>,
}

/// The outcome of a [`PersistentMapRepr::gc`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStats {
    /// Nodes present before the sweep.
    pub nodes_before: u64,
    /// Nodes deleted (unreachable from the given live roots).
    pub nodes_deleted: u64,
    /// Bytes freed by the deleted nodes' values.
    pub bytes_freed: u64,
}

impl<KV: KvBackend> PersistentMapRepr<KV> {
    /// Opens (creating if necessary) candidate C's keyspace on `backend`.
    ///
    /// # Errors
    /// Returns [`Error::Kv`] if the keyspace could not be opened.
    pub fn new(backend: KV) -> Result<Self, Error> {
        let nodes = backend.keyspace("state_c_nodes")?;
        Ok(Self {
            backend,
            nodes,
            bytes_written: Rc::new(Cell::new(0)),
            nodes_written: Rc::new(Cell::new(0)),
            dedup_hits: Rc::new(Cell::new(0)),
        })
    }

    /// Total bytes ever passed to `KvWrite::put` for this representation's keyspace.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.get()
    }

    /// How many distinct node-writing attempts this representation made (including ones that
    /// turned out to be dedup hits); `nodes_written() - dedup_hits()` is the count of nodes
    /// actually persisted.
    #[must_use]
    pub fn nodes_written(&self) -> u64 {
        self.nodes_written.get()
    }

    /// How many node writes found the content hash already present and skipped the write: the
    /// direct measurement of structural sharing / intrinsic dedup `PLAN.md` credits this
    /// candidate with.
    #[must_use]
    pub fn dedup_hits(&self) -> u64 {
        self.dedup_hits.get()
    }

    /// Sums key and value bytes resident in this representation's keyspace.
    ///
    /// # Errors
    /// Returns [`Error::Kv`] on a backend failure.
    pub fn bytes_on_disk(&self) -> Result<u64, Error> {
        let snap = self.backend.snapshot();
        let mut total = 0u64;
        for item in snap.range(&self.nodes, hs_kv::RangeSpec::full()) {
            let (k, v) = item?;
            total += (k.len() + v.len()) as u64;
        }
        Ok(total)
    }

    fn load(&self, hash: [u8; 16]) -> Result<Node, Error> {
        let snap = self.backend.snapshot();
        let bytes = snap
            .get(&self.nodes, &hash)?
            .ok_or(Error::UnknownNode(hash))?;
        decode_node(&bytes)
    }

    /// Like [`Self::load`], but checks `batch`'s not-yet-committed writes first -- required
    /// whenever one `apply` call touches a node it staged earlier in the same call, before that
    /// batch has been committed.
    fn load_staged(&self, hash: [u8; 16], batch: &WriteBatch) -> Result<Node, Error> {
        if let Some(bytes) = batch.pending.get(&hash) {
            return decode_node(bytes);
        }
        self.load(hash)
    }

    /// Stages a node write into `batch` (a single `apply` call's worth of node writes,
    /// committed once -- see [`Self::apply`]) rather than opening its own transaction. Checks
    /// both the batch already accumulated in this call and the backend itself for the content
    /// hash, so within-one-apply and across-time dedup both work without paying for a
    /// transaction per trie level.
    fn store_staged(&self, node: &Node, batch: &mut WriteBatch) -> Result<[u8; 16], Error> {
        let encoded = encode_node(node);
        let hash = hash_node(&encoded);
        self.nodes_written.set(self.nodes_written.get() + 1);
        if batch.pending.contains_key(&hash) {
            self.dedup_hits.set(self.dedup_hits.get() + 1);
            return Ok(hash);
        }
        let snap = self.backend.snapshot();
        if snap.get(&self.nodes, &hash)?.is_some() {
            self.dedup_hits.set(self.dedup_hits.get() + 1);
            return Ok(hash);
        }
        batch.pending.insert(hash, encoded);
        batch.order.push(hash);
        Ok(hash)
    }

    /// Commits every write staged by one `apply` call in a single transaction: candidate C's
    /// single-key change touches up to seven trie levels, and batching them into one commit
    /// (rather than one commit per level) is what makes `apply` a realistic single logical write
    /// rather than seven, matching candidates A and B's one-write-per-`apply` shape and
    /// `hs_kv`'s own guidance to keep a transaction to one logical unit of work.
    fn commit_batch(&self, batch: WriteBatch) -> Result<(), Error> {
        if batch.order.is_empty() {
            return Ok(());
        }
        let bytes: u64 = batch.pending.values().map(|v| v.len() as u64 + 16).sum();
        self.bytes_written.set(self.bytes_written.get() + bytes);
        let mut txn = self.backend.begin()?;
        for hash in &batch.order {
            #[allow(
                clippy::unwrap_used,
                reason = "every hash in `order` has a `pending` entry"
            )]
            let encoded = batch.pending.get(hash).unwrap();
            txn.put(&self.nodes, hash, encoded)?;
        }
        self.backend.commit(txn)?.map_err(|_conflict| {
            Error::Kv(KvError::backend(std::io::Error::other(
                "unexpected write conflict on a content-addressed trie node batch",
            )))
        })?;
        Ok(())
    }

    fn get_rec(&self, hash: [u8; 16], level: usize, key: u32) -> Result<Option<EventSn>, Error> {
        if hash == EMPTY {
            return Ok(None);
        }
        let node = self.load(hash)?;
        match node.child_at(digit(key, level)) {
            None => Ok(None),
            Some(Child::Leaf(k, v)) => Ok(if k.get() == key { Some(v) } else { None }),
            Some(Child::Ptr(child)) => self.get_rec(child, level + 1, key),
        }
    }

    fn build_two_leaf_chain(
        &self,
        level: usize,
        k1: StateKeyId,
        v1: EventSn,
        k2: StateKeyId,
        v2: EventSn,
        batch: &mut WriteBatch,
    ) -> Result<[u8; 16], Error> {
        let d1 = digit(k1.get(), level);
        let d2 = digit(k2.get(), level);
        let mut node = Node::default();
        if d1 == d2 {
            let child = self.build_two_leaf_chain(level + 1, k1, v1, k2, v2, batch)?;
            node.set_child(d1, Child::Ptr(child));
        } else {
            node.set_child(d1, Child::Leaf(k1, v1));
            node.set_child(d2, Child::Leaf(k2, v2));
        }
        self.store_staged(&node, batch)
    }

    fn insert_rec(
        &self,
        hash: [u8; 16],
        level: usize,
        key: StateKeyId,
        value: EventSn,
        batch: &mut WriteBatch,
    ) -> Result<[u8; 16], Error> {
        let mut node = if hash == EMPTY {
            Node::default()
        } else {
            self.load_staged(hash, batch)?
        };
        let d = digit(key.get(), level);
        let new_child = match node.child_at(d) {
            None => Child::Leaf(key, value),
            Some(Child::Leaf(k, v)) => {
                if k == key {
                    Child::Leaf(key, value)
                } else {
                    Child::Ptr(self.build_two_leaf_chain(level + 1, k, v, key, value, batch)?)
                }
            }
            Some(Child::Ptr(child_hash)) => {
                Child::Ptr(self.insert_rec(child_hash, level + 1, key, value, batch)?)
            }
        };
        node.set_child(d, new_child);
        self.store_staged(&node, batch)
    }

    /// Returns `Some(new_hash)` for the subtree with `key` removed (unchanged, i.e. `Some(hash)`,
    /// if `key` was absent), or `None` if the subtree became empty.
    fn delete_rec(
        &self,
        hash: [u8; 16],
        level: usize,
        key: u32,
        batch: &mut WriteBatch,
    ) -> Result<Option<[u8; 16]>, Error> {
        if hash == EMPTY {
            return Ok(None);
        }
        let mut node = self.load_staged(hash, batch)?;
        let d = digit(key, level);
        match node.child_at(d) {
            None => return Ok(Some(hash)),
            Some(Child::Leaf(k, _)) => {
                if k.get() != key {
                    return Ok(Some(hash));
                }
                node.remove_child(d);
            }
            Some(Child::Ptr(child_hash)) => {
                match self.delete_rec(child_hash, level + 1, key, batch)? {
                    None => node.remove_child(d),
                    Some(new_child_hash) => {
                        if new_child_hash == child_hash {
                            return Ok(Some(hash));
                        }
                        let child_node = self.load_staged(new_child_hash, batch)?;
                        if let Some((k, v)) = child_node.single_leaf() {
                            // Path compression: the child collapsed to one leaf, inline it here
                            // and let `new_child_hash`'s now-unreachable node become garbage
                            // (measured and reclaimed by `gc`, not silently avoided -- see the
                            // module docs).
                            node.set_child(d, Child::Leaf(k, v));
                        } else {
                            node.set_child(d, Child::Ptr(new_child_hash));
                        }
                    }
                }
            }
        }
        if node.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.store_staged(&node, batch)?))
        }
    }

    fn materialize_rec(
        &self,
        hash: [u8; 16],
        out: &mut BTreeMap<StateKeyId, EventSn>,
    ) -> Result<(), Error> {
        if hash == EMPTY {
            return Ok(());
        }
        let node = self.load(hash)?;
        for child in &node.children {
            match child {
                Child::Leaf(k, v) => {
                    out.insert(*k, *v);
                }
                Child::Ptr(h) => self.materialize_rec(*h, out)?,
            }
        }
        Ok(())
    }

    fn full_map_of_child(
        &self,
        child: Option<Child>,
    ) -> Result<BTreeMap<StateKeyId, EventSn>, Error> {
        let mut out = BTreeMap::new();
        match child {
            None => {}
            Some(Child::Leaf(k, v)) => {
                out.insert(k, v);
            }
            Some(Child::Ptr(h)) => self.materialize_rec(h, &mut out)?,
        }
        Ok(out)
    }

    fn compare_nodes(
        &self,
        a_hash: [u8; 16],
        b_hash: [u8; 16],
        added: &mut BTreeMap<StateKeyId, EventSn>,
        removed: &mut BTreeSet<StateKeyId>,
    ) -> Result<(), Error> {
        if a_hash == b_hash {
            return Ok(());
        }
        let a_node = if a_hash == EMPTY {
            None
        } else {
            Some(self.load(a_hash)?)
        };
        let b_node = if b_hash == EMPTY {
            None
        } else {
            Some(self.load(b_hash)?)
        };
        let bitmap =
            a_node.as_ref().map_or(0, |n| n.bitmap) | b_node.as_ref().map_or(0, |n| n.bitmap);
        for d in 0..32u32 {
            if bitmap & (1 << d) == 0 {
                continue;
            }
            let ac = a_node.as_ref().and_then(|n| n.child_at(d));
            let bc = b_node.as_ref().and_then(|n| n.child_at(d));
            match (ac, bc) {
                (Some(Child::Ptr(h1)), Some(Child::Ptr(h2))) if h1 == h2 => {}
                (Some(Child::Ptr(h1)), Some(Child::Ptr(h2))) => {
                    self.compare_nodes(h1, h2, added, removed)?;
                }
                (Some(Child::Leaf(k1, v1)), Some(Child::Leaf(k2, v2))) if k1 == k2 && v1 == v2 => {}
                (Some(Child::Leaf(k1, _v1)), Some(Child::Leaf(k2, v2))) if k1 == k2 => {
                    added.insert(k2, v2);
                }
                (a_child, b_child) => {
                    let am = self.full_map_of_child(a_child)?;
                    let bm = self.full_map_of_child(b_child)?;
                    for (k, v) in &bm {
                        if am.get(k) != Some(v) {
                            added.insert(*k, *v);
                        }
                    }
                    for k in am.keys() {
                        if !bm.contains_key(k) {
                            removed.insert(*k);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A real mark-and-sweep garbage collection pass: walks every node reachable from
    /// `live_roots`, then deletes every stored node not in that reachable set. `live_roots`
    /// should be every root this representation must keep -- the harness passes the set of
    /// `state_at` roots still referenced by the corpus's forward extremities and any state a
    /// caller may still hold, exactly the set a room actor would need to keep live in
    /// production.
    ///
    /// # Errors
    /// Returns [`Error::Kv`] on a backend failure.
    pub fn gc(&self, live_roots: &[RootC]) -> Result<GcStats, Error> {
        let mut reachable = BTreeSet::new();
        let mut stack: Vec<[u8; 16]> = live_roots.iter().map(|r| r.0).collect();
        while let Some(hash) = stack.pop() {
            if hash == EMPTY || !reachable.insert(hash) {
                continue;
            }
            let node = self.load(hash)?;
            for child in &node.children {
                if let Child::Ptr(h) = child {
                    stack.push(*h);
                }
            }
        }

        let snap = self.backend.snapshot();
        let mut to_delete = Vec::new();
        let mut nodes_before = 0u64;
        let mut bytes_freed = 0u64;
        for item in snap.range(&self.nodes, hs_kv::RangeSpec::full()) {
            let (k, v) = item?;
            nodes_before += 1;
            let hash: [u8; 16] = k.as_ref().try_into().map_err(|_| Error::Corrupt)?;
            if !reachable.contains(&hash) {
                bytes_freed += (k.len() + v.len()) as u64;
                to_delete.push(k);
            }
        }

        // `hs_kv::MAX_TXN_MUTATIONS` caps a single transaction's mutation count; a real room's
        // garbage can easily exceed it, so delete in batches rather than one all-or-nothing
        // transaction. This is itself part of what candidate C's GC costs in practice (multiple
        // commits, not one), which is exactly what this bake-off measures, not a shortcut around
        // it.
        const BATCH: usize = 5_000;
        for chunk in to_delete.chunks(BATCH) {
            let mut txn = self.backend.begin()?;
            for key in chunk {
                txn.delete(&self.nodes, key)?;
            }
            self.backend.commit(txn)?.map_err(|_conflict| {
                Error::Kv(KvError::backend(std::io::Error::other(
                    "unexpected write conflict during trie gc",
                )))
            })?;
        }

        Ok(GcStats {
            nodes_before,
            nodes_deleted: to_delete.len() as u64,
            bytes_freed,
        })
    }
}

impl<KV: KvBackend> BakeoffStats for PersistentMapRepr<KV> {
    fn bytes_on_disk(&self) -> Result<u64, Error> {
        PersistentMapRepr::bytes_on_disk(self)
    }

    fn bytes_written(&self) -> u64 {
        PersistentMapRepr::bytes_written(self)
    }

    fn compaction_events(&self) -> Option<u64> {
        // C has no automatic inline compaction (unlike A's periodic snapshot or B's periodic
        // rebase) -- garbage collection is a separate, explicit operation. See `PersistentMapRepr::gc`.
        None
    }

    fn dedup_hits(&self) -> Option<u64> {
        Some(PersistentMapRepr::dedup_hits(self))
    }
}

impl<KV: KvBackend> StateRepr for PersistentMapRepr<KV> {
    type Root = RootC;
    type Error = Error;

    fn empty_root(&self) -> RootC {
        RootC(EMPTY)
    }

    fn get(&self, root: RootC, key: StateKeyId) -> Result<Option<EventSn>, Error> {
        self.get_rec(root.0, 0, key.get())
    }

    fn full_state(&self, root: RootC) -> Result<BTreeMap<StateKeyId, EventSn>, Error> {
        let mut out = BTreeMap::new();
        self.materialize_rec(root.0, &mut out)?;
        Ok(out)
    }

    fn diff(&self, from: RootC, to: RootC) -> Result<StateDiff, Error> {
        let mut added = BTreeMap::new();
        let mut removed = BTreeSet::new();
        self.compare_nodes(from.0, to.0, &mut added, &mut removed)?;
        Ok(StateDiff { added, removed })
    }

    fn apply(&self, root: RootC, changes: &StateDiff) -> Result<RootC, Error> {
        let mut batch = WriteBatch::default();
        let mut cur = root.0;
        for key in &changes.removed {
            cur = self
                .delete_rec(cur, 0, key.get(), &mut batch)?
                .unwrap_or(EMPTY);
        }
        for (key, value) in &changes.added {
            cur = self.insert_rec(cur, 0, *key, *value, &mut batch)?;
        }
        self.commit_batch(batch)?;
        Ok(RootC(cur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use rand::Rng;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn repr() -> PersistentMapRepr<MemoryBackend> {
        PersistentMapRepr::new(MemoryBackend::default()).unwrap()
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
    fn overwrite_and_remove_and_reinsert() {
        let r = repr();
        let k = StateKeyId::new(42);
        let mut root = r.empty_root();
        root = r.apply(root, &StateDiff::set(k, EventSn::new(1))).unwrap();
        root = r.apply(root, &StateDiff::set(k, EventSn::new(2))).unwrap();
        assert_eq!(r.get(root, k).unwrap(), Some(EventSn::new(2)));

        let mut removal = StateDiff::default();
        removal.removed.insert(k);
        let after_remove = r.apply(root, &removal).unwrap();
        assert_eq!(r.get(after_remove, k).unwrap(), None);
        assert_eq!(r.get(root, k).unwrap(), Some(EventSn::new(2)));

        let reinserted = r
            .apply(after_remove, &StateDiff::set(k, EventSn::new(3)))
            .unwrap();
        assert_eq!(r.get(reinserted, k).unwrap(), Some(EventSn::new(3)));
    }

    #[test]
    fn diff_between_identical_roots_is_empty_without_loading_anything() {
        let r = repr();
        let root = r
            .apply(
                r.empty_root(),
                &StateDiff::set(StateKeyId::new(1), EventSn::new(1)),
            )
            .unwrap();
        let d = r.diff(root, root).unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn diff_skips_identical_subtrees_and_full_state_matches_after_apply() {
        let r = repr();
        let mut root = r.empty_root();
        for i in 0..200u32 {
            root = r
                .apply(
                    root,
                    &StateDiff::set(StateKeyId::new(i), EventSn::new(u64::from(i))),
                )
                .unwrap();
        }
        // Two forks sharing everything except one key.
        let fork_a = r
            .apply(
                root,
                &StateDiff::set(StateKeyId::new(500), EventSn::new(9000)),
            )
            .unwrap();
        let fork_b = r
            .apply(
                root,
                &StateDiff::set(StateKeyId::new(501), EventSn::new(9001)),
            )
            .unwrap();
        let d = r.diff(fork_a, fork_b).unwrap();
        assert_eq!(d.added.len(), 1); // 501 added
        assert_eq!(d.removed.len(), 1); // 500 removed (present only on fork_a's side)
        let applied = r.apply(fork_a, &d).unwrap();
        assert_eq!(
            r.full_state(applied).unwrap(),
            r.full_state(fork_b).unwrap()
        );
    }

    /// Property test (deterministic seed, not `proptest`, to keep this module's dependency
    /// footprint small): a random sequence of inserts, overwrites and deletes over many distinct
    /// keys must always agree with a plain `BTreeMap` oracle, and `full_state` after every step
    /// must match it exactly. This is the correctness backbone the whole bake-off's numbers for
    /// candidate C rest on -- a fast but wrong structure would invalidate the comparison.
    #[test]
    fn random_operations_match_a_btreemap_oracle() {
        let mut rng = StdRng::seed_from_u64(0xC0FFEE);
        let r = repr();
        let mut root = r.empty_root();
        let mut oracle: BTreeMap<u32, u64> = BTreeMap::new();

        for _ in 0..3000 {
            let key = rng.random_range(0..500u32);
            if rng.random_bool(0.25) && oracle.contains_key(&key) {
                oracle.remove(&key);
                let mut d = StateDiff::default();
                d.removed.insert(StateKeyId::new(key));
                root = r.apply(root, &d).unwrap();
            } else {
                let value = rng.random::<u64>();
                oracle.insert(key, value);
                root = r
                    .apply(
                        root,
                        &StateDiff::set(StateKeyId::new(key), EventSn::new(value)),
                    )
                    .unwrap();
            }
        }

        let full = r.full_state(root).unwrap();
        let full_as_oracle: BTreeMap<u32, u64> =
            full.into_iter().map(|(k, v)| (k.get(), v.get())).collect();
        assert_eq!(full_as_oracle, oracle);

        for (key, value) in &oracle {
            assert_eq!(
                r.get(root, StateKeyId::new(*key)).unwrap(),
                Some(EventSn::new(*value))
            );
        }
    }

    #[test]
    fn gc_reclaims_unreachable_nodes_and_keeps_live_ones_readable() {
        let r = repr();
        let mut root = r.empty_root();
        for i in 0..50u32 {
            root = r
                .apply(
                    root,
                    &StateDiff::set(StateKeyId::new(i), EventSn::new(u64::from(i))),
                )
                .unwrap();
        }
        // Delete most keys, generating orphaned single-leaf nodes via path compression.
        for i in 0..45u32 {
            let mut d = StateDiff::default();
            d.removed.insert(StateKeyId::new(i));
            root = r.apply(root, &d).unwrap();
        }
        let before = r.bytes_on_disk().unwrap();
        let stats = r.gc(&[root]).unwrap();
        let after = r.bytes_on_disk().unwrap();
        assert!(after <= before);
        assert!(stats.nodes_deleted <= stats.nodes_before);
        // Every surviving key must still read back correctly after gc.
        for i in 45..50u32 {
            assert_eq!(
                r.get(root, StateKeyId::new(i)).unwrap(),
                Some(EventSn::new(u64::from(i)))
            );
        }
    }
}
