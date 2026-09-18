//! The frozen `hs-state` API: [`StateStore`].
//!
//! This is the week-6 seam `docs/workstreams/README.md` names: "`hs-state` API: `state_at`,
//! `diff`, `apply`, `resolve`, chain-cover queries | 02 State and model | 04 Room, 06 Federation".
//! Track 04 (the room actor) and track 06 (federation, for `/state`, `/state_ids` and MSC4242) are
//! the two consumers this interface is frozen for; nothing about it should need to change once
//! `PLAN.md` section 6.3's bake-off picks a concrete state representation, because that
//! representation is exactly what implements [`StateStore`].
//!
//! # Why a trait, and why now
//!
//! `PLAN.md` section 6.3 evaluates three different in-memory/on-disk representations for room
//! state (snapshot-plus-delta-chains, deduplicated frames, a content-addressed persistent map)
//! before picking one by measurement. Track 04 cannot wait twelve weeks for that decision to start
//! building the room actor's state handling, and the decision itself needs the `hs-state` API held
//! fixed so the bake-off is a swap-in, not a rewrite of every caller. A trait is the mechanism for
//! both: everything downstream of "the events for one room" is expressed only in terms of
//! [`StateStore`]'s methods, so a caller written today against any implementation (including the
//! minimal reference one this crate ships, [`crate::store::InMemoryStateStore`]) keeps working
//! against whichever representation wins the bake-off.
//!
//! # What a `Root` is
//!
//! [`StateStore::Root`] is an opaque handle to one fully resolved room state: a value such that,
//! given a `Root`, [`StateStore::get`] can answer "what event set `(event_type, state_key)` *K* in
//! this state" for any *K*, in whatever time the underlying representation offers (a hash-map
//! lookup, a persistent-map traversal, ...). A `Root` is cheap to copy, compare and hold onto: it
//! might be a persistent-map root hash, a state-group ID, or an index into a local table, and
//! callers must not assume anything about its internal shape or attempt to construct one directly
//! -- the only ways to get a `Root` are [`StateStore::state_at`], [`StateStore::apply`] and
//! [`StateStore::resolve`].
//!
//! # `state_at` returns the state *after* the event
//!
//! The spec (room version 1's "State resolution" section, and unchanged since) defines two state
//! maps for an event *E*: *S(E)*, the state used to authorize *E* (the resolution of the state
//! after each of *E*'s `prev_events`), and *S′(E)*, the state after *E* is applied (*S(E)* with
//! *E*'s own entry replacing whatever was at its `(event_type, state_key)`, or *S(E)* unchanged if
//! *E* is not a state event). [`StateStore::state_at`] returns *S′(E)*: "the room's state once this
//! event has happened" is what every caller of this method actually wants (client `/state_at`-style
//! queries, computing the next event's auth state, serving `/state` at a `prev_events` boundary),
//! and it composes better: `state_at(E)` for a state event is exactly
//! `apply(state_at(E's single prev_event or the resolution of several), {E's own change})`. A
//! caller that specifically needs *S(E)* (the state *E* was authorized against) asks for
//! `state_at` of each of *E*'s `prev_events` and resolves those roots itself with
//! [`StateStore::resolve`] -- which is exactly what an implementation's own `state_at` does
//! internally, so this is not extra work, only making the composition explicit at the call site
//! that needs *S(E)* specifically rather than *S′(E)*.
//!
//! # Errors
//!
//! Every method returns `Result<_, Self::Error>`. `Self::Error` covers both "the answer requires
//! data this store does not have" (an unknown `EventSn`, a `Root` from a different store or a
//! different room) and any storage-layer failure (a KV transaction error, once a real backend sits
//! behind this trait); this crate's reference implementation
//! ([`crate::store::InMemoryStateStore`]) uses [`crate::error::StateResError`] extended with a
//! couple of store-specific variants, but the trait itself does not require any particular error
//! type beyond the usual `std::error::Error + Send + Sync + 'static` bound so a KV-backed
//! implementation can wrap its own transaction error type directly.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use hs_model::ids::{EventSn, StateKeyId};
use ruma::RoomVersionId;

use crate::chain_cover::ChainPosition;

/// A set of changes to a room's state: entries added or changed, and entries removed.
///
/// `StateKeyId` already denotes one `(event_type, state_key)` pair (`PLAN.md` section 6.1: "The
/// Conduit and Palpo trick"), so a diff is just two maps/sets over it, not a triple-keyed
/// structure.
///
/// An entry cannot be in both `added` and `removed`; [`StateStore::apply`] implementations should
/// treat that as a caller error (a contradictory diff) rather than picking a resolution order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateDiff {
    /// Entries that are new or changed value, mapped to the event that now sets them.
    pub added: BTreeMap<StateKeyId, EventSn>,
    /// Entries removed entirely (state resolution can do this: a key present in one fork's state
    /// can be absent from the resolved state if no candidate for it passes authorization -- see
    /// `state_res::v1`'s and `state_res::oracle`'s "left out of R" cases).
    pub removed: BTreeSet<StateKeyId>,
}

impl StateDiff {
    /// An empty diff (no changes).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// A diff that sets a single key.
    #[must_use]
    pub fn set(key: StateKeyId, event: EventSn) -> Self {
        Self {
            added: BTreeMap::from([(key, event)]),
            removed: BTreeSet::new(),
        }
    }
}

