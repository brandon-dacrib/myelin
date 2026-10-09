//! The federation transport server: an axum router fragment mounted the way `hs-media`'s
//! `router.rs` mounts its own (a `Builder`-based function returning `(Router, RouteManifest)`),
//! per `docs/status/06-federation.md` item 9.
//!
//! The `X-Matrix` verification middleware (`crate::xmatrix::verify_x_matrix`) is applied exactly
//! once, in [`router`], over the whole merged router built from [`read_router`] and
//! [`seam_router`] — never per-handler. This is the structural guarantee named in this crate's
//! decisions: a route added to either sub-router in the future is automatically covered, because
//! there is no code path into a handler that does not first pass through the layer.  See
//! [`tests::every_route_is_behind_the_x_matrix_layer`] for the test that enforces this. The one
//! exception is `/version`, which the spec leaves unsigned: it is merged beside the layer from
//! its own router (`version`), marked [`AuthKind::None`] in the manifest, and
//! [`tests::the_only_unsigned_federation_route_is_version`] keeps it the only one.
//!
//! Read/query endpoints (`docs/design/06-federation-threat-model.md` section 2.4) are fully
//! implemented against [`FederationState`]'s [`RoomDataSource`] and [`FederationQuerySource`]
//! seams. The join/leave/knock/invite handshakes and `/send` are seams per section 2.5: they
//! verify the signature (via the shared layer), bound-check the body, and reject with a typed
//! "not implemented" error — nothing else. `/send`, the join, leave and knock handshakes and
//! `/invite` are real now (`send`, `join`, `membership`).

mod join;
pub mod key_server;
mod keys;
mod membership;
pub mod openid;
mod queries;
mod read_routes;
mod seams;
mod send;
mod version;

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
    /// What the routes allow and how fast an origin may send: settings a running server
    /// replaces when they change (`federation.allow_public_rooms_over_federation`,
    /// `federation.allow_device_name_lookup_over_federation`, `rate_limits.federation`).
    pub policy: InboundPolicy,
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
    /// Where `PUT /invite` puts an invite for one of this server's users, and the key it
    /// co-signs it with (`crate::invite`). `None` answers `501`: the manifest-only mount, and
    /// every handler test that does not care.
    pub invites: Option<crate::invite::InviteHandling>,
    /// Where `/send`'s EDUs go once validated (`crate::edu::InboundEduSink`). `None` drops them,
    /// which is what every handler test that does not care about EDUs wants.
    pub edu_sink: Option<Arc<dyn crate::edu::InboundEduSink>>,
}

/// The inbound settings [`FederationState`] reads on every request, shared by every clone so a
/// running server can change them ([`InboundPolicy::set_allow_public_rooms`] and friends).
#[derive(Clone, Debug)]
pub struct InboundPolicy {
    allow_public_rooms: Arc<std::sync::atomic::AtomicBool>,
    allow_device_names: Arc<std::sync::atomic::AtomicBool>,
    /// `rate_limits.federation`: inbound transactions (`PUT /send`), per origin server.
    /// Limits nothing until set.
    pub transactions: Arc<hs_http::buckets::TokenBuckets>,
}

impl InboundPolicy {
    /// A policy allowing what the two flags say, with no transaction limit.
    #[must_use]
    pub fn new(allow_public_rooms: bool, allow_device_names: bool) -> Self {
        Self {
            allow_public_rooms: Arc::new(allow_public_rooms.into()),
            allow_device_names: Arc::new(allow_device_names.into()),
            transactions: Arc::new(hs_http::buckets::TokenBuckets::new("federation")),
        }
    }

