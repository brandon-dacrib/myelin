//! The membership state machine: the client-facing action set (join, invite, leave, kick, ban,
//! unban, knock) and how each maps onto an `m.room.member` event.
//!
//! `hs-state`'s [`hs_state::auth::check_event_auth`] is the authority on whether a given
//! `m.room.member` event is valid (it implements the full spec rule set, including restricted and
//! knock-restricted join rules and third-party invites -- see that crate's module docs); this
//! module does not re-derive those rules. What it owns is the *state machine surface*:
//!
//! - [`Action`]: the seven operations a client or another server can ask for.
//! - [`content_for`]: builds the correct `m.room.member` content for an action (which fields it
//!   sets, and -- for `kick`/`unban`, which are not separate spec actions but a `leave`/`join`
//!   membership value reached by an authorized third party -- which membership value results).
//! - [`precheck`]: [`TRANSITIONS`], an explicit table of "this action is only sane from these
//!   prior membership states", checked *before* the expensive build-authorize-persist pipeline
//!   runs, purely so a client gets a clear `M_FORBIDDEN`/`M_BAD_STATE` early rather than a generic
//!   rejection message from deep inside `hs-state`'s auth checker. It is deliberately a
//!   *necessary*, not sufficient, condition: [`precheck`] passing does not mean `hs-state` will
//!   accept the event (power levels, join rules and third-party invite validity are still real
//!   gates only the full auth check can decide), and every room-version-parameterized case this
//!   table encodes is cross-checked against `hs-state::auth` in this module's property tests.

use hs_model::room_version::RoomVersionRules;

/// The membership actions a room actor exposes to callers. Distinct from
/// [`hs_state::auth::Membership`] (the *value* an `m.room.member` event's `membership` field can
/// hold): `kick`, `ban` and `unban` are actions a third party takes that produce a `leave` or
/// `join`-absent membership value, not membership values of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// The target joins the room themselves.
    Join,
    /// The sender invites the target.
    Invite,
    /// The target leaves (or, if sent by someone else while the target is only invited or
    /// knocking, the invite/knock is retracted -- the spec calls both "leave").
    Leave,
    /// The sender removes an already-joined target (a `leave` event sent by someone other than
    /// the target).
    Kick,
    /// The sender bans the target.
    Ban,
    /// The sender lifts a ban (a `leave` event over a currently-banned target).
    Unban,
    /// The target knocks, requesting an invite.
    Knock,
}

/// The membership states [`precheck`]'s table reasons about: [`hs_state::auth::Membership`] plus
/// "no membership event at all yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriorState {
    /// No `m.room.member` event for this user in the room's current state.
    None,
    /// `membership: join`.
    Join,
    /// `membership: invite`.
    Invite,
    /// `membership: leave`.
    Leave,
    /// `membership: ban`.
    Ban,
    /// `membership: knock`.
    Knock,
}

/// Why [`precheck`] rejected an action, before authorization was even attempted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrecheckError {
    /// The action makes no sense from the target's current membership state (for example,
    /// `kick`ing a user who was never a member).
    #[error("{action:?} is not a valid transition from {prior:?}")]
    InvalidTransition {
        /// The rejected action.
        action: Action,
        /// The target's current membership state.
        prior: PriorState,
    },
    /// `knock` was attempted in a room version that does not support knocking at all
    /// ([`RoomVersionRules::knocking`]).
    #[error("this room version does not support knocking")]
    KnockingUnsupported,
}

/// The explicit transition table: for each [`Action`], the [`PriorState`]s it is valid from. A
/// room-version-gated action (only `knock`, so far) is listed here regardless of version;
/// [`precheck`] checks [`RoomVersionRules::knocking`] separately so the table itself stays a pure
/// function of membership state.
///
/// Rows follow the spec's "Room membership" section
/// (`refs/matrix-spec/content/client-server-api.md`, Apache-2.0) and `hs-state::auth`'s
/// `check_member_*` functions, which this table is a friendlier restatement of, not a
/// replacement for.
pub const TRANSITIONS: &[(Action, &[PriorState])] = &[
    // A user may join if they were never a member, already invited, knocking, or (a public room,
    // or rejoining after leaving) previously left. Not from `ban`.
    (
        Action::Join,
        &[
            PriorState::None,
            PriorState::Invite,
            PriorState::Knock,
            PriorState::Leave,
            PriorState::Join, // Re-sending join while already joined is a harmless no-op event.
        ],
    ),
    // A user may be invited if they are not already joined or banned.
    (
        Action::Invite,
        &[PriorState::None, PriorState::Leave, PriorState::Invite],
    ),
    // The target may leave from join, invite or knock (retracting either of the latter two).
    (
        Action::Leave,
        &[PriorState::Join, PriorState::Invite, PriorState::Knock],
    ),
    // Only a currently-joined user can be kicked.
    (Action::Kick, &[PriorState::Join]),
    // A ban can be placed from any state except an existing ban (banning an already-banned user
    // is a no-op the spec does not forbid, but this table treats it as not worth re-sending).
    (
        Action::Ban,
        &[
            PriorState::None,
            PriorState::Join,
            PriorState::Invite,
            PriorState::Leave,
            PriorState::Knock,
        ],
    ),
    // Unban is only meaningful from a ban.
    (Action::Unban, &[PriorState::Ban]),
    // Knocking is only sane if the user is not already a member in some other capacity.
    (Action::Knock, &[PriorState::None, PriorState::Leave]),
];

