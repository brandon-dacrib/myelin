//! Rendering an [`hs_model::event::Event`] as the client-server API's event JSON shape.

use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue};

/// Round-trips a [`CanonicalJsonObject`] through its canonical bytes into a [`serde_json::Value`].
#[must_use]
pub fn canonical_to_json(obj: &CanonicalJsonObject) -> serde_json::Value {
    let bytes = CanonicalJsonValue::Object(obj.clone()).to_canonical_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// The client-facing JSON for one event: the redacted view if the event is redacted, with
/// `event_id` set (some room versions never carry it in the signed form) and the server-internal
/// fields (`signatures`, `hashes`, `auth_events`, `prev_events`, `depth`) stripped, plus an
/// `unsigned` object (empty unless a caller adds to it -- `crate::routes::relations::bundle_into`
/// does, for bundled aggregations).
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
