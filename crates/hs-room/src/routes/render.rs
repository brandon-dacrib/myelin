//! Rendering an [`hs_model::event::Event`] as the client-server API's event JSON shape.

use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue};

use crate::relations::Bundle;

/// Round-trips a [`CanonicalJsonObject`] through its canonical bytes into a [`serde_json::Value`].
#[must_use]
pub fn canonical_to_json(obj: &CanonicalJsonObject) -> serde_json::Value {
    let bytes = CanonicalJsonValue::Object(obj.clone()).to_canonical_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// The client-facing JSON for one event: the redacted view if the event is redacted, with
/// `event_id` set (some room versions never carry it in the signed form) and the server-internal
/// fields (`signatures`, `hashes`, `auth_events`, `prev_events`, `depth`) stripped, plus an
/// `unsigned` object (empty unless a caller adds to it -- [`client_event_json_bundled`] does, for
/// bundled aggregations).
#[must_use]
pub fn client_event_json(event: &Event) -> serde_json::Value {
    let source = if event.header().flags.is_redacted() {
        event
            .redacted_json()
            .unwrap_or_else(|_| event.json().clone())
    } else {
        event.json().clone()
    };
    let mut value = canonical_to_json(&source);
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
