//! `POST /createRoom`.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{OwnedUserId, RoomVersionId, UserId};
use serde_json::{Value, json};

use crate::actor::{CreateRoomRequest, InitialStateEvent};
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

fn parse_user_list(body: &Value, key: &str) -> Result<Vec<OwnedUserId>, RoomError> {
    let Some(array) = body.get(key) else {
        return Ok(Vec::new());
    };
    let items = array
        .as_array()
        .ok_or_else(|| RoomError::BadRequest(format!("{key} must be an array")))?;
    items
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| RoomError::BadRequest(format!("{key} entries must be strings")))
                .and_then(|s| {
                    UserId::parse(s).map(|u| u.to_owned()).map_err(|e| {
                        RoomError::BadRequest(format!("invalid user ID in {key}: {e}"))
                    })
                })
        })
        .collect()
}

fn parse_initial_state(body: &Value) -> Result<Vec<InitialStateEvent>, RoomError> {
    let Some(array) = body.get("initial_state") else {
        return Ok(Vec::new());
    };
    let items = array
        .as_array()
        .ok_or_else(|| RoomError::BadRequest("initial_state must be an array".into()))?;
    items
        .iter()
        .map(|item| {
            let event_type = item
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| RoomError::BadRequest("initial_state entry missing type".into()))?
                .to_owned();
            let state_key = item
                .get("state_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let content = item.get("content").cloned().unwrap_or_else(|| json!({}));
            Ok(InitialStateEvent {
                event_type,
                state_key,
                content,
            })
        })
        .collect()
}

/// `POST /createRoom`.
pub async fn post_create_room<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let room_version = match body.get("room_version").and_then(Value::as_str) {
        Some(v) => Some(
            RoomVersionId::try_from(v)
                .map_err(|_| RoomError::UnsupportedRoomVersion(v.to_owned()))?,
        ),
        None => None,
    };

    let request = CreateRoomRequest {
        room_version,
        preset: body
            .get("preset")
            .and_then(Value::as_str)
            .map(str::to_owned),
        name: body.get("name").and_then(Value::as_str).map(str::to_owned),
        topic: body.get("topic").and_then(Value::as_str).map(str::to_owned),
        invite: parse_user_list(&body, "invite")?,
        initial_state: parse_initial_state(&body)?,
        power_level_content_override: body.get("power_level_content_override").cloned(),
        creation_content: body
            .get("creation_content")
            .cloned()
            .unwrap_or_else(|| json!({})),
        room_alias_name: body
            .get("room_alias_name")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };

    let handle = state
        .rooms
        .create_room(requester.user_id.clone(), request, now_ms())
        .await?;

    let room_id = handle.query(|actor| actor.room_id().to_owned()).await;

    Ok(Json(json!({"room_id": room_id.to_string()})).into_response())
}
