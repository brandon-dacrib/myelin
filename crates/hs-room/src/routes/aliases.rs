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

/// A local alias from the directory, or, when the directory does not hold it, from the
/// appservice whose alias namespace covers it: asked (`GET /_matrix/app/v1/rooms/{roomAlias}`,
/// through `hs-auth`'s `AppserviceRegistry::query_room_alias`, which `hs-appservice` answers),
/// it creates the room and the alias, and the directory is read again (Sytest's "Accesing an
/// AS-hosted room alias asks the AS server").
pub(crate) async fn resolve_local_alias<B: KvBackend + 'static>(
    state: &RoomState<B>,
    alias: &RoomAliasId,
) -> Result<Option<ruma::OwnedRoomId>, RoomError> {
    if let Some(room_id) = state.rooms.resolve_alias(alias)? {
        return Ok(Some(room_id));
    }
    if alias.server_name() != &*state.identity.server_name
        || !state
            .auth
            .appservices
            .query_room_alias(alias.as_str())
            .await
    {
        return Ok(None);
    }
    state.rooms.resolve_alias(alias)
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
    // An alias an appservice holds exclusively is its own to create (`400 M_EXCLUSIVE`).
    if let Some(owner) = state
        .auth
        .appservices
        .exclusive_alias_owner(alias.as_str())
        .await
        && requester
            .appservice
            .as_ref()
            .map(|a| a.appservice_id.as_str())
            != Some(owner.as_str())
    {
        tracing::info!(%alias, appservice = %owner, "refused an alias in an appservice's exclusive namespace");
        return Err(RoomError::Exclusive(format!(
            "{alias} is reserved by an application service"
        )));
    }
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
    let room_id = resolve_local_alias(&state, &alias)
        .await?
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
    let removed = alias.clone();
    handle
        .query(move |actor| actor.remove_alias(&removed))
        .await?;
    drop_from_canonical_alias(&handle, &requester.user_id, &alias).await;
    Ok(Json(json!({})).into_response())
}

/// `content` of an `m.room.canonical_alias` event with `alias` taken out of `alias` and
/// `alt_aliases` (an emptied `alt_aliases` goes too), or `None` when it names `alias` nowhere.
fn without_alias(content: &Value, alias: &str) -> Option<Value> {
    let mut content = content.as_object()?.clone();
    let mut changed = false;
    if content.get("alias").and_then(Value::as_str) == Some(alias) {
        content.remove("alias");
        changed = true;
    }
    if let Some(Value::Array(alt)) = content.get_mut("alt_aliases") {
        let before = alt.len();
        alt.retain(|a| a.as_str() != Some(alias));
        changed |= alt.len() != before;
        if alt.is_empty() {
            content.remove("alt_aliases");
        }
    }
    changed.then_some(Value::Object(content))
}

