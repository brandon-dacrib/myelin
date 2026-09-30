//! TLS for the PostgreSQL backend: libpq's `sslmode` semantics over `rustls`.
//!
//! The synchronous `postgres` crate (see [`crate::postgres_backend`]) takes a
//! [`postgres::tls::MakeTlsConnect`] at connect time and drives the `SSLRequest` negotiation
//! itself: with [`postgres::config::SslMode::Disable`] it never asks; with `Prefer` it asks and
//! carries on in the clear if the server declines; with `Require` it fails when the server
//! declines. What the *connector* does with the server's certificate is this module's business,
//! and it is what separates libpq's five modes:
//!
//! | [`PgSslMode`] | negotiation | certificate | host name |
//! |---|---|---|---|
//! | `disable` | never | — | — |
//! | `prefer` | if offered | not checked | not checked |
//! | `require` | required | not checked | not checked |
//! | `verify-ca` | required | chained to the CA | not checked |
//! | `verify-full` | required | chained to the CA | must match |
//!
//! `prefer` and `require` encrypt the wire without authenticating the peer, exactly as libpq
//! does (its documentation calls them "protection against eavesdropping" only). The two verify
//! modes chain the server's certificate to the CA file [`PgTlsOptions::root_cert`] names, or to
//! the platform's trust store when no file is given. libpq additionally promotes `require` to
//! `verify-ca` when a root certificate file happens to exist; this module does not, because an
//! implicit upgrade is exactly the kind of behavior an operator reading `ssl_mode: require` in a
//! file would not expect. Ask for `verify-ca` when that is what is wanted.
//!
//! The connector is a small `rustls` adapter written here rather than a third-party
//! `tokio-postgres-rustls` dependency: the workspace already standardizes on `rustls` with the
//! `ring` provider (`hs-cluster`'s mesh, `hs-federation`'s client), the adapter is under a
//! hundred lines, and it keeps the crate's dependency tree to crates the workspace already builds.
//! It reports no channel binding ([`postgres::tls::ChannelBinding::none`]), so SCRAM
//! authentication runs as `SCRAM-SHA-256`, not `SCRAM-SHA-256-PLUS`; PostgreSQL accepts either.

use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsConnector;

/// libpq's `sslmode` values, the ones [`crate::postgres_backend::PostgresBackend`] honours. See
/// the module docs for what each one checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PgSslMode {
    /// Never negotiate TLS; the connection is in the clear.
    Disable,
    /// Negotiate TLS when the server offers it, without checking its certificate; carry on in
    /// the clear when it does not. libpq's default, and this backend's.
    #[default]
    Prefer,
    /// Require TLS, without checking the server's certificate. Fails when the server does not
    /// offer TLS.
    Require,
    /// Require TLS and a server certificate chained to the trusted CA; do not check that the
    /// certificate names the host connected to.
    VerifyCa,
    /// Require TLS, a server certificate chained to the trusted CA, and that the certificate
    /// names the host connected to.
    VerifyFull,
}

impl PgSslMode {
    /// The libpq spelling (`disable`, `prefer`, `require`, `verify-ca`, `verify-full`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PgSslMode::Disable => "disable",
            PgSslMode::Prefer => "prefer",
            PgSslMode::Require => "require",
            PgSslMode::VerifyCa => "verify-ca",
            PgSslMode::VerifyFull => "verify-full",
        }
    }

    /// Parses the libpq spelling. `verify_ca`/`verify_full` (underscored) are accepted too.
    ///
    /// # Errors
    /// Returns the unrecognized input.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "disable" => Ok(PgSslMode::Disable),
            "prefer" => Ok(PgSslMode::Prefer),
            "require" => Ok(PgSslMode::Require),
            "verify-ca" | "verify_ca" => Ok(PgSslMode::VerifyCa),
            "verify-full" | "verify_full" => Ok(PgSslMode::VerifyFull),
            other => Err(other.to_owned()),
        }
    }

    /// Whether this mode refuses a connection the server will not encrypt.
    #[must_use]
    pub fn requires_tls(self) -> bool {
        !matches!(self, PgSslMode::Disable | PgSslMode::Prefer)
    }

    /// The negotiation the `postgres` crate should run for this mode. The certificate checks the
    /// verify modes add live in the connector, not in the negotiation.
    #[must_use]
    pub fn negotiation(self) -> postgres::config::SslMode {
        match self {
            PgSslMode::Disable => postgres::config::SslMode::Disable,
            PgSslMode::Prefer => postgres::config::SslMode::Prefer,
            PgSslMode::Require | PgSslMode::VerifyCa | PgSslMode::VerifyFull => {
                postgres::config::SslMode::Require
            }
        }
    }
}

