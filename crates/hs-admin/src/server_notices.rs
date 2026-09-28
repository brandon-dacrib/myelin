//! The `server_notices.*` operations: messages from the server itself to its users, per the
//! Matrix specification's "Server Notices" module.
//!
//! Each recipient gets the notice in their own server-notices room, created by and sent from the
//! server-notices user and tagged `m.server_notice` for them, which is how a client knows to show
//! it as a notice from the server rather than as a conversation. Rooms, the user and the event
//! are the room layer's business; this module owns the admin API's half -- scopes, validating
//! the request, idempotency, the audit entry and the event -- and reaches the rest through
//! [`ServerNoticeSource`], which `hs serve` implements over the room registry.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{ScopeDecision, require_scope};
use crate::idempotency::{Replay, StoredResponse};
use crate::model::{Page, ResourceRef, Scope};
use crate::router::{
    AdminState, authorization_header, idempotency_key, record_mutation, replay_response,
    source_unavailable,
};
use crate::sources::SourceError;

/// The most recipients one request may name. A notice to every user on a large server is a
/// task, not a request; this keeps one request's work bounded.
pub const MAX_RECIPIENTS: usize = 1000;

/// The OpenAPI `ServerNotice` schema: one notice as sent, and one row of the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminServerNotice {
    /// The notice's identifier in the history.
    pub id: String,
    /// The server-notices user it was sent as.
    pub sender: String,
    /// The event type that was sent.
    #[serde(rename = "type")]
    pub event_type: String,
    /// The event content that was sent.
    pub content: Value,
    /// Who it was sent to.
    pub recipients: Vec<String>,
    /// Each recipient's server-notices room, in the order of `recipients`.
    pub room_ids: Vec<String>,
    /// The event sent into each room, in the order of `recipients`.
    pub event_ids: Vec<String>,
    /// When it was sent (RFC 3339).
    pub sent_at: String,
}

/// A notice to send, after the handler has validated the request.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerNoticeRequest {
    /// Local user IDs, without duplicates, in the order the request named them.
    pub recipients: Vec<String>,
    /// The event type; `m.room.message` unless the request said otherwise.
    pub event_type: String,
    /// The event content.
    pub content: Value,
    /// A state key, which makes the notice a state event in each room.
    pub state_key: Option<String>,
}

/// What sends server notices and remembers them.
#[async_trait::async_trait]
pub trait ServerNoticeSource: Send + Sync + 'static {
    /// Sends one notice to every recipient. Checks every recipient before sending anything:
    /// [`SourceError::NotFound`] (or [`SourceError::InvalidField`]) if one is not a local user,
    /// and then nobody has been sent anything.
    async fn send(&self, request: ServerNoticeRequest) -> Result<AdminServerNotice, SourceError>;
    /// Every notice sent, newest first.
    async fn list(&self) -> Result<Vec<AdminServerNotice>, SourceError>;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendBody {
    recipients: Vec<String>,
    content: Value,
    #[serde(rename = "type")]
    event_type: Option<String>,
    state_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
}

fn invalid_field(pointer: &'static str, detail: impl Into<String>) -> SourceError {
    SourceError::InvalidField {
        pointer,
        detail: detail.into(),
    }
}

/// Checks the request's shape; whether each recipient is a user of this server is the source's
/// to say.
fn validate(body: SendBody) -> Result<ServerNoticeRequest, SourceError> {
    if body.recipients.is_empty() {
        return Err(invalid_field("/recipients", "name at least one recipient"));
    }
    let mut recipients: Vec<String> = Vec::with_capacity(body.recipients.len());
    for recipient in body.recipients {
        let recipient = recipient.trim().to_owned();
        let well_formed = recipient.starts_with('@')
            && recipient
                .split_once(':')
                .is_some_and(|(local, server)| local.len() > 1 && !server.is_empty());
        if !well_formed {
            return Err(invalid_field(
                "/recipients",
                format!("{recipient:?} is not a user ID (@localpart:server)"),
            ));
        }
        if !recipients.contains(&recipient) {
            recipients.push(recipient);
        }
    }
    if recipients.len() > MAX_RECIPIENTS {
        return Err(invalid_field(
            "/recipients",
            format!("at most {MAX_RECIPIENTS} recipients per notice"),
        ));
    }
    if !body.content.is_object() {
        return Err(invalid_field("/content", "content must be a JSON object"));
    }
    let event_type = body
        .event_type
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "m.room.message".to_owned());
    if body.state_key.is_none() && event_type == "m.room.message" {
        let has_body = body
            .content
            .get("body")
            .and_then(Value::as_str)
            .is_some_and(|b| !b.trim().is_empty());
        if !has_body {
            return Err(invalid_field(
                "/content/body",
                "a message needs a non-empty body",
            ));
        }
    }
    Ok(ServerNoticeRequest {
        recipients,
        event_type,
        content: body.content,
        state_key: body.state_key,
    })
}

