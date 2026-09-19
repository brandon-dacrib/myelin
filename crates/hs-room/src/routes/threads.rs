//! `GET /rooms/{roomId}/threads` (client-server API "Threading", added in v1.4).
//!
//! Ported from the spec's documented behaviour
//! (`refs/matrix-spec/content/client-server-api/modules/threading.md` and
//! `refs/matrix-spec/data/api/client-server/threads_list.yaml`, CC-BY-4.0): the thread roots in a
//! room, most-recently-active thread first, optionally filtered to threads the requester has
//! participated in. See [`crate::actor::RoomActor::thread_roots`] for the ordering and
//! participation rules.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::RoomId;
use serde::Deserialize;
use serde_json::json;

use crate::error::RoomError;
use crate::routes::render::{attach_transaction_id, client_event_json_bundled};
use crate::state::{RoomRequester, RoomState};

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// Query parameters for `GET /rooms/{roomId}/threads`.
#[derive(Debug, Deserialize)]
pub struct ThreadsQuery {
    /// `"all"` (default) or `"participated"`.
    pub include: Option<String>,
    /// Maximum number of thread roots to return; defaults to 100, capped at 1000.
    pub limit: Option<usize>,
    /// An opaque offset into the (deterministically ordered) thread list, from a previous
    /// response's `next_batch`. Absent means "start from the most recently active thread".
    ///
    /// This is a plain decimal offset, not this crate's timeline [`crate::timeline::PaginationToken`]
    /// (thread order is "most recently active thread first", not a timeline position at all, so
    /// that token type does not apply here).
    pub from: Option<String>,
}

/// `GET /rooms/{roomId}/threads`.
///
/// Each thread root carries `unsigned.m.relations` (and `unsigned.transaction_id` if the viewer
/// is the device that sent it), exactly like every other event this crate renders.
pub async fn get_threads<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    Query(query): Query<ThreadsQuery>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let participated_only = query.include.as_deref() == Some("participated");
    let limit = query.limit.unwrap_or(100).min(1000);
    let offset: usize = match query.from.as_deref() {
        None => 0,
        Some(raw) => raw.parse().map_err(|_| RoomError::InvalidPaginationToken)?,
    };

    // A non-existent room reports the same `403 M_FORBIDDEN` as "you aren't a member of the
    // room" (matching `crate::routes::query::get_messages`'s documented choice), rather than
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

    let (chunk, next_batch) = handle
        .query(move |actor| -> Result<_, RoomError> {
            if !actor.can_read_room(&requester.user_id)? {
                return Err(RoomError::Forbidden(
                    "you aren't a member of the room".into(),
                ));
            }
            let roots: Vec<&hs_model::Event> = actor
                .thread_roots(&requester.user_id, participated_only)
                .into_iter()
                .filter(|e| {
                    actor
                        .event_visible_to(e, &requester.user_id)
                        .unwrap_or(false)
                })
                .collect();
            let end = (offset + limit).min(roots.len());
            let page = roots.get(offset..end).unwrap_or_default();
            let chunk: Vec<serde_json::Value> = page
                .iter()
                .map(|e| {
                    let bundle = actor.relation_bundle(e.event_id(), &requester.user_id);
                    let txn_id = actor.transaction_id_for(
                        e.event_id(),
                        &requester.user_id,
                        requester.device_id.as_deref(),
                    );
                    attach_transaction_id(client_event_json_bundled(e, &bundle), txn_id)
                })
                .collect();
            let next_batch = (end < roots.len()).then(|| end.to_string());
            Ok((chunk, next_batch))
        })
        .await?;

    let mut body = json!({"chunk": chunk});
    if let Some(next_batch) = next_batch {
        body["next_batch"] = json!(next_batch);
    }
    Ok(Json(body).into_response())
}
