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
//!
//! # Argon2's working memory is pooled
//!
//! Argon2id with the default parameters works in 19 MiB of 64-byte-aligned blocks. Allocating
//! and freeing that per hash -- what the `argon2` crate's `PasswordHasher` and
//! `PasswordVerifier` do -- leaks it on glibc 2.36 (Debian 12; Ubuntu 22.04's 2.35 too): once the
//! first such allocation is freed, glibc's dynamic mmap threshold rises above 19 MiB, the next
//! aligned allocation comes from the heap, and an aligned chunk freed there is not reused (glibc
//! bugs 14581 and 30723). Every registration and every password login kept about 19.5 MB: Sytest
//! drove one server past 10 GB and into the Docker VM's OOM killer (2026-10-01), and 40 logins
//! took a fresh server from 19 MB to 661 MB of anonymous memory, but stayed at 19 MB with
//! `MALLOC_MMAP_THRESHOLD_` pinned. glibc 2.41 grew to 117 MB over 150 logins and stopped.
//!
//! So the blocks come from a process-wide pool, are wiped after use and are kept for the next
//! hash: the process holds one buffer per password operation that has ever run concurrently and
//! allocates nothing per hash after that. The strings are byte-for-byte what the crate's own
//! hasher produces (`a_pooled_hash_is_what_the_argon2_crate_produces` checks both directions).

use std::sync::{Mutex, PoisonError};

use argon2::password_hash::{Output, ParamsString, PasswordHash as Argon2PasswordHash, SaltString};
use argon2::{Algorithm, Argon2, Block, Params, Version};
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
/// any input this function produces; the `Result` exists because the library can fail in
/// principle, e.g. on a `password` so long it overflows Argon2's internal limits).
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    let params = Params::default();
    let mut salt_buf = [0u8; 64];
    let salt_bytes = salt
        .as_salt()
        .decode_b64(&mut salt_buf)
        .map_err(argon2_error)?;
    let output = argon2_output(
        password,
        salt_bytes,
        Algorithm::Argon2id,
        Version::V0x13,
        &params,
        params.output_len().unwrap_or(Params::DEFAULT_OUTPUT_LEN),
    )?;
    let hash = Argon2PasswordHash {
        algorithm: Algorithm::Argon2id.ident(),
        version: Some(Version::V0x13.into()),
        params: ParamsString::try_from(&params).map_err(argon2_error)?,
        salt: Some(salt.as_salt()),
        hash: Some(output),
    };
    Ok(hash.to_string())
}

/// Verifies `password` against `stored_hash`, dispatching on its format:
///
/// - `$argon2...` → Argon2 verification with the algorithm, version and parameters the stored
///   string names (native hashes; `pepper` is not used).
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
        // What the crate's `PasswordVerifier` does, with the working memory from the pool.
        let parsed = Argon2PasswordHash::new(stored_hash).map_err(argon2_error)?;
        let algorithm = Algorithm::try_from(parsed.algorithm).map_err(argon2_error)?;
        let version = parsed
            .version
            .map(Version::try_from)
            .transpose()
            .map_err(argon2_error)?
            .unwrap_or_default();
        let params = Params::try_from(&parsed).map_err(argon2_error)?;
        let (Some(salt), Some(expected)) = (parsed.salt, parsed.hash) else {
            return Err(PasswordError::Argon2(
                "stored hash has no salt or no output".into(),
            ));
        };
        let mut salt_buf = [0u8; 64];
        let salt_bytes = salt.decode_b64(&mut salt_buf).map_err(argon2_error)?;
        let computed = argon2_output(
            password,
            salt_bytes,
            algorithm,
            version,
            &params,
            expected.len(),
        )?;
        // `Output`'s equality is constant-time.
        Ok(computed == expected)
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

/// The Argon2 output for `password` and the decoded `salt`, computed in working memory from
/// [`ARGON2_MEMORY`] (the module documentation says why).
fn argon2_output(
    password: &str,
    salt: &[u8],
    algorithm: Algorithm,
    version: Version,
    params: &Params,
    output_len: usize,
) -> Result<Output, PasswordError> {
    let argon2 = Argon2::new(algorithm, version, params.clone());
    ARGON2_MEMORY.with_blocks(params.block_count(), |blocks| {
        Output::init_with(output_len, |out| {
            argon2
                .hash_password_into_with_memory(password.as_bytes(), salt, out, &mut *blocks)
                .map_err(argon2::password_hash::Error::from)
        })
        .map_err(argon2_error)
    })
}

fn argon2_error(e: impl std::fmt::Display) -> PasswordError {
    PasswordError::Argon2(e.to_string())
}

/// Argon2 working memory shared by every hash and verification in the process.
static ARGON2_MEMORY: BlockPool = BlockPool::new();

