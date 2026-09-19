//! `/room_keys/version*` and `/room_keys/keys*`: key backups.
//!
//! The spec's `etag` is this crate's [`crate::store::BackupVersionRow::etag`] counter's decimal
//! string, and `count` is [`crate::store::BackupVersionRow::count`] — both bumped inside the same
//! transaction as the session write or delete that changes them
//! ([`crate::store::tables::TablesE2eStore::put_session`] and friends), so a client's poll-for-
//! divergence loop (compare `etag`, refetch if changed) can never observe a `count` that has not
//! caught up with the `etag` it just read, or vice versa.

use axum::Json;
use axum::extract::{Path, Query, State};
use hs_kv::KvBackend;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};
use crate::store::{BackupSessionRow, BackupVersionRow};

#[derive(Debug, Deserialize, Default)]
pub struct VersionQuery {
    version: Option<String>,
}

fn version_json(number: u64, row: &BackupVersionRow) -> Value {
    json!({
        "version": number.to_string(),
        "algorithm": row.algorithm,
        "auth_data": row.auth_data,
        "etag": row.etag.to_string(),
        "count": row.count,
    })
}

fn session_json(row: &BackupSessionRow) -> Value {
    json!({
        "first_message_index": row.first_message_index,
        "forwarded_count": row.forwarded_count,
        "is_verified": row.is_verified,
        "session_data": row.session_data,
    })
}

fn session_from_json(value: &Value) -> Result<BackupSessionRow, E2eError> {
    let obj = value
        .as_object()
        .ok_or_else(|| E2eError::BadRequest("session data must be an object".to_string()))?;
    Ok(BackupSessionRow {
        first_message_index: obj
            .get("first_message_index")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        forwarded_count: obj
            .get("forwarded_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        is_verified: obj
            .get("is_verified")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        session_data: obj
            .get("session_data")
            .cloned()
            .ok_or_else(|| E2eError::BadRequest("missing session_data".to_string()))?,
    })
}

fn parse_version_param(raw: &str) -> Result<u64, E2eError> {
    raw.parse::<u64>()
        .map_err(|_| E2eError::BadRequest(format!("not a valid backup version: {raw:?}")))
}

/// Resolves `?version=` for a **write** endpoint: absent means "the current version", present
/// means it must equal the current version exactly (`M_WRONG_ROOM_KEYS_VERSION` otherwise, per
/// the spec — a client that has fallen behind another device's newer backup version must not be
/// allowed to silently keep writing to a stale one).
async fn resolve_write_version<B: KvBackend + 'static>(
    state: &E2eState<B>,
    user_id: &ruma::UserId,
    requested: Option<&str>,
) -> Result<u64, E2eError> {
    let (current, _row) = state
        .store
        .get_version(user_id, None)
        .await?
        .ok_or_else(|| E2eError::NotFound("no current key backup version".to_string()))?;
    match requested {
        None => Ok(current),
        Some(raw) => {
            let requested = parse_version_param(raw)?;
            if requested == current {
                Ok(current)
            } else {
                Err(E2eError::wrong_backup_version(
                    requested.to_string(),
                    current.to_string(),
                ))
            }
        }
    }
}

/// Resolves `?version=` for a **read** endpoint: any existing, non-deleted version may be read,
/// not only the current one.
async fn resolve_read_version<B: KvBackend + 'static>(
    state: &E2eState<B>,
    user_id: &ruma::UserId,
    requested: Option<&str>,
) -> Result<u64, E2eError> {
    let version = match requested {
        Some(raw) => Some(parse_version_param(raw)?),
        None => None,
    };
    let (number, row) = state
        .store
        .get_version(user_id, version)
        .await?
        .ok_or_else(|| E2eError::NotFound("no such key backup version".to_string()))?;
    if row.deleted {
        return Err(E2eError::NotFound(
            "key backup version was deleted".to_string(),
        ));
    }
    Ok(number)
}

// -- version endpoints --------------------------------------------------------------------

/// `GET /room_keys/version` (no path segment: the current version).
pub async fn get_version_latest<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
) -> Result<Json<Value>, E2eError> {
    let (number, row) = state
        .store
        .get_version(&requester.user_id, None)
        .await?
        .filter(|(_, row)| !row.deleted)
        .ok_or_else(|| E2eError::NotFound("no current key backup version".to_string()))?;
    Ok(Json(version_json(number, &row)))
}

