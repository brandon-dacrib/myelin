//! Which route families a listener serves: `listeners[].resources` made true.
//!
//! `hs serve` builds one router and binds every configured listener to it; what differs per
//! listener is a thin filter in front of the router ([`layer`]) that classifies the request path
//! into one resource family ([`resource_for`]) and refuses, with the same `404` an unknown route
//! gets, any family the listener's `resources` list does not name. So the Helm chart's metrics
//! port answers `/metrics` and `/health/*` and nothing else, and its public port answers neither
//! `/metrics` nor the admin API. A path that belongs to no family (there are none the server
//! mounts, which [`crate::serve`]'s tests check against the route manifest) passes through to the
//! router, whose fallback answers `404` for it.
//!
//! The classification is a handful of prefix comparisons on the path, once per request, before
//! routing: cheap enough for the hot path. The set is a bitmask.
//!
//! Where a family's edge is debatable, the choice is the one that keeps a client working against
//! a listener that names only `client`: the authenticated media routes under
//! `/_matrix/client/v1/media` are `client`, not `media` (a client that can log in must be able
//! to fetch an avatar from the same port), and `media` is only the legacy unauthenticated
//! `/_matrix/media/*`. The bare `/` is `admin`, since all it does is redirect to the management
//! interface. `/.well-known/matrix/server` is `federation` and the other two `.well-known`
//! documents are `client`, following who reads each.

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use hs_config::listeners::ListenerResource;

/// The families a listener serves, as a bitmask over [`ListenerResource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSet(u8);

impl ResourceSet {
    /// The set naming exactly `resources` (duplicates are fine).
    pub fn new(resources: &[ListenerResource]) -> Self {
        Self(resources.iter().fold(0, |bits, r| bits | bit(*r)))
    }

    /// Every family: what the unfiltered router serves.
    pub const ALL: Self = Self(0b11_1111);

    /// Whether `resource` is in the set.
    pub fn contains(self, resource: ListenerResource) -> bool {
        self.0 & bit(resource) != 0
    }

    /// The families in the set, in [`ListenerResource::ALL`]'s order.
    pub fn iter(self) -> impl Iterator<Item = ListenerResource> {
        ListenerResource::ALL
            .into_iter()
            .filter(move |r| self.contains(*r))
    }

