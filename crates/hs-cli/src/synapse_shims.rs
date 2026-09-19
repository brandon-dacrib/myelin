//! Mounts `hs-compat`'s read-only `/_synapse/admin` compatibility routes, and mirrors their
//! `routes.json` entries by hand.
//!
//! `hs-compat` hands over a plain `axum::Router` rather than something built through
//! `hs_http::router::Builder` (the same shape `hs-auth`'s fragments use, and for the same reason:
//! that crate does not depend on `hs-http`), so mounting it produces no manifest entries by
//! itself. This module is the mechanical mirror, kept beside the mount so the two are obvious to
//! change together — exactly the arrangement `crate::auth_manifest` documents for `hs-auth`.
//!
//! These routes exist so that tooling written against Synapse's admin API keeps working: they
//! forward into this server's own `/api/v1` router and reshape the response, rather than
//! reimplementing any logic, so the native surface and the compatibility surface can never
//! disagree about the data or about who is allowed to see it.

use hs_http::router::{AuthKind, Route, Surface};

/// The compatibility router, plus the manifest entries describing it.
pub fn router(native_admin: axum::Router) -> (axum::Router, Vec<Route>) {
    let router = hs_compat::admin_proxy_router(hs_compat::AdminProxyState::new(native_admin));
    (router, routes())
}

fn route(path: &str, operation_id: &str) -> Route {
    Route {
        method: "GET".to_owned(),
        path: path.to_owned(),
        surface: Surface::SynapseAdminCompat,
        operation_id: Some(operation_id.to_owned()),
        // The shim forwards the caller's `Authorization` header into the native admin router,
        // which is where the token is actually verified and its scope checked. `Admin` is the
        // honest description of what a caller needs, even though this layer does not check it
        // itself.
        auth: AuthKind::Admin,
        required_scope: None,
        rate_limited: false,
    }
}

/// Every route `hs_compat::admin_proxy_router` registers, mirrored by hand.
#[must_use]
pub fn routes() -> Vec<Route> {
    vec![
        route(
            "/_synapse/admin/v1/server_version",
            "synapseAdminServerVersion",
        ),
        route("/_synapse/admin/v2/users", "synapseAdminUsersList"),
        route("/_synapse/admin/v2/users/{user_id}", "synapseAdminUsersGet"),
        route("/_synapse/admin/v1/rooms", "synapseAdminRoomsList"),
        route("/_synapse/admin/v1/rooms/{room_id}", "synapseAdminRoomsGet"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_every_shimmed_route_on_the_compat_surface() {
        let routes = routes();
        assert_eq!(routes.len(), 5);
        assert!(routes.iter().all(|r| r.method == "GET"));
        assert!(
            routes
                .iter()
                .all(|r| r.surface == Surface::SynapseAdminCompat)
        );
        assert!(
            routes
                .iter()
                .all(|r| r.path.starts_with("/_synapse/admin/"))
        );
    }
}
