//! `GET /joined_rooms`, `GET /publicRooms`, `POST /publicRooms`.

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use serde::Deserialize;
use serde_json::json;

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

/// `GET /joined_rooms`: every room `crate::store::UserStore` currently has this user recorded as
/// `"join"` in.
///
/// # Errors
/// Returns [`UserError`] on a store failure.
pub async fn get_joined_rooms<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    let memberships = state
        .hub
        .store()
        .list_memberships(&requester.user_id)
        .await?;
    let joined: Vec<String> = memberships
        .into_iter()
        .filter(|m| m.membership == "join")
        .map(|m| m.room_id.to_string())
        .collect();
    Ok(Json(json!({"joined_rooms": joined})).into_response())
}

/// Query parameters for `GET /publicRooms`, and the JSON body shape `POST /publicRooms` accepts
/// (the spec gives both the same fields; `POST` additionally allows `filter.generic_search_term`).
#[derive(Debug, Default, Deserialize)]
pub struct PublicRoomsQuery {
    /// Maximum rooms to return.
    pub limit: Option<usize>,
    /// A pagination token from a previous call -- this crate's directory has no stable secondary
    /// ordering beyond room id, so `since` is just an opaque decimal offset into that order.
    pub since: Option<String>,
    /// A server name to fetch another server's public room list -- not implemented: this crate
    /// only ever answers with its own directory (see [`crate::store::UserStore::list_public_rooms`]'s
    /// coverage caveat).
    #[allow(dead_code)]
    pub server: Option<String>,
}

/// `POST /publicRooms`'s body: [`PublicRoomsQuery`]'s fields plus an optional search filter.
#[derive(Debug, Default, Deserialize)]
pub struct PublicRoomsBody {
    /// The paging and server-selection parameters `GET /publicRooms` takes in its query string,
    /// flattened into this body so both methods share one type.
    #[serde(flatten)]
    pub query: PublicRoomsQuery,
    /// `{"generic_search_term": "..."}`: case-insensitive substring match against a room's name,
    /// topic and canonical alias. Applied.
    pub filter: Option<PublicRoomsFilter>,
}

/// See [`PublicRoomsBody::filter`].
#[derive(Debug, Default, Deserialize)]
pub struct PublicRoomsFilter {
    /// The search term.
    pub generic_search_term: Option<String>,
}

fn render_chunk(
    entries: Vec<crate::store::PublicRoomEntry>,
    search: Option<&str>,
) -> Vec<serde_json::Value> {
    entries
        .into_iter()
        .filter(|e| {
            let Some(term) = search else { return true };
            let term = term.to_ascii_lowercase();
            [
                e.name.as_deref(),
                e.topic.as_deref(),
                e.canonical_alias.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|s| s.to_ascii_lowercase().contains(&term))
        })
        .map(|e| {
            json!({
                "room_id": e.room_id,
                "name": e.name,
                "topic": e.topic,
                "canonical_alias": e.canonical_alias,
                "avatar_url": e.avatar_url,
                "num_joined_members": e.num_joined_members,
                "world_readable": e.world_readable,
                "guest_can_join": e.guest_can_join,
            })
        })
        .collect()
}

async fn public_rooms<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    state: &UserState<B, R>,
    query: PublicRoomsQuery,
    search: Option<&str>,
) -> Result<Response, UserError> {
    let mut all = state.hub.store().list_public_rooms().await?;
    all.sort_by(|a, b| a.room_id.cmp(&b.room_id));
    let offset: usize = query
        .since
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit = query.limit.unwrap_or(10).min(200);
    let total = all.len();
    let page: Vec<_> = all.into_iter().skip(offset).take(limit).collect();
    let next_batch = if offset + page.len() < total {
        Some((offset + page.len()).to_string())
    } else {
        None
    };
    let prev_batch = if offset > 0 {
        Some(offset.saturating_sub(limit).to_string())
    } else {
        None
    };
    let chunk = render_chunk(page, search);
    Ok(Json(json!({
        "chunk": chunk,
        "total_room_count_estimate": total,
        "next_batch": next_batch,
        "prev_batch": prev_batch,
    }))
    .into_response())
}

/// `GET /publicRooms`.
///
/// # Errors
/// Returns [`UserError`] on a store failure.
pub async fn get_public_rooms<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Query(query): Query<PublicRoomsQuery>,
    UserRequester(_requester): UserRequester,
) -> Result<Response, UserError> {
    public_rooms(&state, query, None).await
}

/// `POST /publicRooms`.
///
/// # Errors
/// Returns [`UserError`] on a store failure.
pub async fn post_public_rooms<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    UserRequester(_requester): UserRequester,
    body: PermissiveJson<PublicRoomsBody>,
) -> Result<Response, UserError> {
    let body = body.0;
    let search = body
        .filter
        .as_ref()
        .and_then(|f| f.generic_search_term.as_deref());
    public_rooms(&state, body.query, search).await
}
