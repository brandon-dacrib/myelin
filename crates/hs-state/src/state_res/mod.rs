//! State resolution: v1 in-house ([`v1`]), v2 and v2.1 via `ruma-state-res` ([`v2`]), and an
//! independent oracle implementation of v2/v2.1 used only in this crate's tests ([`oracle`]).
//!
//! `docs/workstreams/02-state-and-model.md`: "State resolution v1 in-house; v2 and v2.1 through
//! `ruma-state-res`; an independent oracle implementation of v2 and v2.1 straight from the spec,
//! used only in tests."
//!
//! # Shared types
//!
//! All three algorithms operate on the same event representation: a [`StateMap`] (the classic
//! `(event_type, state_key) -> event_id` map every version of the spec's state resolution section
//! describes) plus an [`EventStore`] that resolves an ID to the event data auth checks need. This
//! is deliberately *not* the interned representation `hs-tables` will eventually provide (or the
//! state representation the bake-off in `PLAN.md` section 6.3 picks): these three algorithms are
//! oracles for correctness, not the hot path, and the frozen [`crate::api`] trait is what the room
//! actor actually calls. `hs-room` is expected to adapt its own state storage into a `StateMap`
//! plus a `StateFetch`/event lookup at the boundary of calling into this module, the same way it
//! would adapt into [`crate::auth`].

pub mod v1;
pub mod v2;

#[cfg(test)]
mod cross_check_tests;
#[cfg(test)]
mod fork_production_cross_check;
#[cfg(test)]
pub(crate) mod oracle;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::BTreeMap;

use hs_model::canonical::CanonicalJsonObject;
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId};

use crate::state_fetch::{StateEntry, StateFetch};

/// A resolved (or partial, mid-resolution) room state: `(event_type, state_key) -> event_id`.
pub type StateMap = BTreeMap<(String, String), OwnedEventId>;

/// One event, in the owned form the state resolution algorithms in this module work with.
///
/// Real callers (`hs-room`) hold events in whatever cached-bytes representation
/// [`hs_model::event::Event`] provides; this type exists so the resolution algorithms have a
/// small, self-contained, owned representation to build ad hoc combinations of events from
/// different branches of history without fighting borrow lifetimes across a `BTreeMap` built from
/// several input state maps at once.
#[derive(Debug, Clone)]
pub struct ResolutionEvent {
    /// The event's ID.
    pub event_id: OwnedEventId,
    /// The room the event belongs to.
    pub room_id: OwnedRoomId,
    /// The event's `type`.
    pub event_type: String,
    /// The event's `state_key` (state resolution only ever operates on state events).
    pub state_key: String,
    /// The event's `sender`.
    pub sender: OwnedUserId,
    /// The event's `content`.
    pub content: CanonicalJsonObject,
    /// The event's `depth`.
    pub depth: i64,
    /// The event's `origin_server_ts`, milliseconds since the Unix epoch. Used as a tie-breaker
    /// by the v2/v2.1 power and mainline orderings.
    pub origin_server_ts: i64,
    /// The event's `auth_events`.
    pub auth_events: Vec<OwnedEventId>,
    /// The event's `prev_events`.
    pub prev_events: Vec<OwnedEventId>,
    /// Whether `prev_events` is exactly `[create_event_id]` -- see
    /// [`crate::auth::IncomingEvent::only_prev_event_is_room_create`].
    pub only_prev_event_is_room_create: bool,
}

/// A lookup from event ID to [`ResolutionEvent`], shared by all events under resolution
/// (typically: every event in the room, or at least everything reachable from the state maps
/// being resolved and their auth chains).
pub type EventStore = BTreeMap<OwnedEventId, ResolutionEvent>;

/// A [`StateFetch`] backed by a [`StateMap`] plus an [`EventStore`]: resolves `(type, state_key)`
/// to an ID via the map, then the ID to sender/content via the store.
pub(crate) struct MapStateFetch<'a> {
    pub map: &'a StateMap,
    pub store: &'a EventStore,
}

impl StateFetch for MapStateFetch<'_> {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>> {
        let id = self
            .map
            .get(&(event_type.to_owned(), state_key.to_owned()))?;
        let event = self.store.get(id)?;
        Some(StateEntry {
            sender: &event.sender,
            content: &event.content,
        })
    }
}

/// Builds the [`crate::auth::IncomingEvent`] view of a [`ResolutionEvent`], for running it through
/// [`crate::auth::check_event_auth`] during resolution.
pub(crate) fn incoming_event(event: &ResolutionEvent) -> crate::auth::IncomingEvent<'_> {
    crate::auth::IncomingEvent {
        event_type: &event.event_type,
        sender: &event.sender,
        room_id: Some(&event.room_id),
        state_key: Some(&event.state_key),
        content: &event.content,
        prev_event_count: event.prev_events.len(),
        only_prev_event_is_room_create: event.only_prev_event_is_room_create,
        event_id: Some(&event.event_id),
        redacts: None,
    }
}

/// Splits the input state maps into the unconflicted union and the conflicted entries (each
/// conflicted key mapped to every distinct candidate ID seen for it, in the order first seen).
///
/// Matches the spec's literal definition (room version 2's "Unconflicted state map and conflicted
/// state set", which this crate also applies to v1 for consistency): a `(type, state_key)` pair is
/// unconflicted only if it is present in *every* input state map with the *same* event ID: a key
/// present in all but one of the input maps, even with everyone who has it agreeing, is
/// conflicted. This matches `ruma_state_res::state_res::split_conflicted_state_set`'s behavior,
/// which this module's tests cross-check against.
pub(crate) fn split_conflicted(
    states: &[StateMap],
) -> (StateMap, BTreeMap<(String, String), Vec<OwnedEventId>>) {
    let mut occurrences: BTreeMap<(String, String), BTreeMap<OwnedEventId, usize>> =
        BTreeMap::new();
    for state in states {
        for (key, id) in state {
            *occurrences
                .entry(key.clone())
                .or_default()
                .entry(id.clone())
                .or_insert(0) += 1;
        }
    }

    let mut unconflicted = StateMap::new();
    let mut conflicted: BTreeMap<(String, String), Vec<OwnedEventId>> = BTreeMap::new();
    for (key, by_id) in occurrences {
        for (id, count) in by_id {
            if count == states.len() {
                unconflicted.insert(key.clone(), id);
            } else {
                conflicted.entry(key.clone()).or_default().push(id);
            }
        }
    }
    (unconflicted, conflicted)
}

/// Convenience used by [`v1`] and [`oracle`]: given `event_id`'s ASCII bytes, its SHA-1 digest.
/// Room version 1's state resolution algorithm literally specifies `sha1(event_id)` as a tie
/// breaker; it predates the cryptographic concerns that make SHA-1 unsuitable elsewhere, and is
/// kept here only because the spec text names it.
pub(crate) fn sha1_of_event_id(event_id: &EventId) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    Sha1::digest(event_id.as_str().as_bytes()).into()
}
