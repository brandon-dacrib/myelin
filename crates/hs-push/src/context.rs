//! The shape of "what the room actor must publish for push to evaluate an event", and the
//! function that turns it into `ruma::push::PushConditionRoomCtx` for one recipient.
//!
//! [`PushEvaluationInput`] is the type `docs/status/10-push.md` asks track 04 to put on
//! `hs_room::protocol::RoomUpdate::push_evaluation_inputs` (today `Vec<()>`, a placeholder). It
//! deliberately contains only plain types this crate already needs nothing else for
//! (`ruma::OwnedUserId`, `hs_model::power_levels::PowerLevels`, `ruma::RoomVersionId`) so that
//! `hs-room` does not need to depend on `hs-push` to produce it — `hs-push` already depends on
//! `hs-room`'s publish stream (`docs/workstreams/README.md`'s week-8 seam), so the reverse
//! dependency would be a cycle. See the module docs on `crate::compiled` for how this feeds the
//! hot per-event, per-recipient evaluation path.

use hs_model::power_levels::PowerLevels;
use ruma::power_levels::NotificationPowerLevels;
use ruma::push::{PushConditionPowerLevelsCtx, PushConditionRoomCtx};
use ruma::{Int, OwnedUserId, RoomVersionId, UInt};

/// One room member, as push evaluation needs them. See [`PushEvaluationInput::members`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEvaluationMember {
    /// The member's user ID.
    pub user_id: OwnedUserId,
    /// `"join"` or `"invite"` — the only two memberships push evaluation cares about. Uses the
    /// raw membership string (matching `hs_room::protocol::MembershipDelta::membership`) rather
    /// than a bespoke enum, so track 04 does not need a second membership type to populate this.
    pub membership: String,
    /// This member's current room-specific display name (`m.room.member` `content.displayname`),
    /// falling back to their bare user ID if unset. Exactly the string
    /// `PushConditionRoomCtx::user_display_name` needs for the deprecated
    /// `.m.rule.contains_display_name` condition, without a second state read per recipient.
    pub display_name: String,
    /// Whether this member is local to this server. Remote members are still included (their
    /// power level can affect `sender_notification_permission` for a *local* recipient), but
    /// `crate::pushers` must never generate a push notification, HTTP or email, for a non-local
    /// member — there is nothing local to push to.
    pub is_local: bool,
}

/// The room's current push-relevant state, republished on every `RoomUpdate`. A minimal cut of
/// `m.room.power_levels`, `m.room.member` and the room's member count: exactly what
/// [`build_room_ctx`] needs per recipient, so `hs-push` evaluates every local recipient of one
/// event without a second store read.
#[derive(Debug, Clone)]
pub struct PushEvaluationInput {
    /// The room's current count of joined members (`room_member_count` condition's
    /// `member_count`). Matches the spec's own counting rule: joined members only, invites do
    /// not count toward it.
    pub joined_member_count: u64,
    /// Local and remote members currently joined or invited, for looking up a recipient's
    /// display name and local-ness without a second read.
    pub members: Vec<PushEvaluationMember>,
    /// The room's current effective power levels (`m.room.power_levels`'s content, with the room
    /// version's default-value rules already applied — `hs_model::power_levels::PowerLevels`),
    /// or `None` if no `m.room.power_levels` event has ever been sent. Kept as `Option` rather
    /// than defaulting, because `sender_notification_permission` must be able to tell "no power
    /// levels event yet" apart from "power levels event with all-default values" the same way
    /// `ruma::push::PushConditionRoomCtx::power_levels: Option<_>` does.
    pub power_levels: Option<PowerLevels>,
    /// The room version, needed to build `ruma::room_version_rules::RoomPowerLevelsRules` (the
    /// privileged-creator handling differs by version). See [`build_room_ctx`]'s doc comment for
    /// the one behavior this crate does not yet implement here (MSC4289 creator privilege).
    pub room_version: RoomVersionId,
}

