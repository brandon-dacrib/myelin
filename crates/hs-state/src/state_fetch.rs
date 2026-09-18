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
use ruma::{OwnedUserId, UserId};
use std::collections::BTreeMap;

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
