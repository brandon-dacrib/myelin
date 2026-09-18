//! Version-aware parsing of `m.room.power_levels` content.
//!
//! Room version 10 tightened the spec to require power level values to be JSON integers
//! ([`AuthorizationRules::integer_power_levels`](crate::room_version::RoomVersionRules::integer_power_levels));
//! older room versions accept an integer, a float, or a numeric string, matching what Synapse has
//! always tolerated in the wild. Room version 12 additionally gives room creators an effectively
//! infinite power level regardless of what `m.room.power_levels` says
//! ([`explicitly_privilege_room_creators`](crate::room_version::RoomVersionRules::explicitly_privilege_room_creators),
//! MSC4289); [`EffectivePowerLevels`] layers that on top of a parsed [`PowerLevels`].
//!
//! Written from the "Power levels" section of the client-server specification
//! (`refs/matrix-spec/content/client-server-api.md`, Apache-2.0).

use std::collections::BTreeMap;

use ruma::{OwnedUserId, UserId};

use crate::canonical::{CanonicalJsonObject, CanonicalJsonValue};
use crate::error::PowerLevelsError;
use crate::room_version::RoomVersionRules;

/// The spec's default power level values, used for any field `m.room.power_levels` omits.
pub mod defaults {
    /// Default `ban`, `kick`, `redact` and `state_default`.
    pub const MODERATOR: i64 = 50;
    /// Default `events_default` and `users_default`.
    pub const MEMBER: i64 = 0;
    /// Default `invite`.
    pub const INVITE: i64 = 0;
    /// Default `notifications.room`.
    pub const NOTIFICATIONS_ROOM: i64 = 50;
}

/// A parsed, version-aware `m.room.power_levels` content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerLevels {
    /// Power required to ban.
    pub ban: i64,
    /// Power required to send an event of a given type, keyed by event `type`.
    pub events: BTreeMap<String, i64>,
    /// Power required to send a message event whose type is not in `events`.
    pub events_default: i64,
    /// Power required to invite.
    pub invite: i64,
    /// Power required to kick.
    pub kick: i64,
    /// Power required to redact another user's event.
    pub redact: i64,
    /// Power required to send a state event whose type is not in `events`.
    pub state_default: i64,
    /// Per-user power levels.
    pub users: BTreeMap<OwnedUserId, i64>,
    /// Power level a user has if absent from `users`.
    pub users_default: i64,
    /// Power required to trigger a notification type, keyed by notification type (spec defines
    /// only `"room"`, for `@room` notifications).
    pub notifications: BTreeMap<String, i64>,
}

impl Default for PowerLevels {
    fn default() -> Self {
        Self {
            ban: defaults::MODERATOR,
            events: BTreeMap::new(),
            events_default: defaults::MEMBER,
            invite: defaults::INVITE,
            kick: defaults::MODERATOR,
            redact: defaults::MODERATOR,
            state_default: defaults::MODERATOR,
            users: BTreeMap::new(),
            users_default: defaults::MEMBER,
            notifications: BTreeMap::from([("room".to_owned(), defaults::NOTIFICATIONS_ROOM)]),
        }
    }
}

impl PowerLevels {
    /// Parses an `m.room.power_levels` event's `content`, applying the room version's leniency
    /// rules for numeric fields.
    ///
    /// Fields absent from `content` take the spec defaults ([`defaults`]). An absent
    /// `m.room.power_levels` event entirely (a room with none sent) has the same defaults;
    /// callers should use [`PowerLevels::default`] for that case rather than calling this with an
    /// empty object (which behaves identically, but `default()` is clearer at call sites).
    ///
    /// # Errors
    /// Returns [`PowerLevelsError`] if `content` is not an object, a scalar field is present but
    /// not an integer (per the version's leniency), a map field (`events`, `users`,
    /// `notifications`) is present but not an object, or a `users` key is not a valid user ID.
    pub fn parse(
        content: &CanonicalJsonObject,
        rules: &RoomVersionRules,
    ) -> Result<Self, PowerLevelsError> {
        let strict = rules.integer_power_levels;
        let mut out = Self::default();

        if let Some(v) = content.get("ban") {
            out.ban = parse_level(v, strict, "ban")?;
        }
        if let Some(v) = content.get("events_default") {
            out.events_default = parse_level(v, strict, "events_default")?;
        }
        if let Some(v) = content.get("invite") {
            out.invite = parse_level(v, strict, "invite")?;
        }
        if let Some(v) = content.get("kick") {
            out.kick = parse_level(v, strict, "kick")?;
        }
        if let Some(v) = content.get("redact") {
            out.redact = parse_level(v, strict, "redact")?;
        }
        if let Some(v) = content.get("state_default") {
            out.state_default = parse_level(v, strict, "state_default")?;
        }
        if let Some(v) = content.get("users_default") {
            out.users_default = parse_level(v, strict, "users_default")?;
        }
        if let Some(v) = content.get("events") {
            out.events = parse_level_map(v, strict, "events")?;
        }
        if let Some(v) = content.get("notifications") {
            out.notifications = parse_level_map(v, strict, "notifications")?;
        }
        if let Some(v) = content.get("users") {
            let map = v
                .as_object()
                .ok_or_else(|| PowerLevelsError::NotMap("users".to_owned()))?;
            let mut users = BTreeMap::new();
            for (key, value) in map {
                let user =
                    UserId::parse(key).map_err(|_| PowerLevelsError::InvalidUserId(key.clone()))?;
                let level = parse_level(value, strict, &format!("users.{key}"))?;
                users.insert(user, level);
            }
            out.users = users;
        }

        Ok(out)
    }

