//! `GET /sync`.

use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use serde::Deserialize;

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};
use crate::sync::{self, SyncParams};
use crate::token::SyncToken;

/// The longest a caller may ask this server to hold a `/sync` connection open for. Matches
/// Synapse's own default cap (`max_lag`-adjacent bound in `sync.py`), high enough that a real
/// client's `timeout=30000`-style requests are never truncated, low enough to bound how long one
/// HTTP worker is tied up per idle client.
const MAX_TIMEOUT: Duration = Duration::from_secs(60);

/// Query parameters for `GET /sync`.
#[derive(Debug, Deserialize)]
pub struct SyncQuery {
    /// Inline JSON or a previously uploaded filter id.
    pub filter: Option<String>,
    /// The `since` token from a previous sync.
    pub since: Option<String>,
    /// Whether to send full state for every room regardless of what changed.
    #[serde(default)]
    pub full_state: bool,
    /// Presence-setting side effect. Parsed, not applied: this crate does not implement presence
    /// yet (`crate::sync`'s module docs).
    #[allow(dead_code)]
    pub set_presence: Option<String>,
    /// Milliseconds to long-poll for.
    pub timeout: Option<u64>,
}

/// `GET /sync`.
///
/// # Errors
/// Returns [`UserError`] if `since` or `filter` do not parse, or on a store/room failure.
pub async fn get_sync<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Query(query): Query<SyncQuery>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    let since = query
        .since
        .as_deref()
        .map(SyncToken::decode)
        .transpose()?;
    let filter = crate::filter::resolve(
        state.hub.store(),
        &requester.user_id,
        query.filter.as_deref(),
    )
    .await?;
    let timeout = query
        .timeout
        .map(Duration::from_millis)
        .unwrap_or_default()
        .min(MAX_TIMEOUT);

    let (response, token) = sync::build(
        &state.hub,
        &requester.user_id,
        SyncParams {
            since,
            full_state: query.full_state,
            timeout,
            filter,
        },
    )
    .await?;

    if let Some(device_id) = &requester.device_id {
        state
            .hub
            .store()
            .record_device_cursor(&requester.user_id, device_id, token.feed_seq)
            .await?;
    }

    Ok(Json(response).into_response())
}
