//! `hs serve`: builds the router (`hs-auth`'s legacy client routes plus health and metrics
//! endpoints), binds every configured listener, and serves until asked to shut down.
//!
//! Split out from [`crate::cli`] so the `hs-cli` end-to-end test
//! (`tests/e2e.rs`) can boot a real server in-process — bind to an ephemeral port, register a
//! user through the real HTTP router, log in, hit `/health/ready` — without going through a
//! subprocess or `main`'s process-level signal handling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::watch;

use hs_auth::state::AuthState;
use hs_telemetry::metrics::Metrics;

use crate::config_bridge;
use crate::storage::{self, OpenedStorage};

/// Errors starting the server.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Bridging the native config to `hs-auth`'s own config type failed.
    #[error(transparent)]
    Bridge(#[from] config_bridge::BridgeError),
    /// Opening the configured storage backend failed.
    #[error(transparent)]
    Storage(#[from] storage::StorageOpenError),
    /// A configured listener address could not be bound.
    #[error("failed to bind listener {addr}: {source}")]
    Bind {
        /// The address that failed to bind.
        addr: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// `config.listeners.listeners` was empty after validation (should not happen: `hs-config`
    /// itself rejects an empty listener list, but this is checked again here since a caller
    /// could in principle hand-build a `Config` value that skipped validation).
    #[error("no listeners configured")]
    NoListeners,
}

/// Shared application state the `/health/*` and `/metrics` handlers in [`build_router`] close
/// over. `hs-auth`'s router carries its own [`AuthState`] separately (mounted with its own
/// `.with_state` call below) rather than through this struct, since it is a `Router<AuthState>`
/// fragment this crate does not otherwise touch.
#[derive(Clone)]
struct AppState {
    metrics: Arc<Metrics>,
    ready: Arc<AtomicBool>,
}

/// Builds the full application router: `hs-auth`'s legacy client routes mounted under both
/// `/_matrix/client/v3` and `/_matrix/client/r0` (the historical version alias every Matrix
/// client-server API implementation supports, per `docs/compat/cli-shims.md`'s summary table and
/// `PLAN.md`), plus `/health/live`, `/health/ready` and `/metrics`.
///
/// Every configured listener serves this same router regardless of its declared `resources`
/// list — per-listener resource filtering (splitting `client`/`federation`/`media`/`metrics`
/// traffic onto different sockets, the way Synapse's `listeners[].resources` does) is not
/// implemented yet; see `docs/status/12-platform-and-kubernetes.md`.
fn build_router(auth: AuthState, metrics: Arc<Metrics>, ready: Arc<AtomicBool>) -> Router {
    let auth_router = hs_auth::routes::router().with_state(auth);
    let state = AppState { metrics, ready };

    Router::new()
        .nest("/_matrix/client/v3", auth_router.clone())
        .nest("/_matrix/client/r0", auth_router)
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
        .layer(hs_telemetry::RequestIdLayer::new())
}

async fn health_live() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn health_ready(State(state): State<AppState>) -> impl IntoResponse {
    if state.ready.load(Ordering::SeqCst) {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
    }
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.metrics.encode_to_string() {
        Ok(text) => (
            StatusCode::OK,
            [(http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            text,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// A running server: the addresses it actually bound (useful when a configured port is `0`, as
/// in tests), a handle to trigger graceful shutdown, and a join handle that resolves once every
/// listener has finished draining.
pub struct ServeHandle {
    /// Every address the server ended up bound to, one per `(bind_address, port)` pair across
    /// every configured listener.
    pub addrs: Vec<SocketAddr>,
    shutdown_tx: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
    _storage: OpenedStorage,
}

impl ServeHandle {
    /// Signals every listener to begin graceful shutdown and waits for them to finish draining
    /// in-flight requests.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join.await;
    }

    /// The base URL of the first bound listener (`http://127.0.0.1:PORT`), for a caller (tests,
    /// `hs register` against a just-started local server) that just wants "the" address.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!(
            "http://{}",
            self.addrs.first().expect("at least one listener is bound")
        )
    }
}

/// Builds the router, opens the configured storage backend, binds every listener and starts
/// serving. Returns once every listener is bound and accepting connections (before the first
/// request is necessarily handled — readiness is signaled separately via `/health/ready`, which
/// this function flips true immediately since nothing in this milestone's startup path is
/// asynchronous enough to need a real warm-up gate; see `docs/status/12-platform-and-kubernetes.md`
/// for what a real readiness gate should eventually wait on).
///
/// # Errors
/// See [`ServeError`].
pub async fn spawn_serve(config: hs_config::Config) -> Result<ServeHandle, ServeError> {
    if config.listeners.listeners.is_empty() {
        return Err(ServeError::NoListeners);
    }

    let opened_storage = storage::open_storage(&config.storage)?;

    let auth_config = config_bridge::auth_config_from(&config)?;
    let auth_state = AuthState::in_memory_with_config(auth_config);
    let metrics = Arc::new(Metrics::new());
    let ready = Arc::new(AtomicBool::new(true));

    let app = build_router(auth_state, metrics, ready);

    let mut listeners = Vec::new();
    for listener_cfg in &config.listeners.listeners {
        for bind_address in &listener_cfg.bind_addresses {
            let addr_str = format!("{bind_address}:{}", listener_cfg.port);
            let socket_addr: SocketAddr = normalize_bind_address(bind_address, listener_cfg.port)
                .parse()
                .map_err(|_| ServeError::Bind {
                    addr: addr_str.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "not a valid socket address",
                    ),
                })?;
            let tcp = TcpListener::bind(socket_addr)
                .await
                .map_err(|source| ServeError::Bind {
                    addr: addr_str.clone(),
                    source,
                })?;
            let actual_addr = tcp.local_addr().map_err(|source| ServeError::Bind {
                addr: addr_str.clone(),
                source,
            })?;
            if listener_cfg.tls.is_some() {
                tracing::warn!(
                    listener = %addr_str,
                    "listener declares TLS but hs serve does not terminate TLS yet; \
                     serving plaintext. Terminate TLS at a reverse proxy in front of this listener."
                );
            }
            listeners.push((tcp, actual_addr));
        }
    }

    let addrs: Vec<SocketAddr> = listeners.iter().map(|(_, addr)| *addr).collect();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut tasks = tokio::task::JoinSet::new();
    for (tcp, addr) in listeners {
        let app = app.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tasks.spawn(async move {
            let result = axum::serve(tcp, app.into_make_service())
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await;
            if let Err(e) = result {
                tracing::error!(listener = %addr, error = %e, "listener task exited with an error");
            }
        });
    }

    let join = tokio::spawn(async move { while tasks.join_next().await.is_some() {} });

    Ok(ServeHandle {
        addrs,
        shutdown_tx,
        join,
        _storage: opened_storage,
    })
}

/// `hs-config`'s `bind_addresses` defaults to `"::"` (all interfaces, IPv6-mapped), matching
/// Synapse's own default; [`SocketAddr`]'s `FromStr` wants `"[::]"` bracket syntax for the
/// unspecified IPv6 address, so bare `"::"` and other bracket-less IPv6 literals are normalized
/// before parsing.
fn normalize_bind_address(bind_address: &str, port: u16) -> String {
    if bind_address.contains(':') && !bind_address.starts_with('[') {
        format!("[{bind_address}]:{port}")
    } else {
        format!("{bind_address}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hs-config` itself rejects `port: 0` (`ListenersConfig::validate`: "0 is not a bindable
    /// port; choose an explicit port") since it is meaningful for a real deployment to reject —
    /// an operator who wrote `port: 0` almost certainly meant to write a real port. Tests still
    /// want an OS-assigned ephemeral port, so this reserves one the same way the OS would (bind
    /// to `127.0.0.1:0`, read back the assigned port, then release it) and hands that concrete
    /// port number to `hs-config`. This has an unavoidable, harmless TOCTOU race against another
    /// process grabbing the same port before `spawn_serve` rebinds it; acceptable for a test.
    fn reserve_ephemeral_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Each test needs its own embedded storage directory: `hs-config`'s default (`./data`) is
    /// shared across the whole process, and Fjall exclusively locks its data directory, so two
    /// tests opening it concurrently would collide. The returned `TempDir` must be kept alive
    /// (its `Drop` removes the directory) for as long as the server using it is running.
    fn test_config(port: u16) -> (hs_config::Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            "server:\n  server_name: example.org\nlisteners:\n  listeners:\n    - port: {port}\n      bind_addresses: [\"127.0.0.1\"]\n      resources: [client, health, metrics]\nauth:\n  enable_registration: true\nstorage:\n  backend: embedded\n  data_dir: {:?}\n",
            dir.path()
        );
        (hs_config::Config::from_yaml(&yaml).unwrap(), dir)
    }

    #[tokio::test]
    async fn binds_an_ephemeral_port_and_reports_it() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let handle = spawn_serve(config).await.unwrap();
        assert_eq!(handle.addrs.len(), 1);
        assert_ne!(handle.addrs[0].port(), 0);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn health_live_and_ready_respond_ok() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let handle = spawn_serve(config).await.unwrap();
        let base = handle.base_url();
        let client = reqwest::Client::new();
        let live = client
            .get(format!("{base}/health/live"))
            .send()
            .await
            .unwrap();
        assert_eq!(live.status(), reqwest::StatusCode::OK);
        let ready = client
            .get(format!("{base}/health/ready"))
            .send()
            .await
            .unwrap();
        assert_eq!(ready.status(), reqwest::StatusCode::OK);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_prometheus_text() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let handle = spawn_serve(config).await.unwrap();
        let base = handle.base_url();
        let res = reqwest::get(format!("{base}/metrics")).await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; version=0.0.4")
        );
        handle.shutdown().await;
    }

    #[test]
    fn normalizes_unbracketed_ipv6_bind_addresses() {
        assert_eq!(normalize_bind_address("::", 8008), "[::]:8008");
        assert_eq!(normalize_bind_address("127.0.0.1", 8008), "127.0.0.1:8008");
        assert_eq!(normalize_bind_address("[::1]", 8008), "[::1]:8008");
    }
}
