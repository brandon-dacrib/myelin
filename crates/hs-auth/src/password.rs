//! Password hashing.
//!
//! Argon2id is the native default for every password this server hashes itself
//! ([`hash_password`]). bcrypt verification exists solely so that password hashes imported from a
//! Synapse `users.password_hash` column keep working without forcing a password reset
//! (`verify_password`, dispatching on the stored hash's `$argon2` / `$2` PHC-style prefix).
//!
//! Synapse always hashes `password + pepper`, truncated to bcrypt's 72-byte input limit, using
//! `config.auth.password_pepper` (`refs/synapse/synapse/handlers/auth.py`, behavioral reference
//! only). This module reproduces that truncation exactly (`bcrypt_bytes_to_hash`) so hashes
//! imported byte-for-byte from a Synapse database continue to verify. The native Argon2id path
//! does not use a pepper: Argon2id's own memory-hardness is the defense an extra static secret
//! would otherwise buy, and dropping it removes one more secret operators must provision and
//! rotate. See `docs/rfcs/0002-auth-tokens-and-requester.md` section 5.

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash as Argon2PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use rand_core::OsRng;
use thiserror::Error;

/// bcrypt only reads the first 72 bytes of its input; Synapse truncates explicitly (and warns)
/// rather than relying on the library to do it silently, and so do we.
const BCRYPT_MAX_BYTES: usize = 72;

/// Errors from hashing or verifying a password.
#[derive(Debug, Error)]
pub enum PasswordError {
    /// The stored hash string does not look like an Argon2 PHC string or a bcrypt hash.
    #[error("unrecognized password hash format")]
    UnrecognizedFormat,
    /// Argon2 hashing or verification failed (includes "password did not match", exposed as
    /// `Ok(false)` from `verify_password` instead — this variant is for actual errors, such as a
    /// corrupt stored hash).
    #[error("argon2 error: {0}")]
    Argon2(String),
    /// bcrypt hashing or verification failed for a reason other than a mismatched password.
    #[error("bcrypt error: {0}")]
    Bcrypt(#[from] bcrypt::BcryptError),
}

/// Hashes `password` with Argon2id using a fresh random salt, returning a self-describing PHC
/// string (`$argon2id$v=19$m=...,t=...,p=...$<salt>$<hash>`) suitable for storage.
///
/// # Errors
/// Returns [`PasswordError::Argon2`] if the underlying library reports a failure (it does not for
/// any input this function produces; the `Result` exists because the trait signature can fail in
/// principle, e.g. on a `password` so it long it overflows Argon2's internal limits).
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| PasswordError::Argon2(e.to_string()))?;
    Ok(hash.to_string())
}

/// Verifies `password` against `stored_hash`, dispatching on its format:
///
/// - `$argon2...` → Argon2id verification (native hashes; `pepper` is not used).
/// - `$2a$` / `$2b$` / `$2x$` / `$2y$` → bcrypt verification of `password + pepper`, truncated to
///   72 bytes exactly as Synapse does, so hashes imported from a Synapse database verify
///   unchanged.
///
/// Returns `Ok(false)` for a wrong password (not an error) and `Err` only for a malformed or
/// unrecognized stored hash, matching the shape callers need to distinguish "login failed" from
/// "the record is corrupt, alert an operator".
pub fn verify_password(
    password: &str,
    stored_hash: &str,
    pepper: &str,
) -> Result<bool, PasswordError> {
    if stored_hash.starts_with("$argon2") {
        let parsed = Argon2PasswordHash::new(stored_hash)
            .map_err(|e| PasswordError::Argon2(e.to_string()))?;
        match Argon2::default().verify_password(password.as_bytes(), &parsed) {
            Ok(()) => Ok(true),
            Err(argon2::password_hash::Error::Password) => Ok(false),
            Err(e) => Err(PasswordError::Argon2(e.to_string())),
        }
    } else if stored_hash.starts_with("$2a$")
        || stored_hash.starts_with("$2b$")
        || stored_hash.starts_with("$2x$")
        || stored_hash.starts_with("$2y$")
    {
        let bytes = bcrypt_bytes_to_hash(password, pepper);
        Ok(bcrypt::verify(bytes, stored_hash)?)
    } else {
        Err(PasswordError::UnrecognizedFormat)
    }
}