/// Builds a [`PushConditionRoomCtx`] for one recipient out of a [`PushEvaluationInput`].
///
/// # MSC4289 (room version 12 privileged creators)
///
/// `ruma::push::PushConditionPowerLevelsCtx`'s `sender_notification_permission` evaluation can
/// treat a room's creator(s) as having an effectively infinite power level
/// (`RoomPowerLevelsRules::privileged_creators`). Building that set correctly needs the room's
/// creator identity, which is not on [`PushEvaluationInput`] (a minimal cut, not the full state,
/// per `hs_room::protocol`'s own module docs). This function always passes an empty creator set,
/// which is exactly equivalent to not implementing MSC4289 creator privilege for push: a v12
/// room's creator gets the notification permission their explicit power level says, not an
/// implicit boost. Recorded in `docs/status/10-push.md`'s "Decisions made" as a scoped gap, not
/// a silent one; closing it means adding the room's creator `Vec<OwnedUserId>` to
/// `PushEvaluationInput` if track 04 wants to keep it minimal, or growing the room-version rules
/// consulted here.
pub fn build_room_ctx(
    room_id: &ruma::RoomId,
    input: &PushEvaluationInput,
    recipient: &PushEvaluationMember,
) -> PushConditionRoomCtx {
    let member_count = UInt::try_from(input.joined_member_count).unwrap_or(UInt::MAX);
    let ctx = PushConditionRoomCtx::new(
        room_id.to_owned(),
        member_count,
        recipient.user_id.clone(),
        recipient.display_name.clone(),
    );
    let Some(power_levels) = &input.power_levels else {
        return ctx;
    };
    let users = power_levels
        .users
        .iter()
        .filter_map(|(user, level)| Some((user.clone(), Int::try_from(*level).ok()?)))
        .collect();
    let users_default = Int::try_from(power_levels.users_default).unwrap_or_default();
    // `NotificationPowerLevels` is `#[non_exhaustive]`: build the all-defaults value (its `room`
    // default is the spec's 50) and assign over the one field we have a source for.
    let mut notifications = NotificationPowerLevels::new();
    if let Some(room) = power_levels
        .notifications
        .get("room")
        .copied()
        .and_then(|v| Int::try_from(v).ok())
    {
        notifications.room = room;
    }
    let auth_rules = input
        .room_version
        .rules()
        .map(|r| r.authorization)
        .unwrap_or(ruma::room_version_rules::AuthorizationRules::V1);
    let rules = ruma::room_version_rules::RoomPowerLevelsRules::new(&auth_rules, std::iter::empty());
    let power_levels_ctx = PushConditionPowerLevelsCtx::new(users, users_default, notifications, rules);
    ctx.with_power_levels(power_levels_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str) -> PushEvaluationMember {
        PushEvaluationMember {
            user_id: ruma::user_id!("@alice:example.org").to_owned(),
            membership: "join".to_owned(),
            display_name: id.to_owned(),
            is_local: true,
        }
    }

    #[test]
    fn no_power_levels_event_leaves_power_levels_none() {
        let input = PushEvaluationInput {
            joined_member_count: 2,
            members: vec![],
            power_levels: None,
            room_version: RoomVersionId::V11,
        };
        let room_id = ruma::room_id!("!room:example.org");
        let ctx = build_room_ctx(room_id, &input, &member("Alice"));
        assert!(ctx.power_levels.is_none());
        assert_eq!(ctx.member_count, UInt::from(2u32));
        assert_eq!(ctx.user_display_name, "Alice");
    }

    #[test]
    fn power_levels_event_populates_notification_permission_context() {
        let mut power_levels = PowerLevels::default();
        power_levels
            .users
            .insert(ruma::user_id!("@bob:example.org").to_owned(), 100);
        power_levels
            .notifications
            .insert("room".to_owned(), 60);
        let input = PushEvaluationInput {
            joined_member_count: 5,
            members: vec![],
            power_levels: Some(power_levels),
            room_version: RoomVersionId::V11,
        };
        let room_id = ruma::room_id!("!room:example.org");
        let ctx = build_room_ctx(room_id, &input, &member("Alice"));
        let pls = ctx.power_levels.expect("power levels should be Some");
        assert_eq!(pls.notifications.room, Int::from(60));
        assert_eq!(
            pls.users.get(ruma::user_id!("@bob:example.org")),
            Some(&Int::from(100))
        );
    }

    #[test]
    fn missing_notifications_room_defaults_to_fifty() {
        let power_levels = PowerLevels::default();
        let input = PushEvaluationInput {
            joined_member_count: 1,
            members: vec![],
            power_levels: Some(power_levels),
            room_version: RoomVersionId::V11,
        };
        let room_id = ruma::room_id!("!room:example.org");
        let ctx = build_room_ctx(room_id, &input, &member("Alice"));
        // `PowerLevels::default()` already sets `notifications["room"] = 50` (spec default), so
        // this exercises the *lookup*, not the fallback branch; the fallback is unreachable
        // through `hs_model::power_levels::PowerLevels::default()` on purpose (see that type),
        // so this test's job is to prove the plumbing reads it rather than hard-coding 50 itself.
        let pls = ctx.power_levels.expect("power levels should be Some");
        assert_eq!(pls.notifications.room, Int::from(50));
    }
}
