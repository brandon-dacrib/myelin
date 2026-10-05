//! `GET /_matrix/client/v1/room_summary/{roomIdOrAlias}` (MSC3266, in the spec since v1.15):
//! what a client shows about a room before joining it -- a link preview, a "join this room?"
//! dialog -- and `GET /rooms/{roomId}/timestamp_to_event` (MSC3030, "jump to date").

use axum::Json;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{OwnedRoomId, RoomAliasId, RoomId};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::hierarchy::Access;
use crate::state::{RoomRequester, RoomState};
use crate::timeline::Direction;

/// A requester when the request carries an access token, nobody when it carries none: for the
/// endpoints the spec lets anyone call. A token that is there but not valid is still refused.
#[derive(Debug)]
pub struct MaybeRequester(pub Option<hs_auth::requester::Requester>);

impl<B: KvBackend> FromRequestParts<RoomState<B>> for MaybeRequester {
    type Rejection = hs_http::error::MatrixError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RoomState<B>,
    ) -> Result<Self, Self::Rejection> {
        let has_header = parts
            .headers
            .contains_key(axum::http::header::AUTHORIZATION);
        let has_query = parts.uri.query().is_some_and(|q| {
            q.split('&')
                .any(|pair| pair.split('=').next() == Some("access_token"))
        });
        if !has_header && !has_query {
            return Ok(Self(None));
        }
        RoomRequester::from_request_parts(parts, state)
            .await
            .map(|RoomRequester(requester)| Self(Some(requester)))
    }
}

/// The room a `roomIdOrAlias` names: a room ID as it is, an alias resolved here, or by its own
/// server when it is another's.
async fn resolve_room<B: KvBackend + 'static>(
    state: &RoomState<B>,
    raw: &str,
) -> Result<OwnedRoomId, RoomError> {
    if let Ok(room_id) = RoomId::parse(raw) {
        return Ok(room_id.to_owned());
    }
    let alias = RoomAliasId::parse(raw).map_err(|e| {
        RoomError::BadRequest(format!("{raw:?} is neither a room ID nor an alias: {e}"))
    })?;
    if alias.server_name() == state.identity.server_name {
        return state
            .rooms
            .resolve_alias(&alias)?
            .ok_or_else(|| RoomError::RoomNotFound(raw.to_owned()));
    }
    let remote = state
        .remote_join
        .as_ref()
        .ok_or_else(|| RoomError::RoomNotFound(raw.to_owned()))?;
    Ok(remote.resolve_alias(&alias).await?.0)
}

/// `GET /room_summary/{roomIdOrAlias}`: the room's `RoomSummary` (name, topic, avatar,
/// canonical alias, joined members, join rule, `allowed_room_ids` for a restricted room, room
/// type, version, encryption, world-readability, guest access) plus the requester's
/// `membership` (`leave` when they have none), for a room this server holds.
///
/// Who may see it is who may see a room in the space hierarchy (`crate::hierarchy::local_access`):
/// a member or invitee, anyone for a public, knockable or world-readable room, and a member of
/// one of the rooms a restricted room allows. Anyone else -- and anybody at all, for a room this
/// server does not hold -- gets `404`, the same as for a room that does not exist, so that the
/// endpoint says nothing about rooms the requester may not know of. Complement's
/// `TestRoomSummaryAllowedRoomIDs`.
pub async fn get_room_summary<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id_or_alias): Path<String>,
    MaybeRequester(requester): MaybeRequester,
) -> Result<Response, RoomError> {
    let not_found = || RoomError::RoomNotFound(room_id_or_alias.clone());
    let room_id = resolve_room(&state, &room_id_or_alias).await?;
    let handle = match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => handle,
        Err(RoomError::RoomNotFound(_)) => return Err(not_found()),
        Err(error) => return Err(error),
    };
    let viewer = requester.as_ref().map(|r| r.user_id.clone());
    let (summary, access, membership) = handle
        .query(move |actor| -> Result<_, RoomError> {
            let summary = crate::hierarchy::summarize(actor)?;
            let membership = match viewer.as_deref() {
                Some(user) => Some(
                    actor
                        .state_event("m.room.member", user.as_str())?
                        .and_then(|e| {
                            e.json()
                                .get("content")
                                .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                                .and_then(|c| c.get("membership"))
                                .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| "leave".to_owned()),
                ),
                None => None,
            };
            let access = match viewer.as_deref() {
                // A knock is pending: the knocker may see what they knocked on.
                Some(_) if membership.as_deref() == Some("knock") => Access::Visible,
                Some(user) => crate::hierarchy::local_access(actor, user),
                None => crate::hierarchy::anonymous_access(actor),
            };
            Ok((summary, access, membership))
        })
        .await?;
    let visible = match access {
        Access::Visible => true,
        Access::Hidden => false,
        Access::IfInAnyOf(allowed) => match requester.as_ref() {
            Some(requester) => {
                let joined = state.rooms.rooms_joined_by_user(&requester.user_id)?;
                allowed.iter().any(|room| joined.contains(room))
            }
            None => false,
        },
    };
    if !visible {
        tracing::debug!(%room_id, "a room summary was asked for by someone who may not see the room");
        return Err(not_found());
    }
    let mut body = serde_json::to_value(&summary)
        .map_err(|e| RoomError::Internal(format!("could not render a room summary: {e}")))?;
    if let (Some(membership), Some(object)) = (membership, body.as_object_mut()) {
        object.insert("membership".to_owned(), Value::String(membership));
    }
    Ok(Json(body).into_response())
}

