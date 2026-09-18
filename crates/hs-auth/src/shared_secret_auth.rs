//! The `com.devture.shared_secret_auth` login provider: legacy mautrix bridges' double-puppeting
//! mechanism (`PLAN.md` section 6, WS14's "built-in native module ports"; `docs/status/
//! 11-appservices-and-bridges.md`'s "Next" item 2).
//!
//! # Protocol
//!
//! A bridge that already knows a real user wants to be double-puppeted (the user proved their
//! identity to the bridge out of band — typically by pasting an access token or running a bridge
//! command as themselves) computes
//!
//! ```text
//! token = hex(HMAC-SHA512(key = shared_secret, message = full_mxid_utf8_bytes))
//! ```
//!
//! and logs in with `POST /login`, body
//! `{"type": "com.devture.shared_secret_auth", "identifier": {"type": "m.id.user", "user":
//! "@alice:example.org"}, "token": "<hex>"}`. This is exactly what `mautrix-python`'s
//! `CustomPuppetMixin.login_with_shared_secret` and `mautrix-go`'s equivalent do — see
//! `refs/mautrix-python/mautrix/bridge/custom_puppet.py`'s `token = hmac.new(secret,
//! mxid.encode("utf-8"), hashlib.sha512).hexdigest()`, read for behavior only, no code copied
//! (`docs/decisions/0001-license.md`; mautrix-python is Mozilla-2.0, compatible, but this is a
//! four-line HMAC call with no expression worth copying either way).
//!
//! Originally a third-party Synapse `password_auth_provider` module
//! (<https://github.com/devture/matrix-synapse-shared-secret-auth>, referenced from
//! `refs/synapse/docs/password_auth_providers.md`), ported here as a native login type per
//! decision 0007 ("build only what is genuinely ours" — the *protocol* is a fixed external
//! contract every mautrix bridge already speaks, so faithfully reproducing it, not redesigning
//! it, is the job).
//!
//! # Relationship to `hs_compat::shared_secret`
//!
//! `hs-compat` (track 13) already implements a shared-secret HMAC protocol:
//! Synapse's `POST /_synapse/admin/v1/register` nonce-plus-HMAC-SHA1 admin registration MAC. That
//! is a *different* wire protocol — a different hash (SHA-1 vs. SHA-512), a different message
//! (nonce + username + password + admin-flag, NUL-separated, vs. just the mxid) and a different
//! purpose (one-time account creation vs. a repeatable login credential) — so its `compute_mac`/
//! `verify_mac` functions, hardcoded to `Hmac<Sha1>` and that exact message shape, cannot be
//! reused unchanged for this protocol; see this crate's status file's "Reuse considered" section
//! for the fuller reasoning. What *is* reused, deliberately mirroring that module's design rather
//! than inventing a new one, is: the same `hmac`/`sha2`/`hex` crates (already workspace
//! dependencies), and the same constant-time verification shape
//! ([`hmac::Mac::verify_slice`], not a manual byte comparison, which would leak timing
//! information proportional to the matching prefix length).

use hmac::{Hmac, Mac};
use ruma::UserId;
use sha2::Sha512;

type HmacSha512 = Hmac<Sha512>;

/// Errors verifying a `com.devture.shared_secret_auth` token.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenVerifyError {
    /// The presented token was not valid hex, or not the right length for an HMAC-SHA512 digest.
    #[error("malformed token: not a 128-character hex string")]
    Malformed,
    /// The digest did not match. Deliberately carries no further detail: telling a caller *how*
    /// it was wrong helps forge a valid one.
    #[error("token does not match")]
    Mismatch,
}

fn new_mac(secret: &[u8]) -> HmacSha512 {
    // HMAC accepts any key length (it hashes down oversized keys itself), so this never fails.
    HmacSha512::new_from_slice(secret).expect("HMAC-SHA512 accepts any key length")
}

/// Computes the hex-encoded HMAC-SHA512 token for `user_id`, keyed with `secret`. Exposed mainly
/// for tests (an independent oracle for [`verify_token`]) and for anything that wants to mint a
/// token for a bridge's config file (an admin tool, say), not used by the login handler itself
/// (which only verifies a caller-presented token).
#[must_use]
pub fn compute_token(secret: &[u8], user_id: &UserId) -> String {
    let mut mac = new_mac(secret);
    mac.update(user_id.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verifies a hex-encoded `com.devture.shared_secret_auth` token in constant time.
///
/// # Errors
/// [`TokenVerifyError::Malformed`] if `token_hex` is not valid hex of the right length;
/// [`TokenVerifyError::Mismatch`] if it does not match the digest computed for `user_id` under
/// `secret`.
pub fn verify_token(
    secret: &[u8],
    user_id: &UserId,
    token_hex: &str,
) -> Result<(), TokenVerifyError> {
    let given = hex::decode(token_hex).map_err(|_| TokenVerifyError::Malformed)?;
    let mut mac = new_mac(secret);
    mac.update(user_id.as_bytes());
    mac.verify_slice(&given)
        .map_err(|_| TokenVerifyError::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    #[test]
    fn compute_then_verify_round_trips() {
        let token = compute_token(b"sekrit", user_id!("@alice:example.org"));
        assert!(verify_token(b"sekrit", user_id!("@alice:example.org"), &token).is_ok());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = compute_token(b"sekrit", user_id!("@alice:example.org"));
        let err = verify_token(b"wrong", user_id!("@alice:example.org"), &token).unwrap_err();
        assert_eq!(err, TokenVerifyError::Mismatch);
    }

    #[test]
    fn wrong_user_is_rejected() {
        let token = compute_token(b"sekrit", user_id!("@alice:example.org"));
        let err = verify_token(b"sekrit", user_id!("@bob:example.org"), &token).unwrap_err();
        assert_eq!(err, TokenVerifyError::Mismatch);
    }

    #[test]
    fn malformed_hex_is_rejected() {
        let err = verify_token(b"sekrit", user_id!("@alice:example.org"), "not-hex").unwrap_err();
        assert_eq!(err, TokenVerifyError::Malformed);
    }

    #[test]
    fn known_vector_matches_hmac_sha512() {
        // Independently computed (Python `hmac.new(b"topsecret",
        // "@alice:example.org".encode(), hashlib.sha512).hexdigest()`), not round-tripped
        // through this module's own code, matching this crate's existing convention of
        // cross-checking cryptographic primitives against an independent oracle (see
        // `token.rs`'s CRC-32/base62 test and `password.rs`'s bcrypt vectors).
        let token = compute_token(b"topsecret", user_id!("@alice:example.org"));
        assert_eq!(
            token,
            "50112084898ac88c141b7ebd719f2a9a39d4d95a8050d39d6bdb1ecaa8552813eec7fe8a5708f76d8b3a557a9129a9fde4aab2e1195282190c4570937e288da0"
        );
    }
}
