//! `PUT /rooms/{roomId}/typing/{userId}`.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{RoomId, UserId};
use serde::Deserialize;
use serde_json::json;

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

/// The default timeout Synapse itself falls back to if a client omits the field, even though the
/// spec marks it required -- matches real-world client behavior this server should tolerate
/// rather than reject.
const DEFAULT_TYPING_TIMEOUT_MS: u64 = 30_000;

/// `PUT /rooms/{roomId}/typing/{userId}`'s body.
#[derive(Debug, Deserialize)]
pub struct TypingBody {
    /// Whether the user is now typing.
    pub typing: bool,
    /// Milliseconds to honor `typing: true` for. Ignored (but accepted) when `typing` is `false`.
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, UserError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| UserError::InvalidId(e.to_string()))
}

fn parse_user_id(raw: &str) -> Result<ruma::OwnedUserId, UserError> {
    UserId::parse(raw)
        .map(|u| u.to_owned())
        .map_err(|e| UserError::InvalidId(e.to_string()))
}

/// `PUT /rooms/{roomId}/typing/{userId}`: a client may only set its own typing state, and only in
/// a room it currently has `join` membership in (a left, banned or merely-invited user has no
/// business telling a room's other members it is typing).
///
/// # Errors
/// Returns [`UserError`] if `roomId`/`userId` do not parse, `userId` is not the requester, the
/// requester is not a joined member of the room, or on a room-load failure.
pub async fn put_typing<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((room_id, user_id)): Path<(String, String)>,
    UserRequester(requester): UserRequester,
    PermissiveJson(body): PermissiveJson<TypingBody>,
) -> Result<Response, UserError> {
    let room_id = parse_room_id(&room_id)?;
    let target = parse_user_id(&user_id)?;
    if target != requester.user_id {
        return Err(UserError::NotSelf(
            "cannot set another user's typing state".to_owned(),
        ));
    }
    let membership = state
        .hub
        .store()
        .get_membership(&requester.user_id, &room_id)
        .await?;
    if !matches!(
        membership.as_ref().map(|m| m.membership.as_str()),
        Some("join")
    ) {
        return Err(UserError::Forbidden(
            "must be a joined member of the room to set a typing notification".to_owned(),
        ));
    }

    let timeout = Duration::from_millis(body.timeout.unwrap_or(DEFAULT_TYPING_TIMEOUT_MS));
    state
        .hub
        .set_typing(&room_id, &requester.user_id, body.typing, timeout)
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
    use hs_room::actor::{CreateRoomRequest, RoomActorHandle};
    use ruma::user_id;
    use std::sync::Arc;
    use std::time::Duration;

    async fn test_state() -> (
        UserState<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>,
        ruma::OwnedRoomId,
    ) {
        let store: crate::store::DynUserStore =
            Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        let e2e: Arc<dyn hs_e2e::store::E2eStore> =
            Arc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        let hub = Arc::new(SessionHub::new(store, registry("typing.test"), usize::MAX));
        let alice = user_id!("@alice:typing.test").to_owned();
        let handle: RoomActorHandle<MemoryBackend> = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .expect("create room");
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        hub.watch_room(handle.clone()).await;
        // The room-creation events themselves were published to the broadcast channel *before*
        // `watch_room` subscribed to it, so this hub never saw them (`crate::hub::SessionHub`'s
        // module docs, "The discovery gap"); a follow-up event is what actually gets the
        // creator's own `join` membership recorded, matching `crate::hub::tests`'s identical
        // pattern for the identical reason.
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hi"}),
                None,
                2,
            )
            .await
            .expect("seed event");
        tokio::time::sleep(Duration::from_millis(30)).await;
        (
            UserState {
                auth: AuthState::in_memory(),
                hub,
                e2e,
            },
            room_id,
        )
    }

    #[tokio::test]
    async fn a_joined_member_can_set_their_own_typing_state() {
        let (state, room_id) = test_state().await;
        let alice = user_id!("@alice:typing.test");
        let response = put_typing(
            State(state.clone()),
            Path((room_id.to_string(), alice.to_string())),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(TypingBody {
                typing: true,
                timeout: Some(30_000),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let (users, _) = state.hub.typing_users(&room_id).await;
        assert_eq!(users, vec![alice.to_owned()]);
    }

    #[tokio::test]
    async fn setting_someone_elses_typing_state_is_forbidden() {
        let (state, room_id) = test_state().await;
        let alice = user_id!("@alice:typing.test");
        let bob = user_id!("@bob:typing.test");
        let err = put_typing(
            State(state),
            Path((room_id.to_string(), bob.to_string())),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(TypingBody {
                typing: true,
                timeout: None,
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
    async fn a_non_member_cannot_set_typing_state() {
        let (state, room_id) = test_state().await;
        let carol = user_id!("@carol:typing.test");
        let err = put_typing(
            State(state),
            Path((room_id.to_string(), carol.to_string())),
            UserRequester(Requester::for_user(carol.to_owned())),
            PermissiveJson(TypingBody {
                typing: true,
                timeout: None,
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
    async fn invalid_room_id_is_a_bad_request() {
        let (state, _room_id) = test_state().await;
        let alice = user_id!("@alice:typing.test");
        let err = put_typing(
            State(state),
            Path(("not-a-room-id".to_string(), alice.to_string())),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(TypingBody {
                typing: true,
                timeout: None,
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