impl fmt::Display for PgSslMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PgSslMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PgSslMode::parse(s)
    }
}

/// How the PostgreSQL backend should (or should not) encrypt its connections.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PgTlsOptions {
    /// Which of libpq's modes to apply.
    pub mode: PgSslMode,
    /// A PEM file of CA certificates the server's certificate must chain to, for the verify
    /// modes. When `None`, the verify modes use the platform's trust store. Ignored by
    /// `disable`, `prefer` and `require` (see the module docs).
    pub root_cert: Option<PathBuf>,
}

/// A TLS configuration or handshake failure the requested [`PgSslMode`] could not tolerate.
/// Carries the mode so a caller can name the setting that asked for it.
#[derive(Debug)]
pub struct PgTlsError {
    /// The mode that was asked for.
    pub mode: PgSslMode,
    /// What went wrong, in the underlying library's words.
    pub detail: String,
}

impl fmt::Display for PgTlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ssl_mode = {} could not be satisfied: {}",
            self.mode, self.detail
        )
    }
}

impl std::error::Error for PgTlsError {}

/// Whether a `postgres::Error` came from the TLS negotiation or handshake.
///
/// `postgres::Error` keeps its kind private, and the only public trace of `Kind::Tls` is the
/// fixed text its `Display` writes (`"error performing TLS handshake"`), so that text is what
/// this checks. The server declining `SSLRequest` under `require` reports this way too, with
/// `"server does not support TLS"` as the cause.
#[must_use]
pub fn is_tls_error(err: &postgres::Error) -> bool {
    err.to_string()
        .starts_with("error performing TLS handshake")
}

/// An error and every cause under it, joined with `: `. `postgres::Error`'s own `Display` stops
/// at its kind ("error performing TLS handshake") and keeps the reason — the server declined, the
/// certificate did not verify, and why — one `source()` down, which is the part worth reading.
#[must_use]
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut cause = err.source();
    while let Some(e) = cause {
        text.push_str(": ");
        text.push_str(&e.to_string());
        cause = e.source();
    }
    text
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Reads every certificate in a PEM file into a root store.
fn root_store_from_file(path: &Path) -> Result<RootCertStore, PgTlsError> {
    let describe = |detail: String| PgTlsError {
        mode: PgSslMode::VerifyCa,
        detail,
    };
    let file = std::fs::File::open(path)
        .map_err(|e| describe(format!("cannot read root certificate file {path:?}: {e}")))?;
    let mut reader = io::BufReader::new(file);
    let mut roots = RootCertStore::empty();
    let mut count = 0usize;
    for cert in rustls_pemfile::certs(&mut reader) {
        let cert = cert.map_err(|e| {
            describe(format!(
                "root certificate file {path:?} is not PEM certificates: {e}"
            ))
        })?;
        roots.add(cert).map_err(|e| {
            describe(format!(
                "root certificate file {path:?} holds a certificate that is not a usable trust anchor: {e}"
            ))
        })?;
        count += 1;
    }
    if count == 0 {
        return Err(describe(format!(
            "root certificate file {path:?} holds no certificates"
        )));
    }
    Ok(roots)
}

/// The platform's trust store, for a verify mode without a root certificate file.
fn root_store_from_platform() -> Result<RootCertStore, PgTlsError> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    for cert in loaded.certs {
        // One unparseable platform certificate should not disable the rest of the store; the
        // strict reading is reserved for a file the operator pointed at explicitly.
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        let errors: Vec<String> = loaded.errors.iter().map(ToString::to_string).collect();
        return Err(PgTlsError {
            mode: PgSslMode::VerifyCa,
            detail: format!(
                "no root certificates in the platform trust store, and no root certificate file \
                 was given{}",
                if errors.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", errors.join("; "))
                }
            ),
        });
    }
    Ok(roots)
}

