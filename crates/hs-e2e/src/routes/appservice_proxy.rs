//! MSC3983 (`POST /_matrix/client/unstable/org.matrix.msc3983/keys/claim`) and MSC3984
//! (`POST /_matrix/client/unstable/org.matrix.msc3984/keys/query`): the appservice-facing key
//! proxies track 11 is blocked on (`docs/status/11-appservices-and-bridges.md`).
//!
//! Both MSCs exist so an appservice can ask this server to claim/query keys **on behalf of any
//! user**, not only the ones its registration namespaces masquerade as — the point is letting a
//! bridge encrypt a first message to a real Matrix user before that user has joined a room the
//! bridge's ghost is in, which the ordinary `/keys/claim` and `/keys/query` endpoints (scoped to
//! "any local user, any remote user already known through shared-room federation") do not
//! support. This crate's implementation reuses exactly the same
//! [`crate::routes::keys_claim::build_keys_claim_response`] /
//! [`crate::routes::keys_query::build_keys_query_response`] logic the ordinary endpoints use, so
//! today it has the identical limitation: only local users resolve to anything (see those
//! modules' doc comments for the federation seam). The one behavioral difference these two routes
//! add over the ordinary ones is the authentication requirement below.
//!
//! Both routes require the caller to be an application service (`Requester::appservice.is_some()`
//! — set by `hs-auth`'s middleware when the request authenticated with an appservice token,
//! honoring `user_id`/`device_id` masquerade query parameters already). An ordinary user token
//! gets `403 M_FORBIDDEN`.

use axum::Json;
use axum::extract::State;
use hs_kv::KvBackend;
use serde_json::{Value, json};

use crate::error::E2eError;
use crate::routes::keys_claim::build_keys_claim_response;
use crate::routes::keys_query::build_keys_query_response;
use crate::state::{E2eRequester, E2eState};

fn require_appservice(requester: &hs_auth::requester::Requester) -> Result<(), E2eError> {
    if requester.appservice.is_none() {
        return Err(E2eError::Forbidden(
            "this endpoint is only available to application services".to_string(),
        ));
    }
    Ok(())
}

/// `POST /_matrix/client/unstable/org.matrix.msc3983/keys/claim`.
pub async fn post_msc3983_claim<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    require_appservice(&requester)?;
    // MSC3983's request body is the map of `{"<user_id>": {"<device_id>": "<algorithm>"}}`
    // directly (unlike `/keys/claim`, it is not wrapped in a `one_time_keys` field).
    let response = build_keys_claim_response(&state, &body).await?;
    Ok(Json(response))
}

/// `POST /_matrix/client/unstable/org.matrix.msc3984/keys/query`.
pub async fn post_msc3984_keys_query<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    require_appservice(&requester)?;
    // MSC3984's request body is `{"<user_id>": ["<device_id>", ...]}` directly (unlike
    // `/keys/query`, it is not wrapped in a `device_keys` field).
    let response = build_keys_query_response(&state, &requester.user_id, &body).await?;
    Ok(Json(json!({
        "device_keys": response["device_keys"],
        "master_keys": response["master_keys"],
        "self_signing_keys": response["self_signing_keys"],
    })))
}