    /// The power level a user has, per `users`/`users_default`. Does not account for the
    /// creator-power rule; see [`EffectivePowerLevels`] for the version-aware version auth checks
    /// should use.
    #[must_use]
    pub fn user_power(&self, user: &UserId) -> i64 {
        self.users.get(user).copied().unwrap_or(self.users_default)
    }

    /// The power level required to send an event, per `events`/`events_default`/`state_default`.
    #[must_use]
    pub fn required_power(&self, event_type: &str, is_state_event: bool) -> i64 {
        if let Some(level) = self.events.get(event_type) {
            return *level;
        }
        if is_state_event {
            self.state_default
        } else {
            self.events_default
        }
    }
}

/// Parses one power-level scalar field.
///
/// In strict mode (room version 10 and later), only a JSON integer is accepted. In lenient mode,
/// a numeric string or a float (truncated toward zero) is also accepted, matching the leeway
/// older room versions give in the wild.
fn parse_level(
    value: &CanonicalJsonValue,
    strict: bool,
    field: &str,
) -> Result<i64, PowerLevelsError> {
    match value {
        CanonicalJsonValue::Integer(i) => Ok(*i),
        CanonicalJsonValue::Float(f) if !strict => Ok(*f as i64),
        CanonicalJsonValue::String(s) if !strict => {
            s.trim()
                .parse::<i64>()
                .map_err(|_| PowerLevelsError::NotInteger {
                    field: field.to_owned(),
                    value: format!("{s:?}"),
                })
        }
        other => Err(PowerLevelsError::NotInteger {
            field: field.to_owned(),
            value: debug_render(other),
        }),
    }
}

/// Parses a `{string: level}` map field (`events` or `notifications`).
fn parse_level_map(
    value: &CanonicalJsonValue,
    strict: bool,
    field: &str,
) -> Result<BTreeMap<String, i64>, PowerLevelsError> {
    let map = value
        .as_object()
        .ok_or_else(|| PowerLevelsError::NotMap(field.to_owned()))?;
    map.iter()
        .map(|(k, v)| {
            let level = parse_level(v, strict, &format!("{field}.{k}"))?;
            Ok((k.clone(), level))
        })
        .collect()
}

fn debug_render(value: &CanonicalJsonValue) -> String {
    String::from_utf8_lossy(&value.to_canonical_bytes()).into_owned()
}

/// [`PowerLevels`] layered with the room's creator-power rule (MSC4289, room version 12).
///
/// From room version 12, the room's creator(s) always have an effectively infinite power level:
/// `m.room.power_levels` cannot demote them. This wraps a parsed [`PowerLevels`] with the set of
/// privileged creators (empty, unless the room version enables the rule); auth checks should read
/// power levels through this type rather than through [`PowerLevels`] directly.
#[derive(Debug, Clone)]
pub struct EffectivePowerLevels<'a> {
    levels: &'a PowerLevels,
    privileged_creators: Vec<OwnedUserId>,
}

