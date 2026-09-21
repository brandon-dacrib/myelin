//! `POST /keys/upload`.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use hs_http::body::PermissiveJson;
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
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, E2eError> {
    let Some(device_id) = requester.device_id.clone() else {
        return Err(E2eError::BadRequest(
            "this endpoint requires a device-bound access token".to_string(),
        ));
    };

    if let Some(device_keys) = body.get("device_keys") {
        let Some(device_keys_obj) = device_keys.as_object() else {
            return Err(E2eError::BadRequest(
                "device_keys must be an object".to_string(),
            ));
        };
        // `algorithms`, `keys` and `signatures` are all `Required` by the spec's `DeviceKeys`
        // schema; a body missing one is malformed, not merely "unusual" -- accepting it would
        // store (and later hand other users via `/keys/query`) a device-keys object no client can
        // actually parse. Matches Complement's `upload_keys_test.go` "Rejects invalid device
        // keys".
        for field in ["algorithms", "keys", "signatures"] {
            if !device_keys_obj.contains_key(field) {
                return Err(E2eError::BadRequest(format!(
                    "device_keys is missing required field {field:?}"
                )));
            }
        }
        if let Some(claimed) = device_keys.get("device_id").and_then(Value::as_str)
            && claimed != device_id.as_str()
        {
            return Err(E2eError::BadRequest(format!(
                "device_keys.device_id {claimed:?} does not match this session's device {device_id}"
            )));
        }
        if let Some(claimed) = device_keys.get("user_id").and_then(Value::as_str)
            && claimed != requester.user_id.as_str()
        {
            return Err(E2eError::BadRequest(format!(
                "device_keys.user_id {claimed:?} does not match this session's user {}",
                requester.user_id
            )));
        }
        state
            .store
            .upload_device_keys(&requester.user_id, &device_id, device_keys.clone())
            .await?;
        // Identity-key upload is a device-list change (client-server API "Device list tracking"):
        // other users sharing a room with this one need to learn about it on their next
        // `/keys/changes`/`/sync`. Before this, the only call site that ever bumped this stream
        // was cross-signing bootstrap (`crate::routes::cross_signing`) -- an ordinary key upload,
        // the far more common case, never did. Deliberately scoped to `device_keys` only, not a
        // one-time/fallback-key-only upload: those never change what a device *is*, only its
        // available key material, and Synapse's own `notify_device_update` call site is the same
        // (`_upload_keys` -> device-keys branch only).
        state
            .store
            .record_device_list_change(&requester.user_id)
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
    use crate::store::DeviceKeyStore;
    use crate::store::tables::TablesE2eStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::{device_id, user_id};
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

    fn device_bound_requester() -> Requester {
        Requester {
            device_id: Some(device_id!("DEV1").to_owned()),
            ..Requester::for_user(user_id!("@alice:example.org").to_owned())
        }
    }

    /// The gap this session closes: uploading identity (`device_keys`) keys is the ordinary case
    /// (not cross-signing bootstrap, `crate::routes::cross_signing`, the only call site that
    /// bumped this stream before) and must still be visible to other users' `/keys/changes` and
    /// `/sync` `device_lists`.
    #[tokio::test]
    async fn uploading_device_keys_bumps_the_device_list_stream() {
        let backend = MemoryBackend::new();
        let store = Arc::new(TablesE2eStore::open(backend).unwrap());
        let state: E2eState<MemoryBackend> = E2eState::new(AuthState::in_memory(), store.clone());
        assert_eq!(store.current_stream_pos().await.unwrap(), 0);

        let Json(_) = post_keys_upload::<MemoryBackend>(
            State(state),
            E2eRequester(device_bound_requester()),
            PermissiveJson(json!({
                "device_keys": {
                    "algorithms": ["m.olm.v1.curve25519-aes-sha2"],
                    "device_id": "DEV1",
                    "keys": {"curve25519:DEV1": "abc", "ed25519:DEV1": "def"},
                    "signatures": {},
                }
            })),
        )
        .await
        .unwrap();

        assert!(store.current_stream_pos().await.unwrap() > 0);
        let changed = store.changed_users_since(0, None).await.unwrap();
        assert!(changed.contains(user_id!("@alice:example.org")));
    }

    /// One-time/fallback-key-only uploads (no `device_keys`) do not change what the device *is*,
    /// so they must not bump the device-list stream -- matching Synapse's own `_upload_keys` (the
    /// device-keys branch only calls `notify_device_update`).
    #[tokio::test]
    async fn uploading_only_one_time_keys_does_not_bump_the_device_list_stream() {
        let backend = MemoryBackend::new();
        let store = Arc::new(TablesE2eStore::open(backend).unwrap());
        let state: E2eState<MemoryBackend> = E2eState::new(AuthState::in_memory(), store.clone());

        let Json(_) = post_keys_upload::<MemoryBackend>(
            State(state),
            E2eRequester(device_bound_requester()),
            PermissiveJson(json!({
                "one_time_keys": {"curve25519:AAAA": "base64key"},
            })),
        )
        .await
        .unwrap();

        assert_eq!(store.current_stream_pos().await.unwrap(), 0);
    }
}
