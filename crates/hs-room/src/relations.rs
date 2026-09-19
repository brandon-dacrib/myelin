//! `m.relates_to` indexing and bundled aggregations.
//!
//! Every persisted event whose `content.m.relates_to` names an `event_id` and `rel_type` is
//! recorded in [`crate::persist::Tables::relations`] so `/relations` can page through a target
//! event's children without scanning the whole timeline, and so [`bundle`] can compute the
//! `unsigned.m.relations` summary the spec asks every event to carry when it has children.
//!
//! Three `rel_type`s get first-class bundling, matching the client-server spec's "Aggregations"
//! module: `m.replace` (edits -- the bundle is just the latest edit's event ID and
//! `origin_server_ts`), `m.annotation` (reactions -- counted per annotation `key`), and
//! `m.thread` (the bundle carries the latest event, a count, and whether the requesting user has
//! participated). Any other `rel_type` is still indexed (so `/relations?rel_type=...` works for
//! it) but is not bundled into `unsigned`.

use hs_model::ids::{EventSn, RoomSn};

/// `(RoomSn, target_event_sn, rel_type, child_event_sn) -> b""`.
pub type RelationKey = (RoomSn, EventSn, String, EventSn);

/// One relation extracted from an event's `content.m.relates_to`, if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The event this one relates to.
    pub target: ruma::OwnedEventId,
    /// `content.m.relates_to.rel_type`. Absent (a bare `m.in_reply_to`, the only relation shape
    /// that predates `rel_type`) is represented as `"m.in_reply_to"` so callers have one type to
    /// switch on.
    pub rel_type: String,
    /// `content.m.relates_to.key`, for `m.annotation` (the reaction key, e.g. an emoji).
    pub key: Option<String>,
}

/// Extracts the relation named in an event's content, if any.
#[must_use]
pub fn relation_of(content: &serde_json::Value) -> Option<Relation> {
    let rel = content.get("m.relates_to")?.as_object()?;
    if let Some(in_reply_to) = rel.get("m.in_reply_to").and_then(|v| v.as_object()) {
        let target = in_reply_to.get("event_id")?.as_str()?;
        return Some(Relation {
            target: ruma::EventId::parse(target).ok()?,
            rel_type: "m.in_reply_to".to_owned(),
            key: None,
        });
    }
    let target = rel.get("event_id")?.as_str()?;
    let rel_type = rel.get("rel_type")?.as_str()?.to_owned();
    let key = rel.get("key").and_then(|v| v.as_str()).map(str::to_owned);
    Some(Relation {
        target: ruma::EventId::parse(target).ok()?,
        rel_type,
        key,
    })
}

/// One child event as needed to compute a bundle: its ID, sender, `rel_type`/`key`, and
/// `origin_server_ts`.
#[derive(Debug, Clone)]
pub struct ChildEvent {
    /// The child event's ID.
    pub event_id: ruma::OwnedEventId,
    /// The child event's sender.
    pub sender: ruma::OwnedUserId,
    /// The relation this child carries.
    pub relation: Relation,
    /// The child event's `origin_server_ts`.
    pub origin_server_ts: i64,
}

/// The `unsigned.m.relations` bundle for one target event, computed from its children.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Bundle {
    /// `m.replace`: the latest edit, if any.
    #[serde(rename = "m.replace", skip_serializing_if = "Option::is_none")]
    pub replace: Option<serde_json::Value>,
    /// `m.annotation` (Synapse/MSC2677 shape: counts per `(rel_type, key)`).
    #[serde(rename = "m.annotation", skip_serializing_if = "Option::is_none")]
    pub annotation: Option<serde_json::Value>,
    /// `m.thread`.
    #[serde(rename = "m.thread", skip_serializing_if = "Option::is_none")]
    pub thread: Option<serde_json::Value>,
}

