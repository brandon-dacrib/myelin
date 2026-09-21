//! `POST /user/{userId}/filter`, `GET /user/{userId}/filter/{filterId}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use serde_json::json;

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

fn require_self(
    requester: &hs_auth::requester::Requester,
    path_user_id: &str,
) -> Result<(), UserError> {
    if requester.user_id.as_str() != path_user_id {
        return Err(UserError::NotSelf(
            "cannot manage another user's filters".to_owned(),
        ));
    }
    Ok(())
}

/// `POST /user/{userId}/filter`. Validates the body against [`crate::filter::SyncFilter`]'s shape
/// before storing it (an inline `filter` on `/sync` is validated the same way at resolution time
/// -- see `crate::filter::resolve`).
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the body does not parse as a filter,
/// or on a store failure.
pub async fn post_filter<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path(user_id): Path<String>,
    UserRequester(requester): UserRequester,
    PermissiveJson(body): PermissiveJson<serde_json::Value>,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let _validated: crate::filter::SyncFilter = serde_json::from_value(body.clone())
        .map_err(|e| UserError::InvalidFilter(e.to_string()))?;
    let filter_id = state
        .hub
        .store()
        .put_filter(&requester.user_id, body)
        .await?;
    Ok(Json(json!({"filter_id": filter_id})).into_response())
}

/// `GET /user/{userId}/filter/{filterId}`.
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the filter id is unknown, or on a
/// store failure.
pub async fn get_filter<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, filter_id)): Path<(String, String)>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let body = state
        .hub
        .store()
        .get_filter(&requester.user_id, &filter_id)
        .await?
        .ok_or_else(|| UserError::UnknownFilterId(filter_id.clone()))?;
    Ok(Json(body).into_response())
}
