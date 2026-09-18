//! `GET`/`PUT /user/{userId}/account_data/{type}` and the room-scoped equivalent
//! `GET`/`PUT /user/{userId}/rooms/{roomId}/account_data/{type}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

fn require_self(requester: &hs_auth::requester::Requester, path_user_id: &str) -> Result<(), UserError> {
    if requester.user_id.as_str() != path_user_id {
        return Err(UserError::NotSelf(
            "cannot access another user's account data".to_owned(),
        ));
    }
    Ok(())
}

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, UserError> {
    ruma::RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| UserError::InvalidId(e.to_string()))
}

/// `GET /user/{userId}/account_data/{type}`.
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the data does not exist, or on a store
/// failure.
pub async fn get_global<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, event_type)): Path<(String, String)>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let record = state
        .hub
        .store()
        .get_global_account_data(&requester.user_id, &event_type)
        .await?
        .ok_or_else(|| UserError::NotFound("account data not found".to_owned()))?;
    Ok(Json(record.content).into_response())
}

/// `PUT /user/{userId}/account_data/{type}`.
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, or on a store failure.
pub async fn put_global<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, event_type)): Path<(String, String)>,
    UserRequester(requester): UserRequester,
    Json(content): Json<serde_json::Value>,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    state
        .hub
        .store()
        .put_global_account_data(&requester.user_id, &event_type, content)
        .await?;
    Ok(Json(serde_json::json!({})).into_response())
}

/// `GET /user/{userId}/rooms/{roomId}/account_data/{type}`.
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the room id is invalid, the data does
/// not exist, or on a store failure.
pub async fn get_room<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, room_id, event_type)): Path<(String, String, String)>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let room_id = parse_room_id(&room_id)?;
    let record = state
        .hub
        .store()
        .list_room_account_data(&requester.user_id, &room_id)
        .await?
        .into_iter()
        .find(|a| a.event_type == event_type)
        .ok_or_else(|| UserError::NotFound("account data not found".to_owned()))?;
    Ok(Json(record.content).into_response())
}

/// `PUT /user/{userId}/rooms/{roomId}/account_data/{type}`.
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the room id is invalid, or on a store
/// failure.
pub async fn put_room<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, room_id, event_type)): Path<(String, String, String)>,
    UserRequester(requester): UserRequester,
    Json(content): Json<serde_json::Value>,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let room_id = parse_room_id(&room_id)?;
    state
        .hub
        .store()
        .put_room_account_data(&requester.user_id, &room_id, &event_type, content)
        .await?;
    Ok(Json(serde_json::json!({})).into_response())
}
