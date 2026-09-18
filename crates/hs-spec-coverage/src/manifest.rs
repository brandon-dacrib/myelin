//! The `routes.json` route manifest: what a router built anywhere in this workspace emits, and
//! what this crate compares the spec against. See `docs/rfcs/0005-routes-json-manifest.md` for
//! the authoritative format definition (owned by track 15, `hs-http::router`, which is the
//! reference producer — `crates/hs-http/src/router.rs`).
//!
//! This module defines its own copy of the schema rather than depending on `hs-http` so that
//! `hs-spec-coverage` can consume a `routes.json` from *any* producer (a future `hs-server`
//! binary, a hand-written fixture, a differently-shaped tool in another language) without a
//! compile-time dependency on one particular router implementation, matching the ownership split
//! in `docs/workstreams/README.md` ("each track owns its crates ... cross-track changes are
//! interface RFCs"). [`tests/routes_json_schema.rs`] checks this module's `Deserialize` against
//! RFC 0005's own example JSON, so drift between the documented format and this parser is caught.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CoverageError;

/// One row of `routes.json` (RFC 0005 section 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// Upper-case HTTP method.
    pub method: String,
    /// Axum path syntax (`{param}`), which is also OpenAPI 3.1's — directly comparable to
    /// [`crate::spec::SpecRoute::path`].
    pub path: String,
    /// Open enum: `matrix-client`, `matrix-federation`, `matrix-appservice`,
    /// `synapse-admin-compat`, `admin`, or (this crate's addition, see
    /// [`crate::spec::ApiFamily::manifest_surface`]) `matrix-identity` / `matrix-push-gateway`.
    pub surface: String,
    /// The `<resource>.<verb>` id for `admin` routes; absent for everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Open enum: `none`, `matrix`, `admin`, `appservice`.
    #[serde(default = "default_auth")]
    pub auth: String,
    /// The required OAuth scope for `auth: "admin"` routes; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_scope: Option<String>,
    /// Whether the route is subject to rate limiting.
    #[serde(default)]
    pub rate_limited: bool,
}

fn default_auth() -> String {
    "none".to_string()
}

/// The full `routes.json` document (RFC 0005 section 2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteManifest {
    /// When the manifest was generated; not meaningful across builds (RFC 0005: "consumers diff
    /// `routes`, not this field").
    #[serde(default)]
    pub generated_at: Option<String>,
    /// One entry per registered `(method, path)`.
    #[serde(default)]
    pub routes: Vec<Route>,
}

impl RouteManifest {
    /// An empty manifest: every spec route is reported missing, nothing is reported extra. Used
    /// when no `routes.json` was supplied — the honest answer on a day when nothing has mounted
    /// the Matrix surfaces yet, rather than refusing to run.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads and parses a `routes.json` file.
    ///
    /// # Errors
    /// Returns [`CoverageError::Io`] if the file cannot be read, or [`CoverageError::Json`] if it
    /// is not a valid `routes.json` document.
    pub fn load(path: &Path) -> Result<Self, CoverageError> {
        let text = fs::read_to_string(path).map_err(|source| CoverageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| CoverageError::Json {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The `(METHOD, path)` pairs registered under `surface`, for diffing against a spec family's
    /// routes ([`crate::spec::ApiFamily::manifest_surface`]).
    #[must_use]
    pub fn method_paths_for(&self, surface: &str) -> BTreeSet<(String, String)> {
        self.routes
            .iter()
            .filter(|r| r.surface == surface)
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal example from `docs/rfcs/0005-routes-json-manifest.md` section 2, verbatim.
    /// If this stops parsing, this module's schema has drifted from the documented contract.
    const RFC_0005_EXAMPLE: &str = r#"
{
  "generated_at": "2026-09-18T00:00:00.000Z",
  "routes": [
    {
      "method": "GET",
      "path": "/api/v1/users",
      "surface": "admin",
      "operation_id": "users.list",
      "auth": "admin",
      "required_scope": "admin:read",
      "rate_limited": true
    },
    {
      "method": "POST",
      "path": "/_matrix/client/v3/rooms/{roomId}/join",
      "surface": "matrix-client",
      "operation_id": null,
      "auth": "matrix",
      "required_scope": null,
      "rate_limited": true
    }
  ]
}
"#;

    #[test]
    fn parses_the_rfc_0005_example_verbatim() {
        let manifest: RouteManifest = serde_json::from_str(RFC_0005_EXAMPLE).unwrap();
        assert_eq!(manifest.routes.len(), 2);
        assert_eq!(
            manifest.routes[0].operation_id.as_deref(),
            Some("users.list")
        );
        assert_eq!(manifest.routes[1].operation_id, None);
        assert_eq!(manifest.routes[1].surface, "matrix-client");
    }

    #[test]
    fn method_paths_for_filters_by_surface() {
        let manifest: RouteManifest = serde_json::from_str(RFC_0005_EXAMPLE).unwrap();
        let client_routes = manifest.method_paths_for("matrix-client");
        assert_eq!(client_routes.len(), 1);
        assert!(client_routes.contains(&(
            "POST".to_string(),
            "/_matrix/client/v3/rooms/{roomId}/join".to_string()
        )));
    }

    #[test]
    fn minimal_manifest_with_only_required_fields_parses() {
        let json =
            r#"{"routes": [{"method": "GET", "path": "/x", "surface": "matrix-federation"}]}"#;
        let manifest: RouteManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.routes[0].auth, "none");
        assert!(!manifest.routes[0].rate_limited);
    }

    #[test]
    fn empty_manifest_has_no_routes() {
        assert!(RouteManifest::empty().routes.is_empty());
    }
}
