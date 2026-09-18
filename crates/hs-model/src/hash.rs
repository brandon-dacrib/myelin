//! Content hash and reference hash.
//!
//! The [content hash] covers the whole event (unredacted) minus `hashes`, `signatures` and
//! `unsigned`; it is embedded back into the event's own `hashes.sha256` field before signing, so
//! recipients can detect tampering with fields that redaction would otherwise strip.
//!
//! The [reference hash] covers the *redacted* event minus `signatures` (and `unsigned`, which
//! redaction already drops); it is what room versions 3 and later use as the event ID, and what
//! every room version uses to reference an event from `auth_events` and `prev_events`.
//!
//! Re-expressed from the Matrix server-server specification
//! (`refs/matrix-spec/content/server-server-api.md`, "Calculating the content hash for an event"
//! and "Calculating the reference hash for an event", Apache-2.0) and cross-checked in tests
//! against `ruma_signatures::{content_hash, reference_hash}` (MIT license, Ruma project).
//!
//! [content hash]: https://spec.matrix.org/v1.19/server-server-api/#calculating-the-content-hash-for-an-event
//! [reference hash]: https://spec.matrix.org/v1.19/server-server-api/#calculating-the-reference-hash-for-an-event

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use crate::canonical::{CanonicalJsonObject, CanonicalJsonValue};
use crate::error::RedactionError;
use crate::redaction::redact;
use crate::room_version::{EventIdFormat, RoomVersionRules};

/// Fields stripped before computing the content hash.
const CONTENT_HASH_STRIP: &[&str] = &["hashes", "signatures", "unsigned"];
/// Fields stripped, on top of redaction, before computing the reference hash.
const REFERENCE_HASH_STRIP: &[&str] = &["signatures"];

/// A SHA-256 digest.
pub type Sha256Digest = [u8; 32];

/// Computes the content hash of an event's full (unredacted) JSON.
///
/// `event` should not yet have a `hashes.sha256` field set (or its prior value is irrelevant: it
/// is stripped before hashing either way).
#[must_use]
pub fn content_hash(event: &CanonicalJsonObject) -> Sha256Digest {
    let mut stripped = event.clone();
    for key in CONTENT_HASH_STRIP {
        stripped.remove(*key);
    }
    let bytes = CanonicalJsonValue::Object(stripped).to_canonical_bytes();
    Sha256::digest(bytes).into()
}

/// Computes the content hash and returns it base64-encoded (standard alphabet, unpadded), ready
/// to store as `hashes.sha256`.
#[must_use]
pub fn content_hash_base64(event: &CanonicalJsonObject) -> String {
    STANDARD_NO_PAD.encode(content_hash(event))
}

/// Computes the reference hash of an event: redact per the room version, strip `signatures`, then
/// hash.
///
/// # Errors
/// Returns [`RedactionError`] if `event` cannot be redacted (see [`crate::redaction::redact`]).
pub fn reference_hash(
    event: &CanonicalJsonObject,
    rules: &RoomVersionRules,
) -> Result<Sha256Digest, RedactionError> {
    let mut redacted = redact(event, &rules.redaction)?;
    for key in REFERENCE_HASH_STRIP {
        redacted.remove(*key);
    }
    let bytes = CanonicalJsonValue::Object(redacted).to_canonical_bytes();
    Ok(Sha256::digest(bytes).into())
}

/// Base64-encodes a reference hash using the alphabet the room version specifies: standard for
/// room versions 1 and 2 (used only to reference events, not as the event ID), standard for room
/// version 3 (used as the event ID), and URL-safe from room version 4 onward.
#[must_use]
pub fn encode_reference_hash(hash: &Sha256Digest, rules: &RoomVersionRules) -> String {
    match rules.event_id_format {
        EventIdFormat::V1Opaque | EventIdFormat::V2StandardBase64 => STANDARD_NO_PAD.encode(hash),
        EventIdFormat::V3UrlSafeBase64 => URL_SAFE_NO_PAD.encode(hash),
    }
}

