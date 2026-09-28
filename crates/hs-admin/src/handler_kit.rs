//! The small amount of plumbing every handler in [`crate::reports`], [`crate::tasks`] and
//! [`crate::statistics`] shares: authenticate and check a scope, honour an `Idempotency-Key`,
//! write the audit entry and publish the event for a mutation, and answer JSON.
//!
//! `crate::router` has its own private copies of the same steps, written inline per handler as
//! the first operations landed. These are the same rules, not new ones (RFC 0004 sections 3.6,
//! 8 and 9); they live in their own module so that areas added later do not grow `router.rs`
//! further, and so that what a mutation must do is written once for them.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use serde::Serialize;

use crate::auth::{ScopeDecision, require_scope};
use crate::idempotency::{Replay, StoredResponse};
use crate::model::{AuditChange, AuditEntry, AuditOutcome, Event, Principal, ResourceRef, Scope};
use crate::router::AdminState;

/// Authenticates the request and checks it holds `scope`, or answers the `401`/`403` problem
/// with `instance` set.
// `Response` is returned once per request; boxing it would only add noise at every call site.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize(
    state: &AdminState,
    headers: &HeaderMap,
    scope: Scope,
    instance: &str,
) -> Result<Principal, Response> {
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match require_scope(state.verifier.as_ref(), authorization, Some(scope)).await {
        ScopeDecision::Allowed(principal) => Ok(principal),
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            Err(p.with_instance(instance.to_owned()).into_response())
        }
    }
}

/// The `503 unavailable` answer for an area whose data source is not wired onto
/// [`AdminState`]: never a `501` (the handler exists) and never an invented `200`.
pub(crate) fn unwired(source_name: &str, instance: &str) -> Response {
    Problem::unavailable()
        .with_detail(format!(
            "the {source_name} data source is not wired into this server"
        ))
        .with_instance(instance.to_owned())
        .into_response()
}

/// The `Idempotency-Key` header, if the client sent one.
pub(crate) fn idempotency_key(headers: &HeaderMap) -> Option<&str> {
    headers.get("idempotency-key").and_then(|v| v.to_str().ok())
}

/// Before running a mutation: if this `Idempotency-Key` was already used with the same body,
/// the stored response (marked as a replay); with a different body, `422`. `Ok(())` means run
/// it.
#[allow(clippy::result_large_err)]
pub(crate) fn check_replay(
    state: &AdminState,
    headers: &HeaderMap,
    operation_id: &str,
    body: &[u8],
    instance: &str,
) -> Result<(), Response> {
    let Some(key) = idempotency_key(headers) else {
        return Ok(());
    };
    match state.idempotency.check(operation_id, key, body) {
        Replay::Fresh => Ok(()),
        Replay::Same(stored) => Err(axum::http::Response::builder()
            .status(stored.status)
            .header(axum::http::header::CONTENT_TYPE, stored.content_type)
            .header("idempotency-replayed", "true")
            .body(axum::body::Body::from(stored.body))
            .unwrap_or_else(|_| Problem::internal().into_response())),
        Replay::Mismatch => Err(Problem::idempotency_key_payload_mismatch()
            .with_instance(instance.to_owned())
            .into_response()),
    }
}

/// One mutation's audit entry and event: the entry is appended first, and a failed append fails
/// the request with `503` (RFC 0004 section 9: an action whose record could not be written is
/// not reported as done).
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
pub(crate) async fn record(
    state: &AdminState,
    principal: &Principal,
    action: &str,
    event_type: &str,
    target: ResourceRef,
    changes: Vec<AuditChange>,
    event_data: serde_json::Value,
    status: u16,
) -> Result<(), Response> {
    let actor = principal.to_actor();
    let mut entry = AuditEntry::new(
        action,
        actor.clone(),
        target.clone(),
        AuditOutcome::success(status),
    );
    entry.changes = changes;
    state
        .audit
        .append(entry)
        .await
        .map_err(|e| e.to_problem().into_response())?;
    state.events.publish(
        Event::new(event_type, event_data)
            .with_resource(target)
            .with_actor(actor),
    );
    Ok(())
}

/// Answers `value` as JSON with `status`, remembering the answer under the request's
/// `Idempotency-Key` (if any) so a retry is answered the same way without running again.
/// `extra` headers (a `Location`, say) are added to the live answer; a replay carries only the
/// body, as every other replay in this API does.
pub(crate) fn respond_and_remember<T: Serialize>(
    state: &AdminState,
    headers: &HeaderMap,
    operation_id: &str,
    request_body: &[u8],
    status: StatusCode,
    value: &T,
    extra: &[(&'static str, String)],
) -> Response {
    let body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(_) => return Problem::internal().into_response(),
    };
    if let Some(key) = idempotency_key(headers) {
        state.idempotency.record(
            operation_id,
            key,
            request_body,
            StoredResponse {
                status: status.as_u16(),
                content_type: "application/json".to_owned(),
                body: body.clone(),
            },
        );
    }
    let mut response = (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response();
    for (name, value) in extra {
        if let Ok(value) = axum::http::HeaderValue::from_str(value) {
            response.headers_mut().insert(*name, value);
        }
    }
    response
}

/// What the HTTP tests of the areas built on this module share: a state with one token per
/// scope that matters, and a one-call request helper.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::audit::InMemoryAuditSink;
    use crate::auth::StaticVerifier;
    use crate::events::EventBus;
    use crate::model::{Principal, PrincipalKind, Scope};
    use crate::router::AdminState;

    fn principal(id: &str, scope: Scope) -> Principal {
        Principal {
            kind: PrincipalKind::User,
            id: id.to_owned(),
            display_name: None,
            scopes: vec![scope],
            token_id: None,
            expires_at: None,
            issued_by: None,
        }
    }

    /// A state whose tokens are `admin` (`admin:write`), `read` (`admin:read`), `mod-read`
    /// (`moderation:read`) and `mod-write` (`moderation:write`), with an in-memory audit sink
    /// the test can inspect.
    pub(crate) fn state() -> (AdminState, Arc<InMemoryAuditSink>) {
        let verifier = StaticVerifier::new()
            .with_token("admin", principal("@ops:example.org", Scope::AdminWrite))
            .with_token("read", principal("@viewer:example.org", Scope::AdminRead))
            .with_token(
                "mod-read",
                principal("@watcher:example.org", Scope::ModerationRead),
            )
            .with_token(
                "mod-write",
                principal("@mod:example.org", Scope::ModerationWrite),
            );
        let audit = Arc::new(InMemoryAuditSink::new());
        (
            AdminState::new(Arc::new(verifier), audit.clone(), Arc::new(EventBus::new())),
            audit,
        )
    }

    /// Sends one request through the whole router and answers its status, headers and JSON body
    /// (`Null` for an empty one).
    pub(crate) async fn call(
        state: &AdminState,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
        idempotency_key: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let (router, _) = crate::router::build_router(state.clone());
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, headers, json)
    }
}
