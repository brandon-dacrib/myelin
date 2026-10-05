//! `GET /rooms/{roomId}/relations/{eventId}(/{relType}(/{eventType}))`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{EventId, RoomId};
use serde::Deserialize;
use serde_json::json;

use crate::error::RoomError;
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

/// Query parameters for `GET /rooms/{roomId}/relations/...`.
#[derive(Debug, Default, Deserialize)]
pub struct RelationsQuery {
    /// Where to continue from: a `next_batch` from an earlier page, or a sync or `/messages`
    /// token (only children after it, forwards; before it, backwards).
    pub from: Option<String>,
    /// Where to stop: children beyond it are not returned.
    pub to: Option<String>,
    /// How many children; defaults to 5 (Synapse's default), capped at 1000.
    pub limit: Option<usize>,
    /// `"b"` (newest first, the default) or `"f"`.
    pub dir: Option<String>,
    /// Also the children's children, three levels down (MSC3981, stable in v1.10).
    pub recurse: Option<bool>,
    /// MSC3981's unstable name for `recurse`.
    #[serde(rename = "org.matrix.msc3981.recurse")]
    pub recurse_unstable: Option<bool>,
}

/// How far `recurse` follows relations below the parent: Synapse's depth.
const RECURSION_DEPTH: usize = 3;

/// The relation type of `event`, if it has one.
fn rel_type_of(event: &hs_model::Event) -> Option<&str> {
    event
        .json()
        .get("content")
        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        .and_then(|c| c.get("m.relates_to"))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        .and_then(|r| r.get("rel_type"))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
}

/// `GET /rooms/{roomId}/relations/{eventId}(/{relType}(/{eventType}))`: the children of an
/// event, paginated in timeline order (newest first unless `dir=f`), `limit` at a time, with a
/// `next_batch` while there are more and a `prev_batch` naming the `from` the page was read
/// from. `from` and `to` take this endpoint's own `next_batch`, a `/messages` token or a sync
/// token (Complement's `TestRelationsPaginationSync` pages forwards from a sync's
/// `next_batch`). Children the reader may not see under `m.room.history_visibility` are left
/// out, and so are the edits of a redacted parent, whose content they would give back. A reader
/// who may not read the room gets `403`.
#[allow(clippy::too_many_lines)]
async fn relations_response<B: KvBackend + 'static>(
    state: RoomState<B>,
    requester: hs_auth::requester::Requester,
    room_id: String,
    event_id: String,
    rel_type: Option<String>,
    event_type: Option<String>,
    query: RelationsQuery,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let event_id = parse_event_id(&event_id)?;
    let direction = query
        .dir
        .as_deref()
        .and_then(Direction::from_query)
        .unwrap_or(Direction::Backward);
    let limit = query.limit.unwrap_or(5).min(1000);
    let recurse = query.recurse.or(query.recurse_unstable).unwrap_or(false);
    let from = crate::routes::query::resolve_token(
        &state,
        &requester.user_id,
        &room_id,
        query.from.as_deref(),
        direction,
    )
    .await?;
    let opposite = match direction {
        Direction::Backward => Direction::Forward,
        Direction::Forward => Direction::Backward,
    };
    let to = crate::routes::query::resolve_token(
        &state,
        &requester.user_id,
        &room_id,
        query.to.as_deref(),
        opposite,
    )
    .await?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let local_server = state.identity.server_name.clone();
    let (viewed, next) = handle
        .query(move |actor| -> Result<_, RoomError> {
            if !actor.can_read_room(&requester.user_id)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            let parent_redacted = actor
                .event_by_id(&event_id)
                .is_some_and(|e| e.header().flags.is_redacted());
            // The children (and, with `recurse`, theirs), each once, with their positions.
            let mut seen = std::collections::HashSet::new();
            let mut found: Vec<(i64, &hs_model::Event)> = Vec::new();
            let mut level = vec![event_id.clone()];
            for depth in 0..=RECURSION_DEPTH {
                if level.is_empty() || (depth > 0 && !recurse) {
                    break;
                }
                let mut below = Vec::new();
                for parent in &level {
                    for child in actor.relations_of(parent, None) {
                        if !seen.insert(child.event_id().to_owned()) {
                            continue;
                        }
                        below.push(child.event_id().to_owned());
                        if let Some(pos) = actor.timeline_position(child.event_id()) {
                            found.push((pos, child));
                        }
                    }
                }
                level = below;
            }
            found.retain(|(pos, e)| {
                rel_type
                    .as_deref()
                    .is_none_or(|want| rel_type_of(e) == Some(want))
                    && event_type
                        .as_deref()
                        .is_none_or(|t| e.header().event_type == t)
                    && !(parent_redacted && rel_type_of(e) == Some("m.replace"))
                    && match (from, direction) {
                        (None, _) => true,
                        (Some(from), Direction::Forward) => *pos > from.room_pos,
                        (Some(from), Direction::Backward) => *pos < from.room_pos,
                    }
                    && match (to, direction) {
                        (None, _) => true,
                        (Some(to), Direction::Forward) => *pos < to.room_pos,
                        (Some(to), Direction::Backward) => *pos > to.room_pos,
                    }
                    && actor
                        .event_visible_to(e, &requester.user_id)
                        .unwrap_or(false)
            });
            found.sort_by_key(|(pos, _)| *pos);
            if direction == Direction::Backward {
                found.reverse();
            }
            let next = (found.len() > limit)
                .then(|| found.get(limit.saturating_sub(1)).map(|(pos, _)| *pos))
                .flatten()
                .map(|pos| PaginationToken::new(pos, direction).to_string());
            let viewed = found
                .into_iter()
                .take(limit)
                .map(|(_, e)| {
                    crate::routes::client_events::view_event(actor, e, &requester, &local_server)
                })
                .collect::<Vec<_>>();
            Ok((viewed, next))
        })
        .await?;
    let chunk = crate::routes::client_events::finish(&state, viewed).await;
    let mut body = json!({"chunk": chunk});
    if let Some(next) = next {
        body["next_batch"] = serde_json::Value::String(next);
    }
    if let Some(from) = query.from.filter(|f| !f.is_empty()) {
        body["prev_batch"] = serde_json::Value::String(from);
    }
    if recurse {
        body["recursion_depth"] = json!(RECURSION_DEPTH);
    }
    Ok(Json(body).into_response())
}

