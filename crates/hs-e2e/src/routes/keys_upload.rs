//! `POST /keys/upload`.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use hs_kv::KvBackend;
use serde_json::{Value, json};

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};

fn as_object_map(value: Option<&Value>) -> Result<BTreeMap<String, Value>, E2eError> {
    match value {
        None => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        Some(_) => Err(E2eError::BadRequest(
            "expected a JSON object of keys".to_string(),
        )),
    }
}

/// `POST /keys/upload`: uploads this device's identity keys, and/or tops up its one-time and
/// fallback keys. Returns `one_time_key_counts` for the caller's device either way (the spec
/// requires it even on a call that uploads nothing but wants a fresh count).
pub async fn post_keys_upload<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let Some(device_id) = requester.device_id.clone() else {
        return Err(E2eError::BadRequest(
            "this endpoint requires a device-bound access token".to_string(),
        ));
    };

    if let Some(device_keys) = body.get("device_keys") {
        if let Some(claimed) = device_keys.get("device_id").and_then(Value::as_str)
            && claimed != device_id.as_str()
        {
            return Err(E2eError::BadRequest(format!(
                "device_keys.device_id {claimed:?} does not match this session's device {device_id}"
            )));
        }
        state
            .store
            .upload_device_keys(&requester.user_id, &device_id, device_keys.clone())
            .await?;
    }

    let one_time_keys = as_object_map(body.get("one_time_keys"))?;
    if !one_time_keys.is_empty() {
        state
            .store
            .upload_one_time_keys(&requester.user_id, &device_id, one_time_keys)
            .await?;
    }

    // Both the stable and the MSC2732 unstable-prefixed spellings are accepted, matching the
    // dual spelling `hs-appservice`'s `Transaction` emits on the way back out.
    let fallback_source = body
        .get("fallback_keys")
        .or_else(|| body.get("org.matrix.msc2732.fallback_keys"));
    let fallback_keys = as_object_map(fallback_source)?;
    if !fallback_keys.is_empty() {
        state
            .store
            .upload_fallback_keys(&requester.user_id, &device_id, fallback_keys)
            .await?;
    }

    let counts = state
        .store
        .count_one_time_keys(&requester.user_id, &device_id)
        .await?;
    Ok(Json(json!({ "one_time_key_counts": counts })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::E2eState;
    use crate::store::tables::TablesE2eStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn router() -> axum::Router<()> {
        let backend = MemoryBackend::new();
        let store = TablesE2eStore::open(backend).unwrap();
        let state: E2eState<MemoryBackend> = E2eState::new(AuthState::in_memory(), Arc::new(store));
        axum::Router::new()
            .route(
                "/keys/upload",
                axum::routing::post(post_keys_upload::<MemoryBackend>),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn upload_without_a_device_bound_token_is_rejected() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/keys/upload")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // No Authorization header at all -> the Requester extractor itself rejects with 401
        // before the handler's own device-id check ever runs.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
