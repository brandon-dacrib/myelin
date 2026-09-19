//! `GET`/`PUT /presence/{userId}/status`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::UserId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

/// The presence values the spec defines. Anything else in a `PUT` body is `400 M_INVALID_PARAM`.
const VALID_PRESENCE: &[&str] = &["online", "offline", "unavailable"];

fn parse_user_id(raw: &str) -> Result<ruma::OwnedUserId, UserError> {
    UserId::parse(raw)
        .map(|u| u.to_owned())
        .map_err(|e| UserError::InvalidId(e.to_string()))
}

/// `GET /presence/{userId}/status`: any authenticated user may query, per the spec's own default
/// (a server *may* restrict this further -- `docs/status/05-sync.md` records not doing so as a
/// deliberate scope cut, not an oversight). A user this server has never heard a presence update
/// from at all gets the spec's own implied default (`"offline"`, no `status_msg`, no
/// `last_active_ago`) rather than `404` -- `404` is reserved for a `userId` that does not even
/// parse as one of *this* server's own users would be a stretch this crate has no cheap way to
/// check (see the module docs on why this crate does not depend on `hs-auth`'s user table for
/// this).
///
/// # Errors
/// Returns [`UserError`] if `userId` does not parse.
pub async fn get_status<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path(user_id): Path<String>,
    UserRequester(_requester): UserRequester,
) -> Result<Response, UserError> {
    let uid = parse_user_id(&user_id)?;
    let body = match state.hub.presence_of(&uid).await {
        Some(record) => {
            let mut body = json!({
                "presence": record.presence,
                "last_active_ago": record.last_active_ago_ms(),
            });
            if let Some(msg) = record.status_msg {
                body["status_msg"] = Value::String(msg);
            }
            body
        }
        None => json!({"presence": "offline"}),
    };
    Ok(Json(body).into_response())
}

/// `PUT /presence/{userId}/status`'s body.
#[derive(Debug, Deserialize)]
pub struct PresenceBody {
    /// `"online"`, `"offline"` or `"unavailable"`.
    pub presence: String,
    /// An optional free-text status message.
    #[serde(default)]
    pub status_msg: Option<String>,
}

/// `PUT /presence/{userId}/status`: a user may only set their own presence.
///
/// # Errors
/// Returns [`UserError`] if `userId` does not parse, is not the requester, or `presence` is not
/// one of the spec's three values.
pub async fn put_status<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path(user_id): Path<String>,
    UserRequester(requester): UserRequester,
    Json(body): Json<PresenceBody>,
) -> Result<Response, UserError> {
    let uid = parse_user_id(&user_id)?;
    if uid != requester.user_id {
        return Err(UserError::NotSelf(
            "cannot set another user's presence".to_owned(),
        ));
    }
    if !VALID_PRESENCE.contains(&body.presence.as_str()) {
        return Err(UserError::InvalidParam(format!(
            "invalid presence value: {:?}",
            body.presence
        )));
    }
    state
        .hub
        .set_presence(&requester.user_id, body.presence, body.status_msg)
        .await?;
    Ok(Json(json!({})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::SessionHub;
    use crate::room_source::test_support::registry;
    use crate::store::tables::TablesUserStore;
    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;
    use std::sync::Arc;

    fn test_state() -> UserState<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>
    {
        let store: crate::store::DynUserStore =
            Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        let e2e: Arc<dyn hs_e2e::store::E2eStore> =
            Arc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        let hub = Arc::new(SessionHub::new(
            store,
            registry("presence.test"),
            usize::MAX,
        ));
        UserState {
            auth: AuthState::in_memory(),
            hub,
            e2e,
        }
    }

    #[tokio::test]
    async fn a_never_seen_user_defaults_to_offline() {
        let state = test_state();
        let alice = user_id!("@alice:presence.test");
        let response = get_status(
            State(state),
            Path(alice.to_string()),
            UserRequester(Requester::for_user(alice.to_owned())),
        )
        .await
        .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["presence"], "offline");
        assert!(json.get("status_msg").is_none());
    }

    #[tokio::test]
    async fn put_then_get_round_trips_presence_and_status_msg() {
        let state = test_state();
        let alice = user_id!("@alice:presence.test");
        put_status(
            State(state.clone()),
            Path(alice.to_string()),
            UserRequester(Requester::for_user(alice.to_owned())),
            Json(PresenceBody {
                presence: "unavailable".to_owned(),
                status_msg: Some("brb".to_owned()),
            }),
        )
        .await
        .unwrap();

        let response = get_status(
            State(state),
            Path(alice.to_string()),
            UserRequester(Requester::for_user(alice.to_owned())),
        )
        .await
        .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["presence"], "unavailable");
        assert_eq!(json["status_msg"], "brb");
        assert!(json["last_active_ago"].as_u64().unwrap() < 5000);
    }

    #[tokio::test]
    async fn setting_someone_elses_presence_is_forbidden() {
        let state = test_state();
        let alice = user_id!("@alice:presence.test");
        let bob = user_id!("@bob:presence.test");
        let err = put_status(
            State(state),
            Path(bob.to_string()),
            UserRequester(Requester::for_user(alice.to_owned())),
            Json(PresenceBody {
                presence: "online".to_owned(),
                status_msg: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.into_response().status(),
            axum::http::StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn an_invalid_presence_value_is_rejected() {
        let state = test_state();
        let alice = user_id!("@alice:presence.test");
        let err = put_status(
            State(state),
            Path(alice.to_string()),
            UserRequester(Requester::for_user(alice.to_owned())),
            Json(PresenceBody {
                presence: "extremely-online".to_owned(),
                status_msg: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.into_response().status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }
}
