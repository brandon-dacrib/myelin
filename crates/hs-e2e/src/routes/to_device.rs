//! `PUT /sendToDevice/{eventType}/{txnId}`.
//!
//! Delivery to remote users is a documented seam: track 06's federation client exists, but
//! inbound transaction processing (and, symmetrically, this crate ever calling out over
//! federation) does not, so a message addressed to a user on another server is silently dropped
//! after being logged — the sender still gets a `200 OK` per the spec's fire-and-forget contract
//! for this endpoint (a to-device message has no delivery receipt), matching what
//! `PLAN.md`/`docs/workstreams/08-e2ee.md` calls the federation delivery seam. See
//! `docs/status/08-e2ee.md`.

use axum::Json;
use axum::extract::{Path, State};
use hs_kv::KvBackend;
use serde_json::{Value, json};

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};

/// `PUT /sendToDevice/{eventType}/{txnId}`.
pub async fn put_send_to_device<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Path((event_type, txn_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let Some(sender_device) = requester.device_id.clone() else {
        return Err(E2eError::BadRequest(
            "this endpoint requires a device-bound access token".to_string(),
        ));
    };

    // Idempotency: a client retrying a request it never got a response for must not have its
    // messages delivered twice.
    if state
        .store
        .check_and_mark_txn(&requester.user_id, &sender_device, &txn_id)
        .await?
    {
        return Ok(Json(json!({})));
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_object)
        .ok_or_else(|| E2eError::BadRequest("missing messages".to_string()))?;

    let server_name = state.auth.server_name();

    for (user_id_str, per_device) in messages {
        let Ok(recipient) = ruma::UserId::parse(user_id_str.as_str()) else {
            continue;
        };
        if recipient.server_name() != server_name {
            tracing::debug!(
                user = %recipient,
                "to-device message addressed to a remote user; federation delivery is not implemented yet"
            );
            continue;
        }
        let Some(per_device_obj) = per_device.as_object() else {
            continue;
        };
        for (device_id_str, content) in per_device_obj {
            if device_id_str == "*" {
                // Broadcast to every device hs-auth knows about for this user, not just devices
                // that have uploaded e2e keys -- to-device messages are not exclusively an
                // encryption feature.
                let devices = state.auth.store.list_devices(&recipient).await.map_err(|e| {
                    E2eError::Store(crate::store::StoreError::Backend(e.to_string()))
                })?;
                for device in devices {
                    state
                        .store
                        .send_to_device(
                            &requester.user_id,
                            &recipient,
                            &device.device_id,
                            &event_type,
                            content.clone(),
                        )
                        .await?;
                }
            } else {
                let device_id: ruma::OwnedDeviceId = device_id_str.as_str().into();
                state
                    .store
                    .send_to_device(
                        &requester.user_id,
                        &recipient,
                        &device_id,
                        &event_type,
                        content.clone(),
                    )
                    .await?;
            }
        }
    }

    Ok(Json(json!({})))
}
