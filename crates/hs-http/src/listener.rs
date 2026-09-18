//! Listener configuration: TCP, TLS (via `rustls`), and unix domain sockets, all servable through
//! [`serve`] with the same `axum::Router`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::serve::Listener;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;

/// How the server accepts connections for one listener (a homeserver typically runs several: a
/// client listener, a federation listener, and an admin listener, each independently configured).
#[derive(Debug, Clone)]
pub enum ListenerConfig {
    Tcp {
        addr: SocketAddr,
    },
    Tls {
        addr: SocketAddr,
        cert_path: PathBuf,
        key_path: PathBuf,
    },
    Unix {
        path: PathBuf,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    #[error("could not bind: {0}")]
    Bind(#[from] std::io::Error),
    #[error("could not load TLS certificate or key: {0}")]
    Tls(String),
    #[error("serve loop exited: {0}")]
    Serve(std::io::Error),
}

fn load_certs(path: &PathBuf) -> Result<Vec<CertificateDer<'static>>, ListenerError> {
    let bytes = std::fs::read(path)
        .map_err(|e| ListenerError::Tls(format!("reading {}: {e}", path.display())))?;
    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut bytes.as_slice()).collect();
    certs.map_err(|e| ListenerError::Tls(format!("parsing certificate {}: {e}", path.display())))
}

fn load_key(path: &PathBuf) -> Result<PrivateKeyDer<'static>, ListenerError> {
    let bytes = std::fs::read(path)
        .map_err(|e| ListenerError::Tls(format!("reading {}: {e}", path.display())))?;
    rustls_pemfile::private_key(&mut bytes.as_slice())
        .map_err(|e| ListenerError::Tls(format!("parsing key {}: {e}", path.display())))?
        .ok_or_else(|| ListenerError::Tls(format!("no private key found in {}", path.display())))
}

/// A [`Listener`] that terminates TLS on each accepted TCP connection before handing the
/// plaintext stream to axum. Accept errors and failed handshakes are logged and do not stop the
/// listener (matching `TcpListener`'s own `Listener` impl in axum).
struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.tcp.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "TCP accept failed");
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, addr),
                Err(e) => {
                    tracing::warn!(error = %e, %addr, "TLS handshake failed");
                    continue;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Serves `router` on the given listener configuration until the process is killed. Each variant
/// binds its own socket; callers spawn one task per configured listener.
pub async fn serve(config: ListenerConfig, router: Router) -> Result<(), ListenerError> {
    match config {
        ListenerConfig::Tcp { addr } => {
            let listener = TcpListener::bind(addr).await?;
            tracing::info!(%addr, "listening (tcp)");
            axum::serve(listener, router.into_make_service())
                .await
                .map_err(ListenerError::Serve)
        }
        ListenerConfig::Tls {
            addr,
            cert_path,
            key_path,
        } => {
            let certs = load_certs(&cert_path)?;
            let key = load_key(&key_path)?;
            let mut server_config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .map_err(|e| ListenerError::Tls(e.to_string()))?;
            server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            let tcp = TcpListener::bind(addr).await?;
            tracing::info!(%addr, "listening (tls)");
            let listener = TlsListener { tcp, acceptor };
            axum::serve(listener, router.into_make_service())
                .await
                .map_err(ListenerError::Serve)
        }
        ListenerConfig::Unix { path } => {
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            let listener = UnixListener::bind(&path)?;
            tracing::info!(path = %path.display(), "listening (unix)");
            axum::serve(listener, router.into_make_service())
                .await
                .map_err(ListenerError::Serve)
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::get;

    use super::*;

    #[tokio::test]
    async fn tcp_listener_serves_a_request() {
        let router = Router::new().route("/ping", get(|| async { "pong" }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });
        // give the server a moment to start accepting
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let response = reqwest_get(addr).await;
        assert_eq!(response, "pong");
        server.abort();
    }

    /// A tiny hand-rolled GET so this test does not need an HTTP client dependency.
    async fn reqwest_get(addr: SocketAddr) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        text.rsplit("\r\n").next().unwrap_or_default().to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_binds_and_removes_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hs-admin.sock");
        std::fs::write(&path, b"stale").unwrap();
        let listener = UnixListener::bind(&path);
        // a stale non-socket file makes bind fail; the real `serve()` path removes it first.
        assert!(listener.is_err());
        std::fs::remove_file(&path).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
    }
}
