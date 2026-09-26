//! [`KvStateStore`]: the production [`crate::api::StateStore`], generic over one
//! [`StateRepr`] implementation.
//!
//! This is [`crate::store::InMemoryStateStore`]'s ingestion and resolution logic, unchanged in
//! substance, made generic over the state representation so it is not duplicated across the
//! production representation (`crate::frames::FrameRepr`, the bake-off's winning candidate B --
//! `docs/decisions/0006-state-bakeoff-results.md`) and the two benchmark-only representations kept
//! under `crate::bakeoff` for that decision's own "what would change this decision" re-runs.
//! [`ProductionStateStore`] is this type instantiated with the production representation; that is
//! what tracks 04 and 06 should hold.
//!
//! `InMemoryStateStore` itself is deliberately *not* rebuilt on top of this module (it stays a
//! hand-written, dependency-light reference implementation for tests -- see its own module docs);
//! this module and `crate::store` therefore still look similar to each other by construction, not
//! by accident.

use std::cell::RefCell;
use std::collections::BTreeMap;

use hs_kv::KvBackend;
use hs_model::canonical::CanonicalJsonObject;
use hs_model::ids::{EventSn, StateKeyId};
use hs_model::room_version::{self, RoomVersionRules, StateResolutionVersion};
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId};
use thiserror::Error;

use crate::api::{StateDiff, StateStore};
use crate::chain_cover::{ChainCoverIndex, ChainPosition};
use crate::error::StateResError;
use crate::frames::FrameRepr;
use crate::repr::StateRepr;
use crate::state_res::{self, EventStore, ResolutionEvent};

/// Errors from [`KvStateStore`]: the representation's own errors, plus the same ingestion-level
/// errors [`crate::store::InMemoryStateStore`]'s `StoreError` defines.
#[derive(Debug, Error)]
pub enum KvStoreError<E: std::error::Error + Send + Sync + 'static> {
    /// The representation itself failed (storage I/O, an unknown root).
    #[error(transparent)]
    Repr(E),
    /// [`StateStore::state_at`] was called for an event this store has not ingested.
    #[error("unknown event {0}")]
    UnknownEvent(EventSn),
    /// [`StateStore::resolve`] was called with no forks.
    #[error("resolve() requires at least one fork")]
    EmptyForks,
    /// [`StateStore::resolve`] was called with a different room version than this store was
    /// created for.
    #[error("resolve() called with a different room version than this store was created for")]
    WrongRoomVersion,
    /// The room version is not one this crate's room-version table or `ruma-state-res` supports.
    #[error("unsupported room version: {0}")]
    UnsupportedRoomVersion(String),
    /// State resolution failed.
    #[error(transparent)]
    StateRes(#[from] StateResError),
}

struct CommonInner<Root> {
    state_at: BTreeMap<EventSn, Root>,
    event_id_of: BTreeMap<EventSn, OwnedEventId>,
    sn_of_event_id: BTreeMap<OwnedEventId, EventSn>,
    key_of: BTreeMap<(String, String), StateKeyId>,
    key_strings: BTreeMap<StateKeyId, (String, String)>,
    next_key_id: u32,
    events: EventStore,
    chain_index: ChainCoverIndex,
}

impl<Root> Default for CommonInner<Root> {
    fn default() -> Self {
        Self {
            state_at: BTreeMap::new(),
            event_id_of: BTreeMap::new(),
            sn_of_event_id: BTreeMap::new(),
            key_of: BTreeMap::new(),
            key_strings: BTreeMap::new(),
            next_key_id: 0,
            events: EventStore::default(),
            chain_index: ChainCoverIndex::new(),
        }
    }
}

/// A [`StateStore`] backed by any [`StateRepr`] implementation `R`. See the module docs; the
/// concrete type production callers want is [`ProductionStateStore`].
pub struct KvStateStore<R: StateRepr> {
    room_version: RoomVersionId,
    rules: RoomVersionRules,
    repr: R,
    common: RefCell<CommonInner<R::Root>>,
}

