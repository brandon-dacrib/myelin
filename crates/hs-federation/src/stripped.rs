//! Stripped state: the few state events that describe a room to a server that is not in it.
//!
//! Two handshakes hand it across: an invite carries the room's stripped state to the invitee's
//! server (`invite_room_state`), and a resident answers `send_knock` with it
//! (`knock_room_state`). Either way the receiving server's user has not joined and cannot read
//! the room, and this is all their client has to show them what they were invited to or knocked
//! on: its name, avatar, topic, join rule, whether it is encrypted, and who asked.
//!
//! A stripped state event carries exactly four properties -- `type`, `state_key`, `sender` and
//! `content` -- per the client-server API's "Stripped state".

use serde_json::{Value, json};

/// The state event types stripped state carries, from the client-server API's list ("Stripped
/// state should contain some or all of the following"). `m.room.create` is required there as of
/// Matrix v1.16.
pub const STRIPPED_STATE_TYPES: &[&str] = &[
    "m.room.create",
    "m.room.join_rules",
    "m.room.canonical_alias",
    "m.room.name",
    "m.room.avatar",
    "m.room.topic",
    "m.room.encryption",
];

/// The most stripped state events kept from what another server hands over. Stripped state
/// describes a room in a handful of events ([`STRIPPED_STATE_TYPES`] and a member or two); a list
/// longer than this is cut here rather than stored with the invite or knock and served to a
/// client.
pub const MAX_RECEIVED_STRIPPED_STATE: usize = 64;

/// One event as a stripped state event: its four allowed properties, nothing else.
#[must_use]
pub fn strip(event: &Value) -> Value {
    json!({
        "type": event.get("type").cloned().unwrap_or(Value::Null),
        "state_key": event.get("state_key").cloned().unwrap_or(Value::Null),
        "sender": event.get("sender").cloned().unwrap_or(Value::Null),
        "content": event.get("content").cloned().unwrap_or_else(|| json!({})),
    })
}

/// The stripped form of `state` (a room's current state, one event per key): every event of a
/// [`STRIPPED_STATE_TYPES`] type, plus the `m.room.member` events of `members` -- the inviter,
/// so "Alice invited you" can name Alice. Events of other types are left out.
#[must_use]
pub fn stripped_state<'a>(
    state: impl IntoIterator<Item = &'a Value>,
    members: &[&str],
) -> Vec<Value> {
    state
        .into_iter()
        .filter(|event| {
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
            if STRIPPED_STATE_TYPES.contains(&event_type) {
                return true;
            }
            event_type == "m.room.member"
                && event
                    .get("state_key")
                    .and_then(Value::as_str)
                    .is_some_and(|key| members.contains(&key))
        })
        .map(strip)
        .collect()
}

/// [`stripped_state`]'s events of `state`, whole: what a resident answers `send_knock` with as
/// `knock_room_state` (MSC4311: full PDUs, which the knocking server can verify).
#[must_use]
pub fn stripped_state_events<'a>(
    state: impl IntoIterator<Item = &'a Value>,
    members: &[&str],
) -> Vec<Value> {
    state
        .into_iter()
        .filter(|event| {
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
            STRIPPED_STATE_TYPES.contains(&event_type)
                || (event_type == "m.room.member"
                    && event
                        .get("state_key")
                        .and_then(Value::as_str)
                        .is_some_and(|key| members.contains(&key)))
        })
        .cloned()
        .collect()
}

/// Keeps only the entries of a stripped-state list received from another server that are
/// shaped like stripped state events (an object with a string `type`, a string `state_key` and
/// an object `content`), reduced to the four allowed properties. What a remote server hands over
/// is shown to a local user's client as it is, so it is not trusted to be anything more, and at
/// most [`MAX_RECEIVED_STRIPPED_STATE`] entries are kept.
#[must_use]
pub fn sanitize_received(received: &[Value]) -> Vec<Value> {
    received
        .iter()
        .filter(|event| {
            event.get("type").is_some_and(Value::is_string)
                && event.get("state_key").is_some_and(Value::is_string)
                && event.get("content").is_some_and(Value::is_object)
        })
        .take(MAX_RECEIVED_STRIPPED_STATE)
        .map(strip)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_describing_types_and_the_named_members_are_kept_with_four_properties() {
        let state = [
            json!({"type": "m.room.create", "state_key": "", "sender": "@a:x", "content": {"room_version": "11"}, "event_id": "$1", "origin_server_ts": 1}),
            json!({"type": "m.room.name", "state_key": "", "sender": "@a:x", "content": {"name": "n"}}),
            json!({"type": "m.room.power_levels", "state_key": "", "sender": "@a:x", "content": {}}),
            json!({"type": "m.room.member", "state_key": "@a:x", "sender": "@a:x", "content": {"membership": "join"}}),
            json!({"type": "m.room.member", "state_key": "@c:x", "sender": "@c:x", "content": {"membership": "join"}}),
        ];
        let stripped = stripped_state(&state, &["@a:x"]);
        let types: Vec<&str> = stripped
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["m.room.create", "m.room.name", "m.room.member"]);
        for event in &stripped {
            let mut keys: Vec<&String> = event.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["content", "sender", "state_key", "type"]);
        }
    }

    #[test]
    fn received_entries_that_are_not_stripped_events_are_dropped() {
        let received = [
            json!({"type": "m.room.name", "state_key": "", "sender": "@a:x", "content": {"name": "n"}, "extra": 1}),
            json!({"type": "m.room.name", "content": {"name": "no state key"}}),
            json!("not an object"),
            json!({"type": 5, "state_key": "", "content": {}}),
        ];
        let kept = sanitize_received(&received);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].get("extra").is_none());
    }

    #[test]
    fn at_most_a_bounded_number_of_received_entries_are_kept() {
        let received: Vec<Value> = (0..1000)
            .map(|i| json!({"type": "m.room.name", "state_key": format!("{i}"), "content": {}}))
            .collect();
        assert_eq!(
            sanitize_received(&received).len(),
            MAX_RECEIVED_STRIPPED_STATE
        );
    }
}