/// After `alias` is deleted from the directory, sends the room a new `m.room.canonical_alias`
/// without it, as `user_id`, when the current one names it -- so the room stops advertising an
/// address that leads nowhere (Synapse's `_update_canonical_alias`; Sytest's and Complement's
/// "Can delete canonical alias", which wait for an `m.room.canonical_alias` with empty
/// content). Best effort, as Synapse's is: a deleter who may delete the alias but not send the
/// state event (an alias creator without power) leaves the event as it was, logged.
async fn drop_from_canonical_alias<B: KvBackend + 'static>(
    handle: &crate::actor::RoomActorHandle<B>,
    user_id: &ruma::UserId,
    alias: &ruma::RoomAliasId,
) {
    let current = handle
        .query(|actor| {
            actor
                .state_event("m.room.canonical_alias", "")
                .ok()
                .flatten()
                .and_then(|event| event.json().get("content"))
                .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                .map(crate::routes::render::canonical_to_json)
        })
        .await;
    let Some(content) = current.and_then(|c| without_alias(&c, alias.as_str())) else {
        return;
    };
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    match handle
        .send_event(
            user_id.to_owned(),
            "m.room.canonical_alias".to_owned(),
            Some(String::new()),
            content,
            None,
            now_ms,
        )
        .await
    {
        Ok(event) => tracing::info!(
            alias = %alias,
            event_id = %event.event_id(),
            "a deleted alias was taken out of the room's canonical alias"
        ),
        Err(error) => tracing::info!(
            alias = %alias,
            %error,
            "a deleted alias is still in the room's canonical alias: its deleter may not change it"
        ),
    }
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

    /// An appservice that holds `#irc_*:hs1` exclusively, and provides `#irc_new:hs1` (on
    /// `room`) when it is asked about it.
    struct IrcBridge {
        rooms: Arc<RoomRegistry<MemoryBackend>>,
        room: Mutex<Option<OwnedRoomId>>,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl hs_auth::appservice::AppserviceRegistry for IrcBridge {
        async fn lookup_by_token(
            &self,
            _token: &str,
        ) -> Option<hs_auth::appservice::AppserviceRecord> {
            None
        }

        async fn exclusive_alias_owner(&self, alias: &str) -> Option<String> {
            alias.starts_with("#irc_").then(|| "irc".to_owned())
        }

        async fn query_room_alias(&self, alias: &str) -> bool {
            self.asked.lock().unwrap().push(alias.to_owned());
            if alias != "#irc_new:hs1" {
                return false;
            }
            let room = self.room.lock().unwrap().clone().unwrap();
            let handle = self.rooms.get_or_load(&room).await.unwrap();
            let alias = RoomAliasId::parse(alias).unwrap().to_owned();
            let bot = UserId::parse("@ircbot:hs1").unwrap().to_owned();
            handle
                .query(move |actor| actor.create_alias(&alias, &bot))
                .await
                .unwrap();
            true
        }
    }

    fn requester(user: &str, appservice: Option<&str>) -> RoomRequester {
        let mut requester =
            hs_auth::requester::Requester::for_user(UserId::parse(user).unwrap().to_owned());
        requester.appservice = appservice.map(|id| hs_auth::requester::AppserviceIdentity {
            appservice_id: id.to_owned(),
            sender: UserId::parse("@ircbot:hs1").unwrap().to_owned(),
            masqueraded_user: false,
            masqueraded_device_id: None,
            rate_limited: true,
            msc4190_enabled: false,
        });
        RoomRequester(requester)
    }

    /// Sytest's "Regular users cannot create room aliases within the AS namespace" (`400
    /// M_EXCLUSIVE`, while the appservice itself may), and "Accesing an AS-hosted room alias asks
    /// the AS server": a local alias the directory does not hold is asked of the appservice,
    /// which creates it, and it then resolves.
    #[tokio::test]
    async fn an_appservices_aliases_are_its_own_and_it_is_asked_for_ones_nobody_has_made() {
        let mut state = state(None);
        let bridge = Arc::new(IrcBridge {
            rooms: state.rooms.clone(),
            room: Mutex::new(None),
            asked: Mutex::new(Vec::new()),
        });
        state.auth.appservices = bridge.clone();
        let alice = UserId::parse("@alice:hs1").unwrap().to_owned();
        let handle = state
            .rooms
            .create_room(alice, crate::actor::CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        *bridge.room.lock().unwrap() = Some(room_id.clone());
        let put = |alias: &str, who: RoomRequester| {
            put_alias::<MemoryBackend>(
                State(state.clone()),
                Path(alias.to_owned()),
                who,
                PermissiveJson(json!({"room_id": room_id.to_string()})),
            )
        };

        let err = put("#irc_mine:hs1", requester("@alice:hs1", None))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::Exclusive(_)), "{err:?}");
        assert_eq!(err.to_matrix_error().errcode.as_str(), "M_EXCLUSIVE");
        put("#irc_mine:hs1", requester("@ircbot:hs1", Some("irc")))
            .await
            .unwrap();
        put("#plain:hs1", requester("@alice:hs1", None))
            .await
            .unwrap();

        let response = get_alias(State(state.clone()), Path("#irc_new:hs1".to_owned()))
            .await
            .unwrap();
        assert_eq!(body_json(response).await["room_id"], room_id.to_string());
        let err = get_alias(State(state.clone()), Path("#irc_gone:hs1".to_owned()))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err:?}");
        // A held alias is not asked about; an unknown one is.
        get_alias(State(state.clone()), Path("#plain:hs1".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            *bridge.asked.lock().unwrap(),
            vec!["#irc_new:hs1", "#irc_gone:hs1"]
        );
    }

    #[tokio::test]
    async fn a_remote_alias_without_federation_is_not_found() {
        let err = get_alias(State(state(None)), Path("#test:remote.example".to_owned()))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err:?}");
    }

    #[test]
    fn without_alias_takes_it_out_of_alias_and_alt_aliases() {
        assert_eq!(
            without_alias(&json!({"alias": "#a:hs1"}), "#a:hs1"),
            Some(json!({}))
        );
        assert_eq!(
            without_alias(
                &json!({"alias": "#a:hs1", "alt_aliases": ["#b:hs1", "#a:hs1"]}),
                "#a:hs1"
            ),
            Some(json!({"alt_aliases": ["#b:hs1"]}))
        );
        assert_eq!(
            without_alias(
                &json!({"alias": "#b:hs1", "alt_aliases": ["#a:hs1"]}),
                "#a:hs1"
            ),
            Some(json!({"alias": "#b:hs1"}))
        );
        assert_eq!(without_alias(&json!({"alias": "#b:hs1"}), "#a:hs1"), None);
    }

    /// Sytest's and Complement's "Can delete canonical alias": deleting the alias the room's
    /// `m.room.canonical_alias` names sends a new one without it.
    #[tokio::test]
    async fn deleting_the_canonical_alias_takes_it_out_of_the_room_state() {
        let state = state(None);
        let alice = ruma::user_id!("@alice:hs1");
        let handle = state
            .rooms
            .create_room(
                alice.to_owned(),
                crate::actor::CreateRoomRequest::default(),
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        put_alias(
            State(state.clone()),
            Path("#gone:hs1".to_owned()),
            requester(alice.as_str(), None),
            PermissiveJson(json!({"room_id": room_id.to_string()})),
        )
        .await
        .unwrap();
        handle
            .send_event(
                alice.to_owned(),
                "m.room.canonical_alias".to_owned(),
                Some(String::new()),
                json!({"alias": "#gone:hs1"}),
                None,
                2,
            )
            .await
            .unwrap();
        delete_alias(
            State(state.clone()),
            Path("#gone:hs1".to_owned()),
            requester(alice.as_str(), None),
        )
        .await
        .unwrap();
        let content = handle
            .query(|actor| {
                actor
                    .state_event("m.room.canonical_alias", "")
                    .unwrap()
                    .and_then(|e| e.json().get("content"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .map(crate::routes::render::canonical_to_json)
            })
            .await;
        assert_eq!(content, Some(json!({})));
        assert!(
            state
                .rooms
                .resolve_alias(&RoomAliasId::parse("#gone:hs1").unwrap())
                .unwrap()
                .is_none()
        );
    }
}