/// Accepts any server certificate. Used by `prefer` and `require`, whose contract (libpq's) is
/// encryption without authentication. Handshake signatures are still checked against the
/// presented certificate, so a garbled handshake fails rather than being waved through.
#[derive(Debug)]
struct AcceptAnyServerCert {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `verify-ca`: the full WebPKI chain check, with the one host-name error forgiven.
#[derive(Debug)]
struct ChainOnlyVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl ServerCertVerifier for ChainOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Builds the `rustls` client configuration `options` asks for.
///
/// # Errors
/// Returns [`PgTlsError`] when a verify mode's root certificates cannot be loaded (the file is
/// missing, is not PEM, or holds nothing usable; or the platform trust store is empty).
pub fn client_config(options: &PgTlsOptions) -> Result<Arc<ClientConfig>, PgTlsError> {
    let provider = provider();
    let describe = |e: rustls::Error| PgTlsError {
        mode: options.mode,
        detail: e.to_string(),
    };
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(describe)?;
    let config = match options.mode {
        PgSslMode::Disable | PgSslMode::Prefer | PgSslMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
            .with_no_client_auth(),
        PgSslMode::VerifyCa | PgSslMode::VerifyFull => {
            let roots = match &options.root_cert {
                Some(path) => root_store_from_file(path),
                None => root_store_from_platform(),
            }
            .map_err(|e| PgTlsError {
                mode: options.mode,
                ..e
            })?;
            if options.mode == PgSslMode::VerifyFull {
                builder.with_root_certificates(roots).with_no_client_auth()
            } else {
                let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
                    Arc::new(roots),
                    provider,
                )
                .build()
                .map_err(|e| PgTlsError {
                    mode: options.mode,
                    detail: e.to_string(),
                })?;
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(ChainOnlyVerifier { inner }))
                    .with_no_client_auth()
            }
        }
    };
    Ok(Arc::new(config))
}

/// A [`MakeTlsConnect`] over `rustls`, for the `postgres` crate and `r2d2_postgres`.
#[derive(Clone)]
pub struct RustlsConnector {
    config: Arc<ClientConfig>,
}

impl fmt::Debug for RustlsConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RustlsConnector")
    }
}

impl RustlsConnector {
    /// A connector over `config`.
    #[must_use]
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self { config }
    }
}

impl<S> MakeTlsConnect<S> for RustlsConnector
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> Result<RustlsConnect, io::Error> {
        // `domain` is the host the `postgres` crate is connecting to, as configured: a DNS name
        // or an IP address literal. `rustls` verifies either against the certificate's SANs.
        let name = ServerName::try_from(domain.to_owned()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{domain:?} is not a usable TLS server name: {e}"),
            )
        })?;
        Ok(RustlsConnect {
            connector: TlsConnector::from(self.config.clone()),
            name,
        })
    }
}

/// One handshake's worth of [`TlsConnect`], made by [`RustlsConnector`].
pub struct RustlsConnect {
    connector: TlsConnector,
    name: ServerName<'static>,
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<RustlsStream<S>, io::Error>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            self.connector
                .connect(self.name, stream)
                .await
                .map(RustlsStream)
        })
    }
}

