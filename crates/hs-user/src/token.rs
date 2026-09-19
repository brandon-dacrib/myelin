//! [`SyncToken`]: the opaque, versioned `/sync` `since`/`next_batch` token.
//!
//! `PLAN.md` section 6.6: "Sync tokens are opaque and encode `feed_seq` plus the small
//! independent cursors (to-device, device lists, account data, presence, receipts)." This module
//! is that codec, built and tested before anything else in this crate (per this track's brief,
//! "day-one work": "design the token codec with property tests") because every other piece --
//! the durable feed (`crate::store`), the session hub (`crate::hub`) and `/sync` itself
//! (`crate::sync`) -- is built on top of what a token can carry.
//!
//! # Why a struct of small integers, not a per-room position vector
//!
//! A token that carried one position per room the user is in would grow with the size of the
//! user's account (thousands of rooms for a power user), defeating the whole point of an opaque,
//! constant-size token a client stores and echoes back. Instead, `feed_seq` is the *one* position
//! that matters for "what rooms changed": it indexes into the user's durable, coalesced feed
//! (`crate::store::UserStore::append_feed_entry`), and each room's own resume position is derived
//! from that feed at sync time (`crate::sync::room_pos_as_of`), not carried in the token. The
//! five cursors are genuinely independent of feed position (to-device messages, device-list
//! updates, account data, presence and read receipts are not `RoomUpdate`-shaped) and of each
//! other, so each gets its own field rather than being folded into `feed_seq`.
//!
//! # Opacity
//!
//! The spec requires clients to treat `since`/`next_batch` as an opaque string, never to parse or
//! construct one themselves. This codec's wire format (a version byte, six big-endian `u64`
//! fields, base64url-no-pad, prefixed with `hsu1_` for grep-ability in logs) is therefore an
//! implementation detail this server is free to change across a version bump -- see
//! [`SyncToken::decode`]'s handling of [`TokenError::UnsupportedVersion`].

use std::fmt;
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// The wire prefix every encoded token carries, ahead of the base64 payload. Not meaningful to a
/// client (tokens are opaque); useful to a human reading a log line to recognize "this is one of
/// our sync tokens" at a glance.
const PREFIX: &str = "hsu1_";

/// The current wire version. Bumped whenever the byte layout below changes in a way that is not
/// backwards-compatible (adding a field at the end, for instance, still needs a version bump: see
/// [`SyncToken::decode`]'s fixed-length check).
///
/// Bumped `1` -> `2` in an earlier session to add `typing_seq`, for the same reason
/// `presence_seq`/`receipts_seq` were reserved up front: `m.typing` needed its own independent
/// cursor once typing distribution was actually implemented (`crate::routes::typing`,
/// `crate::typing`).
///
/// Bumped `2` -> `3` this session to add `push_rules_seq` (below), following the identical
/// precedent: `m.push_rules` needed its own independent cursor once track 10's ruleset seam
/// landed (`docs/status/10-push.md`'s "Interfaces provided" -- `hs_push::rulesets::RulesetStore`'s
/// own per-user monotonic change-seq, exactly the shape `account_data_seq` and `typing_seq`
/// already use). A version-N token now fails to decode (`UnsupportedVersion`) for any N below the
/// current one, which is correct and harmless here: every test and every real deployment of this
/// greenfield server restarts from a freshly built binary, so no long-lived client ever holds a
/// stale-version token across a version bump.
const VERSION: u8 = 3;

/// `1` (version byte) + `8 * 8` (eight `u64` fields).
const PAYLOAD_LEN: usize = 1 + 8 * 8;

/// A decoded `/sync` token: the user's feed position plus the independent extension cursors.
///
/// All fields start at `0` for a brand-new user/device ([`SyncToken::initial`]) and only ever
/// move forward (each is a monotonically increasing counter maintained by
/// `crate::store::UserStore`); `0` for a given field always means "nothing of that kind has ever
/// been observed", which is what makes [`SyncToken::initial`] a valid token rather than a special
/// `None` case every consumer has to branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyncToken {
    /// Position in the user's durable, coalesced feed (`crate::store`). The primary key for "what
    /// rooms changed since this token".
    pub feed_seq: u64,
    /// Cursor into the user's to-device message queue.
    pub to_device_seq: u64,
    /// Cursor into the user's device-list change queue.
    pub device_list_seq: u64,
    /// Cursor into the user's account-data change log (global and per-room).
    pub account_data_seq: u64,
    /// Cursor into presence updates for users this session cares about.
    pub presence_seq: u64,
    /// Cursor into read-receipt updates for rooms this session is in.
    pub receipts_seq: u64,
    /// Cursor into `m.typing` state changes across this user's joined rooms
    /// (`crate::typing::TypingRegistry`).
    pub typing_seq: u64,
    /// Cursor into this user's push-rules change-seq
    /// (`hs_push::rulesets::RulesetStore::changed_seq`) -- compared against the *current*
    /// change-seq to decide whether an incremental sync needs to carry `m.push_rules` again.
    /// `0` (this field's value in [`SyncToken::initial`]) always means "never customized",
    /// matching `RulesetStore::changed_seq`'s own "0 forever for a never-customized user"
    /// convention (`docs/status/10-push.md`), so a brand-new user's first sync compares `0 > 0`
    /// (false) and correctly omits `m.push_rules` on their first *incremental* sync unless they
    /// had already changed their rules -- initial syncs always send it regardless, per
    /// `crate::sync`'s own is-initial branch.
    pub push_rules_seq: u64,
}

