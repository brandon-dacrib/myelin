//! Membership endpoints: join, leave, forget, invite, kick, ban, unban, knock.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{RoomId, UserId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::membership::Action;
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
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

fn target_user(body: &Value, requester: &ruma::UserId) -> Result<ruma::OwnedUserId, RoomError> {
    match body.get("user_id").and_then(Value::as_str) {
        Some(s) => UserId::parse(s)
            .map(|u| u.to_owned())
            .map_err(|e| RoomError::BadRequest(format!("invalid user_id: {e}"))),
        None => Ok(requester.to_owned()),
    }
}

fn extra(body: &Value) -> Value {
    let mut out = json!({});
    if let Some(reason) = body.get("reason") {
        out["reason"] = reason.clone();
    }
    if let Some(via) = body.get("join_authorised_via_users_server") {
        out["join_authorised_via_users_server"] = via.clone();
    }
    out
}

async fn act<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &str,
    sender: ruma::OwnedUserId,
    action: Action,
    target: ruma::OwnedUserId,
    body: &Value,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    handle
        .membership(sender, action, target, extra(body), now_ms())
        .await?;
    Ok(Json(json!({})).into_response())
}

/// Like [`act`], but for the two join endpoints: per the spec, `POST /rooms/{roomId}/join` and
/// `POST /join/{roomIdOrAlias}` respond `{"room_id": "!..."}`, not `{}`. Found by
/// `crates/hs-loadgen`'s `matrix-rust-sdk` scenario: the SDK's `join_room_by_id` deserializes the
/// response strictly and rejected the empty body `act` had been sending on every join
/// (`missing field `room_id``), which every unit test speaking this crate's own dialect had
/// missed because none of them asserted the join response body, only that joining succeeded.
async fn act_join<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &str,
    sender: ruma::OwnedUserId,
    body: &Value,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    handle
        .membership(sender.clone(), Action::Join, sender, extra(body), now_ms())
        .await?;
    Ok(Json(json!({ "room_id": room_id })).into_response())
}

/// `POST /rooms/{roomId}/join`.
pub async fn post_join<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    act_join(&state, &room_id, requester.user_id, &body).await
}

/// `POST /join/{roomIdOrAlias}`.
pub async fn post_join_by_id_or_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id_or_alias): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let room_id = if room_id_or_alias.starts_with('!') {
        parse_room_id(&room_id_or_alias)?
    } else {
        let alias = ruma::RoomAliasId::parse(&room_id_or_alias)
            .map_err(|e| RoomError::BadRequest(e.to_string()))?;
        state
            .rooms
            .resolve_alias(&alias)?
            .ok_or_else(|| RoomError::RoomNotFound(room_id_or_alias.clone()))?
    };
    act_join(&state, room_id.as_str(), requester.user_id, &body).await
}

/// `POST /rooms/{roomId}/leave`.
pub async fn post_leave<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let user = requester.user_id.clone();
    act(&state, &room_id, user.clone(), Action::Leave, user, &body).await
}

/// `POST /rooms/{roomId}/forget`. Not implemented as a distinct effect in this pass (this crate
/// does not yet track a "forgotten" flag per user per room, since serving `/sync` -- the consumer
/// of that flag -- is track 05's job); accepted as a no-op so clients that call it unconditionally
/// after `/leave` are not broken.
pub async fn post_forget<B: KvBackend + 'static>(
    State(_state): State<RoomState<B>>,
    Path(_room_id): Path<String>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    Ok(Json(json!({})).into_response())
}

/// `POST /rooms/{roomId}/invite`.
pub async fn post_invite<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Invite,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/kick`.
pub async fn post_kick<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Kick,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/ban`.
pub async fn post_ban<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Ban,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/unban`.
pub async fn post_unban<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Unban,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/knock`.
pub async fn post_knock<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let user = requester.user_id.clone();
    act(&state, &room_id, user.clone(), Action::Knock, user, &body).await
}

/// `POST /knock/{roomIdOrAlias}`.
pub async fn post_knock_by_id_or_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id_or_alias): Path<String>,
    RoomRequester(requester): RoomRequester,
    Json(body): Json<Value>,
) -> Result<Response, RoomError> {
    let room_id = if room_id_or_alias.starts_with('!') {
        parse_room_id(&room_id_or_alias)?
    } else {
        let alias = ruma::RoomAliasId::parse(&room_id_or_alias)
            .map_err(|e| RoomError::BadRequest(e.to_string()))?;
        state
            .rooms
            .resolve_alias(&alias)?
            .ok_or_else(|| RoomError::RoomNotFound(room_id_or_alias.clone()))?
    };
    let user = requester.user_id.clone();
    act(
        &state,
        room_id.as_str(),
        user.clone(),
        Action::Knock,
        user,
        &body,
    )
    .await
}
