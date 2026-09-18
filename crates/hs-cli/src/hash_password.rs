//! `hs hash-password`: bcrypt-hashes... actually Argon2id-hashes a password the same way the
//! server would at registration time, for provisioning scripts that write directly into a
//! password field rather than calling `/register` (`docs/compat/cli-shims.md`).
//!
//! Delegates the hashing itself to [`hs_auth::password::hash_password`] (owned by track 07); this
//! module is only the CLI-facing wrapper `docs/compat/cli-shims.md` calls for.
//!
//! Note on the pepper flag: `hs_auth::password::hash_password` (Argon2id, the native path) does
//! not take a pepper at all — see that function's doc comment: "Argon2id's own memory-hardness is
//! the defense an extra static secret would otherwise buy". `hs hash-password -c config.yaml`
//! still reads `auth.password.pepper` from the given config (via
//! [`crate::config_bridge::read_pepper_from_config`]) for parity with Synapse's script, but a
//! freshly hashed password never uses it — the pepper only matters when *verifying* an imported
//! bcrypt hash, not when minting a new native one. This is called out explicitly rather than
//! silently accepting and ignoring the flag, so an operator piping a pepper in does not assume it
//! took effect.

pub use hs_auth::password::hash_password;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_a_password() {
        let hash = hash_password("hunter2").unwrap();
        assert!(hash.starts_with("$argon2id$"));
    }
}
