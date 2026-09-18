//! The redaction algorithm, per room version.
//!
//! Redaction strips an event down to the fields the spec calls "essential": enough to keep the
//! event graph, authorization and state intact, while discarding everything a moderator or the
//! sender might want removed. Which fields count as essential has grown a few times across room
//! versions ([`crate::room_version::RedactionRules`]); this module applies whichever set the
//! room's version specifies.
//!
//! Re-expressed from the algorithm in the Matrix specification ("Redactions",
//! `refs/matrix-spec/content/client-server-api/modules/redaction.md`, Apache-2.0) and from
//! `ruma_common::canonical_json::redaction` (`refs/ruma/crates/ruma-common/src/canonical_json/redaction.rs`,
//! MIT license, Ruma project), which this module's tests cross-check against.

use crate::canonical::{CanonicalJsonObject, CanonicalJsonValue};
use crate::error::RedactionError;
use crate::room_version::RedactionRules;

/// Redacts an event's JSON object, returning a new, smaller object.
///
/// This keeps the top-level fields every room version retains
/// (`event_id`, `type`, `room_id`, `sender`, `state_key`, `hashes`, `signatures`, `depth`,
/// `prev_events`, `auth_events`, `origin_server_ts`), the version-gated legacy fields (`origin`,
/// `membership`, `prev_state`), and a `content` object filtered down to the keys the event's
/// `type` is allowed to keep after redaction.
///
/// This does *not* strip `signatures` or `unsigned`: they are part of the standard redaction
/// algorithm's output (a redacted event a client displays is still signed). Callers computing the
/// reference hash of a redacted event must strip those themselves; see [`crate::hash`].
///
/// # Errors
/// Returns [`RedactionError::MissingType`] if `event` has no string `type` field, or
/// [`RedactionError::ContentNotObject`] if it has a `content` field that is not an object.
pub fn redact(
    event: &CanonicalJsonObject,
    rules: &RedactionRules,
) -> Result<CanonicalJsonObject, RedactionError> {
    let event_type = event
        .get("type")
        .and_then(CanonicalJsonValue::as_str)
        .ok_or(RedactionError::MissingType)?
        .to_owned();

    let mut out = CanonicalJsonObject::new();
    for (key, value) in event {
        if !retain_top_level_key(key, rules) {
            continue;
        }
        if key == "content" {
            let content = value.as_object().ok_or(RedactionError::ContentNotObject)?;
            out.insert(
                key.clone(),
                CanonicalJsonValue::Object(redact_content(content, &event_type, rules)),
            );
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Ok(out)
}

/// Redacts just an event's `content`, given its `type`. Used when the caller already has the
/// content object in hand (for example, comparing "what would this event's content look like
/// redacted" without a full event wrapper).
#[must_use]
pub fn redact_content(
    content: &CanonicalJsonObject,
    event_type: &str,
    rules: &RedactionRules,
) -> CanonicalJsonObject {
    match event_type {
        "m.room.member" => redact_room_member_content(content, rules),
        "m.room.create" => {
            if rules.keep_room_create_content {
                content.clone()
            } else {
                retain_only(content, &["creator"])
            }
        }
        "m.room.join_rules" => {
            if rules.keep_room_join_rules_allow {
                retain_only(content, &["join_rule", "allow"])
            } else {
                retain_only(content, &["join_rule"])
            }
        }
        "m.room.power_levels" => {
            const ALWAYS: &[&str] = &[
                "ban",
                "events",
                "events_default",
                "kick",
                "redact",
                "state_default",
                "users",
                "users_default",
            ];
            if rules.keep_room_power_levels_invite {
                let mut keys = ALWAYS.to_vec();
                keys.push("invite");
                retain_only(content, &keys)
            } else {
                retain_only(content, ALWAYS)
            }
        }
        "m.room.history_visibility" => retain_only(content, &["history_visibility"]),
        "m.room.redaction" => {
            if rules.keep_room_redaction_redacts {
                retain_only(content, &["redacts"])
            } else {
                CanonicalJsonObject::new()
            }
        }
        "m.room.aliases" => {
            if rules.keep_room_aliases_aliases {
                retain_only(content, &["aliases"])
            } else {
                CanonicalJsonObject::new()
            }
        }
        _ => CanonicalJsonObject::new(),
    }
}

/// Whether a top-level event field survives redaction, regardless of event type.
fn retain_top_level_key(key: &str, rules: &RedactionRules) -> bool {
    match key {
        "content" => true,
        "event_id" | "type" | "room_id" | "sender" | "state_key" | "hashes" | "signatures"
        | "depth" | "prev_events" | "auth_events" | "origin_server_ts" => true,
        "origin" | "membership" | "prev_state" => rules.keep_origin_membership_prev_state,
        _ => false,
    }
}

/// Builds a new object containing only the given keys of `content`, if present.
fn retain_only(content: &CanonicalJsonObject, keys: &[&str]) -> CanonicalJsonObject {
    content
        .iter()
        .filter(|(k, _)| keys.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `m.room.member` content redaction: `membership` is always kept; `join_authorised_via_users_server`
/// and `third_party_invite.signed` are version-gated.
fn redact_room_member_content(
    content: &CanonicalJsonObject,
    rules: &RedactionRules,
) -> CanonicalJsonObject {
    let mut out = CanonicalJsonObject::new();
    for (key, value) in content {
        match key.as_str() {
            "membership" => {
                out.insert(key.clone(), value.clone());
            }
            "join_authorised_via_users_server"
                if rules.keep_room_member_join_authorised_via_users_server =>
            {
                out.insert(key.clone(), value.clone());
            }
            "third_party_invite" if rules.keep_room_member_third_party_invite_signed => {
                let filtered = match value.as_object() {
                    Some(tpi) => CanonicalJsonValue::Object(retain_only(tpi, &["signed"])),
                    // Malformed (non-object) values are retained as-is, matching the reference
                    // implementation's behavior of only filtering objects.
                    None => value.clone(),
                };
                out.insert(key.clone(), filtered);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::to_canonical_object;
    use serde_json::json;

    fn redact_json(value: serde_json::Value, rules: &RedactionRules) -> CanonicalJsonObject {
        let obj = to_canonical_object(&value, true).unwrap();
        redact(&obj, rules).unwrap()
    }

    #[test]
    fn missing_type_is_an_error() {
        let obj = to_canonical_object(&json!({"content": {}}), true).unwrap();
        assert_eq!(
            redact(&obj, &RedactionRules::V1),
            Err(RedactionError::MissingType)
        );
    }

    #[test]
    fn content_must_be_an_object() {
        let obj =
            to_canonical_object(&json!({"type": "m.room.message", "content": 5}), true).unwrap();
        assert_eq!(
            redact(&obj, &RedactionRules::V1),
            Err(RedactionError::ContentNotObject)
        );
    }

    #[test]
    fn v1_strips_message_content_entirely() {
        let out = redact_json(
            json!({
                "type": "m.room.message",
                "event_id": "$a:x",
                "room_id": "!r:x",
                "sender": "@u:x",
                "origin_server_ts": 1,
                "content": {"body": "hello", "msgtype": "m.text"},
                "prev_events": [],
                "auth_events": [],
            }),
            &RedactionRules::V1,
        );
        assert_eq!(
            out.get("content"),
            Some(&CanonicalJsonValue::Object(CanonicalJsonObject::new()))
        );
        assert!(out.contains_key("event_id"));
        assert!(out.contains_key("room_id"));
    }

    #[test]
    fn v1_keeps_room_aliases_aliases_v11_drops_it() {
        let event = json!({
            "type": "m.room.aliases",
            "content": {"aliases": ["#a:x"]},
        });
        let v1 = redact_json(event.clone(), &RedactionRules::V1);
        assert!(
            v1.get("content")
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("aliases")
        );

        let v11 = redact_json(event, &RedactionRules::V11);
        assert!(v11.get("content").unwrap().as_object().unwrap().is_empty());
    }

    #[test]
    fn v11_keeps_full_create_content_and_redaction_redacts() {
        let create = json!({
            "type": "m.room.create",
            "content": {"creator": "@u:x", "room_version": "11", "m.federate": false},
        });
        let out = redact_json(create, &RedactionRules::V11);
        let content = out.get("content").unwrap().as_object().unwrap();
        assert!(content.contains_key("room_version"));
        assert!(content.contains_key("m.federate"));

        let redaction =
            json!({"type": "m.room.redaction", "content": {"redacts": "$x:x", "reason": "spam"}});
        let out = redact_json(redaction, &RedactionRules::V11);
        let content = out.get("content").unwrap().as_object().unwrap();
        assert!(content.contains_key("redacts"));
        assert!(!content.contains_key("reason"));
    }

    #[test]
    fn v9_keeps_join_authorised_via_users_server() {
        let member = json!({
            "type": "m.room.member",
            "content": {
                "membership": "join",
                "join_authorised_via_users_server": "@auth:x",
                "displayname": "should be dropped",
            },
        });
        let out = redact_json(member, &RedactionRules::V9);
        let content = out.get("content").unwrap().as_object().unwrap();
        assert!(content.contains_key("membership"));
        assert!(content.contains_key("join_authorised_via_users_server"));
        assert!(!content.contains_key("displayname"));
    }

    #[test]
    fn origin_membership_prev_state_gated_by_version() {
        let event = json!({
            "type": "m.room.message",
            "origin": "example.org",
            "membership": "join",
            "prev_state": [],
            "content": {},
        });
        let v1 = redact_json(event.clone(), &RedactionRules::V1);
        assert!(v1.contains_key("origin"));
        assert!(v1.contains_key("membership"));
        assert!(v1.contains_key("prev_state"));

        let v11 = redact_json(event, &RedactionRules::V11);
        assert!(!v11.contains_key("origin"));
        assert!(!v11.contains_key("membership"));
        assert!(!v11.contains_key("prev_state"));
    }

    /// Cross-check against `ruma_common::canonical_json::redaction::redact` for every room
    /// version's rules, on a representative event of each redaction-sensitive type.
    #[test]
    fn cross_check_against_ruma_redaction() {
        use ruma::room_version_rules::RoomVersionRules as RumaRules;

        let events = [
            json!({"type": "m.room.create", "content": {"creator": "@u:x", "room_version": "11"}, "event_id": "$a:x"}),
            json!({"type": "m.room.member", "content": {"membership": "invite", "join_authorised_via_users_server": "@a:x", "third_party_invite": {"signed": {"token": "t"}, "display_name": "x"}}, "state_key": "@b:x"}),
            json!({"type": "m.room.join_rules", "content": {"join_rule": "restricted", "allow": [{"type": "m.room_membership"}]}}),
            json!({"type": "m.room.power_levels", "content": {"ban": 50, "invite": 0, "users": {"@a:x": 100}}}),
            json!({"type": "m.room.history_visibility", "content": {"history_visibility": "joined", "extra": 1}}),
            json!({"type": "m.room.redaction", "content": {"redacts": "$x:x", "reason": "spam"}, "redacts": "$x:x"}),
            json!({"type": "m.room.aliases", "content": {"aliases": ["#a:x"]}}),
            json!({"type": "m.room.message", "content": {"body": "hi"}, "origin": "x", "membership": "join", "prev_state": []}),
        ];

        let versions = [
            ("1", RedactionRules::V1, RumaRules::V1),
            ("6", RedactionRules::V6, RumaRules::V6),
            ("8", RedactionRules::V8, RumaRules::V8),
            ("9", RedactionRules::V9, RumaRules::V9),
            ("11", RedactionRules::V11, RumaRules::V11),
        ];

        for (name, ours_rules, ruma_rules) in versions {
            for event in &events {
                let ours = redact_json(event.clone(), &ours_rules);
                let ours_bytes = CanonicalJsonValue::Object(ours).to_canonical_bytes();

                let ruma_obj: ruma::CanonicalJsonObject =
                    serde_json::from_value(event.clone()).unwrap();
                let theirs =
                    ruma::canonical_json::redact(ruma_obj, &ruma_rules.redaction, None).unwrap();
                let theirs_str = serde_json::to_string(&theirs).unwrap();

                assert_eq!(
                    String::from_utf8(ours_bytes).unwrap(),
                    theirs_str,
                    "mismatch for room version {name}, event {event}"
                );
            }
        }
    }
}
