//! The event wrapper: parsed PDU fields, cached canonical bytes and hashes, and the internal
//! metadata flags a homeserver tracks per event but never puts on the wire
//! (`rejected`, `soft-failed`, `redacted`, `outlier`, `partial-state` -- PLAN.md section 6.2).
//!
//! [`Event`] parses and validates a PDU against its room version's [`crate::room_version`] rules
//! once, then caches the canonical JSON bytes (so `hs-room` and `hs-federation` can serve the
//! event without re-encoding it -- PLAN.md section 3, "Parse JSON") and exposes the content hash
//! and reference hash as cheap on-demand computations over that cache. What it does *not* do is
//! authorize the event or resolve state; that is `hs-state`'s job, built on top of this type and
//! [`crate::power_levels`].

use bytes::Bytes;
use ruma::{OwnedEventId, OwnedUserId, RoomVersionId, UserId};

use crate::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use crate::error::EventError;
use crate::hash::{self, Sha256Digest};
use crate::redaction::{self};
use crate::room_version::{RoomVersion, RoomVersionRules};

/// The [maximum PDU size](https://spec.matrix.org/v1.19/client-server-api/#size-limits) allowed on
/// the wire: 64 KiB.
pub const MAX_PDU_BYTES: usize = 65_535;

/// Internal metadata flags a homeserver tracks per event, never serialized onto the wire.
///
/// Packed into a single byte, matching the "flags for rejected, soft-failed, redacted, outlier,
/// partial-state" header field PLAN.md section 6.2 describes for the on-disk event record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventFlags(u8);

impl EventFlags {
    const REJECTED: u8 = 1 << 0;
    const SOFT_FAILED: u8 = 1 << 1;
    const REDACTED: u8 = 1 << 2;
    const OUTLIER: u8 = 1 << 3;
    const PARTIAL_STATE: u8 = 1 << 4;