/// The production [`StateStore`]: [`KvStateStore`] instantiated with the bake-off's winning
/// representation (`crate::frames::FrameRepr`) over a caller-chosen `hs_kv::KvBackend`. This is
/// what tracks 04 and 06 should hold one of per room -- see
/// `docs/status/02-state-and-model.md`'s "Interfaces provided" for the call patterns.
pub type ProductionStateStore<KV> = KvStateStore<FrameRepr<KV>>;

impl<R: StateRepr> KvStateStore<R> {
    /// Wraps `repr` (freshly created, empty) as a `StateStore` for a room of `room_version`.
    ///
    /// # Errors
    /// Returns [`KvStoreError::UnsupportedRoomVersion`] if `room_version` is not in
    /// [`hs_model::room_version`]'s table.
    pub fn new(room_version: RoomVersionId, repr: R) -> Result<Self, KvStoreError<R::Error>> {
        let rules = room_version::rules_for(&room_version).ok_or_else(|| {
            KvStoreError::UnsupportedRoomVersion(room_version.as_str().to_owned())
        })?;
        Ok(Self {
            room_version,
            rules,
            repr,
            common: RefCell::new(CommonInner::default()),
        })
    }

    /// The empty state.
    #[must_use]
    pub fn empty_root(&self) -> R::Root {
        self.repr.empty_root()
    }

    /// The interned `(event_type, state_key)` id for `key`, allocating a fresh one if unseen.
    /// Exposed so the corpus generators (which need to build [`StateDiff`]s directly for
    /// throughput reasons on the largest scenarios) can intern the same way `add_event` does.
    pub fn intern(&self, event_type: &str, state_key: &str) -> StateKeyId {
        let mut inner = self.common.borrow_mut();
        Self::intern_key(&mut inner, event_type, state_key)
    }

    fn intern_key(
        inner: &mut CommonInner<R::Root>,
        event_type: &str,
        state_key: &str,
    ) -> StateKeyId {
        let pair = (event_type.to_owned(), state_key.to_owned());
        if let Some(id) = inner.key_of.get(&pair) {
            return *id;
        }
        let id = StateKeyId::new(inner.next_key_id);
        inner.next_key_id += 1;
        inner.key_of.insert(pair.clone(), id);
        inner.key_strings.insert(id, pair);
        id
    }

    /// Ingests one event exactly as [`crate::store::InMemoryStateStore::add_event`] does. See
    /// that method's docs.
    #[allow(clippy::too_many_arguments)]
    pub fn add_event(
        &self,
        event: EventSn,
        event_id: OwnedEventId,
        room_id: OwnedRoomId,
        event_type: &str,
        state_key: Option<&str>,
        sender: OwnedUserId,
        content: CanonicalJsonObject,
        depth: i64,
        origin_server_ts: i64,
        auth_events: &[EventSn],
        prev_events: &[EventSn],
        only_prev_event_is_room_create: bool,
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        let mut inner = self.common.borrow_mut();

        Self::record_event(
            &mut inner,
            event,
            event_id,
            room_id,
            event_type,
            state_key,
            sender,
            content,
            depth,
            origin_server_ts,
            auth_events,
            prev_events,
            only_prev_event_is_room_create,
        );

        let prev_roots: Vec<R::Root> = prev_events
            .iter()
            .filter_map(|sn| inner.state_at.get(sn).copied())
            .collect();
        let state_before = match prev_roots.len() {
            0 => self.repr.empty_root(),
            1 => prev_roots[0],
            _ => self.resolve_locked(&mut inner, &prev_roots)?,
        };

        self.apply_and_index(
            &mut inner,
            event,
            event_type,
            state_key,
            auth_events,
            state_before,
        )
    }

