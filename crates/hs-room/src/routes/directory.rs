//! `PUT`/`GET /_matrix/client/v3/directory/list/room/{roomId}` (one room's own publish/unpublish
//! toggle) and `GET`/`POST /publicRooms` (the server's published-room directory listing).
//!
//! **Coordination note with track 05 (`hs-user`).** `hs-user` also has a `GET`/`POST
//! /publicRooms` implementation (`crates/hs-user/src/routes/rooms.rs`, backed by
//! `crate::store::UserStore::list_public_rooms`, populated from `m.room.join_rules ==
//! "public"` via `hub.rs`'s `public_directory_entry`) -- `hs_http::router::Builder` panics at
//! server-boot time on an overlapping method+path registration, so only one crate's handlers can
//! be mounted from `hs-cli`'s router (`crates/hs-cli/src/serve.rs`). Track 05 found this
//! collision independently and left its own registration unmounted, deferring to this crate's
//! (see `crates/hs-user/src/routes/mod.rs`'s own doc comment, added the same session). **This
//! crate's version is the one actually mounted** (`crate::routes::router`'s `.get`/`.post` for
//! `/publicRooms` below) and is the spec-correct one on the one dimension that matters most: it
//! reads a real, explicit publish flag (`crate::registry::RoomRegistry::is_directory_public`,
//! backed by `crate::persist::Tables::public_rooms`, set by [`put_directory_visibility`] or
//! `POST /createRoom`'s `visibility: "public"`), not `hs-user`'s `join_rule == "public"` proxy --
//! the two are genuinely different rooms in general (a `private_chat`-preset room published to
//! the directory has `join_rule: "invite"` but should still be listed). `hs-user`'s
//! implementation is left in place, unmounted, in case it has something worth merging in later
//! (its own doc comment says the same from its side).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::RoomId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::actor::RoomActor;
use crate::error::RoomError;
use crate::state::{RoomRequester, RoomState};

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// Reads `event.content[key]` as an owned string, for an `Option<&Event>` (as
/// `RoomActor::state_event` returns) rather than the `&Event` `crate::actor`'s own private
/// `content_str` helper takes -- this module cannot see that helper (it is private to the
/// `actor` module), and the pattern is short enough not to need a shared, crate-visible version
/// for its one other caller.
fn content_str(event: Option<&hs_model::Event>, key: &str) -> Option<String> {
    event?
        .json()
        .get("content")
        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        .and_then(|c| c.get(key))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
        .map(str::to_owned)
}

/// Builds one room's `GET /publicRooms` chunk entry (the `PublicRoomsChunk` shape) from its
/// current state. Optional fields (`name`, `topic`, `canonical_alias`, `avatar_url`) are omitted
/// entirely when unset, matching `public_rooms_test.go`'s "Name/topic keys are correct" (a room
/// with no name/topic must not report an empty-string value for it).
pub fn public_rooms_chunk_entry<B: KvBackend>(actor: &RoomActor<B>) -> Value {
    let name = content_str(actor.state_event("m.room.name", "").ok().flatten(), "name");
    let topic = content_str(
        actor.state_event("m.room.topic", "").ok().flatten(),
        "topic",
    );
    let canonical_alias = content_str(
        actor
            .state_event("m.room.canonical_alias", "")
            .ok()
            .flatten(),
        "alias",
    );
    let avatar_url = content_str(actor.state_event("m.room.avatar", "").ok().flatten(), "url");
    let world_readable = content_str(
        actor
            .state_event("m.room.history_visibility", "")
            .ok()
            .flatten(),
        "history_visibility",
    )
    .as_deref()
        == Some("world_readable");
    let guest_can_join = content_str(
        actor.state_event("m.room.guest_access", "").ok().flatten(),
        "guest_access",
    )
    .as_deref()
        == Some("can_join");
    let join_rule = content_str(
        actor.state_event("m.room.join_rules", "").ok().flatten(),
        "join_rule",
    )
    .unwrap_or_else(|| "invite".to_owned());
    let num_joined_members = actor.joined_members().map_or(0, |m| m.len());

    let mut entry = serde_json::Map::new();
    entry.insert(
        "room_id".to_owned(),
        Value::String(actor.room_id().to_string()),
    );
    entry.insert(
        "num_joined_members".to_owned(),
        Value::from(num_joined_members),
    );
    entry.insert("world_readable".to_owned(), Value::Bool(world_readable));
    entry.insert("guest_can_join".to_owned(), Value::Bool(guest_can_join));
    entry.insert("join_rule".to_owned(), Value::String(join_rule));
    if let Some(name) = name {
        entry.insert("name".to_owned(), Value::String(name));
    }
    if let Some(topic) = topic {
        entry.insert("topic".to_owned(), Value::String(topic));
    }
    if let Some(alias) = canonical_alias {
        entry.insert("canonical_alias".to_owned(), Value::String(alias));
    }
    if let Some(avatar_url) = avatar_url {
        entry.insert("avatar_url".to_owned(), Value::String(avatar_url));
    }
    Value::Object(entry)
}

