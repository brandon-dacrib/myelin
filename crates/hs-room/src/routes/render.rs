//! Rendering an [`hs_model::event::Event`] as the client-server API's event JSON shape.

use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue};
use ruma::{OwnedEventId, OwnedUserId};

use crate::relations::Bundle;

/// Round-trips a [`CanonicalJsonObject`] through its canonical bytes into a [`serde_json::Value`].
#[must_use]
pub fn canonical_to_json(obj: &CanonicalJsonObject) -> serde_json::Value {
    let bytes = CanonicalJsonValue::Object(obj.clone()).to_canonical_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// The JSON a client should be shown for `event`: its redacted form if it has been redacted,
/// its own form otherwise. Factored out because `unsigned.prev_content` has to make the same
/// choice about the *replaced* event that [`client_event_json`] makes about the event itself --
/// a state event that was redacted after it was superseded must not leak its pre-redaction
/// content back to a client through the `prev_content` of whatever replaced it.
fn readable_json(event: &Event) -> CanonicalJsonObject {
    if event.header().flags.is_redacted() || self_destructed(event, now_ms()) {
        event
            .redacted_json()
            .unwrap_or_else(|_| event.json().clone())
    } else {
        event.json().clone()
    }
}

/// MSC2228's content key: the time (milliseconds since the Unix epoch) after which the sender
/// wants the event's content gone.
pub const SELF_DESTRUCT_AFTER: &str = "org.matrix.self_destruct_after";

/// Milliseconds since the Unix epoch, now.
fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Whether `event` is an ephemeral message (MSC2228) whose time is up at `now_ms`: its content
/// carries an integer [`SELF_DESTRUCT_AFTER`] at or before `now_ms`. Such an event is shown
/// redacted from then on, by every read that renders it here -- `/messages`, `/context`,
/// `/event`, `/sync`, the admin API -- as Synapse expires it (Sytest's "Ephemeral messages
/// received from clients are correctly expired"). Applied on read, so it holds across restarts
/// with no job to run late; the stored event keeps its content, as a redacted one does.
#[must_use]
pub fn self_destructed(event: &Event, now_ms: i64) -> bool {
    event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|c| c.get(SELF_DESTRUCT_AFTER))
        .is_some_and(|v| matches!(v, CanonicalJsonValue::Integer(at) if *at <= now_ms))
}

/// The state event that one state event replaced, in the form [`attach_replaced_state`] needs to
/// render it -- resolved by the room actor, which is the only thing that can answer "what was the
/// current state for this `(type, state_key)` at the point this event was sent"
/// ([`crate::actor::RoomActor::replaced_state_for`]).
///
/// Three client-server fields come from this one lookup, and the spec gates them differently
/// (`refs/matrix-spec/data/api/client-server/definitions/client_event_without_room_id.yaml`):
/// `unsigned.replaces_state` and `unsigned.prev_sender` are "included regardless of history
/// visibility", while `unsigned.prev_content` is "only returned if ... the client has permission
/// to see the previous event". That is why `content` is an `Option` rather than this whole struct
/// being one: a reader who may not see the event that was replaced still learns that *something*
/// was replaced, and by whom, but not what it said.
#[derive(Debug, Clone)]
pub struct ReplacedState {
    /// The replaced event's ID -- `unsigned.replaces_state`.
    pub event_id: OwnedEventId,
    /// The replaced event's sender -- `unsigned.prev_sender`.
    pub sender: OwnedUserId,
    /// The replaced event's content -- `unsigned.prev_content` -- or `None` if the reader may not
    /// see the replaced event under `m.room.history_visibility`.
    pub content: Option<serde_json::Value>,
}

impl ReplacedState {
    /// Describes `replaced` for rendering. `content_visible` is the caller's history-visibility
    /// verdict on `replaced` itself (`crate::actor::RoomActor::event_visible_to`), not on the
    /// event doing the replacing.
    #[must_use]
    pub fn new(replaced: &Event, content_visible: bool) -> Self {
        let content = content_visible.then(|| {
            let obj = readable_json(replaced)
                .get("content")
                .and_then(CanonicalJsonValue::as_object)
                .cloned()
                .unwrap_or_default();
            canonical_to_json(&obj)
        });
        Self {
            event_id: replaced.event_id().to_owned(),
            sender: replaced.header().sender.clone(),
            content,
        }
    }
}

