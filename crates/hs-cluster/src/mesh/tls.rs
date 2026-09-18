//! Mutual TLS material loading and `rustls` config construction for the mesh
//! ([`crate::mesh::auth::AuthMode::MutualTls`]).
//!
//! Rotation: callers reload [`TlsMaterial`] and rebuild the configs on SIGHUP (track 13's
//! reloadable-section mechanism); existing connections keep the config they were accepted with
//! and are closed by the caller when the old certificate expires (RFC 0001 section 11). This
//! module only builds configs from files on disk; it does not watch them.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::RootCertStore;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// Errors loading or building TLS material.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A PEM file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// The file that failed to read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A PEM file contained no usable certificate or key.
    #[error("{path}: no PEM-encoded {what} found")]
    Empty {
        /// The file.
        path: String,
        /// What was expected (`"certificate"` or `"private key"`).
        what: &'static str,
    },
    /// `rustls` rejected the material or configuration.
    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let file = File::open(path).map_err(|source| TlsError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(file))
        .filter_map(Result::ok)
        .collect();
    if certs.is_empty() {
        return Err(TlsError::Empty {
            path: path.display().to_string(),
            what: "certificate",
        });
    }
    Ok(certs)
}

fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let file = File::open(path).map_err(|source| TlsError::Io {
        path: path.display().to_string(),
        source,
    })?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .ok()
        .flatten()
        .ok_or_else(|| TlsError::Empty {
            path: path.display().to_string(),
            what: "private key",
        })
}

/// This replica's certificate chain and key, plus the cluster CA used to verify peers.
pub struct TlsMaterial {
    /// This replica's certificate chain (leaf first).
    pub cert_chain: Vec<CertificateDer<'static>>,
    /// This replica's private key.
    pub key: PrivateKeyDer<'static>,
    /// The cluster CA, trusted for verifying peers.
    pub ca: RootCertStore,
}

impl TlsMaterial {
    /// Loads PEM-encoded material from disk.
    ///
    /// # Errors
    /// Returns [`TlsError`] if a file is missing, unreadable or contains no usable PEM data.
    pub fn load(ca_file: &Path, cert_file: &Path, key_file: &Path) -> Result<Self, TlsError> {
        let ca_certs = read_certs(ca_file)?;
        let mut ca = RootCertStore::empty();
        for cert in ca_certs {
            // A CA file with an entry `rustls` cannot parse as a trust anchor is a
            // misconfiguration; surfacing it as a `rustls::Error` (rather than silently skipping
            // it) is deliberate, this file is on the mesh's trust boundary.
            ca.add(cert)?;
        }
        Ok(Self {
            cert_chain: read_certs(cert_file)?,
            key: read_key(key_file)?,
            ca,
        })
    }

    fn provider() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// Builds a server config that requires and verifies a client certificate chained to the
    /// cluster CA.
    ///
    /// # Errors
    /// Returns [`TlsError`] if the verifier or config could not be built.
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, TlsError> {
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(self.ca.clone()),
            Self::provider(),
        )
        .build()
        .map_err(|e| TlsError::Rustls(rustls::Error::General(e.to_string())))?;
        let cfg = rustls::ServerConfig::builder_with_provider(Self::provider())
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.cert_chain.clone(), self.key.clone_key())?;
        Ok(Arc::new(cfg))
    }

    /// Builds a client config that presents this replica's certificate and verifies the peer's
    /// certificate against the cluster CA.
    ///
    /// # Errors
    /// Returns [`TlsError`] if the config could not be built.
    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, TlsError> {
        let cfg = rustls::ClientConfig::builder_with_provider(Self::provider())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(self.ca.clone())
            .with_client_auth_cert(self.cert_chain.clone(), self.key.clone_key())?;
        Ok(Arc::new(cfg))
    }
}

/// The lowercase-hex SHA-256 fingerprint of a certificate's DER encoding.
#[must_use]
pub fn fingerprint_sha256_hex(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    hex::encode(digest)
}

/// Extracts the `dNSName` subject alternative names from an X.509 (DER) certificate.
///
/// This is a narrow, purpose-built ASN.1 DER reader, not a general X.509 parser: it locates the
/// `subjectAltName` extension (OID 2.5.29.17) by its unique DER-encoded OID byte pattern and
/// reads the `dNSName` (`GeneralName` context tag `[2]`) entries inside it. `rustls`'s own
/// certificate verification (chain-to-CA, validity period, key usage) already ran before this is
/// called; this function only extracts names for the mesh's own SAN-suffix check, so a
/// certificate this cannot parse simply yields no names rather than being treated as an error --
/// [`crate::mesh::auth::MutualTlsAuthenticator`] then rejects it for having no matching SAN,
/// which is the safe direction to fail in.
#[must_use]
pub fn extract_dns_sans(cert: &CertificateDer<'_>) -> Vec<String> {
    der::dns_sans(cert.as_ref())
}