/// `PUT /_matrix/client/v3/directory/list/room/{roomId}`.
pub async fn put_directory_visibility<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(_requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let published = match body.get("visibility").and_then(Value::as_str) {
        Some("public") => true,
        Some("private") | None => false,
        Some(_) => {
            return Err(RoomError::BadRequest(
                "visibility must be \"public\" or \"private\"".into(),
            ));
        }
    };
    // `get_or_load` first so an unknown room reports the same `RoomNotFound` (404) every other
    // room-scoped route does, rather than `set_directory_visibility`'s own `RoomNotFound` message
    // (identical status/errcode either way -- this is purely so the message text is consistent).
    state.rooms.get_or_load(&room_id).await?;
    state.rooms.set_directory_visibility(&room_id, published)?;
    Ok(Json(json!({})).into_response())
}

/// `GET /_matrix/client/v3/directory/list/room/{roomId}`.
pub async fn get_directory_visibility<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let published = state.rooms.is_directory_public(&room_id)?;
    Ok(Json(json!({"visibility": if published { "public" } else { "private" }})).into_response())
}

/// Query parameters for `GET /publicRooms`.
#[derive(Debug, Deserialize, Default)]
pub struct PublicRoomsQuery {
    /// Maximum number of rooms to return.
    pub limit: Option<usize>,
    /// Pagination token. Not implemented (this crate's directory is small enough in Phase 0 scope
    /// to return everything up to `limit` in one page); accepted and ignored rather than rejected,
    /// so a client that always sends one back from an earlier response does not break.
    #[allow(dead_code)]
    pub since: Option<String>,
    /// Another server, whose directory is asked over federation
    /// ([`crate::remote_join::RemoteJoin::public_rooms`]) and answered as it came. Absent, or
    /// this server's own name: this server's directory.
    pub server: Option<String>,
    /// [`PublicRoomsNetworks::third_party_instance_id`].
    pub third_party_instance_id: Option<String>,
    /// [`PublicRoomsNetworks::include_all_networks`].
    pub include_all_networks: Option<bool>,
}

/// Which room directories `/publicRooms` lists: the server's own (neither field), one
/// appservice network's (`third_party_instance_id`, `{appservice id}|{network id}` as
/// `/thirdparty/protocols` gives it), or the server's and every network's
/// (`include_all_networks`). Appservices publish to their networks' directories with `PUT
/// /directory/list/appservice/{networkId}/{roomId}`; `hs-appservice` keeps those and answers
/// through `hs-auth`'s `AppserviceRegistry::network_room_ids`.
#[derive(Debug, Default)]
struct PublicRoomsNetworks {
    third_party_instance_id: Option<String>,
    include_all_networks: bool,
}

