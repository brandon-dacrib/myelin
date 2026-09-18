//! Router construction that emits a `routes.json` manifest as a side effect of building the
//! `axum::Router`, so the manifest can never drift from what is actually served. See
//! `docs/rfcs/0005-routes-json-manifest.md` for the format and rationale.

use std::collections::BTreeMap;

use axum::http::Method;
use axum::routing::{MethodFilter, MethodRouter};
use serde::{Deserialize, Serialize};

/// Which family of routes an entry belongs to. Open enum: consumers must tolerate values not
/// listed here yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Surface {
    MatrixClient,
    MatrixFederation,
    MatrixAppservice,
    SynapseAdminCompat,
    Admin,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MatrixClient => "matrix-client",
            Self::MatrixFederation => "matrix-federation",
            Self::MatrixAppservice => "matrix-appservice",
            Self::SynapseAdminCompat => "synapse-admin-compat",
            Self::Admin => "admin",
        }
    }
}

/// How a route authenticates its caller. Open enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    None,
    Matrix,
    Admin,
    Appservice,
}

/// One row of `routes.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub method: String,
    pub path: String,
    pub surface: Surface,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub auth: AuthKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_scope: Option<String>,
    pub rate_limited: bool,
}

/// Metadata attached to one route registration; everything [`Route`] needs beyond method and
/// path, which [`Builder::add`] already takes as separate arguments.
#[derive(Debug, Clone, Default)]
pub struct RouteMeta {
    pub surface: Option<Surface>,
    pub operation_id: Option<String>,
    pub auth: Option<AuthKind>,
    pub required_scope: Option<String>,
    pub rate_limited: bool,
}

impl RouteMeta {
    pub fn new(surface: Surface, auth: AuthKind) -> Self {
        Self {
            surface: Some(surface),
            auth: Some(auth),
            operation_id: None,
            required_scope: None,
            rate_limited: false,
        }
    }

    pub fn with_operation_id(mut self, id: impl Into<String>) -> Self {
        self.operation_id = Some(id.into());
        self
    }

    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.required_scope = Some(scope.into());
        self
    }

    pub fn rate_limited(mut self) -> Self {
        self.rate_limited = true;
        self
    }
}

/// The manifest written to `routes.json` (RFC 0005).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteManifest {
    pub generated_at: String,
    pub routes: Vec<Route>,
}

impl RouteManifest {
    /// Pretty JSON, routes sorted by path then method for stable diffs.
    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        let mut sorted = self.clone();
        sorted
            .routes
            .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
        serde_json::to_string_pretty(&sorted)
    }

    pub fn write_to_file(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let json = self.to_json_pretty().map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// The `(METHOD, path)` pairs for one surface, for contract testing.
    pub fn method_paths_for(
        &self,
        surface: Surface,
    ) -> std::collections::BTreeSet<(String, String)> {
        self.routes
            .iter()
            .filter(|r| r.surface == surface)
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect()
    }
}

fn method_filter(method: &Method) -> MethodFilter {
    match method.as_str() {
        "GET" => MethodFilter::GET,
        "POST" => MethodFilter::POST,
        "PUT" => MethodFilter::PUT,
        "PATCH" => MethodFilter::PATCH,
        "DELETE" => MethodFilter::DELETE,
        "HEAD" => MethodFilter::HEAD,
        "OPTIONS" => MethodFilter::OPTIONS,
        "TRACE" => MethodFilter::TRACE,
        _ => MethodFilter::GET,
    }
}

/// Builds an `axum::Router<S>` one route at a time, recording a [`Route`] for each so the final
/// [`RouteManifest`] can never diverge from what was actually registered.
pub struct Builder<S = ()> {
    routers: BTreeMap<String, MethodRouter<S>>,
    nested: Vec<(String, axum::Router<S>)>,
    routes: Vec<Route>,
}

