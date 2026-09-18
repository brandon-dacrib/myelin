//! [`StateFetch`]: the narrow view of "the room's state" event authorization needs.
//!
//! Event auth never needs the whole resolved state map; it needs to look up at most a handful of
//! entries by `(event_type, state_key)` -- `m.room.create`, `m.room.power_levels`,
//! `m.room.join_rules`, one or two `m.room.member` entries, occasionally an
//! `m.room.third_party_invite`. [`StateFetch`] is that lookup, kept abstract so [`crate::auth`]
//! never has to know whether the caller is querying a fully materialized state map (`hs-room`, via
//! [`FlatState`]), a state snapshot behind `hs-state`'s own API, or -- in tests -- a handful of
//! events built by hand or by a generator.

use hs_model::canonical::CanonicalJsonObject;
use hs_model::ids::EventSn;
use ruma::{OwnedUserId, UserId};
use std::collections::BTreeMap;

use crate::api::StateStore;

/// One looked-up state event: enough of it for auth checks, without committing to a full event
/// type.
#[derive(Debug, Clone, Copy)]
pub struct StateEntry<'a> {
    /// The event's sender.
    pub sender: &'a UserId,
    /// The event's `content`.
    pub content: &'a CanonicalJsonObject,
}

/// Looks up a state event by `(event_type, state_key)`.
pub trait StateFetch {
    /// Returns the current state event for `(event_type, state_key)`, if any.
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>>;

    /// Convenience: `m.room.create`.
    fn create(&self) -> Option<StateEntry<'_>> {
        self.get("m.room.create", "")
    }

    /// Convenience: `m.room.power_levels`.
    fn power_levels(&self) -> Option<StateEntry<'_>> {
        self.get("m.room.power_levels", "")
    }

    /// Convenience: `m.room.join_rules`.
    fn join_rules(&self) -> Option<StateEntry<'_>> {
        self.get("m.room.join_rules", "")
    }

    /// Convenience: a user's `m.room.member` entry.
    fn member(&self, user: &UserId) -> Option<StateEntry<'_>> {
        self.get("m.room.member", user.as_str())
    }

    /// Convenience: an `m.room.third_party_invite` entry by its token.
    fn third_party_invite(&self, token: &str) -> Option<StateEntry<'_>> {
        self.get("m.room.third_party_invite", token)
    }
}

impl<T: StateFetch + ?Sized> StateFetch for &T {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>> {
        (**self).get(event_type, state_key)
    }
}

/// A simple, fully materialized `StateFetch`: a `(type, state_key) -> (sender, content)` map.
///
/// This is what test fixtures and the property-test generators in this crate build directly; the
/// production room actor (`hs-room`) is expected to implement [`StateFetch`] itself over whatever
/// representation the bake-off in section 6.3 of `PLAN.md` picks, rather than materializing a
/// `FlatState` on every event.
#[derive(Debug, Clone, Default)]
pub struct FlatState {
    entries: BTreeMap<(String, String), (OwnedUserId, CanonicalJsonObject)>,
}

impl FlatState {
    /// An empty state map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Inserts or replaces a state event.
    pub fn insert(
        &mut self,
        event_type: impl Into<String>,
        state_key: impl Into<String>,
        sender: OwnedUserId,
        content: CanonicalJsonObject,
    ) {
        self.entries
            .insert((event_type.into(), state_key.into()), (sender, content));
    }

    /// The number of state events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no state events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over all entries as `((event_type, state_key), (sender, content))`.
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (&(String, String), &(OwnedUserId, CanonicalJsonObject))> {
        self.entries.iter()
    }
}

impl StateFetch for FlatState {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>> {
        self.entries
            .get(&(event_type.to_owned(), state_key.to_owned()))
            .map(|(sender, content)| StateEntry { sender, content })
    }
}

// -------------------------------------------------------------------------------------------
// The `StateStore` -> `StateFetch` adapter.
// -------------------------------------------------------------------------------------------

