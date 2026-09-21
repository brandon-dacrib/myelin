//! `hs serve`: builds the router (`GET /_matrix/client/versions`, `GET
//! /_matrix/client/v3/capabilities`, `hs-auth`'s legacy client routes, `hs-room`'s room routes,
//! `hs-media`'s authenticated and legacy media routes, `hs-appservice`'s inbound ping route,
//! `hs-admin`'s `/api/v1` surface and management-interface assets, health and metrics endpoints),
//! binds every configured listener, and serves until asked to shut down.
//!
//! Built via `hs_http::router::Builder` rather than a bare `axum::Router`, so every route
//! registered here also produces a `routes.json` entry (`docs/rfcs/0005-routes-json-manifest.md`)
//! — see [`route_manifest`] and `--routes-manifest` on `hs serve` / the `hs routes-manifest`
//! subcommand. `hs-auth`, `hs-room` and `hs-appservice` each hand over a pre-built router
//! fragment rather than routing through `Builder` themselves, so their manifest entries are
//! hand-mirrored (`crate::auth_manifest`, `hs_room::routes::router`'s own `Builder` usage, and
//! `crate::appservice_manifest` respectively — see each for why). `hs-media` and `hs-admin`
//! already build through `Builder` internally, so their manifests come back for free.
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

use hs_appservice::ping::PingService;
use hs_auth::state::AuthState;
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;
use hs_media::state::MediaState;
use hs_room::state::RoomState;
use hs_telemetry::metrics::Metrics;
use hs_user::state::UserState;

