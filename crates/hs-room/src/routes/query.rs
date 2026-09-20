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
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let content = handle
        .query(move |actor| {
            actor
                .state_event_for_reader(&requester.user_id, &event_type, &state_key)
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
    let found: Result<serde_json::Value, RoomError> = handle
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
            let bundle = actor.relation_bundle(event.event_id(), &requester.user_id);
            let txn_id = actor.transaction_id_for(
                event.event_id(),
                &requester.user_id,
                requester.device_id.as_deref(),
            );
            Ok(attach_replaced_state(
                crate::routes::render::attach_transaction_id(
                    crate::routes::render::client_event_json_bundled(event, &bundle),
                    txn_id,
                ),
                actor.replaced_state_for(event, &requester.user_id).as_ref(),
            ))
        })
        .await;
    found.map(|v| Json(v).into_response())
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
            // Same "not found, not forbidden" shape as `get_event`: a target the requester may
            // not see per `m.room.history_visibility` is reported identically to one that does
            // not exist at all.
            if !actor
                .event_visible_to(target, &requester.user_id)
                .unwrap_or(false)
            {
                return None;
            }
            let visible = |e: &&&hs_model::Event| {
                actor
                    .event_visible_to(e, &requester.user_id)
                    .unwrap_or(false)
            };
            let render = |e: &hs_model::Event| {
                let bundle = actor.relation_bundle(e.event_id(), &requester.user_id);
                let txn_id = actor.transaction_id_for(
                    e.event_id(),
                    &requester.user_id,
                    requester.device_id.as_deref(),
                );
                attach_replaced_state(
                    crate::routes::render::attach_transaction_id(
                        crate::routes::render::client_event_json_bundled(e, &bundle),
                        txn_id,
                    ),
                    actor.replaced_state_for(e, &requester.user_id).as_ref(),
                )
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
            let events_before: Vec<_> = all[pos + 1..end]
                .iter()
                .filter(visible)
                .copied()
                .map(render)
                .collect();
            // "events_after" (newer than target) in chronological order (nearest to the target
            // first, i.e. oldest of the "after" set first): `all[start..pos]` is newest-first, so
            // reverse it.
            let start = pos.saturating_sub(limit);
            let events_after: Vec<_> = all[start..pos]
                .iter()
                .rev()
                .filter(visible)
                .copied()
                .map(render)
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
            let state_json = actor
                .state_at_event(target.event_id())
                .ok()??
                .state
                .iter()
                .map(|e| {
                    attach_replaced_state(
                        client_event_json(e),
                        actor.replaced_state_for(e, &requester.user_id).as_ref(),
                    )
                })
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
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    // `members_for_reader`, not `members`: a departed member must not see a member who joined
    // after they left (`room_leave_test.go`'s `TestLeftRoomFixture`).
    let chunk = handle
        .query(move |actor| {
            actor.members_for_reader(&requester.user_id).map(|found| {
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
///
/// `from`, if present, is tried first as this crate's own
/// [`PaginationToken`]; if that fails to parse, [`crate::registry::RoomRegistry::global_token_resolver`]
/// (if one is installed) gets a chance to recognize a different token format instead of an
/// outright `400` -- see [`crate::registry::GlobalTokenResolver`]'s doc comment for why this
/// indirection exists (in production, `hs-user` installs one so this endpoint accepts a token
/// minted by `/sync`, matching every real Matrix client's ordinary sync-then-paginate flow).
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
    let from = match query.from.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<PaginationToken>() {
            Ok(token) => Some(token),
            Err(_) => match state.rooms.global_token_resolver() {
                Some(resolver) => {
                    match resolver.resolve(&requester.user_id, &room_id, raw).await? {
                        // Not shaped like the resolver's own tokens either: neither format
                        // matched, so this really is an invalid token.
                        None => return Err(RoomError::InvalidPaginationToken),
                        // One of the resolver's own tokens, but no position for this room --
                        // treat exactly like an absent `from` (see the trait doc comment).
                        Some(None) => None,
                        Some(Some(pos)) => Some(PaginationToken::new(pos, direction)),
                    }
                }
                None => return Err(RoomError::InvalidPaginationToken),
            },
        },
    };
    let limit = query.limit.unwrap_or(10).min(1000);

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
    let (start, chunk, end) = handle
        .query(move |actor| -> Result<_, RoomError> {
            // The entry gate: forgetting, or never having had a membership record in a
            // non-world-readable room, refuses the whole call outright -- see
            // `RoomActor::can_read_room`'s doc comment for exactly what this distinguishes from
            // per-event filtering below.
            if !actor.can_read_room(&requester.user_id)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            let (events, next) = actor.paginate(from, direction, limit);
            let start_token = from.unwrap_or_else(|| PaginationToken::new(0, direction));
            let chunk = events
                .into_iter()
                .filter(|e| {
                    actor
                        .event_visible_to(e, &requester.user_id)
                        .unwrap_or(false)
                })
                .map(|e| {
                    let bundle = actor.relation_bundle(e.event_id(), &requester.user_id);
                    let txn_id = actor.transaction_id_for(
                        e.event_id(),
                        &requester.user_id,
                        requester.device_id.as_deref(),
                    );
                    attach_replaced_state(
                        crate::routes::render::attach_transaction_id(
                            crate::routes::render::client_event_json_bundled(e, &bundle),
                            txn_id,
                        ),
                        actor.replaced_state_for(e, &requester.user_id).as_ref(),
                    )
                })
                .collect::<Vec<_>>();
            Ok((start_token.to_string(), chunk, next.map(|t| t.to_string())))
        })
        .await?;

    Ok(Json(json!({"start": start, "chunk": chunk, "end": end})).into_response())
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
}
