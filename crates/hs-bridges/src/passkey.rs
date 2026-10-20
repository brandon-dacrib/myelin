//! Hookshot's passkey: an RSA private key the bridge encrypts the tokens it keeps (GitHub
//! logins and the like) with, read from `passFile` (`/data/passkey.pem`) at start. Hookshot does
//! not generate one (`UserTokenStore.fromKeyPath` reads the file and stops without it, read at
//! 7.5.0; its documentation says `openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096`),
//! so the manager mints one with the instance's tokens and keeps it on the instance's row, the
//! way `pickle_key` is kept: every render writes the same key, and a roll never makes the
//! bridge's stored tokens unreadable.
//!
//! The key is written as PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`), the form `openssl genpkey`
//! writes and the first of the two Hookshot's `TokenEncryption` accepts (its `src/tokens/mod.rs`
//! takes PKCS#8 or PKCS#1). Hookshot encrypts 128-byte chunks with PKCS#1 v1.5, which needs a
//! modulus of at least 2048 bits; [`PASSKEY_BITS`] follows its documentation's 4096.

use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};

/// The key size Hookshot's documentation asks for.
#[cfg(not(test))]
pub const PASSKEY_BITS: usize = 4096;
/// Under test, the smallest key Hookshot's chunked PKCS#1 v1.5 encryption works with: a
/// 4096-bit key takes a debug build tens of seconds to make, and the manager's tests mint one
/// per Hookshot instance.
#[cfg(test)]
pub const PASSKEY_BITS: usize = 2048;

/// A failure to make or encode the key.
#[derive(Debug, thiserror::Error)]
pub enum PasskeyError {
    #[error("generating the RSA key: {0}")]
    Generate(rsa::Error),
    #[error("encoding the RSA key as PKCS#8 PEM: {0}")]
    Encode(rsa::pkcs8::Error),
}

/// A new RSA private key of `bits` bits, as PKCS#8 PEM. Slow for 4096 bits (seconds in a
/// release build, longer in a debug one): call it off the async runtime
/// ([`mint_passkey`]).
///
/// # Errors
/// [`PasskeyError`] when the key cannot be generated or encoded.
pub fn mint_passkey_pem(bits: usize) -> Result<String, PasskeyError> {
    let mut rng = rand_core::OsRng;
    let key = RsaPrivateKey::new(&mut rng, bits).map_err(PasskeyError::Generate)?;
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(PasskeyError::Encode)?;
    Ok(pem.to_string())
}

/// [`mint_passkey_pem`] of [`PASSKEY_BITS`] bits, on a blocking thread.
///
/// # Errors
/// [`PasskeyError`] as [`mint_passkey_pem`], or when the blocking task is cancelled (reported
/// as a generation failure).
pub async fn mint_passkey() -> Result<String, String> {
    tokio::task::spawn_blocking(|| mint_passkey_pem(PASSKEY_BITS))
        .await
        .map_err(|e| format!("minting the passkey: {e}"))?
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::DecodePrivateKey;

    /// 2048 bits here: the shape is the same and a debug build makes it in a second or two.
    #[test]
    fn the_passkey_is_a_pkcs8_pem_hookshot_reads() {
        let pem = mint_passkey_pem(2048).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"), "{pem}");
        assert!(
            pem.trim_end().ends_with("-----END PRIVATE KEY-----"),
            "{pem}"
        );
        let key = RsaPrivateKey::from_pkcs8_pem(&pem).unwrap();
        assert_eq!(rsa::traits::PublicKeyParts::size(&key), 256);
        // Two mints are two keys.
        assert_ne!(pem, mint_passkey_pem(2048).unwrap());
    }
}