/// `GET /room_keys/version/{version}`.
pub async fn get_version<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path(version): Path<String>,
) -> Result<Json<Value>, E2eError> {
    let v = parse_version_param(&version)?;
    let (number, row) = state
        .store
        .get_version(&requester.user_id, Some(v))
        .await?
        .filter(|(_, row)| !row.deleted)
        .ok_or_else(|| E2eError::NotFound("no such key backup version".to_string()))?;
    Ok(Json(version_json(number, &row)))
}

#[derive(Debug, Deserialize)]
pub struct CreateVersionBody {
    algorithm: String,
    auth_data: Value,
}

/// `POST /room_keys/version`.
pub async fn post_version<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Json(body): Json<CreateVersionBody>,
) -> Result<Json<Value>, E2eError> {
    let version = state
        .store
        .create_version(&requester.user_id, body.algorithm, body.auth_data)
        .await?;
    Ok(Json(json!({ "version": version.to_string() })))
}

#[derive(Debug, Deserialize)]
pub struct UpdateVersionBody {
    auth_data: Value,
    #[allow(
        dead_code,
        reason = "accepted for spec compatibility, not currently cross-checked"
    )]
    algorithm: Option<String>,
    version: Option<String>,
}

/// `PUT /room_keys/version/{version}`.
pub async fn put_version<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path(version): Path<String>,
    Json(body): Json<UpdateVersionBody>,
) -> Result<Json<Value>, E2eError> {
    if let Some(body_version) = &body.version
        && body_version != &version
    {
        return Err(E2eError::BadRequest(
            "body version does not match the path".to_string(),
        ));
    }
    let v = parse_version_param(&version)?;
    state
        .store
        .update_version_auth_data(&requester.user_id, v, body.auth_data)
        .await?;
    Ok(Json(json!({})))
}

/// `DELETE /room_keys/version/{version}`.
pub async fn delete_version<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path(version): Path<String>,
) -> Result<Json<Value>, E2eError> {
    let v = parse_version_param(&version)?;
    state.store.delete_version(&requester.user_id, v).await?;
    Ok(Json(json!({})))
}

// -- key endpoints -------------------------------------------------------------------------

fn rooms_wrapper(
    sessions_by_room: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, BackupSessionRow>,
    >,
) -> Value {
    let mut rooms = Map::new();
    for (room_id, sessions) in sessions_by_room {
        let mut sessions_json = Map::new();
        for (session_id, row) in sessions {
            sessions_json.insert(session_id, session_json(&row));
        }
        rooms.insert(room_id, json!({ "sessions": sessions_json }));
    }
    json!({ "rooms": rooms })
}

/// `GET /room_keys/keys`.
pub async fn get_keys_all<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Query(q): Query<VersionQuery>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_read_version(&state, &requester.user_id, q.version.as_deref()).await?;
    let sessions = state
        .store
        .get_all_sessions(&requester.user_id, version)
        .await?;
    Ok(Json(rooms_wrapper(sessions)))
}

/// `GET /room_keys/keys/{roomId}`.
pub async fn get_keys_room<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path(room_id): Path<String>,
    Query(q): Query<VersionQuery>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_read_version(&state, &requester.user_id, q.version.as_deref()).await?;
    let sessions = state
        .store
        .get_room_sessions(&requester.user_id, version, &room_id)
        .await?;
    let mut sessions_json = Map::new();
    for (session_id, row) in sessions {
        sessions_json.insert(session_id, session_json(&row));
    }
    Ok(Json(json!({ "sessions": sessions_json })))
}

/// `GET /room_keys/keys/{roomId}/{sessionId}`.
pub async fn get_keys_session<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path((room_id, session_id)): Path<(String, String)>,
    Query(q): Query<VersionQuery>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_read_version(&state, &requester.user_id, q.version.as_deref()).await?;
    let row = state
        .store
        .get_session(&requester.user_id, version, &room_id, &session_id)
        .await?
        .ok_or_else(|| E2eError::NotFound("no such session in this backup".to_string()))?;
    Ok(Json(session_json(&row)))
}