impl SyncToken {
    /// The token a client that has never synced before implicitly starts from: every field zero.
    /// Not itself sent to a client (an initial sync's `since` is absent, not this value), but the
    /// baseline every cursor comparison and the initial-sync code path treats "no token" as
    /// equivalent to.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            feed_seq: 0,
            to_device_seq: 0,
            device_list_seq: 0,
            account_data_seq: 0,
            presence_seq: 0,
            receipts_seq: 0,
            typing_seq: 0,
            push_rules_seq: 0,
        }
    }

    /// Encodes this token to its opaque wire form.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut buf = Vec::with_capacity(PAYLOAD_LEN);
        buf.push(VERSION);
        buf.extend_from_slice(&self.feed_seq.to_be_bytes());
        buf.extend_from_slice(&self.to_device_seq.to_be_bytes());
        buf.extend_from_slice(&self.device_list_seq.to_be_bytes());
        buf.extend_from_slice(&self.account_data_seq.to_be_bytes());
        buf.extend_from_slice(&self.presence_seq.to_be_bytes());
        buf.extend_from_slice(&self.receipts_seq.to_be_bytes());
        buf.extend_from_slice(&self.typing_seq.to_be_bytes());
        buf.extend_from_slice(&self.push_rules_seq.to_be_bytes());
        debug_assert_eq!(buf.len(), PAYLOAD_LEN);
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(buf))
    }

    /// Decodes a wire-form token produced by [`SyncToken::encode`].
    ///
    /// # Errors
    /// Returns [`TokenError`] for anything not shaped like a token this server issued: missing
    /// prefix, invalid base64, wrong payload length, or an unsupported version byte. Never
    /// panics on malformed input -- see the property test
    /// [`tests::decoding_never_panics_on_arbitrary_bytes`].
    pub fn decode(s: &str) -> Result<Self, TokenError> {
        let payload = s.strip_prefix(PREFIX).ok_or(TokenError::MissingPrefix)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| TokenError::InvalidBase64)?;
        if bytes.len() != PAYLOAD_LEN {
            return Err(TokenError::WrongLength {
                expected: PAYLOAD_LEN,
                actual: bytes.len(),
            });
        }
        let version = bytes[0];
        if version != VERSION {
            return Err(TokenError::UnsupportedVersion(version));
        }
        let field = |i: usize| -> u64 {
            let start = 1 + i * 8;
            // Safe: `bytes.len() == PAYLOAD_LEN == 1 + 8 * 8`, checked above, and `i < 8` for
            // every call site below, so `start + 8 <= PAYLOAD_LEN` always.
            u64::from_be_bytes(bytes[start..start + 8].try_into().expect("checked length"))
        };
        Ok(Self {
            feed_seq: field(0),
            to_device_seq: field(1),
            device_list_seq: field(2),
            account_data_seq: field(3),
            presence_seq: field(4),
            receipts_seq: field(5),
            typing_seq: field(6),
            push_rules_seq: field(7),
        })
    }
}

impl fmt::Display for SyncToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for SyncToken {
    type Err = TokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::decode(s)
    }
}

impl Serialize for SyncToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.encode())
    }
}

impl<'de> Deserialize<'de> for SyncToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::decode(&s).map_err(serde::de::Error::custom)
    }
}

/// Why [`SyncToken::decode`] rejected a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The string did not start with this server's token prefix -- not a token this server
    /// issued at all (a stray `/messages` pagination token, a Synapse token pasted into the wrong
    /// field, or outright garbage).
    #[error("not a recognized sync token")]
    MissingPrefix,
    /// The payload after the prefix was not valid base64url.
    #[error("malformed sync token encoding")]
    InvalidBase64,
    /// The decoded payload was not the expected fixed length.
    #[error("sync token payload is {actual} bytes, expected {expected}")]
    WrongLength {
        /// The length this codec's current version requires.
        expected: usize,
        /// The length actually decoded.
        actual: usize,
    },
    /// The version byte is not one this build understands. Distinct from the other variants so a
    /// future version bump can, if ever needed, special-case "this looks like a newer token from
    /// a server that has since been upgraded" rather than lumping it in with plain garbage.
    #[error("unsupported sync token version {0}")]
    UnsupportedVersion(u8),
}

