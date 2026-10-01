//! `POST /createRoom`.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
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

/// `PRESETS`: the only `preset` values the spec defines. A `preset` outside this set is rejected
/// with `M_BAD_JSON` rather than silently falling back to `private_chat`'s defaults, which is what
/// `RoomActor::create_room`'s own `match preset { ... _ => ... }` would otherwise do for a typo.
const PRESETS: &[&str] = &["private_chat", "public_chat", "trusted_private_chat"];

/// `POST /createRoom`.
pub async fn post_create_room<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    RoomRequester(requester): RoomRequester,
    preassigned: Option<axum::Extension<hs_cluster::PreassignedRoomId>>,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    crate::moderation::refuse_if_suspended(&requester)?;
    // `room_version` must be a JSON string if present at all -- a well-formed-but-wrong-typed
    // value (a number, an object, ...) is `M_BAD_JSON` (sytest/Complement: "rejects attempts to
    // create rooms with numeric versions"), distinct from a well-typed but unrecognized version
    // string, which is `M_UNSUPPORTED_ROOM_VERSION` (below, via `RoomActor::create`'s own check --
    // `ruma::RoomVersionId::try_from` accepts any syntactically valid opaque token, known or not,
    // so *this* function cannot tell "unsupported" apart from "unknown" itself; that gate is
    // `hs_model::room_version::rules_for`, reached through `RoomActor::create`).
    let room_version = match body.get("room_version") {
        None | Some(Value::Null) => None,
        Some(Value::String(v)) => Some(
            RoomVersionId::try_from(v.as_str())
                .map_err(|_| RoomError::UnsupportedRoomVersion(v.clone()))?,
        ),
        Some(_) => {
            return Err(RoomError::BadRequest(
                "room_version must be a string".into(),
            ));
        }
    };

    let preset = body
        .get("preset")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(preset) = preset.as_deref()
        && !PRESETS.contains(&preset)
    {
        return Err(RoomError::BadRequest(format!(
            "preset must be one of {PRESETS:?}, got {preset:?}"
        )));
    }

    if let Some(visibility) = body.get("visibility") {
        match visibility.as_str() {
            Some("public" | "private") => {}
            _ => {
                return Err(RoomError::BadRequest(
                    "visibility must be \"public\" or \"private\"".into(),
                ));
            }
        }
    }

    let creation_content = match body.get("creation_content") {
        None => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => {
            return Err(RoomError::BadRequest(
                "creation_content must be an object".into(),
            ));
        }
    };

    // The override is merged key-by-key over the generated power-levels content, so anything
    // that is not a JSON object has no meaning at all -- reject it rather than ignore it.
    let power_level_content_override = match body.get("power_level_content_override") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Object(_)) => Some(v.clone()),
        Some(_) => {
            return Err(RoomError::BadRequest(
                "power_level_content_override must be an object".into(),
            ));
        }
    };

    let publish = body.get("visibility").and_then(Value::as_str) == Some("public");
    // The spec, on `preset`: "If unspecified, the server should use the `visibility` to determine
    // which preset to use. A visibility of `public` equates to a preset of `public_chat` and
    // `private` visibility equates to a preset of `private_chat`." Without this a room created
    // with only `visibility: public` was listed in the directory and impossible to join from it:
    // published, and invite-only.
    let preset = preset.or_else(|| publish.then(|| "public_chat".to_owned()));

    let mut invite = parse_user_list(&body, "invite")?;
    // A shadow-banned creator gets their room, and believes the invitations went out; none do.
    if requester.shadow_banned && !invite.is_empty() {
        crate::moderation::note_shadowed(&requester, "createRoom invite");
        invite.clear();
    }
    // The membership events creating the room sends -- the creator's join, the invitations --
    // say who these people are, like any other membership event. And the invitations of a
    // direct chat say that it is one: `is_direct` on the invitation is the only way the invitee's
    // client can know to file the room under people rather than rooms.
    let is_direct = body.get("is_direct").and_then(Value::as_bool) == Some(true);
    let mut member_content = std::collections::HashMap::new();
    for user in std::iter::once(&requester.user_id).chain(&invite) {
        let mut content = json!({});
        if is_direct && user != &requester.user_id {
            content["is_direct"] = Value::Bool(true);
        }
        crate::routes::membership::fill_in_profile(&state, user, &mut content).await;
        member_content.insert(user.clone(), content);
    }

    // An invitation of a user of another server has to be co-signed by that server before it
    // goes into the room (`PUT /invite`), so those are sent here once the room exists, the same
    // way `POST /invite` sends them, rather than written by the actor while it builds the room.
    // Without a federation hook they are written as before.
    let remote_invites: Vec<(OwnedUserId, Value)> = match &state.remote_join {
        Some(_) => invite
            .iter()
            .filter(|user| user.server_name() != &*state.identity.server_name)
            .map(|user| {
                let content = member_content
                    .get(user)
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                (user.clone(), content)
            })
            .collect(),
        None => Vec::new(),
    };

    let request = CreateRoomRequest {
        room_version,
        preset,
        name: body.get("name").and_then(Value::as_str).map(str::to_owned),
        topic: body.get("topic").and_then(Value::as_str).map(str::to_owned),
        local_invites_only: state.remote_join.is_some(),
        invite,
        member_content,
        initial_state: parse_initial_state(&body)?,
        power_level_content_override,
        creation_content,
        room_alias_name: body
            .get("room_alias_name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        // In cluster mode the shard gate chose the room's id before routing the request here,
        // so the room is built on the replica that owns it (RFC 0019); single-node requests
        // carry no extension and the actor mints the id as before.
        room_id: preassigned
            .map(|axum::Extension(p)| ruma::RoomId::parse(p.as_str()).map(|r| r.to_owned()))
            .transpose()
            .map_err(|e| RoomError::Internal(format!("pre-assigned room id: {e}")))?,
    };

    let handle = state
        .rooms
        .create_room(requester.user_id.clone(), request, now_ms())
        .await?;

    let room_id = handle.query(|actor| actor.room_id().to_owned()).await;

    // The room exists whatever an invitee's server says, and the client needs its ID, so an
    // invitation that fails is logged and the others are still sent (as Synapse and conduwuit
    // do); the inviter can try `POST /invite` again.
    if let Some(remote) = &state.remote_join {
        for (user, content) in remote_invites {
            if let Err(error) = crate::routes::membership::invite_remote(
                remote.as_ref(),
                &handle,
                requester.user_id.clone(),
                user.clone(),
                content,
            )
            .await
            {
                tracing::warn!(%room_id, invitee = %user, %error, "createRoom could not invite a user of another server");
            }
        }
    }

    // `invite_3pid`: each address is invited as `POST /invite` with an address would
    // (`crate::third_party_invite`). Like the invitations above, one that fails is logged and the
    // room is still made.
    for entry in body
        .get("invite_3pid")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Err(error) =
            crate::third_party_invite::invite(&state, &room_id, &requester.user_id, entry).await
        {
            tracing::warn!(%room_id, %error, "createRoom could not send a third-party invite");
        }
    }

    // `visibility` controls only the published room directory (`GET /publicRooms`), orthogonal to
    // `preset`'s join-rule/history-visibility/guest-access defaults -- see this crate's status
    // file for why these are two independent request fields, not one.
    if publish {
        state.rooms.set_directory_visibility(&room_id, true)?;
    }

    Ok(Json(json!({"room_id": room_id.to_string()})).into_response())
}