    /// The families in the set as the configuration spells them, comma-separated: what the
    /// startup log shows per listener.
    pub fn names(self) -> String {
        self.iter()
            .map(ListenerResource::name)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn bit(resource: ListenerResource) -> u8 {
    match resource {
        ListenerResource::Client => 1 << 0,
        ListenerResource::Federation => 1 << 1,
        ListenerResource::Media => 1 << 2,
        ListenerResource::Metrics => 1 << 3,
        ListenerResource::Admin => 1 << 4,
        ListenerResource::Health => 1 << 5,
    }
}

/// The resource family `path` belongs to, or `None` for a path the server does not mount at all.
///
/// The module docs say where the edges are and why.
pub fn resource_for(path: &str) -> Option<ListenerResource> {
    use ListenerResource::*;
    if let Some(rest) = path.strip_prefix("/_matrix/") {
        return if rest == "client" || rest.starts_with("client/") || rest.starts_with("r0/") {
            Some(Client)
        } else if rest.starts_with("federation/") || rest.starts_with("key/") {
            Some(Federation)
        } else if rest.starts_with("media/") {
            Some(Media)
        } else {
            None
        };
    }
    if let Some(rest) = path.strip_prefix("/.well-known/matrix/") {
        return match rest {
            "server" => Some(Federation),
            "client" | "support" => Some(Client),
            _ => None,
        };
    }
    if let Some(rest) = path.strip_prefix("/_synapse/") {
        return if rest.starts_with("admin/") {
            Some(Admin)
        } else if rest.starts_with("client/") {
            Some(Client)
        } else {
            None
        };
    }
    if path == "/" || is_under(path, "/admin") || is_under(path, hs_http::fallback::ADMIN_PREFIX) {
        return Some(Admin);
    }
    if is_under(path, "/health") {
        return Some(Health);
    }
    if path == "/metrics" {
        return Some(Metrics);
    }
    if is_under(path, hs_bridges::manager::ROUTE_PREFIX) {
        return Some(Client);
    }
    None
}

/// `path` is `prefix` itself or something below it (`/admin` and `/admin/x`, not `/adminx`).
fn is_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `router`, answering only the families in `resources`; the rest get the unknown-endpoint
/// `404` in the shape the path's API speaks ([`hs_http::fallback::unknown_endpoint_response`]).
///
/// The filter is outermost, so a refused request is not routed, counted or traced: it was never
/// this listener's to serve. It is logged at debug with the path and what the listener does
/// serve, which is what an operator needs when a probe or a scrape is pointed at the wrong port.
pub fn layer(router: Router, resources: ResourceSet) -> Router {
    if resources == ResourceSet::ALL {
        return router;
    }
    router.layer(middleware::from_fn_with_state(resources, filter))
}

async fn filter(State(resources): State<ResourceSet>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    match resource_for(path) {
        Some(resource) if !resources.contains(resource) => {
            tracing::debug!(
                path,
                resource = resource.name(),
                serves = %resources.names(),
                "refused: this listener does not serve that resource family"
            );
            hs_http::fallback::unknown_endpoint_response(path)
        }
        _ => next.run(request).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ListenerResource::*;
    use axum::body::Body;
    use axum::routing::get;
    use http::StatusCode;
    use tower::ServiceExt;

    #[test]
    fn classifies_every_mounted_prefix() {
        let cases = [
            ("/_matrix/client/versions", Some(Client)),
            ("/_matrix/client/v3/login", Some(Client)),
            ("/_matrix/client/r0/login", Some(Client)),
            ("/_matrix/client/v1/media/download/x/y", Some(Client)),
            ("/_matrix/client/unstable/x", Some(Client)),
            ("/_matrix/r0/login", Some(Client)),
            ("/_synapse/client/x", Some(Client)),
            ("/.well-known/matrix/client", Some(Client)),
            ("/.well-known/matrix/support", Some(Client)),
            ("/_myelin/bridges/_matrix/app/v1/ping", Some(Client)),
            ("/_matrix/federation/v1/version", Some(Federation)),
            ("/_matrix/federation/v1/media/download/x", Some(Federation)),
            ("/_matrix/key/v2/server", Some(Federation)),
            ("/.well-known/matrix/server", Some(Federation)),
            ("/_matrix/media/v3/upload", Some(Media)),
            ("/_matrix/media/v1/create", Some(Media)),
            ("/metrics", Some(Metrics)),
            ("/health/live", Some(Health)),
            ("/health/ready", Some(Health)),
            ("/", Some(Admin)),
            ("/admin", Some(Admin)),
            ("/admin/", Some(Admin)),
            ("/admin/setup", Some(Admin)),
            ("/admin/assets/index-abc.js", Some(Admin)),
            ("/api/v1/me", Some(Admin)),
            ("/_synapse/admin/v1/register", Some(Admin)),
            ("/adminx", None),
            ("/healthz", None),
            ("/metrics/x", None),
            ("/_matrix/app/v1/ping", None),
            ("/.well-known/matrix/x", None),
            ("/nothing", None),
        ];
        for (path, want) in cases {
            assert_eq!(resource_for(path), want, "{path}");
        }
    }

    #[test]
    fn set_membership_and_names() {
        let set = ResourceSet::new(&[Metrics, Health, Metrics]);
        assert!(set.contains(Metrics));
        assert!(set.contains(Health));
        assert!(!set.contains(Client));
        assert_eq!(set.names(), "metrics,health");
        assert_eq!(ResourceSet::new(&ListenerResource::ALL), ResourceSet::ALL);
        assert_eq!(
            ResourceSet::ALL.names(),
            "client,federation,media,metrics,admin,health"
        );
        assert_eq!(ResourceSet::new(&[]).names(), "");
    }

    fn app() -> Router {
        hs_http::fallback::apply(
            Router::new()
                .route("/_matrix/client/versions", get(|| async { "versions" }))
                .route("/metrics", get(|| async { "metrics" }))
                .route("/api/v1/me", get(|| async { "me" }))
                .route("/health/live", get(|| async { "ok" })),
        )
    }

    async fn get_status(router: Router, path: &str) -> (StatusCode, String) {
        let response = router
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn a_listener_answers_only_its_families() {
        let metrics_only = layer(app(), ResourceSet::new(&[Metrics, Health]));
        let (status, body) = get_status(metrics_only.clone(), "/metrics").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "metrics"));
        let (status, _) = get_status(metrics_only.clone(), "/health/live").await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = get_status(metrics_only.clone(), "/_matrix/client/versions").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("M_UNRECOGNIZED"), "{body}");
        // The admin API's refusal speaks its own error shape, as its fallback does.
        let (status, body) = get_status(metrics_only.clone(), "/api/v1/me").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("no such endpoint"), "{body}");
        // A path nobody mounts is the router's fallback, as before.
        let (status, body) = get_status(metrics_only, "/nothing").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("M_UNRECOGNIZED"), "{body}");
    }

    #[tokio::test]
    async fn the_full_set_is_the_router_itself() {
        let (status, _) = get_status(layer(app(), ResourceSet::ALL), "/api/v1/me").await;
        assert_eq!(status, StatusCode::OK);
    }
}