    /// No flags set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Decodes flags from their single-byte on-disk representation.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        Self(byte)
    }

    /// Encodes flags to their single-byte on-disk representation.
    #[must_use]
    pub const fn to_byte(self) -> u8 {
        self.0
    }

    /// The event failed authorization outright and must not be part of any room state or be
    /// served to clients or other servers.
    #[must_use]
    pub const fn is_rejected(self) -> bool {
        self.0 & Self::REJECTED != 0
    }

    /// Sets or clears [`Self::is_rejected`].
    pub fn set_rejected(&mut self, value: bool) {
        self.set(Self::REJECTED, value);
    }

    /// The event passed authorization against the state before it, but not against the room's
    /// current state at receipt time; it is stored and can become visible later (for example
    /// after further events resolve the fork), but was not part of the state used to serve
    /// `/sync` at receipt time.
    #[must_use]
    pub const fn is_soft_failed(self) -> bool {
        self.0 & Self::SOFT_FAILED != 0
    }

    /// Sets or clears [`Self::is_soft_failed`].
    pub fn set_soft_failed(&mut self, value: bool) {
        self.set(Self::SOFT_FAILED, value);
    }

    /// A redaction for this event has been accepted; the redacted content, not the original,
    /// should be served.
    #[must_use]
    pub const fn is_redacted(self) -> bool {
        self.0 & Self::REDACTED != 0
    }

    /// Sets or clears [`Self::is_redacted`].
    pub fn set_redacted(&mut self, value: bool) {
        self.set(Self::REDACTED, value);
    }

    /// The event is known only as an `auth_events`/`prev_events` reference (for example, fetched
    /// to check another event's auth chain) and its own prior state has not been computed; it is
    /// not part of the room's timeline.
    #[must_use]
    pub const fn is_outlier(self) -> bool {
        self.0 & Self::OUTLIER != 0
    }

    /// Sets or clears [`Self::is_outlier`].
    pub fn set_outlier(&mut self, value: bool) {
        self.set(Self::OUTLIER, value);
    }

    /// The event was accepted during a faster join before the full room state was backfilled;
    /// state computed through it may still be incomplete.
    #[must_use]
    pub const fn is_partial_state(self) -> bool {
        self.0 & Self::PARTIAL_STATE != 0
    }

    /// Sets or clears [`Self::is_partial_state`].
    pub fn set_partial_state(&mut self, value: bool) {
        self.set(Self::PARTIAL_STATE, value);
    }

    fn set(&mut self, bit: u8, value: bool) {
        if value {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

/// The fixed fields every PDU carries, extracted once at parse time.
///
/// Mirrors the "fixed header" PLAN.md section 6.2 describes for the on-disk event record, using
/// Ruma's owned identifier types rather than the interned short IDs `hs-tables` assigns; the
/// mapping from one to the other is track 01's interning API.
#[derive(Debug, Clone)]
pub struct EventHeader {
    /// The room version this event was parsed and validated against.
    pub room_version: RoomVersionId,
    /// The event's `type`.
    pub event_type: String,
    /// The event's `sender`.
    pub sender: OwnedUserId,
    /// The event's `state_key`, if it is a state event.
    pub state_key: Option<String>,
    /// The event's `origin_server_ts`, milliseconds since the Unix epoch.
    pub origin_server_ts: i64,
    /// The event's `depth`.
    pub depth: i64,
    /// Internal processing flags. Not part of the signed event.
    pub flags: EventFlags,
}

impl EventHeader {
    /// Whether this is a state event (has a `state_key`).
    #[must_use]
    pub const fn is_state_event(&self) -> bool {
        self.state_key.is_some()
    }
}

/// A parsed, validated PDU with cached canonical bytes and hashes.
///
/// Construct with [`Event::parse`]. The wrapped JSON ([`Event::json`]) is the full, unredacted
/// event as received or created, including `hashes` and `signatures`; [`Event::redacted_json`]
/// computes the redacted view on demand (cheap: it is a shallow filter over already-canonical
/// data, not a re-parse).
#[derive(Debug, Clone)]
pub struct Event {
    header: EventHeader,
    event_id: OwnedEventId,
    json: CanonicalJsonObject,
    canonical_bytes: Bytes,
}

impl Event {
    /// Parses and validates a PDU's JSON against the given room version.
    ///
    /// Validation covers: the room version is known; `value` is a JSON object canonicalizable
    /// under the version's canonical JSON strictness; the required fields (`type`, `sender`,
    /// `origin_server_ts`, `depth`, and `room_id` unless this is a room version 12+
    /// `m.room.create`) are present and well-typed; the event ID is present and valid (room
    /// versions 1 and 2) or is derived from the reference hash (room version 3 onward); the whole
    /// event does not exceed [`MAX_PDU_BYTES`].
    ///
    /// This does **not** check signatures, hashes, or authorization -- see [`crate::signing`],
    /// [`crate::hash`] and `hs-state`'s event auth for those.
    ///
    /// # Errors
    /// Returns [`EventError`] describing the first validation failure found.
    pub fn parse(
        value: &serde_json::Value,
        room_version_id: RoomVersionId,
    ) -> Result<Self, EventError> {
        let rules = RoomVersion::lookup(&room_version_id)?.rules;

        if !value.is_object() {
            return Err(EventError::NotObject);
        }
        let json = to_canonical_object(value, rules.strict_canonical_json)?;

        let event_type = required_string(&json, "type")?;
        let sender_str = required_string(&json, "sender")?;
        let sender = UserId::parse(&sender_str).map_err(|e| EventError::InvalidId {
            field: "sender",
            reason: e.to_string(),
        })?;
        let origin_server_ts = required_integer(&json, "origin_server_ts")?;
        let depth = required_integer(&json, "depth")?;
        let state_key = optional_string(&json, "state_key")?;

        let requires_room_id =
            event_type != "m.room.create" || rules.event_format_requires_room_create_room_id;
        if requires_room_id {
            required_string(&json, "room_id")?;
        }

        let event_id = if rules.event_format_requires_event_id {
            let id = required_string(&json, "event_id")?;
            OwnedEventId::try_from(id.as_str()).map_err(|e| EventError::InvalidId {
                field: "event_id",
                reason: e.to_string(),
            })?
        } else {
            let derived = hash::derive_event_id(&json, &rules)?;
            OwnedEventId::try_from(derived.as_str()).map_err(|e| EventError::InvalidId {
                field: "event_id",
                reason: e.to_string(),
            })?
        };

        let canonical_bytes =
            Bytes::from(CanonicalJsonValue::Object(json.clone()).to_canonical_bytes());
        if canonical_bytes.len() > MAX_PDU_BYTES {
            return Err(EventError::TooLarge {
                field: "<event>",
                limit: MAX_PDU_BYTES,
            });
        }

        Ok(Self {
            header: EventHeader {
                room_version: room_version_id,
                event_type,
                sender,
                state_key,
                origin_server_ts,
                depth,
                flags: EventFlags::empty(),
            },
            event_id,
            json,
            canonical_bytes,
        })
    }

    /// The event's fixed header fields and processing flags.
    #[must_use]
    pub fn header(&self) -> &EventHeader {
        &self.header
    }

    /// Mutable access to the processing flags.
    pub fn flags_mut(&mut self) -> &mut EventFlags {
        &mut self.header.flags
    }

    /// The event ID.
    #[must_use]
    pub fn event_id(&self) -> &ruma::EventId {
        &self.event_id
    }

    /// The full, unredacted event JSON (including `hashes` and `signatures`), canonicalized.
    #[must_use]
    pub fn json(&self) -> &CanonicalJsonObject {
        &self.json
    }

    /// The cached canonical JSON encoding of [`Self::json`]. Storage and federation should serve
    /// this directly rather than re-encoding.
    #[must_use]
    pub fn canonical_bytes(&self) -> &Bytes {
        &self.canonical_bytes
    }

    /// Computes this event's redacted JSON per its room version's redaction rules.
    ///
    /// # Errors
    /// Returns [`crate::error::RedactionError`] if the cached JSON is somehow missing `type` or
    /// has a non-object `content`; this cannot happen for an `Event` produced by
    /// [`Event::parse`], since that already validated `type`'s presence, but the possibility is
    /// preserved in the return type rather than asserted away.
    pub fn redacted_json(&self) -> Result<CanonicalJsonObject, crate::error::RedactionError> {
        let rules = self.rules();
        redaction::redact(&self.json, &rules.redaction)
    }

    /// The content hash of the full event.
    #[must_use]
    pub fn content_hash(&self) -> Sha256Digest {
        hash::content_hash(&self.json)
    }

    /// The reference hash of the event (redacted, minus `signatures`).
    ///
    /// # Errors
    /// See [`Self::redacted_json`].
    pub fn reference_hash(&self) -> Result<Sha256Digest, crate::error::RedactionError> {
        hash::reference_hash(&self.json, &self.rules())
    }

    /// This event's room version rules, looked up again from [`EventHeader::room_version`].
    ///
    /// # Panics
    /// Never: [`Event::parse`] already validated the room version is known.
    #[must_use]
    pub fn rules(&self) -> RoomVersionRules {
        RoomVersion::lookup(&self.header.room_version)
            .expect("Event::parse already validated the room version")
            .rules
    }
}

fn required_string(json: &CanonicalJsonObject, field: &'static str) -> Result<String, EventError> {
    match json.get(field) {
        Some(CanonicalJsonValue::String(s)) => Ok(s.clone()),
        Some(_) => Err(EventError::WrongType(field)),
        None => Err(EventError::MissingField(field)),
    }
}

fn optional_string(
    json: &CanonicalJsonObject,
    field: &'static str,
) -> Result<Option<String>, EventError> {
    match json.get(field) {
        Some(CanonicalJsonValue::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(EventError::WrongType(field)),
        None => Ok(None),
    }
}

fn required_integer(json: &CanonicalJsonObject, field: &'static str) -> Result<i64, EventError> {
    match json.get(field) {
        Some(CanonicalJsonValue::Integer(i)) => Ok(*i),
        Some(_) => Err(EventError::WrongType(field)),
        None => Err(EventError::MissingField(field)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v1_event() -> serde_json::Value {
        json!({
            "event_id": "$a:example.org",
            "type": "m.room.message",
            "room_id": "!r:example.org",
            "sender": "@u:example.org",
            "origin_server_ts": 1,
            "depth": 3,
            "content": {"body": "hi"},
            "prev_events": [["$b:example.org", {"sha256": "x"}]],
            "auth_events": [["$c:example.org", {"sha256": "y"}]],
        })
    }

    fn v11_event() -> serde_json::Value {
        json!({
            "type": "m.room.message",
            "room_id": "!r:example.org",
            "sender": "@u:example.org",
            "origin_server_ts": 1,
            "depth": 3,
            "content": {"body": "hi"},
            "prev_events": ["$b"],
            "auth_events": ["$c"],
        })
    }

    #[test]
    fn v1_event_uses_explicit_event_id() {
        let event = Event::parse(&v1_event(), RoomVersionId::V1).unwrap();
        assert_eq!(event.event_id().as_str(), "$a:example.org");
        assert!(!event.header().is_state_event());
    }

    #[test]
    fn v11_event_derives_event_id_from_reference_hash() {
        let event = Event::parse(&v11_event(), RoomVersionId::V11).unwrap();
        assert!(event.event_id().as_str().starts_with('$'));

        let expected = hash::derive_event_id(event.json(), &event.rules()).unwrap();
        assert_eq!(event.event_id().as_str(), expected);
    }

    #[test]
    fn unknown_room_version_is_rejected() {
        let id = RoomVersionId::try_from("not-a-version").unwrap();
        let err = Event::parse(&v11_event(), id).unwrap_err();
        assert!(matches!(err, EventError::UnknownRoomVersion(_)));
    }

    #[test]
    fn missing_required_field_is_rejected() {
        let mut value = v11_event();
        value.as_object_mut().unwrap().remove("sender");
        let err = Event::parse(&value, RoomVersionId::V11).unwrap_err();
        assert_eq!(err, EventError::MissingField("sender"));
    }

    #[test]
    fn wrong_type_field_is_rejected() {
        let mut value = v11_event();
        value
            .as_object_mut()
            .unwrap()
            .insert("depth".to_owned(), json!("not-a-number"));
        let err = Event::parse(&value, RoomVersionId::V11).unwrap_err();
        assert_eq!(err, EventError::WrongType("depth"));
    }

    #[test]
    fn invalid_sender_is_rejected() {
        let mut value = v11_event();
        value
            .as_object_mut()
            .unwrap()
            .insert("sender".to_owned(), json!("not-a-user-id"));
        let err = Event::parse(&value, RoomVersionId::V11).unwrap_err();
        assert!(matches!(
            err,
            EventError::InvalidId {
                field: "sender",
                ..
            }
        ));
    }

    #[test]
    fn v12_create_event_does_not_require_room_id() {
        let create = json!({
            "type": "m.room.create",
            "sender": "@creator:example.org",
            "origin_server_ts": 1,
            "depth": 1,
            "content": {"room_version": "12"},
            "prev_events": [],
            "auth_events": [],
        });
        let event = Event::parse(&create, RoomVersionId::V12).unwrap();
        assert_eq!(event.header().event_type, "m.room.create");
    }

    #[test]
    fn v1_create_event_requires_room_id() {
        let create = json!({
            "event_id": "$a:example.org",
            "type": "m.room.create",
            "sender": "@creator:example.org",
            "origin_server_ts": 1,
            "depth": 1,
            "content": {"creator": "@creator:example.org"},
            "prev_events": [],
            "auth_events": [],
        });
        let err = Event::parse(&create, RoomVersionId::V1).unwrap_err();
        assert_eq!(err, EventError::MissingField("room_id"));
    }

    #[test]
    fn oversized_event_is_rejected() {
        let mut value = v11_event();
        let big = "x".repeat(MAX_PDU_BYTES);
        value
            .as_object_mut()
            .unwrap()
            .insert("content".to_owned(), json!({"body": big}));
        let err = Event::parse(&value, RoomVersionId::V11).unwrap_err();
        assert!(matches!(err, EventError::TooLarge { .. }));
    }

    #[test]
    fn flags_round_trip_through_byte_encoding() {
        let mut flags = EventFlags::empty();
        assert!(!flags.is_rejected());
        flags.set_rejected(true);
        flags.set_partial_state(true);
        assert!(flags.is_rejected());
        assert!(flags.is_partial_state());
        assert!(!flags.is_soft_failed());

        let byte = flags.to_byte();
        let decoded = EventFlags::from_byte(byte);
        assert_eq!(decoded, flags);

        flags.set_rejected(false);
        assert!(!flags.is_rejected());
        assert!(flags.is_partial_state());
    }

    #[test]
    fn redacted_json_drops_disallowed_content() {
        let event = Event::parse(&v11_event(), RoomVersionId::V11).unwrap();
        let redacted = event.redacted_json().unwrap();
        assert!(
            redacted
                .get("content")
                .unwrap()
                .as_object()
                .unwrap()
                .is_empty()
        );
        // Room version 11 (v3+) PDUs never carry an `event_id` field -- it is derived from the
        // reference hash, not part of the signed content -- but `sender` and `room_id` are
        // always-retained top-level fields (see `crate::redaction`).
        assert!(!redacted.contains_key("event_id"));
        assert!(redacted.contains_key("sender"));
        assert!(redacted.contains_key("room_id"));
    }

    #[test]
    fn content_and_reference_hash_are_consistent_with_the_hash_module() {
        let event = Event::parse(&v11_event(), RoomVersionId::V11).unwrap();
        assert_eq!(event.content_hash(), hash::content_hash(event.json()));
        assert_eq!(
            event.reference_hash().unwrap(),
            hash::reference_hash(event.json(), &event.rules()).unwrap()
        );
    }
}
