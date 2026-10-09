//! `GET /_matrix/federation/v1/version`: the name and version of this server's implementation.
//!
//! The spec (`server-server/version.yaml`) gives the endpoint no `security` requirement, Synapse
//! answers it unsigned, and federation testers and other servers call it without an `X-Matrix`
//! header. So it is its own router, merged beside the `X-Matrix` layer in [`super::router`]
//! rather than behind it: an unsigned request and a signed one both get `200` (the
//! `Authorization` header is not read). Before 2026-10-09 it sat inside the layer and an unsigned
//! call was `401 M_UNAUTHORIZED` ("signature verification failed"), which is what the live demo
//! answered.
//!
//! It is still part of the federation router, so a server with `federation.enabled: false`
//! does not serve it (`404`), as Synapse serves `/version` only from its `federation` listener
//! resource. `/openid/userinfo`, the other unsigned route under the prefix, is served either way
//! ([`super::openid`]).

use axum::response::{IntoResponse, Response};
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};

/// The implementation name `/version` reports, as `server.name`.
pub const SERVER_NAME: &str = "hs";

/// The `/version` route, spec-relative (the caller mounts it under `/_matrix/federation/v1`),
/// unauthenticated in the manifest ([`AuthKind::None`]).
pub(super) fn router() -> (axum::Router, RouteManifest) {
    Builder::<()>::new()
        .get(
            "/version",
            version,
            RouteMeta::new(Surface::MatrixFederation, AuthKind::None)
                .with_operation_id("federationVersion"),
        )
        .build()
}

/// `{"server": {"name", "version"}}`. Logged at debug level: callers are federation testers and
/// other servers probing reachability, and each request is already counted by the HTTP layer.
async fn version() -> Response {
    tracing::debug!("answering GET /_matrix/federation/v1/version");
    axum::Json(serde_json::json!({
        "server": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
    }))
    .into_response()
}
