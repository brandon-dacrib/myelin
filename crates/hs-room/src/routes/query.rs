//! State, event, member and timeline queries: `GET /rooms/{roomId}/state(...)`,
//! `/event/{eventId}`, `/context/{eventId}`, `/members`, `/joined_members`, `/messages`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{EventId, RoomId};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

use crate::error::RoomError;
use crate::routes::render::client_event_json;
use crate::state::{RoomRequester, RoomState};
use crate::timeline::{Direction, PaginationToken};

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

fn parse_event_id(raw: &str) -> Result<ruma::OwnedEventId, RoomError> {
    EventId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// `GET /rooms/{roomId}/state/{eventType}/{stateKey}`.
pub async fn get_state_with_key<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_type, state_key)): Path<(String, String, String)>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let content = handle
        .query(move |actor| {
            actor
                .state_event(&event_type, &state_key)
                .map(|found| found.and_then(|e| e.json().get("content").cloned()))
        })
        .await?;
    match content {
        Some(content) => Ok(Json(crate::routes::render::canonical_to_json(
            &content.as_object().cloned().unwrap_or_default(),
        ))
        .into_response()),
        None => Err(RoomError::EventNotFound("state event not found".into())),
    }
}

/// `GET /rooms/{roomId}/state/{eventType}` (empty state key).
pub async fn get_state_no_key<B: KvBackend + 'static>(
    state: State<RoomState<B>>,
    Path((room_id, event_type)): Path<(String, String)>,
    requester: RoomRequester,
) -> Result<Response, RoomError> {
    get_state_with_key(state, Path((room_id, event_type, String::new())), requester).await
}

/// `GET /rooms/{roomId}/state`.
pub async fn get_state<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let events = handle
        .query(|actor| {
            actor.full_state().map(|events| {
                events
                    .into_iter()
                    .map(client_event_json)
                    .collect::<Vec<_>>()
            })
        })
        .await?;
    Ok(Json(events).into_response())
}

/// `GET /rooms/{roomId}/event/{eventId}`.
///
/// The response carries `unsigned.m.relations` (bundled aggregations -- `m.replace`,
/// `m.annotation`, `m.thread`; `crate::relations::bundle`) if the event has any children.
pub async fn get_event<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id)): Path<(String, String)>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let event_id = parse_event_id(&event_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let found = handle
        .query(move |actor| {
            actor.event_by_id(&event_id).map(|event| {
                let bundle = actor.relation_bundle(event.event_id(), &requester.user_id);
                crate::routes::render::client_event_json_bundled(event, &bundle)
            })
        })
        .await;
    found
        .map(|v| Json(v).into_response())
        .ok_or_else(|| RoomError::EventNotFound("event not found".into()))
}

