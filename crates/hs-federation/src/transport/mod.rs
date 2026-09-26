//! The federation transport server: an axum router fragment mounted the way `hs-media`'s
//! `router.rs` mounts its own (a `Builder`-based function returning `(Router, RouteManifest)`),
//! per `docs/status/06-federation.md` item 9.
//!
//! The `X-Matrix` verification middleware (`crate::xmatrix::verify_x_matrix`) is applied exactly
//! once, in [`router`], over the whole merged router built from [`read_router`] and
//! [`seam_router`] — never per-handler. This is the structural guarantee named in this crate's
//! decisions: a route added to either sub-router in the future is automatically covered, because
//! there is no code path into a handler that does not first pass through the layer.  See
//! [`tests::every_route_is_behind_the_x_matrix_layer`] for the test that enforces this.
//!
//! Read/query endpoints (`docs/design/06-federation-threat-model.md` section 2.4) are fully
//! implemented against [`FederationState`]'s [`RoomDataSource`] and [`FederationQuerySource`]
//! seams. The join/leave/knock/invite handshakes and `/send` are seams per section 2.5: they
//! verify the signature (via the shared layer), bound-check the body, and reject with a typed
//! "not implemented" error — nothing else.

mod join;
mod queries;
mod read_routes;
mod seams;
mod send;

use std::sync::Arc;

use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};

use crate::inbound::{RoomWriteSink, TransactionStore};
use crate::room_source::RoomDataSource;
use crate::xmatrix::{self, XMatrixContext};

pub use queries::{FederationQuerySource, InMemoryQuerySource};

/// State shared by every federation transport handler.
#[derive(Clone)]
pub struct FederationState {
    pub own_server_name: Arc<str>,
    pub rooms: Arc<dyn RoomDataSource>,
    pub queries: Arc<dyn FederationQuerySource>,
    pub allow_public_rooms_over_federation: bool,
    pub allow_device_name_lookup_over_federation: bool,
    /// Applies an already-verified inbound event (`/send`'s PDUs, and a validated `send_join`
    /// submission) to a room this server hosts. See `crate::inbound`'s module doc for what this
    /// can and cannot promise today.
    pub write_sink: Arc<dyn RoomWriteSink>,
    /// Idempotency cache for `/send` transactions, keyed by `(origin, txnId)`.
    pub transactions: Arc<dyn TransactionStore>,
    /// Fetches ancestor events from a remote server when an inbound event cites a
    /// `prev_events`/`auth_events` entry this server does not hold
    /// (`hs_room::RoomError::MissingAncestors`). `None` disables backfill entirely: `/send` still
    /// accepts events whose ancestors are already held, but a genuine gap is reported as a
    /// per-event error immediately instead of triggering any outbound calls. See `crate::backfill`.
    pub ancestor_fetcher: Option<Arc<dyn crate::backfill::AncestorFetcher>>,
    /// Bounds applied to every backfill resolution attempt. See `crate::backfill` for what an
    /// attacker can and cannot cost this server by dangling a missing-ancestor chain.
    pub backfill_limits: crate::backfill::BackfillLimits,
    /// Where an event this server accepts on behalf of a room it hosts is handed for delivery
    /// to the room's other servers -- today, a join accepted by `send_join`, which the spec
    /// requires the resident server to forward to every other server in the room. `None` means
    /// nothing is forwarded (the manifest-only mount, and every handler test that does not care).
    /// See `crate::sender`.
    pub sender: Option<Arc<dyn crate::sender::OutboundPduSink>>,
}

fn matrix_federation(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixFederation, AuthKind::Matrix).with_operation_id(operation_id)
}

/// Builds the full **v1** federation router: every read/query endpoint, every seam, `/send`,
/// `make_join` and the v1 `send_join` spelling, with the `X-Matrix` verification layer wrapping
/// the whole thing. Paths are spec-relative (registered as `/version`, `/send/{txnId}`, etc. —
/// mounting under `/_matrix/federation/v1` and composing with other listeners is the caller's job,
/// matching `hs-media`'s convention).
///
/// The v2-only spellings (`send_join`, `send_leave`, `invite`) live in [`router_v2`], mounted
/// separately at `/_matrix/federation/v2` — see that function's doc for why they are not just more
/// routes registered here.
///
/// # Panics
/// Never during normal construction; this function only builds route tables and applies layers.
pub fn router(
    state: FederationState,
    x_matrix_ctx: Arc<XMatrixContext>,
) -> (axum::Router, RouteManifest) {
    let builder = Builder::<FederationState>::new();
    let builder = read_routes::add_routes(builder);
    let builder = seams::add_routes(builder);
    let builder = send::add_routes(builder);
    let builder = join::add_routes(builder);
    let (merged, manifest) = builder.build();

    (apply_x_matrix_layer(merged, state, x_matrix_ctx), manifest)
}

