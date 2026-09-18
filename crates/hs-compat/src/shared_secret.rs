//! The shared-secret registration protocol: Synapse's
//! `POST /_synapse/admin/v1/register` (and the `register_new_matrix_user`
//! CLI script that calls it), reimplemented here so both the compat admin
//! surface and the `hs` CLI shim (see `docs/compat/cli-shims.md`) can use
//! one verified implementation.
//!
//! # Protocol
//!
//! 1. `GET /_synapse/admin/v1/register` issues a one-time, short-lived
//!    nonce ([`NonceRegistry::issue`]).
//! 2. The caller computes an HMAC-SHA1 digest ([`compute_mac`]) over the
//!    nonce, username, password, the literal string `"admin"` or
//!    `"notadmin"`, and optionally a user type, each separated by a NUL
//!    byte, keyed with the server's `registration_shared_secret`
//!    (`auth.registration_shared_secret` in `hs-config`).
//! 3. `POST /_synapse/admin/v1/register` with the nonce, username,
//!    password, admin flag, optional user type and the digest. The server
//!    consumes the nonce (rejecting replay) and verifies the digest in
//!    constant time ([`verify_registration_request`]) before creating the
//!    account.
//!
//! Byte-for-byte compatible with Synapse's construction (see
//! `refs/synapse/docs/admin_api/register_api.md`), so existing
//! `register_new_matrix_user` invocations and any script that has hardcoded
//! the HMAC recipe keep working unmodified.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// How long an issued nonce remains valid. Matches Synapse's own
/// `NONCE_TIMEOUT` (60 seconds) so operator tooling timed against Synapse's
/// behavior keeps working.
pub const NONCE_TIMEOUT: Duration = Duration::from_secs(60);

/// Computes the hex-encoded HMAC-SHA1 digest for a shared-secret
/// registration request.
///
/// The message is `nonce \0 user \0 password \0 ("admin" | "notadmin")
/// [\0 user_type]`, exactly as Synapse's `generate_mac` builds it.
pub fn compute_mac(
    secret: &[u8],
    nonce: &str,
    user: &str,
    password: &str,
    admin: bool,
    user_type: Option<&str>,
) -> String {
    let mut mac = new_mac(secret);
    update_message(&mut mac, nonce, user, password, admin, user_type);
    hex::encode(mac.finalize().into_bytes())
}

/// Errors verifying a shared-secret registration request's MAC.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MacVerifyError {
    /// `mac` was not valid hex, or not the right length for a SHA1 HMAC.
    #[error("malformed mac: not a 40-character hex string")]
    Malformed,
    /// The digest did not match. Deliberately carries no further detail:
    /// telling a caller *how* it was wrong helps forge a valid one.
    #[error("mac does not match")]
    Mismatch,
}

/// Verifies a hex-encoded MAC in constant time (via
/// [`hmac::Mac::verify_slice`], not a string or byte-slice `==`, which
/// would leak timing information proportional to the matching prefix
/// length — the same property Synapse's `hmac.compare_digest` gives it).
pub fn verify_mac(
    secret: &[u8],
    nonce: &str,
    user: &str,
    password: &str,
    admin: bool,
    user_type: Option<&str>,
    mac_hex: &str,
) -> Result<(), MacVerifyError> {
    let given = hex::decode(mac_hex).map_err(|_| MacVerifyError::Malformed)?;
    let mut mac = new_mac(secret);
    update_message(&mut mac, nonce, user, password, admin, user_type);
    mac.verify_slice(&given)
        .map_err(|_| MacVerifyError::Mismatch)
}

fn new_mac(secret: &[u8]) -> HmacSha1 {
    // HMAC accepts any key length (it hashes down oversized keys itself),
    // so this never fails.
    HmacSha1::new_from_slice(secret).expect("HMAC-SHA1 accepts any key length")
}

fn update_message(
    mac: &mut HmacSha1,
    nonce: &str,
    user: &str,
    password: &str,
    admin: bool,
    user_type: Option<&str>,
) {
    mac.update(nonce.as_bytes());
    mac.update(b"\x00");
    mac.update(user.as_bytes());
    mac.update(b"\x00");
    mac.update(password.as_bytes());
    mac.update(b"\x00");
    mac.update(if admin { b"admin" } else { b"notadmin" });
    if let Some(ut) = user_type {
        mac.update(b"\x00");
        mac.update(ut.as_bytes());
    }
}