/// Attaches `unsigned.replaces_state`, `unsigned.prev_sender` and (when the reader may see it)
/// `unsigned.prev_content` to an already-rendered event. A `None` `replaced` leaves `unsigned`
/// untouched, which is the right answer for a message event (the spec returns these three only
/// for state events) and for the first state event of its `(type, state_key)` in a room, which
/// replaced nothing.
///
/// Without `prev_content` a client cannot tell a display-name change from a join: both are an
/// `m.room.member` event with `membership: "join"`, and the only thing distinguishing them is
/// what the *previous* membership event said. Element renders the first as "Alice joined the
/// room" when this field is missing, which is the failure this exists to prevent.
///
/// Composes with [`attach_transaction_id`] and [`client_event_json_bundled`] in any order -- all
/// three write into the same `unsigned` object under distinct keys.
#[must_use]
pub fn attach_replaced_state(
    mut value: serde_json::Value,
    replaced: Option<&ReplacedState>,
) -> serde_json::Value {
    if let Some(replaced) = replaced
        && let Some(unsigned) = value
            .get_mut("unsigned")
            .and_then(serde_json::Value::as_object_mut)
    {
        unsigned.insert(
            "replaces_state".to_owned(),
            serde_json::Value::String(replaced.event_id.to_string()),
        );
        unsigned.insert(
            "prev_sender".to_owned(),
            serde_json::Value::String(replaced.sender.to_string()),
        );
        if let Some(content) = &replaced.content {
            unsigned.insert("prev_content".to_owned(), content.clone());
        }
    }
    value
}

/// The client-facing JSON for one event: the redacted view if the event is redacted, with
/// `event_id` set (some room versions never carry it in the signed form) and the server-internal
/// fields (`signatures`, `hashes`, `auth_events`, `prev_events`, `depth`) stripped, plus an
/// `unsigned` object (empty unless a caller adds to it -- [`client_event_json_bundled`] does, for
/// bundled aggregations, and [`attach_replaced_state`] for `prev_content` and friends).
///
/// This deliberately takes only the event, and so cannot populate the `unsigned` fields that need
/// the room's *state history* to answer (`prev_content`, `replaces_state`, `prev_sender`). Those
/// are a second step, [`attach_replaced_state`], because the lookup behind them belongs to the
/// room actor rather than to a renderer -- see [`crate::actor::RoomActor::replaced_state_for`].
#[must_use]
pub fn client_event_json(event: &Event) -> serde_json::Value {
    let mut value = client_form(
        canonical_to_json(&readable_json(event)),
        event.event_id().as_str(),
    );
    if let Some(unsigned) = value
        .get_mut("unsigned")
        .and_then(serde_json::Value::as_object_mut)
    {
        // The stripped state an invite or knock from another server arrived with is kept on
        // the membership event (`unsigned.invite_room_state`/`knock_room_state`) so `/sync`'s
        // `invite`/`knock` section can describe a room this server holds nothing else of
        // (`hs_user::sync`, which reads it from the stored event, not from here). It is not
        // part of the event: the timeline, `/messages`, `/context` and `/event` show the event
        // without it.
        for key in STRIPPED_STATE_KEYS {
            unsigned.remove(*key);
        }
        // A redacted event says what redacted it: the redaction, as a client event, and its ID
        // under Synapse's older name (`crate::actor::redactions`, which keeps both in the
        // event). The redacted form drops `unsigned`, so they are read from the event as held.
        if event.header().flags.is_redacted() {
            unsigned.extend(redaction_unsigned(event));
        }
    }
    value
}

/// The server-internal fields a client is never shown: what a server needs to verify and place
/// an event.
const SERVER_ONLY_KEYS: &[&str] = &[
    "signatures",
    "hashes",
    "auth_events",
    "prev_events",
    "depth",
];

/// `pdu` (a PDU as JSON) in the client-server shape: the server-only fields stripped, `event_id`
/// set (most room versions never carry it in the event), an `unsigned` object, and for an
/// `m.room.redaction` of room version 11 or later -- where `redacts` moved into `content` -- its
/// `redacts` at the top level as well, as Synapse shows it, so a client that reads it where it
/// used to be finds it.
fn client_form(mut pdu: serde_json::Value, event_id: &str) -> serde_json::Value {
    if let Some(obj) = pdu.as_object_mut() {
        for key in SERVER_ONLY_KEYS {
            obj.remove(*key);
        }
        obj.insert(
            "event_id".to_owned(),
            serde_json::Value::String(event_id.to_owned()),
        );
        obj.entry("unsigned")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(room_id) = create_event_room_id(obj, event_id) {
            obj.insert("room_id".to_owned(), serde_json::Value::String(room_id));
        }
        if obj.get("type").and_then(serde_json::Value::as_str) == Some("m.room.redaction")
            && !obj.contains_key("redacts")
            && let Some(redacts) = obj
                .get("content")
                .and_then(|content| content.get("redacts"))
                .cloned()
        {
            obj.insert("redacts".to_owned(), redacts);
        }
    }
    pdu
}

/// The `room_id` a room-version-12 (MSC4291) `m.room.create` event is shown with, when the event
/// itself carries none: the room's ID *is* the create event's reference hash, so it is the event
/// ID with `!` for `$`. Every client-server read that returns events shows a create event with
/// its `room_id`, as any other event has one (Complement's
/// `TestMSC4291RoomIDAsHashOfCreateEvent_RoomIDIsOnCreateEvent`: `/state`, `/messages`,
/// `/event`, `/context`, `/state?format=event`). `None` for any other event, or one that has a
/// `room_id` already.
fn create_event_room_id(
    obj: &serde_json::Map<String, serde_json::Value>,
    event_id: &str,
) -> Option<String> {
    if obj.contains_key("room_id")
        || obj.get("type").and_then(serde_json::Value::as_str) != Some("m.room.create")
    {
        return None;
    }
    event_id.strip_prefix('$').map(|hash| format!("!{hash}"))
}

