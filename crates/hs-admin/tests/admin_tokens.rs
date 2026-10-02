//! The `admin_tokens.*` operations through the real router, with the minted tokens verified by
//! the same [`ScopedTokenVerifier`] `hs serve` mounts: a token minted with `bridges:read` is
//! served `bridges:read` operations, refused `admin:read` ones with the RFC's `403` naming the
//! scope, and refused everything once revoked; the mint is audited with the token's scopes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hs_admin::admin_tokens::{InMemoryAdminTokens, ScopedTokenVerifier, TOKEN_PREFIX};
use hs_admin::audit::{AuditFilter, AuditSink, InMemoryAuditSink};
use hs_admin::auth::StaticVerifier;
use hs_admin::events::EventBus;
use hs_admin::model::{Principal, PrincipalKind, Scope};
use hs_admin::router::{AdminState, build_router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn legacy_admin() -> Principal {
    Principal {
        kind: PrincipalKind::Legacy,
        id: "@ops:example.org".into(),
        display_name: None,
        scopes: vec![Scope::AdminRead, Scope::AdminWrite],
        token_id: None,
        expires_at: None,
        issued_by: None,
    }
}

struct Harness {
    router: axum::Router,
    audit: Arc<InMemoryAuditSink>,
}

fn harness() -> Harness {
    let tokens = Arc::new(InMemoryAdminTokens::new());
    let legacy = StaticVerifier::new().with_token("syt_admin", legacy_admin());
    let verifier = ScopedTokenVerifier::new(tokens.clone(), Arc::new(legacy));
    let audit = Arc::new(InMemoryAuditSink::new());
    let state = AdminState::new(Arc::new(verifier), audit.clone(), Arc::new(EventBus::new()))
        .with_admin_tokens(tokens);
    let (router, _manifest) = build_router(state);
    Harness { router, audit }
}

async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
    idempotency_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
    }
    let request = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn a_narrow_token_is_served_its_scopes_and_refused_the_rest() {
    let h = harness();

    // Mint a bridges:read token with the legacy administrator's credential.
    let (status, created) = call(
        &h.router,
        "POST",
        "/api/v1/admin-tokens",
        "syt_admin",
        Some(json!({"name": "bridge team", "scopes": ["bridges:read"]})),
        Some("mint-1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let secret = created["token"].as_str().expect("the token is shown once");
    assert!(secret.starts_with(TOKEN_PREFIX));
    assert_eq!(created["scopes"], json!(["bridges:read"]));
    assert_eq!(created["created_by"], "@ops:example.org");
    assert_eq!(created["expires_at"], Value::Null);
    let id = created["id"].as_str().unwrap().to_owned();

    // The same Idempotency-Key gives back the same token, not a second one.
    let (status, replayed) = call(
        &h.router,
        "POST",
        "/api/v1/admin-tokens",
        "syt_admin",
        Some(json!({"name": "bridge team", "scopes": ["bridges:read"]})),
        Some("mint-1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(replayed["token"], secret);

    // /me says what the token holds.
    let (status, me) = call(&h.router, "GET", "/api/v1/me", secret, None, None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["kind"], "service_account");
    assert_eq!(me["id"], id);
    assert_eq!(me["display_name"], "bridge team");
    assert_eq!(me["scopes"], json!(["bridges:read"]));
    assert_eq!(me["issued_by"], "admin-tokens");

    // A bridges:read operation: past the scope check (503 here, since no bridge source is
    // wired into this harness; the scope contract test covers every operation's scope).
    let (status, _) = call(&h.router, "GET", "/api/v1/appservices", secret, None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // An admin:read operation: the RFC's 403, naming the scope.
    let (status, problem) = call(&h.router, "GET", "/api/v1/users", secret, None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert_eq!(problem["type"], "urn:hs:problem:insufficient-scope");
    assert_eq!(problem["required_scope"], "admin:read");

    // It cannot list or mint tokens either.
    let (status, problem) =
        call(&h.router, "GET", "/api/v1/admin-tokens", secret, None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(problem["required_scope"], "admin:read");
    let (status, problem) = call(
        &h.router,
        "POST",
        "/api/v1/admin-tokens",
        secret,
        Some(json!({"name": "escalate", "scopes": ["admin:write"]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(problem["required_scope"], "admin:write");

    // The administrator lists it, without the secret, with its scopes.
    let (status, page) = call(
        &h.router,
        "GET",
        "/api/v1/admin-tokens",
        "syt_admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], id);
    assert_eq!(items[0]["scopes"], json!(["bridges:read"]));
    assert!(items[0]["token"].is_null());
    assert!(items[0]["secret_hash"].is_null());
    let (status, one) = call(
        &h.router,
        "GET",
        &format!("/api/v1/admin-tokens/{id}"),
        "syt_admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["name"], "bridge team");

    // The mint is audited with the scopes.
    let entries = h
        .audit
        .query(&AuditFilter {
            action: Some("admin_tokens.create".into()),
            limit: 10,
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.target.r#type, "admin_token");
    assert_eq!(entry.target.id, id);
    assert_eq!(entry.actor.id, "@ops:example.org");
    let scopes_change = entry
        .changes
        .iter()
        .find(|c| c.pointer == "/scopes")
        .expect("the audit entry records the scopes");
    assert_eq!(scopes_change.to, Some(json!(["bridges:read"])));
    let as_json = serde_json::to_string(entry).unwrap();
    assert!(
        !as_json.contains(secret),
        "the audit log never holds the token"
    );

    // Revoke: the next request with it is 401.
    let (status, _) = call(
        &h.router,
        "DELETE",
        &format!("/api/v1/admin-tokens/{id}"),
        "syt_admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&h.router, "GET", "/api/v1/me", secret, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &h.router,
        "DELETE",
        &format!("/api/v1/admin-tokens/{id}"),
        "syt_admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let revocations = h
        .audit
        .query(&AuditFilter {
            action: Some("admin_tokens.delete".into()),
            limit: 10,
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(revocations.len(), 1);
    assert_eq!(
        revocations[0].changes[0].from,
        Some(json!(["bridges:read"]))
    );
}

#[tokio::test]
async fn a_token_minted_without_scopes_is_a_full_administrator() {
    let h = harness();
    let (status, created) = call(
        &h.router,
        "POST",
        "/api/v1/admin-tokens",
        "syt_admin",
        Some(json!({"name": "ci"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["scopes"], json!(["admin:read", "admin:write"]));
    let secret = created["token"].as_str().unwrap();

    // A full token can mint another, narrower one, and is served admin:read operations.
    let (status, narrower) = call(
        &h.router,
        "POST",
        "/api/v1/admin-tokens",
        secret,
        Some(json!({"name": "moderators", "scopes": ["moderation:write"]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{narrower}");
    assert_eq!(narrower["created_by"], created["id"]);
    let (status, page) = call(&h.router, "GET", "/api/v1/admin-tokens", secret, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_bad_mint_names_the_field() {
    let h = harness();
    for (body, pointer) in [
        (json!({}), "/name"),
        (json!({"name": "x", "scopes": []}), "/scopes"),
        (json!({"name": "x", "scopes": ["root"]}), "/scopes"),
        (
            json!({"name": "x", "expires_at": "2000-01-01T00:00:00Z"}),
            "/expires_at",
        ),
    ] {
        let (status, problem) = call(
            &h.router,
            "POST",
            "/api/v1/admin-tokens",
            "syt_admin",
            Some(body.clone()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {problem}");
        assert_eq!(
            problem["errors"][0]["pointer"], pointer,
            "{body}: {problem}"
        );
    }
    let (status, problem) = call(
        &h.router,
        "POST",
        "/api/v1/admin-tokens",
        "syt_admin",
        Some(json!({"name": "x", "unknown": 1})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn an_unminted_admin_shaped_token_is_unauthenticated() {
    let h = harness();
    let (status, problem) = call(
        &h.router,
        "GET",
        "/api/v1/me",
        "hsa_0000000000000000000000000000000000000000",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(problem["type"], "urn:hs:problem:unauthenticated");
}
