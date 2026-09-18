//! Hand-mirrored `routes.json` entry for `hs_appservice::routes::ping_router`, following exactly
//! the pattern `crate::auth_manifest` already established for `hs-auth`'s router fragment: like
//! `hs-auth`'s, `ping_router` is a bare `axum::Router<AuthState>`, not built through
//! `hs_http::router::Builder`, so mounting it produces no [`hs_http::router::Route`] entries on
//! its own.
//!
//! Path is spec-relative (`"/appservice/{appserviceId}/ping"`) — `crate::serve` mounts it under
//! `/_matrix/client/v1` via `Builder::merge_router`, which prepends that prefix.

use hs_http::router::{AuthKind, Route, Surface};

/// The one route `hs_appservice::routes::ping_router` registers
/// (`crates/hs-appservice/src/routes.rs`): `POST .../appservice/{appserviceId}/ping`, appservice
/// bearer-token authenticated.
#[must_use]
pub fn routes() -> Vec<Route> {
    vec![Route {
        method: "POST".to_owned(),
        path: "/appservice/{appserviceId}/ping".to_owned(),
        surface: Surface::MatrixClient,
        operation_id: Some("appservicePing".to_owned()),
        auth: AuthKind::Appservice,
        required_scope: None,
        rate_limited: false,
    }]
}
