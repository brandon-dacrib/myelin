//! Mesh authentication: pluggable behind [`Authenticator`]. Two implementations ship, matching
//! `docs/rfcs/0001-cluster-ownership.md` section 11: mutual TLS for production, and a
//! constant-time shared secret for tests, development and single-process integration tests.
//!
//! Authorization is coarse by design: any authenticated peer may call any mesh route. All
//! replicas run the same binary under the same operator, so the mesh exists to keep everyone
//! else out, not to distinguish peers from each other.

use std::path::PathBuf;

use http::HeaderMap;
use subtle::ConstantTimeEq;

use crate::error::AuthError;

/// How the mesh authenticates peers. Chosen once at startup via [`crate::ClusterConfig`].
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// `Authorization: Bearer <secret>`, compared in constant time. Not for production over an
    /// untrusted network.
    SharedSecret {
        /// The shared secret every replica is configured with.
        secret: String,
    },
    /// Mutual TLS: the peer must present a certificate chained to `ca_file`, and this replica
    /// presents `cert_file` / `key_file` in turn.
    MutualTls {
        /// PEM file containing the cluster CA certificate(s).
        ca_file: PathBuf,
        /// PEM file containing this replica's certificate chain.
        cert_file: PathBuf,
        /// PEM file containing this replica's private key.
        key_file: PathBuf,
        /// If set, a peer's certificate SAN must end with this suffix (for example
        /// `.hs-mesh.svc.cluster.local`).
        peer_san_suffix: Option<String>,
    },
}

/// The identity of an authenticated mesh peer, as established by [`Authenticator::authenticate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerIdentity {
    /// Authenticated by the shared secret; there is no finer-grained identity in that mode.
    SharedSecret,
    /// Authenticated by a client certificate, identified by its SHA-256 fingerprint.
    Certificate {
        /// Lowercase hex SHA-256 fingerprint of the leaf certificate's DER encoding.
        fingerprint_sha256_hex: String,
    },
}

/// What a connection's TLS layer observed about the peer, handed to [`Authenticator::authenticate`]
/// when the connection is TLS. `None` (via the `tls` parameter) on a plaintext connection.
#[derive(Debug, Clone)]
pub struct TlsPeerInfo {
    /// SHA-256 fingerprint of the peer's leaf certificate, lowercase hex.
    pub fingerprint_sha256_hex: String,
    /// The subject alternative names on the peer's leaf certificate, as presented (DNS names
    /// only; this mesh has no use for IP SANs).
    pub sans: Vec<String>,
}

/// Authenticates one mesh connection or request. Implementations must not block; TLS handshake
/// verification happens beneath this trait (see [`crate::mesh::tls`]), so by the time
/// `authenticate` runs, a certificate's chain-to-CA validity is already established -- this trait
/// only adds the mesh-specific checks (secret comparison, SAN suffix).
pub trait Authenticator: Send + Sync {
    /// Authenticates a request, given its headers and, on a TLS connection, what the transport
    /// observed about the peer's certificate.
    ///
    /// # Errors
    /// Returns [`AuthError`] if authentication fails.
    fn authenticate(
        &self,
        headers: &HeaderMap,
        tls: Option<&TlsPeerInfo>,
    ) -> Result<PeerIdentity, AuthError>;
}

/// `Authorization: Bearer <secret>` compared in constant time via [`subtle`]. See
/// [`AuthMode::SharedSecret`].
pub struct SharedSecretAuthenticator {
    secret: String,
}

impl SharedSecretAuthenticator {
    /// Builds an authenticator that requires `secret`.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
        }
    }
}

impl Authenticator for SharedSecretAuthenticator {
    fn authenticate(
        &self,
        headers: &HeaderMap,
        _tls: Option<&TlsPeerInfo>,
    ) -> Result<PeerIdentity, AuthError> {
        let value = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError::Missing)?;
        let presented = value
            .strip_prefix("Bearer ")
            .ok_or_else(|| AuthError::Rejected("not a Bearer token".into()))?;
        let expected = self.secret.as_bytes();
        let got = presented.as_bytes();
        // Constant-time comparison requires equal length; padding the comparison itself would
        // leak length, so a length mismatch short-circuits (this is standard practice: token
        // length is not the secret).
        let matches = got.len() == expected.len() && bool::from(got.ct_eq(expected));
        if matches {
            Ok(PeerIdentity::SharedSecret)
        } else {
            Err(AuthError::Rejected("shared secret mismatch".into()))
        }
    }
}

/// Requires a client certificate chained to the cluster CA (verified by the TLS layer before this
/// runs) whose SAN, if [`AuthMode::MutualTls::peer_san_suffix`] is configured, ends with that
/// suffix. See [`crate::mesh::tls`] for the certificate loading and handshake configuration.
pub struct MutualTlsAuthenticator {
    peer_san_suffix: Option<String>,
}

impl MutualTlsAuthenticator {
    /// Builds an authenticator that additionally requires the peer's SAN to end with
    /// `peer_san_suffix`, when given.
    #[must_use]
    pub fn new(peer_san_suffix: Option<String>) -> Self {
        Self { peer_san_suffix }
    }
}

impl Authenticator for MutualTlsAuthenticator {
    fn authenticate(
        &self,
        _headers: &HeaderMap,
        tls: Option<&TlsPeerInfo>,
    ) -> Result<PeerIdentity, AuthError> {
        let tls = tls.ok_or(AuthError::Missing)?;
        if let Some(suffix) = &self.peer_san_suffix
            && !tls.sans.iter().any(|san| san.ends_with(suffix.as_str()))
        {
            return Err(AuthError::Rejected(format!(
                "peer SANs {:?} do not include a name ending in {suffix:?}",
                tls.sans
            )));
        }
        Ok(PeerIdentity::Certificate {
            fingerprint_sha256_hex: tls.fingerprint_sha256_hex.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    #[test]
    fn shared_secret_accepts_the_right_token() {
        let auth = SharedSecretAuthenticator::new("s3cr3t");
        assert_eq!(
            auth.authenticate(&headers_with_bearer("s3cr3t"), None)
                .unwrap(),
            PeerIdentity::SharedSecret
        );
    }

    #[test]
    fn shared_secret_rejects_the_wrong_token() {
        let auth = SharedSecretAuthenticator::new("s3cr3t");
        assert!(
            auth.authenticate(&headers_with_bearer("nope"), None)
                .is_err()
        );
    }

    #[test]
    fn shared_secret_rejects_missing_header() {
        let auth = SharedSecretAuthenticator::new("s3cr3t");
        assert!(matches!(
            auth.authenticate(&HeaderMap::new(), None),
            Err(AuthError::Missing)
        ));
    }

    #[test]
    fn mtls_requires_tls_info() {
        let auth = MutualTlsAuthenticator::new(None);
        assert!(matches!(
            auth.authenticate(&HeaderMap::new(), None),
            Err(AuthError::Missing)
        ));
    }

    #[test]
    fn mtls_checks_san_suffix() {
        let auth = MutualTlsAuthenticator::new(Some(".hs-mesh.svc.cluster.local".into()));
        let ok = TlsPeerInfo {
            fingerprint_sha256_hex: "ab".into(),
            sans: vec!["hs-0.hs-mesh.svc.cluster.local".into()],
        };
        let bad = TlsPeerInfo {
            fingerprint_sha256_hex: "ab".into(),
            sans: vec!["evil.example.org".into()],
        };
        assert!(auth.authenticate(&HeaderMap::new(), Some(&ok)).is_ok());
        assert!(auth.authenticate(&HeaderMap::new(), Some(&bad)).is_err());
    }
}