/// The frozen `hs-state` API. See the module docs for the design rationale.
///
/// Implementations are expected to be cheap to clone or to be used behind a shared reference (a
/// room actor holds one per room it owns); nothing in this trait assumes interior mutability one
/// way or the other, so implementations choose for themselves (a KV-transaction-backed
/// implementation naturally takes `&self` and opens its own transactions; the in-memory reference
/// implementation in this crate uses a `RefCell` internally for the same reason `hs-kv`'s own
/// snapshot handles do).
pub trait StateStore {
    /// An opaque handle to one fully resolved room state. See the module docs, "What a `Root` is".
    type Root: Copy + Eq + Ord + Debug + Send + Sync + 'static;

    /// The error type every method returns. See the module docs, "Errors".
    type Error: std::error::Error + Send + Sync + 'static;

    /// The room's state immediately after `event` (*S′(event)* in the spec's notation -- see the
    /// module docs).
    ///
    /// # Errors
    /// Returns `Self::Error` if `event` is not known to this store.
    fn state_at(&self, event: EventSn) -> Result<Self::Root, Self::Error>;

    /// Looks up one entry in a resolved state.
    ///
    /// Returns `Ok(None)` if `key` has never been set in `root`'s state (not an error: most
    /// `(event_type, state_key)` pairs are absent from most rooms' state).
    ///
    /// # Errors
    /// Returns `Self::Error` if `root` is not known to this store.
    fn get(&self, root: Self::Root, key: StateKeyId) -> Result<Option<EventSn>, Self::Error>;

    /// The changes between two resolved states: what `diff(a, b)` returns, applied to `a` via
    /// [`StateStore::apply`], reproduces `b`.
    ///
    /// Implementations are free to compute this however their representation makes cheapest
    /// (structural sharing makes "which nodes differ" close to free for a persistent map; a
    /// snapshot representation may need a linear merge) -- this is exactly the operation
    /// `PLAN.md` section 6.3's benchmark corpus measures ("diff between two states 1, 100 and
    /// 10,000 state events apart") to compare candidates by.
    ///
    /// # Errors
    /// Returns `Self::Error` if either root is not known to this store.
    fn diff(&self, from: Self::Root, to: Self::Root) -> Result<StateDiff, Self::Error>;

    /// Applies a diff to a state, returning the resulting state.
    ///
    /// `root` is unchanged (this is a persistent/functional update, matching every candidate
    /// representation `PLAN.md` section 6.3 considers: forking cheaply from an existing root, not
    /// mutating it, is what lets the room actor keep several forward extremities' states alive at
    /// once during a fork).
    ///
    /// # Errors
    /// Returns `Self::Error` if `root` is not known to this store, `changes` references an
    /// `EventSn` this store does not know, or `changes` is contradictory (a key in both `added`
    /// and `removed`).
    fn apply(&self, root: Self::Root, changes: &StateDiff) -> Result<Self::Root, Self::Error>;

    /// Resolves several forks of a room's state into one, using the state resolution algorithm
    /// `room_version` specifies (v1: `state_res::v1`; v2/v2.1: `state_res::v2`, backed by
    /// `ruma-state-res`).
    ///
    /// `forks` is typically the [`StateStore::state_at`] of an event's `prev_events`; resolving a
    /// single-element slice returns that element's state unchanged (not an error: most events
    /// have exactly one `prev_event` and resolution is a no-op for them, which callers should not
    /// need to special-case).
    ///
    /// # Errors
    /// Returns `Self::Error` if any root is not known to this store, `forks` is empty (there is no
    /// state to resolve *to*: even a room's `m.room.create` event has a defined state before it,
    /// the empty state, which callers should represent as a single fork rather than none), or the
    /// underlying resolution algorithm fails (a malformed event it needed to inspect, per
    /// [`crate::error::StateResError`]).
    fn resolve(
        &self,
        room_version: &RoomVersionId,
        forks: &[Self::Root],
    ) -> Result<Self::Root, Self::Error>;

    /// This event's position in the room's chain-cover index (`crate::chain_cover`), if the event
    /// has been indexed.
    ///
    /// # Errors
    /// Returns `Self::Error` only for a storage-layer failure; an event simply not being indexed
    /// yet is `Ok(None)`, not an error (outliers and partial-state events, `hs_model::event::EventFlags`,
    /// are exactly the case where an event is known but not yet chain-indexed).
    fn chain_position(&self, event: EventSn) -> Result<Option<ChainPosition>, Self::Error>;

    /// Whether `ancestor` is in `event`'s auth chain (reflexively: `event` is in its own auth
    /// chain). `Ok(None)` if either event is not chain-indexed.
    ///
    /// # Errors
    /// Returns `Self::Error` only for a storage-layer failure.
    fn auth_chain_contains(
        &self,
        event: EventSn,
        ancestor: EventSn,
    ) -> Result<Option<bool>, Self::Error>;

    /// The [auth difference](https://spec.matrix.org/v1.19/rooms/v2/#definitions) of several event
    /// sets: state resolution v2/v2.1's most expensive primitive, and the whole reason the
    /// chain-cover index exists (`PLAN.md` section 6.4).
    ///
    /// # Errors
    /// Returns `Self::Error` only for a storage-layer failure; an event in `sets` that is not
    /// chain-indexed is treated as contributing no ancestors beyond itself, matching
    /// [`crate::chain_cover::ChainCoverIndex::coverage`]'s handling of events outside the indexed
    /// region.
    fn auth_chain_difference(&self, sets: &[Vec<EventSn>]) -> Result<Vec<EventSn>, Self::Error>;
}