    /// Whether `GET /publicRooms` answers other servers
    /// (`federation.allow_public_rooms_over_federation`).
    #[must_use]
    pub fn allow_public_rooms(&self) -> bool {
        self.allow_public_rooms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether device display names are given to other servers
    /// (`federation.allow_device_name_lookup_over_federation`).
    #[must_use]
    pub fn allow_device_names(&self) -> bool {
        self.allow_device_names
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Replaces [`Self::allow_public_rooms`] for every clone.
    pub fn set_allow_public_rooms(&self, allow: bool) {
        self.allow_public_rooms
            .store(allow, std::sync::atomic::Ordering::Relaxed);
    }

    /// Replaces [`Self::allow_device_names`] for every clone.
    pub fn set_allow_device_names(&self, allow: bool) {
        self.allow_device_names
            .store(allow, std::sync::atomic::Ordering::Relaxed);
    }
}

fn matrix_federation(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixFederation, AuthKind::Matrix).with_operation_id(operation_id)
}

/// Builds the full **v1** federation router: every read/query endpoint, every seam, `/send`,
/// `make_join` and the v1 `send_join` spelling, with the `X-Matrix` verification layer wrapping
/// all of them, plus the unsigned `/version` beside that layer. Paths are spec-relative (registered as `/version`, `/send/{txnId}`, etc. —
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
    let builder = membership::add_routes(builder);
    let builder = keys::add_routes(builder);
    let (merged, mut manifest) = builder.build();
    // `/version` is unsigned (the spec gives it no `security`), so it is merged *beside* the
    // `X-Matrix` layer, not under it: `Router::layer` wraps only the routes present when it is
    // applied. `/openid/userinfo`, the other unsigned route under the prefix, is not here at all:
    // it is served whether or not federation is enabled ([`openid::router`]).
    let (version_router, version_manifest) = version::router();
    manifest.routes.extend(version_manifest.routes);
    (
        apply_x_matrix_layer(merged, state, x_matrix_ctx).merge(version_router),
        manifest,
    )
}

/// Builds the **v2** federation router: `send_join`, `send_leave` and `invite`, under the same
/// `X-Matrix` layer as [`router`].
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
    let builder = membership::add_routes_v2(builder);
    let (merged, manifest) = builder.build();

    (apply_x_matrix_layer(merged, state, x_matrix_ctx), manifest)
}

/// Puts a federation router built outside this crate behind the same `X-Matrix` verification
/// layer [`router`] applies to its own: today, `hs-media`'s
/// `/_matrix/federation/v1/media/{download,thumbnail}` routes, whose handlers authenticate
/// nothing themselves. The layer runs before any handler, exactly as it does here, and verifies
/// against the full request path even when the router is nested under a prefix.
pub fn behind_x_matrix(router: axum::Router, x_matrix_ctx: Arc<XMatrixContext>) -> axum::Router {
    router
        .layer(axum::middleware::from_fn(xmatrix::verify_x_matrix))
        .layer(axum::Extension(x_matrix_ctx))
}