/// `password.as_bytes() + pepper.as_bytes()`, truncated to 72 bytes. Exposed for tests that need
/// to reproduce Synapse's exact truncation boundary; production code should use
/// [`verify_password`].
fn bcrypt_bytes_to_hash(password: &str, pepper: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(password.len() + pepper.len());
    bytes.extend_from_slice(password.as_bytes());
    bytes.extend_from_slice(pepper.as_bytes());
    bytes.truncate(BCRYPT_MAX_BYTES);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_round_trips() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery staple", &hash, "unused-pepper").unwrap());
        assert!(!verify_password("wrong password", &hash, "unused-pepper").unwrap());
    }

    #[test]
    fn argon2_hashes_are_salted_differently_each_time() {
        let h1 = hash_password("same password").unwrap();
        let h2 = hash_password("same password").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn unrecognized_format_is_an_error_not_a_mismatch() {
        let err = verify_password("x", "not-a-real-hash", "pepper").unwrap_err();
        assert!(matches!(err, PasswordError::UnrecognizedFormat));
    }

    /// Known-hash test: this bcrypt hash (cost 4, chosen low for test speed) and password were
    /// generated independently with Python's `bcrypt` 5.0.0 (`bcrypt.hashpw(b"hunter2",
    /// bcrypt.gensalt(4))`), not with this crate's code, so this is a genuine cross-check of our
    /// bcrypt wiring against an independent implementation.
    #[test]
    fn verifies_a_known_bcrypt_hash_with_no_pepper() {
        let hash = "$2b$04$Uxc0/aV.bLQUtbHtHqTkTeS0VTVN5kJZVS6qjRLDbBu4SPuIoYu6.";
        assert!(verify_password("hunter2", hash, "").unwrap());
        assert!(!verify_password("hunter3", hash, "").unwrap());
    }

    /// Known-hash test with a pepper, matching Synapse's `password + pepper` concatenation.
    /// Independently generated: `bcrypt.hashpw(("correct horse battery staple" +
    /// "the-server-pepper").encode(), bcrypt.gensalt(4))`.
    #[test]
    fn verifies_a_known_bcrypt_hash_with_a_pepper() {
        let hash = "$2b$04$fnfuoNXunbGf6XtOmKyZz.Q5gnk5X5vCjqIeoXKrbAfY1MBE7/RhK";
        assert!(
            verify_password("correct horse battery staple", hash, "the-server-pepper").unwrap()
        );
        // Wrong pepper must not verify.
        assert!(!verify_password("correct horse battery staple", hash, "wrong-pepper").unwrap());
    }

    /// Known-hash test proving 72-byte truncation semantics match Synapse's exactly: two
    /// passwords that are byte-identical only in their first 72 bytes of `password + pepper` must
    /// both verify against the same hash. Independently generated: `password_a = "x"*72 +
    /// "tail-AAAA"`, `password_b = "x"*72 + "tail-BBBB"`, `pepper = "pep"`; both truncate to
    /// `"x"*72` before hashing, so `bcrypt.hashpw(("x"*72).encode(), gensalt(4))` is the hash both
    /// must verify against.
    #[test]
    fn bcrypt_truncates_password_plus_pepper_to_72_bytes_like_synapse() {
        let hash = "$2b$04$2oSAY9zZgwyGJePxSDziQOHY.Ngb2J8G5aEr.RKx0RIMyeukvwS9O";
        let pepper = "pep";
        let password_a = format!("{}{}", "x".repeat(72), "tail-AAAA");
        let password_b = format!("{}{}", "x".repeat(72), "tail-BBBB");
        assert!(verify_password(&password_a, hash, pepper).unwrap());
        assert!(verify_password(&password_b, hash, pepper).unwrap());
    }

    #[test]
    fn bcrypt_bytes_to_hash_truncates_at_72_bytes() {
        let long = "y".repeat(100);
        let bytes = bcrypt_bytes_to_hash(&long, "pepper-goes-over");
        assert_eq!(bytes.len(), 72);
        assert!(bytes.iter().all(|&b| b == b'y'));
    }

    #[test]
    fn bcrypt_bytes_to_hash_does_not_truncate_short_input() {
        let bytes = bcrypt_bytes_to_hash("short", "pep");
        assert_eq!(bytes, b"shortpep");
    }
}
