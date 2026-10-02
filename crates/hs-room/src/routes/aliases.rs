//! `GET /rooms/{roomId}/aliases`, `PUT`/`GET`/`DELETE /directory/room/{roomAlias}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{RoomAliasId, RoomId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::state::{RoomRequester, RoomState};

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

fn parse_alias(raw: &str) -> Result<ruma::OwnedRoomAliasId, RoomError> {
    RoomAliasId::parse(raw)
        .map(|a| a.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// `GET /rooms/{roomId}/aliases`.
pub async fn get_room_aliases<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let aliases = handle
        .query(move |actor| {
            if !actor.can_see_current_membership(&requester.user_id)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            actor.list_aliases()
        })
        .await?;
    Ok(Json(json!({"aliases": aliases})).into_response())
}

/// `PUT /directory/room/{roomAlias}`.
pub async fn put_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_alias): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    crate::moderation::refuse_if_suspended(&requester)?;
    let alias = parse_alias(&room_alias)?;
    let room_id_str = body
        .get("room_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RoomError::BadRequest("missing room_id".into()))?;
    let room_id = parse_room_id(room_id_str)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let creator = requester.user_id.clone();
    handle
        .query(move |actor| actor.create_alias(&alias, &creator))
        .await?;
    Ok(Json(json!({})).into_response())
}

/// `GET /directory/room/{roomAlias}`.
///
/// An alias of another server is asked of that server (`GET
/// /_matrix/federation/v1/query/directory`, through [`crate::remote_join::RemoteJoin::resolve_alias`]),
/// and its answer -- the room and the servers it names -- is passed through. Without a remote
/// resolver (federation off) such an alias is `404 M_NOT_FOUND`, as an unknown local one is.
pub async fn get_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_alias): Path<String>,
) -> Result<Response, RoomError> {
    let alias = parse_alias(&room_alias)?;
    if alias.server_name() != &*state.identity.server_name {
        let Some(remote) = state.remote_join.as_ref() else {
            tracing::info!(%alias, "a remote alias was asked for, and federation is off");
            return Err(RoomError::RoomNotFound(room_alias));
        };
        let (room_id, servers) = remote.resolve_alias(&alias).await?;
        tracing::debug!(%alias, %room_id, "resolved an alias through its server");
        return Ok(
            Json(json!({"room_id": room_id.to_string(), "servers": servers})).into_response(),
        );
    }
    let room_id = state
        .rooms
        .resolve_alias(&alias)?
        .ok_or_else(|| RoomError::RoomNotFound(room_alias.clone()))?;
    Ok(Json(json!({"room_id": room_id.to_string(), "servers": [state.identity.server_name.to_string()]})).into_response())
}

/// `DELETE /directory/room/{roomAlias}`.
///
/// Anyone could delete anyone's alias before this: the requester was extracted and dropped. The
/// rule the spec allows and every other server implements is that you may remove an alias you
/// created, or one in a room where you have the power to set `m.room.canonical_alias` -- a
/// moderator tidying up after somebody, not a passer-by unpicking a room's address.
pub async fn delete_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_alias): Path<String>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    crate::moderation::refuse_if_suspended(&requester)?;
    let alias = parse_alias(&room_alias)?;
    let room_id = state
        .rooms
        .resolve_alias(&alias)?
        .ok_or_else(|| RoomError::RoomNotFound(room_alias.clone()))?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let user_id = requester.user_id.clone();
    let check_alias = alias.clone();
    let allowed = handle
        .query(move |actor| {
            let created_it = actor
                .alias_creator(&check_alias)?
                .is_some_and(|creator| creator == user_id);
            if created_it {
                return Ok::<bool, RoomError>(true);
            }
            actor.can_send_state(&user_id, "m.room.canonical_alias")
        })
        .await?;
    if !allowed {
        return Err(RoomError::Forbidden(format!(
            "{} was not created by you, and you do not have permission to remove it",
            alias.as_str()
        )));
    }
    handle
        .query(move |actor| actor.remove_alias(&alias))
        .await?;
    Ok(Json(json!({})).into_response())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use hs_auth::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::{OwnedRoomId, RoomAliasId, RoomId, UserId};

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;

    /// Stands in for the other server: records which aliases it was asked about and answers as
    /// its `/query/directory` would.
    #[derive(Default)]
    struct RecordingRemoteJoin {
        resolved: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl crate::remote_join::RemoteJoin for RecordingRemoteJoin {
        async fn join(
            &self,
            _user_id: &UserId,
            room_id: &RoomId,
            _via: &[String],
            _content: Value,
        ) -> Result<OwnedRoomId, RoomError> {
            Ok(room_id.to_owned())
        }

        async fn resolve_alias(
            &self,
            alias: &RoomAliasId,
        ) -> Result<(OwnedRoomId, Vec<String>), RoomError> {
            self.resolved.lock().unwrap().push(alias.to_string());
            if alias.alias() == "nowhere" {
                return Err(RoomError::RoomNotFound(alias.to_string()));
            }
            Ok((
                RoomId::parse("!the-room-id:remote.example:8448")
                    .unwrap()
                    .to_owned(),
                vec!["remote.example:8448".to_owned()],
            ))
        }
    }

    fn state(remote: Option<Arc<RecordingRemoteJoin>>) -> RoomState<MemoryBackend> {
        let identity = HomeserverIdentity::for_tests("hs1");
        let rooms = Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).unwrap());
        RoomState {
            auth: AuthState::in_memory(),
            rooms,
            identity,
            remote_join: remote.map(|r| r as Arc<dyn crate::remote_join::RemoteJoin>),
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Sytest's "Outbound federation can query room alias directory" and "Remote room alias
    /// queries can handle Unicode": an alias of another server is asked of that server, and its
    /// answer is passed through.
    #[tokio::test]
    async fn an_alias_of_another_server_is_resolved_by_that_server() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        let state = state(Some(remote.clone()));

        let response = get_alias(
            State(state.clone()),
            Path("#test:remote.example:8448".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(
            body_json(response).await,
            json!({"room_id": "!the-room-id:remote.example:8448", "servers": ["remote.example:8448"]})
        );
        let response = get_alias(
            State(state.clone()),
            Path("#☕:remote.example:8448".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(
            body_json(response).await["room_id"],
            "!the-room-id:remote.example:8448"
        );
        // The other server's "no such alias" is this server's.
        let err = get_alias(
            State(state.clone()),
            Path("#nowhere:remote.example:8448".to_owned()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err:?}");
        // A local alias is never asked of another server.
        let err = get_alias(State(state), Path("#local:hs1".to_owned()))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err:?}");
        assert_eq!(
            *remote.resolved.lock().unwrap(),
            vec![
                "#test:remote.example:8448",
                "#☕:remote.example:8448",
                "#nowhere:remote.example:8448"
            ]
        );
    }

    #[tokio::test]
    async fn a_remote_alias_without_federation_is_not_found() {
        let err = get_alias(State(state(None)), Path("#test:remote.example".to_owned()))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err:?}");
    }
}
