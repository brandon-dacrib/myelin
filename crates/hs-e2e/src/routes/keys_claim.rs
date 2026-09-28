//! `POST /keys/claim`.

use axum::Json;
use axum::extract::State;
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{OwnedDeviceId, UserId};
use serde_json::{Map, Value, json};

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};

/// Builds a `/keys/claim`-shaped response for `one_time_keys_req` (the request body's
/// `one_time_keys` field: `{"<user_id>": {"<device_id>": "<algorithm>"}}`). Shared with
/// [`crate::routes::appservice_proxy`]'s MSC3983 proxy.
///
/// For each requested `(user, device, algorithm)`: first attempts
/// [`OneTimeKeyStore::claim_one_time_key`] (the atomic, single-use claim); if none remain, falls
/// back to [`FallbackKeyStore::claim_fallback_key`] (reusable), matching the spec's fallback
/// order. A remote user's keys are claimed from their own server through the installed
/// [`crate::federation::RemoteKeys`], one request per server; a server that cannot be reached is
/// listed in `failures`. Without one installed, remote users are absent from the response.
///
/// # Errors
/// Returns [`E2eError::BadRequest`] if `one_time_keys_req` is not a JSON object, or a storage
/// error.
pub(crate) async fn build_keys_claim_response<B: KvBackend + 'static>(
    state: &E2eState<B>,
    one_time_keys_req: &Value,
) -> Result<Value, E2eError> {
    let mut out = local_keys_claim(state, one_time_keys_req).await?;
    let mut failures = Map::new();
    if let (Some(remote), Some(requested)) = (state.remote_keys(), one_time_keys_req.as_object()) {
        let by_server =
            crate::federation::remote_part(requested, state.auth.server_name().as_str());
        let asked = by_server.clone();
        for (server, answer) in
            crate::federation::ask_servers(remote, crate::federation::Ask::Claim, by_server).await
        {
            let Some(asked) = asked.get(&server) else {
                continue;
            };
            match answer {
                Ok(answer) => crate::federation::merge_for_server(
                    &mut out,
                    &answer,
                    "one_time_keys",
                    &server,
                    asked,
                ),
                Err(reason) => {
                    tracing::info!(server, reason, "could not claim keys from a server");
                    failures.insert(server, crate::federation::failure(&reason));
                }
            }
        }
    }
    Ok(json!({ "one_time_keys": out, "failures": failures }))
}

/// Claims one key per requested device of this server's own users named in
/// `one_time_keys_req`; users of other servers are skipped. Returns the `one_time_keys` map.
///
/// # Errors
/// Returns [`E2eError::BadRequest`] if `one_time_keys_req` is not a JSON object, or a storage
/// error.
pub(crate) async fn local_keys_claim<B: KvBackend + 'static>(
    state: &E2eState<B>,
    one_time_keys_req: &Value,
) -> Result<Map<String, Value>, E2eError> {
    let Some(requested) = one_time_keys_req.as_object() else {
        return Err(E2eError::BadRequest(
            "one_time_keys must be an object".to_string(),
        ));
    };
    let mut out = Map::new();
    let server_name = state.auth.server_name();

    for (user_id_str, per_device) in requested {
        let Ok(user_id) = UserId::parse(user_id_str.as_str()) else {
            continue;
        };
        if user_id.server_name() != server_name {
            continue;
        }
        let Some(per_device_obj) = per_device.as_object() else {
            continue;
        };
        let mut per_user = Map::new();
        for (device_id_str, algorithm_val) in per_device_obj {
            let Some(algorithm) = algorithm_val.as_str() else {
                continue;
            };
            let device_id: OwnedDeviceId = device_id_str.as_str().into();
            let claimed = claim_one(state, &user_id, &device_id, algorithm).await?;
            if let Some((key_id, value)) = claimed {
                let mut device_map = Map::new();
                device_map.insert(format!("{algorithm}:{key_id}"), value);
                per_user.insert(device_id_str.clone(), Value::Object(device_map));
            }
        }
        if !per_user.is_empty() {
            out.insert(user_id.to_string(), Value::Object(per_user));
        }
    }

    Ok(out)
}

async fn claim_one<B: KvBackend + 'static>(
    state: &E2eState<B>,
    user_id: &UserId,
    device_id: &ruma::DeviceId,
    algorithm: &str,
) -> Result<Option<(String, Value)>, E2eError> {
    if let Some(claim) = state
        .store
        .claim_one_time_key(user_id, device_id, algorithm)
        .await?
    {
        return Ok(Some(claim));
    }
    Ok(state
        .store
        .claim_fallback_key(user_id, device_id, algorithm)
        .await?)
}

/// `POST /keys/claim`.
pub async fn post_keys_claim<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(_requester): E2eRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, E2eError> {
    let one_time_keys_req = body.get("one_time_keys").cloned().unwrap_or(json!({}));
    let response = build_keys_claim_response(&state, &one_time_keys_req).await?;
    Ok(Json(response))
}
