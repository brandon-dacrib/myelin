//! Runs the real spec parser against the actual checkout at `refs/matrix-spec/data/api/` (cloned
//! by `tools/fetch-refs.sh`; present in this workspace already, no network needed here). This is
//! the "tests over the real spec data" deliverable, distinct from `src/spec.rs`'s unit tests
//! against synthetic fixtures: it proves the parser survives the spec's actual YAML shapes (long
//! multi-line descriptions, `$ref`s this crate never follows, deprecated/`x-`-prefixed
//! extensions, ...), not just the shapes this crate's author anticipated.

use std::path::{Path, PathBuf};

use hs_spec_coverage::{
    ApiFamily, CoverageReport, RouteManifest, load_all, load_family, render_markdown,
};

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../refs/matrix-spec/data/api")
}

/// Skips (rather than fails) if `refs/matrix-spec` has not been cloned — `tools/fetch-refs.sh`
/// needs network, which this test environment might not have. When it *is* present (as it is in
/// this workspace), the rest of this file exercises it for real.
macro_rules! require_spec_or_skip {
    () => {
        if !spec_dir().is_dir() {
            eprintln!(
                "skipping: {} not found; run tools/fetch-refs.sh (network required)",
                spec_dir().display()
            );
            return;
        }
    };
}

#[test]
fn every_family_parses_and_yields_a_plausible_number_of_routes() {
    require_spec_or_skip!();
    for family in ApiFamily::ALL {
        let routes = load_family(&spec_dir(), family).unwrap_or_else(|e| panic!("{family}: {e}"));
        assert!(
            !routes.is_empty(),
            "{family}: expected at least one route parsed from the real spec checkout"
        );
        for route in &routes {
            assert!(
                route.path.starts_with('/'),
                "{family}: path {} does not start with /",
                route.path
            );
            assert!(
                matches!(
                    route.method.as_str(),
                    "GET" | "PUT" | "POST" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
                ),
                "{family}: unexpected method {}",
                route.method
            );
        }
    }
}

#[test]
fn client_server_has_well_over_a_hundred_routes() {
    require_spec_or_skip!();
    // 72 top-level files as of this writing, most with several operations; this is a loose sanity
    // floor, not a pinned count (the spec checkout will grow over time).
    let routes = load_family(&spec_dir(), ApiFamily::ClientServer).unwrap();
    assert!(
        routes.len() > 150,
        "only parsed {} client-server routes",
        routes.len()
    );
}

#[test]
fn well_known_legacy_auth_routes_are_present_with_the_client_v3_base_path() {
    require_spec_or_skip!();
    let routes = load_family(&spec_dir(), ApiFamily::ClientServer).unwrap();
    let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
    for expected in [
        "/_matrix/client/v3/login",
        "/_matrix/client/v3/logout",
        "/_matrix/client/v3/register",
        "/_matrix/client/v3/refresh",
        "/_matrix/client/v3/account/whoami",
    ] {
        assert!(
            paths.contains(&expected),
            "expected {expected} in parsed client-server routes"
        );
    }
}

#[test]
fn federation_send_transaction_is_present_with_the_federation_v1_base_path() {
    require_spec_or_skip!();
    let routes = load_family(&spec_dir(), ApiFamily::ServerServer).unwrap();
    assert!(
        routes
            .iter()
            .any(|r| r.method == "PUT" && r.path.starts_with("/_matrix/federation/v1/send/")),
        "expected a PUT /_matrix/federation/v1/send/{{txnId}} route"
    );
}

#[test]
fn coverage_against_an_empty_manifest_reports_every_route_missing_and_none_extra() {
    require_spec_or_skip!();
    let routes = load_all(&spec_dir()).unwrap();
    assert!(
        routes.len() > 200,
        "expected well over 200 routes across all five APIs, got {}",
        routes.len()
    );

    let report = CoverageReport::compute(&routes, &RouteManifest::empty());
    assert_eq!(report.total_registered(), 0);
    assert_eq!(report.total_spec_routes(), routes.len());
    for api in &report.apis {
        assert!(
            api.extra.is_empty(),
            "{}: empty manifest cannot have extra routes",
            api.family
        );
    }
}

#[test]
fn coverage_against_hs_auths_actual_route_list_shows_real_partial_coverage() {
    require_spec_or_skip!();
    // hs-auth (track 07, crates/hs-auth/src/routes/mod.rs) mounts these bare paths under
    // /_matrix/client/v3 in its real router. Hand-listing them here (rather than depending on
    // hs-auth to introspect its axum::Router, which offers no reflection API) is exactly the
    // routes.json a hs-http::router::Builder would emit for that router once something mounts it
    // (RFC 0005) — this test is the demonstration that hs-spec-coverage produces a sensible,
    // partial-but-real coverage number the day *some* routes exist, not just the two extremes of
    // "nothing" or "a synthetic fixture."
    let auth_paths = [
        ("GET", "/login"),
        ("POST", "/login"),
        ("POST", "/logout"),
        ("POST", "/logout/all"),
        ("POST", "/refresh"),
        ("GET", "/account/whoami"),
        ("POST", "/register"),
        ("GET", "/register/available"),
        ("POST", "/account/password"),
        ("POST", "/account/deactivate"),
        ("GET", "/password_policy"),
        ("GET", "/devices"),
        ("GET", "/devices/{deviceId}"),
        ("PUT", "/devices/{deviceId}"),
        ("DELETE", "/devices/{deviceId}"),
        ("POST", "/delete_devices"),
    ];
    let manifest = RouteManifest {
        generated_at: Some("2026-09-18T00:00:00Z".to_string()),
        routes: auth_paths
            .iter()
            .map(|(method, path)| hs_spec_coverage::manifest::Route {
                method: method.to_string(),
                path: format!("/_matrix/client/v3{path}"),
                surface: "matrix-client".to_string(),
                operation_id: None,
                auth: "matrix".to_string(),
                required_scope: None,
                rate_limited: false,
            })
            .collect(),
    };

    let routes = load_all(&spec_dir()).unwrap();
    let report = CoverageReport::compute(&routes, &manifest);
    let cs = report
        .apis
        .iter()
        .find(|a| a.family == ApiFamily::ClientServer)
        .unwrap();

    // Every hs-auth route except GET /password_policy (not present in this spec checkout — a
    // capabilities-adjacent addition this repo's refs/matrix-spec snapshot predates) is a real
    // spec route, so it shows up as "extra": a genuine, tool-caught mismatch between what
    // hs-auth serves and what this spec checkout declares, not a bug in the coverage tool.
    assert_eq!(cs.registered, auth_paths.len() - 1);
    assert_eq!(
        cs.extra,
        vec![(
            "GET".to_string(),
            "/_matrix/client/v3/password_policy".to_string()
        )]
    );
    // ...out of a much larger total (the client-server API has far more than login/registration).
    assert!(cs.spec_total > auth_paths.len() * 5);
    assert!(cs.percent() > 0.0 && cs.percent() < 20.0);

    // The Markdown report renders without panicking over this realistic, partial state.
    let markdown = render_markdown(&report, 50);
    assert!(markdown.contains("client-server"));
}
