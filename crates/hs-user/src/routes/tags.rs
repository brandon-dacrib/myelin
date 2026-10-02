//! Room tags: `GET /user/{userId}/rooms/{roomId}/tags` and
//! `PUT`/`DELETE /user/{userId}/rooms/{roomId}/tags/{tag}`.
//!
//! A tag is one key of the `tags` map in the user's `m.tag` room account data for that room;
//! these endpoints are that map seen one key at a time. Every change is written back as a whole
//! `m.tag` event through [`crate::store::UserStore::put_room_account_data`], so it bumps the
//! user's account-data counter and reaches `/sync` as any other room account data does
//! (`crate::sync`'s per-room `account_data.events`): a tag added or removed is the room's
//! `m.tag` carrying the whole current map, as the spec has it. Sytest's `42tags.pl` is the
//! reference behaviour.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use serde_json::{Map, Value, json};

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

/// The room account data type that holds a room's tags.
pub const TAG_EVENT_TYPE: &str = "m.tag";

fn require_self(
    requester: &hs_auth::requester::Requester,
    path_user_id: &str,
) -> Result<(), UserError> {
    if requester.user_id.as_str() != path_user_id {
        return Err(UserError::NotSelf(
            "cannot access another user's tags".to_owned(),
        ));
    }
    Ok(())
}

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, UserError> {
    ruma::RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| UserError::InvalidId(e.to_string()))
}

/// The `tags` map of `user_id`'s `m.tag` account data for `room_id`: empty when there is none,
/// or when what is stored has no object under `tags` (a client could have written any JSON
/// through `PUT .../account_data/m.tag`; this reads it as "no tags" rather than failing every
/// tag request from then on).
async fn current_tags<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    state: &UserState<B, R>,
    user_id: &ruma::UserId,
    room_id: &ruma::RoomId,
) -> Result<Map<String, Value>, UserError> {
    Ok(state
        .hub
        .store()
        .list_room_account_data(user_id, room_id)
        .await?
        .into_iter()
        .find(|a| a.event_type == TAG_EVENT_TYPE)
        .and_then(|a| match a.content {
            Value::Object(mut content) => match content.remove("tags") {
                Some(Value::Object(tags)) => Some(tags),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default())
}

async fn store_tags<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    state: &UserState<B, R>,
    user_id: &ruma::UserId,
    room_id: &ruma::RoomId,
    tags: Map<String, Value>,
) -> Result<(), UserError> {
    state
        .hub
        .store()
        .put_room_account_data(user_id, room_id, TAG_EVENT_TYPE, json!({ "tags": tags }))
        .await?;
    state.hub.account_data_changed(user_id).await;
    Ok(())
}

/// `GET /user/{userId}/rooms/{roomId}/tags`: `{"tags": {...}}`, empty when the room has none.
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the room id is invalid, or on a store
/// failure.
pub async fn get_tags<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, room_id)): Path<(String, String)>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let room_id = parse_room_id(&room_id)?;
    let tags = current_tags(&state, &requester.user_id, &room_id).await?;
    Ok(Json(json!({ "tags": tags })).into_response())
}

/// `PUT /user/{userId}/rooms/{roomId}/tags/{tag}`: sets one tag, its content being the body
/// (an object, usually `{}` or `{"order": 0.5}`). Replaces the tag's previous content, if any.
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the room id is invalid, the body is
/// not a JSON object, or on a store failure.
pub async fn put_tag<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, room_id, tag)): Path<(String, String, String)>,
    UserRequester(requester): UserRequester,
    PermissiveJson(content): PermissiveJson<Value>,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let room_id = parse_room_id(&room_id)?;
    if !content.is_object() {
        return Err(UserError::InvalidParam(
            "a tag's content must be a JSON object".to_owned(),
        ));
    }
    let mut tags = current_tags(&state, &requester.user_id, &room_id).await?;
    tags.insert(tag.clone(), content);
    store_tags(&state, &requester.user_id, &room_id, tags).await?;
    tracing::debug!(user_id = %requester.user_id, %room_id, %tag, "set a room tag");
    Ok(Json(json!({})).into_response())
}

