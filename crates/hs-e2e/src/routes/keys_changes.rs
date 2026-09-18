//! `GET /keys/changes`.
//!
//! The spec's `from`/`to` parameters are opaque `/sync` batch tokens. `hs-user`'s sync
//! implementation does not exist yet (track 05, week 8), so there is no real sync token format to
//! parse yet; this crate defines the token as the decimal string of a
//! [`crate::store::DeviceKeyStore`] stream position and documents that as the seam sync must
//! either adopt verbatim or wrap. See `docs/status/08-e2ee.md`.

use axum::Json;
use axum::extract::{Query, State};
use hs_kv::KvBackend;
use serde::Deserialize;
use serde_json::json;

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};

/// `?from=&to=` — `to` is optional (an absent `to` means "up to now").
#[derive(Debug, Deserialize)]
pub struct KeysChangesQuery {
    from: String,
    to: Option<String>,
}

fn parse_stream_pos(raw: &str) -> Result<u64, E2eError> {
    raw.parse::<u64>()
        .map_err(|_| E2eError::BadRequest(format!("not a valid device-list stream token: {raw:?}")))
}

/// `GET /keys/changes?from=...&to=...`.
pub async fn get_keys_changes<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(_requester): E2eRequester,
    Query(query): Query<KeysChangesQuery>,
) -> Result<Json<serde_json::Value>, E2eError> {
    let from = parse_stream_pos(&query.from)?;
    let to = query.to.as_deref().map(parse_stream_pos).transpose()?;
    let changed = state.store.changed_users_since(from, to).await?;
    // This crate has no notion of room membership, so it cannot compute `left` (users who
    // stopped sharing an encrypted room) -- see `crate::appservice_feed`'s doc comment for the
    // same limitation on the appservice-facing side. Always empty here, documented as a seam.
    Ok(Json(json!({
        "changed": changed.into_iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        "left": Vec::<String>::new(),
    })))
}
