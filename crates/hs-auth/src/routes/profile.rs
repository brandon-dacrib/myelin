//! `GET`/`PUT /profile/{userId}/displayname`, `GET`/`PUT /profile/{userId}/avatar_url`, and the
//! combined `GET /profile/{userId}`.
//!
//! Spec rules this module enforces (client-server API, "Profiles"):
//!
//! - A `GET` is unauthenticated (no [`Requester`] extraction at all): any caller, logged in or
//!   not, can look up any user's public profile.
//! - A `PUT` requires authentication, and only the profile's own owner may write it -- a `PUT`
//!   for a different `userId` is `403 M_FORBIDDEN`, not merely ignored.
//! - A `GET` for a `userId` this server has never heard of is `404 M_NOT_FOUND`.
//! - A field that was never set is **omitted** from the response object entirely, never sent as
//!   an explicit `null` (`serde_json`'s `Value` construction below only inserts a key when the
//!   stored value is `Some`).
//!
//! This server only serves local users' profiles here (no federation profile-query fallback for
//! a remote `userId`; that would be track 06's `query/profile` federation client call, out of
//! scope for this crate). A `PUT` for a remote user id is also `M_FORBIDDEN` since it can never
//! equal the local requester's own id.
//!
//! # This module owns the storage write; `hs-room` owns mounting the `PUT`s
//!
//! `put_displayname`/`put_avatar_url` below are the actual `UserRecord` write and every
//! validation rule around it (self-only, 404 for an unknown user, body shape) -- that part still
//! lives here, since profile data is account data keyed by user id, not room state, the same
//! shape as `/account/whoami` and the device endpoints this crate already owns. But **this
//! crate's own router no longer calls these two functions**: a `PUT` to either path is served by
//! `hs_room::routes::profile`, which calls straight back into the two functions below for the
//! write and then re-stamps the user's `m.room.member` event in every room they are joined to --
//! the fan-out `hs-auth` cannot do itself, since it cannot depend on `hs-room` (the reverse
//! already holds: `hs-room` depends on `hs-auth` for exactly this store, plus reads it directly at
//! join/invite/knock time -- see `hs_room::routes::membership`'s module doc). `get_profile`/
//! `get_displayname`/`get_avatar_url` need no room context and are still routed from here
//! unchanged. See `hs_room::routes::profile`'s module doc for the full design, including why the
//! two routers can both mount paths under `/profile/{userId}/...` without colliding.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use ruma::UserId;
use serde_json::{Value, json};

use crate::error::MatrixError;
use crate::requester::Requester;
use crate::state::AuthState;
use crate::store::UserRecord;
use hs_http::body::PermissiveJson;

fn parse_user_id(raw: &str) -> Result<ruma::OwnedUserId, MatrixError> {
    UserId::parse(raw).map_err(|e| MatrixError::invalid_param(format!("invalid user_id: {e}")))
}

async fn load_user(state: &AuthState, user_id: &UserId) -> Result<UserRecord, MatrixError> {
    state
        .store
        .get_user(user_id)
        .await?
        .ok_or_else(|| MatrixError::not_found(format!("{user_id} not found")))
}

fn require_self(requester: &Requester, target: &UserId) -> Result<(), MatrixError> {
    if requester.user_id.as_str() == target.as_str() {
        Ok(())
    } else {
        Err(MatrixError::forbidden("Cannot set another user's profile"))
    }
}

/// `GET /profile/{userId}`: the combined profile document. Unauthenticated.
pub async fn get_profile(
    State(state): State<AuthState>,
    Path(user_id): Path<String>,
) -> Result<Response, MatrixError> {
    let uid = parse_user_id(&user_id)?;
    let record = load_user(&state, &uid).await?;
    let mut body = json!({});
    if let Some(name) = record.display_name {
        body["displayname"] = Value::String(name);
    }
    if let Some(avatar) = record.avatar_url {
        body["avatar_url"] = Value::String(avatar);
    }
    Ok(Json(body).into_response())
}

/// `GET /profile/{userId}/displayname`. Unauthenticated.
pub async fn get_displayname(
    State(state): State<AuthState>,
    Path(user_id): Path<String>,
) -> Result<Response, MatrixError> {
    let uid = parse_user_id(&user_id)?;
    let record = load_user(&state, &uid).await?;
    let mut body = json!({});
    if let Some(name) = record.display_name {
        body["displayname"] = Value::String(name);
    }
    Ok(Json(body).into_response())
}

/// `PUT /profile/{userId}/displayname`. Requires authentication as `userId` itself.
pub async fn put_displayname(
    State(state): State<AuthState>,
    Path(user_id): Path<String>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    let uid = parse_user_id(&user_id)?;
    require_self(&requester, &uid)?;
    // A `PUT` for a user id this server has never heard of (for example a deleted or never-real
    // account presented via a forged but well-formed token) is also worth a clear error rather
    // than silently creating profile data with no backing `UserRecord`; `set_profile_display_name`
    // already returns `NotFound` for that, converted below.
    let name = body
        .get("displayname")
        .and_then(Value::as_str)
        .map(str::to_owned);
    state
        .store
        .set_profile_display_name(&uid, name)
        .await
        .map_err(|e| match e {
            crate::store::StoreError::NotFound(msg) => MatrixError::not_found(msg),
            other => other.into(),
        })?;
    Ok(Json(json!({})).into_response())
}

