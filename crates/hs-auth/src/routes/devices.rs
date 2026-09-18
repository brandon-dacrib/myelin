//! `GET/PUT/DELETE /devices` (`{deviceId}` and the collection) and `POST /delete_devices`.
//!
//! Reading and renaming a device (`GET`, `PUT`) need only an ordinary [`Requester`]. Deleting one
//! or more devices (`DELETE /devices/{deviceId}`, `POST /delete_devices`) re-authenticates
//! through [`crate::reauth`] first, matching the spec: removing a device revokes every credential
//! bound to it, so the spec requires proof the caller still is who they say they are.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::error::MatrixError;
use crate::reauth;
use crate::requester::Requester;
use crate::state::AuthState;
use crate::store::DeviceRecord;

fn device_json(d: &DeviceRecord) -> Value {
    json!({
        "device_id": d.device_id,
        "display_name": d.display_name,
        "last_seen_ip": d.last_seen_ip,
        "last_seen_ts": d.last_seen_ms,
    })
}

/// `GET /devices`.
pub async fn get_devices(
    State(state): State<AuthState>,
    requester: Requester,
) -> Result<Json<Value>, MatrixError> {
    let devices = state.store.list_devices(&requester.user_id).await?;
    Ok(Json(
        json!({"devices": devices.iter().map(device_json).collect::<Vec<_>>()}),
    ))
}

/// `GET /devices/{deviceId}`.
pub async fn get_device(
    State(state): State<AuthState>,
    requester: Requester,
    Path(device_id): Path<String>,
) -> Result<Json<Value>, MatrixError> {
    let device_id: ruma::OwnedDeviceId = device_id.into();
    let device = state
        .store
        .get_device(&requester.user_id, &device_id)
        .await?
        .ok_or_else(|| MatrixError::not_found("Unknown device"))?;
    Ok(Json(device_json(&device)))
}

/// `PUT /devices/{deviceId}`: currently only `display_name` is settable, matching the spec.
pub async fn put_device(
    State(state): State<AuthState>,
    requester: Requester,
    Path(device_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, MatrixError> {
    let device_id: ruma::OwnedDeviceId = device_id.into();
    if state
        .store
        .get_device(&requester.user_id, &device_id)
        .await?
        .is_none()
    {
        return Err(MatrixError::not_found("Unknown device"));
    }
    let display_name = body
        .get("display_name")
        .and_then(Value::as_str)
        .map(String::from);
    state
        .store
        .set_display_name(&requester.user_id, &device_id, display_name)
        .await?;
    Ok(Json(json!({})))
}

/// `DELETE /devices/{deviceId}`: requires UIA re-authentication (the body carries `auth`).
pub async fn delete_device(
    State(state): State<AuthState>,
    requester: Requester,
    Path(device_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, MatrixError> {
    if let Some(response) = reauth::run(&state, &requester, &body).await? {
        return Ok(response);
    }
    let device_id: ruma::OwnedDeviceId = device_id.into();
    if state
        .store
        .get_device(&requester.user_id, &device_id)
        .await?
        .is_none()
    {
        return Err(MatrixError::not_found("Unknown device"));
    }
    state
        .store
        .delete_access_tokens_for_device(&requester.user_id, &device_id)
        .await?;
    state
        .store
        .delete_device(&requester.user_id, &device_id)
        .await?;
    Ok(Json(json!({})).into_response())
}

/// `POST /delete_devices`: bulk delete, also requires UIA.
pub async fn post_delete_devices(
    State(state): State<AuthState>,
    requester: Requester,
    Json(body): Json<Value>,
) -> Result<Response, MatrixError> {
    if let Some(response) = reauth::run(&state, &requester, &body).await? {
        return Ok(response);
    }
    let device_ids: Vec<String> = body
        .get("devices")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    for raw in device_ids {
        let device_id: ruma::OwnedDeviceId = raw.into();
        state
            .store
            .delete_access_tokens_for_device(&requester.user_id, &device_id)
            .await?;
        state
            .store
            .delete_device(&requester.user_id, &device_id)
            .await?;
    }
    Ok(Json(json!({})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::UserRecord;
    use axum::http::StatusCode;
    use ruma::{device_id, user_id};

    async fn state_with_device() -> (AuthState, Requester, ruma::OwnedDeviceId) {
        let state = AuthState::in_memory();
        let uid = user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let did = device_id!("DEV1").to_owned();
        state
            .store
            .upsert_device(DeviceRecord {
                user_id: uid.clone(),
                device_id: did.clone(),
                display_name: Some("phone".to_string()),
                last_seen_ms: None,
                last_seen_ip: None,
            })
            .await
            .unwrap();
        (state, Requester::for_user(uid), did)
    }

    #[tokio::test]
    async fn list_and_get_device() {
        let (state, requester, did) = state_with_device().await;
        let Json(list) = get_devices(State(state.clone()), requester.clone())
            .await
            .unwrap();
        assert_eq!(list["devices"].as_array().unwrap().len(), 1);

        let Json(one) = get_device(State(state), requester, Path(did.to_string()))
            .await
            .unwrap();
        assert_eq!(one["device_id"], did.as_str());
    }

    #[tokio::test]
    async fn get_unknown_device_is_404() {
        let (state, requester, _) = state_with_device().await;
        let err = get_device(State(state), requester, Path("NOPE".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_device_renames_without_uia() {
        let (state, requester, did) = state_with_device().await;
        let body = json!({"display_name": "renamed"});
        let _ = put_device(
            State(state.clone()),
            requester.clone(),
            Path(did.to_string()),
            Json(body),
        )
        .await
        .unwrap();
        let device = state
            .store
            .get_device(&requester.user_id, &did)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(device.display_name.as_deref(), Some("renamed"));
    }

    #[tokio::test]
    async fn delete_device_requires_uia() {
        let (state, requester, did) = state_with_device().await;
        let response = delete_device(
            State(state),
            requester,
            Path(did.to_string()),
            Json(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn delete_device_succeeds_with_dummy_stage_for_passwordless_account() {
        let (state, requester, did) = state_with_device().await;
        let body = json!({"auth": {"type": "m.login.dummy"}});
        let response = delete_device(
            State(state.clone()),
            requester.clone(),
            Path(did.to_string()),
            Json(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            state
                .store
                .get_device(&requester.user_id, &did)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn bulk_delete_devices() {
        let (state, requester, did) = state_with_device().await;
        let body = json!({"auth": {"type": "m.login.dummy"}, "devices": [did.to_string()]});
        let response = post_delete_devices(State(state.clone()), requester.clone(), Json(body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            state
                .store
                .list_devices(&requester.user_id)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