/// Computes the bundle for a target event from its children, in the order the children were
/// persisted (ascending `origin_server_ts`/timeline order is what callers should pass).
///
/// `root_sender` is the target (thread root) event's own sender, if known -- needed for
/// `m.thread`'s `current_user_participated`, which the spec defines as true for *either* the
/// sender of a threaded reply *or* the sender of the thread root itself
/// (`refs/matrix-spec/content/client-server-api/modules/threading.md`, CC-BY-4.0: "The `sender`
/// of the thread root event" is rule 1, listed before "the `sender` of an event which references
/// the thread root"). `None` (the root is not resident, or genuinely has no sender on record)
/// falls back to rule 2 alone.
#[must_use]
pub fn bundle(
    children: &[ChildEvent],
    requesting_user: &ruma::UserId,
    root_sender: Option<&ruma::UserId>,
) -> Bundle {
    let mut out = Bundle::default();

    if let Some(latest_edit) = children
        .iter()
        .filter(|c| c.relation.rel_type == "m.replace")
        .max_by_key(|c| c.origin_server_ts)
    {
        out.replace = Some(serde_json::json!({
            "event_id": latest_edit.event_id.to_string(),
            "origin_server_ts": latest_edit.origin_server_ts,
            "sender": latest_edit.sender.to_string(),
        }));
    }

    let annotations: Vec<&ChildEvent> = children
        .iter()
        .filter(|c| c.relation.rel_type == "m.annotation")
        .collect();
    if !annotations.is_empty() {
        let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
        for a in &annotations {
            *counts
                .entry(a.relation.key.clone().unwrap_or_default())
                .or_insert(0) += 1;
        }
        let chunk: Vec<serde_json::Value> = counts
            .into_iter()
            .map(|(key, count)| serde_json::json!({"type": "m.annotation", "key": key, "count": count}))
            .collect();
        out.annotation = Some(serde_json::json!({"chunk": chunk}));
    }

    let threads: Vec<&ChildEvent> = children
        .iter()
        .filter(|c| c.relation.rel_type == "m.thread")
        .collect();
    if let Some(latest) = threads.iter().max_by_key(|c| c.origin_server_ts) {
        let participated = root_sender == Some(requesting_user)
            || threads.iter().any(|c| c.sender == requesting_user);
        out.thread = Some(serde_json::json!({
            "latest_event": {
                "event_id": latest.event_id.to_string(),
                "origin_server_ts": latest.origin_server_ts,
                "sender": latest.sender.to_string(),
            },
            "count": threads.len(),
            "current_user_participated": participated,
        }));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{event_id, user_id};

    fn child(rel_type: &str, key: Option<&str>, sender: &ruma::UserId, ts: i64) -> ChildEvent {
        ChildEvent {
            event_id: event_id!("$c:hs1").to_owned(),
            sender: sender.to_owned(),
            relation: Relation {
                target: event_id!("$t:hs1").to_owned(),
                rel_type: rel_type.to_owned(),
                key: key.map(str::to_owned),
            },
            origin_server_ts: ts,
        }
    }

    #[test]
    fn relation_of_extracts_rel_type_and_target() {
        let content = serde_json::json!({
            "m.relates_to": {"rel_type": "m.annotation", "event_id": "$t:hs1", "key": "👍"}
        });
        let rel = relation_of(&content).unwrap();
        assert_eq!(rel.rel_type, "m.annotation");
        assert_eq!(rel.key.as_deref(), Some("👍"));
    }

    #[test]
    fn in_reply_to_has_no_rel_type_field_but_is_recognized() {
        let content = serde_json::json!({
            "m.relates_to": {"m.in_reply_to": {"event_id": "$t:hs1"}}
        });
        let rel = relation_of(&content).unwrap();
        assert_eq!(rel.rel_type, "m.in_reply_to");
    }

    #[test]
    fn bundle_picks_latest_edit_and_counts_annotations() {
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let children = vec![
            child("m.replace", None, alice, 1),
            child("m.replace", None, alice, 5),
            child("m.annotation", Some("👍"), alice, 2),
            child("m.annotation", Some("👍"), bob, 3),
            child("m.annotation", Some("🎉"), bob, 4),
        ];
        let b = bundle(&children, alice, None);
        assert_eq!(b.replace.unwrap()["origin_server_ts"], 5);
        let chunk = b.annotation.unwrap()["chunk"].clone();
        assert_eq!(chunk.as_array().unwrap().len(), 2);
    }

    #[test]
    fn bundle_reports_thread_participation() {
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let children = vec![child("m.thread", None, bob, 1)];
        let b = bundle(&children, alice, Some(bob));
        let thread = b.thread.unwrap();
        assert_eq!(thread["count"], 1);
        assert_eq!(
            thread["current_user_participated"], false,
            "alice neither started nor replied to this thread"
        );

        let children = vec![child("m.thread", None, alice, 1)];
        let b = bundle(&children, alice, Some(bob));
        assert_eq!(
            b.thread.unwrap()["current_user_participated"],
            true,
            "alice replied to the thread, even though bob started it"
        );
    }

    /// Threading module rule 1 ("The `sender` of the thread root event"): a user who started a
    /// thread but never replied to it still counts as having participated.
    #[test]
    fn bundle_counts_the_thread_root_sender_as_participating_even_without_a_reply() {
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let children = vec![child("m.thread", None, bob, 1)];
        let b = bundle(&children, alice, Some(alice));
        assert_eq!(
            b.thread.unwrap()["current_user_participated"],
            true,
            "alice started the thread, even though only bob has replied so far"
        );
    }
}