/// `POST /api/v1/server-notices` (`moderation:write`): `201` with the notice as sent. Honors
/// `Idempotency-Key`, so a retried send does not notify anybody twice.
pub(crate) async fn send(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/server-notices";
    let principal = match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::ModerationWrite),
    )
    .await
    {
        ScopeDecision::Allowed(p) => p,
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            return p.with_instance(instance).into_response();
        }
    };
    let Some(source) = &state.server_notices else {
        return source_unavailable("server notices", instance);
    };
    if let Some(key) = idempotency_key(&headers) {
        match state.idempotency.check("server_notices.send", key, &body) {
            Replay::Same(stored) => return replay_response(stored),
            Replay::Mismatch => {
                return Problem::idempotency_key_payload_mismatch()
                    .with_instance(instance)
                    .into_response();
            }
            Replay::Fresh => {}
        }
    }
    let request: SendBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return Problem::validation_failed()
                .with_detail(format!("invalid JSON body: {e}"))
                .with_instance(instance)
                .into_response();
        }
    };
    let request = match validate(request) {
        Ok(r) => r,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let sent = match source.send(request).await {
        Ok(n) => n,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "server_notices.send",
        "server_notice.sent",
        ResourceRef::new("server_notice", sent.id.clone()),
        Vec::new(),
        json!({
            "id": sent.id,
            "recipients": sent.recipients,
            "event_ids": sent.event_ids,
        }),
    )
    .await
    {
        return resp;
    }
    let response_body = serde_json::to_vec(&sent).unwrap_or_default();
    if let Some(key) = idempotency_key(&headers) {
        state.idempotency.record(
            "server_notices.send",
            key,
            &body,
            StoredResponse {
                status: 201,
                content_type: "application/json".to_string(),
                body: response_body.clone(),
            },
        );
    }
    (
        StatusCode::CREATED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        response_body,
    )
        .into_response()
}

/// `GET /api/v1/server-notices` (`moderation:read`): the history, newest first.
pub(crate) async fn list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let instance = "/api/v1/server-notices";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::ModerationRead),
    )
    .await
    {
        ScopeDecision::Allowed(_) => {
            let Some(source) = &state.server_notices else {
                return source_unavailable("server notices", instance);
            };
            match source.list().await {
                Ok(items) => axum::Json(Page::paginate(
                    items,
                    query.cursor.as_deref(),
                    query.limit,
                    query.include_total.unwrap_or(false),
                ))
                .into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            p.with_instance(instance).into_response()
        }
    }
}

/// An in-memory [`ServerNoticeSource`] for tests: "sends" by recording, with made-up room and
/// event IDs, and knows which users exist.
pub struct InMemoryServerNotices {
    sender: String,
    users: Vec<String>,
    sent: std::sync::Mutex<Vec<AdminServerNotice>>,
}

impl InMemoryServerNotices {
    /// A source whose notices come from `sender` and that knows `users`.
    #[must_use]
    pub fn new(sender: impl Into<String>, users: Vec<String>) -> Self {
        Self {
            sender: sender.into(),
            users,
            sent: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl ServerNoticeSource for InMemoryServerNotices {
    async fn send(&self, request: ServerNoticeRequest) -> Result<AdminServerNotice, SourceError> {
        if let Some(unknown) = request.recipients.iter().find(|r| !self.users.contains(r)) {
            return Err(invalid_field(
                "/recipients",
                format!("{unknown} is not a user of this server"),
            ));
        }
        let mut sent = self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = sent.len();
        let notice = AdminServerNotice {
            id: crate::model::new_id(),
            sender: self.sender.clone(),
            event_type: request.event_type,
            content: request.content,
            room_ids: request
                .recipients
                .iter()
                .enumerate()
                .map(|(i, _)| format!("!notices{i}:test"))
                .collect(),
            event_ids: request
                .recipients
                .iter()
                .enumerate()
                .map(|(i, _)| format!("$notice{n}_{i}"))
                .collect(),
            recipients: request.recipients,
            sent_at: hs_http::time::now_rfc3339(),
        };
        sent.insert(0, notice.clone());
        Ok(notice)
    }

    async fn list(&self) -> Result<Vec<AdminServerNotice>, SourceError> {
        Ok(self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> SendBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_message_defaults_to_m_room_message_and_dedupes_recipients() {
        let r = validate(body(json!({
            "recipients": ["@a:x", " @a:x ", "@b:x"],
            "content": {"msgtype": "m.text", "body": "hi"}
        })))
        .unwrap();
        assert_eq!(r.recipients, vec!["@a:x", "@b:x"]);
        assert_eq!(r.event_type, "m.room.message");
    }

    #[test]
    fn refuses_no_recipients_malformed_ids_non_object_content_and_empty_bodies() {
        for (v, pointer) in [
            (
                json!({"recipients": [], "content": {"body": "x"}}),
                "/recipients",
            ),
            (
                json!({"recipients": ["alice"], "content": {"body": "x"}}),
                "/recipients",
            ),
            (
                json!({"recipients": ["@:x"], "content": {"body": "x"}}),
                "/recipients",
            ),
            (json!({"recipients": ["@a:x"], "content": "x"}), "/content"),
            (
                json!({"recipients": ["@a:x"], "content": {"body": "  "}}),
                "/content/body",
            ),
        ] {
            match validate(body(v.clone())) {
                Err(SourceError::InvalidField { pointer: p, .. }) => assert_eq!(p, pointer, "{v}"),
                other => panic!("{v}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_state_notice_needs_no_body() {
        let r = validate(body(json!({
            "recipients": ["@a:x"],
            "type": "m.room.topic",
            "state_key": "",
            "content": {"topic": "t"}
        })))
        .unwrap();
        assert_eq!(r.state_key.as_deref(), Some(""));
    }
}