/// A parsed `POST /_synapse/admin/v1/register` body (the fields that feed
/// the MAC; `displayname` and other account-creation fields are not part of
/// the digest and are handled by the caller after verification succeeds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationRequest {
    /// The nonce from a prior `GET`.
    pub nonce: String,
    /// Localpart or full Matrix ID of the account to create.
    pub username: String,
    /// Plaintext password (hashed by the caller after verification, never
    /// stored or logged by this module).
    pub password: String,
    /// Whether the created account should be a server admin.
    pub admin: bool,
    /// Optional user type (Synapse's `support`/`bot` categories).
    pub user_type: Option<String>,
    /// The claimed digest, to be checked against one computed here.
    pub mac: String,
}

/// Errors from [`verify_registration_request`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistrationError {
    /// The nonce was never issued, already consumed, or has expired.
    #[error(transparent)]
    Nonce(#[from] NonceError),
    /// The digest did not verify.
    #[error(transparent)]
    Mac(#[from] MacVerifyError),
}

/// Verifies and consumes the nonce, then verifies the MAC. Call this from
/// the `/_synapse/admin/v1/register` handler before creating the account;
/// on success the nonce is already consumed (replay of the same request is
/// then a [`NonceError::Unknown`]).
pub fn verify_registration_request(
    registry: &mut NonceRegistry,
    secret: &[u8],
    req: &RegistrationRequest,
) -> Result<(), RegistrationError> {
    registry.consume(&req.nonce)?;
    verify_mac(
        secret,
        &req.nonce,
        &req.username,
        &req.password,
        req.admin,
        req.user_type.as_deref(),
        &req.mac,
    )?;
    Ok(())
}

/// Errors consuming a nonce.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NonceError {
    /// No such nonce was issued (or it was already consumed).
    #[error("unknown or already-used nonce")]
    Unknown,
    /// The nonce was issued but has expired.
    #[error("nonce expired (valid for {}s)", NONCE_TIMEOUT.as_secs())]
    Expired,
}

/// Tracks issued, unconsumed nonces. One instance per server process is
/// enough in single-node mode; a clustered deployment routes
/// `/_synapse/admin/v1/register` to whichever replica owns the global
/// shard, matching how Synapse's own single-writer nonce cache behaves.
#[derive(Debug, Default)]
pub struct NonceRegistry {
    issued: HashMap<String, Instant>,
}