/// `GET /rooms/{roomId}/relations/{eventId}`.
pub async fn get_relations<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id)): Path<(String, String)>,
    Query(query): Query<RelationsQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    relations_response(state, requester, room_id, event_id, None, None, query).await
}

/// `GET /rooms/{roomId}/relations/{eventId}/{relType}`.
pub async fn get_relations_by_rel_type<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id, rel_type)): Path<(String, String, String)>,
    Query(query): Query<RelationsQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    relations_response(
        state,
        requester,
        room_id,
        event_id,
        Some(rel_type),
        None,
        query,
    )
    .await
}

/// `GET /rooms/{roomId}/relations/{eventId}/{relType}/{eventType}`.
pub async fn get_relations_by_rel_type_and_event_type<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id, rel_type, event_type)): Path<(String, String, String, String)>,
    Query(query): Query<RelationsQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    relations_response(
        state,
        requester,
        room_id,
        event_id,
        Some(rel_type),
        Some(event_type),
        query,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hs_auth::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;

    fn requester(user: &ruma::UserId) -> RoomRequester {
        RoomRequester(hs_auth::requester::Requester {
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

    /// Complement's `TestRelationsPagination`: ten thread replies, three at a time, newest first
    /// by default and oldest first with `dir=f`, each page's `next_batch` leading to the next.
    #[tokio::test]
    async fn relations_page_both_ways_with_next_batch() {
        let identity = HomeserverIdentity::for_tests("hs1");
        let state = RoomState {
            auth: AuthState::in_memory(),
            rooms: Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).unwrap()),
            identity,
            remote_join: None,
        };
        let alice = user_id!("@alice:hs1");
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
        let send = |content: serde_json::Value| {
            let handle = handle.clone();
            async move {
                handle
                    .send_event(
                        alice.to_owned(),
                        "m.room.message".to_owned(),
                        None,
                        content,
                        None,
                        2,
                    )
                    .await
                    .unwrap()
                    .event_id()
                    .to_string()
            }
        };
        let root = send(json!({"msgtype": "m.text", "body": "root"})).await;
        let mut replies = Vec::new();
        for i in 0..10 {
            replies.push(
                send(json!({
                    "msgtype": "m.text",
                    "body": format!("reply {i}"),
                    "m.relates_to": {"event_id": root, "rel_type": "m.thread"},
                }))
                .await,
            );
        }
        let page = |dir: &str, from: Option<String>| {
            let state = state.clone();
            let room_id = room_id.to_string();
            let root = root.clone();
            let dir = dir.to_owned();
            async move {
                let response = get_relations::<MemoryBackend>(
                    State(state),
                    Path((room_id, root)),
                    Query(RelationsQuery {
                        from,
                        limit: Some(3),
                        dir: Some(dir),
                        ..Default::default()
                    }),
                    requester(alice),
                )
                .await
                .unwrap();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let ids: Vec<String> = body["chunk"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["event_id"].as_str().unwrap().to_owned())
                    .collect();
                (ids, body["next_batch"].as_str().map(str::to_owned))
            }
        };
        let (ids, next) = page("b", None).await;
        assert_eq!(
            ids,
            vec![replies[9].clone(), replies[8].clone(), replies[7].clone()]
        );
        let (ids, _) = page("b", next).await;
        assert_eq!(
            ids,
            vec![replies[6].clone(), replies[5].clone(), replies[4].clone()]
        );
        let (ids, next) = page("f", None).await;
        assert_eq!(ids, replies[0..3].to_vec());
        let (ids, _) = page("f", next).await;
        assert_eq!(ids, replies[3..6].to_vec());
        // The last page has no `next_batch`.
        let mut from = None;
        let mut seen = 0;
        loop {
            let (ids, next) = page("f", from).await;
            seen += ids.len();
            match next {
                Some(next) => from = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, 10);
    }
}