/// `GET /profile/{userId}/avatar_url`. Unauthenticated.
pub async fn get_avatar_url(
    State(state): State<AuthState>,
    Path(user_id): Path<String>,
) -> Result<Response, MatrixError> {
    let uid = parse_user_id(&user_id)?;
    let record = load_user(&state, &uid).await?;
    let mut body = json!({});
    if let Some(avatar) = record.avatar_url {
        body["avatar_url"] = Value::String(avatar);
    }
    Ok(Json(body).into_response())
}

/// `PUT /profile/{userId}/avatar_url`. Requires authentication as `userId` itself.
pub async fn put_avatar_url(
    State(state): State<AuthState>,
    Path(user_id): Path<String>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    let uid = parse_user_id(&user_id)?;
    require_self(&requester, &uid)?;
    let avatar_url = body
        .get("avatar_url")
        .and_then(Value::as_str)
        .map(str::to_owned);
    state
        .store
        .set_profile_avatar_url(&uid, avatar_url)
        .await
        .map_err(|e| match e {
            crate::store::StoreError::NotFound(msg) => MatrixError::not_found(msg),
            other => other.into(),
        })?;
    Ok(Json(json!({})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use ruma::user_id;

    async fn state_with_user() -> AuthState {
        let state = AuthState::in_memory();
        state
            .store
            .create_user(UserRecord::new(
                user_id!("@alice:example.org").to_owned(),
                0,
            ))
            .await
            .unwrap();
        state
    }

    #[tokio::test]
    async fn get_profile_omits_unset_fields() {
        let state = state_with_user().await;
        let response = get_profile(State(state), Path("@alice:example.org".to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("displayname").is_none());
        assert!(json.get("avatar_url").is_none());
        assert_eq!(json, json!({}));
    }

    #[tokio::test]
    async fn get_profile_unknown_user_is_not_found() {
        let state = AuthState::in_memory();
        let err = get_profile(State(state), Path("@ghost:example.org".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.errcode().as_str(), "M_NOT_FOUND");
    }

    #[tokio::test]
    async fn put_displayname_then_get_round_trips() {
        let state = state_with_user().await;
        let requester = Requester::for_user(user_id!("@alice:example.org").to_owned());
        put_displayname(
            State(state.clone()),
            Path("@alice:example.org".to_string()),
            requester,
            PermissiveJson(json!({"displayname": "Alice"})),
        )
        .await
        .unwrap();

        let response = get_displayname(State(state), Path("@alice:example.org".to_string()))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["displayname"], "Alice");
    }

    #[tokio::test]
    async fn put_displayname_for_another_user_is_forbidden() {
        let state = state_with_user().await;
        state
            .store
            .create_user(UserRecord::new(user_id!("@bob:example.org").to_owned(), 1))
            .await
            .unwrap();
        let requester = Requester::for_user(user_id!("@bob:example.org").to_owned());
        let err = put_displayname(
            State(state),
            Path("@alice:example.org".to_string()),
            requester,
            PermissiveJson(json!({"displayname": "Not Alice"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.errcode().as_str(), "M_FORBIDDEN");
    }

    #[tokio::test]
    async fn put_avatar_url_then_get_round_trips() {
        let state = state_with_user().await;
        let requester = Requester::for_user(user_id!("@alice:example.org").to_owned());
        put_avatar_url(
            State(state.clone()),
            Path("@alice:example.org".to_string()),
            requester,
            PermissiveJson(json!({"avatar_url": "mxc://example.org/abc"})),
        )
        .await
        .unwrap();

        let response = get_avatar_url(State(state), Path("@alice:example.org".to_string()))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["avatar_url"], "mxc://example.org/abc");
    }

    #[tokio::test]
    async fn put_avatar_url_for_another_user_is_forbidden() {
        let state = state_with_user().await;
        state
            .store
            .create_user(UserRecord::new(user_id!("@bob:example.org").to_owned(), 1))
            .await
            .unwrap();
        let requester = Requester::for_user(user_id!("@bob:example.org").to_owned());
        let err = put_avatar_url(
            State(state),
            Path("@alice:example.org".to_string()),
            requester,
            PermissiveJson(json!({"avatar_url": "mxc://evil/x"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_displayname_unauthenticated_still_works_after_put() {
        // No Requester extraction happens in `get_displayname` at all -- this test's point is
        // that the handler signature takes no `Requester`, so there is nothing to fail even for
        // a request with no access token; compiling and passing without ever constructing a
        // `Requester` is the assertion.
        let state = state_with_user().await;
        state
            .store
            .set_profile_display_name(user_id!("@alice:example.org"), Some("Alice".to_string()))
            .await
            .unwrap();
        let response = get_displayname(State(state), Path("@alice:example.org".to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn combined_profile_reflects_both_fields() {
        let state = state_with_user().await;
        state
            .store
            .set_profile_display_name(user_id!("@alice:example.org"), Some("Alice".to_string()))
            .await
            .unwrap();
        state
            .store
            .set_profile_avatar_url(
                user_id!("@alice:example.org"),
                Some("mxc://example.org/abc".to_string()),
            )
            .await
            .unwrap();

        let response = get_profile(State(state), Path("@alice:example.org".to_string()))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["displayname"], "Alice");
        assert_eq!(json["avatar_url"], "mxc://example.org/abc");
    }
}
