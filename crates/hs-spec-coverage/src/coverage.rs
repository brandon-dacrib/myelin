//! Computes registered/missing/extra routes per API family by diffing [`crate::spec::SpecRoute`]s
//! against a [`crate::manifest::RouteManifest`].
//!
//! Matching is exact `(method, path)` string equality after the spec side already has its base
//! path prepended ([`crate::spec::load_family`]) — both sides use `{param}` path-parameter
//! syntax (RFC 0005 section 2), so a route registered with the same path-parameter *names* the
//! spec uses matches directly. A route whose implementation renamed a path parameter (`{roomId}`
//! vs `{room_id}`) will show up as both missing and extra; that's a real naming mismatch worth
//! surfacing, not a false positive to paper over here.

use std::collections::BTreeSet;

use crate::manifest::RouteManifest;
use crate::spec::{ApiFamily, SpecRoute};

/// Coverage for one API family.
#[derive(Debug, Clone)]
pub struct ApiCoverage {
    /// Which API this covers.
    pub family: ApiFamily,
    /// How many `(method, path)` pairs the spec declares for this family.
    pub spec_total: usize,
    /// How many of those are present in the manifest under this family's surface.
    pub registered: usize,
    /// Spec routes with no matching manifest entry, sorted by path then method.
    pub missing: Vec<SpecRoute>,
    /// Manifest entries under this family's surface with no matching spec route, sorted by path
    /// then method. A nonzero count here means either the spec moved (an endpoint was renamed or
    /// deprecated-and-removed in a spec version this crate's spec checkout has), or the
    /// implementation registered something spec-non-compliant.
    pub extra: Vec<(String, String)>,
}

impl ApiCoverage {
    /// Percentage of spec routes registered, `0.0` when the spec declares no routes for this
    /// family (never divides by zero).
    #[must_use]
    pub fn percent(&self) -> f64 {
        if self.spec_total == 0 {
            return 0.0;
        }
        100.0 * self.registered as f64 / self.spec_total as f64
    }
}

/// The full report: one [`ApiCoverage`] per family, in [`ApiFamily::ALL`] order.
#[derive(Debug, Clone)]
pub struct CoverageReport {
    /// Per-family coverage, in [`ApiFamily::ALL`] order.
    pub apis: Vec<ApiCoverage>,
}

impl CoverageReport {
    /// Diffs `spec_routes` (from [`crate::spec::load_all`] or a per-family
    /// [`crate::spec::load_family`] call) against `manifest`.
    #[must_use]
    pub fn compute(spec_routes: &[SpecRoute], manifest: &RouteManifest) -> Self {
        let apis = ApiFamily::ALL
            .into_iter()
            .map(|family| Self::compute_family(family, spec_routes, manifest))
            .collect();
        Self { apis }
    }