/// Buffers of Argon2 blocks, one handed out per concurrent operation and kept afterwards. It
/// never shrinks: freeing a buffer is what leaks on the glibc versions the module documentation
/// names, and the number of buffers is bounded by the most password operations that have ever
/// run at once.
struct BlockPool {
    free: Mutex<Vec<Vec<Block>>>,
}

impl BlockPool {
    const fn new() -> Self {
        Self {
            free: Mutex::new(Vec::new()),
        }
    }

    /// Runs `f` with `count` blocks from the pool (a new buffer if none is free), then wipes the
    /// blocks, which hold material derived from the password, and puts the buffer back.
    fn with_blocks<T>(&self, count: usize, f: impl FnOnce(&mut [Block]) -> T) -> T {
        let mut blocks = self
            .free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .unwrap_or_default();
        blocks.resize(count, Block::default());
        let result = f(&mut blocks[..count]);
        blocks.fill(Block::default());
        self.free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(blocks);
        result
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
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify_password("correct horse battery staple", &hash, "unused-pepper").unwrap());
        assert!(!verify_password("wrong password", &hash, "unused-pepper").unwrap());
    }

    #[test]
    fn argon2_hashes_are_salted_differently_each_time() {
        let h1 = hash_password("same password").unwrap();
        let h2 = hash_password("same password").unwrap();
        assert_ne!(h1, h2);
    }

    /// The pooled path must produce and accept exactly what the `argon2` crate's own
    /// `PasswordHasher` and `PasswordVerifier` do, or stored hashes would stop verifying.
    #[test]
    fn a_pooled_hash_is_what_the_argon2_crate_produces() {
        use argon2::password_hash::{PasswordHasher, PasswordVerifier};

        let ours = hash_password("pool me").unwrap();
        let parsed = Argon2PasswordHash::new(&ours).unwrap();
        assert!(
            Argon2::default()
                .verify_password(b"pool me", &parsed)
                .is_ok()
        );
        assert!(
            Argon2::default()
                .verify_password(b"not me", &parsed)
                .is_err()
        );

        let salt = SaltString::generate(&mut OsRng);
        let theirs = Argon2::default()
            .hash_password(b"pool me", &salt)
            .unwrap()
            .to_string();
        assert!(verify_password("pool me", &theirs, "").unwrap());
        assert!(!verify_password("pool me!", &theirs, "").unwrap());

        // Same salt, same output, bit for bit.
        let mut salt_buf = [0u8; 64];
        let salt_bytes = salt.as_salt().decode_b64(&mut salt_buf).unwrap();
        let params = Params::default();
        let output = argon2_output(
            "pool me",
            salt_bytes,
            Algorithm::Argon2id,
            Version::V0x13,
            &params,
            Params::DEFAULT_OUTPUT_LEN,
        )
        .unwrap();
        assert_eq!(Argon2PasswordHash::new(&theirs).unwrap().hash, Some(output));
    }

    /// A hash made with other parameters (an older or a future cost) still verifies: they come
    /// from the stored string, not from the defaults.
    #[test]
    fn a_hash_with_other_parameters_still_verifies() {
        use argon2::password_hash::PasswordHasher;

        let params = Params::new(4096, 3, 1, None).unwrap();
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let salt = SaltString::generate(&mut OsRng);
        let stored = argon2.hash_password(b"cheap", &salt).unwrap().to_string();
        assert!(stored.contains("m=4096,t=3,p=1"));
        assert!(verify_password("cheap", &stored, "").unwrap());
        assert!(!verify_password("dear", &stored, "").unwrap());
    }

    /// The point of the pool: a second operation reuses the first one's buffer instead of
    /// allocating (and later freeing) another 19 MiB, and the blocks are wiped after use.
    #[test]
    fn the_block_pool_reuses_its_buffer_and_wipes_it() {
        let pool = BlockPool::new();
        let first = pool.with_blocks(8, |blocks| {
            blocks[3].as_mut()[0] = 0xdead_beef;
            blocks.as_ptr() as usize
        });
        let second = pool.with_blocks(8, |blocks| {
            assert!(blocks.iter().all(|b| b.as_ref().iter().all(|w| *w == 0)));
            blocks.as_ptr() as usize
        });
        assert_eq!(first, second);
        assert_eq!(pool.free.lock().unwrap().len(), 1);
    }

    #[test]
    fn unrecognized_format_is_an_error_not_a_mismatch() {
        let err = verify_password("x", "not-a-real-hash", "pepper").unwrap_err();
        assert!(matches!(err, PasswordError::UnrecognizedFormat));
    }

    #[test]
    fn a_corrupt_argon2_hash_is_an_error_not_a_mismatch() {
        let err = verify_password("x", "$argon2id$v=19$m=19456,t=2,p=1$", "").unwrap_err();
        assert!(matches!(err, PasswordError::Argon2(_)));
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