async fn write_status<B: KvBackend + 'static>(
    state: &E2eState<B>,
    user_id: &ruma::UserId,
    version: u64,
) -> Result<Value, E2eError> {
    let (_n, row) = state
        .store
        .get_version(user_id, Some(version))
        .await?
        .ok_or_else(|| E2eError::NotFound("no such key backup version".to_string()))?;
    Ok(json!({ "count": row.count, "etag": row.etag.to_string() }))
}

/// `PUT /room_keys/keys`: `{"rooms": {"<roomId>": {"sessions": {"<sessionId>": <session>}}}}`.
pub async fn put_keys_all<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Query(q): Query<VersionQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_write_version(&state, &requester.user_id, q.version.as_deref()).await?;
    let rooms = body
        .get("rooms")
        .and_then(Value::as_object)
        .ok_or_else(|| E2eError::BadRequest("missing rooms".to_string()))?;
    for (room_id, room_body) in rooms {
        let sessions = room_body
            .get("sessions")
            .and_then(Value::as_object)
            .ok_or_else(|| E2eError::BadRequest("missing sessions".to_string()))?;
        for (session_id, session_body) in sessions {
            let row = session_from_json(session_body)?;
            state
                .store
                .put_session(&requester.user_id, version, room_id, session_id, row)
                .await?;
        }
    }
    Ok(Json(
        write_status(&state, &requester.user_id, version).await?,
    ))
}

/// `PUT /room_keys/keys/{roomId}`: `{"sessions": {"<sessionId>": <session>}}`.
pub async fn put_keys_room<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path(room_id): Path<String>,
    Query(q): Query<VersionQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_write_version(&state, &requester.user_id, q.version.as_deref()).await?;
    let sessions = body
        .get("sessions")
        .and_then(Value::as_object)
        .ok_or_else(|| E2eError::BadRequest("missing sessions".to_string()))?;
    for (session_id, session_body) in sessions {
        let row = session_from_json(session_body)?;
        state
            .store
            .put_session(&requester.user_id, version, &room_id, session_id, row)
            .await?;
    }
    Ok(Json(
        write_status(&state, &requester.user_id, version).await?,
    ))
}

/// `PUT /room_keys/keys/{roomId}/{sessionId}`: the session object directly.
pub async fn put_keys_session<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path((room_id, session_id)): Path<(String, String)>,
    Query(q): Query<VersionQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_write_version(&state, &requester.user_id, q.version.as_deref()).await?;
    let row = session_from_json(&body)?;
    state
        .store
        .put_session(&requester.user_id, version, &room_id, &session_id, row)
        .await?;
    Ok(Json(
        write_status(&state, &requester.user_id, version).await?,
    ))
}

/// `DELETE /room_keys/keys`.
pub async fn delete_keys_all<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Query(q): Query<VersionQuery>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_write_version(&state, &requester.user_id, q.version.as_deref()).await?;
    state
        .store
        .delete_all_sessions(&requester.user_id, version)
        .await?;
    Ok(Json(
        write_status(&state, &requester.user_id, version).await?,
    ))
}

/// `DELETE /room_keys/keys/{roomId}`.
pub async fn delete_keys_room<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path(room_id): Path<String>,
    Query(q): Query<VersionQuery>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_write_version(&state, &requester.user_id, q.version.as_deref()).await?;
    state
        .store
        .delete_room_sessions(&requester.user_id, version, &room_id)
        .await?;
    Ok(Json(
        write_status(&state, &requester.user_id, version).await?,
    ))
}

/// `DELETE /room_keys/keys/{roomId}/{sessionId}`.
pub async fn delete_keys_session<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path((room_id, session_id)): Path<(String, String)>,
    Query(q): Query<VersionQuery>,
) -> Result<Json<Value>, E2eError> {
    let version = resolve_write_version(&state, &requester.user_id, q.version.as_deref()).await?;
    state
        .store
        .delete_session(&requester.user_id, version, &room_id, &session_id)
        .await?;
    Ok(Json(
        write_status(&state, &requester.user_id, version).await?,
    ))
}
