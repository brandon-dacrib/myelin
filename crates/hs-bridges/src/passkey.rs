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
//!
//! The key is made by aws-lc-rs, which rustls and matrix-sdk already bring in. The `rsa` crate
//! did this until 2026-10-20: it carries RUSTSEC-2023-0071 (the Marvin timing attack on
//! decryption), which `cargo audit` on CI refuses outright, although this server only ever
//! generates and never decrypts with the key.

use aws_lc_rs::encoding::{AsDer as _, Pkcs8V1Der};
use aws_lc_rs::rsa::{KeyPair, KeySize};
use base64::Engine as _;

/// The key size Hookshot's documentation asks for.
#[cfg(not(test))]
pub const PASSKEY_BITS: usize = 4096;
/// Under test, the smallest key Hookshot's chunked PKCS#1 v1.5 encryption works with: a
/// 4096-bit key takes a debug build seconds to make, and the manager's tests mint one per
/// Hookshot instance.
#[cfg(test)]
pub const PASSKEY_BITS: usize = 2048;

/// A failure to make or encode the key.
#[derive(Debug, thiserror::Error)]
pub enum PasskeyError {
    #[error("the key size must be 2048, 3072 or 4096 bits, not {0}")]
    Size(usize),
    #[error("generating the RSA key")]
    Generate,
    #[error("encoding the RSA key as PKCS#8")]
    Encode,
}

/// A new RSA private key of `bits` bits (2048, 3072 or 4096), as PKCS#8 PEM. Slow for 4096
/// bits (a second or more): call it off the async runtime ([`mint_passkey`]).
///
/// # Errors
/// [`PasskeyError`] when the size is not one aws-lc-rs makes, or the key cannot be generated
/// or encoded.
pub fn mint_passkey_pem(bits: usize) -> Result<String, PasskeyError> {
    let size = match bits {
        2048 => KeySize::Rsa2048,
        3072 => KeySize::Rsa3072,
        4096 => KeySize::Rsa4096,
        other => return Err(PasskeyError::Size(other)),
    };
    let key = KeyPair::generate(size).map_err(|_| PasskeyError::Generate)?;
    let der: Pkcs8V1Der<'static> = key.as_der().map_err(|_| PasskeyError::Encode)?;
    Ok(pkcs8_pem(der.as_ref()))
}

/// `der` as the PEM `openssl genpkey` writes: the PKCS#8 label and base64 in 64-column lines.
fn pkcs8_pem(der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::with_capacity(body.len() + body.len() / 64 + 64);
    pem.push_str("-----BEGIN PRIVATE KEY-----\n");
    for line in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    pem
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
    use rustls_pki_types::PrivatePkcs8KeyDer;
    use rustls_pki_types::pem::PemObject as _;

    /// 2048 bits here: the shape is the same and a debug build makes it in a second or two.
    #[test]
    fn the_passkey_is_a_pkcs8_pem_hookshot_reads() {
        let pem = mint_passkey_pem(2048).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"), "{pem}");
        assert!(pem.ends_with("-----END PRIVATE KEY-----\n"), "{pem}");
        assert!(
            pem.lines().all(|l| l.len() <= 64),
            "a line longer than 64 columns"
        );
        // Read back the way a PEM reader does, then as the RSA key Hookshot's PKCS#1 v1.5
        // chunking needs: a 2048-bit modulus is 256 bytes.
        let der = PrivatePkcs8KeyDer::from_pem_slice(pem.as_bytes()).unwrap();
        let key = KeyPair::from_pkcs8(der.secret_pkcs8_der()).unwrap();
        assert_eq!(key.public_modulus_len(), 256);
        // Two mints are two keys.
        assert_ne!(pem, mint_passkey_pem(2048).unwrap());
        assert!(matches!(
            mint_passkey_pem(1024),
            Err(PasskeyError::Size(1024))
        ));
    }
}
