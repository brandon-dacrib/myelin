//! `POST /keys/query`.
//!
//! Local users only: a remote `user_id` (`user_id.server_name() != this server`) is silently
//! skipped rather than reported in `failures`, since this server never attempts to reach it —
//! federation key query is a documented seam (track 06's federation client exists; inbound
//! transaction processing and an outbound `/user/keys/query` call do not). See
//! `docs/status/08-e2ee.md`.

use axum::Json;
use axum::extract::State;
use hs_kv::KvBackend;
use ruma::UserId;
use serde_json::{Map, Value, json};

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};
use crate::store::CrossSigningKeyType;

/// Builds a `/keys/query`-shaped response for `device_keys_req` (the request body's
/// `device_keys` field: `{"<user_id>": ["<device_id>", ...]}`, an empty array meaning "every
/// device"), as `requesting_user`. Shared with [`crate::routes::appservice_proxy`]'s MSC3984
/// proxy, which serves the identical shape to an appservice.
///
/// The user-signing key is only ever included for `requesting_user` themselves, matching the
/// spec's privacy rule (who you have verified is not visible to anyone else, unlike the master
/// and self-signing keys, which are always shared).
///
/// # Errors
/// Returns [`E2eError::BadRequest`] if `device_keys_req` is not a JSON object, or a storage
/// error.
pub(crate) async fn build_keys_query_response<B: KvBackend + 'static>(
    state: &E2eState<B>,
    requesting_user: &UserId,
    device_keys_req: &Value,
) -> Result<Value, E2eError> {
    let server_name = state.auth.server_name();
    let mut device_keys_out = Map::new();
    let mut master_keys = Map::new();
    let mut self_signing_keys = Map::new();
    let mut user_signing_keys = Map::new();
    let failures = Map::new();

    let Some(requested) = device_keys_req.as_object() else {
        return Err(E2eError::BadRequest(
            "device_keys must be an object".to_string(),
        ));
    };

    for (user_id_str, device_filter) in requested {
        // `UserId::parse` returns an owned id; every store call below wants `&UserId`, which
        // `&user_id` gets via `OwnedUserId`'s `Deref<Target = UserId>`.
        let Ok(user_id) = UserId::parse(user_id_str.as_str()) else {
            continue;
        };
        let user_id = &user_id;
        if user_id.server_name() != server_name {
            continue;
        }
        let wanted: Option<Vec<String>> = device_filter.as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });

        let devices = state.store.list_device_keys(user_id).await?;
        let mut per_user = Map::new();
        for (device_id, row) in devices {
            if let Some(list) = &wanted
                && !list.is_empty()
                && !list.contains(&device_id.to_string())
            {
                continue;
            }
            per_user.insert(device_id.to_string(), row.keys);
        }
        if !per_user.is_empty() {
            device_keys_out.insert(user_id.to_string(), Value::Object(per_user));
        }

        if let Some(key) = state
            .store
            .get_cross_signing_key(user_id, CrossSigningKeyType::Master)
            .await?
        {
            master_keys.insert(user_id.to_string(), key);
        }
        if let Some(key) = state
            .store
            .get_cross_signing_key(user_id, CrossSigningKeyType::SelfSigning)
            .await?
        {
            self_signing_keys.insert(user_id.to_string(), key);
        }
        if user_id.as_str() == requesting_user.as_str()
            && let Some(key) = state
                .store
                .get_cross_signing_key(user_id, CrossSigningKeyType::UserSigning)
                .await?
        {
            user_signing_keys.insert(user_id.to_string(), key);
        }
    }

    Ok(json!({
        "device_keys": device_keys_out,
        "master_keys": master_keys,
        "self_signing_keys": self_signing_keys,
        "user_signing_keys": user_signing_keys,
        "failures": failures,
    }))
}

/// `POST /keys/query`.
pub async fn post_keys_query<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let device_keys_req = body.get("device_keys").cloned().unwrap_or(json!({}));
    let response = build_keys_query_response(&state, &requester.user_id, &device_keys_req).await?;
    Ok(Json(response))
}