fn apply_x_matrix_layer(
    merged: axum::Router<FederationState>,
    state: FederationState,
    x_matrix_ctx: Arc<XMatrixContext>,
) -> axum::Router {
    let rooms = state.rooms.clone();
    merged
        // Runs after routing (a route layer), so the matched `{roomId}` is known, and inside the
        // `X-Matrix` layer below, so the origin it checks has been verified.
        .route_layer(axum::middleware::from_fn_with_state(
            rooms,
            crate::acl::enforce_on_room_routes,
        ))
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
            policy: crate::transport::InboundPolicy::new(true, true),
            write_sink: Arc::new(crate::inbound::StaticWriteSink::new(
                Vec::new(),
                "not supported",
            )),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
            invites: None,
            edu_sink: None,
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
            if route.auth == hs_http::router::AuthKind::None {
                continue;
            }
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

    /// Every route of the federation router is signed except `/version`, which the spec gives
    /// no `security` requirement. `/openid/userinfo`, the other unsigned route under the prefix,
    /// is its own router ([`openid::router`]), served with federation off too.
    #[tokio::test]
    async fn the_only_unsigned_federation_route_is_version() {
        let (_router, manifest) = router(test_state(), test_ctx());
        let unsigned: Vec<(&str, &str)> = manifest
            .routes
            .iter()
            .filter(|r| r.auth == hs_http::router::AuthKind::None)
            .map(|r| (r.method.as_str(), r.path.as_str()))
            .collect();
        assert_eq!(unsigned, [("GET", "/version")]);
        assert!(
            !manifest.routes.iter().any(|r| r.path == "/openid/userinfo"),
            "the federation router does not serve /openid/userinfo"
        );
        let (_router, manifest_v2) = router_v2(test_state(), test_ctx());
        assert!(
            manifest_v2
                .routes
                .iter()
                .all(|r| r.auth != hs_http::router::AuthKind::None),
            "every v2 route is signed"
        );
    }

    /// Every route whose path names a `{roomId}` -- `make_join`, `send_join` (both versions),
    /// `make_leave`, `send_leave`, `make_knock`, `send_knock`, `invite`, `state`, `state_ids`,
    /// `backfill`, `event_auth`, `get_missing_events`, `hierarchy`, `timestamp_to_event` -- is
    /// refused `403 M_FORBIDDEN` for a server the room's `m.room.server_acl` denies (by host,
    /// port ignored), before the handler runs, and counted. Iterating the manifest makes a
    /// room-scoped route added later part of this test without anyone listing it. Without the
    /// route layer, Sytest's nine "Banned servers cannot ..." tests failed.
    #[tokio::test]
    async fn every_room_scoped_route_refuses_a_server_the_room_acl_denies() {
        let dir = tempfile::tempdir().unwrap();
        let evil_keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        struct FixedFetcher(serde_json::Value);
        #[async_trait]
        impl KeyServerFetcher for FixedFetcher {
            async fn fetch_server_key(&self, _server_name: &str) -> Option<serde_json::Value> {
                Some(self.0.clone())
            }
        }
        let origin = "evil.example.org:8448";
        let doc = build_server_key_response(origin, &evil_keys, &[], 3600).unwrap();
        let key_cache: Arc<DynRemoteKeyCache> = Arc::new(RemoteKeyCache::new(
            Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>,
        ));
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "us.example.org".to_string(),
            key_cache,
        });

        let room_id = "!r:us.example.org";
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            room_id,
            crate::room_source::FakeRoom {
                room_version: Some("11".to_owned()),
                world_readable: true,
                joined_servers: vec![origin.to_owned()],
                state: vec![serde_json::json!({
                    "event_id": "$acl", "type": "m.room.server_acl", "state_key": "",
                    "room_id": room_id, "sender": "@admin:us.example.org",
                    "content": {"allow": ["*"], "deny": ["evil.example.org"]},
                })],
                ..Default::default()
            },
        );
        let mut state = test_state();
        state.rooms = Arc::new(rooms);

        let (v1, manifest_v1) = router(state.clone(), ctx.clone());
        let (v2, manifest_v2) = router_v2(state, ctx);
        let mut checked = Vec::new();
        for (router, manifest) in [(v1, manifest_v1), (v2, manifest_v2)] {
            for route in manifest
                .routes
                .iter()
                .filter(|route| route.path.contains("{roomId}"))
            {
                let path = route.path.replace("{roomId}", room_id);
                let path = concretize(&path);
                let body = (route.method != "GET").then(|| serde_json::json!({}));
                let header = xmatrix::sign_request(
                    route.method.as_str(),
                    &path,
                    origin,
                    "us.example.org",
                    body.as_ref(),
                    evil_keys.primary(),
                )
                .unwrap();
                let before = crate::metrics::acl_refusals(crate::acl::endpoint_label(&route.path));
                let response = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(route.method.as_str())
                            .uri(&path)
                            .header(axum::http::header::AUTHORIZATION, header)
                            .header(axum::http::header::CONTENT_TYPE, "application/json")
                            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "{} {} was not refused",
                    route.method,
                    route.path
                );
                let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                    .await
                    .unwrap();
                let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(error["errcode"], "M_FORBIDDEN", "{}", route.path);
                assert!(
                    error["error"].as_str().unwrap().contains("server ACL"),
                    "{} answered {error}",
                    route.path
                );
                assert!(
                    crate::metrics::acl_refusals(crate::acl::endpoint_label(&route.path)) > before
                );
                checked.push(crate::acl::endpoint_label(&route.path));
            }
        }
        for endpoint in [
            "make_join",
            "send_join",
            "make_leave",
            "send_leave",
            "make_knock",
            "send_knock",
            "invite",
            "state",
            "state_ids",
            "backfill",
            "event_auth",
            "get_missing_events",
            "hierarchy",
            "timestamp_to_event",
        ] {
            assert!(checked.contains(&endpoint), "{endpoint} was not checked");
        }
        assert!(!checked.contains(&"other"), "{checked:?}");
    }

    /// An event ID with a `/` in it (room version 3's base64), sent the way this server's client
    /// now sends it (`crate::client::encode_path_segment`), reaches the handler whole and the
    /// signature over the encoded path verifies. Unencoded, the `/` split the path and the route
    /// did not match (`404`), for half of a version-3 room's invites, joins and leaves.
    #[tokio::test]
    async fn an_event_id_with_a_slash_is_routed_whole_when_encoded() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        struct FixedFetcher(serde_json::Value);
        #[async_trait]
        impl KeyServerFetcher for FixedFetcher {
            async fn fetch_server_key(&self, _server_name: &str) -> Option<serde_json::Value> {
                Some(self.0.clone())
            }
        }
        let origin = "them.example.org";
        let doc = build_server_key_response(origin, &keys, &[], 3600).unwrap();
        let key_cache: Arc<DynRemoteKeyCache> = Arc::new(RemoteKeyCache::new(
            Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>,
        ));
        let ctx = Arc::new(XMatrixContext {
            own_server_name: "us.example.org".to_string(),
            key_cache,
        });
        let event_id = "$Ab/cd+ef";
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            "!r:us.example.org",
            crate::room_source::FakeRoom {
                world_readable: true,
                events: [(
                    event_id.to_owned(),
                    serde_json::json!({"event_id": event_id, "type": "m.room.message"}),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );
        let mut state = test_state();
        state.rooms = Arc::new(rooms);
        let (router, _) = router(state, ctx);
        let path = format!("/event/{}", crate::client::encode_path_segment(event_id));
        assert_eq!(path, "/event/$Ab%2Fcd+ef");
        let header =
            xmatrix::sign_request("GET", &path, origin, "us.example.org", None, keys.primary())
                .unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(&path)
                    .header(axum::http::header::AUTHORIZATION, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["pdus"][0]["event_id"], event_id);
    }

    /// `GET /version` with the body of a 200, asserting the spec's `server.name`.
    async fn get_version(router: axum::Router, authorization: Option<String>) -> serde_json::Value {
        let mut request = Request::builder().method("GET").uri("/version");
        if let Some(header) = authorization {
            request = request.header(axum::http::header::AUTHORIZATION, header);
        }
        let response = router
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["server"]["name"], version::SERVER_NAME, "{body}");
        assert_eq!(
            body["server"]["version"],
            env!("CARGO_PKG_VERSION"),
            "{body}"
        );
        body
    }

    /// The spec gives `/version` no `security` requirement, and federation testers and other
    /// servers call it unsigned: it answers `200` with no `Authorization` header at all. On
    /// the live demo before 2026-10-09 it was `401 M_UNAUTHORIZED`.
    #[tokio::test]
    async fn version_answers_an_unsigned_request() {
        let (router, _manifest) = router(test_state(), test_ctx());
        get_version(router, None).await;
    }

    /// A server that signs every request anyway (as this one's own client does) still gets the
    /// answer, and so does one whose signature would not verify: the header is not read.
    #[tokio::test]
    async fn version_answers_a_signed_request_and_ignores_a_bad_signature() {
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
        get_version(router.clone(), Some(header)).await;
        get_version(
            router,
            Some(
                "X-Matrix origin=\"origin.example.org\",destination=\"us.example.org\",\
                 key=\"ed25519:1\",sig=\"AAAA\""
                    .to_owned(),
            ),
        )
        .await;
    }
}