/// `DELETE /user/{userId}/rooms/{roomId}/tags/{tag}`: removes one tag. Removing a tag the room
/// does not have succeeds and changes nothing (Sytest's "Can remove tag" removes one that was
/// never added).
///
/// # Errors
/// Returns [`UserError`] if `userId` is not the requester, the room id is invalid, or on a store
/// failure.
pub async fn delete_tag<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((user_id, room_id, tag)): Path<(String, String, String)>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    require_self(&requester, &user_id)?;
    let room_id = parse_room_id(&room_id)?;
    let mut tags = current_tags(&state, &requester.user_id, &room_id).await?;
    if tags.remove(&tag).is_some() {
        store_tags(&state, &requester.user_id, &room_id, tags).await?;
        tracing::debug!(user_id = %requester.user_id, %room_id, %tag, "removed a room tag");
    }
    Ok(Json(json!({})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::SessionHub;
    use crate::room_source::test_support::registry;
    use crate::store::tables::TablesUserStore;
    use axum::body::to_bytes;
    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::{room_id, user_id};
    use std::sync::Arc;

    type TestState = UserState<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>;

    fn test_state() -> TestState {
        let store: crate::store::DynUserStore =
            Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        let e2e: Arc<dyn hs_e2e::store::E2eStore> =
            Arc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        UserState {
            auth: AuthState::in_memory(),
            hub: Arc::new(SessionHub::new(store, registry("tags.test"), usize::MAX)),
            e2e,
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn list(state: &TestState, user: &ruma::UserId, room: &ruma::RoomId) -> Value {
        let response = get_tags(
            State(state.clone()),
            Path((user.to_string(), room.to_string())),
            UserRequester(Requester::for_user(user.to_owned())),
        )
        .await
        .unwrap();
        body_json(response).await["tags"].clone()
    }

    /// Sytest's `42tags.pl`, in one: a room starts with no tags, a tag added with content is
    /// listed with it, and removing it leaves the map empty -- with the `m.tag` account data
    /// (what `/sync` carries) agreeing at every step.
    #[tokio::test]
    async fn a_tag_is_listed_after_put_and_gone_after_delete() {
        let state = test_state();
        let alice = user_id!("@alice:tags.test");
        let room = room_id!("!room:tags.test");
        assert_eq!(list(&state, alice, room).await, json!({}));

        put_tag(
            State(state.clone()),
            Path((alice.to_string(), room.to_string(), "test_tag".to_owned())),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(json!({"order": 1})),
        )
        .await
        .unwrap();
        assert_eq!(
            list(&state, alice, room).await,
            json!({"test_tag": {"order": 1}})
        );
        let stored = state
            .hub
            .store()
            .list_room_account_data(alice, room)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].event_type, "m.tag");
        assert_eq!(
            stored[0].content,
            json!({"tags": {"test_tag": {"order": 1}}})
        );

        // A second tag joins the first; replacing one's content keeps the other.
        put_tag(
            State(state.clone()),
            Path((
                alice.to_string(),
                room.to_string(),
                "m.favourite".to_owned(),
            )),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        put_tag(
            State(state.clone()),
            Path((alice.to_string(), room.to_string(), "test_tag".to_owned())),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(json!({"order": 0.5})),
        )
        .await
        .unwrap();
        assert_eq!(
            list(&state, alice, room).await,
            json!({"test_tag": {"order": 0.5}, "m.favourite": {}})
        );

        delete_tag(
            State(state.clone()),
            Path((alice.to_string(), room.to_string(), "test_tag".to_owned())),
            UserRequester(Requester::for_user(alice.to_owned())),
        )
        .await
        .unwrap();
        assert_eq!(list(&state, alice, room).await, json!({"m.favourite": {}}));
        delete_tag(
            State(state.clone()),
            Path((
                alice.to_string(),
                room.to_string(),
                "m.favourite".to_owned(),
            )),
            UserRequester(Requester::for_user(alice.to_owned())),
        )
        .await
        .unwrap();
        assert_eq!(list(&state, alice, room).await, json!({}));
        let stored = state
            .hub
            .store()
            .list_room_account_data(alice, room)
            .await
            .unwrap();
        assert_eq!(
            stored[0].content,
            json!({"tags": {}}),
            "an empty m.tag remains"
        );
    }

    /// Removing a tag that was never set is not an error (Sytest's "Can remove tag" does exactly
    /// that on a fresh room), and writes nothing.
    #[tokio::test]
    async fn deleting_an_absent_tag_succeeds_and_writes_nothing() {
        let state = test_state();
        let alice = user_id!("@alice:tags.test");
        let room = room_id!("!room:tags.test");
        let response = delete_tag(
            State(state.clone()),
            Path((alice.to_string(), room.to_string(), "test_tag".to_owned())),
            UserRequester(Requester::for_user(alice.to_owned())),
        )
        .await
        .unwrap();
        assert_eq!(body_json(response).await, json!({}));
        assert!(
            state
                .hub
                .store()
                .list_room_account_data(alice, room)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A tag's content is an object; anything else is rejected, and another user's tags are not
    /// reachable through one's own token.
    #[tokio::test]
    async fn bad_content_and_other_users_are_rejected() {
        let state = test_state();
        let alice = user_id!("@alice:tags.test");
        let bob = user_id!("@bob:tags.test");
        let room = room_id!("!room:tags.test");
        let err = put_tag(
            State(state.clone()),
            Path((alice.to_string(), room.to_string(), "test_tag".to_owned())),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(json!("not an object")),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, UserError::InvalidParam(_)), "{err:?}");
        let err = get_tags(
            State(state.clone()),
            Path((bob.to_string(), room.to_string())),
            UserRequester(Requester::for_user(alice.to_owned())),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, UserError::NotSelf(_)), "{err:?}");
    }

    /// Setting a tag wakes the user's long-polling `/sync`, as any account-data write does: a
    /// registered waker fires.
    #[tokio::test]
    async fn setting_a_tag_wakes_the_users_sync() {
        let state = test_state();
        let alice = user_id!("@alice:tags.test");
        let room = room_id!("!room:tags.test");
        let waker = state.hub.waker(alice).await;
        let notified = waker.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        put_tag(
            State(state.clone()),
            Path((alice.to_string(), room.to_string(), "test_tag".to_owned())),
            UserRequester(Requester::for_user(alice.to_owned())),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .expect("the tag write woke the sync");
    }
}
