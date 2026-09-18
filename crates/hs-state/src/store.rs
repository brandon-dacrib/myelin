//! [`InMemoryStateStore`]: a reference [`StateStore`] implementation.
//!
//! This is *not* one of the three candidate representations `PLAN.md` section 6.3's bake-off
//! evaluates (it is a plain `Vec` of maps with no structural sharing, dedication to write
//! amplification, or persistence story); it exists to prove [`crate::api::StateStore`] is usable
//! end to end -- ingest events, resolve forks, diff and apply, answer chain-cover queries -- and to
//! give tracks 04 and 06 something real to build against before the bake-off lands. If budget
//! allows before Phase 0 closes, the bake-off candidates replace this module's internals behind
//! the same trait, per `docs/workstreams/02-state-and-model.md`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use hs_model::canonical::CanonicalJsonObject;
use hs_model::ids::{EventSn, StateKeyId};
use hs_model::room_version::{self, RoomVersionRules, StateResolutionVersion};
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId};
use thiserror::Error;

use crate::api::{StateDiff, StateStore};
use crate::chain_cover::{ChainCoverIndex, ChainPosition};
use crate::error::StateResError;
use crate::state_res::{self, EventStore, ResolutionEvent};

/// An opaque state handle for [`InMemoryStateStore`]: an index into its internal table of
/// resolved state snapshots. Callers must treat this as opaque (see [`StateStore::Root`]'s docs);
/// the `usize` is not stable across stores and carries no meaning on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Root(usize);

/// Errors from [`InMemoryStateStore`].
#[derive(Debug, Error)]
pub enum StoreError {
    /// [`StateStore::state_at`] was called for an event this store has not ingested.
    #[error("unknown event {0}")]
    UnknownEvent(EventSn),
    /// A [`Root`] was not produced by this store (or is stale after the store was reset, which
    /// this implementation never does, but a future persistent implementation might).
    #[error("state root not known to this store")]
    UnknownRoot,
    /// [`StateStore::resolve`] was called with no forks.
    #[error("resolve() requires at least one fork")]
    EmptyForks,
    /// [`StateStore::resolve`] was called with a room version other than the one this store was
    /// constructed for.
    #[error("resolve() called with a different room version than this store was created for")]
    WrongRoomVersion,
    /// The room version is not one this crate's room-version table or `ruma-state-res` supports.
    #[error("unsupported room version: {0}")]
    UnsupportedRoomVersion(String),
    /// State resolution failed.
    #[error(transparent)]
    StateRes(#[from] StateResError),
}

#[derive(Default)]
struct Inner {
    /// Every resolved state snapshot ever produced; `Root(i)` indexes here. `Root(0)` is always
    /// the empty state.
    roots: Vec<BTreeMap<StateKeyId, EventSn>>,
    /// `state_at(event)` (*S′(event)*).
    state_at: BTreeMap<EventSn, Root>,
    /// Bridges this store's `EventSn`s to the `OwnedEventId`s `state_res` and `ruma-state-res`
    /// need. A real implementation gets this from `hs-tables`' interning API; this reference
    /// implementation interns it locally.
    event_id_of: BTreeMap<EventSn, OwnedEventId>,
    sn_of_event_id: BTreeMap<OwnedEventId, EventSn>,
    /// Bridges `StateKeyId` to `(event_type, state_key)`, likewise interned locally here.
    key_of: BTreeMap<(String, String), StateKeyId>,
    key_strings: BTreeMap<StateKeyId, (String, String)>,
    next_key_id: u32,
    /// Full event bodies, for the state resolution algorithms.
    events: EventStore,
    /// The room's chain-cover index.
    chain_index: ChainCoverIndex,
}

/// A reference, in-memory [`StateStore`] for one room.
pub struct InMemoryStateStore {
    room_version: RoomVersionId,
    rules: RoomVersionRules,
    inner: RefCell<Inner>,
}

impl InMemoryStateStore {
    /// Creates an empty store for a room of the given version.
    ///
    /// # Errors
    /// Returns [`StoreError::UnsupportedRoomVersion`] if `room_version` is not in
    /// [`hs_model::room_version`]'s table.
    pub fn new(room_version: RoomVersionId) -> Result<Self, StoreError> {
        let rules = room_version::rules_for(&room_version)
            .ok_or_else(|| StoreError::UnsupportedRoomVersion(room_version.as_str().to_owned()))?;
        let mut inner = Inner::default();
        inner.roots.push(BTreeMap::new()); // Root(0): the empty state.
        Ok(Self {
            room_version,
            rules,
            inner: RefCell::new(inner),
        })
    }

