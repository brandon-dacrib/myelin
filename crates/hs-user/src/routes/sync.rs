//! `GET /sync`.

use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use serde::Deserialize;

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::routes::presence::VALID_PRESENCE;
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
    /// The presence state this poll puts the caller in. Omitted means `online` -- polling
    /// `/sync` is itself the signal that a client is there, which is the spec's own default.
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
    let since = query.since.as_deref().map(SyncToken::decode).transpose()?;
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

    // The presence side effect, before the long poll rather than after it: a client that polls
    // with a 30-second timeout is present *now*, and everyone who shares a room with them should
    // learn it now rather than half a minute later.
    //
    // The spec: omitting `set_presence` marks the client online, `offline` means "do not mark me
    // online" (leave whatever is stored alone -- it does not say to mark them offline), and
    // `unavailable` marks them idle.
    match query.set_presence.as_deref() {
        Some("offline") => {}
        Some(other) => {
            if !VALID_PRESENCE.contains(&other) {
                return Err(UserError::InvalidId(format!(
                    "set_presence must be one of {VALID_PRESENCE:?}, got {other:?}"
                )));
            }
            state.hub.touch_presence(&requester.user_id, other).await?;
        }
        None => {
            state
                .hub
                .touch_presence(&requester.user_id, "online")
                .await?;
        }
    }

    let (response, token) = sync::build(
        &state.hub,
        &state.e2e,
        &requester.user_id,
        SyncParams {
            since,
            full_state: query.full_state,
            timeout,
            filter,
            device_id: requester.device_id.clone(),
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
