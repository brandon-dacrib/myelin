//! Membership endpoints: join, leave, forget, invite, kick, ban, unban, knock.
//!
//! # Profile propagation
//!
//! Per the spec, `displayname`/`avatar_url` on an `m.room.member` event are a snapshot of the
//! target user's profile *at the time the event was sent*, not a live reference -- a later
//! profile change does not retroactively edit past membership events. [`extra`] reads the
//! target's current profile (`hs_auth::store::UserRecord::display_name`/`avatar_url`, via
//! `RoomState::auth`'s embedded `AuthState::store`) and merges it into the `m.room.member`
//! content for [`Action::Join`], [`Action::Invite`] and [`Action::Knock`] -- the three actions
//! that put the *target's own* profile into their own membership event. A local user's profile
//! lookup is a synchronous, in-process call to `hs-auth`'s store (both crates already share the
//! same store in `hs serve`'s single-process deployment); a remote user's profile is simply
//! whatever `get_user` returns for them locally, which is `None` today (this crate does not
//! query federation for a remote profile) -- their membership event carries no profile fields,
//! same as before this change.
//!
//! **Rewriting a user's already-sent `m.room.member` event in every room they are currently
//! joined to whenever their profile changes** (Synapse's fuller behavior, via
//! `ProfileHandler.on_profile_update`/`_update_join_states`) **is now implemented**, in
//! `crate::routes::profile` (`PUT /profile/{userId}/displayname`/`avatar_url`, mounted from this
//! crate's router rather than `hs-auth`'s -- see that module's doc comment for why and for the
//! full design). It reuses exactly the mechanism this module already had for a different reason:
//! the join transition table (`crate::membership::TRANSITIONS`) allows [`Action::Join`] again from
//! [`crate::membership::PriorState::Join`] as a harmless re-send
//! (`RoomActor::refresh_own_profile` calls `membership_action` the same way `act_join` below
//! does), so a client that wants its already-joined membership event updated with a fresh profile
//! can *also* still get one for free by calling `POST /rooms/{roomId}/join` again -- both paths
//! converge on the same idempotent re-send.

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

/// Builds the extra `m.room.member` content fields beyond `membership` itself: the client-supplied
/// `reason`/`join_authorised_via_users_server`, plus -- for [`Action::Join`], [`Action::Invite`]
/// and [`Action::Knock`] -- the target's current profile. See the module docs for exactly what
/// this does and does not cover.
async fn extra<B: hs_kv::KvBackend + 'static>(
    state: &RoomState<B>,
    action: Action,
    target: &ruma::UserId,
    body: &Value,
) -> Value {
    let mut out = json!({});
    if let Some(reason) = body.get("reason") {
        out["reason"] = reason.clone();
    }
    if let Some(via) = body.get("join_authorised_via_users_server") {
        out["join_authorised_via_users_server"] = via.clone();
    }
    if matches!(action, Action::Join | Action::Invite | Action::Knock)
        && let Ok(Some(profile)) = state.auth.store.get_user(target).await
    {
        if let Some(name) = profile.display_name {
            out["displayname"] = Value::String(name);
        }
        if let Some(avatar) = profile.avatar_url {
            out["avatar_url"] = Value::String(avatar);
        }
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
    let content = extra(state, action, &target, body).await;
    handle
        .membership(sender, action, target, content, now_ms())
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
    let content = extra(state, Action::Join, &sender, body).await;
    handle
        .membership(sender.clone(), Action::Join, sender, content, now_ms())
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

/// `POST /rooms/{roomId}/forget`. Per the spec
/// (`refs/matrix-spec/data/api/client-server/leaving.yaml`, Apache-2.0): `400 M_UNKNOWN` if the
/// requester is still joined to the room, or if the room does not exist at all (this crate does
/// not distinguish the two in its response, matching the spec's one documented error shape for
/// this endpoint -- see [`RoomError::StillJoined`]'s doc comment). Otherwise marks the room
/// forgotten (`crate::actor::RoomActor::forget`), which `GET .../messages` (`can_read_room`) then
/// refuses outright until the requester rejoins.
pub async fn post_forget<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => handle,
        Err(RoomError::RoomNotFound(_)) => {
            return Err(RoomError::StillJoined(format!(
                "room {room_id} does not exist"
            )));
        }
        Err(e) => return Err(e),
    };
    handle.forget(requester.user_id).await?;
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