impl NonceRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a fresh, random nonce (32 bytes of randomness, hex-encoded —
    /// far more entropy than Synapse's own nonce, which only needs to be
    /// unguessable for [`NONCE_TIMEOUT`] seconds).
    pub fn issue(&mut self) -> String {
        self.sweep_expired();
        let mut bytes = [0u8; 32];
        rand::Rng::fill(&mut rand::rng(), &mut bytes);
        let nonce = hex::encode(bytes);
        self.issued.insert(nonce.clone(), Instant::now());
        nonce
    }

    /// Consumes `nonce`: valid exactly once, within [`NONCE_TIMEOUT`] of
    /// issuance.
    pub fn consume(&mut self, nonce: &str) -> Result<(), NonceError> {
        match self.issued.remove(nonce) {
            None => Err(NonceError::Unknown),
            Some(issued_at) if issued_at.elapsed() > NONCE_TIMEOUT => Err(NonceError::Expired),
            Some(_) => Ok(()),
        }
    }

    fn sweep_expired(&mut self) {
        self.issued
            .retain(|_, issued_at| issued_at.elapsed() <= NONCE_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known vectors cross-checked against Synapse's own documented recipe
    // (`refs/synapse/docs/admin_api/register_api.md`) using its bash/openssl
    // example:
    //
    //   printf '%s\0%s\0%s\0%s' "$nonce" "$user" "$pass" "$admin" |
    //     openssl sha1 -hmac "$secret" | awk '{print $2}'

    #[test]
    fn matches_synapse_vector_admin_no_user_type() {
        let mac = compute_mac(
            b"shared_secret",
            "thisisanonce",
            "pepper_roni",
            "pizza",
            true,
            None,
        );
        assert_eq!(mac, "48715842ad67d5dc9a9ee938a3bda4fcfae8d7c7");
    }

    #[test]
    fn matches_synapse_vector_notadmin_no_user_type() {
        let mac = compute_mac(b"supersecret", "abc123", "alice", "hunter2", false, None);
        assert_eq!(mac, "ef1e9bcc70e3a886abfac388c7a2c7812f205c12");
    }

    #[test]
    fn matches_synapse_vector_with_user_type() {
        let mac = compute_mac(b"sekrit", "n0nce", "bob", "p@ss", false, Some("support"));
        assert_eq!(mac, "14413b94700ffc3322fc38e7473eb1e4c4d60ca1");
    }

    #[test]
    fn verify_mac_accepts_a_matching_digest() {
        let mac = compute_mac(
            b"shared_secret",
            "thisisanonce",
            "pepper_roni",
            "pizza",
            true,
            None,
        );
        assert!(
            verify_mac(
                b"shared_secret",
                "thisisanonce",
                "pepper_roni",
                "pizza",
                true,
                None,
                &mac
            )
            .is_ok()
        );
    }

    #[test]
    fn verify_mac_rejects_a_wrong_secret() {
        let mac = compute_mac(
            b"shared_secret",
            "thisisanonce",
            "pepper_roni",
            "pizza",
            true,
            None,
        );
        let err = verify_mac(
            b"wrong_secret",
            "thisisanonce",
            "pepper_roni",
            "pizza",
            true,
            None,
            &mac,
        )
        .unwrap_err();
        assert_eq!(err, MacVerifyError::Mismatch);
    }

    #[test]
    fn verify_mac_rejects_a_tampered_field() {
        let mac = compute_mac(
            b"shared_secret",
            "thisisanonce",
            "pepper_roni",
            "pizza",
            true,
            None,
        );
        // Same digest, different (attacker-controlled) admin flag: this is
        // exactly the attack the MAC exists to prevent.
        let err = verify_mac(
            b"shared_secret",
            "thisisanonce",
            "pepper_roni",
            "pizza",
            false,
            None,
            &mac,
        )
        .unwrap_err();
        assert_eq!(err, MacVerifyError::Mismatch);
    }

    #[test]
    fn verify_mac_rejects_malformed_hex() {
        let err = verify_mac(b"shared_secret", "n", "u", "p", false, None, "not-hex").unwrap_err();
        assert_eq!(err, MacVerifyError::Malformed);
    }

    #[test]
    fn nonce_is_single_use() {
        let mut registry = NonceRegistry::new();
        let nonce = registry.issue();
        assert!(registry.consume(&nonce).is_ok());
        assert_eq!(registry.consume(&nonce), Err(NonceError::Unknown));
    }

    #[test]
    fn unknown_nonce_is_rejected() {
        let mut registry = NonceRegistry::new();
        assert_eq!(registry.consume("never-issued"), Err(NonceError::Unknown));
    }

    #[test]
    fn two_issued_nonces_are_distinct_and_independently_consumable() {
        let mut registry = NonceRegistry::new();
        let a = registry.issue();
        let b = registry.issue();
        assert_ne!(a, b);
        assert!(registry.consume(&a).is_ok());
        assert!(registry.consume(&b).is_ok());
    }

    #[test]
    fn full_request_round_trip() {
        let mut registry = NonceRegistry::new();
        let nonce = registry.issue();
        let secret = b"registration secret";
        let mac = compute_mac(secret, &nonce, "newuser", "pw", false, None);
        let req = RegistrationRequest {
            nonce,
            username: "newuser".into(),
            password: "pw".into(),
            admin: false,
            user_type: None,
            mac,
        };
        assert!(verify_registration_request(&mut registry, secret, &req).is_ok());
    }

    #[test]
    fn replaying_a_consumed_request_fails() {
        let mut registry = NonceRegistry::new();
        let nonce = registry.issue();
        let secret = b"registration secret";
        let mac = compute_mac(secret, &nonce, "newuser", "pw", false, None);
        let req = RegistrationRequest {
            nonce,
            username: "newuser".into(),
            password: "pw".into(),
            admin: false,
            user_type: None,
            mac,
        };
        assert!(verify_registration_request(&mut registry, secret, &req).is_ok());
        let err = verify_registration_request(&mut registry, secret, &req).unwrap_err();
        assert_eq!(err, RegistrationError::Nonce(NonceError::Unknown));
    }
}