/// Query parameters for `GET /rooms/{roomId}/timestamp_to_event`.
#[derive(Debug, Deserialize)]
pub struct TimestampQuery {
    /// The time, in milliseconds since the Unix epoch.
    pub ts: i64,
    /// `f`: the first event at or after `ts`; `b`: the last at or before it.
    pub dir: String,
}

/// `GET /rooms/{roomId}/timestamp_to_event` (MSC3030): the event closest to `ts` in direction
/// `dir` -- forwards, the first sent at or after it, the topologically first of several sent in
/// the same millisecond; backwards, the last at or before it, the topologically last of those
/// -- as `{event_id, origin_server_ts}`, or `404` when there is none. Who may not read the room
/// gets `403` (a non-member of a room that is not world-readable, Complement's
/// `TestJumpToDateEndpoint`).
///
/// When this server's copy of the room's history begins later than the room does (joined
/// through another server, earlier history not fetched) and the answer from what is held is
/// nothing, or the oldest event held, the other servers in the room are asked
/// (`GET /_matrix/federation/v1/timestamp_to_event`, through
/// [`crate::remote_join::RemoteJoin::timestamp_to_event`]) and their answer is preferred when it
/// is closer; its history is then fetched (`crate::backfill`) so that `/context` and
/// `/messages` can be read from the event, as the spec has a server do.
pub async fn get_timestamp_to_event<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    Query(query): Query<TimestampQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = RoomId::parse(&room_id)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))?;
    let direction = Direction::from_query(&query.dir)
        .ok_or_else(|| RoomError::InvalidParam("dir must be \"f\" or \"b\"".to_owned()))?;
    let handle = match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => handle,
        Err(RoomError::RoomNotFound(_)) => {
            return Err(RoomError::Forbidden(
                "you aren't a member of the room".into(),
            ));
        }
        Err(error) => return Err(error),
    };
    let own = state.identity.server_name.clone();
    let ts = query.ts;
    let reader = requester.user_id.clone();
    let (local, ask) = handle
        .query(move |actor| -> Result<_, RoomError> {
            if !actor.can_read_room(&reader)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            let found = actor
                .event_nearest(ts, direction)
                .filter(|e| actor.event_visible_to(e, &reader).unwrap_or(false))
                .map(|e| (e.event_id().to_owned(), e.header().origin_server_ts));
            // The oldest event held, when the room's history goes on before it: an answer
            // next to that gap may only be the nearest *held*.
            let oldest = actor
                .paginate(None, Direction::Forward, 1)
                .0
                .first()
                .map(|e| e.event_id().to_owned());
            let at_gap = actor.history_before_oldest()
                && found
                    .as_ref()
                    .is_none_or(|(id, _)| oldest.as_deref() == Some(id.as_ref()));
            let servers: Vec<String> = if at_gap {
                let mut servers = Vec::new();
                for member in actor.joined_members()? {
                    if let Some(server) = member
                        .header()
                        .state_key
                        .as_deref()
                        .and_then(|k| ruma::UserId::parse(k).ok())
                        .map(|u| u.server_name().to_owned())
                        && server != own
                        && !servers.iter().any(|s: &String| s == server.as_str())
                    {
                        servers.push(server.to_string());
                    }
                }
                servers
            } else {
                Vec::new()
            };
            Ok((found, servers))
        })
        .await?;
    let mut answer = local;
    if let Some(remote) = state.remote_join.as_ref() {
        for server in &ask {
            match remote
                .timestamp_to_event(server, &room_id, ts, direction)
                .await
            {
                Ok(Some((event_id, at))) => {
                    let closer = answer.as_ref().is_none_or(|(_, held)| match direction {
                        Direction::Forward => at < *held,
                        Direction::Backward => at > *held,
                    });
                    tracing::debug!(%room_id, server, %event_id, closer, "another server answered timestamp_to_event");
                    if closer {
                        answer = Some((event_id, at));
                    }
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::info!(%room_id, server, %error, "another server could not answer timestamp_to_event");
                }
            }
        }
    }
    let Some((event_id, origin_server_ts)) = answer else {
        return Err(RoomError::EventNotFound(format!(
            "no event found from {ts} in direction {}",
            query.dir
        )));
    };
    fetch_history_down_to(&state, &handle, &room_id, &event_id).await;
    Ok(Json(json!({"event_id": event_id, "origin_server_ts": origin_server_ts})).into_response())
}