/// Computes an event ID of the form `$<reference hash>` for room versions that derive it that
/// way (room version 3 onward). Room versions 1 and 2 carry an explicit `event_id` field instead;
/// callers should not call this for those versions.
///
/// # Errors
/// Returns [`RedactionError`] if the reference hash computation fails.
pub fn derive_event_id(
    event: &CanonicalJsonObject,
    rules: &RoomVersionRules,
) -> Result<String, RedactionError> {
    let hash = reference_hash(event, rules)?;
    Ok(format!("${}", encode_reference_hash(&hash, rules)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::to_canonical_object;
    use crate::room_version::RoomVersionRules;
    use serde_json::json;

    /// The exact example from the server-server spec's content hash section.
    #[test]
    fn spec_example_content_hash() {
        let event = to_canonical_object(
            &json!({
                "room_id": "!x:domain",
                "sender": "@a:domain",
                "origin": "domain",
                "origin_server_ts": 1_000_000,
                "type": "X",
                "content": {},
                "prev_events": [],
                "auth_events": [],
                "depth": 3
            }),
            true,
        )
        .unwrap();
        assert_eq!(
            content_hash_base64(&event),
            "5jM4wQpv6lnBo7CLIghJuHdW+s2CMBJPUOGOC89ncos"
        );
    }

    #[test]
    fn reference_hash_v11_uses_url_safe_alphabet() {
        let event = to_canonical_object(
            &json!({
                "type": "m.room.message",
                "room_id": "!r:x",
                "sender": "@u:x",
                "origin_server_ts": 1,
                "content": {"body": "hi"},
                "prev_events": [],
                "auth_events": [],
                "depth": 1,
                "hashes": {"sha256": "abc"},
            }),
            true,
        )
        .unwrap();
        let id = derive_event_id(&event, &RoomVersionRules::V11).unwrap();
        assert!(id.starts_with('$'));
        assert!(
            !id.contains('+') && !id.contains('/'),
            "expected URL-safe base64: {id}"
        );
    }

    #[test]
    fn cross_check_against_ruma_hashes() {
        let event = to_canonical_object(
            &json!({
                "type": "m.room.member",
                "room_id": "!r:x",
                "sender": "@u:x",
                "state_key": "@u:x",
                "origin_server_ts": 42,
                "content": {"membership": "join"},
                "prev_events": ["$a"],
                "auth_events": ["$b"],
                "depth": 4,
            }),
            true,
        )
        .unwrap();

        let ruma_event: ruma::CanonicalJsonObject =
            serde_json::from_value(event_to_json(&event)).unwrap();

        let our_hash = content_hash(&event);
        let their_hash = ruma::signatures::content_hash(&ruma_event).unwrap();
        assert_eq!(&our_hash[..], their_hash.as_bytes());

        for (rules, ruma_rules) in [
            (
                RoomVersionRules::V1,
                ruma::room_version_rules::RoomVersionRules::V1,
            ),
            (
                RoomVersionRules::V3,
                ruma::room_version_rules::RoomVersionRules::V3,
            ),
            (
                RoomVersionRules::V11,
                ruma::room_version_rules::RoomVersionRules::V11,
            ),
        ] {
            let ours = reference_hash(&event, &rules).unwrap();
            let theirs = ruma::signatures::reference_hash(&ruma_event, &ruma_rules).unwrap();
            let ours_encoded = encode_reference_hash(&ours, &rules);
            assert_eq!(
                ours_encoded, theirs,
                "mismatch for {:?}",
                rules.event_id_format
            );
        }
    }

    /// Converts our `CanonicalJsonObject` back to `serde_json::Value` for cross-checking against
    /// Ruma, which parses JSON text rather than accepting our type directly.
    fn event_to_json(obj: &CanonicalJsonObject) -> serde_json::Value {
        serde_json::from_slice(&CanonicalJsonValue::Object(obj.clone()).to_canonical_bytes())
            .unwrap()
    }
}