/// The minimal DER walker behind [`extract_dns_sans`].
mod der {
    /// DER-encoded OID for `subjectAltName` (2.5.29.17): tag `0x06`, length `3`, value
    /// `55 1D 11`.
    const SAN_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x1D, 0x11];
    /// Context-specific primitive tag `[2]`, i.e. `GeneralName::dNSName`.
    const DNS_NAME_TAG: u8 = 0x82;

    /// Reads a DER length at `pos`, returning `(length, bytes consumed by the length field)`.
    fn read_len(data: &[u8], pos: usize) -> Option<(usize, usize)> {
        let first = *data.get(pos)?;
        if first & 0x80 == 0 {
            Some((first as usize, 1))
        } else {
            let n = (first & 0x7F) as usize;
            if n == 0 || n > std::mem::size_of::<usize>() {
                return None;
            }
            let bytes = data.get(pos + 1..pos + 1 + n)?;
            let mut len = 0usize;
            for b in bytes {
                len = len.checked_shl(8)?.checked_add(*b as usize)?;
            }
            Some((len, 1 + n))
        }
    }

    /// Finds the `subjectAltName` extension's value (the DER bytes of the `OCTET STRING`, which
    /// themselves encode `SEQUENCE OF GeneralName`), if present.
    fn find_san_extension_value(cert: &[u8]) -> Option<&[u8]> {
        let oid_pos = cert.windows(SAN_OID.len()).position(|w| w == SAN_OID)?;
        let mut pos = oid_pos + SAN_OID.len();
        // Optional `critical BOOLEAN DEFAULT FALSE`: tag 0x01, length 1, one value byte.
        if cert.get(pos) == Some(&0x01) && cert.get(pos + 1) == Some(&0x01) {
            pos += 3;
        }
        // `extnValue OCTET STRING`: tag 0x04.
        if cert.get(pos) != Some(&0x04) {
            return None;
        }
        let (len, consumed) = read_len(cert, pos + 1)?;
        let start = pos + 1 + consumed;
        cert.get(start..start + len)
    }

    /// Parses `SEQUENCE OF GeneralName` and collects the `dNSName` entries.
    pub(super) fn dns_sans(cert: &[u8]) -> Vec<String> {
        let Some(octet_string) = find_san_extension_value(cert) else {
            return Vec::new();
        };
        // `octet_string` is itself `SEQUENCE OF GeneralName`: tag 0x30, length, then entries.
        let Some(&0x30) = octet_string.first() else {
            return Vec::new();
        };
        let Some((seq_len, consumed)) = read_len(octet_string, 1) else {
            return Vec::new();
        };
        let start = 1 + consumed;
        let Some(mut body) = octet_string.get(start..start + seq_len) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Some(&tag) = body.first() {
            let Some((len, consumed)) = read_len(body, 1) else {
                break;
            };
            let value_start = 1 + consumed;
            let Some(value) = body.get(value_start..value_start + len) else {
                break;
            };
            if tag == DNS_NAME_TAG
                && let Ok(name) = std::str::from_utf8(value)
            {
                out.push(name.to_owned());
            }
            let Some(rest) = body.get(value_start + len..) else {
                break;
            };
            body = rest;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed_with_sans(sans: &[&str]) -> CertificateDer<'static> {
        let mut params =
            rcgen::CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .expect("valid SAN list");
        params.distinguished_name = rcgen::DistinguishedName::new();
        let key_pair = rcgen::KeyPair::generate().expect("keypair");
        let cert = params.self_signed(&key_pair).expect("self-sign");
        cert.der().clone()
    }

    #[test]
    fn extracts_dns_sans_from_a_generated_certificate() {
        let cert = self_signed_with_sans(&["hs-0.hs-mesh.svc.cluster.local", "hs-0"]);
        let sans = extract_dns_sans(&cert);
        assert_eq!(
            sans,
            vec![
                "hs-0.hs-mesh.svc.cluster.local".to_string(),
                "hs-0".to_string()
            ]
        );
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_content() {
        let a = self_signed_with_sans(&["a.example"]);
        let b = self_signed_with_sans(&["b.example"]);
        assert_eq!(fingerprint_sha256_hex(&a), fingerprint_sha256_hex(&a));
        assert_ne!(fingerprint_sha256_hex(&a), fingerprint_sha256_hex(&b));
        assert_eq!(fingerprint_sha256_hex(&a).len(), 64);
    }

    #[test]
    fn certificate_without_sans_yields_no_names() {
        let cert = self_signed_with_sans(&[]);
        assert!(extract_dns_sans(&cert).is_empty());
    }
}
