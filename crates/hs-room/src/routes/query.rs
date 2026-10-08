//! State, event, member and timeline queries: `GET /rooms/{roomId}/state(...)`,
//! `/event/{eventId}`, `/context/{eventId}`, `/members`, `/joined_members`, `/messages`, and the
//! deprecated `/initialSync`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{EventId, RoomId};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

use crate::error::RoomError;
use crate::routes::render::{attach_replaced_state, client_event_json};
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
///
/// Reads through [`crate::actor::RoomActor::state_event_for_reader`], not the room's unconditional
/// current state: a departed member sees this room's state as of when they left, per
/// `m.room.history_visibility` ("after a user has left a room, they may see any events which they
/// were allowed to see before they left the room, but no events received after they left") --
/// `apidoc_room_history_visibility_test.go`-style coverage for `.../state`, not just
/// `.../event`/`.../messages`.
pub async fn get_state_with_key<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_type, state_key)): Path<(String, String, String)>,
    Query(query): Query<StateEventQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    // `format=event` asks for the whole event -- sender, timestamps, `unsigned` -- where the
    // default, `content`, is only its content. A client uses it to find out *who* set a piece
    // of state and when, which the content alone cannot say.
    let whole_event = query.format.as_deref() == Some("event");
    let found = handle
        .query(move |actor| {
            actor
                .state_event_for_reader(&requester.user_id, &event_type, &state_key)
                .map(|found| {
                    found.map(|e| {
                        if whole_event {
                            attach_replaced_state(
                                client_event_json(e),
                                actor.replaced_state_for(e, &requester.user_id).as_ref(),
                            )
                        } else {
                            crate::routes::render::canonical_to_json(
                                &e.json()
                                    .get("content")
                                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                                    .cloned()
                                    .unwrap_or_default(),
                            )
                        }
                    })
                })
        })
        .await?;
    match found {
        Some(body) => Ok(Json(body).into_response()),
        None => Err(RoomError::EventNotFound("state event not found".into())),
    }
}

/// Query parameters for `GET /rooms/{roomId}/state/{eventType}(/{stateKey})`.
#[derive(Debug, Default, Deserialize)]
pub struct StateEventQuery {
    /// `content` (the default) or `event`.
    #[serde(default)]
    pub format: Option<String>,
}

/// `GET /rooms/{roomId}/state/{eventType}` (empty state key).
pub async fn get_state_no_key<B: KvBackend + 'static>(
    state: State<RoomState<B>>,
    Path((room_id, event_type)): Path<(String, String)>,
    query: Query<StateEventQuery>,
    requester: RoomRequester,
) -> Result<Response, RoomError> {
    get_state_with_key(
        state,
        Path((room_id, event_type, String::new())),
        query,
        requester,
    )
    .await
}

