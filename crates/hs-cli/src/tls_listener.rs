//! TLS termination for a listener that declares `tls:` (`hs_config::TlsConfig`): the server
//! serves HTTPS on it itself, with no proxy in front.
//!
//! Until 2026-10-09 a listener with `tls:` was served in plaintext with a warning, and every
//! deployment put nginx, Traefik or an Ingress in front for TLS. Those still work (and are what
//! a Kubernetes install uses); this is for the deployment with nothing in front, such as the
//! federation port of a small host, or the Myelin<->Synapse interop harness, which no longer
//! needs its nginx. The certificate chain and key are read once, at start; a renewed
//! certificate takes effect on the next start (as the mesh's does, `hs_cluster::mesh::tls`).
//!
//! [`TlsListener`] implements [`axum::serve::Listener`], so the same `axum::serve` with the same
//! graceful shutdown serves it. Handshakes run on their own tasks: a slow or hostile client in
//! the middle of its handshake does not hold up the next accept.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use axum::extract::{ConnectInfo, Request};
use axum::response::Response;
use axum::serve::IncomingStream;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tower::ServiceBuilder;
use tower::util::BoxCloneService;

/// Why a listener's TLS material could not be used.
#[derive(Debug, thiserror::Error)]
pub enum TlsListenerError {
    /// A PEM file could not be read.
    #[error("could not read {path}: {source}")]
    Read {
        /// The file.
        path: String,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A PEM file held nothing of the kind expected.
    #[error("{path} holds no {what}")]
    Empty {
        /// The file.
        path: String,
        /// `certificate` or `private key`.
        what: &'static str,
    },
    /// rustls refused the certificate and key together.
    #[error("the certificate and key could not be combined into a server configuration: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Reads the PEM certificate chain and private key `tls` names and builds the rustls server
/// configuration for them, advertising HTTP/2 and HTTP/1.1 through ALPN (hyper's automatic
/// server, which `axum::serve` uses, speaks both).
///
/// # Errors
/// Returns [`TlsListenerError`] when a file cannot be read, holds no certificate or key, or the
/// pair is refused by rustls (a key that does not match the certificate, for one).
pub fn load_server_config(
    tls: &hs_config::listeners::TlsConfig,
) -> Result<Arc<rustls::ServerConfig>, TlsListenerError> {
    let certs = read_certs(&tls.certificate_path)?;
    let key = read_key(&tls.private_key_path)?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsListenerError> {
    let pem = std::fs::read(path).map_err(|source| TlsListenerError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|source| TlsListenerError::Read {
            path: path.display().to_string(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsListenerError::Empty {
            path: path.display().to_string(),
            what: "certificate",
        });
    }
    Ok(certs)
}

fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsListenerError> {
    let pem = std::fs::read(path).map_err(|source| TlsListenerError::Read {
        path: path.display().to_string(),
        source,
    })?;
    rustls_pemfile::private_key(&mut pem.as_slice())
        .map_err(|source| TlsListenerError::Read {
            path: path.display().to_string(),
            source,
        })?
        .ok_or_else(|| TlsListenerError::Empty {
            path: path.display().to_string(),
            what: "private key",
        })
}

/// A bound TCP listener whose connections are handed to `axum::serve` only once their TLS
/// handshake has completed, each handshake on a task of its own.
pub struct TlsListener {
    local_addr: SocketAddr,
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    accept_loop: tokio::task::JoinHandle<()>,
}

impl TlsListener {
    /// Wraps `tcp`: an accept loop (on the current runtime) accepts connections and starts a
    /// handshake task for each; completed handshakes are queued for [`Listener::accept`]. A
    /// handshake that fails is logged at debug and dropped: a plaintext client on a TLS port,
    /// a scanner, a client that does not trust the certificate.
    ///
    /// # Errors
    /// Returns the I/O error if `tcp` has no local address.
    pub fn new(
        tcp: TcpListener,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<Self, std::io::Error> {
        let local_addr = tcp.local_addr()?;
        let acceptor = TlsAcceptor::from(config);
        let (tx, ready) = mpsc::channel(64);
        let accept_loop = tokio::spawn(async move {
            loop {
                let (stream, peer) = match tcp.accept().await {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        // The same backoff `axum::serve` applies to a transient accept error
                        // (file-descriptor exhaustion, a reset before accept).
                        tracing::warn!(listener = %local_addr, %error, "accept failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls) => {
                            // The receiver is gone only when the listener was dropped, which
                            // is shutdown; nothing to do with the connection then.
                            let _ = tx.send((tls, peer)).await;
                        }
                        Err(error) => {
                            tracing::debug!(listener = %local_addr, %peer, %error, "TLS handshake failed");
                        }
                    }
                });
            }
        });
        Ok(Self {
            local_addr,
            ready,
            accept_loop,
        })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accept_loop.abort();
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(accepted) => accepted,
            // Every sender is held by the accept loop and its handshake tasks; with the loop
            // aborted (only in `Drop`) and the last handshake done there is nothing more to
            // accept, and `axum::serve` must simply never be woken again.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

/// The make-service for a [`TlsListener`]: what `Router::into_make_service_with_connect_info`
/// is for a plain `TcpListener`, so handlers extract `ConnectInfo<SocketAddr>` (the rate
/// limiter's client address, for one) exactly as they do behind a plaintext listener. Axum's
/// own implements `Connected` for `SocketAddr` only over its `TcpListener`, and a foreign trait
/// cannot be implemented for a foreign type here, so this is the same thing by hand.
#[derive(Clone)]
pub struct WithConnectInfo {
    router: Router,
}

impl WithConnectInfo {
    /// Wraps `router`.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self { router }
    }
}

impl<'a> tower::Service<IncomingStream<'a, TlsListener>> for WithConnectInfo {
    // Boxed: the extension-adding wrapper axum uses for this is not a public type.
    type Response = BoxCloneService<Request, Response, Infallible>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, incoming: IncomingStream<'a, TlsListener>) -> Self::Future {
        let connect_info = ConnectInfo(*incoming.remote_addr());
        let service = ServiceBuilder::new()
            .layer(axum::Extension(connect_info))
            .service(self.router.clone());
        std::future::ready(Ok(BoxCloneService::new(service)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mint(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let cert_path = dir.join("tls.crt");
        let key_path = dir.join("tls.key");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        (cert_path, key_path, cert.pem())
    }

    #[test]
    fn a_pem_pair_becomes_a_server_config_that_offers_h2_and_http1() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key, _) = mint(dir.path());
        let config = load_server_config(&hs_config::listeners::TlsConfig {
            certificate_path: cert,
            private_key_path: key,
        })
        .unwrap();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn a_missing_or_empty_file_is_named_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _, _) = mint(dir.path());
        let missing = load_server_config(&hs_config::listeners::TlsConfig {
            certificate_path: cert.clone(),
            private_key_path: dir.path().join("nope.key"),
        })
        .unwrap_err();
        assert!(missing.to_string().contains("nope.key"), "{missing}");
        let empty = dir.path().join("empty.crt");
        std::fs::write(&empty, "").unwrap();
        let no_cert = load_server_config(&hs_config::listeners::TlsConfig {
            certificate_path: empty.clone(),
            private_key_path: cert,
        })
        .unwrap_err();
        assert!(
            no_cert.to_string().contains("holds no certificate"),
            "{no_cert}"
        );
    }

    #[test]
    fn a_key_that_does_not_match_the_certificate_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _, _) = mint(dir.path());
        let other = rcgen::KeyPair::generate().unwrap();
        let other_key = dir.path().join("other.key");
        std::fs::write(&other_key, other.serialize_pem()).unwrap();
        let err = load_server_config(&hs_config::listeners::TlsConfig {
            certificate_path: cert,
            private_key_path: other_key,
        })
        .unwrap_err();
        assert!(matches!(err, TlsListenerError::Rustls(_)), "{err}");
    }
}