    /// Ingests one event exactly as [`KvStateStore::add_event`] does, except that the state
    /// *before* the event is not derived from its `prev_events` but handed in explicitly:
    /// `state` is the complete list of events, one per `(event_type, state_key)`, that make up
    /// the room's resolved state immediately before `event`. Every entry must already have been
    /// ingested into this store (by either `add_event` method), in any order; its `(event_type,
    /// state_key)` is read back from what was recorded then.
    ///
    /// This is what a room bootstrapped from a federation `send_join` response needs
    /// (`docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`): the joining server holds
    /// the resident's resolved state and the join event, but not the join's `prev_events` (the
    /// resident's forward extremities, ordinarily plain messages the response does not carry),
    /// so there is no `state_at` of any prev event to derive the state before the join from. The
    /// resident's `state` *is* that state, by construction, and this method records it as such:
    /// the root for `event` is the empty state with every entry of `state` applied, then `event`'s
    /// own entry if it is a state event -- exactly the root `add_event` would have produced had
    /// the prev events been ingested and resolved to `state`.
    ///
    /// `prev_events` and `only_prev_event_is_room_create` are still recorded on the event (for
    /// [`crate::state_res`]'s benefit, if a later fork ever resolves through it) but play no part
    /// in computing its state; entries of `prev_events` this store does not know are skipped,
    /// as `add_event` already does for auth events. No state resolution runs.
    ///
    /// # Errors
    /// Returns [`KvStoreError::UnknownEvent`] naming the first entry of `state` this store has
    /// not ingested (nothing is recorded in that case), or [`KvStoreError::Repr`] if the
    /// representation fails to apply the state.
    #[allow(clippy::too_many_arguments)]
    pub fn add_event_with_state(
        &self,
        event: EventSn,
        event_id: OwnedEventId,
        room_id: OwnedRoomId,
        event_type: &str,
        state_key: Option<&str>,
        sender: OwnedUserId,
        content: CanonicalJsonObject,
        depth: i64,
        origin_server_ts: i64,
        auth_events: &[EventSn],
        prev_events: &[EventSn],
        only_prev_event_is_room_create: bool,
        state: &[EventSn],
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        let mut inner = self.common.borrow_mut();

        // Resolve every snapshot entry back to its `(event_type, state_key)` before recording
        // anything, so an unknown entry leaves the store untouched.
        let mut keyed: Vec<(String, String, EventSn)> = Vec::with_capacity(state.len());
        for &sn in state {
            let recorded = inner
                .event_id_of
                .get(&sn)
                .and_then(|id| inner.events.get(id))
                .ok_or(KvStoreError::UnknownEvent(sn))?;
            keyed.push((recorded.event_type.clone(), recorded.state_key.clone(), sn));
        }
        let mut added = BTreeMap::new();
        for (state_event_type, state_state_key, sn) in keyed {
            let key_id = Self::intern_key(&mut inner, &state_event_type, &state_state_key);
            added.insert(key_id, sn);
        }

        Self::record_event(
            &mut inner,
            event,
            event_id,
            room_id,
            event_type,
            state_key,
            sender,
            content,
            depth,
            origin_server_ts,
            auth_events,
            prev_events,
            only_prev_event_is_room_create,
        );

        let state_before = if added.is_empty() {
            self.repr.empty_root()
        } else {
            let diff = StateDiff {
                added,
                removed: std::collections::BTreeSet::new(),
            };
            self.repr
                .apply(self.repr.empty_root(), &diff)
                .map_err(KvStoreError::Repr)?
        };

        self.apply_and_index(
            &mut inner,
            event,
            event_type,
            state_key,
            auth_events,
            state_before,
        )
    }

