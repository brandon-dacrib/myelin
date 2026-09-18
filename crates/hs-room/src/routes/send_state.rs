//! `PUT /rooms/{roomId}/send/{eventType}/{txnId}`, `PUT /rooms/{roomId}/state/{eventType}(/{stateKey})`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::RoomId;
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

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw).map(|r| r.to_owned()).map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// `PUT /rooms/{roomId}/send/{eventType}/{txnId}`.
///
/// Transaction-ID deduplication (the spec requires replaying the same `txnId` to return the same
/// `event_id` rather than sending a second event) is not implemented in this pass -- each call
/// sends a new event. Recorded in `docs/status/04-room-and-events.md`.
pub async fn put_send<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_type, _txn_id)): Path<(String, String, String)>,
    RoomRequester(requester): RoomRequester,
    Json(content): Json<Value>,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let event = handle
        .send_event(
            requester.user_id.clone(),
            event_type,
            None,
            content,
            None,
            now_ms(),
        )
        .await?;
    Ok(Json(json!({"event_id": event.event_id().to_string()})).into_response())
}

/// `PUT /rooms/{roomId}/state/{eventType}/{stateKey}`.
pub async fn put_state<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_type, state_key)): Path<(String, String, String)>,
    RoomRequester(requester): RoomRequester,
    Json(content): Json<Value>,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let event = handle
        .send_event(
            requester.user_id.clone(),
            event_type,
            Some(state_key),
            content,
            None,
            now_ms(),
        )
        .await?;
    Ok(Json(json!({"event_id": event.event_id().to_string()})).into_response())
}

/// `PUT /rooms/{roomId}/state/{eventType}` (empty state key).
pub async fn put_state_no_key<B: KvBackend + 'static>(
    state: State<RoomState<B>>,
    Path((room_id, event_type)): Path<(String, String)>,
    requester: RoomRequester,
    body: Json<Value>,
) -> Result<Response, RoomError> {
    put_state(state, Path((room_id, event_type, String::new())), requester, body).await
}