    /// The empty state (no keys set): the state before a room's `m.room.create` event.
    #[must_use]
    pub fn empty_root(&self) -> Root {
        Root(0)
    }

    /// Ingests one event: records its body, resolves the state before it (from its
    /// `prev_events`' [`StateStore::state_at`], via [`StateStore::resolve`] if there is more than
    /// one), applies its own state change if it is a state event, and -- if it has a
    /// `(event_type, state_key)` -- adds it to the chain-cover index.
    ///
    /// `auth_events` and `prev_events` are this store's own `EventSn`s of events already ingested;
    /// an entry not yet known to this store is silently skipped, matching
    /// [`ChainCoverIndex::add_event`]'s handling of an auth chain that reaches further back than
    /// what has been loaded.
    ///
    /// Returns `state_at(event)`, i.e. the same value a subsequent
    /// [`StateStore::state_at`] call would return.
    ///
    /// # Errors
    /// Returns [`StoreError::StateRes`] if resolving multiple `prev_events` fails.
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
    ) -> Result<Root, StoreError> {
        let mut inner = self.inner.borrow_mut();

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

        let prev_roots: Vec<Root> = prev_events
            .iter()
            .filter_map(|sn| inner.state_at.get(sn).copied())
            .collect();
        let state_before = match prev_roots.len() {
            0 => Root(0),
            1 => prev_roots[0],
            _ => self.resolve_locked(&mut inner, &prev_roots)?,
        };

        let new_root = if let Some(state_key) = state_key {
            let key_id = self.intern_key(&mut inner, event_type, state_key);
            self.apply_locked(&mut inner, state_before, &StateDiff::set(key_id, event))
        } else {
            state_before
        };

        inner.state_at.insert(event, new_root);

        if let Some(state_key) = state_key {
            let key_id = self.intern_key(&mut inner, event_type, state_key);
            inner.chain_index.add_event(event, key_id, auth_events);
        }

        Ok(new_root)
    }

    fn intern_key(&self, inner: &mut Inner, event_type: &str, state_key: &str) -> StateKeyId {
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

    fn apply_locked(&self, inner: &mut Inner, root: Root, changes: &StateDiff) -> Root {
        let mut map = inner.roots[root.0].clone();
        for key in &changes.removed {
            map.remove(key);
        }
        for (key, event) in &changes.added {
            map.insert(*key, *event);
        }
        let new_root = Root(inner.roots.len());
        inner.roots.push(map);
        new_root
    }

    fn resolve_locked(&self, inner: &mut Inner, forks: &[Root]) -> Result<Root, StoreError> {
        if forks.iter().all(|r| *r == forks[0]) {
            return Ok(forks[0]);
        }

        let state_maps: Vec<state_res::StateMap> = forks
            .iter()
            .map(|root| {
                inner.roots[root.0]
                    .iter()
                    .filter_map(|(key, sn)| {
                        let pair = inner.key_strings.get(key)?.clone();
                        let id = inner.event_id_of.get(sn)?.clone();
                        Some((pair, id))
                    })
                    .collect()
            })
            .collect();

        let resolved: state_res::StateMap = match self.rules.state_res {
            StateResolutionVersion::V1 => {
                state_res::v1::resolve(&self.rules, &state_maps, &inner.events)?
            }
            StateResolutionVersion::V2 { .. } => {
                state_res::v2::resolve(&self.room_version, &state_maps, &inner.events)?
            }
        };

        let mut map = BTreeMap::new();
        for ((event_type, state_key), id) in resolved {
            let Some(&sn) = inner.sn_of_event_id.get(&id) else {
                continue;
            };
            let key_id = self.intern_key(inner, &event_type, &state_key);
            map.insert(key_id, sn);
        }

        let new_root = Root(inner.roots.len());
        inner.roots.push(map);
        Ok(new_root)
    }
}

impl StateStore for InMemoryStateStore {
    type Root = Root;
    type Error = StoreError;

