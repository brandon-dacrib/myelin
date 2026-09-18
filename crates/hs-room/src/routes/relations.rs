//! `GET /rooms/{roomId}/relations/{eventId}(/{relType}(/{eventType}))`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{EventId, RoomId};
use serde_json::json;

use crate::error::RoomError;
use crate::routes::render::client_event_json;
use crate::state::{RoomRequester, RoomState};

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

fn parse_event_id(raw: &str) -> Result<ruma::OwnedEventId, RoomError> {
    EventId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

async fn relations_response<B: KvBackend + 'static>(
    state: RoomState<B>,
    room_id: String,
    event_id: String,
    rel_type: Option<String>,
    event_type: Option<String>,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let event_id = parse_event_id(&event_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let chunk = handle
        .query(move |actor| {
            actor
                .relations_of(&event_id, rel_type.as_deref())
                .into_iter()
                .filter(|e| {
                    event_type
                        .as_deref()
                        .is_none_or(|t| e.header().event_type == t)
                })
                .map(client_event_json)
                .collect::<Vec<_>>()
        })
        .await;
    Ok(Json(json!({"chunk": chunk})).into_response())
}

/// `GET /rooms/{roomId}/relations/{eventId}`.
pub async fn get_relations<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id)): Path<(String, String)>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    relations_response(state, room_id, event_id, None, None).await
}

/// `GET /rooms/{roomId}/relations/{eventId}/{relType}`.
pub async fn get_relations_by_rel_type<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id, rel_type)): Path<(String, String, String)>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    relations_response(state, room_id, event_id, Some(rel_type), None).await
}

/// `GET /rooms/{roomId}/relations/{eventId}/{relType}/{eventType}`.
pub async fn get_relations_by_rel_type_and_event_type<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id, rel_type, event_type)): Path<(String, String, String, String)>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    relations_response(state, room_id, event_id, Some(rel_type), Some(event_type)).await
}