use crate::config_bridge;
use crate::metrics_layer::track_metrics;
use crate::storage::{self};
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
    /// `server.server_name` or `server.signing_key_path` could not be turned into this server's
    /// room-actor identity.
    #[error("failed to build this server's room identity: {0}")]
    Identity(#[from] ruma::IdParseError),
    /// Opening `hs-room`'s room registry over the configured storage backend failed.
    #[error("failed to open the room registry: {0}")]
    Room(#[from] hs_kv::KvError),
    /// Opening the keyspaces `hs-user`, `hs-e2e` or `hs-push` need failed. One variant for all
    /// three because they fail the same way (a keyspace could not be opened on the configured
    /// backend) and the boxed source carries which one it was.
    #[error("failed to open session, encryption or push storage")]
    Sessions(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Building `hs-media`'s state (object store, metadata store, and — if
    /// `--media-scanning-config` was given — the content scanning engine) failed.
    #[error(transparent)]
    Media(#[from] crate::media::MediaSetupError),
    /// Loading `appservices.registration_files` failed.
    #[error(transparent)]
    Appservices(#[from] crate::appservices::LoadAppservicesError),
    /// Starting the `hs-cluster` ownership manager (or, when clustered, its mesh forwarder)
    /// failed.
    #[error(transparent)]
    Cluster(#[from] crate::cluster::ClusterSetupError),
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
/// `Debug` is written out rather than derived: a `ConfigSource` is a trait object and has no
/// `Debug` bound, and requiring one on every implementation to make this one line shorter would
/// be the tail wagging the dog.
#[derive(Clone, Default)]
pub struct ServeOptions {
    /// `--capabilities-config`: an optional YAML file overriding
    /// `crate::versions::default_unstable_features`.
    pub capabilities_config: Option<PathBuf>,
    /// `--routes-manifest`: an optional path to write the `routes.json` manifest to at startup.
    /// When `None`, the manifest is still computed (cheap: no I/O, no Kubernetes/network calls)
    /// but not written — use the `hs routes-manifest` subcommand to get it without booting a
    /// server at all.
    pub routes_manifest_path: Option<PathBuf>,
    /// `--media-scanning-config`: an optional `media.scanning` YAML file
    /// (`hs_media::scanning::ScanningConfig::from_yaml`'s shape — see `crate::media`'s module doc
    /// for why this cannot live in `-c`/`--config`'s native config file yet). Omitted means no
    /// content scanning is attached (`ScanningConfig::default()`'s `mode: off`, zero behavioral
    /// change).
    pub media_scanning_config: Option<PathBuf>,
    /// The configuration source the admin API writes through
    /// (`crate::config_source::StoreConfigSource`). `None` leaves every `/config*` operation
    /// answering an honest `503`, which is what a caller that has no store open -- the route
    /// manifest, the in-process tests -- should get.
    pub config_source: Option<Arc<dyn hs_admin::sources::ConfigSource>>,
}

impl std::fmt::Debug for ServeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeOptions")
            .field("capabilities_config", &self.capabilities_config)
            .field("routes_manifest_path", &self.routes_manifest_path)
            .field("media_scanning_config", &self.media_scanning_config)
            .field("config_source", &self.config_source.is_some())
            .finish()
    }
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
/// - `GET /.well-known/matrix/server` and `GET /.well-known/matrix/client`
///   ([`crate::well_known`]) — always registered, but each answers 404 unless its configuration
///   field is set, so an operator who does not delegate publishes no document.
/// - `/health/live`, `/health/ready`, `/metrics`.
///
/// - `hs-room`'s room routes ([`hs_room::routes::router`]), mounted under both `/_matrix/client/v3`
///   and `/_matrix/client/r0`, the same as `hs-auth`'s.
/// - `hs-media`'s authenticated routes ([`hs_media::router::authenticated_router`]) under
///   `/_matrix/client/v1/media`, and — when `media.allow_legacy_unauthenticated_media` is set —
///   its legacy routes ([`hs_media::router::legacy_router`]) under `/_matrix/media/v3`.
/// - `hs-appservice`'s inbound ping route ([`hs_appservice::routes::ping_router`]) under
///   `/_matrix/client/v1`.
/// - `hs-admin`'s `/api/v1` surface and `/admin/` management-interface assets
///   ([`hs_admin::router::build_router`]) — merged directly rather than through this function's
///   own `Builder`, since that function already builds through its own `Builder` internally and
///   its paths are absolute, not spec-relative (see its own doc comment).
/// - `/health/live`, `/health/ready`, `/metrics`.
///
/// Every configured listener serves this same router regardless of its declared `resources`
/// list — per-listener resource filtering (splitting `client`/`federation`/`media`/`metrics`
/// traffic onto different sockets, the way Synapse's `listeners[].resources` does) is not
/// implemented yet; see `docs/status/12-platform-and-kubernetes.md`.
fn build_router<B: KvBackend>(
    auth: AuthState,
    mounts: Mounts<B>,
    metrics: Arc<Metrics>,
    ready: Arc<AtomicBool>,
    unstable_features: Arc<BTreeMap<String, bool>>,
    well_known: crate::well_known::WellKnown,
    cluster: &crate::cluster::ClusterHandles,
) -> (Router, RouteManifest) {
    let auth_router = hs_auth::routes::router().with_state(auth.clone());
    let auth_routes = crate::auth_manifest::routes();

    // `hs-auth`'s shared-secret registration fragment spells its own absolute path
    // (`/_synapse/admin/v1/register`), so unlike the fragment above it is merged at the root
    // rather than under the two client-API version prefixes. It is what makes an admin exist at
    // all: `hs serve` has no other way to set a user's `is_admin` flag, and without that flag no
    // credential satisfies `hs_auth::admin_verifier::AdminTokenVerifier` below.
    let synapse_admin_router = hs_auth::synapse_admin_router().with_state(auth.clone());
    let synapse_admin_routes = crate::auth_manifest::synapse_admin_routes();

    let (room_router, room_manifest) = hs_room::routes::router::<B>();
    let room_router = room_router.with_state(mounts.room);
    let room_routes = room_manifest.routes;

    let legacy_media_enabled = mounts.media.legacy_media_enabled;
    let (media_router, media_manifest) = hs_media::router::authenticated_router::<B>();
    let media_router = media_router.with_state(mounts.media.clone());
    let media_routes = media_manifest.routes;

    let (user_router, user_manifest) =
        hs_user::routes::router::<B, Arc<hs_room::registry::RoomRegistry<B>>>();
    let user_router = user_router.with_state(mounts.user);
    let user_routes = user_manifest.routes;

    let (e2e_router, e2e_manifest) = hs_e2e::routes::router::<B>();
    let e2e_router = e2e_router.with_state(mounts.e2e.clone());
    let e2e_routes = e2e_manifest.routes;

    // MSC3983/MSC3984's appservice key proxies spell `unstable` in their own paths, so they mount
    // under `/_matrix/client/unstable` rather than alongside the versioned routes above.
    let (e2e_unstable_router, e2e_unstable_manifest) = hs_e2e::routes::unstable_router::<B>();
    let e2e_unstable_router = e2e_unstable_router.with_state(mounts.e2e);
    let e2e_unstable_routes = e2e_unstable_manifest.routes;

    let (push_router, push_manifest) = hs_push::routes::router::<B>();
    let push_router = push_router.with_state(mounts.push);
    let push_routes = push_manifest.routes;

    let ping_router =
        hs_appservice::routes::ping_router::<B>(mounts.appservice_ping).with_state(auth);
    let ping_routes = crate::appservice_manifest::routes();

    let federation = mounts.federation;

    let mut builder = Builder::<()>::new()
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
            "/.well-known/matrix/server",
            crate::well_known::get_server,
            RouteMeta::new(Surface::MatrixFederation, AuthKind::None)
                .with_operation_id("getWellKnownServer"),
        )
        .get(
            "/.well-known/matrix/client",
            crate::well_known::get_client,
            RouteMeta::new(Surface::MatrixClient, AuthKind::None)
                .with_operation_id("getWellKnownClient"),
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
        .merge_router(
            "/_matrix/client/v3",
            room_router.clone(),
            room_routes.clone(),
        )
        // `/relations` and `/threads` are defined by the spec under `v1`, not `v3`, and a real
        // client (and Complement) calls them there. Mounting the room router at `v1` as well is
        // what makes them reachable at all: they were implemented, tested, and still answered 404
        // in the running server until this line existed. The `v3`/`r0` mounts stay because every
        // other room route lives there; a route is only reachable under a prefix it is mounted on,
        // so this deliberately exposes the whole room router three times rather than splitting it.
        .merge_router(
            "/_matrix/client/v1",
            room_router.clone(),
            room_routes.clone(),
        )
        .merge_router("/_matrix/client/r0", room_router, room_routes)
        .merge_router(
            "/_matrix/client/v3",
            user_router.clone(),
            user_routes.clone(),
        )
        .merge_router("/_matrix/client/r0", user_router, user_routes)
        .merge_router("/_matrix/client/v3", e2e_router.clone(), e2e_routes.clone())
        .merge_router("/_matrix/client/r0", e2e_router, e2e_routes)
        .merge_router(
            "/_matrix/client/unstable",
            e2e_unstable_router,
            e2e_unstable_routes,
        )
        .merge_router(
            "/_matrix/client/v3",
            push_router.clone(),
            push_routes.clone(),
        )
        .merge_router("/_matrix/client/r0", push_router, push_routes)
        .merge_router("/_matrix/client/v1/media", media_router, media_routes)
        .merge_router("/_matrix/client/v1", ping_router, ping_routes);

    if let Some((state, x_matrix, own_keys, server_name)) = federation {
        let (federation_router, federation_manifest) =
            hs_federation::transport::router(state.clone(), x_matrix.clone());
        // The v2 spellings of `send_join`/`send_leave`/`invite` live under their own prefix. They
        // were previously registered inside the v1 router with a literal `/v2/` path segment,
        // which nothing noticed while they were seams and every remote would have noticed the
        // moment they were not. Both routers share one `FederationState` (it is `Clone` over
        // `Arc`s) so a join handled by either sees the same rooms and the same key cache.
        let (federation_router_v2, federation_manifest_v2) =
            hs_federation::transport::router_v2(state, x_matrix);
        // `/_matrix/key/v2/server` is deliberately *outside* that router: it is the one federation
        // endpoint that must answer an unsigned request, since it is what a remote server fetches
        // in order to be able to check signatures in the first place. Putting it behind the
        // `X-Matrix` layer would make key discovery require the keys it discovers.
        builder = builder
            .get(
                "/_matrix/key/v2/server",
                move || {
                    let own_keys = own_keys.clone();
                    let server_name = server_name.clone();
                    async move {
                        match crate::federation::server_key_response(&server_name, &own_keys) {
                            Ok(body) => axum::Json(body).into_response(),
                            Err(error) => {
                                tracing::error!(%error, "could not sign this server's key response");
                                hs_http::error::MatrixError::custom(
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    hs_http::error::MatrixErrorCode::Unknown,
                                    "could not sign the server key response",
                                )
                                .into_response()
                            }
                        }
                    }
                },
                RouteMeta::new(Surface::MatrixFederation, AuthKind::None)
                    .with_operation_id("getServerKey"),
            )
            .merge_router(
                "/_matrix/federation/v1",
                federation_router,
                federation_manifest.routes,
            )
            .merge_router(
                "/_matrix/federation/v2",
                federation_router_v2,
                federation_manifest_v2.routes,
            );
    }

    if legacy_media_enabled {
        // MSC2246's async upload splits across two prefixes: `create` reserves a content URI under
        // `/_matrix/media/v1`, while the later fill-in `PUT` uses `v3` like every other legacy
        // media route. Serving `create` only under `v3` is why it 404'd. Clone the state before
        // the legacy mount below moves it.
        let (media_v1_router, media_v1_manifest) = hs_media::router::v1_router::<B>();
        let media_v1_router = media_v1_router.with_state(mounts.media.clone());
        builder = builder.merge_router(
            "/_matrix/media/v1",
            media_v1_router,
            media_v1_manifest.routes,
        );

        let (legacy_router, legacy_manifest) = hs_media::router::legacy_router::<B>();
        let legacy_router = legacy_router.with_state(mounts.media);
        builder = builder.merge_router("/_matrix/media/v3", legacy_router, legacy_manifest.routes);
    }

    let (router, mut manifest) = builder.build();

    // `hs-admin`'s router already builds through its own `Builder` and already has
    // `.with_state(...)` applied internally (`hs_admin::router::build_router`'s own doc: it
    // "returns the manifest alongside so callers can write routes.json") — its paths
    // (`/api/v1/...`, `/admin/...`) are absolute, so it merges directly onto the top-level router
    // rather than through `merge_router`, which would (harmlessly, but confusingly) prepend an
    // empty prefix.
    let (admin_router, admin_manifest) = hs_admin::router::build_router(mounts.admin);
    manifest.routes.extend(admin_manifest.routes);
    // Cloned before the merge below consumes it: the Synapse compatibility shims forward into
    // this very router, so they answer from the same handlers and the same scope checks.
    let admin_router_for_shims = admin_router.clone();
    let router = router.merge(admin_router);

    // Same reasoning as the admin router directly above: an absolute path merges onto the
    // top-level router rather than nesting under a prefix.
    manifest.routes.extend(synapse_admin_routes);
    let router = router.merge(synapse_admin_router);

    // The read-only Synapse admin shims (`hs-compat`), which answer from the native `/api/v1`
    // router rather than reimplementing anything: an operator's existing Synapse tooling keeps
    // working against this server. They are built over a clone of the admin router assembled
    // above, so both surfaces enforce exactly the same scopes on the same data.
    let (shim_router, shim_routes) = crate::synapse_shims::router(admin_router_for_shims);
    manifest.routes.extend(shim_routes);
    let router = router.merge(shim_router);

    // Last, after every route and every merge: axum's `method_not_allowed_fallback` attaches to
    // the `MethodRouter`s registered before it, so anything merged afterwards would keep the
    // empty-bodied default. Turns an unknown endpoint into `404 M_UNRECOGNIZED` and a known path
    // called with the wrong method into `405 M_UNRECOGNIZED`, in whichever error shape the path's
    // API speaks -- see `hs_http::fallback`.
    let router = hs_http::fallback::apply(router);

    let router = router
        // Without this a browser client cannot talk to this server at all: it fails every
        // request after the preflight and shows only opaque network errors. Found by pointing
        // Element Web at it. The policy is the spec's own (wildcard origin, the five methods, the
        // three headers), and is deliberately not the admin API's same-origin default — see
        // `hs_http::cors::matrix_layer`.
        .layer(hs_http::cors::matrix_layer())
        .layer(Extension(well_known))
        .layer(Extension(ready))
        .layer(Extension(unstable_features))
        .layer(Extension(metrics.clone()))
        .layer(Extension(cluster.cluster.clone()))
        .layer(middleware::from_fn_with_state(metrics, track_metrics))
        .layer(hs_telemetry::RequestIdLayer::new());

    // Outermost layer: gates every `/rooms/{roomId}/...` request on shard ownership before it
    // can reach `hs-room`'s registry at all (see `crate::cluster`'s module docs). A no-op in
    // single-node mode (`is_mine` is always `true`), so this changes nothing about single-node
    // behavior beyond one cheap path scan per request.
    let router = crate::cluster::RoomShardGate::new(cluster).layer(router);

    (router, manifest)
}

/// Everything [`build_router`] needs beyond the always-present auth state, metrics and readiness
/// flag: one piece per crate it mounts. Bundled into a struct (rather than more bare parameters)
/// since both [`spawn_serve`] and [`route_manifest`] need to build one of these, and a
/// six-plus-argument generic function invites transposition bugs.
struct Mounts<B: KvBackend> {
    room: RoomState<B>,
    /// The federation transport server and the context its `X-Matrix` layer verifies against, or
    /// `None` when `federation.enabled` is off -- in which case nothing under
    /// `/_matrix/federation` or `/_matrix/key` is mounted at all, rather than mounted and
    /// refusing, so a server with federation disabled looks to a remote exactly like one that
    /// does not implement federation.
    federation: Option<(
        hs_federation::transport::FederationState,
        Arc<hs_federation::xmatrix::XMatrixContext>,
        Arc<hs_federation::keys::OwnSigningKeys>,
        String,
    )>,
    user: UserState<B, Arc<hs_room::registry::RoomRegistry<B>>>,
    e2e: hs_e2e::state::E2eState<B>,
    push: hs_push::state::PushState<B>,
    media: MediaState<B>,
    appservice_ping: Arc<PingService<B>>,
    admin: hs_admin::router::AdminState,
}

/// The default fan-out threshold for `hs-user`'s session hub: rooms with more joined members than
/// this stop getting a durable feed entry per member per event and are marked "hot" instead, so a
/// message to a very large room does not cost one store write per member. `hs_user::hub`'s module
/// docs explain the trade (a hot room's `/sync` reads the room's live position directly). No
/// config field exists for it yet; recorded in `docs/status/05-sync.md`.
const DEFAULT_FAN_OUT_THRESHOLD: usize = 500;

/// Builds the three states added in this pass -- `hs-user`'s session hub, `hs-e2e`'s key store and
/// `hs-push`'s rule/pusher/count stores -- over one already-open backend, so both [`spawn_serve`]
/// and [`throwaway_mounts`] assemble them the same way.
///
/// # Errors
/// Returns the store's own error if any keyspace could not be opened.
#[allow(clippy::type_complexity)]
fn build_session_mounts<B: KvBackend>(
    backend: &B,
    auth: &AuthState,
    rooms: &Arc<hs_room::registry::RoomRegistry<B>>,
) -> Result<
    (
        UserState<B, Arc<hs_room::registry::RoomRegistry<B>>>,
        hs_e2e::state::E2eState<B>,
        hs_push::state::PushState<B>,
    ),
    ServeError,
> {
    fn opening(e: impl std::error::Error + Send + Sync + 'static) -> ServeError {
        ServeError::Sessions(Box::new(e))
    }

    let user_store: hs_user::store::DynUserStore =
        Arc::new(hs_user::store::tables::TablesUserStore::open(backend.clone()).map_err(opening)?);
    // Built once and shared between `user` and `e2e` below (rather than opened twice over the
    // same backend): `hs-user`'s `GET /sync` needs the same e2e store `hs-e2e`'s own routes use,
    // per docs/rfcs/0013-e2ee-sync-extensions.md -- both sides must observe the same to-device
    // queue, device-list stream and key counts.
    let e2e_store: Arc<dyn hs_e2e::store::E2eStore> =
        Arc::new(hs_e2e::store::tables::TablesE2eStore::open(backend.clone()).map_err(opening)?);
    let user = UserState {
        auth: auth.clone(),
        hub: Arc::new(hs_user::hub::SessionHub::new(
            user_store,
            rooms.clone(),
            DEFAULT_FAN_OUT_THRESHOLD,
        )),
        e2e: e2e_store.clone(),
    };

    let e2e = hs_e2e::state::E2eState::new(auth.clone(), e2e_store);

    let push = hs_push::state::PushState {
        auth: auth.clone(),
        rulesets: Arc::new(hs_push::rulesets::CachedRulesetStore::new(
            hs_push::rulesets::tables::TablesRulesetStore::open(backend.clone())
                .map_err(opening)?,
        )),
        pushers: Arc::new(
            hs_push::pushers::tables::TablesPusherStore::open(backend.clone()).map_err(opening)?,
        ),
        counts: Arc::new(
            hs_push::counts::tables::TablesCountsStore::open(backend.clone()).map_err(opening)?,
        ),
        http_pushers: Arc::new(hs_push::pushers::http::HttpPusherClient::new(
            hs_push::pushers::http::RetryPolicy::default(),
        )),
    };

    // Three seams other crates built their half of and cannot reach across themselves, because
    // all three states are constructed here as siblings: push rules and notification counts into
    // `/sync` (`docs/status/10-push.md` defines the shapes), and a resolver so `/keys/changes`
    // can understand the opaque token `/sync` mints instead of rejecting it as malformed. Each
    // install is idempotent and absent-by-default, so a caller that never wires them gets the
    // previous behaviour rather than a panic.
    user.hub.install_push_rules_store(push.rulesets.clone());
    user.hub.install_counts_store(push.counts.clone());
    user.hub.install_device_list_token_resolver(&e2e);

    Ok((user, e2e, push))
}

/// The `/api/v1` state a real `hs serve` runs on: admin credentials are verified against this
/// server's own user store, and the user-directory operations read from it.
///
/// `hs_auth::admin_verifier::AdminTokenVerifier` accepts an ordinary client-server access token
/// whose user carries `is_admin` (there is no separate admin credential type), and
/// `hs_auth::admin_directory::AuthStoreUserDirectory` serves `/api/v1/users` from the same open
/// store — both constructed `from_auth_state` so the admin surface and the client-server surface
/// read one backend handle rather than two.
///
/// The audit sink is durable ([`crate::audit::TablesAuditSink`], over the same backend as every
/// other store), so who locked an account or promoted an admin survives a restart. The event bus
/// stays in-process by design: it is a live stream for `GET /api/v1/events` subscribers, not a
/// record — the record is the audit log.
fn admin_state<B: KvBackend + 'static>(
    auth: &AuthState,
    audit: Arc<crate::audit::TablesAuditSink<B>>,
    rooms: &Arc<hs_room::registry::RoomRegistry<B>>,
    server_name: &str,
    enabled_components: Vec<String>,
    config_source: Option<Arc<dyn hs_admin::sources::ConfigSource>>,
    setup: Arc<hs_auth::setup::FirstRunSetup>,
) -> hs_admin::router::AdminState {
    let state = hs_admin::router::AdminState::new(
        Arc::new(hs_auth::admin_verifier::AdminTokenVerifier::from_auth_state(auth)),
        audit,
        Arc::new(hs_admin::events::EventBus::new()),
    )
    .with_users(Arc::new(
        hs_auth::admin_directory::AuthStoreUserDirectory::from_auth_state(auth),
    ))
    // Until this, every `/api/v1/rooms*` operation answered an honest 503 saying no room source
    // was wired. It is wired now, and blocking a room through the admin API stops its very next
    // message.
    .with_rooms(Arc::new(hs_room::admin::RoomRegistryDirectory::new(
        rooms.clone(),
    )))
    // What lets the management interface create this server's first administrator, instead of
    // that taking a shared secret, `hs register --admin`, a `curl` and a pasted token.
    .with_setup(setup)
    .with_server_info(hs_admin::model::ServerInfo {
        name: server_name.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        // No build metadata is stamped into the binary yet (no `vergen`/`build.rs`), so this says
        // so rather than inventing a commit or a date.
        build: "unstamped".to_owned(),
        supported_room_versions: hs_model::room_version::known_room_version_ids()
            .map(str::to_owned)
            .collect(),
        enabled_components,
        contract_version: hs_admin::model::ServerInfo::default().contract_version,
    });
    // Without this the whole `/config*` surface answers 503: the operations are real, but they
    // have nothing to read or write. This is what makes the management interface able to change
    // the server's configuration rather than only display it.
    match config_source {
        Some(source) => state.with_config(source),
        None => state,
    }
}

/// The `/api/v1` state for [`route_manifest`]'s throwaway router: routes are registered the same
/// way regardless of who can authenticate against them, and this one is never served. An empty
/// `StaticVerifier` means every request would answer `401`, which is why it must not be used by
/// [`spawn_serve`] — see [`admin_state`] for the real one.
fn manifest_only_admin_state() -> hs_admin::router::AdminState {
    hs_admin::router::AdminState::new(
        Arc::new(hs_admin::auth::StaticVerifier::new()),
        Arc::new(hs_admin::audit::InMemoryAuditSink::new()),
        Arc::new(hs_admin::events::EventBus::new()),
    )
}

/// A throwaway [`Mounts`] over an in-memory backend, for [`route_manifest`]: routes are static,
/// independent of runtime configuration, so this exists purely to read off the manifest
/// [`Builder::build`] records before the router itself is dropped.
fn throwaway_mounts() -> Mounts<hs_kv::memory::MemoryBackend> {
    use hs_kv::memory::MemoryBackend;

    let identity = hs_room::identity::HomeserverIdentity::for_tests("routes-manifest.invalid");
    let auth = AuthState::in_memory();
    let backend = MemoryBackend::new();
    let rooms = Arc::new(
        hs_room::registry::RoomRegistry::open(backend.clone(), identity.clone())
            .expect("opening an in-memory room registry cannot fail"),
    );
    let room = RoomState {
        auth: auth.clone(),
        rooms: rooms.clone(),
        identity,
    };
    let (user, e2e, push) = build_session_mounts(&backend, &auth, &rooms)
        .expect("opening in-memory session/e2e/push keyspaces cannot fail");

    let (federation_state, x_matrix) = crate::federation::manifest_only_mount();
    let federation = Some((
        federation_state,
        x_matrix,
        Arc::new(hs_federation::keys::OwnSigningKeys::from_keys(vec![
            hs_model::signing::SigningKeyPair::generate("a_manifest"),
        ])),
        "routes-manifest.invalid".to_string(),
    ));

    let object_store: Arc<dyn object_store::ObjectStore> =
        Arc::new(object_store::memory::InMemory::new());
    let metadata = hs_media::metadata::MetadataStore::open(MemoryBackend::new())
        .expect("opening in-memory media metadata cannot fail");
    let repository = hs_media::repository::MediaRepository::new(
        object_store,
        metadata,
        Arc::new(hs_config::MediaConfig::default()),
        Arc::new(hs_media::policy::InMemoryQuotaPolicy::unlimited()),
        hs_media::thumbnail::ThumbnailPolicy::default(),
        "routes-manifest.invalid".to_owned(),
        || 0,
    );
    let media = MediaState {
        auth: AuthState::in_memory(),
        repository: Arc::new(repository),
        legacy_media_enabled: true,
        legacy_freeze_ms: None,
    };

    let registry = Arc::new(
        hs_appservice::registry::Registry::open(
            MemoryBackend::new(),
            ruma::server_name!("routes-manifest.invalid"),
        )
        .expect("opening an in-memory appservice registry cannot fail"),
    );
    let appservice_ping = Arc::new(hs_appservice::ping::PingService::new(
        registry,
        Arc::new(hs_appservice::ping::HttpPingTransport::new()),
    ));

    Mounts {
        room,
        federation,
        user,
        e2e,
        push,
        media,
        appservice_ping,
        admin: manifest_only_admin_state(),
    }
}

/// The `routes.json` manifest [`build_router`] would produce, without needing a real
/// [`AuthState`], config or storage backend — routes are static, independent of runtime
/// configuration, so this builds one with throwaway in-memory state purely to read off the
/// manifest [`Builder::build`] records, then drops the router. Used by `hs serve --routes-manifest`
/// and the standalone `hs routes-manifest` subcommand.
#[must_use]
pub fn route_manifest() -> RouteManifest {
    let cluster = crate::cluster::ClusterHandles::single_node(hs_cluster::ShardLayout::default());
    let (_router, manifest) = build_router(
        AuthState::in_memory(),
        throwaway_mounts(),
        Arc::new(Metrics::new()),
        Arc::new(AtomicBool::new(true)),
        Arc::new(BTreeMap::new()),
        // Routes are registered unconditionally; whether a `.well-known` document is *served* or
        // 404s is a runtime decision inside the handler, so the manifest is the same either way.
        crate::well_known::WellKnown::default(),
        &cluster,
    );
    manifest
}

async fn health_live() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Ready only once both this process's own startup flag is set *and* `hs-cluster` reports this
/// replica ready (RFC 0001 section 7): in single-node mode the latter is always `Ready`, so this
/// is unchanged from before this module existed; in clustered mode a replica that has not yet
/// heartbeated successfully (still joining, or its own heartbeats are failing) now correctly
/// answers "not ready" instead of accepting traffic for shards it may not actually hold yet.
async fn health_ready(
    Extension(ready): Extension<Arc<AtomicBool>>,
    Extension(cluster): Extension<hs_cluster::Cluster>,
) -> impl IntoResponse {
    if !ready.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, "not ready".to_owned()).into_response();
    }
    match cluster.ready() {
        hs_cluster::Readiness::Ready => (StatusCode::OK, "ready".to_owned()).into_response(),
        hs_cluster::Readiness::NotReady(reason) => {
            (StatusCode::SERVICE_UNAVAILABLE, reason).into_response()
        }
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
    /// The link that creates this server's first administrator, while it has none (see
    /// [`hs_auth::setup`]). `None` once an administrator exists. `hs serve` writes it to the log;
    /// it is on the handle so that is one decision made in one place, and so a test can follow
    /// the same link an operator would.
    pub setup_link: Option<String>,
    shutdown_tx: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
    /// Keeps the opened storage backend alive for as long as the server is: Fjall holds an
    /// exclusive lock on its data directory and the Postgres backend owns a connection pool, and
    /// dropping either while listeners are still serving would take the store out from under
    /// them. Type-erased because [`ServeHandle`] is not generic over the backend.
    _storage: Box<dyn std::any::Any + Send + Sync>,
    /// This replica's `hs-cluster` handle, drained on [`ServeHandle::shutdown`] before the HTTP
    /// listeners stop accepting (RFC 0001 section 10).
    cluster: hs_cluster::Cluster,
    /// The mesh listener, if this replica is clustered.
    mesh: Option<crate::cluster::MeshRuntime>,
}

/// How long [`ServeHandle::shutdown`] gives `Cluster::drain` to release this replica's shards and
/// see them claimed by a peer before giving up and shutting down anyway (RFC 0001 section 10). No
/// `hs-config` field exists for this yet (see `docs/status/03-cluster.md`); chosen to comfortably
/// fit inside a typical Kubernetes `terminationGracePeriodSeconds` (30s) with margin for the
/// listeners' own drain afterwards. A no-op in single-node mode regardless (`SingleNode::drain`
/// returns immediately, per its own doc comment).
const CLUSTER_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

impl ServeHandle {
    /// Runs the cluster's graceful handoff (releasing every shard this replica owns and waiting,
    /// up to [`CLUSTER_DRAIN_DEADLINE`], for a peer to claim it), stops the mesh listener, then
    /// signals every HTTP listener to begin graceful shutdown and waits for them to finish
    /// draining in-flight requests. `hs serve`'s `SIGTERM` handler calls this.
    pub async fn shutdown(self) {
        let report = self.cluster.drain(CLUSTER_DRAIN_DEADLINE).await;
        if report.handed_off > 0 || report.released_unclaimed > 0 {
            tracing::info!(
                handed_off = report.handed_off,
                released_unclaimed = report.released_unclaimed,
                elapsed = ?report.elapsed,
                "cluster drain complete"
            );
        }
        if let Some(mesh) = self.mesh {
            mesh.shutdown().await;
        }
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
    let storage = storage::open_storage(&config.storage)?;
    spawn_serve_with_storage(storage, config, options).await
}

/// [`spawn_serve`] over a backend somebody else already opened.
///
/// `hs serve` takes this path, because by the time it has a configuration to serve it has already
/// had to open the database: that is where the configuration lives (`crate::bootstrap`). Opening
/// it a second time here would not merely be wasteful — the embedded backend holds an exclusive
/// lock on its directory, so the second open fails and the server never starts.
///
/// # Errors
/// See [`ServeError`].
pub async fn spawn_serve_with_storage(
    storage: storage::OpenedStorage,
    config: hs_config::Config,
    options: ServeOptions,
) -> Result<ServeHandle, ServeError> {
    if config.listeners.listeners.is_empty() {
        return Err(ServeError::NoListeners);
    }

    // One generic server, instantiated per backend. `spawn_serve_with_backend` is generic over
    // `B: KvBackend`, so both arms below get the same server built over a different store rather
    // than two code paths that could drift apart; the cost is that it is monomorphized twice.
    match storage {
        storage::OpenedStorage::Embedded(backend) => {
            spawn_serve_with_backend(backend, config, options).await
        }
        storage::OpenedStorage::Postgres(backend) => {
            spawn_serve_with_backend(backend, config, options).await
        }
    }
}

/// [`spawn_serve`]'s body, over whichever `hs-kv` backend the configuration opened.
///
/// # Errors
/// See [`ServeError`].
async fn spawn_serve_with_backend<B: KvBackend + 'static>(
    backend: B,
    config: hs_config::Config,
    options: ServeOptions,
) -> Result<ServeHandle, ServeError> {
    // Wired by the integration lead per docs/status/07-auth-and-identity.md "For track 12":
    // the persistent store replaces the in-memory one, so users, devices and tokens survive a
    // restart. `backend.clone()` is a cheap Arc-backed handle sharing the same open database —
    // every subsystem below (auth, rooms, media, appservices) gets its own clone of the same
    // opened backend rather than a separate store, so they all see the same durable data.
    let auth_config = hs_auth::config::AuthConfig::try_from(&config)?;
    let auth_store: Arc<dyn hs_auth::store::AuthStore> = Arc::new(
        hs_auth::store::tables::TablesAuthStore::open(backend.clone())?,
    );
    let mut auth_state = AuthState::with_store(auth_store, auth_config);

    let identity = crate::identity::load_or_generate(&config)?;
    let server_name = identity.server_name.clone();

    let metrics = Arc::new(Metrics::new());

    let appservices = crate::appservices::load(&config.appservices, backend.clone(), &server_name)?;
    // Replaces `hs-auth`'s stub `InMemoryAppserviceRegistry` (empty by default) with
    // `hs-appservice`'s real, store-backed registry, so an `as_token` a loaded registration
    // declares actually authenticates through `Requester` — see
    // `hs_appservice::auth_registry::RegistryAppserviceAdapter`'s own doc comment. Set before
    // `room_state`/`media_state` are built below, since both embed a clone of `auth_state`.
    auth_state.appservices = Arc::new(
        hs_appservice::auth_registry::RegistryAppserviceAdapter::new(appservices.registry.clone()),
    );

    let rooms = Arc::new(hs_room::registry::RoomRegistry::open(
        backend.clone(),
        identity.clone(),
    )?);
    let room_state = RoomState {
        auth: auth_state.clone(),
        rooms: rooms.clone(),
        identity,
    };

    let (user_state, e2e_state, push_state) = build_session_mounts(&backend, &auth_state, &rooms)?;

    // Closes the discovery gap (`docs/rfcs/0012-room-registry-global-updates.md`): every room the
    // registry creates or loads is followed into users' durable feeds, so a room created through
    // `/createRoom` shows up in `/sync` and an invite reaches a target who has never synced.
    // Subscribed here, before any listener is bound below, because the stream does not replay: an
    // update published before this subscription exists would be missed.
    user_state.hub.watch_all(rooms.subscribe_global());

    let media_state = crate::media::build_media_state(
        &config,
        backend.clone(),
        auth_state.clone(),
        options.media_scanning_config.as_deref(),
        &metrics,
    )?;

    // A content scan in `defer` or `quarantine` mode records its intent durably before the scan
    // task starts, so a crash cannot lose the verdict — but nothing resolves those rows on its
    // own. Sweep them once at startup. Spawned rather than awaited: a slow or unreachable scanner
    // must not hold up binding a listener, and an unresolved item stays unservable meanwhile,
    // which is the safe direction to fail.
    {
        let repository = media_state.repository.clone();
        tokio::spawn(async move {
            match repository.resume_pending_scans().await {
                Ok(0) => {}
                Ok(count) => {
                    tracing::info!(
                        count,
                        "resumed content scans left pending by a previous run"
                    );
                }
                Err(error) => {
                    tracing::error!(%error, "could not resume pending content scans");
                }
            }
        });
    }

    // Federation is mounted over the same open backend and stores every other surface uses, so a
    // remote server reading `/state` sees exactly what a local client reading `/messages` sees.
    let federation = if config.federation.enabled {
        let mount = crate::federation::build_mount(
            &config,
            &room_state.identity,
            backend.clone(),
            rooms.clone(),
            user_state.hub.store().clone(),
            auth_state.store.clone(),
            e2e_state.store.clone(),
        )?;
        Some((
            mount.state,
            mount.x_matrix,
            mount.own_keys,
            mount.server_name,
        ))
    } else {
        tracing::info!("federation is disabled; not mounting the federation transport server");
        None
    };

    // What `/api/v1/server` reports as enabled: derived from what this process actually mounted
    // just above, not from a static list — a component that is off must not appear.
    let mut enabled_components = vec![
        "client".to_owned(),
        "media".to_owned(),
        "sync".to_owned(),
        "e2ee".to_owned(),
        "push".to_owned(),
        "admin-api".to_owned(),
    ];
    if federation.is_some() {
        enabled_components.push("federation".to_owned());
    }
    // `list()` reads the registry's own store; a failure there is not worth failing startup for,
    // so an unreadable registry reports as no appservices rather than as a mounted component.
    if appservices
        .registry
        .list()
        .map(|rows| !rows.is_empty())
        .unwrap_or(false)
    {
        enabled_components.push("appservices".to_owned());
    }
    enabled_components.sort();

    let setup = Arc::new(hs_auth::setup::FirstRunSetup::from_auth_state(&auth_state));

    let mounts = Mounts {
        room: room_state,
        federation,
        user: user_state,
        e2e: e2e_state,
        push: push_state,
        media: media_state,
        appservice_ping: appservices.ping_service,
        admin: admin_state(
            &auth_state,
            Arc::new(
                crate::audit::TablesAuditSink::open(backend.clone())
                    .map_err(|e| ServeError::Sessions(Box::new(e)))?,
            ),
            &rooms,
            server_name.as_str(),
            enabled_components,
            options.config_source.clone(),
            setup.clone(),
        ),
    };

    // Built before the router below so the room-shard gate layer can wrap it: see
    // `crate::cluster`'s module docs for why gating every `/rooms/{roomId}/...` request on shard
    // ownership (not just writes) is what actually stops two replicas from forking a room's event
    // DAG (`docs/status/03-cluster.md`'s two-replica experiment). Inert in single-node mode
    // (`config.cluster.single_node`, the default) — this matches today's behavior exactly.
    let cluster_handles = crate::cluster::start(&config, backend.clone()).await?;

    // The routing gate above stops two replicas both building a room actor, which is what closed
    // the silent split-brain. This is the belt-and-braces underneath it: the fence is read inside
    // the same transaction the write commits in, so a handoff that races the gate's ownership
    // check still cannot land a write. Inert in single-node mode, which is why it is installed
    // unconditionally.
    rooms.install_fencing(Arc::new(hs_room::fencing::RoomFencing {
        ownership: cluster_handles.cluster.ownership().clone(),
        layout: cluster_handles.layout,
        cluster_store: hs_cluster::store::ClusterStore::open(backend.clone())
            .map_err(|e| ServeError::Sessions(Box::new(e)))?,
    }));

    let ready = Arc::new(AtomicBool::new(true));
    let unstable_features = Arc::new(versions::load_unstable_features(
        options.capabilities_config.as_deref(),
    )?);

    let well_known = crate::well_known::WellKnown::from_config(&config);
    if well_known.is_empty() {
        tracing::info!(
            "no .well-known documents are published (set server.well_known_server to delegate \
             federation, server.public_baseurl to advertise a client base URL)"
        );
    } else {
        tracing::info!(
            server = ?well_known.server,
            client_base_url = ?well_known.client_base_url,
            "publishing .well-known discovery documents"
        );
    }

    let (app, manifest) = build_router(
        auth_state,
        mounts,
        metrics,
        ready,
        unstable_features,
        well_known,
        &cluster_handles,
    );

    // The mesh listener replays a forwarded request against this exact router (see
    // `crate::cluster::ClusterHandles::spawn_mesh`'s doc comment): a `None` in single-node mode,
    // where nothing should ever dial in.
    let mesh = cluster_handles.spawn_mesh(app.clone());
    let cluster = cluster_handles.cluster.clone();

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

    // Asked last, once the listeners are bound, because the link needs a port that is real. A
    // server that cannot work out whether to offer setup still serves: everything else about it
    // is fine, and `hs register --admin` remains a way in.
    let setup_link = match setup.offer().await {
        Ok(token) => {
            token.map(|token| setup_link(config.server.public_baseurl.as_deref(), &addrs, &token))
        }
        Err(e) => {
            tracing::error!(error = %e, "could not determine whether this server needs its first administrator; no setup link will be offered");
            None
        }
    };

    Ok(ServeHandle {
        addrs,
        setup_link,
        shutdown_tx,
        join,
        _storage: Box::new(backend.clone()),
        cluster,
        mesh,
    })
}

/// The first-run setup link for `token`.
///
/// Rooted at `server.public_baseurl` when the operator has said what this server is called from
/// outside, and otherwise at `localhost` on the first bound port -- which is right for a first
/// run on a laptop or behind `docker run -p`, the cases where nothing has been configured yet.
///
/// The token is in the *fragment*. Browsers do not send a fragment to the server, so the token
/// cannot land in an access log, a reverse proxy's log or a `Referer` header on its way to the
/// page that reads it.
fn setup_link(public_baseurl: Option<&str>, addrs: &[SocketAddr], token: &str) -> String {
    let base = match public_baseurl.map(str::trim).filter(|s| !s.is_empty()) {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => format!(
            "http://localhost:{}",
            addrs.first().map_or(8008, SocketAddr::port)
        ),
    };
    format!("{base}/admin/setup#token={token}")
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
                media_scanning_config: None,
                config_source: None,
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
                media_scanning_config: None,
                config_source: None,
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