/// `GET /rooms/{roomId}/state`. See [`get_state_with_key`]'s doc comment: reads through
/// `RoomActor::full_state_for_reader`, so a departed member sees this room's state as of when
/// they left, not its live current state.
pub async fn get_state<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let events = handle
        .query(move |actor| {
            actor
                .full_state_for_reader(&requester.user_id)
                .map(|found| {
                    found
                        .unwrap_or_default()
                        .into_iter()
                        .map(|e| {
                            attach_replaced_state(
                                client_event_json(e),
                                actor.replaced_state_for(e, &requester.user_id).as_ref(),
                            )
                        })
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
    let local_server = state.identity.server_name.clone();
    let viewed = handle
        .query(move |actor| {
            let event = actor
                .event_by_id(&event_id)
                .ok_or_else(|| RoomError::EventNotFound("event not found".into()))?;
            // `m.room.history_visibility` denies by returning "not found", not "forbidden": the
            // spec's read-side algorithm makes no distinction between "this event does not exist"
            // and "you may not see it", so neither does this response (see this crate's status
            // file, session on history-visibility enforcement).
            if !actor.event_visible_to(event, &requester.user_id)? {
                return Err(RoomError::EventNotFound("event not found".into()));
            }
            Ok::<_, RoomError>(crate::routes::client_events::view_event(
                actor,
                event,
                &requester,
                &local_server,
            ))
        })
        .await?;
    let mut shown = crate::routes::client_events::finish(&state, vec![viewed]).await;
    Ok(Json(shown.pop().unwrap_or(serde_json::Value::Null)).into_response())
}

/// `GET /rooms/{roomId}/context/{eventId}`.
///
/// `event`, `events_before` and `events_after` each carry `unsigned.m.relations` if they have
/// children (`crate::relations::bundle`) and the reader's membership at them in
/// `unsigned.membership` (MSC4115); an erased sender's events are pruned for a reader who was not
/// there ([`crate::routes::client_events::finish`]).
///
/// A reader who may not read the room at all (never a member of a room that is not
/// `world_readable`) is refused with `403`, as Synapse refuses them -- Sytest's "/context/ on non
/// world readable room does not work"; one who may read the room but not that event gets `404`,
/// the same as for an event that does not exist. `filter` is a `RoomEventFilter`: its content
/// conditions apply to `events_before` and `events_after`, and with `lazy_load_members` the
/// `state` is only the member events of the senders of the events returned (Sytest's
/// "/context/ with lazy_load_members filter works"), not the room's whole state.
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
    let filter = crate::routes::client_events::RoomEventFilter::from_param(
        params.get("filter").map(String::as_str),
    )?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let local_server = state.identity.server_name.clone();

    let (target, before, after, state_json, (start, end)) = handle
        .query(move |actor| -> Result<_, RoomError> {
            if !actor.can_read_room(&requester.user_id)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            let not_found = || RoomError::EventNotFound("event not found".into());
            let target = actor.event_by_id(&event_id).ok_or_else(not_found)?;
            // Same "not found, not forbidden" shape as `get_event`: a target the requester may
            // not see per `m.room.history_visibility` is reported identically to one that does
            // not exist at all.
            if !actor
                .event_visible_to(target, &requester.user_id)
                .unwrap_or(false)
            {
                return Err(not_found());
            }
            let shown = |e: &&&hs_model::Event| {
                filter.matches(e)
                    && actor
                        .event_visible_to(e, &requester.user_id)
                        .unwrap_or(false)
            };
            let view = |e: &hs_model::Event| {
                crate::routes::client_events::view_event(actor, e, &requester, &local_server)
            };
            // Find the target's position in the timeline via a full scan. Acceptable for Phase
            // 0's in-memory timeline (no store round trip either way, and the whole room's
            // history is already resident -- see `RoomActor`'s doc comment on its `events`
            // field); a real position index is the documented next step.
            //
            // `all` is newest-first (descending `room_pos`): index `pos - 1` is the event
            // immediately *newer* than the target, index `pos + 1` immediately *older*.
            let (all, _) = actor.paginate(None, Direction::Backward, usize::MAX);
            let pos = all
                .iter()
                .position(|e| e.event_id() == target.event_id())
                .ok_or_else(not_found)?;
            // "events_before" (older than target) in reverse-chronological order (nearest to the
            // target first): that is exactly ascending-index order over `all[pos+1..end]`, since
            // `all` is already newest-first.
            let end = (pos + 1 + limit).min(all.len());
            let before: Vec<&hs_model::Event> =
                all[pos + 1..end].iter().filter(shown).copied().collect();
            // "events_after" (newer than target) in chronological order (nearest to the target
            // first, i.e. oldest of the "after" set first): `all[start..pos]` is newest-first, so
            // reverse it.
            let start = pos.saturating_sub(limit);
            let after: Vec<&hs_model::Event> = all[start..pos]
                .iter()
                .rev()
                .filter(shown)
                .copied()
                .collect();
            // Pinned to the target event, not this room's *live* current state -- the same bug
            // class already fixed for `/messages`/`/event`/`/state`/`/members` (reading a live
            // value instead of one pinned to a point in time). The spec's `state` field is "the
            // state of the room at the last event returned" for `events_before`'s pagination
            // window, which for `/context` is the target event itself
            // (`refs/matrix-spec/content/client-server-api.md`, "get_events_context": "state: A
            // list of state events relevant to displaying `id`"). `RoomActor::state_at_event`
            // already exists for exactly this ("the room's state as of immediately after the
            // queried event").
            let state_json = if filter.lazy_loads_members() {
                let senders: Vec<&ruma::UserId> = std::iter::once(target)
                    .chain(before.iter().copied())
                    .chain(after.iter().copied())
                    .map(|e| e.header().sender.as_ref())
                    .collect();
                crate::routes::client_events::lazy_member_state(
                    actor,
                    target,
                    &senders,
                    &requester.user_id,
                )
            } else {
                actor
                    .state_at_event(target.event_id())?
                    .map(|snapshot| {
                        snapshot
                            .state
                            .iter()
                            .map(|e| {
                                attach_replaced_state(
                                    client_event_json(e),
                                    actor.replaced_state_for(e, &requester.user_id).as_ref(),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .ok_or_else(not_found)?
            };
            // `start` is the boundary before the oldest event returned, `end` the one after the
            // newest (the target itself when either side is empty): a backward `/messages` from
            // `start` continues with what is older, a forward one from `end` with what is
            // newer, and a backward one from `end` begins with the target or what follows it.
            let position = |e: &hs_model::Event| actor.timeline_position(e.event_id());
            let start = before
                .last()
                .copied()
                .and_then(position)
                .or_else(|| position(target))
                .map(|p| PaginationToken::new(p, Direction::Backward).to_string());
            let end = after
                .last()
                .copied()
                .and_then(position)
                .or_else(|| position(target))
                .map(|p| PaginationToken::new(p, Direction::Forward).to_string());
            Ok((
                view(target),
                before.into_iter().map(view).collect::<Vec<_>>(),
                after.into_iter().map(view).collect::<Vec<_>>(),
                state_json,
                (start, end),
            ))
        })
        .await?;
    let mut target = crate::routes::client_events::finish(&state, vec![target]).await;
    let events_before = crate::routes::client_events::finish(&state, before).await;
    let events_after = crate::routes::client_events::finish(&state, after).await;
    Ok(Json(json!({
        "event": target.pop().unwrap_or(serde_json::Value::Null),
        "events_before": events_before,
        "events_after": events_after,
        "state": state_json,
        "start": start.unwrap_or_default(),
        "end": end.unwrap_or_default(),
    }))
    .into_response())
}

/// Query parameters for `GET /rooms/{roomId}/members`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct MembersQuery {
    /// A pagination token (a sync's `prev_batch` or `next_batch`, or a `/messages` token): the
    /// members as of the newest event before it, rather than now.
    #[serde(default)]
    pub at: Option<String>,
    /// Only members whose `membership` is this.
    #[serde(default)]
    pub membership: Option<String>,
    /// Leave out members whose `membership` is this.
    #[serde(default)]
    pub not_membership: Option<String>,
}

/// `GET /rooms/{roomId}/members`.
pub async fn get_members<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    Query(filter): Query<MembersQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let at = resolve_token(
        &state,
        &requester.user_id,
        &room_id,
        filter.at.as_deref(),
        Direction::Backward,
    )
    .await?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let at_text = filter.at.clone().unwrap_or_default();
    // `members_for_reader`, not `members`: a departed member must not see a member who joined
    // after they left (`room_leave_test.go`'s `TestLeftRoomFixture`).
    let chunk = handle
        .query(move |actor| -> Result<Vec<serde_json::Value>, RoomError> {
            let found = actor.members_for_reader(&requester.user_id)?;
            // `at`: the members in the state after the newest event before the token
            // (Complement's `TestGetRoomMembersAtPoint`, Synapse's reading), when the reader
            // may see that event; otherwise what they may see now. No event before the token
            // at all is `404`, as Synapse answers ("Can't find event for token"): there is no
            // point in the room to read the members at, and the current members are not the
            // members then.
            let found = match (found, at) {
                (Some(now), Some(at)) => {
                    let (newest, _) = actor.paginate(Some(at), Direction::Backward, 1);
                    let Some(point) = newest.first().copied() else {
                        return Err(RoomError::EventNotFound(format!(
                            "before the token {at_text}"
                        )));
                    };
                    let then = actor
                        .event_visible_to(point, &requester.user_id)
                        .unwrap_or(false)
                        .then(|| actor.state_at_event(point.event_id()).ok().flatten())
                        .flatten()
                        .map(|snapshot| {
                            snapshot
                                .state
                                .iter()
                                .filter(|e| e.header().event_type == "m.room.member")
                                .filter_map(|e| actor.event_by_id(e.event_id()))
                                .collect::<Vec<&hs_model::Event>>()
                        });
                    Some(then.unwrap_or(now))
                }
                (found, _) => found,
            };
            Ok(found
                .unwrap_or_default()
                .into_iter()
                // `membership` and `not_membership`: how a client asks for "everyone who is
                // here" without paging through everyone who ever was. Both were ignored, so
                // `?not_membership=leave` came back with the people who had left.
                .filter(|e| {
                    let membership = e
                        .json()
                        .get("content")
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                        .and_then(|c| c.get("membership"))
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
                    filter
                        .membership
                        .as_deref()
                        .is_none_or(|want| membership == Some(want))
                        && filter
                            .not_membership
                            .as_deref()
                            .is_none_or(|unwanted| membership != Some(unwanted))
                })
                .map(|e| {
                    attach_replaced_state(
                        client_event_json(e),
                        actor.replaced_state_for(e, &requester.user_id).as_ref(),
                    )
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Json(json!({"chunk": chunk})).into_response())
}

/// `GET /rooms/{roomId}/joined_members`.
pub async fn get_joined_members<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let joined = handle
        .query(move |actor| {
            if !actor.can_see_current_membership(&requester.user_id)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            actor.joined_members().map(|members| {
                members
                    .into_iter()
                    .filter_map(|e| {
                        let user_id = e.header().state_key.clone()?;
                        let content = e
                            .json()
                            .get("content")
                            .and_then(hs_model::canonical::CanonicalJsonValue::as_object);
                        let field = |key: &str| {
                            content
                                .and_then(|c| c.get(key))
                                .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                                .map(str::to_owned)
                        };
                        // Both keys, always: the spec's `RoomMember` has the two, and a client
                        // that reads `avatar_url` should find `null`, not nothing.
                        Some((
                            user_id,
                            json!({
                                "display_name": field("displayname"),
                                "avatar_url": field("avatar_url"),
                            }),
                        ))
                    })
                    .collect::<HashMap<_, _>>()
            })
        })
        .await?;
    Ok(Json(json!({"joined": joined})).into_response())
}

/// Query parameters for `GET /rooms/{roomId}/messages`.
#[derive(Debug, Default, Deserialize)]
pub struct MessagesQuery {
    /// The pagination token to start from. Absent, or empty (Synapse's reading of `from=`, which
    /// a client sends when a sync gave it no `prev_batch`), means "the live end of the timeline"
    /// backwards and "the room's start" forwards.
    pub from: Option<String>,
    /// The token to stop at: nothing at or beyond it is returned (a sync token works here, as
    /// it does for `from`).
    pub to: Option<String>,
    /// `"f"` or `"b"`; defaults to `"b"`.
    pub dir: Option<String>,
    /// Maximum number of events to return; defaults to 10, capped at 1000.
    pub limit: Option<usize>,
    /// A `RoomEventFilter`, JSON-encoded ([`crate::routes::client_events::RoomEventFilter`]).
    pub filter: Option<String>,
}

/// Resolves a `from`/`to` pagination token for `room_id`: `None` for an absent or empty one;
/// this crate's own [`PaginationToken`] as it is; otherwise, if a
/// [`crate::registry::GlobalTokenResolver`] is installed (in production `hs-user`'s, for the
/// tokens `/sync` hands out), the position it names, as a token a page in `direction` starts
/// from. A sync token covers its room *through* the position it names (the newest event the
/// sync had handed out), while a page excludes its `from` position: so a backward page starts
/// just above it -- the events the sync showed are the first a client paging back from it sees
/// -- and a forward page starts after it.
///
/// # Errors
/// [`RoomError::InvalidPaginationToken`] when neither format matches.
pub(crate) async fn resolve_token<B: KvBackend + 'static>(
    state: &RoomState<B>,
    user_id: &ruma::UserId,
    room_id: &RoomId,
    raw: Option<&str>,
    direction: Direction,
) -> Result<Option<PaginationToken>, RoomError> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    if let Ok(token) = raw.parse::<PaginationToken>() {
        return Ok(Some(towards(token, direction)));
    }
    let Some(resolver) = state.rooms.global_token_resolver() else {
        return Err(RoomError::InvalidPaginationToken);
    };
    match resolver.resolve(user_id, room_id, raw).await? {
        // Not shaped like the resolver's own tokens either: neither format matched, so this
        // really is an invalid token.
        None => Err(RoomError::InvalidPaginationToken),
        // One of the resolver's own tokens, but no position for this room -- treat exactly like
        // an absent token (see the trait doc comment).
        Some(None) => Ok(None),
        Some(Some(pos)) => Ok(Some(match direction {
            Direction::Backward => PaginationToken::new(pos.saturating_add(1), direction),
            Direction::Forward => PaginationToken::new(pos, direction),
        })),
    }
}

/// `token` as the start of a page in `direction`. A token is a boundary between two events: a
/// backward token at `p` lies just before position `p` (a backward page from it begins below
/// `p`), a forward token at `p` just after it (a forward page begins above `p`). Read the other
/// way it is the same boundary, so a forward token at `p` starts a backward page at `p`
/// included, and a backward token at `p` a forward page at `p` included -- what `/context`'s
/// `end` and `start` need to be (Complement's `TestJumpToDateEndpoint` pages backwards from a
/// `/context` `end` and expects the event itself).
fn towards(token: PaginationToken, direction: Direction) -> PaginationToken {
    match (token.direction, direction) {
        (Direction::Forward, Direction::Backward) => {
            PaginationToken::new(token.room_pos.saturating_add(1), direction)
        }
        (Direction::Backward, Direction::Forward) => {
            PaginationToken::new(token.room_pos.saturating_sub(1), direction)
        }
        _ => token,
    }
}

/// The opposite direction: how a `to` token is resolved (see [`MessagesBound`]).
fn opposite(direction: Direction) -> Direction {
    match direction {
        Direction::Backward => Direction::Forward,
        Direction::Forward => Direction::Backward,
    }
}

/// Where a `/messages` page must stop, from its `to` token: the token resolved for the opposite
/// direction marks the region a page *towards* this one would read, and a page keeps only what
/// lies on its own side of it -- forwards, positions below it; backwards, positions above it.
#[derive(Debug, Clone, Copy)]
struct MessagesBound {
    direction: Direction,
    at: i64,
}

impl MessagesBound {
    fn keeps(self, pos: i64) -> bool {
        match self.direction {
            Direction::Forward => pos < self.at,
            Direction::Backward => pos > self.at,
        }
    }
}

/// `GET /rooms/{roomId}/messages`.
///
/// Each event in `chunk` carries `unsigned.m.relations` if it has children
/// (`crate::relations::bundle`) and the reader's own membership at it in `unsigned.membership`
/// (MSC4115); an erased sender's events are pruned for a reader who was not there when they
/// were sent ([`crate::routes::client_events::finish`]).
///
/// `from` and `to`, if present, are tried first as this crate's own [`PaginationToken`] and then
/// through [`crate::registry::RoomRegistry::global_token_resolver`] ([`resolve_token`]). `filter`
/// is a `RoomEventFilter`: its content conditions drop events from the page (the page's tokens
/// stay where the unfiltered page would put them, so nothing is skipped), and with
/// `lazy_load_members` the page's senders' member events come back in `state`.
///
/// `end` is given whenever the page has events, and left out once a page comes back empty at
/// the end of the room, as Synapse does: Sytest's `/messages` tests page once more from the
/// last page's `end` and expect that page to say it is the end.
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
    let filter =
        crate::routes::client_events::RoomEventFilter::from_param(query.filter.as_deref())?;
    let from = resolve_token(
        &state,
        &requester.user_id,
        &room_id,
        query.from.as_deref(),
        direction,
    )
    .await?;
    let to = resolve_token(
        &state,
        &requester.user_id,
        &room_id,
        query.to.as_deref(),
        opposite(direction),
    )
    .await?
    .map(|token| MessagesBound {
        direction,
        at: token.room_pos,
    });
    let limit = query.limit.or(filter.limit).unwrap_or(10).min(1000);

    // A non-existent room reports the same `403 M_FORBIDDEN` as "you aren't a member of the
    // room" (`room_messages_test.go`'s `TestFetchMessagesFromNonExistentRoom`), rather than
    // leaking room existence through a distinct 404.
    let handle = match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => handle,
        Err(RoomError::RoomNotFound(_)) => {
            return Err(RoomError::Forbidden(
                "you aren't a member of the room".into(),
            ));
        }
        Err(e) => return Err(e),
    };
    // A backward page can reach two places where the room's history goes on but this server
    // does not hold it: the oldest event held, in a room joined elsewhere whose earlier history
    // is on the resident; and an open gap in the middle of the timeline, the history between a
    // leave and a rejoin through another server. Either way one batch is fetched
    // (`crate::backfill`) and the page is read again -- at most one fetch of each kind per
    // request, so a request costs at most two round trips to other servers.
    //
    // At the oldest held event, a fetch that adds nothing -- nobody to ask, nobody answering --
    // leaves the first page as it was, with no `end`: the client stops here, and its next look
    // at the room tries again, rather than being handed the same token forever while a peer is
    // down. At a gap, a fetch that adds nothing reads on across the gap instead, so the client
    // still reaches what was held before the leave.
    let options = PageOptions {
        requester: requester.clone(),
        filter,
        to,
        local_server: state.identity.server_name.clone(),
    };
    let hook = state.rooms.backfill_hook().cloned();
    let mut stop_at_gaps = hook.is_some();
    let mut older_fetched = false;
    let mut older_tried = false;
    let mut gap_tried = false;
    let page = loop {
        let page = messages_page(
            &handle,
            from,
            direction,
            limit,
            options.clone(),
            older_fetched,
            stop_at_gaps,
        )
        .await?;
        let Some(hook) = hook.as_ref() else {
            break page;
        };
        match page.wants {
            Wants::Nothing => break page,
            Wants::Older if older_tried => break page,
            Wants::Older => {
                older_tried = true;
                match hook.backfill(&room_id).await {
                    Ok(added) if added > 0 => older_fetched = true,
                    Ok(_) => break page,
                    Err(error) => {
                        tracing::warn!(
                            %room_id,
                            %error,
                            "could not fetch the room's earlier history; answering from what is held"
                        );
                        break page;
                    }
                }
            }
            Wants::Gap(_) if gap_tried => break page,
            Wants::Gap(top) => {
                gap_tried = true;
                match hook.fill_gap(&room_id, top).await {
                    Ok(added) if added > 0 => {}
                    Ok(_) => stop_at_gaps = false,
                    Err(error) => {
                        tracing::warn!(
                            %room_id,
                            top,
                            %error,
                            "could not fetch the history between a leave and a rejoin; reading on past it"
                        );
                        stop_at_gaps = false;
                    }
                }
            }
        }
    };
    let MessagesPage {
        start,
        chunk,
        state: members,
        end,
        ..
    } = page;
    let chunk = crate::routes::client_events::finish(&state, chunk).await;
    // `end` is left out, not `null`, when there is nothing further: the spec's signal for "you
    // have reached the start of the room", and the one a paginating client stops on.
    let mut body = json!({"start": start, "chunk": chunk});
    if let Some(end) = end {
        body["end"] = serde_json::Value::String(end);
    }
    if !members.is_empty() {
        body["state"] = serde_json::Value::Array(members);
    }
    Ok(Json(body).into_response())
}

/// Query parameters for `GET /rooms/{roomId}/initialSync`.
#[derive(Debug, Default, Deserialize)]
pub struct RoomInitialSyncQuery {
    /// How many of the newest events `messages` carries; defaults to 10, capped at 1000.
    pub limit: Option<usize>,
}

/// `GET /rooms/{roomId}/initialSync` (deprecated, but the spec's guest access module still lists
/// it, and Sytest reads rooms through it): one room as a client first sees it -- the requester's
/// `membership`, the room's `state` as the requester may see it (as of their leaving, for a
/// departed member), the newest `messages` in chronological order with `start`/`end`
/// pagination tokens (`start` pages backwards with `/messages`), and whether the room is in the
/// directory (`visibility`). `presence`, `receipts` and `account_data` are empty: `/sync` is
/// where a client reads those.
///
/// Who may call it is who may read the room with `/messages`: a member, a past member, or anybody
/// at all for a `world_readable` room; anyone else gets `403`.
pub async fn get_room_initial_sync<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    Query(query): Query<RoomInitialSyncQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let limit = query.limit.unwrap_or(10).min(1000);
    let handle = match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => handle,
        Err(RoomError::RoomNotFound(_)) => {
            return Err(RoomError::Forbidden(
                "you aren't a member of the room".into(),
            ));
        }
        Err(e) => return Err(e),
    };
    let page = messages_page(
        &handle,
        None,
        Direction::Backward,
        limit,
        PageOptions {
            requester: requester.clone(),
            filter: crate::routes::client_events::RoomEventFilter::default(),
            to: None,
            local_server: state.identity.server_name.clone(),
        },
        false,
        false,
    )
    .await?;
    let reader = requester.user_id.clone();
    let (membership, state_events) = handle
        .query(move |actor| -> Result<_, RoomError> {
            let membership = actor
                .state_event_for_reader(&reader, "m.room.member", reader.as_str())?
                .and_then(|e| {
                    e.json()
                        .get("content")
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                        .and_then(|c| c.get("membership"))
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                        .map(str::to_owned)
                });
            let state_events = actor
                .full_state_for_reader(&reader)?
                .unwrap_or_default()
                .into_iter()
                .map(|e| {
                    attach_replaced_state(
                        client_event_json(e),
                        actor.replaced_state_for(e, &reader).as_ref(),
                    )
                })
                .collect::<Vec<_>>();
            Ok((membership, state_events))
        })
        .await?;
    let visibility = if state.rooms.is_directory_public(&room_id)? {
        "public"
    } else {
        "private"
    };
    let MessagesPage {
        start, chunk, end, ..
    } = page;
    let mut chunk = crate::routes::client_events::finish(&state, chunk).await;
    // A backward page is newest first; a client reads `messages` oldest first.
    chunk.reverse();
    let mut messages = json!({"chunk": chunk, "end": start});
    if let Some(older) = end {
        messages["start"] = serde_json::Value::String(older);
    } else {
        messages["start"] = messages["end"].clone();
    }
    let mut body = json!({
        "room_id": room_id,
        "messages": messages,
        "state": state_events,
        "presence": [],
        "receipts": [],
        "account_data": [],
        "visibility": visibility,
    });
    if let Some(membership) = membership {
        body["membership"] = serde_json::Value::String(membership);
    }
    Ok(Json(body).into_response())
}

/// What a `/messages` page says should be fetched before it is final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wants {
    /// Nothing: the page is what this server can answer.
    Nothing,
    /// The room's history from before the oldest event held (`RoomActor::backfill_anchor`).
    Older,
    /// The history missing from the open timeline gap below this position
    /// (`RoomActor::gap_anchor`).
    Gap(i64),
}

/// One page of `GET /messages`, rendered for a requester, before erasure is applied
/// ([`crate::routes::client_events::finish`]).
struct MessagesPage {
    start: String,
    chunk: Vec<crate::routes::client_events::ViewedEvent>,
    /// The lazy-loaded member events, when the filter asked for them.
    state: Vec<serde_json::Value>,
    end: Option<String>,
    wants: Wants,
}

/// What a `/messages` page is read with beyond its position, direction and size.
#[derive(Clone)]
struct PageOptions {
    requester: hs_auth::requester::Requester,
    filter: crate::routes::client_events::RoomEventFilter,
    to: Option<MessagesBound>,
    local_server: ruma::OwnedServerName,
}

/// One page of `GET /messages`, rendered for `requester`, with what it [`Wants`] fetched. A
/// backward page that reached the oldest event this server holds while the room's history
/// continues before it (`RoomActor::history_before_oldest`) wants [`Wants::Older`]; until
/// `after_backfill` says the caller has fetched that history and is paging again, such a page
/// carries no `end`: with nothing to fetch it from, the oldest held event *is* the end for this
/// server. With `stop_at_gaps`, a backward page stops at an open gap in the middle of the
/// timeline (`RoomActor::paginate_page`) and wants [`Wants::Gap`], with an `end` naming the
/// boundary; without it, it reads across gaps.
///
/// Otherwise a page with events always has an `end` (its last event's position, when the actor
/// says there is nothing beyond it): the empty page after it is the one without, as Synapse
/// answers. A `to` bound cuts the page where it lies.
async fn messages_page<B: KvBackend + 'static>(
    handle: &crate::actor::RoomActorHandle<B>,
    from: Option<PaginationToken>,
    direction: Direction,
    limit: usize,
    options: PageOptions,
    after_backfill: bool,
    stop_at_gaps: bool,
) -> Result<MessagesPage, RoomError> {
    handle
        .query(move |actor| -> Result<_, RoomError> {
            let PageOptions {
                requester,
                filter,
                to,
                local_server,
            } = options;
            // The entry gate: forgetting, or never having had a membership record in a
            // non-world-readable room, refuses the whole call outright -- see
            // `RoomActor::can_read_room`'s doc comment for exactly what this distinguishes from
            // per-event filtering below.
            if !actor.can_read_room(&requester.user_id)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            let page = if stop_at_gaps {
                actor.paginate_page(from, direction, limit)
            } else {
                actor.paginate_page_across_gaps(from, direction, limit)
            };
            let wants = match page.gap {
                Some(top) => Wants::Gap(top),
                None if direction == Direction::Backward
                    && page.reached_edge
                    && actor.history_before_oldest() =>
                {
                    Wants::Older
                }
                None => Wants::Nothing,
            };
            let mut events = page.events;
            let mut next = page.next;
            if let Some(to) = to
                && let Some(cut) = events.iter().position(|e| {
                    actor
                        .timeline_position(e.event_id())
                        .is_some_and(|pos| !to.keeps(pos))
                })
            {
                events.truncate(cut);
                next = None;
            }
            // A page with events always says where the next one starts; the empty page after
            // the last one is the one that says it is the end.
            if next.is_none()
                && let Some(last) = events.last()
                && let Some(pos) = actor.timeline_position(last.event_id())
            {
                next = Some(PaginationToken::new(pos, direction));
            }
            let end = if wants == Wants::Older && !after_backfill {
                None
            } else {
                next
            };
            let start_token = from.unwrap_or_else(|| PaginationToken::new(0, direction));
            let shown: Vec<&hs_model::Event> = events
                .into_iter()
                .filter(|e| filter.matches(e))
                .filter(|e| {
                    actor
                        .event_visible_to(e, &requester.user_id)
                        .unwrap_or(false)
                })
                .collect();
            let state = match shown.first() {
                Some(first) if filter.lazy_loads_members() => {
                    let senders: Vec<&ruma::UserId> =
                        shown.iter().map(|e| e.header().sender.as_ref()).collect();
                    crate::routes::client_events::lazy_member_state(
                        actor,
                        first,
                        &senders,
                        &requester.user_id,
                    )
                }
                _ => Vec::new(),
            };
            let chunk = shown
                .into_iter()
                .map(|e| {
                    crate::routes::client_events::view_event(actor, e, &requester, &local_server)
                })
                .collect::<Vec<_>>();
            Ok(MessagesPage {
                start: start_token.to_string(),
                chunk,
                state,
                end: end.map(|t| t.to_string()),
                wants,
            })
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::Query;
    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;
    use crate::state::RoomState;
    use serde_json::json;

    fn requester(user: &ruma::UserId) -> RoomRequester {
        RoomRequester(Requester {
            user_id: user.to_owned(),
            device_id: None,
            is_guest: false,
            is_admin: false,
            shadow_banned: false,
            suspended: false,
            appservice: None,
            access_token_id: None,
        })
    }

    fn app() -> RoomState<MemoryBackend> {
        let backend = MemoryBackend::new();
        let identity = HomeserverIdentity::for_tests("hs1");
        let rooms = Arc::new(RoomRegistry::open(backend, identity.clone()).unwrap());
        RoomState {
            auth: AuthState::in_memory(),
            rooms,
            identity,
            remote_join: None,
        }
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The bug this session fixed: `/context`'s `state` field must reflect the room's state
    /// pinned to immediately after the target event, not this room's *live* current state --
    /// the same "read a live value instead of one pinned to a point in time" bug class already
    /// closed for `/messages`/`/event`/`/state`/`/members`. Sends a message while the topic is
    /// "first", changes the topic to "second" afterwards, then asserts `/context` for that
    /// message still reports "first" -- proving `state` did not just re-read current state
    /// (which would report "second").
    #[tokio::test]
    async fn context_state_is_pinned_to_the_target_event_not_current_state() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let handle = state
            .rooms
            .create_room(
                alice.to_owned(),
                crate::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    topic: Some("first".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .expect("create should succeed");
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;

        let message = handle
            .send_event(
                alice.to_owned(),
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": "while topic was first"}),
                None,
                2,
            )
            .await
            .expect("message send should succeed");

        handle
            .send_event(
                alice.to_owned(),
                "m.room.topic".to_owned(),
                Some(String::new()),
                json!({"topic": "second"}),
                None,
                3,
            )
            .await
            .expect("topic change should succeed");

        // Sanity check: current state really did move on, so the assertion below is meaningful.
        let current_topic = handle
            .query(|actor| {
                actor
                    .state_event("m.room.topic", "")
                    .unwrap()
                    .and_then(|e| e.json().get("content"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|c| c.get("topic"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                    .map(str::to_owned)
            })
            .await;
        assert_eq!(current_topic.as_deref(), Some("second"));

        let response = get_context::<MemoryBackend>(
            State(state),
            Path((room_id.to_string(), message.event_id().to_string())),
            Query(HashMap::new()),
            requester(alice),
        )
        .await
        .expect("context should succeed")
        .into_response();
        let body = json_body(response).await;

        let state_topic = body["state"]
            .as_array()
            .expect("state should be an array")
            .iter()
            .find(|e| e["type"] == "m.room.topic")
            .and_then(|e| e["content"]["topic"].as_str())
            .map(str::to_owned);
        assert_eq!(
            state_topic.as_deref(),
            Some("first"),
            "context's state must be pinned to the target event, not the room's live current state"
        );
    }

    /// A display-name change and a join are the same event to a client that cannot see what the
    /// previous `m.room.member` event said: both are `m.room.member` with `membership: "join"`.
    /// Element renders the former as "Alice joined the room" when `unsigned.prev_content` is
    /// missing, which is what this test exists to keep fixed. Reads back through `/messages` --
    /// the endpoint a client actually pages a room's timeline with -- rather than calling the
    /// renderer directly, so it fails if the route stops asking for the replaced state even
    /// though the renderer still knows how to attach it.
    ///
    /// Also pins down the two neighbouring fields the same lookup feeds:
    /// `unsigned.replaces_state` must name the superseded event, and `unsigned.prev_sender` its
    /// sender (`refs/matrix-spec/data/api/client-server/definitions/client_event_without_room_id.yaml`).
    #[tokio::test]
    async fn a_display_name_change_carries_the_old_name_in_unsigned_prev_content() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let handle = state
            .rooms
            .create_room(
                alice.to_owned(),
                crate::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .expect("create should succeed");
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;

        let named = handle
            .refresh_own_profile(alice.to_owned(), Some("Alice".to_owned()), None, 2)
            .await
            .expect("setting a display name should succeed")
            .expect("alice is joined, so this mints a membership event");
        let renamed = handle
            .refresh_own_profile(alice.to_owned(), Some("Alice Smith".to_owned()), None, 3)
            .await
            .expect("changing a display name should succeed")
            .expect("alice is joined, so this mints a membership event");

        let response = get_messages::<MemoryBackend>(
            State(state),
            Path(room_id.to_string()),
            Query(MessagesQuery {
                filter: None,
                to: None,
                from: None,
                dir: Some("b".to_owned()),
                limit: Some(50),
            }),
            requester(alice),
        )
        .await
        .expect("messages should succeed")
        .into_response();
        let body = json_body(response).await;
        let chunk = body["chunk"].as_array().expect("chunk should be an array");

        let find = |event_id: &str| {
            chunk
                .iter()
                .find(|e| e["event_id"] == event_id)
                .unwrap_or_else(|| panic!("{event_id} should be in the /messages chunk"))
                .clone()
        };

        let rename = find(renamed.event_id().as_str());
        assert_eq!(
            rename["content"]["displayname"], "Alice Smith",
            "sanity: this is the rename event"
        );
        assert_eq!(
            rename["unsigned"]["prev_content"]["displayname"], "Alice",
            "the rename must carry the old display name, or a client cannot tell it from a join"
        );
        assert_eq!(
            rename["unsigned"]["replaces_state"],
            serde_json::Value::String(named.event_id().to_string()),
            "replaces_state must name the membership event this one superseded"
        );
        assert_eq!(
            rename["unsigned"]["prev_sender"],
            serde_json::Value::String(alice.to_string()),
            "prev_sender must name the superseded event's sender"
        );

        // The first membership event for `(m.room.member, @alice)` replaced nothing, so all three
        // fields must be absent rather than present-and-empty.
        let first_join = chunk
            .iter()
            .filter(|e| e["type"] == "m.room.member" && e["state_key"] == alice.as_str())
            .min_by_key(|e| e["origin_server_ts"].as_i64().unwrap_or(i64::MAX))
            .expect("the room's bootstrap join should be in the chunk")
            .clone();
        assert!(
            first_join["unsigned"].get("prev_content").is_none()
                && first_join["unsigned"].get("replaces_state").is_none()
                && first_join["unsigned"].get("prev_sender").is_none(),
            "a state event that replaced nothing must omit all three fields: {first_join}"
        );

        // A message event is not a state event, so it never carries them either.
        let create = chunk
            .iter()
            .find(|e| e["type"] == "m.room.create")
            .expect("the create event should be in the chunk");
        assert!(
            create["unsigned"].get("prev_content").is_none(),
            "m.room.create replaced nothing: {create}"
        );
    }

    /// A client paginating backwards stops when `end` is absent; it used to be `null` after one
    /// extra empty page, and a reader that takes "present" literally never stopped. The last
    /// page with events has an `end` (Sytest's `/messages` tests ask for it), and the empty page
    /// after it has none.
    #[tokio::test]
    async fn paginating_backwards_reaches_the_start_of_the_room_and_says_so() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let handle = state
            .rooms
            .create_room(
                alice.to_owned(),
                crate::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .expect("create should succeed");
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        for i in 0..5 {
            handle
                .send_event(
                    alice.to_owned(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"msgtype": "m.text", "body": format!("m{i}")}),
                    None,
                    10 + i,
                )
                .await
                .expect("send should succeed");
        }

        let mut from: Option<String> = None;
        let mut pages = 0;
        let mut seen = 0;
        loop {
            let response = get_messages::<MemoryBackend>(
                State(state.clone()),
                Path(room_id.to_string()),
                Query(MessagesQuery {
                    filter: None,
                    to: None,
                    from: from.clone(),
                    dir: Some("b".to_owned()),
                    limit: Some(2),
                }),
                requester(alice),
            )
            .await
            .expect("messages should succeed")
            .into_response();
            let body = json_body(response).await;
            pages += 1;
            seen += body["chunk"].as_array().expect("chunk").len();
            match body.get("end") {
                None => break,
                Some(serde_json::Value::String(token)) => from = Some(token.clone()),
                Some(other) => panic!("`end` must be a token or absent, not {other}"),
            }
            assert!(
                pages < 20,
                "the pagination never reached the start of the room"
            );
        }
        // Every event the room has, in as many pages as a limit of 2 needs, then one empty page
        // without `end` -- Synapse's shape: a page with events always has an `end`.
        let total = handle.query(|actor| actor.events_after(0, 100).len()).await;
        assert_eq!(seen, total);
        assert_eq!(pages, total.div_ceil(2) + 1);
    }

    /// A resolver that knows one token, `synctok`, standing for a sync that covered the room
    /// through `pos`.
    struct OneToken {
        pos: i64,
    }

    #[async_trait::async_trait]
    impl crate::registry::GlobalTokenResolver for OneToken {
        async fn resolve(
            &self,
            _user_id: &ruma::UserId,
            _room_id: &ruma::RoomId,
            raw: &str,
        ) -> Result<Option<Option<i64>>, RoomError> {
            Ok((raw == "synctok").then_some(Some(self.pos)))
        }
    }

    /// Sync, then page back from its `next_batch` (what Sytest's `matrix_get_room_messages` and
    /// most clients do): the newest event the sync handed out is the first one the page shows.
    /// It was left out -- the page started below it -- so a message a sync had just shown was
    /// missing from `/messages` (Sytest's "Guest users can send messages to guest_access rooms if
    /// joined", for any sender).
    #[tokio::test]
    async fn a_backward_page_from_a_sync_token_starts_with_the_newest_event_the_sync_showed() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let handle = state
            .rooms
            .create_room(
                alice.to_owned(),
                crate::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        let hello = handle
            .send_event(
                alice.to_owned(),
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": "hello"}),
                None,
                2,
            )
            .await
            .unwrap();
        let id = hello.event_id().to_owned();
        let pos = handle
            .query(move |actor| actor.timeline_position(&id))
            .await
            .unwrap();
        state
            .rooms
            .install_global_token_resolver(Arc::new(OneToken { pos }));
        let page = |dir: &str| {
            get_messages::<MemoryBackend>(
                State(state.clone()),
                Path(room_id.to_string()),
                Query(MessagesQuery {
                    filter: None,
                    to: None,
                    from: Some("synctok".to_owned()),
                    dir: Some(dir.to_owned()),
                    limit: Some(1),
                }),
                requester(alice),
            )
        };
        let back = json_body(page("b").await.unwrap()).await;
        assert_eq!(
            back["chunk"][0]["event_id"],
            hello.event_id().as_str(),
            "{back}"
        );
        let forward = json_body(page("f").await.unwrap()).await;
        assert!(forward["chunk"].as_array().unwrap().is_empty(), "{forward}");
    }

    /// `GET /rooms/{roomId}/initialSync`: a member sees their membership, the room's state and
    /// its newest messages oldest first; a stranger may read a `world_readable` room the same
    /// way, with no membership, and is refused any other room.
    #[tokio::test]
    async fn room_initial_sync_answers_members_and_world_readable_strangers_only() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let stranger = user_id!("@stranger:hs1");
        let handle = state
            .rooms
            .create_room(
                alice.to_owned(),
                crate::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        for (at, text) in [(2, "one"), (3, "two"), (4, "three")] {
            handle
                .send_event(
                    alice.to_owned(),
                    "m.room.message".to_owned(),
                    None,
                    json!({"msgtype": "m.text", "body": text}),
                    None,
                    at,
                )
                .await
                .unwrap();
        }
        let initial_sync = |who: &ruma::UserId, limit: usize| {
            get_room_initial_sync::<MemoryBackend>(
                State(state.clone()),
                Path(room_id.to_string()),
                Query(RoomInitialSyncQuery { limit: Some(limit) }),
                requester(who),
            )
        };

        let body = json_body(initial_sync(alice, 2).await.unwrap()).await;
        assert_eq!(body["room_id"], room_id.as_str());
        assert_eq!(body["membership"], "join");
        let texts: Vec<&str> = body["messages"]["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["content"]["body"].as_str())
            .collect();
        assert_eq!(texts, vec!["two", "three"]);
        assert!(body["messages"]["start"].is_string());
        assert!(body["messages"]["end"].is_string());
        assert!(
            body["state"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == "m.room.create")
        );

        let err = initial_sync(stranger, 10).await.unwrap_err();
        assert!(matches!(err, RoomError::Forbidden(_)), "{err}");

        handle
            .send_event(
                alice.to_owned(),
                "m.room.history_visibility".to_owned(),
                Some(String::new()),
                json!({"history_visibility": "world_readable"}),
                None,
                9,
            )
            .await
            .unwrap();
        let body = json_body(initial_sync(stranger, 10).await.unwrap()).await;
        assert!(body.get("membership").is_none(), "{body}");
        assert!(!body["state"].as_array().unwrap().is_empty());
    }

    /// A public room alice made, with `n` messages from her, and its handle.
    async fn public_room(
        state: &RoomState<MemoryBackend>,
        alice: &ruma::UserId,
    ) -> (
        crate::actor::RoomActorHandle<MemoryBackend>,
        ruma::OwnedRoomId,
    ) {
        let handle = state
            .rooms
            .create_room(
                alice.to_owned(),
                crate::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .expect("create should succeed");
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        (handle, room_id)
    }

    async fn say(
        handle: &crate::actor::RoomActorHandle<MemoryBackend>,
        who: &ruma::UserId,
        content: serde_json::Value,
    ) -> ruma::OwnedEventId {
        handle
            .send_event(
                who.to_owned(),
                "m.room.message".to_owned(),
                None,
                content,
                None,
                100,
            )
            .await
            .expect("send should succeed")
            .event_id()
            .to_owned()
    }

    async fn join(handle: &crate::actor::RoomActorHandle<MemoryBackend>, who: &ruma::UserId) {
        handle
            .membership(
                who.to_owned(),
                crate::membership::Action::Join,
                who.to_owned(),
                serde_json::json!({}),
                100,
            )
            .await
            .expect("join should succeed");
    }

    async fn messages(
        state: &RoomState<MemoryBackend>,
        room_id: &ruma::RoomId,
        who: &ruma::UserId,
        query: MessagesQuery,
    ) -> serde_json::Value {
        let response = get_messages::<MemoryBackend>(
            State(state.clone()),
            Path(room_id.to_string()),
            Query(query),
            requester(who),
        )
        .await
        .expect("messages should succeed")
        .into_response();
        json_body(response).await
    }

    /// Sytest's "GET /rooms/:room_id/messages returns a message" and "... lazy loads members
    /// correctly": `from=` (a sync that gave no `prev_batch`) is the live end; the page has an
    /// `end` and, lazy-loading, only the sender's member event in `state`; the page after it is
    /// empty and has no `end`.
    #[tokio::test]
    async fn an_empty_from_pages_back_from_the_live_end_with_lazy_members() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let (handle, room_id) = public_room(&state, alice).await;
        say(&handle, alice, json!({"msgtype": "m.text", "body": "hi"})).await;

        let body = messages(
            &state,
            &room_id,
            alice,
            MessagesQuery {
                from: Some(String::new()),
                dir: Some("b".to_owned()),
                filter: Some(r#"{ "lazy_load_members" : true }"#.to_owned()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(body["chunk"][0]["content"]["body"], "hi");
        let state_events = body["state"].as_array().expect("state");
        assert_eq!(state_events.len(), 1, "{body}");
        assert_eq!(state_events[0]["type"], "m.room.member");
        assert_eq!(state_events[0]["state_key"], alice.as_str());
        let end = body["end"]
            .as_str()
            .expect("a page with events has an end")
            .to_owned();

        let body = messages(
            &state,
            &room_id,
            alice,
            MessagesQuery {
                from: Some(end),
                dir: Some("b".to_owned()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(body["chunk"], json!([]));
        assert!(body.get("end").is_none(), "{body}");
        assert!(body.get("state").is_none(), "{body}");
    }

    /// Sytest's "Ephemeral messages received from clients are correctly expired": with a
    /// `types` filter only the messages come back, and one whose
    /// `org.matrix.self_destruct_after` has passed has empty content; one still in its time is
    /// whole.
    #[tokio::test]
    async fn a_types_filter_and_an_expired_ephemeral_message() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let (handle, room_id) = public_room(&state, alice).await;
        let now = crate::metrics::now_ms();
        say(
            &handle,
            alice,
            json!({"msgtype": "m.text", "body": "gone", crate::routes::render::SELF_DESTRUCT_AFTER: now - 1000}),
        )
        .await;
        say(
            &handle,
            alice,
            json!({"msgtype": "m.text", "body": "here", crate::routes::render::SELF_DESTRUCT_AFTER: now + 3_600_000}),
        )
        .await;
        let body = messages(
            &state,
            &room_id,
            alice,
            MessagesQuery {
                filter: Some(r#"{"types":["m.room.message"]}"#.to_owned()),
                ..Default::default()
            },
        )
        .await;
        let chunk = body["chunk"].as_array().expect("chunk");
        assert_eq!(chunk.len(), 2, "{body}");
        assert_eq!(chunk[0]["content"]["body"], "here");
        assert_eq!(chunk[1]["content"], json!({}));
    }

    /// Complement's `TestRoomMessagesLazyLoading`: a forward page with a `to` stops there.
    #[tokio::test]
    async fn to_stops_a_forward_page() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let (handle, room_id) = public_room(&state, alice).await;
        let first = say(&handle, alice, json!({"msgtype": "m.text", "body": "1"})).await;
        say(&handle, alice, json!({"msgtype": "m.text", "body": "2"})).await;
        let third = say(&handle, alice, json!({"msgtype": "m.text", "body": "3"})).await;
        let (p1, p3) = handle
            .query(move |actor| {
                (
                    actor.timeline_position(&first).unwrap(),
                    actor.timeline_position(&third).unwrap(),
                )
            })
            .await;
        let body = messages(
            &state,
            &room_id,
            alice,
            MessagesQuery {
                from: Some(PaginationToken::new(p1, Direction::Forward).to_string()),
                to: Some(PaginationToken::new(p3, Direction::Backward).to_string()),
                dir: Some("f".to_owned()),
                ..Default::default()
            },
        )
        .await;
        let bodies: Vec<_> = body["chunk"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["content"]["body"].clone())
            .collect();
        assert_eq!(bodies, vec![json!("2")], "{body}");
    }

    /// MSC4115 (Complement's `TestMembershipOnEvents`): each event says what the reader's
    /// membership was at it -- `leave` before bob joined, `join` from his join on.
    #[tokio::test]
    async fn every_event_carries_the_readers_membership_at_it() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let (handle, room_id) = public_room(&state, alice).await;
        say(
            &handle,
            alice,
            json!({"msgtype": "m.text", "body": "before"}),
        )
        .await;
        join(&handle, bob).await;
        say(
            &handle,
            alice,
            json!({"msgtype": "m.text", "body": "after"}),
        )
        .await;
        let body = messages(
            &state,
            &room_id,
            bob,
            MessagesQuery {
                dir: Some("f".to_owned()),
                limit: Some(100),
                ..Default::default()
            },
        )
        .await;
        let mut joined = false;
        for event in body["chunk"].as_array().unwrap() {
            if event["type"] == "m.room.member" && event["state_key"] == bob.as_str() {
                joined = true;
            }
            let want = if joined { "join" } else { "leave" };
            assert_eq!(event["unsigned"]["membership"], want, "{event}");
        }
        assert!(joined);
    }

    /// Sytest's "Only original members of the room can see messages from erased users": once
    /// alice's account is erased, bob, who was there, still reads her message; carol, who
    /// joined after it, reads it pruned.
    #[tokio::test]
    async fn an_erased_senders_message_is_pruned_for_who_was_not_there() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let carol = user_id!("@carol:hs1");
        let (handle, room_id) = public_room(&state, alice).await;
        join(&handle, bob).await;
        let said = say(
            &handle,
            alice,
            json!({"msgtype": "m.text", "body": "body1"}),
        )
        .await;
        join(&handle, carol).await;
        state
            .auth
            .store
            .create_user(hs_auth::store::UserRecord::new(alice.to_owned(), 1))
            .await
            .unwrap();
        state.auth.store.erase_user(alice, 2).await.unwrap();
        let content_for = |who: &'static ruma::UserId| {
            let state = state.clone();
            let room_id = room_id.clone();
            let said = said.clone();
            async move {
                let body = messages(
                    &state,
                    &room_id,
                    who,
                    MessagesQuery {
                        limit: Some(100),
                        ..Default::default()
                    },
                )
                .await;
                body["chunk"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["event_id"] == said.as_str())
                    .map(|e| e["content"].clone())
            }
        };
        assert_eq!(content_for(bob).await.unwrap()["body"], "body1");
        assert_eq!(content_for(carol).await, Some(json!({})));
    }

    /// Sytest's "/context/ on non world readable room does not work" (a stranger is refused
    /// with 403, not told the event does not exist) and "/context/ with lazy_load_members
    /// filter works" (only the senders of what is returned are in `state`).
    #[tokio::test]
    async fn context_refuses_a_stranger_and_lazy_loads_senders() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
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
        let said = say(
            &handle,
            alice,
            json!({"msgtype": "m.text", "body": "hello"}),
        )
        .await;
        let err = get_context::<MemoryBackend>(
            State(state.clone()),
            Path((room_id.to_string(), said.to_string())),
            Query(HashMap::new()),
            requester(bob),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RoomError::Forbidden(_)), "{err:?}");

        let (handle, room_id) = public_room(&state, alice).await;
        join(&handle, bob).await;
        say(&handle, alice, json!({"msgtype": "m.text", "body": "1"})).await;
        let last = say(&handle, alice, json!({"msgtype": "m.text", "body": "2"})).await;
        let response = get_context::<MemoryBackend>(
            State(state.clone()),
            Path((room_id.to_string(), last.to_string())),
            Query(HashMap::from([
                ("limit".to_owned(), "1".to_owned()),
                (
                    "filter".to_owned(),
                    r#"{"lazy_load_members": true}"#.to_owned(),
                ),
            ])),
            requester(alice),
        )
        .await
        .unwrap();
        let body = json_body(response).await;
        let members: Vec<_> = body["state"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["state_key"].clone())
            .collect();
        assert_eq!(members, vec![json!(alice.as_str())], "{body}");
        assert_eq!(body["event"]["unsigned"]["membership"], "join");
    }

    /// Complement's `TestGetRoomMembersAtPoint`: `at` gives the members as of the token, not
    /// now.
    #[tokio::test]
    async fn members_at_a_token_are_the_members_then() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let (handle, room_id) = public_room(&state, alice).await;
        let said = say(&handle, alice, json!({"msgtype": "m.text", "body": "hi"})).await;
        let pos = handle
            .query(move |actor| actor.timeline_position(&said).unwrap())
            .await;
        join(&handle, bob).await;
        let members = |at: Option<String>| {
            let state = state.clone();
            let room_id = room_id.clone();
            async move {
                let response = get_members::<MemoryBackend>(
                    State(state),
                    Path(room_id.to_string()),
                    Query(MembersQuery {
                        at,
                        ..Default::default()
                    }),
                    requester(alice),
                )
                .await
                .unwrap();
                let mut keys: Vec<String> = json_body(response).await["chunk"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["state_key"].as_str().unwrap().to_owned())
                    .collect();
                keys.sort();
                keys
            }
        };
        assert_eq!(
            members(Some(
                PaginationToken::new(pos + 1, Direction::Backward).to_string()
            ))
            .await,
            vec![alice.to_string()]
        );
        assert_eq!(
            members(None).await,
            vec![alice.to_string(), bob.to_string()]
        );
    }

    /// `at` a token no event precedes (the room's very start) is `404 M_NOT_FOUND`, as
    /// Synapse answers ("Can't find event for token"), not the current members.
    #[tokio::test]
    async fn members_at_a_token_before_every_event_are_not_found() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let (handle, room_id) = public_room(&state, alice).await;
        join(&handle, bob).await;
        let first = handle
            .query(|actor| {
                let create = actor
                    .state_event("m.room.create", "")
                    .unwrap()
                    .unwrap()
                    .event_id()
                    .to_owned();
                actor.timeline_position(&create).unwrap()
            })
            .await;
        let response = get_members::<MemoryBackend>(
            State(state.clone()),
            Path(room_id.to_string()),
            Query(MembersQuery {
                at: Some(PaginationToken::new(first, Direction::Backward).to_string()),
                ..Default::default()
            }),
            requester(alice),
        )
        .await
        .expect_err("no event before the token");
        let error = response.to_matrix_error();
        assert_eq!(
            error.status,
            axum::http::StatusCode::NOT_FOUND,
            "{response}"
        );
        assert_eq!(error.errcode, hs_http::error::MatrixErrorCode::NotFound);
    }
}
