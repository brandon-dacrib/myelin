//! [`StateRepr`]: the narrow interface a resolved-state storage representation implements.
//!
//! `PLAN.md` section 6.3's bake-off (`docs/decisions/0006-state-bakeoff-results.md`) picked
//! candidate B (`crate::frames::FrameRepr`) as the production representation; candidates A and C
//! (`crate::bakeoff::snapshot_delta`, `crate::bakeoff::persistent_map`) remain in the crate as
//! benchmark-only implementations of this same trait, kept for the results document's "What would
//! change this decision" re-run scenarios, not for production use. [`KvStateStore`]
//! (`crate::kv_store`) is the shared code generic over one `StateRepr` implementation -- ingesting
//! events, calling `state_res::v1`/`v2`, the chain-cover index -- so the winning representation and
//! the two benchmark-only ones (and `InMemoryStateStore`'s hand-written equivalent) do not each
//! reimplement it. This keeps the bake-off honest about what it measured (state *storage*, see
//! `docs/decisions/0005-state-bakeoff-methodology.md`, "What is and is not varied") and avoids
//! three near-duplicate copies of resolution glue now that one of the three is production code.

use std::collections::BTreeMap;
use std::fmt::Debug;

use hs_model::ids::{EventSn, StateKeyId};

use crate::api::StateDiff;

/// One candidate's storage representation of resolved room state.
///
/// A `Root` here is exactly [`crate::api::StateStore::Root`] for whichever representation
/// implements this trait; [`crate::kv_store::KvStateStore`] is the adapter that makes any
/// `StateRepr` into a full `StateStore`.
pub trait StateRepr {
    /// Opaque handle to one resolved state, as stored by this representation.
    type Root: Copy + Eq + Ord + Debug + Send + Sync + 'static;
    /// This representation's error type (typically wrapping `hs_kv::KvError`).
    type Error: std::error::Error + Send + Sync + 'static;

    /// The state with no keys set. Never actually stored -- every candidate uses a sentinel
    /// value for this so an empty room's `m.room.create` predecessor state costs nothing.
    fn empty_root(&self) -> Self::Root;

    /// Looks up one entry. `Ok(None)` if `key` was never set in `root`'s state.
    ///
    /// # Errors
    /// Returns `Self::Error` if `root` is not known to this representation, or on a storage
    /// failure.
    fn get(&self, root: Self::Root, key: StateKeyId) -> Result<Option<EventSn>, Self::Error>;

    /// The full state map for `root`. Used only by state resolution (which needs every entry,
    /// not one key at a time) -- not part of [`crate::api::StateStore`] itself, and deliberately
    /// the most expensive operation this trait exposes: a candidate whose representation makes
    /// this cheap (or, better, avoidable in the common case) has a real advantage the bake-off's
    /// "resolution time on forks" measurement is designed to surface.
    ///
    /// # Errors
    /// Returns `Self::Error` if `root` is not known to this representation, or on a storage
    /// failure.
    fn full_state(&self, root: Self::Root) -> Result<BTreeMap<StateKeyId, EventSn>, Self::Error>;

    /// The changes between two states. See [`crate::api::StateStore::diff`] -- this is that
    /// method's whole implementation for whichever candidate implements it.
    ///
    /// # Errors
    /// Returns `Self::Error` if either root is not known to this representation, or on a storage
    /// failure.
    fn diff(&self, from: Self::Root, to: Self::Root) -> Result<StateDiff, Self::Error>;

    /// Applies a diff, returning the resulting state. `root` is unchanged (persistent update).
    ///
    /// # Errors
    /// Returns `Self::Error` if `root` is not known to this representation, `changes` is
    /// contradictory, or on a storage failure.
    fn apply(&self, root: Self::Root, changes: &StateDiff) -> Result<Self::Root, Self::Error>;
}

/// Instrumentation a [`StateRepr`] exposes, beyond the storage operations that trait itself
/// needs, so the bake-off harness (`src/bin/bakeoff.rs`) can read bytes-on-disk, write
/// amplification and compaction/dedup counters generically across every representation
/// (including the production one, `crate::frames::FrameRepr`, which implements this too).
pub trait ReprStats: StateRepr {
    /// Sums key and value bytes resident in this representation's own keyspace(s).
    ///
    /// # Errors
    /// Returns `Self::Error` on a storage failure.
    fn bytes_on_disk(&self) -> Result<u64, Self::Error>;

    /// Total bytes ever passed to the backend's `put` for this representation.
    fn bytes_written(&self) -> u64;

    /// How many writes were a "compaction" event in this candidate's own terms: a full snapshot
    /// (A), a re-basing frame (B). `None` for a candidate with no such automatic, inline
    /// compaction (C, whose garbage collection is a separate, explicit operation -- see
    /// `bakeoff::persistent_map::PersistentMapRepr::gc`).
    fn compaction_events(&self) -> Option<u64>;

    /// How many writes were skipped because the content hash already existed. `None` for a
    /// candidate with no content-addressed dedup (A).
    fn dedup_hits(&self) -> Option<u64>;
}

/// The plain set-difference between two full state maps: `O(|a| + |b|)`, used by every
/// candidate's fallback path when a cheaper structural shortcut (ancestor-chain walk for A/B,
/// identical-subtree-hash skip for C) does not apply. Shared here so the "general case" cost
/// model is identical across candidates, and only the fast paths differ.
#[must_use]
pub(crate) fn set_diff(
    a: &BTreeMap<StateKeyId, EventSn>,
    b: &BTreeMap<StateKeyId, EventSn>,
) -> StateDiff {
    let mut added = BTreeMap::new();
    for (key, event) in b {
        if a.get(key) != Some(event) {
            added.insert(*key, *event);
        }
    }
    let removed = a
        .keys()
        .filter(|key| !b.contains_key(key))
        .copied()
        .collect();
    StateDiff { added, removed }
}