/// How many batches of earlier history [`fetch_history_down_to`] asks for at most.
const FETCH_ROUNDS: usize = 5;

/// Fetches the room's earlier history (`crate::backfill`) until `event_id` is held, a fetch
/// brings nothing, or [`FETCH_ROUNDS`] batches have come: so that an event another server named
/// can be read with `/context` and paged from. Best effort; nothing here fails the request.
async fn fetch_history_down_to<B: KvBackend + 'static>(
    state: &RoomState<B>,
    handle: &crate::actor::RoomActorHandle<B>,
    room_id: &RoomId,
    event_id: &ruma::EventId,
) {
    let Some(hook) = state.rooms.backfill_hook().cloned() else {
        return;
    };
    for _ in 0..FETCH_ROUNDS {
        let wanted = event_id.to_owned();
        if handle
            .query(move |actor| actor.event_by_id(&wanted).is_some())
            .await
        {
            return;
        }
        match hook.backfill(room_id).await {
            Ok(added) if added > 0 => {}
            Ok(_) => return,
            Err(error) => {
                tracing::info!(%room_id, %event_id, %error, "could not fetch the history down to an event another server named");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hs_auth::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::{RoomVersionId, user_id};

    use super::*;
    use crate::actor::{CreateRoomRequest, InitialStateEvent};
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;

    fn requester(user: &ruma::UserId) -> hs_auth::requester::Requester {
        hs_auth::requester::Requester {
            user_id: user.to_owned(),
            device_id: None,
            is_guest: false,
            is_admin: false,
            shadow_banned: false,
            suspended: false,
            appservice: None,
            access_token_id: None,
        }
    }

    fn app() -> RoomState<MemoryBackend> {
        let identity = HomeserverIdentity::for_tests("hs1");
        RoomState {
            auth: AuthState::in_memory(),
            rooms: Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).unwrap()),
            identity,
            remote_join: None,
        }
    }

    async fn create(state: &RoomState<MemoryBackend>, request: CreateRoomRequest) -> OwnedRoomId {
        let handle = state
            .rooms
            .create_room(user_id!("@alice:hs1").to_owned(), request, 1)
            .await
            .unwrap();
        handle.query(|actor| actor.room_id().to_owned()).await
    }

    async fn summary(
        state: &RoomState<MemoryBackend>,
        room: &RoomId,
        who: Option<&ruma::UserId>,
    ) -> Result<Value, RoomError> {
        let response = get_room_summary::<MemoryBackend>(
            State(state.clone()),
            Path(room.to_string()),
            MaybeRequester(who.map(requester)),
        )
        .await?;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    /// Complement's `TestRoomSummaryAllowedRoomIDs`: a restricted room names the rooms it
    /// allows, an invite-only room does not have the key, and a stranger is not shown the
    /// invite-only one at all.
    #[tokio::test]
    async fn the_summary_names_allowed_rooms_and_hides_what_a_stranger_may_not_see() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let space = create(
            &state,
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                creation_content: json!({"type": "m.space"}),
                ..Default::default()
            },
        )
        .await;
        let restricted = create(
            &state,
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                room_version: Some(RoomVersionId::V8),
                initial_state: vec![InitialStateEvent {
                    event_type: "m.room.join_rules".to_owned(),
                    state_key: String::new(),
                    content: json!({
                        "join_rule": "restricted",
                        "allow": [{"type": "m.room_membership", "room_id": space}],
                    }),
                }],
                ..Default::default()
            },
        )
        .await;
        let invite_only = create(
            &state,
            CreateRoomRequest {
                preset: Some("private_chat".to_owned()),
                ..Default::default()
            },
        )
        .await;

        let body = summary(&state, &restricted, Some(alice)).await.unwrap();
        assert_eq!(body["room_id"], restricted.as_str());
        assert_eq!(body["join_rule"], "restricted");
        assert_eq!(body["allowed_room_ids"], json!([space]));
        assert_eq!(body["membership"], "join");

        let body = summary(&state, &invite_only, Some(alice)).await.unwrap();
        assert_eq!(body["room_id"], invite_only.as_str());
        assert!(body.get("allowed_room_ids").is_none(), "{body}");

        let err = summary(&state, &invite_only, Some(user_id!("@bob:hs1")))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err:?}");
        let body = summary(&state, &space, None).await.unwrap();
        assert_eq!(body["room_type"], "m.space");
        assert!(body.get("membership").is_none());
    }

    /// Complement's `TestJumpToDateEndpoint` over what this server holds: forwards the first
    /// event at or after the time, backwards the last at or before it, `404` past either end,
    /// `403` for a stranger to a private room.
    #[tokio::test]
    async fn timestamp_to_event_finds_the_nearest_event_each_way() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let room = create(&state, CreateRoomRequest::default()).await;
        let handle = state.rooms.get_or_load(&room).await.unwrap();
        let mut sent = Vec::new();
        for ts in [1_000, 2_000, 2_000] {
            let event = handle
                .send_event(
                    alice.to_owned(),
                    "m.room.message".to_owned(),
                    None,
                    json!({"msgtype": "m.text", "body": ts.to_string()}),
                    None,
                    ts,
                )
                .await
                .unwrap();
            sent.push(event.event_id().to_string());
        }
        let ask = |ts: i64, dir: &str, who: &'static ruma::UserId| {
            let state = state.clone();
            let room = room.to_string();
            let dir = dir.to_owned();
            async move {
                let response = get_timestamp_to_event::<MemoryBackend>(
                    State(state),
                    Path(room),
                    Query(TimestampQuery { ts, dir }),
                    RoomRequester(requester(who)),
                )
                .await?;
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                Ok::<_, RoomError>(body["event_id"].as_str().unwrap().to_owned())
            }
        };
        assert_eq!(ask(1_500, "f", alice).await.unwrap(), sent[1]);
        assert_eq!(ask(1_500, "b", alice).await.unwrap(), sent[0]);
        // Two in the same millisecond: forwards the first, backwards the last.
        assert_eq!(ask(2_000, "f", alice).await.unwrap(), sent[1]);
        assert_eq!(ask(2_000, "b", alice).await.unwrap(), sent[2]);
        assert!(matches!(
            ask(5_000, "f", alice).await,
            Err(RoomError::EventNotFound(_))
        ));
        assert!(matches!(
            ask(1_500, "f", user_id!("@bob:hs1")).await,
            Err(RoomError::Forbidden(_))
        ));
    }
}
