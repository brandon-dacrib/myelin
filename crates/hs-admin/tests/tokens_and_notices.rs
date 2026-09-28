//! The `registration_tokens.*` and `server_notices.*` operations through the real router: scopes,
//! validation, idempotency, and the audit entry and event every mutation writes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hs_admin::audit::{AuditFilter, AuditSink, InMemoryAuditSink};
use hs_admin::auth::StaticVerifier;
use hs_admin::events::EventBus;
use hs_admin::model::{Principal, PrincipalKind, Scope};
use hs_admin::registration_tokens::InMemoryRegistrationTokens;
use hs_admin::router::{AdminState, build_router};
use hs_admin::server_notices::InMemoryServerNotices;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn principal(scopes: Vec<Scope>) -> Principal {
    Principal {
        kind: PrincipalKind::User,
        id: "@ops:example.org".into(),
        display_name: None,
        scopes,
        token_id: None,
        expires_at: None,
        issued_by: None,
    }
}

struct Harness {
    router: axum::Router,
    audit: Arc<InMemoryAuditSink>,
    events: Arc<EventBus>,
    tokens: Arc<InMemoryRegistrationTokens>,
}

fn harness() -> Harness {
    let verifier = StaticVerifier::new()
        .with_token("admin", principal(vec![Scope::AdminWrite]))
        .with_token("reader", principal(vec![Scope::AdminRead]))
        .with_token("moderator", principal(vec![Scope::ModerationWrite]));
    let audit = Arc::new(InMemoryAuditSink::new());
    let events = Arc::new(EventBus::new());
    let tokens = Arc::new(InMemoryRegistrationTokens::new());
    let state = AdminState::new(Arc::new(verifier), audit.clone(), events.clone())
        .with_registration_tokens(tokens.clone())
        .with_server_notices(Arc::new(InMemoryServerNotices::new(
            "@_server:example.org",
            vec!["@alice:example.org".into(), "@bob:example.org".into()],
        )));
    let (router, _manifest) = build_router(state);
    Harness {
        router,
        audit,
        events,
        tokens,
    }
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
async fn a_created_token_is_listed_read_changed_and_deleted_and_each_change_is_audited() {
    let h = harness();
    let mut events = h.events.subscribe();

    let (status, created) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "admin",
        Some(json!({"token": "invite-1", "uses_allowed": 2})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["token"], "invite-1");
    assert_eq!(created["uses_allowed"], 2);
    assert_eq!(created["pending"], 0);
    assert_eq!(created["completed"], 0);
    assert_eq!(created["valid"], true);
    assert_eq!(created["expires_at"], Value::Null);
    assert!(created["created_at"].as_str().is_some());
    let event = events.recv().await.unwrap();
    assert_eq!(event.r#type, "registration_token.created");

    let (status, page) = call(
        &h.router,
        "GET",
        "/api/v1/registration-tokens?include_total=true",
        "reader",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["token"], "invite-1");

    // Used up: two registrations finished.
    h.tokens.set_counts("invite-1", 0, 2);
    let (_, got) = call(
        &h.router,
        "GET",
        "/api/v1/registration-tokens/invite-1",
        "reader",
        None,
        None,
    )
    .await;
    assert_eq!(got["valid"], false);

    let (status, updated) = call(
        &h.router,
        "PATCH",
        "/api/v1/registration-tokens/invite-1",
        "admin",
        Some(json!({"uses_allowed": null, "expires_at": "2999-01-01T00:00:00Z"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["uses_allowed"], Value::Null);
    assert_eq!(updated["expires_at"], "2999-01-01T00:00:00.000Z");
    assert_eq!(updated["valid"], true);

    let (status, _) = call(
        &h.router,
        "DELETE",
        "/api/v1/registration-tokens/invite-1",
        "admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &h.router,
        "GET",
        "/api/v1/registration-tokens/invite-1",
        "reader",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let entries = h.audit.query(&AuditFilter::default()).await.unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    for action in [
        "registration_tokens.create",
        "registration_tokens.update",
        "registration_tokens.delete",
    ] {
        assert!(actions.contains(&action), "{actions:?}");
    }
    let update = entries
        .iter()
        .find(|e| e.action == "registration_tokens.update")
        .unwrap();
    assert_eq!(update.changes.len(), 2);
    // The audit entry records the status the client was answered with: a creation is `201`.
    let create = entries
        .iter()
        .find(|e| e.action == "registration_tokens.create")
        .unwrap();
    assert_eq!(create.outcome.status, 201);
    assert_eq!(update.outcome.status, 200);
}

#[tokio::test]
async fn a_generated_token_has_the_requested_length_and_a_duplicate_is_a_conflict() {
    let h = harness();
    let (status, created) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "admin",
        Some(json!({"length": 24})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["token"].as_str().unwrap().len(), 24);

    let (status, _) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "admin",
        Some(json!({"token": created["token"]})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_empty_body_creates_a_generated_token_and_a_bad_one_names_the_field() {
    let h = harness();
    let (status, created) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["token"].as_str().unwrap().len(), 16);

    let (status, problem) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "admin",
        Some(json!({"token": "white space"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["errors"][0]["pointer"], "/token", "{problem}");
}

#[tokio::test]
async fn a_retried_create_with_the_same_key_makes_one_token() {
    let h = harness();
    let body = json!({"uses_allowed": 1});
    let (_, first) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "admin",
        Some(body.clone()),
        Some("k1"),
    )
    .await;
    let (_, second) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "admin",
        Some(body),
        Some("k1"),
    )
    .await;
    assert_eq!(first["token"], second["token"]);
    let (_, page) = call(
        &h.router,
        "GET",
        "/api/v1/registration-tokens",
        "reader",
        None,
        None,
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn token_writes_need_admin_write_and_reads_need_admin_read() {
    let h = harness();
    let (status, _) = call(
        &h.router,
        "POST",
        "/api/v1/registration-tokens",
        "reader",
        Some(json!({})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &h.router,
        "GET",
        "/api/v1/registration-tokens",
        "moderator",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &h.router,
        "PATCH",
        "/api/v1/registration-tokens/nope",
        "admin",
        Some(json!({"uses_allowed": 1})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_sent_notice_is_listed_audited_and_idempotent() {
    let h = harness();
    let body = json!({
        "recipients": ["@alice:example.org", "@bob:example.org"],
        "content": {"msgtype": "m.text", "body": "maintenance tonight"}
    });
    let (status, sent) = call(
        &h.router,
        "POST",
        "/api/v1/server-notices",
        "moderator",
        Some(body.clone()),
        Some("txn-1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    assert_eq!(sent["sender"], "@_server:example.org");
    assert_eq!(sent["type"], "m.room.message");
    assert_eq!(sent["event_ids"].as_array().unwrap().len(), 2);
    assert_eq!(sent["room_ids"].as_array().unwrap().len(), 2);

    let (_, again) = call(
        &h.router,
        "POST",
        "/api/v1/server-notices",
        "moderator",
        Some(body),
        Some("txn-1"),
    )
    .await;
    assert_eq!(again["event_ids"], sent["event_ids"]);

    let (status, page) = call(
        &h.router,
        "GET",
        "/api/v1/server-notices",
        "moderator",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["content"]["body"], "maintenance tonight");

    let entries = h.audit.query(&AuditFilter::default()).await.unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.action == "server_notices.send")
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .find(|e| e.action == "server_notices.send")
            .map(|e| e.outcome.status),
        Some(201)
    );
}

#[tokio::test]
async fn a_notice_to_an_unknown_user_sends_nothing_and_needs_moderation_write() {
    let h = harness();
    let (status, problem) = call(
        &h.router,
        "POST",
        "/api/v1/server-notices",
        "moderator",
        Some(json!({
            "recipients": ["@alice:example.org", "@nobody:example.org"],
            "content": {"msgtype": "m.text", "body": "hi"}
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    let (_, page) = call(
        &h.router,
        "GET",
        "/api/v1/server-notices",
        "moderator",
        None,
        None,
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 0);

    let (status, _) = call(
        &h.router,
        "POST",
        "/api/v1/server-notices",
        "reader",
        Some(json!({"recipients": ["@alice:example.org"], "content": {"body": "x"}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