impl<S> Default for Builder<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Builder<S>
where
    S: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            routers: BTreeMap::new(),
            nested: Vec::new(),
            routes: Vec::new(),
        }
    }

    /// Registers one `(method, path)` with its handler and metadata.
    pub fn add<H, T>(mut self, method: Method, path: &str, handler: H, meta: RouteMeta) -> Self
    where
        H: axum::handler::Handler<T, S>,
        T: 'static,
    {
        let entry = self.routers.remove(path).unwrap_or_default();
        let merged = entry.on(method_filter(&method), handler);
        self.routers.insert(path.to_string(), merged);
        self.routes.push(Route {
            method: method.as_str().to_string(),
            path: path.to_string(),
            surface: meta.surface.unwrap_or(Surface::Admin),
            operation_id: meta.operation_id,
            auth: meta.auth.unwrap_or(AuthKind::None),
            required_scope: meta.required_scope,
            rate_limited: meta.rate_limited,
        });
        self
    }

    pub fn get<H, T>(self, path: &str, handler: H, meta: RouteMeta) -> Self
    where
        H: axum::handler::Handler<T, S>,
        T: 'static,
    {
        self.add(Method::GET, path, handler, meta)
    }

    pub fn post<H, T>(self, path: &str, handler: H, meta: RouteMeta) -> Self
    where
        H: axum::handler::Handler<T, S>,
        T: 'static,
    {
        self.add(Method::POST, path, handler, meta)
    }

    pub fn put<H, T>(self, path: &str, handler: H, meta: RouteMeta) -> Self
    where
        H: axum::handler::Handler<T, S>,
        T: 'static,
    {
        self.add(Method::PUT, path, handler, meta)
    }

    pub fn patch<H, T>(self, path: &str, handler: H, meta: RouteMeta) -> Self
    where
        H: axum::handler::Handler<T, S>,
        T: 'static,
    {
        self.add(Method::PATCH, path, handler, meta)
    }

    pub fn delete<H, T>(self, path: &str, handler: H, meta: RouteMeta) -> Self
    where
        H: axum::handler::Handler<T, S>,
        T: 'static,
    {
        self.add(Method::DELETE, path, handler, meta)
    }

    /// Merges in routes already built elsewhere (for example another `Builder`'s output), for
    /// composing the client, federation and admin listeners' routers out of sub-routers while
    /// keeping one manifest.
    pub fn merge_router(
        mut self,
        prefix: &str,
        router: axum::Router<S>,
        routes: Vec<Route>,
    ) -> Self {
        let full_prefix = prefix.trim_end_matches('/');
        // axum::Router nesting requires the prefix not collide with an existing path; callers
        // are responsible for choosing disjoint prefixes.
        self.routes.extend(routes.into_iter().map(|mut r| {
            r.path = format!("{full_prefix}{}", r.path);
            r
        }));
        self.nested.push((full_prefix.to_string(), router));
        self
    }

    pub fn build(self) -> (axum::Router<S>, RouteManifest) {
        let mut router = axum::Router::new();
        for (path, mr) in self.routers {
            router = router.route(&path, mr);
        }
        for (prefix, nested) in self.nested {
            router = router.nest(&prefix, nested);
        }
        let manifest = RouteManifest {
            generated_at: crate::time::now_rfc3339(),
            routes: self.routes,
        };
        (router, manifest)
    }
}

/// Parses an OpenAPI 3.1 YAML document's `paths` and returns the `(METHOD, path)` pairs it
/// declares, for comparing against [`RouteManifest::method_paths_for`].
pub fn openapi_method_paths(
    openapi_yaml: &str,
) -> Result<std::collections::BTreeSet<(String, String)>, serde_yaml_ng::Error> {
    const HTTP_METHODS: &[&str] = &[
        "get", "post", "put", "patch", "delete", "head", "options", "trace",
    ];
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(openapi_yaml)?;
    // OpenAPI's `paths` keys are relative to `servers[0].url` (RFC 0004's admin API declares
    // `/api/v1` there); a manifest built by `Builder`, on the other hand, records the path
    // actually mounted on the axum router, which includes that prefix. Prepending it here is
    // what makes the two sets comparable.
    let base_path = doc
        .get("servers")
        .and_then(|s| s.as_sequence())
        .and_then(|s| s.first())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let mut out = std::collections::BTreeSet::new();
    if let Some(paths) = doc.get("paths").and_then(|p| p.as_mapping()) {
        for (path_key, ops) in paths {
            let Some(path) = path_key.as_str() else {
                continue;
            };
            let Some(ops_map) = ops.as_mapping() else {
                continue;
            };
            for (method_key, _op) in ops_map {
                let Some(method) = method_key.as_str() else {
                    continue;
                };
                if HTTP_METHODS.contains(&method) {
                    out.insert((method.to_uppercase(), format!("{base_path}{path}")));
                }
            }
        }
    }
    Ok(out)
}

/// Diffs a manifest's `admin` surface against an OpenAPI document's paths. `Ok(())` if they
/// agree; otherwise a human-readable list of mismatches (missing from one side or the other).
pub fn assert_matches_openapi(
    openapi_yaml: &str,
    manifest: &RouteManifest,
) -> Result<(), Vec<String>> {
    let from_openapi = match openapi_method_paths(openapi_yaml) {
        Ok(set) => set,
        Err(e) => return Err(vec![format!("could not parse OpenAPI document: {e}")]),
    };
    let from_router = manifest.method_paths_for(Surface::Admin);
    let mut problems = Vec::new();
    for missing in from_openapi.difference(&from_router) {
        problems.push(format!(
            "in openapi.yaml but not routed: {} {}",
            missing.0, missing.1
        ));
    }
    for extra in from_router.difference(&from_openapi) {
        problems.push(format!(
            "routed but not in openapi.yaml: {} {}",
            extra.0, extra.1
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}