/// A TLS-wrapped PostgreSQL connection.
pub struct RustlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for RustlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RustlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream for RustlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_through_their_libpq_spelling() {
        for mode in [
            PgSslMode::Disable,
            PgSslMode::Prefer,
            PgSslMode::Require,
            PgSslMode::VerifyCa,
            PgSslMode::VerifyFull,
        ] {
            assert_eq!(PgSslMode::parse(mode.as_str()), Ok(mode));
            assert_eq!(mode.to_string().parse::<PgSslMode>(), Ok(mode));
        }
        assert_eq!(PgSslMode::parse("verify_full"), Ok(PgSslMode::VerifyFull));
        assert_eq!(PgSslMode::parse("allow"), Err("allow".to_owned()));
        assert_eq!(PgSslMode::default(), PgSslMode::Prefer);
    }

    #[test]
    fn only_disable_and_prefer_tolerate_a_plain_server() {
        assert!(!PgSslMode::Disable.requires_tls());
        assert!(!PgSslMode::Prefer.requires_tls());
        assert!(PgSslMode::Require.requires_tls());
        assert!(PgSslMode::VerifyCa.requires_tls());
        assert!(PgSslMode::VerifyFull.requires_tls());
        assert_eq!(
            PgSslMode::VerifyCa.negotiation(),
            postgres::config::SslMode::Require
        );
        assert_eq!(
            PgSslMode::Disable.negotiation(),
            postgres::config::SslMode::Disable
        );
    }

    #[test]
    fn the_unverified_modes_build_without_any_roots() {
        for mode in [PgSslMode::Disable, PgSslMode::Prefer, PgSslMode::Require] {
            client_config(&PgTlsOptions {
                mode,
                root_cert: None,
            })
            .unwrap_or_else(|e| panic!("{mode} needs no roots: {e}"));
        }
    }

    #[test]
    fn a_verify_mode_names_a_missing_or_empty_root_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        let err = client_config(&PgTlsOptions {
            mode: PgSslMode::VerifyFull,
            root_cert: Some(missing.clone()),
        })
        .unwrap_err();
        assert_eq!(err.mode, PgSslMode::VerifyFull);
        assert!(err.detail.contains("missing.pem"), "{err}");
        assert!(
            err.to_string().starts_with("ssl_mode = verify-full"),
            "{err}"
        );

        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        let err = client_config(&PgTlsOptions {
            mode: PgSslMode::VerifyCa,
            root_cert: Some(empty),
        })
        .unwrap_err();
        assert_eq!(err.mode, PgSslMode::VerifyCa);
        assert!(err.detail.contains("no certificates"), "{err}");
    }

    #[test]
    fn a_verify_mode_accepts_a_pem_root_file() {
        // A self-signed certificate is its own trust anchor; `rcgen` is not a dependency here, so
        // this is a fixed PEM made once with `openssl req -x509` (a 2048-bit RSA key, CN=test).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.pem");
        std::fs::write(&path, TEST_ROOT_PEM).unwrap();
        client_config(&PgTlsOptions {
            mode: PgSslMode::VerifyCa,
            root_cert: Some(path),
        })
        .expect("a PEM certificate is a usable root");
    }

    #[test]
    fn a_connector_takes_ip_literals_and_dns_names() {
        let config = client_config(&PgTlsOptions::default()).unwrap();
        let mut connector = RustlsConnector::new(config);
        for domain in ["127.0.0.1", "::1", "db.example.org", "localhost"] {
            let made: Result<RustlsConnect, io::Error> = <RustlsConnector as MakeTlsConnect<
                tokio::net::TcpStream,
            >>::make_tls_connect(
                &mut connector, domain
            );
            assert!(made.is_ok(), "{domain}");
        }
        let bad: Result<RustlsConnect, io::Error> = <RustlsConnector as MakeTlsConnect<
            tokio::net::TcpStream,
        >>::make_tls_connect(
            &mut connector, "not a host name"
        );
        assert!(bad.is_err());
    }

    const TEST_ROOT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIDFzCCAf+gAwIBAgIUP0pbdDfoHkx2TLJMGTB/PMNnxzAwDQYJKoZIhvcNAQEL
BQAwGjEYMBYGA1UEAwwPaHMta3YgdGVzdCByb290MCAXDTI2MDkzMDE3NDk0NFoY
DzIxMjYwOTA2MTc0OTQ0WjAaMRgwFgYDVQQDDA9ocy1rdiB0ZXN0IHJvb3QwggEi
MA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQDy4f3Cxi+FDYPAG7wgHl8OxtHg
PzrcehqS3i6CkWqghZi8Ib7GU9swT4XaTevEgbKpmoayHpq6uOCqs0c++5O2Se92
Vwwh8qH2UlO1OpjgRdzG1VSb/mUvAHkkiuBfCeSG8t7GucYuQ6SgPJJk9ElOqNM3
mBztWiKq2/sOyv6XA0KJZYC1/PmyDDCanwFvaGh2vS8DvmtSTmtmfDG9Ye1Hhhc0
eXyneECnH0z48XH9aMX6X1P9m565FFAmasBZOXofR3c5sagCkTdSje/yQlLhdDVp
8/ime7eqn8V9mr5sE5Wi29xcN85K4SlhG3Qrpj57EV07ADAEfFxbhngLRM0PAgMB
AAGjUzBRMB0GA1UdDgQWBBSyCTM5JGzibDRdwDumwr/VgkMMzTAfBgNVHSMEGDAW
gBSyCTM5JGzibDRdwDumwr/VgkMMzTAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3
DQEBCwUAA4IBAQDfepvh0N9R2rLdLwgI+R1JZeh1E6nZIRg75XjSfigo/Eju2BQ6
Mto668H19I71oJf/6OEvJEZIJ9dkf9raoz2kL5WhE8emmvr+pYUa5HXHlvlhKGzP
Le2OM2ouPwJI+dBtWwYhVeXdSIUosNlzATjHCdlss9qUJS1vo9/9kQEQaNhuRMt9
vYnm+X3DLUJf7ZmOZDHRGU4kESnU50malZBK+3TLq9VxJHS4vAmtRYXMlzsQM5jw
3bitTBu6JF1nxzRPfEYIiOfh9msH7SVd1oa13qS9K0aavw+cKtfNXq+KZMp5Wgw2
L2W7hb5CcFYJoDcptVZZ325ol4bigIgEbxAO
-----END CERTIFICATE-----
";
}