/// `unsigned.redacted_by` and `unsigned.redacted_because` for a redacted `event`, from what
/// `crate::actor::redactions` kept in it; nothing for an event redacted without a redaction
/// being named (an administrator's purge).
fn redaction_unsigned(event: &Event) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let Some(unsigned) = event
        .json()
        .get("unsigned")
        .and_then(CanonicalJsonValue::as_object)
    else {
        return out;
    };
    let Some(by) = unsigned
        .get(crate::actor::redactions::REDACTED_BY)
        .and_then(CanonicalJsonValue::as_str)
    else {
        return out;
    };
    out.insert(
        crate::actor::redactions::REDACTED_BY.to_owned(),
        serde_json::Value::String(by.to_owned()),
    );
    if let Some(because) = unsigned
        .get(crate::actor::redactions::REDACTED_BECAUSE)
        .and_then(CanonicalJsonValue::as_object)
    {
        out.insert(
            crate::actor::redactions::REDACTED_BECAUSE.to_owned(),
            client_form(canonical_to_json(because), by),
        );
    }
    out
}

/// The `unsigned` keys under which a membership event received from another server carries the
/// room's stripped state; never shown to a client as part of the event. See
/// [`client_event_json`].
pub const STRIPPED_STATE_KEYS: &[&str] = &["invite_room_state", "knock_room_state"];

/// Attaches `unsigned.transaction_id` if `txn_id` is `Some` -- the client-server API's local-echo
/// field ("Transaction identifiers": a client matches an optimistic local copy of a message it
/// sent against the real event by transaction ID). Left absent, exactly as [`client_event_json`]
/// leaves it, when `txn_id` is `None` -- either the event was never sent through a
/// `{txnId}`-suffixed endpoint, or the viewer is not the `(sender, device)` that sent it; see
/// [`crate::actor::RoomActor::transaction_id_for`]'s doc comment for exactly which case is which.
#[must_use]
pub fn attach_transaction_id(
    mut value: serde_json::Value,
    txn_id: Option<&str>,
) -> serde_json::Value {
    if let Some(txn_id) = txn_id
        && let Some(unsigned) = value
            .get_mut("unsigned")
            .and_then(serde_json::Value::as_object_mut)
    {
        unsigned.insert(
            "transaction_id".to_owned(),
            serde_json::Value::String(txn_id.to_owned()),
        );
    }
    value
}

/// [`client_event_json`], with `bundle` attached to `unsigned.m.relations` if it has any
/// aggregation to report (`crate::relations::bundle`'s bundled-aggregations module: `m.replace`,
/// `m.annotation`, `m.thread`). A `Bundle` with nothing set (the target event has no children)
/// leaves `unsigned` exactly as [`client_event_json`] would.
#[must_use]
pub fn client_event_json_bundled(event: &Event, bundle: &Bundle) -> serde_json::Value {
    let mut value = client_event_json(event);
    let is_empty =
        bundle.replace.is_none() && bundle.annotation.is_none() && bundle.thread.is_none();
    if !is_empty
        && let Some(unsigned) = value
            .get_mut("unsigned")
            .and_then(serde_json::Value::as_object_mut)
        && let Ok(bundle_json) = serde_json::to_value(bundle)
    {
        unsigned.insert("m.relations".to_owned(), bundle_json);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::RoomVersionId;
    use serde_json::json;

    /// MSC4291: a version-12 create event carries no `room_id`; a client is shown the room's ID
    /// on it all the same, the event ID with `!` for `$`.
    #[test]
    fn a_version_12_create_event_is_shown_with_its_room_id() {
        let create = json!({
            "type": "m.room.create",
            "sender": "@creator:example.org",
            "origin_server_ts": 1,
            "depth": 1,
            "state_key": "",
            "content": {"room_version": "12"},
            "prev_events": [],
            "auth_events": [],
        });
        let event = Event::parse(&create, RoomVersionId::V12).unwrap();
        assert!(!event.json().contains_key("room_id"));
        let rendered = client_event_json(&event);
        let event_id = event.event_id().as_str();
        assert_eq!(
            rendered["room_id"].as_str().unwrap(),
            format!("!{}", &event_id[1..])
        );
        assert_eq!(rendered["event_id"], event_id);
    }

    /// An event that names its room keeps the room it names.
    #[test]
    fn an_older_create_event_keeps_its_own_room_id() {
        let create = json!({
            "event_id": "$a:example.org",
            "room_id": "!r:example.org",
            "type": "m.room.create",
            "sender": "@creator:example.org",
            "origin_server_ts": 1,
            "depth": 1,
            "state_key": "",
            "content": {"creator": "@creator:example.org"},
            "prev_events": [],
            "auth_events": [],
        });
        let event = Event::parse(&create, RoomVersionId::V1).unwrap();
        assert_eq!(client_event_json(&event)["room_id"], "!r:example.org");
    }
}
