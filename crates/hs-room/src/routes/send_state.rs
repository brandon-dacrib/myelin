//! `PUT /rooms/{roomId}/send/{eventType}/{txnId}`, `PUT /rooms/{roomId}/state/{eventType}(/{stateKey})`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
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
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// `PUT /rooms/{roomId}/send/{eventType}/{txnId}`.
///
/// Deduplicated on `(sender, device, txnId)`: replaying the same transaction ID returns the same
/// `event_id` rather than sending a second event (`RoomActor::send_event_txn`).
pub async fn put_send<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_type, txn_id)): Path<(String, String, String)>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(content): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let event = handle
        .send_event_txn(
            requester.user_id.clone(),
            requester.device_id.clone(),
            txn_id,
            event_type,
            content,
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
    PermissiveJson(content): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    if event_type == "m.room.canonical_alias" && state_key.is_empty() {
        check_canonical_alias(&state, &room_id, &handle, &content).await?;
    }
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

/// Every alias an `m.room.canonical_alias` content names: `alias`, then `alt_aliases`.
///
/// # Errors
/// [`RoomError::InvalidParam`] if either has the wrong JSON type. `alias: null` and a missing
/// `alt_aliases` are both fine -- that is how a canonical alias is *removed*.
fn named_aliases(content: &Value) -> Result<Vec<String>, RoomError> {
    let mut named = Vec::new();
    match content.get("alias") {
        None | Some(Value::Null) => {}
        Some(Value::String(alias)) => named.push(alias.clone()),
        Some(_) => return Err(RoomError::InvalidParam("alias must be a string".into())),
    }
    match content.get("alt_aliases") {
        None | Some(Value::Null) => {}
        Some(Value::Array(alt)) => {
            for alias in alt {
                named.push(
                    alias
                        .as_str()
                        .ok_or_else(|| {
                            RoomError::InvalidParam(
                                "alt_aliases must be an array of strings".into(),
                            )
                        })?
                        .to_owned(),
                );
            }
        }
        Some(_) => {
            return Err(RoomError::InvalidParam(
                "alt_aliases must be an array of strings".into(),
            ));
        }
    }
    Ok(named)
}

/// Refuses an `m.room.canonical_alias` that names an alias which is not this room's.
///
/// The spec, on `PUT /rooms/{roomId}/state/m.room.canonical_alias`: a server should check that
/// each alias is valid (`400 M_INVALID_PARAM` if not) and that it points to this room (`400
/// M_BAD_ALIAS` if not). Without the check a room could advertise itself under any alias at all,
/// including another room's -- which a client then shows as the room's name and offers as its
/// address. Complement's `TestRoomCanonicalAlias` is nine assertions of this.
///
/// Only aliases the event *adds* are checked, as Synapse does: one already in the room's current
/// canonical alias event is left alone, so that an alias later deleted from the directory does
/// not make every subsequent edit of the event fail until somebody removes it.
///
/// An alias on another server cannot be checked without asking that server, which this does not
/// do; it is accepted as given.
async fn check_canonical_alias<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &ruma::RoomId,
    handle: &crate::actor::RoomActorHandle<B>,
    content: &Value,
) -> Result<(), RoomError> {
    let named = named_aliases(content)?;
    if named.is_empty() {
        return Ok(());
    }
    let already: Vec<String> = handle
        .query(|actor| {
            let Some(content) = actor
                .state_event("m.room.canonical_alias", "")
                .ok()
                .flatten()
                .and_then(|event| event.json().get("content"))
                .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
            else {
                return Vec::new();
            };
            let alias = content
                .get("alias")
                .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
            let alt = content
                .get("alt_aliases")
                .and_then(hs_model::canonical::CanonicalJsonValue::as_array)
                .into_iter()
                .flatten()
                .filter_map(hs_model::canonical::CanonicalJsonValue::as_str);
            alias.into_iter().chain(alt).map(str::to_owned).collect()
        })
        .await;

    for raw in named.iter().filter(|alias| !already.contains(alias)) {
        let alias = ruma::RoomAliasId::parse(raw)
            .map_err(|_| RoomError::InvalidParam(format!("{raw:?} is not a valid room alias")))?;
        if alias.server_name() != state.auth.server_name() {
            continue;
        }
        match state.rooms.resolve_alias(&alias)? {
            Some(target) if target == room_id => {}
            Some(_) => {
                return Err(RoomError::BadAlias(format!(
                    "{alias} points to a different room"
                )));
            }
            None => return Err(RoomError::BadAlias(format!("{alias} does not exist"))),
        }
    }
    Ok(())
}

/// `PUT /rooms/{roomId}/state/{eventType}` (empty state key).
pub async fn put_state_no_key<B: KvBackend + 'static>(
    state: State<RoomState<B>>,
    Path((room_id, event_type)): Path<(String, String)>,
    requester: RoomRequester,
    body: PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    put_state(
        state,
        Path((room_id, event_type, String::new())),
        requester,
        body,
    )
    .await
}
