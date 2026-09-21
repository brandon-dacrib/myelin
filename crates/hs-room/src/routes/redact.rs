//! `PUT /rooms/{roomId}/redact/{eventId}/{txnId}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{EventId, RoomId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::state::{RoomRequester, RoomState};

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// `PUT /rooms/{roomId}/redact/{eventId}/{txnId}`.
///
/// Deduplicated on `(sender, device, txnId)`, like `crate::routes::send_state::put_send`
/// (`RoomActor::redact_txn`).
pub async fn put_redact<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id, txn_id)): Path<(String, String, String)>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let room_id = RoomId::parse(&room_id)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))?;
    let target = EventId::parse(&event_id)
        .map(|e| e.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))?;
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let handle = state.rooms.get_or_load(&room_id).await?;
    let event = handle
        .redact(
            requester.user_id.clone(),
            requester.device_id.clone(),
            txn_id,
            target,
            reason,
            now_ms(),
        )
        .await?;
    Ok(Json(json!({"event_id": event.event_id().to_string()})).into_response())
}