    fn compute_family(
        family: ApiFamily,
        spec_routes: &[SpecRoute],
        manifest: &RouteManifest,
    ) -> ApiCoverage {
        let family_routes: Vec<&SpecRoute> =
            spec_routes.iter().filter(|r| r.family == family).collect();
        let registered_pairs: BTreeSet<(String, String)> =
            manifest.method_paths_for(family.manifest_surface());
        let spec_pairs: BTreeSet<(String, String)> = family_routes
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect();

        let mut missing: Vec<SpecRoute> = family_routes
            .into_iter()
            .filter(|r| !registered_pairs.contains(&(r.method.clone(), r.path.clone())))
            .cloned()
            .collect();
        missing.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));

        let mut extra: Vec<(String, String)> =
            registered_pairs.difference(&spec_pairs).cloned().collect();
        extra.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        ApiCoverage {
            family,
            spec_total: spec_pairs.len(),
            registered: spec_pairs.len() - missing.len(),
            missing,
            extra,
        }
    }

    /// Overall percentage across every family (spec routes registered / spec routes total),
    /// `0.0` if the spec has no routes at all.
    #[must_use]
    pub fn overall_percent(&self) -> f64 {
        let total: usize = self.apis.iter().map(|a| a.spec_total).sum();
        let registered: usize = self.apis.iter().map(|a| a.registered).sum();
        if total == 0 {
            return 0.0;
        }
        100.0 * registered as f64 / total as f64
    }

    /// Total spec routes across every family.
    #[must_use]
    pub fn total_spec_routes(&self) -> usize {
        self.apis.iter().map(|a| a.spec_total).sum()
    }

    /// Total registered routes across every family.
    #[must_use]
    pub fn total_registered(&self) -> usize {
        self.apis.iter().map(|a| a.registered).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Route;

    fn spec_route(family: ApiFamily, method: &str, path: &str) -> SpecRoute {
        SpecRoute {
            method: method.to_string(),
            path: path.to_string(),
            family,
            operation_id: None,
            source_file: "test.yaml".to_string(),
        }
    }

    fn manifest_route(surface: &str, method: &str, path: &str) -> Route {
        Route {
            method: method.to_string(),
            path: path.to_string(),
            surface: surface.to_string(),
            operation_id: None,
            auth: "matrix".to_string(),
            required_scope: None,
            rate_limited: false,
        }
    }

    #[test]
    fn empty_manifest_reports_everything_missing() {
        let spec = vec![
            spec_route(ApiFamily::ClientServer, "GET", "/login"),
            spec_route(ApiFamily::ClientServer, "POST", "/login"),
        ];
        let report = CoverageReport::compute(&spec, &RouteManifest::empty());
        let cs = report
            .apis
            .iter()
            .find(|a| a.family == ApiFamily::ClientServer)
            .unwrap();
        assert_eq!(cs.spec_total, 2);
        assert_eq!(cs.registered, 0);
        assert_eq!(cs.missing.len(), 2);
        assert_eq!(cs.percent(), 0.0);
    }

    #[test]
    fn a_fully_matching_manifest_reports_full_coverage() {
        let spec = vec![
            spec_route(ApiFamily::ClientServer, "GET", "/login"),
            spec_route(ApiFamily::ClientServer, "POST", "/login"),
        ];
        let manifest = RouteManifest {
            generated_at: None,
            routes: vec![
                manifest_route("matrix-client", "GET", "/login"),
                manifest_route("matrix-client", "POST", "/login"),
            ],
        };
        let report = CoverageReport::compute(&spec, &manifest);
        let cs = report
            .apis
            .iter()
            .find(|a| a.family == ApiFamily::ClientServer)
            .unwrap();
        assert_eq!(cs.registered, 2);
        assert!(cs.missing.is_empty());
        assert!(cs.extra.is_empty());
        assert_eq!(cs.percent(), 100.0);
    }

    #[test]
    fn a_registered_route_not_in_the_spec_is_extra() {
        let spec = vec![spec_route(ApiFamily::ClientServer, "GET", "/login")];
        let manifest = RouteManifest {
            generated_at: None,
            routes: vec![
                manifest_route("matrix-client", "GET", "/login"),
                manifest_route("matrix-client", "GET", "/not-in-spec"),
            ],
        };
        let report = CoverageReport::compute(&spec, &manifest);
        let cs = report
            .apis
            .iter()
            .find(|a| a.family == ApiFamily::ClientServer)
            .unwrap();
        assert_eq!(cs.registered, 1);
        assert!(cs.missing.is_empty());
        assert_eq!(
            cs.extra,
            vec![("GET".to_string(), "/not-in-spec".to_string())]
        );
    }

    #[test]
    fn routes_under_a_different_surface_do_not_count_toward_this_family() {
        let spec = vec![spec_route(ApiFamily::ClientServer, "GET", "/login")];
        let manifest = RouteManifest {
            generated_at: None,
            // Registered, but on the admin surface, not matrix-client: still missing here.
            routes: vec![manifest_route("admin", "GET", "/login")],
        };
        let report = CoverageReport::compute(&spec, &manifest);
        let cs = report
            .apis
            .iter()
            .find(|a| a.family == ApiFamily::ClientServer)
            .unwrap();
        assert_eq!(cs.registered, 0);
        assert_eq!(cs.missing.len(), 1);
    }

    #[test]
    fn overall_percent_aggregates_across_families() {
        let spec = vec![
            spec_route(ApiFamily::ClientServer, "GET", "/a"),
            spec_route(ApiFamily::ServerServer, "GET", "/b"),
        ];
        let manifest = RouteManifest {
            generated_at: None,
            routes: vec![manifest_route("matrix-client", "GET", "/a")],
        };
        let report = CoverageReport::compute(&spec, &manifest);
        assert_eq!(report.total_spec_routes(), 2);
        assert_eq!(report.total_registered(), 1);
        assert_eq!(report.overall_percent(), 50.0);
    }
}