/// `GET /rooms/{roomId}/context/{eventId}`.
///
/// `event`, `events_before` and `events_after` each carry `unsigned.m.relations` if they have
/// children (`crate::relations::bundle`).
pub async fn get_context<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let event_id = parse_event_id(&event_id)?;
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let handle = state.rooms.get_or_load(&room_id).await?;

    let result = handle
        .query(move |actor| {
            let target = actor.event_by_id(&event_id)?;
            let render = |e: &hs_model::Event| {
                let bundle = actor.relation_bundle(e.event_id(), &requester.user_id);
                crate::routes::render::client_event_json_bundled(e, &bundle)
            };
            let target_json = render(target);
            // Find the target's position in the timeline via a full scan. Acceptable for Phase
            // 0's in-memory timeline (no store round trip either way, and the whole room's
            // history is already resident -- see `RoomActor`'s doc comment on its `events`
            // field); a real position index is the documented next step.
            //
            // `all` is newest-first (descending `room_pos`): index `pos - 1` is the event
            // immediately *newer* than the target, index `pos + 1` immediately *older*.
            let (all, _) = actor.paginate(None, Direction::Backward, usize::MAX);
            let pos = all.iter().position(|e| e.event_id() == target.event_id())?;
            // "events_before" (older than target) in reverse-chronological order (nearest to the
            // target first): that is exactly ascending-index order over `all[pos+1..end]`, since
            // `all` is already newest-first.
            let end = (pos + 1 + limit).min(all.len());
            let events_before: Vec<_> = all[pos + 1..end].iter().copied().map(render).collect();
            // "events_after" (newer than target) in chronological order (nearest to the target
            // first, i.e. oldest of the "after" set first): `all[start..pos]` is newest-first, so
            // reverse it.
            let start = pos.saturating_sub(limit);
            let events_after: Vec<_> = all[start..pos].iter().rev().copied().map(render).collect();
            let state_json = actor
                .full_state()
                .ok()?
                .into_iter()
                .map(client_event_json)
                .collect::<Vec<_>>();
            Some(json!({
                "event": target_json,
                "events_before": events_before,
                "events_after": events_after,
                "state": state_json,
                "start": "",
                "end": "",
            }))
        })
        .await;

    result
        .map(|v| Json(v).into_response())
        .ok_or_else(|| RoomError::EventNotFound("event not found".into()))
}

/// `GET /rooms/{roomId}/members`.
pub async fn get_members<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let chunk = handle
        .query(|actor| {
            actor.members().map(|events| {
                events
                    .into_iter()
                    .map(client_event_json)
                    .collect::<Vec<_>>()
            })
        })
        .await?;
    Ok(Json(json!({"chunk": chunk})).into_response())
}

/// `GET /rooms/{roomId}/joined_members`.
pub async fn get_joined_members<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(_requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let joined = handle
        .query(|actor| {
            actor.joined_members().map(|members| {
                members
                    .into_iter()
                    .filter_map(|e| {
                        let user_id = e.header().state_key.clone()?;
                        let display_name = e
                            .json()
                            .get("content")
                            .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                            .and_then(|c| c.get("displayname"))
                            .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                            .map(str::to_owned);
                        Some((user_id, json!({"display_name": display_name})))
                    })
                    .collect::<HashMap<_, _>>()
            })
        })
        .await?;
    Ok(Json(json!({"joined": joined})).into_response())
}

/// Query parameters for `GET /rooms/{roomId}/messages`.
#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    /// The pagination token to start from. Absent means "the live end of the timeline".
    pub from: Option<String>,
    /// `"f"` or `"b"`; defaults to `"b"`.
    pub dir: Option<String>,
    /// Maximum number of events to return; defaults to 10, capped at 1000.
    pub limit: Option<usize>,
}

/// `GET /rooms/{roomId}/messages`.
///
/// Each event in `chunk` carries `unsigned.m.relations` if it has children
/// (`crate::relations::bundle`).
pub async fn get_messages<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    Query(query): Query<MessagesQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let direction = query
        .dir
        .as_deref()
        .and_then(Direction::from_query)
        .unwrap_or(Direction::Backward);
    let from = query
        .from
        .as_deref()
        .map(str::parse::<PaginationToken>)
        .transpose()?;
    let limit = query.limit.unwrap_or(10).min(1000);

    let handle = state.rooms.get_or_load(&room_id).await?;
    let (start, chunk, end) = handle
        .query(move |actor| {
            let (events, next) = actor.paginate(from, direction, limit);
            let start_token = from.unwrap_or_else(|| PaginationToken::new(0, direction));
            let chunk = events
                .into_iter()
                .map(|e| {
                    let bundle = actor.relation_bundle(e.event_id(), &requester.user_id);
                    crate::routes::render::client_event_json_bundled(e, &bundle)
                })
                .collect::<Vec<_>>();
            (start_token.to_string(), chunk, next.map(|t| t.to_string()))
        })
        .await;

    Ok(Json(json!({"start": start, "chunk": chunk, "end": end})).into_response())
}
