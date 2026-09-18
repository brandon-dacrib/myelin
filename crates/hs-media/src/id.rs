//! Media IDs: 24 random characters, matching Synapse's `random_string(24)`
//! (`synapse/util/stringutils.py`, behavior only, no code copied) so imported media IDs and
//! freshly generated ones are indistinguishable and both fit the same validation rule.
//!
//! An MXC URI is `mxc://<server_name>/<media_id>` (spec: "Matrix Content (MXC) URIs"). This
//! module only owns the `media_id` component; `server_name` is a `ruma::OwnedServerName` wherever
//! this crate needs one.

use rand::Rng;
use rand::distr::Alphanumeric;

/// The fixed length of a generated media ID. Synapse always generates exactly 24; imported IDs
/// from other homeservers (or federation-sourced remote media, whose ID is chosen by the
/// *origin* server) are not required to match this length — see [`MediaId::parse`] vs.
/// [`MediaId::generate`].
pub const GENERATED_LENGTH: usize = 24;

/// A media ID: the second path component of an `mxc://` URI.
///
/// Two ways to get one: [`MediaId::generate`] (this server minting a new local upload, always 24
/// characters from the fixed alphabet) and [`MediaId::parse`] (accepting whatever a remote server
/// or an existing Synapse deployment already assigned, validated only for the characters that
/// would make it unsafe as a path segment — see the module doc on why the shapes differ).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaId(String);

/// [`MediaId::parse`] rejected the input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaIdError {
    /// Empty string.
    #[error("media ID must not be empty")]
    Empty,
    /// Longer than any sane media ID should be (defends against building an enormous object-store
    /// key from an attacker-controlled path segment).
    #[error("media ID is too long ({len} bytes, max {max})")]
    TooLong {
        /// The offending length.
        len: usize,
        /// The maximum allowed.
        max: usize,
    },
    /// Contains a character that would be unsafe or ambiguous as an object-store key / URL path
    /// segment / filesystem path component: anything outside `[A-Za-z0-9._-]`, and specifically
    /// `.` `.` (dot-dot) and a leading `.` / `/` are rejected outright (path traversal).
    #[error("media ID contains a disallowed character: {0:?}")]
    DisallowedCharacter(char),
    /// `.` or `..` (or an ID starting with either) — a would-be path traversal.
    #[error("media ID must not be a path traversal segment")]
    PathTraversal,
}

/// The maximum length [`MediaId::parse`] accepts for an externally supplied ID (generous headroom
/// over Synapse's own 24, since another homeserver's IDs are out of this server's control).
pub const MAX_PARSED_LENGTH: usize = 255;

impl MediaId {
    /// Generates a fresh, locally minted media ID: 24 characters, drawn from a
    /// cryptographically secure RNG, from the same 62-symbol alphabet Synapse uses.
    #[must_use]
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let s: String = (0..GENERATED_LENGTH)
            .map(|_| rng.sample(Alphanumeric) as char)
            .collect();
        debug_assert_eq!(s.len(), GENERATED_LENGTH);
        MediaId(s)
    }

    /// Validates an externally supplied media ID (from a remote server's MXC URI, from an
    /// imported Synapse media store, or from a client-supplied path segment). Rejects anything
    /// that would be unsafe to use as an object-store key or a filesystem path component:
    /// non-ASCII-alphanumeric-dash-dot-underscore characters, empty strings, `.`/`..`, and
    /// anything implausibly long.
    ///
    /// # Errors
    /// See [`MediaIdError`].
    pub fn parse(s: &str) -> Result<Self, MediaIdError> {
        if s.is_empty() {
            return Err(MediaIdError::Empty);
        }
        if s.len() > MAX_PARSED_LENGTH {
            return Err(MediaIdError::TooLong {
                len: s.len(),
                max: MAX_PARSED_LENGTH,
            });
        }
        if s == "." || s == ".." {
            return Err(MediaIdError::PathTraversal);
        }
        for c in s.chars() {
            if !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
                return Err(MediaIdError::DisallowedCharacter(c));
            }
        }
        Ok(MediaId(s.to_string()))
    }

    /// The raw string form, as it appears in an MXC URI and in object-store keys.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MediaId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MediaId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// `hs-tables` tuple keys need `KeyEncode`/`KeyDecode` on the *string* form; `MediaId` itself
// stores metadata keyed as `(server_name: String, media_id: String)` in `crate::metadata`, so
// this crate does not implement `hs_tables::key::{KeyEncode,KeyDecode}` on `MediaId` directly —
// callers pass `.as_str().to_string()` when building a key tuple. Kept as a plain newtype instead
// of a table key type so `MediaId::generate`/`parse` stay usable without pulling `hs-tables` into
// call sites that only need an identifier (routing, MXC URI formatting).

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generate_produces_the_expected_length() {
        let id = MediaId::generate();
        assert_eq!(id.as_str().len(), GENERATED_LENGTH);
    }

    #[test]
    fn generate_uses_only_alphanumeric_characters() {
        let id = MediaId::generate();
        assert!(id.as_str().chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn generate_is_not_obviously_predictable() {
        // Not a real randomness test, just a canary: 1000 generations should not collide and
        // should not all start with the same character.
        let mut seen = HashSet::new();
        let mut first_chars = HashSet::new();
        for _ in 0..1000 {
            let id = MediaId::generate();
            first_chars.insert(id.as_str().chars().next().unwrap());
            assert!(seen.insert(id), "generated a duplicate media ID");
        }
        assert!(first_chars.len() > 1);
    }

    #[test]
    fn parse_accepts_synapse_shaped_ids() {
        assert!(MediaId::parse("abcDEF0123456789ghijKLMN").is_ok());
    }

    #[test]
    fn parse_rejects_empty() {
        assert_eq!(MediaId::parse(""), Err(MediaIdError::Empty));
    }

    #[test]
    fn parse_rejects_path_traversal() {
        assert_eq!(MediaId::parse(".."), Err(MediaIdError::PathTraversal));
        assert_eq!(MediaId::parse("."), Err(MediaIdError::PathTraversal));
        assert!(matches!(
            MediaId::parse("../../etc/passwd"),
            Err(MediaIdError::DisallowedCharacter('/'))
        ));
    }

    #[test]
    fn parse_rejects_null_byte_and_control_characters() {
        assert!(MediaId::parse("abc\0def").is_err());
        assert!(MediaId::parse("abc\ndef").is_err());
    }

    #[test]
    fn parse_rejects_too_long() {
        let long = "a".repeat(MAX_PARSED_LENGTH + 1);
        assert!(matches!(
            MediaId::parse(&long),
            Err(MediaIdError::TooLong { .. })
        ));
    }

    #[test]
    fn parse_accepts_dots_and_dashes_inside_the_id() {
        // Not a traversal by itself -- only a *whole segment* of `.` or `..` is rejected.
        assert!(MediaId::parse("foo.bar-baz_1").is_ok());
    }
}