    /// The first half both `add_event` methods share: interns the event's ID and records its
    /// [`ResolutionEvent`] body, with `auth_events`/`prev_events` translated to the event IDs of
    /// whichever entries this store already knows.
    #[allow(clippy::too_many_arguments)]
    fn record_event(
        inner: &mut CommonInner<R::Root>,
        event: EventSn,
        event_id: OwnedEventId,
        room_id: OwnedRoomId,
        event_type: &str,
        state_key: Option<&str>,
        sender: OwnedUserId,
        content: CanonicalJsonObject,
        depth: i64,
        origin_server_ts: i64,
        auth_events: &[EventSn],
        prev_events: &[EventSn],
        only_prev_event_is_room_create: bool,
    ) {
        inner.sn_of_event_id.insert(event_id.clone(), event);
        inner.event_id_of.insert(event, event_id.clone());

        let auth_event_ids: Vec<OwnedEventId> = auth_events
            .iter()
            .filter_map(|sn| inner.event_id_of.get(sn).cloned())
            .collect();
        let prev_event_ids: Vec<OwnedEventId> = prev_events
            .iter()
            .filter_map(|sn| inner.event_id_of.get(sn).cloned())
            .collect();

        inner.events.insert(
            event_id.clone(),
            ResolutionEvent {
                event_id,
                room_id,
                event_type: event_type.to_owned(),
                state_key: state_key.unwrap_or_default().to_owned(),
                sender,
                content,
                depth,
                origin_server_ts,
                auth_events: auth_event_ids,
                prev_events: prev_event_ids,
                only_prev_event_is_room_create,
            },
        );
    }

    /// The second half both `add_event` methods share: applies the event's own entry (if it is a
    /// state event) on top of `state_before`, records the result as the event's `state_at`, and
    /// adds the event to the chain-cover index.
    fn apply_and_index(
        &self,
        inner: &mut CommonInner<R::Root>,
        event: EventSn,
        event_type: &str,
        state_key: Option<&str>,
        auth_events: &[EventSn],
        state_before: R::Root,
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        let new_root = if let Some(state_key) = state_key {
            let key_id = Self::intern_key(inner, event_type, state_key);
            self.repr
                .apply(state_before, &StateDiff::set(key_id, event))
                .map_err(KvStoreError::Repr)?
        } else {
            state_before
        };

        inner.state_at.insert(event, new_root);

        if let Some(state_key) = state_key {
            let key_id = Self::intern_key(inner, event_type, state_key);
            inner.chain_index.add_event(event, key_id, auth_events);
        }

        Ok(new_root)
    }

    fn resolve_locked(
        &self,
        inner: &mut CommonInner<R::Root>,
        forks: &[R::Root],
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        if forks.iter().all(|r| *r == forks[0]) {
            return Ok(forks[0]);
        }

        let mut state_maps: Vec<state_res::StateMap> = Vec::with_capacity(forks.len());
        for root in forks {
            let full = self.repr.full_state(*root).map_err(KvStoreError::Repr)?;
            let map = full
                .iter()
                .filter_map(|(key, sn)| {
                    let pair = inner.key_strings.get(key)?.clone();
                    let id = inner.event_id_of.get(sn)?.clone();
                    Some((pair, id))
                })
                .collect();
            state_maps.push(map);
        }

        let resolved: state_res::StateMap = match self.rules.state_res {
            StateResolutionVersion::V1 => {
                state_res::v1::resolve(&self.rules, &state_maps, &inner.events)?
            }
            StateResolutionVersion::V2 { .. } => {
                state_res::v2::resolve(&self.room_version, &state_maps, &inner.events)?
            }
        };

        let mut resolved_map = BTreeMap::new();
        for ((event_type, state_key), id) in resolved {
            let Some(&sn) = inner.sn_of_event_id.get(&id) else {
                continue;
            };
            let key_id = Self::intern_key(inner, &event_type, &state_key);
            resolved_map.insert(key_id, sn);
        }

        // Base the result on one fork (the first) rather than rebuilding from the empty state:
        // a representation with structural sharing (candidates B and C) only benefits from that
        // sharing if resolution's *write* is expressed as a diff against something it already
        // has, not as "every key, from scratch," which would defeat the whole point of measuring
        // structural sharing under "resolution time on forks."
        let base = forks[0];
        let base_map = self.repr.full_state(base).map_err(KvStoreError::Repr)?;
        let mut added = BTreeMap::new();
        for (key, sn) in &resolved_map {
            if base_map.get(key) != Some(sn) {
                added.insert(*key, *sn);
            }
        }
        let removed = base_map
            .keys()
            .filter(|key| !resolved_map.contains_key(key))
            .copied()
            .collect();
        let diff = StateDiff { added, removed };
        self.repr.apply(base, &diff).map_err(KvStoreError::Repr)
    }
}