/// Builds the **v2** federation router: `send_join`, plus the still-seam `send_leave` and
/// `invite` v2 spellings, under the same `X-Matrix` layer as [`router`].
///
/// This exists as a second function (rather than one router covering both prefixes) because the
/// v1 and v2 paths for `send_join`/`send_leave`/`invite` share the exact same route string
/// (`/send_join/{roomId}/{eventId}`, etc.) — only the mount prefix distinguishes them
/// (`/_matrix/federation/v1` vs `/_matrix/federation/v2`). A single `Builder` cannot register the
/// same `(method, path)` twice with different handlers, so the two versions need two routers,
/// composed by the caller at two different mount points. Before this function existed, the v2
/// spellings were registered *inside* [`router`] under a literal `/v2/` path segment
/// (`/_matrix/federation/v1/send_join/v2/{roomId}/{eventId}`) — wrong, and invisible for as long as
/// every handler behind it was a `501` seam. See this crate's status file for the wiring the
/// caller (`hs-cli`) needs to add: mounting this at `/_matrix/federation/v2`.
///
/// # Panics
/// Never during normal construction; this function only builds route tables and applies layers.
pub fn router_v2(
    state: FederationState,
    x_matrix_ctx: Arc<XMatrixContext>,
) -> (axum::Router, RouteManifest) {
    let builder = Builder::<FederationState>::new();
    let builder = join::add_routes_v2(builder);
    let builder = seams::add_routes_v2(builder);
    let (merged, manifest) = builder.build();

    (apply_x_matrix_layer(merged, state, x_matrix_ctx), manifest)
}

fn apply_x_matrix_layer(
    merged: axum::Router<FederationState>,
    state: FederationState,
    x_matrix_ctx: Arc<XMatrixContext>,
) -> axum::Router {
    merged
        .with_state(state)
        // Layer order matters: `Router::layer` wraps outside-in, so the layer added *last* runs
        // *first*. `Extension` must run before `verify_x_matrix`'s own `Extension` extractor, so
        // it is added last. See `crate::xmatrix::verify_x_matrix`'s doc for the same note.
        .layer(axum::middleware::from_fn(xmatrix::verify_x_matrix))
        .layer(axum::Extension(x_matrix_ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{
        DynRemoteKeyCache, KeyServerFetcher, OwnSigningKeys, RemoteKeyCache,
        build_server_key_response,
    };
    use crate::room_source::InMemoryRoomSource;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    struct EmptyFetcher;
    #[async_trait]
    impl KeyServerFetcher for EmptyFetcher {
        async fn fetch_server_key(&self, _server_name: &str) -> Option<serde_json::Value> {
            None
        }
    }

    fn test_state() -> FederationState {
        FederationState {
            own_server_name: Arc::from("us.example.org"),
            rooms: Arc::new(InMemoryRoomSource::new()),
            queries: Arc::new(InMemoryQuerySource::default()),
            allow_public_rooms_over_federation: true,
            allow_device_name_lookup_over_federation: true,
            write_sink: Arc::new(crate::inbound::StaticWriteSink::new(
                Vec::new(),
                "not supported",
            )),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
        }
    }

    fn test_ctx() -> Arc<XMatrixContext> {
        let key_cache: Arc<DynRemoteKeyCache> = Arc::new(RemoteKeyCache::new(
            Box::new(EmptyFetcher) as Box<dyn KeyServerFetcher>,
        ));
        Arc::new(XMatrixContext {
            own_server_name: "us.example.org".to_string(),
            key_cache,
        })
    }

    /// Replaces every `{param}` path segment with a harmless placeholder, so every registered
    /// route can be requested with a syntactically valid (if semantically meaningless) path.
    fn concretize(path: &str) -> String {
        path.split('/')
            .map(|segment| {
                if segment.starts_with('{') && segment.ends_with('}') {
                    "placeholder"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The load-bearing test named in this crate's decisions: every route this router ever
    /// registers must be rejected when called with no `Authorization` header at all. If a future
    /// route is added to `read_routes` or `seams` without going through `router()`'s merge, or if
    /// the layer is ever restructured to a per-handler opt-in, this test starts failing the
    /// moment the new route ships — it does not need updating when routes are added.
    #[tokio::test]
    async fn every_route_is_behind_the_x_matrix_layer() {
        let (router, manifest) = router(test_state(), test_ctx());
        assert!(
            !manifest.routes.is_empty(),
            "sanity check: the router must register at least one route for this test to mean anything"
        );

        for route in &manifest.routes {
            let path = concretize(&route.path);
            let request = Request::builder()
                .method(route.method.as_str())
                .uri(&path)
                .body(Body::empty())
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "route {} {} was not rejected without an Authorization header (got {})",
                route.method,
                route.path,
                response.status()
            );
        }
    }

    #[tokio::test]
    async fn version_endpoint_works_when_properly_signed() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();

        struct FixedFetcher(serde_json::Value);
        #[async_trait]
        impl KeyServerFetcher for FixedFetcher {
            async fn fetch_server_key(&self, _server_name: &str) -> Option<serde_json::Value> {
                Some(self.0.clone())
            }
        }
        let doc = build_server_key_response("origin.example.org", &keys, &[], 3600).unwrap();
        let key_cache: Arc<DynRemoteKeyCache> = Arc::new(RemoteKeyCache::new(
            Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>,
        ));
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "us.example.org".to_string(),
            key_cache,
        });

        let (router, _manifest) = router(test_state(), ctx);

        let header = xmatrix::sign_request(
            "GET",
            "/version",
            "origin.example.org",
            "us.example.org",
            None,
            keys.primary(),
        )
        .unwrap();

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/version")
                    .header(axum::http::header::AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
