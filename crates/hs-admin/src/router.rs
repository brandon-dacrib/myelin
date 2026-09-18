//! The `hs-admin` axum router skeleton (RFC 0004, deliverable 5 of track 15's brief).
//!
//! Every operation in `openapi/operations.json` (generated alongside `openapi.yaml`) gets a
//! route that: extracts `Authorization`, verifies the token through [`TokenVerifier`]
//! (`crate::auth`), checks the operation's minimal scope, and — once authorized — answers
//! `501 not-implemented` (RFC 0004 section 3.5: "Endpoint declared but not yet served (Phase 0
//! skeleton)"). Real handlers land resource by resource in Phase 1; this proves the plumbing
//! (auth, scopes, the manifest, the OpenAPI contract check) end to end before any of them exist.
//! `tests/contract.rs` asserts this router's manifest agrees with `openapi.yaml`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};

use crate::audit::AuditSink;
use crate::auth::{ScopeDecision, TokenVerifier, require_scope};
use crate::events::EventBus;
use crate::operations::{OperationDef, load as load_operations};

/// Everything an `hs-admin` handler needs. Cloned per-request by axum (cheap: everything inside
/// is an `Arc`).
#[derive(Clone)]
pub struct AdminState {
    pub verifier: Arc<dyn TokenVerifier>,
    pub audit: Arc<dyn AuditSink>,
    pub events: Arc<EventBus>,
    /// The raw OpenAPI document this binary serves at `/api/v1/openapi.yaml` and `.json`.
    pub openapi_yaml: Arc<str>,
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
        }
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
        // their real handlers; every other operation gets the generic 501 handler.
        if op.public {
            continue;
        }
        builder = register_operation(builder, op);
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

fn register_operation(builder: Builder<AdminState>, op: OperationDef) -> Builder<AdminState> {
    let full_path = format!("/api/v1{}", op.path);
    let scope = op.scope;
    let operation_id = op.operation_id.clone();
    let mut meta =
        RouteMeta::new(Surface::Admin, AuthKind::Admin).with_operation_id(op.operation_id.clone());
    if let Some(scope) = scope {
        meta = meta.with_scope(scope.as_str());
    }
    if op.method != axum::http::Method::GET {
        meta = meta.rate_limited();
    }
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
        // 501 (not 403): any authenticated principal is enough for GET /me.
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn manifest_has_one_row_per_operation_plus_openapi_endpoints() {
        let (_router, manifest) = build_router(test_state());
        let admin_routes = manifest.method_paths_for(Surface::Admin);
        assert!(admin_routes.len() >= load_operations().len());
        assert!(admin_routes.contains(&("GET".to_string(), "/api/v1/openapi.yaml".to_string())));
    }
}
