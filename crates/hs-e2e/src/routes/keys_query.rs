//! `POST /keys/query`.
//!
//! Local users only: a remote `user_id` (`user_id.server_name() != this server`) is silently
//! skipped rather than reported in `failures`, since this server never attempts to reach it —
//! federation key query is a documented seam (track 06's federation client exists; inbound
//! transaction processing and an outbound `/user/keys/query` call do not). See
//! `docs/status/08-e2ee.md`.

use axum::Json;
use axum::extract::State;
use hs_http::body::PermissiveJson;
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
        // The per-user device filter must be an array of device id strings (or absent, meaning
        // "no filter" -- note this is *not* the same as an explicit empty array, which the spec
        // also treats as "every device", so both are handled identically below). Anything else
        // (an object, a number, ...) is a malformed request body: Element iOS has been known to
        // send `{"device_id1": true}` in place of `["device_id1"]`, which a loosely-typed server
        // would silently treat as an empty array (Python iterates a dict's keys); this server
        // rejects it outright, matching the spec and Complement's
        // `TestKeysQueryWithDeviceIDAsObjectFails`.
        let wanted: Option<Vec<String>> = match device_filter {
            Value::Null => None,
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            ),
            _ => {
                return Err(E2eError::BadRequest(format!(
                    "device_keys.{user_id_str} must be an array of device ids"
                )));
            }
        };

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
        // Always report an entry for a requested (valid, local) user, even an empty one -- the
        // spec's response shape is "for each user requested", not "for each user found with
        // devices". Omitting the key entirely for a user with no matching devices previously made
        // `device_keys.<user>` come back missing rather than `{}`, which is exactly what
        // Complement's key-management tests (e.g. "query for user with no keys returns empty key
        // dict") catch.
        device_keys_out.insert(user_id.to_string(), Value::Object(per_user));

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
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, E2eError> {
    let device_keys_req = body.get("device_keys").cloned().unwrap_or(json!({}));
    let response = build_keys_query_response(&state, &requester.user_id, &device_keys_req).await?;
    Ok(Json(response))
}
