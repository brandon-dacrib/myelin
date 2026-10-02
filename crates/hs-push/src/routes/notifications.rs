//! `GET /notifications`: the events the caller was notified about, newest first, paged.

use axum::Json;
use axum::extract::{Query, State};
use hs_kv::KvBackend;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::{PushRequester, PushState};

/// `GET /notifications`'s query parameters.
#[derive(Deserialize)]
pub struct NotificationsQuery {
    /// The `next_token` of the previous page.
    from: Option<String>,
    /// Page size; the spec's default is server-chosen, 50 here, capped at 500.
    limit: Option<usize>,
    /// `highlight` to return only highlighted notifications.
    only: Option<String>,
}

/// `GET /notifications`.
pub async fn get_notifications<B: KvBackend + 'static>(
    PushRequester(requester): PushRequester,
    State(state): State<PushState<B>>,
    Query(query): Query<NotificationsQuery>,
) -> Result<Json<Value>, hs_http::error::MatrixError> {
    let before = match query.from.as_deref().filter(|s| !s.is_empty()) {
        Some(token) => Some(token.parse::<u64>().map_err(|_| {
            hs_http::error::MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                hs_http::error::MatrixErrorCode::InvalidParam,
                "from is not a token this server issued",
            )
        })?),
        None => None,
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let only_highlight = query.only.as_deref() == Some("highlight");
    let entries = state
        .notification_log
        .page(&requester.user_id, before, limit, only_highlight)
        .await
        .map_err(|e| {
            hs_http::error::MatrixError::custom(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                hs_http::error::MatrixErrorCode::Unknown,
                e.to_string(),
            )
        })?;
    let next_token = (entries.len() == limit)
        .then(|| entries.last().map(|e| e.seq.to_string()))
        .flatten();
    let notifications: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            json!({
                "room_id": e.room_id,
                "actions": e.actions,
                "event": e.event,
                "profile_tag": e.profile_tag,
                "read": e.read,
                "ts": e.ts_ms,
            })
        })
        .collect();
    let mut body = json!({ "notifications": notifications });
    if let Some(token) = next_token {
        body["next_token"] = json!(token);
    }
    Ok(Json(body))
}
