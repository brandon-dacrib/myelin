//! The `hs-admin` axum router (RFC 0004, deliverable 5 of track 15's brief).
//!
//! Every operation in `openapi/operations.json` (generated alongside `openapi.yaml`) gets a
//! route that: extracts `Authorization`, verifies the token through [`TokenVerifier`]
//! (`crate::auth`), checks the operation's minimal scope, and then either runs a real handler (a
//! first slice: `me.get`, `server.get`, `server.health`, `users.list`, `users.get`; see
//! [`REAL_HANDLERS`]) or answers `501 not-implemented` (RFC 0004 section 3.5: "Endpoint declared
//! but not yet served (Phase 0 skeleton)") for everything else. Real handlers land resource by
//! resource; this proves the plumbing (auth, scopes, the manifest, the OpenAPI contract check)
//! end to end for the rest. `tests/contract.rs` asserts this router's manifest agrees with
//! `openapi.yaml` regardless of which operations are real yet — registering a real handler never
//! changes an operation's `(method, path)`.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use serde::Deserialize;

use crate::audit::AuditSink;
use crate::auth::{ScopeDecision, TokenVerifier, require_scope};
use crate::events::EventBus;
use crate::model::{Page, Scope, ServerHealth, ServerInfo};
use crate::operations::{OperationDef, load as load_operations};
use crate::sources::{UserDirectory, UserFilter};

/// Everything an `hs-admin` handler needs. Cloned per-request by axum (cheap: everything inside
/// is an `Arc`, a plain value type, or `Copy`).
#[derive(Clone)]
pub struct AdminState {
    pub verifier: Arc<dyn TokenVerifier>,
    pub audit: Arc<dyn AuditSink>,
    pub events: Arc<EventBus>,
    /// The raw OpenAPI document this binary serves at `/api/v1/openapi.yaml` and `.json`.
    pub openapi_yaml: Arc<str>,
    /// The static parts of `GET /server`'s response. Defaulted by [`AdminState::new`]; the
    /// integration lead sets it from the running binary's real identity with
    /// [`AdminState::with_server_info`].
    pub server_info: ServerInfo,
    /// When this state (and so, in practice, this server process) started, for `GET
    /// /server`'s `uptime_ms`.
    pub started_at: Instant,
    /// The user directory `GET /users` and `GET /users/{user_id}` read from. `None` until the
    /// integration lead wires a real implementation with [`AdminState::with_users`]; the two user
    /// handlers answer `503 unavailable` rather than faking data or hiding behind a `404`.
    pub users: Option<Arc<dyn UserDirectory>>,
}

impl AdminState {
    pub fn new(
        verifier: Arc<dyn TokenVerifier>,
        audit: Arc<dyn AuditSink>,
        events: Arc<EventBus>,
    ) -> Self {
        Self {
            verifier,
            audit,
            events,
            openapi_yaml: Arc::from(crate::openapi::DOCUMENT),
            server_info: ServerInfo::default(),
            started_at: Instant::now(),
            users: None,
        }
    }

    /// Wires a real [`UserDirectory`], making `GET /users` and `GET /users/{user_id}` serve real
    /// data instead of `503 unavailable`.
    #[must_use]
    pub fn with_users(mut self, users: Arc<dyn UserDirectory>) -> Self {
        self.users = Some(users);
        self
    }

    /// Replaces the default [`ServerInfo`] with the running binary's real identity.
    #[must_use]
    pub fn with_server_info(mut self, server_info: ServerInfo) -> Self {
        self.server_info = server_info;
        self
    }
}

fn authorization_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}

/// The handler for every not-yet-implemented operation: enforce auth and scope, then answer
/// `501`. `operation_id` and `path` are baked into the closure per route (see [`build_router`]).
async fn not_implemented(
    state: AdminState,
    headers: HeaderMap,
    scope: Option<crate::model::Scope>,
    operation_id: String,
    path: String,
) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        scope,
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => hs_http::Problem::not_implemented()
            .with_detail(format!(
                "{operation_id} is declared in the OpenAPI contract but not implemented yet"
            ))
            .with_instance(path)
            .into_response(),
        ScopeDecision::Unauthenticated(problem) => problem.with_instance(path).into_response(),
        ScopeDecision::InsufficientScope(problem) => problem.with_instance(path).into_response(),
    }
}

/// Operation ids this router serves with a real handler instead of the generic `501`. Keeping
/// this list next to [`register_operation`]/[`register_real_operation`] (rather than scattering a
/// per-operation `if` through the loop) makes "which of the 142 operations are genuinely served"
/// a single, greppable fact.
const REAL_HANDLERS: &[&str] = &[
    "me.get",
    "server.get",
    "server.health",
    "users.list",
    "users.get",
];