/// Hands back one already-known event's `sender` and `content` by [`EventSn`], without this crate
/// needing to depend on `hs_model::event::Event` directly.
///
/// This is [`StoreStateFetch`]'s other half: a [`StateStore`] resolves `(event_type, state_key)`
/// down to the [`EventSn`] of the event that set it (`StateStore::get`), but a `StateStore` never
/// stores event bodies itself -- `crate::api`'s module docs describe it purely in terms of
/// `Root`s, diffs and resolution, never `sender`/`content`. The caller already holds bodies
/// somewhere (a room actor's in-memory event cache in production;
/// [`EventBodies`] in tests), and this trait is the narrow view of that cache
/// [`StoreStateFetch`] needs.
pub trait EventBody {
    /// The sender and content of `event`, if this source knows about it.
    fn body(&self, event: EventSn) -> Option<(&UserId, &CanonicalJsonObject)>;
}

impl<T: EventBody + ?Sized> EventBody for &T {
    fn body(&self, event: EventSn) -> Option<(&UserId, &CanonicalJsonObject)> {
        (**self).body(event)
    }
}

/// A simple [`EventBody`] source: `EventSn -> (sender, content)`. What test fixtures for
/// [`StoreStateFetch`] build directly; the production room actor already holds event bodies in
/// some form of its own (see `docs/status/02-state-and-model.md`'s "Interfaces provided" for the
/// production call sequence) and is expected to implement [`EventBody`] over that directly rather
/// than duplicate it into one of these.
#[derive(Debug, Clone, Default)]
pub struct EventBodies {
    entries: BTreeMap<EventSn, (OwnedUserId, CanonicalJsonObject)>,
}

impl EventBodies {
    /// An empty set of event bodies.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Records `event`'s sender and content.
    pub fn insert(&mut self, event: EventSn, sender: OwnedUserId, content: CanonicalJsonObject) {
        self.entries.insert(event, (sender, content));
    }
}

impl EventBody for EventBodies {
    fn body(&self, event: EventSn) -> Option<(&UserId, &CanonicalJsonObject)> {
        self.entries
            .get(&event)
            .map(|(sender, content)| (sender.as_ref(), content))
    }
}

/// The [`StateStore`]-to-[`StateFetch`] adapter: bridges a resolved state
/// ([`StateStore::Root`]) plus a source of event bodies ([`EventBody`]) into the narrow lookup
/// interface [`crate::auth::check_event_auth`] and [`crate::auth::check_auth_events_selection`]
/// read through, so a caller holding a production [`StateStore`] (e.g.
/// `crate::kv_store::ProductionStateStore`) does not need to materialize a whole [`FlatState`]-style
/// map just to authorize one event.
///
/// # Access pattern
///
/// Event authorization looks up at most a handful of entries per event (see the module docs).
/// [`StateFetch::get`] on this type is exactly two [`StateStore`] calls
/// ([`StateStore::intern_state_key`], [`StateStore::get`]) plus one [`EventBody::body`] call --
/// none of them materialize anything beyond the single entry asked for, and nothing here ever
/// calls [`StateStore::diff`] or otherwise walks a whole state map. That matches the production
/// representation's own cheap path (`crate::frames::FrameRepr`, `docs/status/02-state-and-model.md`'s
/// "Implications for tracks 04 and 06"): a handful of point lookups against a `Root` the caller
/// already holds, not a materialize-then-scan.
///
/// # Errors have no channel here
///
/// [`StateFetch::get`] returns a plain `Option`, not a `Result`: a storage-layer error from the
/// underlying [`StateStore`] is therefore indistinguishable from "this key has no value" (both
/// produce `None`). Callers that need to tell those apart (surface a real I/O error rather than
/// silently treating the room as if the key had never been set) should call
/// [`StateStore::intern_state_key`]/[`StateStore::get`] directly instead of going through this
/// adapter.
pub struct StoreStateFetch<'a, S: StateStore, B: EventBody> {
    store: &'a S,
    root: S::Root,
    bodies: &'a B,
}

impl<'a, S: StateStore, B: EventBody> StoreStateFetch<'a, S, B> {
    /// Adapts `store`'s state at `root` into a [`StateFetch`], resolving event bodies through
    /// `bodies`.
    #[must_use]
    pub fn new(store: &'a S, root: S::Root, bodies: &'a B) -> Self {
        Self {
            store,
            root,
            bodies,
        }
    }
}

impl<'a, S: StateStore, B: EventBody> StateFetch for StoreStateFetch<'a, S, B> {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>> {
        let key_id = self.store.intern_state_key(event_type, state_key).ok()?;
        let sn = match self.store.get(self.root, key_id) {
            Ok(Some(sn)) => sn,
            _ => return None,
        };
        let (sender, content) = self.bodies.body(sn)?;
        Some(StateEntry { sender, content })
    }
}