impl<KV: KvBackend> KvStateStore<FrameRepr<KV>> {
    /// Opens the production state store for a room of `room_version` over `backend`: the
    /// convenience constructor tracks 04 and 06 should use instead of building a `FrameRepr` and
    /// wrapping it by hand.
    ///
    /// # Errors
    /// Returns [`KvStoreError::UnsupportedRoomVersion`] if `room_version` is not in
    /// [`hs_model::room_version`]'s table, or [`KvStoreError::Repr`] if `backend` could not open
    /// the frames keyspace.
    pub fn open(
        room_version: RoomVersionId,
        backend: KV,
    ) -> Result<Self, KvStoreError<crate::frames::Error>> {
        let repr = FrameRepr::new(backend).map_err(KvStoreError::Repr)?;
        Self::new(room_version, repr)
    }
}

impl<R: StateRepr> StateStore for KvStateStore<R> {
    type Root = R::Root;
    type Error = KvStoreError<R::Error>;

    fn intern_state_key(
        &self,
        event_type: &str,
        state_key: &str,
    ) -> Result<StateKeyId, Self::Error> {
        Ok(self.intern(event_type, state_key))
    }

    fn state_at(&self, event: EventSn) -> Result<R::Root, Self::Error> {
        self.common
            .borrow()
            .state_at
            .get(&event)
            .copied()
            .ok_or(KvStoreError::UnknownEvent(event))
    }

    fn get(&self, root: R::Root, key: StateKeyId) -> Result<Option<EventSn>, Self::Error> {
        self.repr.get(root, key).map_err(KvStoreError::Repr)
    }

    fn diff(&self, from: R::Root, to: R::Root) -> Result<StateDiff, Self::Error> {
        self.repr.diff(from, to).map_err(KvStoreError::Repr)
    }

    fn apply(&self, root: R::Root, changes: &StateDiff) -> Result<R::Root, Self::Error> {
        self.repr.apply(root, changes).map_err(KvStoreError::Repr)
    }

    fn resolve(
        &self,
        room_version: &RoomVersionId,
        forks: &[R::Root],
    ) -> Result<R::Root, Self::Error> {
        if forks.is_empty() {
            return Err(KvStoreError::EmptyForks);
        }
        if *room_version != self.room_version {
            return Err(KvStoreError::WrongRoomVersion);
        }
        let mut inner = self.common.borrow_mut();
        self.resolve_locked(&mut inner, forks)
    }

    fn chain_position(&self, event: EventSn) -> Result<Option<ChainPosition>, Self::Error> {
        Ok(self.common.borrow().chain_index.position(event))
    }

    fn auth_chain_contains(
        &self,
        event: EventSn,
        ancestor: EventSn,
    ) -> Result<Option<bool>, Self::Error> {
        Ok(self.common.borrow().chain_index.contains(event, ancestor))
    }

    fn auth_chain_difference(&self, sets: &[Vec<EventSn>]) -> Result<Vec<EventSn>, Self::Error> {
        Ok(self.common.borrow().chain_index.auth_chain_difference(sets))
    }
}