/// Checks `action` against [`TRANSITIONS`] for the target's `prior` membership state. See the
/// module docs: this is a fast, friendly precheck, not the authority.
///
/// # Errors
/// Returns [`PrecheckError`] if the transition is not in the table, or if `action` is
/// [`Action::Knock`] and `rules.knocking` is `false`.
pub fn precheck(rules: &RoomVersionRules, action: Action, prior: PriorState) -> Result<(), PrecheckError> {
    if action == Action::Knock && !rules.knocking {
        return Err(PrecheckError::KnockingUnsupported);
    }
    let allowed = TRANSITIONS
        .iter()
        .find(|(a, _)| *a == action)
        .map(|(_, states)| *states)
        .unwrap_or(&[]);
    if allowed.contains(&prior) {
        Ok(())
    } else {
        Err(PrecheckError::InvalidTransition { action, prior })
    }
}

/// The `membership` value an [`Action`] produces on the resulting `m.room.member` event.
#[must_use]
pub fn membership_value(action: Action) -> &'static str {
    match action {
        Action::Join => "join",
        Action::Invite => "invite",
        Action::Leave | Action::Kick | Action::Unban => "leave",
        Action::Ban => "ban",
        Action::Knock => "knock",
    }
}

/// Builds the `m.room.member` content for `action`, merging in `extra` (a client-supplied
/// `reason`, `displayname`/`avatar_url` for a join, `third_party_invite`,
/// `join_authorised_via_users_server`, ...). `extra`'s `membership` key, if present, is
/// overwritten: the action alone determines it.
#[must_use]
pub fn content_for(action: Action, mut extra: serde_json::Value) -> serde_json::Value {
    if !extra.is_object() {
        extra = serde_json::json!({});
    }
    let map = extra.as_object_mut().expect("forced to an object above");
    map.insert(
        "membership".to_owned(),
        serde_json::Value::String(membership_value(action).to_owned()),
    );
    extra
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_model::room_version::{self};
    use proptest::prelude::*;
    use ruma::RoomVersionId;

    fn all_actions() -> Vec<Action> {
        vec![
            Action::Join,
            Action::Invite,
            Action::Leave,
            Action::Kick,
            Action::Ban,
            Action::Unban,
            Action::Knock,
        ]
    }

    fn all_prior_states() -> Vec<PriorState> {
        vec![
            PriorState::None,
            PriorState::Join,
            PriorState::Invite,
            PriorState::Leave,
            PriorState::Ban,
            PriorState::Knock,
        ]
    }

    #[test]
    fn content_for_sets_membership_and_preserves_extra_fields() {
        let content = content_for(Action::Invite, serde_json::json!({"reason": "welcome"}));
        assert_eq!(content["membership"], "invite");
        assert_eq!(content["reason"], "welcome");
    }

    #[test]
    fn content_for_overwrites_a_conflicting_membership_field() {
        let content = content_for(Action::Ban, serde_json::json!({"membership": "join"}));
        assert_eq!(content["membership"], "ban");
    }

    #[test]
    fn knock_is_rejected_outright_in_room_versions_without_knocking() {
        let rules = room_version::rules_for(&RoomVersionId::V6).unwrap();
        assert!(!rules.knocking);
        let err = precheck(&rules, Action::Knock, PriorState::None).unwrap_err();
        assert_eq!(err, PrecheckError::KnockingUnsupported);
    }

    #[test]
    fn every_action_has_at_least_one_allowed_prior_state() {
        for action in all_actions() {
            let row = TRANSITIONS.iter().find(|(a, _)| *a == action);
            assert!(row.is_some(), "{action:?} missing from TRANSITIONS");
            assert!(!row.unwrap().1.is_empty());
        }
    }

    proptest! {
        /// The table is a pure function: every `(action, prior)` pair either matches a row or
        /// does not, and `precheck`'s verdict must agree with a literal table scan -- this is
        /// mostly a change-detector against `precheck` and `TRANSITIONS` drifting apart, since
        /// `precheck` is hand-written to read the table but nothing enforces that mechanically.
        #[test]
        fn precheck_verdict_matches_a_literal_table_scan(
            action_idx in 0..7usize,
            prior_idx in 0..6usize,
        ) {
            let action = all_actions()[action_idx];
            let prior = all_prior_states()[prior_idx];
            let rules = room_version::rules_for(&RoomVersionId::V12).unwrap();

            let expected_in_table = TRANSITIONS
                .iter()
                .find(|(a, _)| *a == action)
                .is_some_and(|(_, states)| states.contains(&prior));

            let result = precheck(&rules, action, prior);
            prop_assert_eq!(result.is_ok(), expected_in_table);
        }
    }
}
