//! `hs serve`: builds the router (`GET /_matrix/client/versions`, `GET
//! /_matrix/client/v3/capabilities`, `hs-auth`'s legacy client routes, health and metrics
//! endpoints), binds every configured listener, and serves until asked to shut down.
//!
//! Built via `hs_http::router::Builder` rather than a bare `axum::Router`, so every route
//! registered here also produces a `routes.json` entry (`docs/rfcs/0005-routes-json-manifest.md`)
//! — see [`route_manifest`] and `--routes-manifest` on `hs serve` / the `hs routes-manifest`
//! subcommand.
//!
//! Split out from [`crate::cli`] so the `hs-cli` end-to-end test
//! (`tests/e2e.rs`) can boot a real server in-process — bind to an ephemeral port, register a
//! user through the real HTTP router, log in, hit `/health/ready` — without going through a
//! subprocess or `main`'s process-level signal handling.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::Extension;
use axum::middleware;
use axum::response::IntoResponse;
use http::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::watch;

use hs_auth::state::AuthState;
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_telemetry::metrics::Metrics;

use crate::config_bridge;
use crate::metrics_layer::track_metrics;
use crate::storage::{self, OpenedStorage};
use crate::versions;

/// Errors starting the server.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Bridging the native config to `hs-auth`'s own config type failed.
    #[error(transparent)]
    Bridge(#[from] config_bridge::BridgeError),
    /// Converting the native config into `hs-auth`'s config type failed.
    #[error(transparent)]
    AuthConfig(#[from] hs_auth::config::ConfigConversionError),
    /// Opening the persistent authentication store failed.
    #[error(transparent)]
    AuthStore(#[from] hs_auth::store::StoreError),
    /// Opening the configured storage backend failed.
    #[error(transparent)]
    Storage(#[from] storage::StorageOpenError),
    /// Loading `--capabilities-config` failed.
    #[error(transparent)]
    Capabilities(#[from] versions::CapabilitiesConfigError),
    /// Writing `--routes-manifest` failed.
    #[error("failed to write routes manifest to {path:?}: {source}")]
    RoutesManifest {
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
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

/// Options controlling `hs serve` beyond the native config file itself — kept separate from
/// [`hs_config::Config`] because none of these belong in that crate's schema (see
/// `crate::versions`'s module doc for why `unstable_features` in particular cannot live there).
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// `--capabilities-config`: an optional YAML file overriding
    /// `crate::versions::default_unstable_features`.
    pub capabilities_config: Option<PathBuf>,
    /// `--routes-manifest`: an optional path to write the `routes.json` manifest to at startup.
    /// When `None`, the manifest is still computed (cheap: no I/O, no Kubernetes/network calls)
    /// but not written — use the `hs routes-manifest` subcommand to get it without booting a
    /// server at all.
    pub routes_manifest_path: Option<PathBuf>,
}

/// Builds the full application router and its `routes.json` manifest:
///
/// - `GET /_matrix/client/versions` ([`crate::versions::get_versions`]) — unauthenticated, no
///   version prefix (the one route in the whole Matrix client-server API that never gets one).
/// - `GET /_matrix/client/v3/capabilities` and the `r0` alias
///   ([`crate::capabilities::get_capabilities`]).
/// - `hs-auth`'s legacy client routes ([`hs_auth::routes::router`]), mounted under both
///   `/_matrix/client/v3` and `/_matrix/client/r0` (the historical version alias every Matrix
///   client-server API implementation supports, per `docs/compat/cli-shims.md`'s summary table
///   and `PLAN.md`).
/// - `/health/live`, `/health/ready`, `/metrics`.
///
/// Every configured listener serves this same router regardless of its declared `resources`
/// list — per-listener resource filtering (splitting `client`/`federation`/`media`/`metrics`
/// traffic onto different sockets, the way Synapse's `listeners[].resources` does) is not
/// implemented yet; see `docs/status/12-platform-and-kubernetes.md`.
fn build_router(
    auth: AuthState,
    metrics: Arc<Metrics>,
    ready: Arc<AtomicBool>,
    unstable_features: Arc<BTreeMap<String, bool>>,
) -> (Router, RouteManifest) {
    let auth_router = hs_auth::routes::router().with_state(auth);
    let auth_routes = crate::auth_manifest::routes();

    let (router, manifest) = Builder::<()>::new()
        .get(
            "/_matrix/client/versions",
            versions::get_versions,
            RouteMeta::new(Surface::MatrixClient, AuthKind::None).with_operation_id("getVersions"),
        )
        .get(
            "/_matrix/client/v3/capabilities",
            crate::capabilities::get_capabilities,
            // `AuthKind::None` reflects actual current behavior, not the spec's requirement:
            // this handler does not check for a token yet (see crate::capabilities's doc
            // comment). Marking it `Matrix` here would be exactly the kind of overclaiming this
            // track was told not to do for `/versions`.
            RouteMeta::new(Surface::MatrixClient, AuthKind::None)
                .with_operation_id("getCapabilities"),
        )
        .get(
            "/_matrix/client/r0/capabilities",
            crate::capabilities::get_capabilities,
            RouteMeta::new(Surface::MatrixClient, AuthKind::None)
                .with_operation_id("getCapabilities"),
        )
        .get(
            "/health/live",
            health_live,
            RouteMeta::new(Surface::Admin, AuthKind::None).with_operation_id("healthLive"),
        )
        .get(
            "/health/ready",
            health_ready,
            RouteMeta::new(Surface::Admin, AuthKind::None).with_operation_id("healthReady"),
        )
        .get(
            "/metrics",
            metrics_handler,
            RouteMeta::new(Surface::Admin, AuthKind::None).with_operation_id("metrics"),
        )
        .merge_router(
            "/_matrix/client/v3",
            auth_router.clone(),
            auth_routes.clone(),
        )
        .merge_router("/_matrix/client/r0", auth_router, auth_routes)
        .build();

    let router = router
        .layer(Extension(ready))
        .layer(Extension(unstable_features))
        .layer(Extension(metrics.clone()))
        .layer(middleware::from_fn_with_state(metrics, track_metrics))
        .layer(hs_telemetry::RequestIdLayer::new());

    (router, manifest)
}

/// The `routes.json` manifest [`build_router`] would produce, without needing a real
/// [`AuthState`] or config — routes are static, independent of runtime configuration, so this
/// builds one with throwaway in-memory state purely to read off the manifest [`Builder::build`]
/// records, then drops the router. Used by `hs serve --routes-manifest` and the standalone
/// `hs routes-manifest` subcommand.
#[must_use]
pub fn route_manifest() -> RouteManifest {
    let (_router, manifest) = build_router(
        AuthState::in_memory(),
        Arc::new(Metrics::new()),
        Arc::new(AtomicBool::new(true)),
        Arc::new(BTreeMap::new()),
    );
    manifest
}

async fn health_live() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn health_ready(Extension(ready): Extension<Arc<AtomicBool>>) -> impl IntoResponse {
    if ready.load(Ordering::SeqCst) {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
    }
}

async fn metrics_handler(Extension(metrics): Extension<Arc<Metrics>>) -> impl IntoResponse {
    match metrics.encode_to_string() {
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
pub async fn spawn_serve(
    config: hs_config::Config,
    options: ServeOptions,
) -> Result<ServeHandle, ServeError> {
    if config.listeners.listeners.is_empty() {
        return Err(ServeError::NoListeners);
    }

    let opened_storage = storage::open_storage(&config.storage)?;

    // Wired by the integration lead per docs/status/07-auth-and-identity.md "For track 12":
    // the persistent store replaces the in-memory one, so users, devices and tokens survive a
    // restart. `backend.clone()` is a cheap Arc-backed handle sharing the same open database.
    let auth_config = hs_auth::config::AuthConfig::try_from(&config)?;
    let auth_store: Arc<dyn hs_auth::store::AuthStore> = match &opened_storage {
        storage::OpenedStorage::Embedded(backend) => {
            Arc::new(hs_auth::store::tables::TablesAuthStore::open(backend.clone())?)
        }
    };
    let auth_state = AuthState::with_store(auth_store, auth_config);
    let metrics = Arc::new(Metrics::new());
    let ready = Arc::new(AtomicBool::new(true));
    let unstable_features = Arc::new(versions::load_unstable_features(
        options.capabilities_config.as_deref(),
    )?);

    let (app, manifest) = build_router(auth_state, metrics, ready, unstable_features);

    if let Some(path) = &options.routes_manifest_path {
        manifest
            .write_to_file(path)
            .map_err(|source| ServeError::RoutesManifest {
                path: path.clone(),
                source,
            })?;
        tracing::info!(path = %path.display(), routes = manifest.routes.len(), "wrote routes.json manifest");
    }

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
        let handle = spawn_serve(config, ServeOptions::default()).await.unwrap();
        assert_eq!(handle.addrs.len(), 1);
        assert_ne!(handle.addrs[0].port(), 0);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn health_live_and_ready_respond_ok() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let handle = spawn_serve(config, ServeOptions::default()).await.unwrap();
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
    async fn metrics_endpoint_serves_prometheus_text_and_has_data_after_traffic() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let handle = spawn_serve(config, ServeOptions::default()).await.unwrap();
        let base = handle.base_url();
        let client = reqwest::Client::new();

        // Generate some traffic before scraping, so the counters are non-empty.
        for _ in 0..3 {
            let _ = client.get(format!("{base}/health/live")).send().await;
        }

        let res = client.get(format!("{base}/metrics")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; version=0.0.4")
        );
        let body = res.text().await.unwrap();
        assert!(body.contains("hs_http_requests_total"), "{body}");
        assert!(body.contains("hs_http_request_duration_seconds"), "{body}");
        assert!(body.contains("route=\"/health/live\""), "{body}");
        assert!(body.contains("method=\"GET\""), "{body}");
        // The scrape request itself (GET /metrics) is only recorded *after* it completes, so it
        // never appears in its own body — asserting that keeps this test honest about what
        // "after traffic" actually proves.
        assert!(!body.contains("route=\"/metrics\" method=\"GET\""));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn versions_endpoint_responds_with_supported_versions_and_empty_features_by_default() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let handle = spawn_serve(config, ServeOptions::default()).await.unwrap();
        let base = handle.base_url();
        let res = reqwest::get(format!("{base}/_matrix/client/versions"))
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        let body: versions::VersionsResponse = res.json().await.unwrap();
        assert_eq!(body.versions, versions::supported_versions());
        assert!(body.unstable_features.is_empty());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn versions_endpoint_reflects_capabilities_config_overrides() {
        let (mut config, _dir) = test_config(reserve_ephemeral_port());
        let capabilities_dir = tempfile::tempdir().unwrap();
        let capabilities_path = capabilities_dir.path().join("capabilities.yaml");
        std::fs::write(
            &capabilities_path,
            "unstable_features:\n  org.matrix.msc9999: true\n",
        )
        .unwrap();
        // Reuse the same listener/storage config, just swap in a fresh port to avoid clashing
        // with other tests that might still be shutting down.
        config.listeners.listeners[0].port = reserve_ephemeral_port();

        let handle = spawn_serve(
            config,
            ServeOptions {
                capabilities_config: Some(capabilities_path),
                routes_manifest_path: None,
            },
        )
        .await
        .unwrap();
        let base = handle.base_url();
        let res = reqwest::get(format!("{base}/_matrix/client/versions"))
            .await
            .unwrap();
        let body: versions::VersionsResponse = res.json().await.unwrap();
        assert_eq!(
            body.unstable_features.get("org.matrix.msc9999"),
            Some(&true)
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn capabilities_endpoint_is_mounted_under_v3_and_r0() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let handle = spawn_serve(config, ServeOptions::default()).await.unwrap();
        let base = handle.base_url();
        let client = reqwest::Client::new();
        for prefix in ["v3", "r0"] {
            let res = client
                .get(format!("{base}/_matrix/client/{prefix}/capabilities"))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), reqwest::StatusCode::OK, "{prefix}");
            let body: serde_json::Value = res.json().await.unwrap();
            assert_eq!(body["capabilities"]["m.change_password"]["enabled"], true);
        }
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn routes_manifest_is_written_when_a_path_is_given() {
        let (config, _dir) = test_config(reserve_ephemeral_port());
        let out_dir = tempfile::tempdir().unwrap();
        let manifest_path = out_dir.path().join("routes.json");
        let handle = spawn_serve(
            config,
            ServeOptions {
                capabilities_config: None,
                routes_manifest_path: Some(manifest_path.clone()),
            },
        )
        .await
        .unwrap();
        let contents = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest: RouteManifest = serde_json::from_str(&contents).unwrap();
        assert!(
            manifest
                .routes
                .iter()
                .any(|r| r.path == "/_matrix/client/versions")
        );
        assert!(
            manifest
                .routes
                .iter()
                .any(|r| r.path == "/_matrix/client/v3/login")
        );
        handle.shutdown().await;
    }

    #[test]
    fn route_manifest_covers_versions_capabilities_and_both_version_prefixes() {
        let manifest = route_manifest();
        let paths: Vec<&str> = manifest.routes.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/_matrix/client/versions"));
        assert!(paths.contains(&"/_matrix/client/v3/capabilities"));
        assert!(paths.contains(&"/_matrix/client/r0/capabilities"));
        assert!(paths.contains(&"/_matrix/client/v3/login"));
        assert!(paths.contains(&"/_matrix/client/r0/login"));
        assert!(paths.contains(&"/health/live"));
        assert!(paths.contains(&"/metrics"));
    }

    #[test]
    fn normalizes_unbracketed_ipv6_bind_addresses() {
        assert_eq!(normalize_bind_address("::", 8008), "[::]:8008");
        assert_eq!(normalize_bind_address("127.0.0.1", 8008), "127.0.0.1:8008");
        assert_eq!(normalize_bind_address("[::1]", 8008), "[::1]:8008");
    }
}