impl PublicRoomsNetworks {
    async fn room_ids<B: KvBackend + 'static>(
        &self,
        state: &RoomState<B>,
    ) -> Result<Vec<ruma::OwnedRoomId>, RoomError> {
        let network = |ids: Vec<String>| -> Vec<ruma::OwnedRoomId> {
            ids.iter()
                .filter_map(|id| RoomId::parse(id.as_str()).ok().map(|r| r.to_owned()))
                .collect()
        };
        if self.include_all_networks {
            let mut rooms = state.rooms.list_published_room_ids()?;
            for room_id in network(state.auth.appservices.network_room_ids(None).await) {
                if !rooms.contains(&room_id) {
                    rooms.push(room_id);
                }
            }
            return Ok(rooms);
        }
        match &self.third_party_instance_id {
            Some(instance) => Ok(network(
                state
                    .auth
                    .appservices
                    .network_room_ids(Some(instance))
                    .await,
            )),
            None => state.rooms.list_published_room_ids(),
        }
    }
}

/// `filter.generic_search_term`, the one filter field `POST /publicRooms` defines.
#[derive(Debug, Deserialize, Default)]
pub struct PublicRoomsFilter {
    /// Case-insensitive substring matched against each room's `name`, `topic` and
    /// `canonical_alias` (Synapse's own documented behavior for this field, which the spec itself
    /// leaves server-defined).
    pub generic_search_term: Option<String>,
}

/// Request body for `POST /publicRooms`.
#[derive(Debug, Deserialize, Default)]
pub struct PublicRoomsBody {
    /// Maximum number of rooms to return.
    pub limit: Option<usize>,
    /// See [`PublicRoomsQuery::since`].
    #[allow(dead_code)]
    pub since: Option<String>,
    /// See [`PublicRoomsQuery::server`].
    pub server: Option<String>,
    /// Search/filter criteria.
    pub filter: Option<PublicRoomsFilter>,
    /// [`PublicRoomsNetworks::third_party_instance_id`].
    pub third_party_instance_id: Option<String>,
    /// [`PublicRoomsNetworks::include_all_networks`].
    pub include_all_networks: Option<bool>,
}

/// Another server's directory, when `server` names one (Sytest's "Can get remote public room
/// list"); `None` when the listing is this server's own.
async fn remote_public_rooms<B: KvBackend + 'static>(
    state: &RoomState<B>,
    server: Option<&str>,
    limit: Option<usize>,
    since: Option<&str>,
    search_term: Option<&str>,
) -> Result<Option<Response>, RoomError> {
    let Some(server) = server.filter(|server| *server != state.identity.server_name.as_str())
    else {
        return Ok(None);
    };
    let Some(remote) = state.remote_join.as_ref() else {
        tracing::info!(
            server,
            "another server's room list was asked for, and federation is off"
        );
        return Err(RoomError::RoomNotFound(format!(
            "cannot fetch the public room list of {server}: federation is off"
        )));
    };
    let body = remote
        .public_rooms(server, limit, since, search_term)
        .await?;
    tracing::debug!(server, "answered another server's public room list");
    Ok(Some(Json(body).into_response()))
}

async fn render_public_rooms<B: KvBackend + 'static>(
    state: &RoomState<B>,
    limit: Option<usize>,
    search_term: Option<String>,
    networks: PublicRoomsNetworks,
) -> Result<Response, RoomError> {
    let room_ids = networks.room_ids(state).await?;
    let mut chunk = Vec::with_capacity(room_ids.len());
    for room_id in room_ids {
        // A room can be unpublished and evicted between the directory scan and this load in a
        // race with another request; skip it rather than fail the whole listing.
        let Ok(handle) = state.rooms.get_or_load(&room_id).await else {
            continue;
        };
        chunk.push(handle.query(public_rooms_chunk_entry).await);
    }

    if let Some(term) = search_term
        .as_deref()
        .map(str::to_lowercase)
        .filter(|t| !t.is_empty())
    {
        chunk.retain(|entry| {
            ["name", "topic", "canonical_alias"].iter().any(|key| {
                entry
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.to_lowercase().contains(&term))
            })
        });
    }

    let total = chunk.len();
    if let Some(limit) = limit {
        chunk.truncate(limit);
    }
    Ok(Json(json!({
        "chunk": chunk,
        "total_room_count_estimate": total,
    }))
    .into_response())
}

