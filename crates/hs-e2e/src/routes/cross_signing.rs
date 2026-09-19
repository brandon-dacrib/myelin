//! `POST /keys/device_signing/upload` and `POST /keys/signatures/upload`.
//!
//! Neither endpoint here cryptographically verifies the Ed25519 signatures it stores — the spec
//! says a server "MUST" reject an unverifiable signature, but wiring canonical-JSON signing
//! verification (`ed25519-dalek`, already a workspace dependency, plus the canonical-JSON and
//! signing-bytes conventions `hs-model` owns for events) was cut from this pass to land the
//! rest of the crate; see `docs/status/08-e2ee.md`'s "Decisions made" for the tracked follow-up.
//! This crate stores and distributes whatever it is given, which is safe for its stated job (key
//! distribution, never message content) but means a malicious or buggy client could currently
//! have an unverifiable signature accepted; every *client's* own verification of what it
//! downloads is unaffected either way, since that verification always happens locally regardless
//! of what the server enforces.
//!
//! `/keys/device_signing/upload` also skips user-interactive auth (UIA) re-authentication, which
//! the spec requires before accepting new cross-signing keys — `hs-auth::uia` exists and could be
//! wired in, but doing so needs a `Requester`-to-UIA-session bridge this crate does not have time
//! to build in this pass. Recorded as the same kind of documented gap.

use axum::Json;
use axum::extract::State;
use hs_kv::KvBackend;
use ruma::{OwnedDeviceId, UserId};
use serde_json::{Map, Value, json};

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};
use crate::store::CrossSigningKeyType;

/// `POST /keys/device_signing/upload`.
pub async fn post_device_signing_upload<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let mut changed = false;
    for (field, key_type) in [
        ("master_key", CrossSigningKeyType::Master),
        ("self_signing_key", CrossSigningKeyType::SelfSigning),
        ("user_signing_key", CrossSigningKeyType::UserSigning),
    ] {
        if let Some(key) = body.get(field) {
            state
                .store
                .put_cross_signing_key(&requester.user_id, key_type, key.clone())
                .await?;
            changed = true;
        }
    }
    if changed {
        state
            .store
            .record_device_list_change(&requester.user_id)
            .await?;
    }
    Ok(Json(json!({})))
}

/// The unprefixed public key value inside a cross-signing key object's `keys` map — the spec's
/// convention for that key's own id in a `/keys/signatures/upload` request (the request's
/// `<key_id>` for a cross-signing key equals this, not an `"ed25519:..."`-prefixed form).
fn cross_signing_key_id(key: &Value) -> Option<&str> {
    key.get("keys")?.as_object()?.values().next()?.as_str()
}

/// Copies `signer`'s entry out of `submitted`'s `signatures` map (ignoring anything submitted
/// under a different signer's name — a client can only ever add signatures it claims to be its
/// own) into `target`'s `signatures` map, returning the merged object.
fn merge_signatures(mut target: Value, submitted: &Value, signer: &UserId) -> Value {
    let Some(new_sigs) = submitted
        .get("signatures")
        .and_then(|s| s.get(signer.as_str()))
        .and_then(Value::as_object)
        .cloned()
    else {
        return target;
    };
    let signatures = target
        .as_object_mut()
        .map(|obj| {
            obj.entry("signatures")
                .or_insert_with(|| Value::Object(Map::new()))
        })
        .and_then(Value::as_object_mut);
    if let Some(signatures) = signatures {
        let entry = signatures
            .entry(signer.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(entry_obj) = entry.as_object_mut() {
            for (k, v) in new_sigs {
                entry_obj.insert(k, v);
            }
        }
    }
    target
}

fn not_found_failure(message: &str) -> Value {
    json!({"errcode": "M_NOT_FOUND", "error": message})
}

/// `POST /keys/signatures/upload`: `{"<user_id>": {"<key_id>": {<object with signatures to
/// merge>}}}` -> `{"failures": {"<user_id>": {"<key_id>": {<error>}}}}`.
pub async fn post_signatures_upload<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E2eError> {
    let Some(by_user) = body.as_object() else {
        return Err(E2eError::BadRequest("body must be an object".to_string()));
    };
    let mut failures = Map::new();

    for (user_id_str, by_key) in by_user {
        let Ok(target_user) = UserId::parse(user_id_str.as_str()) else {
            continue;
        };
        let Some(by_key_obj) = by_key.as_object() else {
            continue;
        };
        for (key_id, submitted) in by_key_obj {
            match apply_one_signature(&state, &requester.user_id, &target_user, key_id, submitted)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    failures
                        .entry(user_id_str.clone())
                        .or_insert_with(|| Value::Object(Map::new()))
                        .as_object_mut()
                        .expect("just inserted as an object")
                        .insert(
                            key_id.clone(),
                            not_found_failure("no device or cross-signing key with this id"),
                        );
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(Json(json!({ "failures": failures })))
}

/// Applies one `(target_user, key_id, submitted)` signature merge, returning `Ok(true)` if a
/// matching device or cross-signing key was found and updated, `Ok(false)` if `key_id` matched
/// neither (a `/keys/signatures/upload` per-item failure, not a request-level error).
async fn apply_one_signature<B: KvBackend + 'static>(
    state: &E2eState<B>,
    signer: &UserId,
    target_user: &UserId,
    key_id: &str,
    submitted: &Value,
) -> Result<bool, E2eError> {
    let device_id: OwnedDeviceId = key_id.into();
    if let Some(row) = state.store.get_device_keys(target_user, &device_id).await? {
        let merged = merge_signatures(row.keys, submitted, signer);
        state
            .store
            .replace_device_keys(target_user, &device_id, merged)
            .await?;
        return Ok(true);
    }

    for key_type in [
        CrossSigningKeyType::Master,
        CrossSigningKeyType::SelfSigning,
        CrossSigningKeyType::UserSigning,
    ] {
        let Some(existing) = state
            .store
            .get_cross_signing_key(target_user, key_type)
            .await?
        else {
            continue;
        };
        if cross_signing_key_id(&existing) == Some(key_id) {
            let merged = merge_signatures(existing, submitted, signer);
            state
                .store
                .put_cross_signing_key(target_user, key_type, merged)
                .await?;
            return Ok(true);
        }
    }

    Ok(false)
}