/// The same fork-and-merge scenario `crate::store::InMemoryStateStore`'s tests exercise, run
/// through `KvStateStore` for every bake-off candidate, over `hs_kv::memory::MemoryBackend`. This
/// is the correctness gate the bake-off's numbers depend on: a candidate that is fast but resolves
/// forks incorrectly would invalidate every other measurement, so this runs the full
/// ingest-then-resolve path (not just `StateRepr::get`/`diff`/`apply` in isolation, which each
/// candidate's own module already covers) before any candidate is trusted with real corpus data.
#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use hs_model::canonical::to_canonical_object;
    use ruma::{EventId, RoomId, UserId};
    use serde_json::json;

    use super::*;
    use crate::bakeoff::{PersistentMapRepr, SnapshotDeltaRepr};
    use crate::frames::FrameRepr;

    fn obj(v: serde_json::Value) -> CanonicalJsonObject {
        to_canonical_object(&v, true).unwrap()
    }

    /// Builds `create -> join -> power_levels -> (topic "a" | topic "b") -> merge` and asserts
    /// the merge event's resolved state carries the higher-depth topic, exactly like
    /// `crate::store::InMemoryStateStore`'s `fork_and_merge_resolves_through_the_trait`.
    fn fork_and_merge_resolves<R: StateRepr>(store: KvStateStore<R>) {
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = UserId::parse("@c:hs1").unwrap();

        store
            .add_event(
                EventSn::new(1),
                EventId::parse("$1:hs1").unwrap(),
                room_id.clone(),
                "m.room.create",
                Some(""),
                creator.clone(),
                obj(json!({"creator": creator.as_str()})),
                1,
                1,
                &[],
                &[],
                false,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(2),
                EventId::parse("$2:hs1").unwrap(),
                room_id.clone(),
                "m.room.member",
                Some(creator.as_str()),
                creator.clone(),
                obj(json!({"membership": "join"})),
                2,
                2,
                &[EventSn::new(1)],
                &[EventSn::new(1)],
                true,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(3),
                EventId::parse("$3:hs1").unwrap(),
                room_id.clone(),
                "m.room.power_levels",
                Some(""),
                creator.clone(),
                obj(json!({
                    "users": {creator.as_str(): 100},
                    "ban": 50, "kick": 50, "redact": 50, "invite": 0,
                    "users_default": 0, "events_default": 0, "state_default": 50,
                })),
                3,
                3,
                &[EventSn::new(1), EventSn::new(2)],
                &[EventSn::new(2)],
                false,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(4),
                EventId::parse("$4a:hs1").unwrap(),
                room_id.clone(),
                "m.room.topic",
                Some(""),
                creator.clone(),
                obj(json!({"topic": "a"})),
                4,
                4,
                &[EventSn::new(3), EventSn::new(2)],
                &[EventSn::new(3)],
                false,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(5),
                EventId::parse("$4b:hs1").unwrap(),
                room_id.clone(),
                "m.room.topic",
                Some(""),
                creator.clone(),
                obj(json!({"topic": "b"})),
                5,
                5,
                &[EventSn::new(3), EventSn::new(2)],
                &[EventSn::new(3)],
                false,
            )
            .unwrap();

        let merged_root = store
            .add_event(
                EventSn::new(6),
                EventId::parse("$6:hs1").unwrap(),
                room_id,
                "m.room.message",
                None,
                creator,
                obj(json!({"body": "merged"})),
                6,
                6,
                &[EventSn::new(3), EventSn::new(2)],
                &[EventSn::new(4), EventSn::new(5)],
                false,
            )
            .unwrap();

        let topic_key = store.intern("m.room.topic", "");
        let winner = store.get(merged_root, topic_key).unwrap();
        assert_eq!(
            winner,
            Some(EventSn::new(5)),
            "the higher-depth candidate (branch B) must win the topic conflict"
        );

        assert!(store.chain_position(EventSn::new(3)).unwrap().is_some());
        assert_eq!(
            store
                .auth_chain_contains(EventSn::new(4), EventSn::new(1))
                .unwrap(),
            Some(true)
        );
    }

    #[test]
    fn candidate_a_snapshot_delta() {
        let repr = SnapshotDeltaRepr::new(MemoryBackend::default()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr).unwrap();
        fork_and_merge_resolves(store);
    }

    #[test]
    fn candidate_b_frames() {
        let repr = FrameRepr::new(MemoryBackend::default()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr).unwrap();
        fork_and_merge_resolves(store);
    }

    #[test]
    fn candidate_c_persistent_map() {
        let repr = PersistentMapRepr::new(MemoryBackend::default()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr).unwrap();
        fork_and_merge_resolves(store);
    }

    /// `add_event_with_state`: the shape a room bootstrapped from a `send_join` response has.
    /// Three snapshot events are ingested as outliers -- with no `prev_events` at all, so their
    /// own `state_at` is deliberately meaningless -- and a join whose prev events the store has
    /// never seen is then ingested with the snapshot as its explicit state. Its `state_at` must
    /// be exactly the snapshot plus itself, its chain-cover position must reach the snapshot's
    /// ancestors, and naming an unknown snapshot entry must be an error that records nothing.
    #[test]
    fn add_event_with_state_seeds_the_state_from_an_explicit_snapshot() {
        let repr = FrameRepr::new(MemoryBackend::default()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr).unwrap();
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = UserId::parse("@c:hs1").unwrap();
        let joiner = UserId::parse("@j:hs2").unwrap();

        // The snapshot, as outliers: create, the creator's join, power levels. No prev events.
        store
            .add_event(
                EventSn::new(1),
                EventId::parse("$1:hs1").unwrap(),
                room_id.clone(),
                "m.room.create",
                Some(""),
                creator.clone(),
                obj(json!({"creator": creator.as_str()})),
                1,
                1,
                &[],
                &[],
                false,
            )
            .unwrap();
        store
            .add_event(
                EventSn::new(2),
                EventId::parse("$2:hs1").unwrap(),
                room_id.clone(),
                "m.room.member",
                Some(creator.as_str()),
                creator.clone(),
                obj(json!({"membership": "join"})),
                2,
                2,
                &[EventSn::new(1)],
                &[],
                true,
            )
            .unwrap();
        store
            .add_event(
                EventSn::new(3),
                EventId::parse("$3:hs1").unwrap(),
                room_id.clone(),
                "m.room.power_levels",
                Some(""),
                creator.clone(),
                obj(json!({"users": {creator.as_str(): 100}})),
                3,
                3,
                &[EventSn::new(1), EventSn::new(2)],
                &[],
                false,
            )
            .unwrap();

        // An unknown snapshot entry is refused before anything is recorded.
        let err = store
            .add_event_with_state(
                EventSn::new(4),
                EventId::parse("$4:hs2").unwrap(),
                room_id.clone(),
                "m.room.member",
                Some(joiner.as_str()),
                joiner.clone(),
                obj(json!({"membership": "join"})),
                50,
                50,
                &[EventSn::new(1), EventSn::new(3)],
                &[EventSn::new(999)],
                false,
                &[
                    EventSn::new(1),
                    EventSn::new(2),
                    EventSn::new(3),
                    EventSn::new(42),
                ],
            )
            .unwrap_err();
        assert!(matches!(err, KvStoreError::UnknownEvent(sn) if sn == EventSn::new(42)));
        assert!(matches!(
            store.state_at(EventSn::new(4)),
            Err(KvStoreError::UnknownEvent(_))
        ));

        // The join, with the snapshot as its explicit state; its prev event ($999) is unknown.
        let root = store
            .add_event_with_state(
                EventSn::new(4),
                EventId::parse("$4:hs2").unwrap(),
                room_id,
                "m.room.member",
                Some(joiner.as_str()),
                joiner.clone(),
                obj(json!({"membership": "join"})),
                50,
                50,
                &[EventSn::new(1), EventSn::new(3)],
                &[EventSn::new(999)],
                false,
                &[EventSn::new(1), EventSn::new(2), EventSn::new(3)],
            )
            .unwrap();
        assert_eq!(store.state_at(EventSn::new(4)).unwrap(), root);

        let full = store.diff(store.empty_root(), root).unwrap();
        let mut sns: Vec<EventSn> = full.added.values().copied().collect();
        sns.sort();
        assert_eq!(
            sns,
            vec![
                EventSn::new(1),
                EventSn::new(2),
                EventSn::new(3),
                EventSn::new(4)
            ],
            "state_at(join) must be exactly the snapshot plus the join itself"
        );
        assert_eq!(
            store
                .get(root, store.intern("m.room.member", joiner.as_str()))
                .unwrap(),
            Some(EventSn::new(4))
        );
        assert_eq!(
            store
                .auth_chain_contains(EventSn::new(4), EventSn::new(1))
                .unwrap(),
            Some(true),
            "the join's chain-cover position must reach the create event through power levels"
        );
    }
}
