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
    if event.header().flags.is_redacted() {
        event
            .redacted_json()
            .unwrap_or_else(|_| event.json().clone())
    } else {
        event.json().clone()
    }
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
    let mut value = canonical_to_json(&readable_json(event));
    if let Some(obj) = value.as_object_mut() {
        for key in [
            "signatures",
            "hashes",
            "auth_events",
            "prev_events",
            "depth",
        ] {
            obj.remove(key);
        }
        obj.insert(
            "event_id".to_owned(),
            serde_json::Value::String(event.event_id().to_string()),
        );
        obj.entry("unsigned")
            .or_insert_with(|| serde_json::json!({}));
    }
    value
}

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
