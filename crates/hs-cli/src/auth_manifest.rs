//! Hand-mirrored `routes.json` entries for `hs_auth::routes::router()`.
//!
//! `hs-auth` hands over a pre-built `axum::Router<AuthState>` fragment (its own doc comment:
//! "hands over a router fragment, not a listener"), not something built through
//! `hs_http::router::Builder` the way `hs-media`'s and `hs-admin`'s routers are — so mounting it
//! does not itself produce `hs_http::router::Route` entries for the `routes.json` manifest
//! (`docs/rfcs/0005-routes-json-manifest.md`). This module is the mechanical, by-hand mirror of
//! exactly what `hs_auth::routes::router()` registers (`crates/hs-auth/src/routes/mod.rs`), kept
//! in one place so it is obvious to update if that list ever changes, and covered by a test that
//! at least checks the count and a few representative entries rather than trusting the mirror
//! silently.
//!
//! Paths here are spec-relative (`"/login"`, not `/_matrix/client/v3/login`) — `crate::serve`
//! passes this list to `hs_http::router::Builder::merge_router` twice, once per version prefix
//! (`/_matrix/client/v3` and `/_matrix/client/r0`), which prepends the prefix itself.

use hs_http::router::{AuthKind, Route, Surface};

fn route(method: &str, path: &str, auth: AuthKind, operation_id: &str) -> Route {
    Route {
        method: method.to_owned(),
        path: path.to_owned(),
        surface: Surface::MatrixClient,
        operation_id: Some(operation_id.to_owned()),
        auth,
        required_scope: None,
        // hs-auth's `AuthState.rate_limiter` field exists but no handler in
        // `crates/hs-auth/src/routes/*.rs` calls it yet (verified by grep: no `rate_limiter`
        // reference anywhere under `crates/hs-auth/src/routes/`), so `false` here is the honest
        // current answer, not a guess — matching this track's "don't overclaim" instruction for
        // `/versions` and `/capabilities`.
        rate_limited: false,
    }
}

/// Every route `hs_auth::routes::router()` registers, mirroring
/// `crates/hs-auth/src/routes/mod.rs::router` exactly (method, path and auth requirement; see
/// that function for the handlers themselves).
#[must_use]
pub fn routes() -> Vec<Route> {
    use AuthKind::{Matrix, None as NoAuth};
    vec![
        route("GET", "/login", NoAuth, "getLoginFlows"),
        route("POST", "/login", NoAuth, "login"),
        route("POST", "/logout", Matrix, "logout"),
        route("POST", "/logout/all", Matrix, "logoutAll"),
        route("POST", "/refresh", NoAuth, "refresh"),
        route("GET", "/account/whoami", Matrix, "whoami"),
        route("POST", "/register", NoAuth, "register"),
        route("GET", "/register/available", NoAuth, "registerAvailable"),
        route("POST", "/account/password", Matrix, "changePassword"),
        route("POST", "/account/deactivate", Matrix, "deactivateAccount"),
        route("GET", "/password_policy", NoAuth, "passwordPolicy"),
        route("GET", "/devices", Matrix, "getDevices"),
        route("GET", "/devices/{deviceId}", Matrix, "getDevice"),
        route("PUT", "/devices/{deviceId}", Matrix, "updateDevice"),
        route("DELETE", "/devices/{deviceId}", Matrix, "deleteDevice"),
        route("POST", "/delete_devices", Matrix, "deleteDevices"),
    ]
}

/// The two `/_synapse/admin/v1/register` entries for `hs_auth::synapse_admin_router()`
/// (`crates/hs-auth/src/routes/synapse_admin.rs`), mirrored by hand for the same reason as
/// [`routes`] above. Unlike that list these paths are **absolute**: `crate::serve` merges this
/// fragment at the router root, not under a version prefix, so no prefix is prepended.
///
/// `AuthKind::None` is the honest answer even though the endpoint is privileged: the credential
/// is the HMAC in the request body, not an `Authorization` header, so a manifest consumer must
/// not be told to send a token. The surface is [`Surface::SynapseAdminCompat`], since this is a
/// Synapse-compatibility route (`docs/compat/cli-shims.md`) rather than a spec one.
#[must_use]
pub fn synapse_admin_routes() -> Vec<Route> {
    fn compat_route(method: &str, path: &str, operation_id: &str) -> Route {
        Route {
            surface: Surface::SynapseAdminCompat,
            ..route(method, path, AuthKind::None, operation_id)
        }
    }
    vec![
        compat_route(
            "GET",
            "/_synapse/admin/v1/register",
            "synapseAdminRegisterNonce",
        ),
        compat_route(
            "POST",
            "/_synapse/admin/v1/register",
            "synapseAdminRegister",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_the_expected_route_count() {
        // One entry per `.route(...)` call in `hs_auth::routes::router()`: 10 distinct paths, 3
        // of which (`/login`, `/devices/{deviceId}`, plus the method-combining ones) carry more
        // than one method, for 16 (method, path) pairs total.
        assert_eq!(routes().len(), 16);
    }

    #[test]
    fn login_get_is_unauthenticated_and_whoami_requires_auth() {
        let all = routes();
        let login_get = all
            .iter()
            .find(|r| r.method == "GET" && r.path == "/login")
            .unwrap();
        assert_eq!(login_get.auth, AuthKind::None);

        let whoami = all
            .iter()
            .find(|r| r.method == "GET" && r.path == "/account/whoami")
            .unwrap();
        assert_eq!(whoami.auth, AuthKind::Matrix);
    }

    #[test]
    fn every_route_is_the_matrix_client_surface() {
        assert!(routes().iter().all(|r| r.surface == Surface::MatrixClient));
    }

    #[test]
    fn the_synapse_admin_routes_are_absolute_and_compat_surfaced() {
        let routes = synapse_admin_routes();
        assert_eq!(routes.len(), 2);
        assert!(
            routes
                .iter()
                .all(|r| r.path == "/_synapse/admin/v1/register")
        );
        assert!(
            routes
                .iter()
                .all(|r| r.surface == Surface::SynapseAdminCompat)
        );
        // The shared secret's HMAC is the credential, not a bearer token.
        assert!(routes.iter().all(|r| r.auth == AuthKind::None));
    }
}
