//! What a shared integrations bridge has connected, read from room state
//! (`GET /bridge-offerings/{type}/connections`, [`crate::model::BridgeConnection`]).
//!
//! Hookshot keeps each connection as one state event in the room it posts into, of a type under
//! `uk.half-shot.matrix-hookshot.` with the connection's name as the state key: a generic
//! webhook is `uk.half-shot.matrix-hookshot.generic.hook` with `{"name": ...,
//! "transformationFunction": ...}`, a GitHub repository `uk.half-shot.matrix-hookshot.github.repository`
//! with `{"org": ..., "repo": ...}`, a feed `uk.half-shot.matrix-hookshot.feed` with `{"url":
//! ...}`. Removing a connection empties the event's content. A webhook's secret id (the last part
//! of its URL) is in the bridge's own room account data, not in the state, so it is not read
//! here. Hookshot 7 has no provisioning API, so this is the only way to see what it bridges
//! (read from the 7.5.0 bridge on the demo, 2026-10-20).

use serde_json::Value;

use crate::model::BridgeConnection;

/// The prefix of every Hookshot connection's state event type.
pub const HOOKSHOT_EVENT_PREFIX: &str = "uk.half-shot.matrix-hookshot.";

/// One state event of a room, as far as a connection is read from it.
#[derive(Debug, Clone)]
pub struct StateEvent {
    pub event_type: String,
    pub state_key: String,
    pub content: Value,
}

/// The connections among `events`, the state of room `room_id` (named `room_name`), in the
/// order given. Events of other types are ignored.
#[must_use]
pub fn connections_in(
    room_id: &str,
    room_name: Option<&str>,
    events: &[StateEvent],
) -> Vec<BridgeConnection> {
    events
        .iter()
        .filter_map(|e| {
            let suffix = e.event_type.strip_prefix(HOOKSHOT_EVENT_PREFIX)?;
            let removed = e.content.as_object().is_none_or(serde_json::Map::is_empty);
            Some(BridgeConnection {
                service: service_of(suffix).to_owned(),
                event_type: e.event_type.clone(),
                state_key: e.state_key.clone(),
                room_id: room_id.to_owned(),
                room_name: room_name.map(str::to_owned),
                name: name_of(suffix, &e.state_key, &e.content),
                state: if removed { "removed" } else { "active" }.to_owned(),
                content: e.content.clone(),
            })
        })
        .collect()
}

/// The room's name from its state (`m.room.name`), if it has one.
#[must_use]
pub fn room_name_in(events: &[StateEvent]) -> Option<String> {
    events
        .iter()
        .find(|e| e.event_type == "m.room.name" && e.state_key.is_empty())
        .and_then(|e| e.content["name"].as_str())
        .filter(|n| !n.trim().is_empty())
        .map(str::to_owned)
}

/// The service a connection type's suffix belongs to, in the admin API's words.
fn service_of(suffix: &str) -> &str {
    match suffix.split('.').next().unwrap_or(suffix) {
        "generic" => "webhook",
        "github" => "github",
        "gitlab" => "gitlab",
        "jira" => "jira",
        "feed" => "feed",
        "figma" => "figma",
        "openproject" => "openproject",
        other => other,
    }
}

/// What to call a connection: a webhook's `name`, a repository's `org/repo` (GitHub) or
/// `instance` and `path` (GitLab), a Jira project's `url`, a feed's `url` (its `label` when it
/// has one), else the state key.
fn name_of(suffix: &str, state_key: &str, content: &Value) -> String {
    let s = |key: &str| {
        content[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let name = match suffix.split('.').next().unwrap_or(suffix) {
        "generic" => s("name"),
        "github" => match (s("org"), s("repo")) {
            (Some(org), Some(repo)) => Some(format!("{org}/{repo}")),
            _ => None,
        },
        "gitlab" => s("path").or_else(|| s("instance")),
        "feed" => s("label").or_else(|| s("url")),
        _ => s("name").or_else(|| s("url")),
    };
    name.unwrap_or_else(|| state_key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(event_type: &str, state_key: &str, content: Value) -> StateEvent {
        StateEvent {
            event_type: event_type.to_owned(),
            state_key: state_key.to_owned(),
            content,
        }
    }

    #[test]
    fn a_webhook_a_repository_and_a_feed_are_read_and_other_state_is_not() {
        let events = vec![
            ev("m.room.name", "", json!({"name": "Deploys"})),
            ev("m.room.create", "", json!({"room_version": "10"})),
            ev(
                "uk.half-shot.matrix-hookshot.generic.hook",
                "myelin",
                json!({"name": "myelin", "transformationFunction": "return {};"}),
            ),
            ev(
                "uk.half-shot.matrix-hookshot.github.repository",
                "brandon-dacrib/myelin",
                json!({"org": "brandon-dacrib", "repo": "myelin"}),
            ),
            ev(
                "uk.half-shot.matrix-hookshot.feed",
                "https://example.org/feed.xml",
                json!({"url": "https://example.org/feed.xml"}),
            ),
        ];
        let name = room_name_in(&events);
        let found = connections_in("!r:example.org", name.as_deref(), &events);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].service, "webhook");
        assert_eq!(found[0].name, "myelin");
        assert_eq!(found[0].room_name.as_deref(), Some("Deploys"));
        assert_eq!(found[0].state, "active");
        assert_eq!(found[1].service, "github");
        assert_eq!(found[1].name, "brandon-dacrib/myelin");
        assert_eq!(found[2].service, "feed");
        assert_eq!(found[2].name, "https://example.org/feed.xml");
        // The webhook's id is not in the state, and so not here.
        assert!(found[0].content.get("hookId").is_none());
    }

    #[test]
    fn an_emptied_connection_is_removed_and_named_by_its_state_key() {
        let events = vec![ev(
            "uk.half-shot.matrix-hookshot.generic.hook",
            "old",
            json!({}),
        )];
        let found = connections_in("!r:example.org", None, &events);
        assert_eq!(found[0].state, "removed");
        assert_eq!(found[0].name, "old");
        assert!(found[0].room_name.is_none());
    }

    #[test]
    fn an_unknown_service_keeps_its_own_word() {
        let events = vec![ev(
            "uk.half-shot.matrix-hookshot.challengehound.activity",
            "x",
            json!({"name": "run"}),
        )];
        let found = connections_in("!r:example.org", None, &events);
        assert_eq!(found[0].service, "challengehound");
        assert_eq!(found[0].name, "run");
    }
}
