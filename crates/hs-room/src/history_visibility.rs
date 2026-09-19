//! The `m.room.history_visibility` read-side algorithm: given the room's history-visibility
//! setting and a user's membership, both evaluated *at a specific event*, decide whether that
//! user may see the event. Pure logic only -- `crate::actor::RoomActor::event_visible_to` and
//! `RoomActor::can_read_room` supply the state snapshots this module reasons about (via
//! `hs_state`-resolved state views, not re-derived here).
//!
//! Ported from `refs/matrix-spec/content/client-server-api/modules/history_visibility.md`
//! ("Server behaviour" section; matrix-spec is CC-BY-4.0) -- the five numbered rules and the two
//! "before or after" special cases are that document's algorithm, restated as code. See
//! `crate::actor::RoomActor::event_visible_to`'s doc comment for exactly how the two special
//! cases (`m.room.history_visibility` events, and a user's own `m.room.member` events) are
//! applied on top of [`base_rule_allows`].

use crate::membership::PriorState;

/// The four values `m.room.history_visibility`'s `content.history_visibility` can hold, plus the
/// spec's documented default: "By default if no `history_visibility` is set, or if the value is
/// not understood, the visibility is assumed to be `shared`."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryVisibility {
    /// Any authenticated user may see the event, whether or not they have ever joined the room.
    WorldReadable,
    /// Any user who joins the room may see this event, even if it was sent before they joined.
    Shared,
    /// Visible from the point a user was invited onwards; stops being visible once their
    /// membership becomes anything other than `invite` or `join`.
    Invited,
    /// Visible from the point a user joined onwards; stops being visible once their membership
    /// becomes anything other than `join`.
    Joined,
}

impl HistoryVisibility {
    /// Parses `content.history_visibility`'s string value. Unset or unrecognized values default
    /// to [`HistoryVisibility::Shared`], per the spec module's documented default.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("world_readable") => Self::WorldReadable,
            Some("invited") => Self::Invited,
            Some("joined") => Self::Joined,
            _ => Self::Shared,
        }
    }
}

/// Rules 1-5 of the spec's "Server behaviour" section, given `visibility` and `membership` both
/// already resolved to the same point in the room's history, plus `joined_later` (rule 3's "the
/// user joined the room at any point after the event was sent" -- computed by the caller by
/// scanning forward from the event in question, since it is not a property of one state snapshot).
///
/// 1. `world_readable` always allows, regardless of membership.
/// 2. A `join` membership always allows, regardless of `visibility`.
/// 3. `shared` allows if the user joined at some later point (even if they are not currently
///    joined, and even for events sent before they ever joined at all).
/// 4. `invited` allows an `invite` membership (a `join` membership already matched rule 2).
/// 5. Otherwise, deny.
#[must_use]
pub fn base_rule_allows(
    visibility: HistoryVisibility,
    membership: PriorState,
    joined_later: bool,
) -> bool {
    match visibility {
        HistoryVisibility::WorldReadable => true,
        _ if membership == PriorState::Join => true,
        HistoryVisibility::Shared if joined_later => true,
        HistoryVisibility::Invited if membership == PriorState::Invite => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_unset_and_unrecognized_to_shared() {
        assert_eq!(HistoryVisibility::parse(None), HistoryVisibility::Shared);
        assert_eq!(
            HistoryVisibility::parse(Some("nonsense")),
            HistoryVisibility::Shared
        );
    }

    #[test]
    fn world_readable_allows_regardless_of_membership() {
        assert!(base_rule_allows(
            HistoryVisibility::WorldReadable,
            PriorState::None,
            false
        ));
    }

    #[test]
    fn join_always_allows() {
        for visibility in [
            HistoryVisibility::Shared,
            HistoryVisibility::Invited,
            HistoryVisibility::Joined,
        ] {
            assert!(base_rule_allows(visibility, PriorState::Join, false));
        }
    }

    #[test]
    fn shared_allows_a_later_joiner_even_for_an_event_sent_before_they_joined() {
        assert!(base_rule_allows(
            HistoryVisibility::Shared,
            PriorState::None,
            true
        ));
        assert!(!base_rule_allows(
            HistoryVisibility::Shared,
            PriorState::None,
            false
        ));
    }

    #[test]
    fn invited_allows_only_invite_or_join() {
        assert!(base_rule_allows(
            HistoryVisibility::Invited,
            PriorState::Invite,
            false
        ));
        assert!(!base_rule_allows(
            HistoryVisibility::Invited,
            PriorState::Leave,
            false
        ));
        assert!(!base_rule_allows(
            HistoryVisibility::Invited,
            PriorState::None,
            false
        ));
    }

    #[test]
    fn joined_denies_anyone_not_currently_join_at_the_event() {
        assert!(!base_rule_allows(
            HistoryVisibility::Joined,
            PriorState::Invite,
            true
        ));
        assert!(!base_rule_allows(
            HistoryVisibility::Joined,
            PriorState::Leave,
            true
        ));
    }
}