impl<'a> EffectivePowerLevels<'a> {
    /// Builds an effective view. `creators` is ignored (and should be empty) unless
    /// `rules.explicitly_privilege_room_creators` is set.
    #[must_use]
    pub fn new(
        levels: &'a PowerLevels,
        rules: &RoomVersionRules,
        creators: impl IntoIterator<Item = OwnedUserId>,
    ) -> Self {
        let privileged_creators = if rules.explicitly_privilege_room_creators {
            creators.into_iter().collect()
        } else {
            Vec::new()
        };
        Self {
            levels,
            privileged_creators,
        }
    }

    /// The effective power level of a user: `i64::MAX` if they are a privileged creator,
    /// otherwise [`PowerLevels::user_power`].
    #[must_use]
    pub fn user_power(&self, user: &UserId) -> i64 {
        if self
            .privileged_creators
            .iter()
            .any(|c| AsRef::<UserId>::as_ref(c) == user)
        {
            i64::MAX
        } else {
            self.levels.user_power(user)
        }
    }

    /// The underlying parsed power levels.
    #[must_use]
    pub fn levels(&self) -> &PowerLevels {
        self.levels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::to_canonical_object;
    use crate::room_version::RoomVersionRules;
    use serde_json::json;

    fn parse(
        value: serde_json::Value,
        rules: &RoomVersionRules,
    ) -> Result<PowerLevels, PowerLevelsError> {
        let obj = to_canonical_object(&value, rules.strict_canonical_json).unwrap();
        PowerLevels::parse(&obj, rules)
    }

    #[test]
    fn defaults_match_spec() {
        let pl = PowerLevels::default();
        assert_eq!(pl.ban, 50);
        assert_eq!(pl.kick, 50);
        assert_eq!(pl.redact, 50);
        assert_eq!(pl.state_default, 50);
        assert_eq!(pl.events_default, 0);
        assert_eq!(pl.users_default, 0);
        assert_eq!(pl.invite, 0);
        assert_eq!(pl.notifications.get("room"), Some(&50));
    }

    #[test]
    fn parses_users_and_events_maps() {
        let pl = parse(
            json!({
                "users": {"@a:x": 100, "@b:x": 0},
                "events": {"m.room.name": 50},
                "state_default": 25,
            }),
            &RoomVersionRules::V11,
        )
        .unwrap();
        assert_eq!(pl.user_power(UserId::parse("@a:x").unwrap().as_ref()), 100);
        assert_eq!(pl.user_power(UserId::parse("@c:x").unwrap().as_ref()), 0);
        assert_eq!(pl.required_power("m.room.name", true), 50);
        assert_eq!(pl.required_power("m.room.topic", true), 25);
    }

    #[test]
    fn v10_rejects_string_power_levels() {
        let err = parse(json!({"ban": "50"}), &RoomVersionRules::V10).unwrap_err();
        assert!(matches!(err, PowerLevelsError::NotInteger { .. }));
    }

    #[test]
    fn v9_accepts_string_power_levels() {
        // From room version 6, `strict_canonical_json` already forbids floats anywhere in an
        // event's JSON (so a float power level cannot even reach this parser in a v9 room), but
        // `integer_power_levels` (which additionally forbids numeric *strings*) only starts at
        // v10. A v6-v9 room can still legally carry a string-typed power level value.
        let pl = parse(json!({"ban": "50"}), &RoomVersionRules::V9).unwrap();
        assert_eq!(pl.ban, 50);
    }

    #[test]
    fn v5_accepts_string_and_float_power_levels() {
        // Before room version 6, canonical JSON itself is not strictly enforced, so both forms
        // seen in the wild are accepted leniently.
        let pl = parse(json!({"ban": "50", "kick": 12.0}), &RoomVersionRules::V5).unwrap();
        assert_eq!(pl.ban, 50);
        assert_eq!(pl.kick, 12);
    }

    #[test]
    fn invalid_user_id_is_rejected() {
        let err = parse(
            json!({"users": {"not-a-user-id": 100}}),
            &RoomVersionRules::V11,
        )
        .unwrap_err();
        assert!(matches!(err, PowerLevelsError::InvalidUserId(_)));
    }

    #[test]
    fn effective_power_levels_privilege_creators_from_v12() {
        let pl = PowerLevels::default();
        let creator = UserId::parse("@creator:x").unwrap().to_owned();

        let v11 = EffectivePowerLevels::new(&pl, &RoomVersionRules::V11, [creator.clone()]);
        assert_eq!(v11.user_power(&creator), 0);

        let v12 = EffectivePowerLevels::new(&pl, &RoomVersionRules::V12, [creator.clone()]);
        assert_eq!(v12.user_power(&creator), i64::MAX);
    }
}
