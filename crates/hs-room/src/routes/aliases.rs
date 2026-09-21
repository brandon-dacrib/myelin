//! `GET /rooms/{roomId}/aliases`, `PUT`/`GET`/`DELETE /directory/room/{roomAlias}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{RoomAliasId, RoomId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::state::{RoomRequester, RoomState};

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

fn parse_alias(raw: &str) -> Result<ruma::OwnedRoomAliasId, RoomError> {
    RoomAliasId::parse(raw)
        .map(|a| a.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// `GET /rooms/{roomId}/aliases`.
pub async fn get_room_aliases<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let aliases = handle.query(|actor| actor.list_aliases()).await?;
    Ok(Json(json!({"aliases": aliases})).into_response())
}

/// `PUT /directory/room/{roomAlias}`.
pub async fn put_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_alias): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let alias = parse_alias(&room_alias)?;
    let room_id_str = body
        .get("room_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RoomError::BadRequest("missing room_id".into()))?;
    let room_id = parse_room_id(room_id_str)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let creator = requester.user_id.clone();
    handle
        .query(move |actor| actor.create_alias(&alias, &creator))
        .await?;
    Ok(Json(json!({})).into_response())
}

/// `GET /directory/room/{roomAlias}`.
pub async fn get_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_alias): Path<String>,
) -> Result<Response, RoomError> {
    let alias = parse_alias(&room_alias)?;
    let room_id = state
        .rooms
        .resolve_alias(&alias)?
        .ok_or_else(|| RoomError::RoomNotFound(room_alias.clone()))?;
    Ok(Json(json!({"room_id": room_id.to_string(), "servers": [state.identity.server_name.to_string()]})).into_response())
}

/// `DELETE /directory/room/{roomAlias}`.
///
/// Anyone could delete anyone's alias before this: the requester was extracted and dropped. The
/// rule the spec allows and every other server implements is that you may remove an alias you
/// created, or one in a room where you have the power to set `m.room.canonical_alias` -- a
/// moderator tidying up after somebody, not a passer-by unpicking a room's address.
pub async fn delete_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_alias): Path<String>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let alias = parse_alias(&room_alias)?;
    let room_id = state
        .rooms
        .resolve_alias(&alias)?
        .ok_or_else(|| RoomError::RoomNotFound(room_alias.clone()))?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let user_id = requester.user_id.clone();
    let check_alias = alias.clone();
    let allowed = handle
        .query(move |actor| {
            let created_it = actor
                .alias_creator(&check_alias)?
                .is_some_and(|creator| creator == user_id);
            if created_it {
                return Ok::<bool, RoomError>(true);
            }
            actor.can_send_state(&user_id, "m.room.canonical_alias")
        })
        .await?;
    if !allowed {
        return Err(RoomError::Forbidden(format!(
            "{} was not created by you, and you do not have permission to remove it",
            alias.as_str()
        )));
    }
    handle
        .query(move |actor| actor.remove_alias(&alias))
        .await?;
    Ok(Json(json!({})).into_response())
}