#[cfg(test)]
mod adapter_tests {
    use hs_kv::memory::MemoryBackend;
    use hs_model::canonical::to_canonical_object;
    use ruma::{EventId, RoomId, RoomVersionId, UserId};
    use serde_json::json;

    use super::*;
    use crate::auth::{self, IncomingEvent};
    use crate::frames::FrameRepr;
    use crate::kv_store::KvStateStore;

    fn obj(v: serde_json::Value) -> CanonicalJsonObject {
        to_canonical_object(&v, true).unwrap()
    }

    /// Builds `create -> creator join -> power_levels` through the production store, adapts its
    /// current state via [`StoreStateFetch`], and checks that authorizing a subsequent event
    /// (alice's join) through the adapter gives the same verdict as authorizing the identical
    /// event through a hand-built [`FlatState`] -- i.e. the adapter is a faithful,
    /// no-materialization substitute for the flat map a caller would otherwise have to build.
    #[test]
    fn store_state_fetch_matches_flat_state_for_auth() {
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = UserId::parse("@creator:hs1").unwrap();
        let alice = UserId::parse("@alice:hs2").unwrap();
        let rules = hs_model::room_version::rules_for(&RoomVersionId::V11).unwrap();

        let repr = FrameRepr::new(MemoryBackend::default()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr).unwrap();
        let mut bodies = EventBodies::new();
        let mut flat = FlatState::new();

        let create_content = obj(json!({"creator": creator.as_str()}));
        store
            .add_event(
                EventSn::new(1),
                EventId::parse("$1:hs1").unwrap(),
                room_id.clone(),
                "m.room.create",
                Some(""),
                creator.clone(),
                create_content.clone(),
                1,
                1,
                &[],
                &[],
                false,
            )
            .unwrap();
        bodies.insert(EventSn::new(1), creator.clone(), create_content.clone());
        flat.insert("m.room.create", "", creator.clone(), create_content);

        let join_content = obj(json!({"membership": "join"}));
        store
            .add_event(
                EventSn::new(2),
                EventId::parse("$2:hs1").unwrap(),
                room_id.clone(),
                "m.room.member",
                Some(creator.as_str()),
                creator.clone(),
                join_content.clone(),
                2,
                2,
                &[EventSn::new(1)],
                &[EventSn::new(1)],
                true,
            )
            .unwrap();
        bodies.insert(EventSn::new(2), creator.clone(), join_content.clone());
        flat.insert(
            "m.room.member",
            creator.as_str(),
            creator.clone(),
            join_content,
        );

        let pl_content = obj(json!({
            "users": {creator.as_str(): 100},
            "ban": 50, "kick": 50, "redact": 50, "invite": 0,
            "users_default": 0, "events_default": 0, "state_default": 50,
        }));
        let final_root = store
            .add_event(
                EventSn::new(3),
                EventId::parse("$3:hs1").unwrap(),
                room_id,
                "m.room.power_levels",
                Some(""),
                creator.clone(),
                pl_content.clone(),
                3,
                3,
                &[EventSn::new(1), EventSn::new(2)],
                &[EventSn::new(2)],
                false,
            )
            .unwrap();
        bodies.insert(EventSn::new(3), creator.clone(), pl_content.clone());
        flat.insert("m.room.power_levels", "", creator.clone(), pl_content);

        let adapted = StoreStateFetch::new(&store, final_root, &bodies);

        // The adapter answers every lookup `FlatState` does, without ever materializing a map.
        assert!(adapted.create().is_some());
        assert!(adapted.power_levels().is_some());
        assert!(adapted.member(&creator).is_some());
        assert!(adapted.member(&alice).is_none());

        let alice_join_content = obj(json!({"membership": "join"}));
        let alice_join = IncomingEvent::new(
            "m.room.member",
            &alice,
            None,
            Some(alice.as_str()),
            &alice_join_content,
        );

        let via_adapter = auth::check_event_auth(&rules, &alice_join, &adapted);
        let via_flat = auth::check_event_auth(&rules, &alice_join, &flat);
        assert_eq!(
            via_adapter.is_ok(),
            via_flat.is_ok(),
            "the adapter and a hand-built FlatState must reach the same auth verdict"
        );
    }
}