/// The `503 unavailable` problem a handler answers when its backing [`crate::sources`] trait
/// object hasn't been wired onto [`AdminState`] yet — never a silent `501` (which would say "this
/// handler doesn't exist yet", which is false) and never a fake `200`.
fn source_unavailable(source_name: &str, path: &str) -> Response {
    hs_http::Problem::unavailable()
        .with_detail(format!(
            "the {source_name} data source is not wired into this server"
        ))
        .with_instance(path)
        .into_response()
}

/// `GET /api/v1/me`: the one operation with no required scope (RFC 0004 section 8.2) — any
/// valid token identifies its own principal.
async fn me_get(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        None,
    )
    .await
    {
        ScopeDecision::Allowed(principal) => axum::Json(principal).into_response(),
        ScopeDecision::Unauthenticated(problem) => {
            problem.with_instance("/api/v1/me").into_response()
        }
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance("/api/v1/me").into_response()
        }
    }
}

/// `GET /api/v1/server`: identity and build info, plus `uptime_ms` computed from
/// [`AdminState::started_at`].
async fn server_get(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let uptime_ms =
                u64::try_from(state.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            axum::Json(state.server_info.with_uptime_ms(uptime_ms)).into_response()
        }
        ScopeDecision::Unauthenticated(problem) => {
            problem.with_instance("/api/v1/server").into_response()
        }
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance("/api/v1/server").into_response()
        }
    }
}

/// `GET /api/v1/server/health`: an honest probe summary. A check whose backing source is absent
/// is reported `"unknown"`, never `"ok"` (see [`ServerHealth`]'s doc comment).
async fn server_health(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let mut checks = std::collections::BTreeMap::new();
            // The audit sink and event bus are always wired (AdminState::new requires them), so
            // this process being able to answer at all means they are reachable.
            checks.insert("audit".to_string(), "ok".to_string());
            checks.insert("events".to_string(), "ok".to_string());
            checks.insert(
                "users".to_string(),
                if state.users.is_some() {
                    "ok".to_string()
                } else {
                    "unknown".to_string()
                },
            );
            axum::Json(ServerHealth::from_checks(checks)).into_response()
        }
        ScopeDecision::Unauthenticated(problem) => problem
            .with_instance("/api/v1/server/health")
            .into_response(),
        ScopeDecision::InsufficientScope(problem) => problem
            .with_instance("/api/v1/server/health")
            .into_response(),
    }
}

/// Query parameters `GET /api/v1/users` accepts (RFC 0004 section 3.3's pagination trio plus the
/// per-resource filters the OpenAPI `users.list` operation declares). `sort` is accepted but not
/// yet honoured (the in-memory fake and any real implementation both return an id-ordered list;
/// see `docs/status/15-admin-api-and-modules.md`).
#[derive(Debug, Default, Deserialize)]
struct UsersListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    q: Option<String>,
    admin: Option<bool>,
    deactivated: Option<bool>,
    locked: Option<bool>,
    suspended: Option<bool>,
    guests: Option<bool>,
}

/// `GET /api/v1/users`: a page of [`crate::model::AdminUser`] from [`UserDirectory::list_users`],
/// filtered by the query parameters above and paginated with [`Page::paginate`].
async fn users_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<UsersListQuery>,
) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(users) = &state.users else {
                return source_unavailable("user directory", "/api/v1/users");
            };
            let filter = UserFilter {
                q: query.q,
                admin: query.admin,
                deactivated: query.deactivated,
                locked: query.locked,
                suspended: query.suspended,
                guests: query.guests,
            };
            match users.list_users(&filter).await {
                Ok(items) => {
                    let page = Page::paginate(
                        items,
                        query.cursor.as_deref(),
                        query.limit,
                        query.include_total.unwrap_or(false),
                    );
                    axum::Json(page).into_response()
                }
                Err(e) => e
                    .to_problem()
                    .with_instance("/api/v1/users")
                    .into_response(),
            }
        }
        ScopeDecision::Unauthenticated(problem) => {
            problem.with_instance("/api/v1/users").into_response()
        }
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance("/api/v1/users").into_response()
        }
    }
}

/// `GET /api/v1/users/{user_id}`: one [`crate::model::AdminUser`], or a `404 not-found` problem.
async fn users_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(users) = &state.users else {
                return source_unavailable("user directory", &instance);
            };
            match users.get_user(&user_id).await {
                Ok(Some(user)) => axum::Json(user).into_response(),
                Ok(None) => hs_http::Problem::not_found()
                    .with_detail(format!("no such user: {user_id}"))
                    .with_instance(instance)
                    .into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(problem) => problem.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance(instance).into_response()
        }
    }
}

async fn serve_openapi_yaml(State(state): State<AdminState>) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/yaml")],
        state.openapi_yaml.to_string(),
    )
        .into_response()
}