/// `GET /publicRooms`.
pub async fn get_public_rooms<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Query(query): Query<PublicRoomsQuery>,
) -> Result<Response, RoomError> {
    if let Some(remote) = remote_public_rooms(
        &state,
        query.server.as_deref(),
        query.limit,
        query.since.as_deref(),
        None,
    )
    .await?
    {
        return Ok(remote);
    }
    let networks = PublicRoomsNetworks {
        third_party_instance_id: query.third_party_instance_id,
        include_all_networks: query.include_all_networks.unwrap_or(false),
    };
    render_public_rooms(&state, query.limit, None, networks).await
}

/// `POST /publicRooms`.
pub async fn post_public_rooms<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    PermissiveJson(body): PermissiveJson<PublicRoomsBody>,
) -> Result<Response, RoomError> {
    let term = body.filter.and_then(|f| f.generic_search_term);
    if let Some(remote) = remote_public_rooms(
        &state,
        body.server.as_deref(),
        body.limit,
        body.since.as_deref(),
        term.as_deref(),
    )
    .await?
    {
        return Ok(remote);
    }
    let networks = PublicRoomsNetworks {
        third_party_instance_id: body.third_party_instance_id,
        include_all_networks: body.include_all_networks.unwrap_or(false),
    };
    render_public_rooms(&state, body.limit, term, networks).await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use hs_auth::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::{OwnedRoomId, UserId};

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;

    /// One recorded directory request: server, limit, since, search term.
    type Asked = (String, Option<usize>, Option<String>, Option<String>);

    /// Stands in for the other server: records what it was asked and answers one room.
    #[derive(Default)]
    struct RecordingRemoteJoin {
        asked: Mutex<Vec<Asked>>,
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
            alias: &ruma::RoomAliasId,
        ) -> Result<(OwnedRoomId, Vec<String>), RoomError> {
            Err(RoomError::RoomNotFound(alias.to_string()))
        }

        async fn public_rooms(
            &self,
            server: &str,
            limit: Option<usize>,
            since: Option<&str>,
            search: Option<&str>,
        ) -> Result<Value, RoomError> {
            self.asked.lock().unwrap().push((
                server.to_owned(),
                limit,
                since.map(str::to_owned),
                search.map(str::to_owned),
            ));
            Ok(json!({
                "chunk": [{"room_id": "!r:remote.example", "num_joined_members": 1,
                           "world_readable": false, "guest_can_join": false}],
                "total_room_count_estimate": 1,
            }))
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

    /// Sytest's "Can get remote public room list": `?server=` names another server, whose
    /// directory is asked and answered as it came; this server's own name, or no `server`, is
    /// this server's directory.
    #[tokio::test]
    async fn another_servers_directory_is_asked_of_that_server() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        let state = state(Some(remote.clone()));

        let response = get_public_rooms(
            State(state.clone()),
            Query(PublicRoomsQuery {
                limit: Some(5),
                since: Some("10".to_owned()),
                server: Some("remote.example".to_owned()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let body = body_json(response).await;
        assert_eq!(body["chunk"][0]["room_id"], "!r:remote.example", "{body}");

        let response = post_public_rooms(
            State(state.clone()),
            PermissiveJson(PublicRoomsBody {
                limit: None,
                since: None,
                server: Some("remote.example".to_owned()),
                filter: Some(PublicRoomsFilter {
                    generic_search_term: Some("tea".to_owned()),
                }),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(body_json(response).await["total_room_count_estimate"], 1);

        // This server's own name is not another server.
        let response = get_public_rooms(
            State(state.clone()),
            Query(PublicRoomsQuery {
                limit: None,
                since: None,
                server: Some("hs1".to_owned()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(body_json(response).await["chunk"], json!([]));

        assert_eq!(
            *remote.asked.lock().unwrap(),
            vec![
                (
                    "remote.example".to_owned(),
                    Some(5),
                    Some("10".to_owned()),
                    None
                ),
                (
                    "remote.example".to_owned(),
                    None,
                    None,
                    Some("tea".to_owned())
                ),
            ]
        );
    }

    /// An appservice with one network, `irc|libera`, whose directory holds the rooms in it.
    struct IrcNetworks(Mutex<Vec<String>>);

    #[async_trait]
    impl hs_auth::appservice::AppserviceRegistry for IrcNetworks {
        async fn lookup_by_token(
            &self,
            _token: &str,
        ) -> Option<hs_auth::appservice::AppserviceRecord> {
            None
        }

        async fn network_room_ids(&self, instance_id: Option<&str>) -> Vec<String> {
            match instance_id {
                None | Some("irc|libera") => self.0.lock().unwrap().clone(),
                Some(_) => Vec::new(),
            }
        }
    }

    fn room_ids(body: &Value) -> Vec<String> {
        let mut ids: Vec<String> = body["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .map(|room| room["room_id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    }

    /// Sytest's "AS can publish rooms in their own list" and "AS and main public room lists are
    /// separate": a room in an appservice network's directory is listed for that network's
    /// `third_party_instance_id` and with `include_all_networks`, never in the server's own
    /// list, which stays as it was.
    #[tokio::test]
    async fn an_appservice_networks_rooms_are_listed_apart_from_the_servers() {
        let mut state = state(None);
        let alice = UserId::parse("@alice:hs1").unwrap().to_owned();
        let mut created = Vec::new();
        for _ in 0..2 {
            let handle = state
                .rooms
                .create_room(alice.clone(), crate::actor::CreateRoomRequest::default(), 1)
                .await
                .unwrap();
            created.push(handle.query(|actor| actor.room_id().to_string()).await);
        }
        let (bridged, main) = (created[0].clone(), created[1].clone());
        state.auth.appservices = Arc::new(IrcNetworks(Mutex::new(vec![bridged.clone()])));
        state
            .rooms
            .set_directory_visibility(&RoomId::parse(&main).unwrap(), true)
            .unwrap();

        let list = |body: PublicRoomsBody| {
            let state = state.clone();
            async move {
                body_json(
                    post_public_rooms(State(state), PermissiveJson(body))
                        .await
                        .unwrap(),
                )
                .await
            }
        };
        assert_eq!(
            room_ids(&list(PublicRoomsBody::default()).await),
            vec![main.clone()]
        );
        let network = list(PublicRoomsBody {
            third_party_instance_id: Some("irc|libera".to_owned()),
            ..Default::default()
        })
        .await;
        assert_eq!(room_ids(&network), vec![bridged.clone()]);
        let other = list(PublicRoomsBody {
            third_party_instance_id: Some("irc|oftc".to_owned()),
            ..Default::default()
        })
        .await;
        assert!(room_ids(&other).is_empty());
        let mut everything = vec![bridged.clone(), main.clone()];
        everything.sort();
        let all = list(PublicRoomsBody {
            include_all_networks: Some(true),
            ..Default::default()
        })
        .await;
        assert_eq!(room_ids(&all), everything);

        // The same through `GET`'s query.
        let response = get_public_rooms(
            State(state.clone()),
            Query(PublicRoomsQuery {
                third_party_instance_id: Some("irc|libera".to_owned()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(room_ids(&body_json(response).await), vec![bridged]);
    }

    #[tokio::test]
    async fn another_servers_directory_without_federation_is_not_found() {
        let err = get_public_rooms(
            State(state(None)),
            Query(PublicRoomsQuery {
                limit: None,
                since: None,
                server: Some("remote.example".to_owned()),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err:?}");
    }
}
