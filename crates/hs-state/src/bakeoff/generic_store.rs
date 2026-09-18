//! [`GenericStore`]: a full [`crate::api::StateStore`] for any [`super::repr::StateRepr`].
//!
//! This is [`crate::store::InMemoryStateStore`]'s ingestion and resolution logic, unchanged in
//! substance, made generic over the state representation so all three bake-off candidates share
//! one tested implementation of "ingest an event, resolve its predecessor state, run state
//! resolution on a fork" and differ only in `R: StateRepr`.

use std::cell::RefCell;
use std::collections::BTreeMap;

use hs_model::canonical::CanonicalJsonObject;
use hs_model::ids::{EventSn, StateKeyId};
use hs_model::room_version::{self, RoomVersionRules, StateResolutionVersion};
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId};
use thiserror::Error;

use super::repr::StateRepr;
use crate::api::{StateDiff, StateStore};
use crate::chain_cover::{ChainCoverIndex, ChainPosition};
use crate::error::StateResError;
use crate::state_res::{self, EventStore, ResolutionEvent};

/// Errors from [`GenericStore`]: the representation's own errors, plus the same ingestion-level
/// errors [`crate::store::InMemoryStateStore`]'s `StoreError` defines.
#[derive(Debug, Error)]
pub enum BakeoffError<E: std::error::Error + Send + Sync + 'static> {
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

/// A [`StateStore`] for any bake-off candidate `R`.
pub struct GenericStore<R: StateRepr> {
    room_version: RoomVersionId,
    rules: RoomVersionRules,
    repr: R,
    common: RefCell<CommonInner<R::Root>>,
}

impl<R: StateRepr> GenericStore<R> {
    /// Wraps `repr` (freshly created, empty) as a `StateStore` for a room of `room_version`.
    ///
    /// # Errors
    /// Returns [`BakeoffError::UnsupportedRoomVersion`] if `room_version` is not in
    /// [`hs_model::room_version`]'s table.
    pub fn new(room_version: RoomVersionId, repr: R) -> Result<Self, BakeoffError<R::Error>> {
        let rules = room_version::rules_for(&room_version).ok_or_else(|| {
            BakeoffError::UnsupportedRoomVersion(room_version.as_str().to_owned())
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
    ) -> Result<R::Root, BakeoffError<R::Error>> {
        let mut inner = self.common.borrow_mut();

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
                event_id: event_id.clone(),
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

        let prev_roots: Vec<R::Root> = prev_events
            .iter()
            .filter_map(|sn| inner.state_at.get(sn).copied())
            .collect();
        let state_before = match prev_roots.len() {
            0 => self.repr.empty_root(),
            1 => prev_roots[0],
            _ => self.resolve_locked(&mut inner, &prev_roots)?,
        };

        let new_root = if let Some(state_key) = state_key {
            let key_id = Self::intern_key(&mut inner, event_type, state_key);
            self.repr
                .apply(state_before, &StateDiff::set(key_id, event))
                .map_err(BakeoffError::Repr)?
        } else {
            state_before
        };

        inner.state_at.insert(event, new_root);

        if let Some(state_key) = state_key {
            let key_id = Self::intern_key(&mut inner, event_type, state_key);
            inner.chain_index.add_event(event, key_id, auth_events);
        }

        Ok(new_root)
    }

    fn resolve_locked(
        &self,
        inner: &mut CommonInner<R::Root>,
        forks: &[R::Root],
    ) -> Result<R::Root, BakeoffError<R::Error>> {
        if forks.iter().all(|r| *r == forks[0]) {
            return Ok(forks[0]);
        }

        let mut state_maps: Vec<state_res::StateMap> = Vec::with_capacity(forks.len());
        for root in forks {
            let full = self.repr.full_state(*root).map_err(BakeoffError::Repr)?;
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
        let base_map = self.repr.full_state(base).map_err(BakeoffError::Repr)?;
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
        self.repr.apply(base, &diff).map_err(BakeoffError::Repr)
    }
}

impl<R: StateRepr> StateStore for GenericStore<R> {
    type Root = R::Root;
    type Error = BakeoffError<R::Error>;

    fn state_at(&self, event: EventSn) -> Result<R::Root, Self::Error> {
        self.common
            .borrow()
            .state_at
            .get(&event)
            .copied()
            .ok_or(BakeoffError::UnknownEvent(event))
    }

    fn get(&self, root: R::Root, key: StateKeyId) -> Result<Option<EventSn>, Self::Error> {
        self.repr.get(root, key).map_err(BakeoffError::Repr)
    }

    fn diff(&self, from: R::Root, to: R::Root) -> Result<StateDiff, Self::Error> {
        self.repr.diff(from, to).map_err(BakeoffError::Repr)
    }

    fn apply(&self, root: R::Root, changes: &StateDiff) -> Result<R::Root, Self::Error> {
        self.repr.apply(root, changes).map_err(BakeoffError::Repr)
    }

    fn resolve(
        &self,
        room_version: &RoomVersionId,
        forks: &[R::Root],
    ) -> Result<R::Root, Self::Error> {
        if forks.is_empty() {
            return Err(BakeoffError::EmptyForks);
        }
        if *room_version != self.room_version {
            return Err(BakeoffError::WrongRoomVersion);
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
/// through `GenericStore` for every bake-off candidate, over `hs_kv::memory::MemoryBackend`. This
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
    use crate::bakeoff::{FrameRepr, PersistentMapRepr, SnapshotDeltaRepr};

    fn obj(v: serde_json::Value) -> CanonicalJsonObject {
        to_canonical_object(&v, true).unwrap()
    }

    /// Builds `create -> join -> power_levels -> (topic "a" | topic "b") -> merge` and asserts
    /// the merge event's resolved state carries the higher-depth topic, exactly like
    /// `crate::store::InMemoryStateStore`'s `fork_and_merge_resolves_through_the_trait`.
    fn fork_and_merge_resolves<R: StateRepr>(store: GenericStore<R>) {
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
        let store = GenericStore::new(RoomVersionId::V11, repr).unwrap();
        fork_and_merge_resolves(store);
    }

    #[test]
    fn candidate_b_frames() {
        let repr = FrameRepr::new(MemoryBackend::default()).unwrap();
        let store = GenericStore::new(RoomVersionId::V11, repr).unwrap();
        fork_and_merge_resolves(store);
    }

    #[test]
    fn candidate_c_persistent_map() {
        let repr = PersistentMapRepr::new(MemoryBackend::default()).unwrap();
        let store = GenericStore::new(RoomVersionId::V11, repr).unwrap();
        fork_and_merge_resolves(store);
    }
}
