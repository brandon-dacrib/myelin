//! Mounts `hs-compat`'s `/_synapse/admin` compatibility routes, and lists their `routes.json`
//! entries from the list `hs-compat` keeps beside its router.
//!
//! `hs-compat` hands over a plain `axum::Router` rather than something built through
//! `hs_http::router::Builder` (the same shape `hs-auth`'s fragments use, and for the same reason:
//! that crate does not depend on `hs-http`), so mounting it produces no manifest entries by
//! itself. `hs-compat` names every route it mounts in `SYNAPSE_ADMIN_ROUTES` (and tests that the
//! two agree); this module turns that list into manifest entries.
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

/// Every route `hs_compat::admin_proxy_router` registers, from the list `hs-compat` keeps
/// beside its router (`hs_compat::SYNAPSE_ADMIN_ROUTES`, which its own tests hold to what is
/// mounted).
#[must_use]
pub fn routes() -> Vec<Route> {
    hs_compat::SYNAPSE_ADMIN_ROUTES
        .iter()
        .map(|(method, path, operation_id)| Route {
            method: (*method).to_owned(),
            path: (*path).to_owned(),
            surface: Surface::SynapseAdminCompat,
            operation_id: Some((*operation_id).to_owned()),
            // The shim forwards the caller's `Authorization` header into the native admin
            // router, which is where the token is actually verified and its scope checked.
            // `Admin` is the honest description of what a caller needs, even though this layer
            // does not check it itself.
            auth: AuthKind::Admin,
            required_scope: None,
            rate_limited: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_every_shimmed_route_on_the_compat_surface() {
        let routes = routes();
        assert!(routes.len() >= 60, "{}", routes.len());
        assert!(routes.iter().filter(|r| r.method == "GET").count() >= 30);
        let mut seen = std::collections::HashSet::new();
        for r in &routes {
            assert!(
                seen.insert((r.method.clone(), r.path.clone())),
                "{} {} twice",
                r.method,
                r.path
            );
        }
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