impl TokenError {
    /// Maps to the Matrix client-server error shape: an unrecognized/malformed `since` token is
    /// a bad request, not a server error.
    #[must_use]
    pub fn to_matrix_error(self) -> hs_http::error::MatrixError {
        hs_http::error::MatrixError::custom(
            axum::http::StatusCode::BAD_REQUEST,
            hs_http::error::MatrixErrorCode::InvalidParam,
            format!("invalid since token: {self}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn initial_is_all_zero() {
        let t = SyncToken::initial();
        assert_eq!(t.feed_seq, 0);
        assert_eq!(t.to_device_seq, 0);
        assert_eq!(t.device_list_seq, 0);
        assert_eq!(t.account_data_seq, 0);
        assert_eq!(t.presence_seq, 0);
        assert_eq!(t.receipts_seq, 0);
        assert_eq!(t.typing_seq, 0);
        assert_eq!(t.push_rules_seq, 0);
    }

    #[test]
    fn encode_carries_the_recognizable_prefix() {
        assert!(SyncToken::initial().encode().starts_with(PREFIX));
    }

    #[test]
    fn decode_rejects_missing_prefix() {
        assert_eq!(
            SyncToken::decode("not_a_token"),
            Err(TokenError::MissingPrefix)
        );
    }

    #[test]
    fn decode_rejects_bad_base64() {
        assert_eq!(
            SyncToken::decode("hsu1_!!!not-base64!!!"),
            Err(TokenError::InvalidBase64)
        );
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let short = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(b"too short"));
        assert!(matches!(
            SyncToken::decode(&short),
            Err(TokenError::WrongLength { .. })
        ));
    }

    #[test]
    fn decode_rejects_unsupported_version() {
        let mut buf = vec![7u8]; // not VERSION
        buf.extend_from_slice(&[0u8; 64]);
        let s = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(buf));
        assert_eq!(
            SyncToken::decode(&s),
            Err(TokenError::UnsupportedVersion(7))
        );
    }

    #[test]
    fn json_round_trips_as_a_string() {
        let t = SyncToken {
            feed_seq: 42,
            to_device_seq: 1,
            device_list_seq: 2,
            account_data_seq: 3,
            presence_seq: 4,
            receipts_seq: 5,
            typing_seq: 6,
            push_rules_seq: 7,
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.starts_with('"'));
        let back: SyncToken = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn json_rejects_a_non_token_string() {
        let result: Result<SyncToken, _> = serde_json::from_str("\"garbage\"");
        assert!(result.is_err());
    }

    proptest! {
        /// The property this whole module exists to guarantee: any token round-trips through
        /// encode/decode exactly, for every possible combination of field values (not just small
        /// or "realistic" ones -- a feed that somehow ran for `u64::MAX` publishes is a decode bug
        /// waiting to happen if only small values were tested).
        #[test]
        fn round_trips_for_arbitrary_field_values(
            feed_seq: u64,
            to_device_seq: u64,
            device_list_seq: u64,
            account_data_seq: u64,
            presence_seq: u64,
            receipts_seq: u64,
            typing_seq: u64,
            push_rules_seq: u64,
        ) {
            let original = SyncToken {
                feed_seq,
                to_device_seq,
                device_list_seq,
                account_data_seq,
                presence_seq,
                receipts_seq,
                typing_seq,
                push_rules_seq,
            };
            let encoded = original.encode();
            let decoded = SyncToken::decode(&encoded).unwrap();
            prop_assert_eq!(decoded, original);

            // `Display`/`FromStr` must agree with `encode`/`decode` (routes parse tokens out of
            // query strings via `FromStr`, and put them back in JSON responses via `Display`).
            let via_display = original.to_string();
            prop_assert_eq!(&via_display, &encoded);
            let via_from_str: SyncToken = via_display.parse().unwrap();
            prop_assert_eq!(via_from_str, original);
        }

        /// Decoding never panics on arbitrary bytes, however they happen to be shaped -- the
        /// property that justifies handing this codec directly to an HTTP query parameter without
        /// a separate validation pass. `TokenError` is the *only* way malformed input surfaces.
        #[test]
        fn decoding_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..80)) {
            // Exercise both "not our prefix at all" (raw bytes interpreted as UTF-8-ish text) and
            // "our prefix, arbitrary payload" (the more interesting malformed-payload path),
            // since a `String::from_utf8_lossy` fallback below could otherwise change which
            // branch a given byte sequence takes across runs.
            let as_text = String::from_utf8_lossy(&bytes).into_owned();
            let _ = SyncToken::decode(&as_text);

            let prefixed = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(&bytes));
            let _ = SyncToken::decode(&prefixed);
        }

        /// A token built from a `feed_seq` strictly greater than another's, with every other
        /// field held equal, decodes back with that same ordering intact -- the property
        /// `crate::sync`'s "did anything change since `since`" comparison (`token.feed_seq <
        /// current_feed_seq`) depends on: encoding must not scramble magnitude.
        #[test]
        fn feed_seq_ordering_survives_the_round_trip(a: u64, b: u64) {
            let low = SyncToken { feed_seq: a.min(b), ..SyncToken::initial() };
            let high = SyncToken { feed_seq: a.max(b), ..SyncToken::initial() };
            let low_back = SyncToken::decode(&low.encode()).unwrap();
            let high_back = SyncToken::decode(&high.encode()).unwrap();
            prop_assert!(low_back.feed_seq <= high_back.feed_seq);
        }
    }
}