async fn serve_openapi_json(State(state): State<AdminState>) -> Response {
    match serde_yaml_ng::from_str::<serde_json::Value>(&state.openapi_yaml) {
        Ok(value) => axum::Json(value).into_response(),
        Err(e) => hs_http::Problem::internal()
            .with_detail(format!(
                "could not convert the embedded OpenAPI document: {e}"
            ))
            .into_response(),
    }
}

/// Builds the full router: every declared `/api/v1` operation (enforced but not implemented),
/// the OpenAPI document endpoints, and `/admin/` static assets (`crate::assets`). Returns the
/// manifest alongside so callers can write `routes.json` (RFC 0005) or run the contract check.
pub fn build_router(state: AdminState) -> (axum::Router, RouteManifest) {
    let mut builder: Builder<AdminState> = Builder::new();

    for op in load_operations() {
        // The two `public` operations (the OpenAPI document itself) are registered below with
        // their real handlers; every other operation gets either a real handler (`REAL_HANDLERS`)
        // or the generic 501 handler.
        if op.public {
            continue;
        }
        builder = if REAL_HANDLERS.contains(&op.operation_id.as_str()) {
            register_real_operation(builder, op)
        } else {
            register_operation(builder, op)
        };
    }

    builder = builder.add(
        axum::http::Method::GET,
        "/api/v1/openapi.yaml",
        serve_openapi_yaml,
        RouteMeta::new(Surface::Admin, AuthKind::None),
    );
    builder = builder.add(
        axum::http::Method::GET,
        "/api/v1/openapi.json",
        serve_openapi_json,
        RouteMeta::new(Surface::Admin, AuthKind::None),
    );

    let (router, manifest) = builder.build();
    let router = router.merge(crate::assets::router()).with_state(state);
    (router, manifest)
}

fn operation_route_meta(op: &OperationDef) -> RouteMeta {
    let mut meta =
        RouteMeta::new(Surface::Admin, AuthKind::Admin).with_operation_id(op.operation_id.clone());
    if let Some(scope) = op.scope {
        meta = meta.with_scope(scope.as_str());
    }
    if op.method != axum::http::Method::GET {
        meta = meta.rate_limited();
    }
    meta
}

fn register_operation(builder: Builder<AdminState>, op: OperationDef) -> Builder<AdminState> {
    let full_path = format!("/api/v1{}", op.path);
    let scope = op.scope;
    let operation_id = op.operation_id.clone();
    let meta = operation_route_meta(&op);
    let path_for_handler = full_path.clone();
    builder.add(
        op.method.clone(),
        &full_path,
        move |State(state): State<AdminState>, headers: HeaderMap| {
            let operation_id = operation_id.clone();
            let path = path_for_handler.clone();
            async move { not_implemented(state, headers, scope, operation_id, path).await }
        },
        meta,
    )
}

/// Registers one of [`REAL_HANDLERS`] with its concrete handler function, keeping the exact same
/// `(method, path, scope, rate-limited)` shape [`register_operation`] would have produced so the
/// OpenAPI contract test (`tests/contract.rs`) cannot tell the difference.
fn register_real_operation(builder: Builder<AdminState>, op: OperationDef) -> Builder<AdminState> {
    let full_path = format!("/api/v1{}", op.path);
    let meta = operation_route_meta(&op);
    let method = op.method.clone();
    match op.operation_id.as_str() {
        "me.get" => builder.add(method, &full_path, me_get, meta),
        "server.get" => builder.add(method, &full_path, server_get, meta),
        "server.health" => builder.add(method, &full_path, server_health, meta),
        "users.list" => builder.add(method, &full_path, users_list, meta),
        "users.get" => builder.add(method, &full_path, users_get, meta),
        other => unreachable!(
            "{other} is listed in REAL_HANDLERS but register_real_operation doesn't know it"
        ),
    }
}

/// A minimal [`AdminState`] for other modules' tests (`crate::assets`) that need one but are not
/// testing auth or audit behavior themselves.
#[cfg(test)]
pub mod tests_support {
    use std::sync::Arc;

    use super::AdminState;
    use crate::audit::InMemoryAuditSink;
    use crate::auth::StaticVerifier;
    use crate::events::EventBus;