    fn state_at(&self, event: EventSn) -> Result<Root, StoreError> {
        self.inner
            .borrow()
            .state_at
            .get(&event)
            .copied()
            .ok_or(StoreError::UnknownEvent(event))
    }

    fn get(&self, root: Root, key: StateKeyId) -> Result<Option<EventSn>, StoreError> {
        let inner = self.inner.borrow();
        let map = inner.roots.get(root.0).ok_or(StoreError::UnknownRoot)?;
        Ok(map.get(&key).copied())
    }

    fn diff(&self, from: Root, to: Root) -> Result<StateDiff, StoreError> {
        let inner = self.inner.borrow();
        let a = inner.roots.get(from.0).ok_or(StoreError::UnknownRoot)?;
        let b = inner.roots.get(to.0).ok_or(StoreError::UnknownRoot)?;

        let mut added = BTreeMap::new();
        for (key, event) in b {
            if a.get(key) != Some(event) {
                added.insert(*key, *event);
            }
        }
        let removed: BTreeSet<StateKeyId> = a
            .keys()
            .filter(|key| !b.contains_key(key))
            .copied()
            .collect();

        Ok(StateDiff { added, removed })
    }

    fn apply(&self, root: Root, changes: &StateDiff) -> Result<Root, StoreError> {
        let mut inner = self.inner.borrow_mut();
        if !inner.roots.indices().contains(&root.0) {
            return Err(StoreError::UnknownRoot);
        }
        Ok(self.apply_locked(&mut inner, root, changes))
    }

    fn resolve(&self, room_version: &RoomVersionId, forks: &[Root]) -> Result<Root, StoreError> {
        if forks.is_empty() {
            return Err(StoreError::EmptyForks);
        }
        if *room_version != self.room_version {
            return Err(StoreError::WrongRoomVersion);
        }
        let mut inner = self.inner.borrow_mut();
        for root in forks {
            if root.0 >= inner.roots.len() {
                return Err(StoreError::UnknownRoot);
            }
        }
        self.resolve_locked(&mut inner, forks)
    }

    fn chain_position(&self, event: EventSn) -> Result<Option<ChainPosition>, StoreError> {
        Ok(self.inner.borrow().chain_index.position(event))
    }

    fn auth_chain_contains(
        &self,
        event: EventSn,
        ancestor: EventSn,
    ) -> Result<Option<bool>, StoreError> {
        Ok(self.inner.borrow().chain_index.contains(event, ancestor))
    }