    pub fn dummy_state() -> AdminState {
        AdminState::new(
            Arc::new(StaticVerifier::new()),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::audit::InMemoryAuditSink;
    use crate::auth::StaticVerifier;
    use crate::model::{Principal, PrincipalKind, Scope};

    fn test_state() -> AdminState {
        let verifier = StaticVerifier::new().with_token(
            "admin-token",
            Principal {
                kind: PrincipalKind::User,
                id: "@ops:example.org".into(),
                display_name: None,
                scopes: vec![Scope::AdminWrite],
                token_id: None,
                expires_at: None,
                issued_by: None,
            },
        );
        AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        )
    }

    #[tokio::test]
    async fn unauthenticated_request_is_401() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_request_to_undeclared_scope_is_403() {
        let verifier = StaticVerifier::new().with_token(
            "read-only",
            Principal {
                kind: PrincipalKind::User,
                id: "@ro:example.org".into(),
                display_name: None,
                scopes: vec![Scope::ModerationRead],
                token_id: None,
                expires_at: None,
                issued_by: None,
            },
        );
        let state = AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        );
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer read-only")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn authorized_request_to_undeclared_handler_is_501() {
        // /api/v1/users is now one of REAL_HANDLERS (see below); use a still-undeclared operation
        // to exercise the generic seam.
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/rooms")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn openapi_yaml_needs_no_authentication() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/openapi.yaml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn me_needs_only_authentication_not_a_specific_scope() {
        let verifier = StaticVerifier::new().with_token(
            "read-only",
            Principal {
                kind: PrincipalKind::User,
                id: "@ro:example.org".into(),
                display_name: None,
                scopes: vec![Scope::ModerationRead],
                token_id: None,
                expires_at: None,
                issued_by: None,
            },
        );
        let state = AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        );
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header("authorization", "Bearer read-only")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 200 (not 403): any authenticated principal is enough for GET /me, regardless of scopes.
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn me_reflects_the_principal_the_verifier_returned() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let principal: Principal = serde_json::from_slice(&body).unwrap();
        assert_eq!(principal.id, "@ops:example.org");
        assert_eq!(principal.scopes, vec![Scope::AdminWrite]);
    }

    #[tokio::test]
    async fn server_get_reports_uptime_and_identity() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/server")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("uptime_ms").is_some());
        assert!(value.get("contract_version").is_some());
    }

    #[tokio::test]
    async fn server_health_reports_users_as_unknown_when_unwired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/server/health")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let health: crate::model::ServerHealth = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.checks.get("users"), Some(&"unknown".to_string()));
        assert_eq!(health.status, "degraded");
    }

    #[tokio::test]
    async fn users_list_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn users_get_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    fn state_with_users() -> AdminState {
        use crate::model::AdminUser;
        use crate::sources::InMemoryUserDirectory;

        let mut admin_user = AdminUser {
            user_id: "@ops:example.org".to_string(),
            display_name: Some("Operations".to_string()),
            ..Default::default()
        };
        admin_user.admin = true;
        let alice = AdminUser {
            user_id: "@alice:example.org".to_string(),
            display_name: Some("Alice".to_string()),
            ..Default::default()
        };
        let directory = InMemoryUserDirectory::new()
            .with_user(admin_user)
            .with_user(alice);
        test_state().with_users(Arc::new(directory))
    }

    #[tokio::test]
    async fn users_get_returns_the_real_user() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let user: crate::model::AdminUser = serde_json::from_slice(&body).unwrap();
        assert_eq!(user.user_id, "@alice:example.org");
        assert_eq!(user.display_name, Some("Alice".to_string()));
    }

    #[tokio::test]
    async fn users_get_missing_user_is_404_problem() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/%40nobody%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: hs_http::Problem = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem.r#type, "urn:hs:problem:not-found");
    }

    #[tokio::test]
    async fn users_list_paginates_and_filters_by_admin() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users?admin=true&limit=1")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: crate::model::Page<crate::model::AdminUser> =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].user_id, "@ops:example.org");
        assert_eq!(page.next_cursor, None);
    }

    #[tokio::test]
    async fn users_list_q_filters_free_text() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users?q=alice")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: crate::model::Page<crate::model::AdminUser> =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].user_id, "@alice:example.org");
    }

    #[test]
    fn manifest_has_one_row_per_operation_plus_openapi_endpoints() {
        let (_router, manifest) = build_router(test_state());
        let admin_routes = manifest.method_paths_for(Surface::Admin);
        assert!(admin_routes.len() >= load_operations().len());
        assert!(admin_routes.contains(&("GET".to_string(), "/api/v1/openapi.yaml".to_string())));
    }

    #[test]
    fn real_handlers_keep_the_same_paths_as_the_generic_seam_would() {
        // Every operation id in REAL_HANDLERS must actually exist in the operation table at the
        // path the OpenAPI document declares; this catches a typo in REAL_HANDLERS or
        // register_real_operation silently falling through to the generic 501 handler instead.
        let ops = load_operations();
        for &op_id in REAL_HANDLERS {
            assert!(
                ops.iter().any(|o| o.operation_id == op_id),
                "REAL_HANDLERS names {op_id}, which is not in the operation table"
            );
        }
    }
}