    fn auth_chain_difference(&self, sets: &[Vec<EventSn>]) -> Result<Vec<EventSn>, StoreError> {
        Ok(self.inner.borrow().chain_index.auth_chain_difference(sets))
    }
}

/// Small helper trait to make `apply`'s bounds check read naturally; kept private in spirit
/// (module-private via not being re-exported from `lib.rs`) but must be `pub` for the inherent
/// method's bound to compile.
trait IndicesExt {
    fn indices(&self) -> std::ops::Range<usize>;
}
impl<T> IndicesExt for Vec<T> {
    fn indices(&self) -> std::ops::Range<usize> {
        0..self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_model::canonical::to_canonical_object;
    use ruma::{EventId, RoomId, UserId};
    use serde_json::json;

    fn store() -> InMemoryStateStore {
        InMemoryStateStore::new(RoomVersionId::V11).unwrap()
    }

    fn obj(v: serde_json::Value) -> CanonicalJsonObject {
        to_canonical_object(&v, true).unwrap()
    }

    #[test]
    fn linear_history_state_at_accumulates() {
        let s = store();
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = UserId::parse("@c:hs1").unwrap();

        let create_id = EventId::parse("$1:hs1").unwrap();
        let r0 = s
            .add_event(
                EventSn::new(1),
                create_id,
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
        assert!(
            s.get(r0, hs_model::ids::StateKeyId::new(0))
                .unwrap()
                .is_some()
        );

        let join_id = EventId::parse("$2:hs1").unwrap();
        let r1 = s
            .add_event(
                EventSn::new(2),
                join_id,
                room_id,
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

        assert_ne!(r0, r1);
        assert_eq!(s.state_at(EventSn::new(2)).unwrap(), r1);

        let diff = s.diff(r0, r1).unwrap();
        assert_eq!(diff.added.len(), 1);
        assert!(diff.removed.is_empty());

        let applied = s.apply(r0, &diff).unwrap();
        assert_eq!(s.diff(applied, r1).unwrap(), StateDiff::default());
    }

    #[test]
    fn resolve_single_fork_is_identity() {
        let s = store();
        let root = s.empty_root();
        let resolved = s.resolve(&RoomVersionId::V11, &[root]).unwrap();
        assert_eq!(resolved, root);
    }

    #[test]
    fn resolve_rejects_empty_forks_and_wrong_version() {
        let s = store();
        assert!(matches!(
            s.resolve(&RoomVersionId::V11, &[]),
            Err(StoreError::EmptyForks)
        ));
        assert!(matches!(
            s.resolve(&RoomVersionId::V6, &[s.empty_root()]),
            Err(StoreError::WrongRoomVersion)
        ));
    }

    #[test]
    fn unknown_event_and_root_are_errors() {
        let s = store();
        assert!(matches!(
            s.state_at(EventSn::new(999)),
            Err(StoreError::UnknownEvent(_))
        ));
        assert!(matches!(
            s.get(Root(999), StateKeyId::new(0)),
            Err(StoreError::UnknownRoot)
        ));
    }

    /// A genuine fork: two branches each change the room topic; a merge event citing both
    /// `prev_events` triggers `resolve()` internally (through `add_event`), and the winner is
    /// authorized (both senders are the joined creator, so the higher-depth one wins, matching
    /// `state_res::v1`/`v2`'s "otherwise, highest depth" tie-break). The chain-cover queries are
    /// exercised through the trait too.
    #[test]
    fn fork_and_merge_resolves_through_the_trait() {
        let s = store();
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = UserId::parse("@c:hs1").unwrap();

        let create_id = EventId::parse("$1:hs1").unwrap();
        s.add_event(
            EventSn::new(1),
            create_id,
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

        let join_id = EventId::parse("$2:hs1").unwrap();
        s.add_event(
            EventSn::new(2),
            join_id,
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

        let pl_id = EventId::parse("$3:hs1").unwrap();
        s.add_event(
            EventSn::new(3),
            pl_id,
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

        // Branch A: topic "a" at depth 4.
        let topic_a_id = EventId::parse("$4a:hs1").unwrap();
        s.add_event(
            EventSn::new(4),
            topic_a_id,
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

        // Branch B: topic "b" at depth 5 (from the same power_levels event, a sibling fork).
        let topic_b_id = EventId::parse("$4b:hs1").unwrap();
        s.add_event(
            EventSn::new(5),
            topic_b_id,
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

        // Merge: a message citing both forks as prev_events, forcing resolve().
        let merge_id = EventId::parse("$6:hs1").unwrap();
        let merged_root = s
            .add_event(
                EventSn::new(6),
                merge_id,
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

        let topic_key = hs_model::ids::StateKeyId::new(3); // interned 4th distinct key: create(0), member(1), power_levels(2), topic(3)
        let winner = s.get(merged_root, topic_key).unwrap();
        assert!(
            winner.is_some(),
            "the topic conflict must resolve to *some* winner"
        );
        // The higher-depth candidate (branch B, EventSn(5)) should win under either state_res
        // algorithm's "otherwise, highest depth passing auth" tie-break.
        assert_eq!(winner, Some(EventSn::new(5)));

        // Chain-cover queries, through the trait.
        assert!(s.chain_position(EventSn::new(3)).unwrap().is_some());
        assert_eq!(
            s.auth_chain_contains(EventSn::new(4), EventSn::new(1))
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            s.auth_chain_contains(EventSn::new(4), EventSn::new(5))
                .unwrap(),
            Some(false)
        );
    }
}
