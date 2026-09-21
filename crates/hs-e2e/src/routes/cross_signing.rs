//! `POST /keys/device_signing/upload` and `POST /keys/signatures/upload`.
//!
//! Both endpoints now cryptographically verify the Ed25519 signatures they are asked to store,
//! per `refs/matrix-spec/content/client-server-api/modules/end_to_end_encryption.md`'s
//! "Cross-signing" section and `refs/matrix-spec/data/api/client-server/cross_signing.yaml`'s
//! documented error cases. Verification reuses `hs_model::signing` (canonical-JSON signing bytes
//! and Ed25519 verification) and `hs_model::canonical` exactly as `hs-model` already does for
//! events, rather than re-implementing canonicalisation here.
//!
//! # What is enforced (see `docs/status/08-e2ee.md` for the reasoning in full)
//!
//! - A master key is the trust root and is never required to verify against anything (the spec's
//!   `CrossSigningKey` schema marks `signatures` "optional for the master signing key").
//! - A self-signing or user-signing key uploaded via `/keys/device_signing/upload` MUST carry a
//!   valid signature by the user's master key -- the one in the same request if given, else the
//!   most recently stored one (per `cross_signing.yaml`'s description of those two fields). No
//!   master key available at all is `M_MISSING_PARAM`; a present-but-unverifiable signature is
//!   `M_INVALID_SIGNATURE`. Verification happens before anything is written, so a rejected request
//!   changes no stored state.
//! - In `/keys/signatures/upload`, every signature entry submitted under the caller's own user ID
//!   is verified before being merged:
//!   - A signature on a device key is only accepted from that device's own user, resolved against
//!     one of the signer's own known keys (their self-signing key, in the normal case; their
//!     master key or another device is also accepted, since the spec allows a user's own device to
//!     sign their own master key for verification migration and does not otherwise name a single
//!     fixed key for "the signer's own material").
//!   - A signature on another user's master key is only accepted if it resolves specifically to
//!     the signer's user-signing key -- not just any of the signer's keys -- since that is the one
//!     key in the whole model that is allowed to vouch for a different user's identity.
//!   - A signature on the caller's own master/self-signing/user-signing key (by their own device
//!     or another of their own cross-signing keys) is resolved the same general way as the device
//!     case above.
//!
//!   A per-item failure (an unverifiable signature, or one claiming a signing key this server has
//!   no record of) is reported in the response's `failures` map with `M_INVALID_SIGNATURE`,
//!   exactly like the existing "no such device or cross-signing key" `M_NOT_FOUND` failure --
//!   never as a request-level error, matching `cross_signing.yaml`'s response schema.
//!
//! # What is deliberately not enforced this session
//!
//! - Structural/schema validation of the key objects themselves (`usage` contents, `user_id`
//!   matching the path, exactly-one-entry `keys` maps) beyond what is needed to extract a key ID
//!   and public key to verify with. A malformed object simply fails to resolve a verifying key and
//!   is rejected as an invalid signature; it is not given a more specific errcode.
//! - The `403 M_FORBIDDEN` "key ID in use" case (a cross-signing public key colliding with an
//!   existing device ID) that `cross_signing.yaml` documents for `/keys/device_signing/upload`.
//!   Unrelated to signature verification; not touched this session.
//!
//! `/keys/device_signing/upload` also still skips user-interactive auth (UIA) re-authentication,
//! which the spec requires before accepting new cross-signing keys for a non-appservice caller.
//! `hs_auth::reauth::run` is public and this crate's `E2eState` already embeds an `AuthState`, so
//! the bridge needed to wire it in does exist -- but doing so risks breaking
//! `hs-loadgen`'s `real_client_encrypted` test (which bootstraps cross-signing with no `auth`
//! data at all) unless that crate is updated in the same session, and `hs-loadgen` belongs to
//! another track. Left for a coordinated session; see `docs/status/08-e2ee.md`.

use axum::Json;
use axum::extract::State;
use ed25519_dalek::VerifyingKey;
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use hs_model::signing::{to_signable_object, verify_object, verifying_key_from_base64};
use ruma::{OwnedDeviceId, UserId};
use serde_json::{Map, Value, json};

use crate::error::E2eError;
use crate::state::{E2eRequester, E2eState};
use crate::store::CrossSigningKeyType;

/// `POST /keys/device_signing/upload`.
pub async fn post_device_signing_upload<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Json<Value>, E2eError> {
    // Resolve the master key to verify `self_signing_key`/`user_signing_key` against *before*
    // writing anything: the one in this request if given, else the user's most recently stored
    // one. Per `cross_signing.yaml`, having neither is `M_MISSING_PARAM`, and a request that fails
    // verification must leave existing stored keys untouched.
    let master_key_in_request = body.get("master_key").cloned();
    let effective_master = match master_key_in_request {
        Some(mk) => Some(mk),
        None => {
            state
                .store
                .get_cross_signing_key(&requester.user_id, CrossSigningKeyType::Master)
                .await?
        }
    };

    for field in ["self_signing_key", "user_signing_key"] {
        let Some(key) = body.get(field) else {
            continue;
        };
        let master = effective_master.as_ref().ok_or_else(|| {
            E2eError::MissingParam(format!(
                "no master signing key is available to verify {field} against"
            ))
        })?;
        verify_signed_by_own_master(key, master, &requester.user_id)
            .map_err(|msg| E2eError::InvalidSignature(format!("{field}: {msg}")))?;
    }

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

/// The unprefixed public key value inside a cross-signing key object's `keys` map -- the spec's
/// convention for that key's own id in a `/keys/signatures/upload` request (the request's
/// `<key_id>` for a cross-signing key equals this, not an `"ed25519:..."`-prefixed form).
fn cross_signing_key_id(key: &Value) -> Option<&str> {
    key.get("keys")?.as_object()?.values().next()?.as_str()
}

/// The full (`"ed25519:<version>"`-prefixed) key ID and decoded [`VerifyingKey`] for the single
/// entry in a device or cross-signing key object's `keys` map matching `key_id` exactly.
fn verifying_key_for(key_obj: &Value, key_id: &str) -> Option<VerifyingKey> {
    let encoded = key_obj.get("keys")?.as_object()?.get(key_id)?.as_str()?;
    verifying_key_from_base64(encoded).ok()
}

/// The full key ID and [`VerifyingKey`] of a cross-signing key object's own (single) `keys` entry
/// -- i.e. treating `key_obj` itself as a signing key, not looking up one of its signature claims.
fn own_key_id_and_verifying_key(key_obj: &Value) -> Option<(String, VerifyingKey)> {
    let (key_id, encoded) = key_obj.get("keys")?.as_object()?.iter().next()?;
    let verifying_key = verifying_key_from_base64(encoded.as_str()?).ok()?;
    Some((key_id.clone(), verifying_key))
}

/// Verifies that `key` carries, under `signatures.<user_id>`, a valid signature by `master`'s own
/// key -- the check `/keys/device_signing/upload` applies to an uploaded self-signing or
/// user-signing key. `key` is verified as submitted (its own `signatures`/`unsigned` are stripped
/// before computing the signing bytes, per `hs_model::signing`), not merged with anything stored.
fn verify_signed_by_own_master(
    key: &Value,
    master: &Value,
    user_id: &UserId,
) -> Result<(), String> {
    let (master_key_id, master_verifying_key) =
        own_key_id_and_verifying_key(master).ok_or("the master key is malformed")?;
    let canonical =
        to_signable_object(key).map_err(|e| format!("key is not valid canonical JSON: {e}"))?;
    verify_object(
        &canonical,
        user_id.as_str(),
        &master_key_id,
        &master_verifying_key,
    )
    .map_err(|_| format!("signature by master key {master_key_id} does not verify"))
}

fn not_found_failure(message: &str) -> Value {
    json!({"errcode": "M_NOT_FOUND", "error": message})
}

fn invalid_signature_failure(message: &str) -> Value {
    json!({"errcode": "M_INVALID_SIGNATURE", "error": message})
}

/// `POST /keys/signatures/upload`: `{"<user_id>": {"<key_id>": {<object with signatures to
/// merge>}}}` -> `{"failures": {"<user_id>": {"<key_id>": {<error>}}}}`.
pub async fn post_signatures_upload<B: KvBackend + 'static>(
    State(state): State<E2eState<B>>,
    E2eRequester(requester): E2eRequester,
    PermissiveJson(body): PermissiveJson<Value>,
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
        let mut signed_something = false;
        for (key_id, submitted) in by_key_obj {
            let outcome =
                apply_one_signature(&state, &requester.user_id, &target_user, key_id, submitted)
                    .await?;
            match outcome {
                SignatureOutcome::Applied => signed_something = true,
                SignatureOutcome::NotFound => {
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
                SignatureOutcome::Invalid(msg) => {
                    failures
                        .entry(user_id_str.clone())
                        .or_insert_with(|| Value::Object(Map::new()))
                        .as_object_mut()
                        .expect("just inserted as an object")
                        .insert(key_id.clone(), invalid_signature_failure(&msg));
                }
            }
        }
        // A key that has gained a signature is a key that has changed, and a signature is the
        // whole of what verifying a device *is*: the device's keys are the same, and what is new
        // is that its owner vouches for them. Nothing here said so, so nobody re-fetched --
        // other people went on seeing a verified device as unverified, and the user's own
        // client, having just signed its own device while setting up cross-signing, never
        // learned that the server had taken the signature: Element marked every message its
        // own user sent "Encrypted by a device not verified by its owner".
        if signed_something {
            state.store.record_device_list_change(&target_user).await?;
        }
    }

    Ok(Json(json!({ "failures": failures })))
}

/// Copies `signer`'s entry out of `submitted`'s `signatures` map (ignoring anything submitted
/// under a different signer's name -- a client can only ever add signatures it claims to be its
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

/// The result of trying to apply one `(target_user, key_id, submitted)` signature merge.
enum SignatureOutcome {
    /// A matching key was found and every signature entry submitted under `signer`'s name
    /// verified, so the merge was applied.
    Applied,
    /// `key_id` matched neither a device nor a cross-signing key of `target_user`.
    NotFound,
    /// A matching key was found, but at least one submitted signature entry did not verify (or
    /// named a signing key this server has no record of, or is not the kind of key allowed to
    /// make this particular claim). Carries the reason for the `failures` entry.
    Invalid(String),
}

/// Looks up a [`VerifyingKey`] for one of `signer`'s *own* known keys (any of their three
/// cross-signing keys, or one of their devices) matching `key_id` exactly. This is "the claimed
/// signing key" for the general case: a signature naming a key ID this server cannot resolve to
/// any key of the signer's is never accepted, regardless of what it is signing.
async fn resolve_signers_own_key<B: KvBackend + 'static>(
    state: &E2eState<B>,
    signer: &UserId,
    key_id: &str,
) -> Result<Option<VerifyingKey>, E2eError> {
    for key_type in [
        CrossSigningKeyType::Master,
        CrossSigningKeyType::SelfSigning,
        CrossSigningKeyType::UserSigning,
    ] {
        if let Some(csk) = state.store.get_cross_signing_key(signer, key_type).await?
            && let Some(vk) = verifying_key_for(&csk, key_id)
        {
            return Ok(Some(vk));
        }
    }
    if let Some(device_id) = key_id.strip_prefix("ed25519:") {
        let device_id: OwnedDeviceId = device_id.into();
        if let Some(row) = state.store.get_device_keys(signer, &device_id).await?
            && let Some(vk) = verifying_key_for(&row.keys, key_id)
        {
            return Ok(Some(vk));
        }
    }
    Ok(None)
}

/// The entries under `submitted.signatures.<signer>`, paired with `submitted`'s own canonical
/// signing bytes computed once for however many entries there are.
type EntriesAndCanonical<'v> = (
    &'v serde_json::Map<String, Value>,
    hs_model::canonical::CanonicalJsonObject,
);

/// The entries under `submitted.signatures.<signer>`, and `submitted`'s own canonical signing
/// bytes computed once for however many entries there are -- shared setup for both verification
/// functions below. `Ok(None)` means there is nothing to verify (a no-op merge, same as before
/// verification existed), not a failure.
fn submitted_entries_and_canonical<'v>(
    submitted: &'v Value,
    signer: &UserId,
) -> Result<Option<EntriesAndCanonical<'v>>, String> {
    let Some(entries) = submitted
        .get("signatures")
        .and_then(|s| s.get(signer.as_str()))
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let canonical = to_signable_object(submitted)
        .map_err(|e| format!("submitted object is not valid canonical JSON: {e}"))?;
    Ok(Some((entries, canonical)))
}

/// Verifies every signature entry `submitted.signatures.<signer>.*` against one of `signer`'s own
/// known keys (any of their three cross-signing keys, or one of their devices), resolved per entry
/// by its key ID. Used for the general case: signing your own device, or signing your own
/// master/self-signing/user-signing key with another key of your own. An entry whose key ID cannot
/// be resolved to any key of the signer's is a failure, not skipped -- a signature naming an
/// unknown key can never be a legitimate claim.
async fn verify_submitted_entries_by_signer<B: KvBackend + 'static>(
    state: &E2eState<B>,
    submitted: &Value,
    signer: &UserId,
) -> Result<(), String> {
    let Some((entries, canonical)) = submitted_entries_and_canonical(submitted, signer)? else {
        return Ok(());
    };
    for key_id in entries.keys() {
        let verifying_key = resolve_signers_own_key(state, signer, key_id)
            .await
            .map_err(|e| format!("looking up signing key {key_id}: {e}"))?
            .ok_or_else(|| format!("no known key {key_id} for signer {signer}"))?;
        verify_object(&canonical, signer.as_str(), key_id, &verifying_key)
            .map_err(|_| format!("signature under key {key_id} does not verify"))?;
    }
    Ok(())
}

/// Verifies every signature entry `submitted.signatures.<signer>.*` against exactly one expected
/// key (key ID and verifying key) -- used for the one case where a specific key, and no other, is
/// allowed to make the claim: another user's user-signing key vouching for this user's master key.
fn verify_submitted_entries_as(
    submitted: &Value,
    signer: &UserId,
    expected_key_id: &str,
    verifying_key: &VerifyingKey,
) -> Result<(), String> {
    let Some((entries, canonical)) = submitted_entries_and_canonical(submitted, signer)? else {
        return Ok(());
    };
    for key_id in entries.keys() {
        if key_id != expected_key_id {
            return Err(format!(
                "signature under key {key_id} does not match {signer}'s user-signing key \
                 {expected_key_id}"
            ));
        }
        verify_object(&canonical, signer.as_str(), key_id, verifying_key)
            .map_err(|_| format!("signature under key {key_id} does not verify"))?;
    }
    Ok(())
}

/// Applies one `(target_user, key_id, submitted)` signature merge: verifies every signature entry
/// submitted under `signer`'s name against the appropriate key material, then merges and stores
/// only if every entry verified.
async fn apply_one_signature<B: KvBackend + 'static>(
    state: &E2eState<B>,
    signer: &UserId,
    target_user: &UserId,
    key_id: &str,
    submitted: &Value,
) -> Result<SignatureOutcome, E2eError> {
    let device_id: OwnedDeviceId = key_id.into();
    if let Some(row) = state.store.get_device_keys(target_user, &device_id).await? {
        // Only a user's own key material can sign their own device -- there is no key in the
        // cross-signing model that lets one user vouch for another user's device directly.
        if signer != target_user {
            return Ok(SignatureOutcome::Invalid(
                "a device key can only be signed by its own user".to_string(),
            ));
        }
        let verify = verify_submitted_entries_by_signer(state, submitted, signer).await;
        return Ok(match verify {
            Ok(()) => {
                let merged = merge_signatures(row.keys, submitted, signer);
                state
                    .store
                    .replace_device_keys(target_user, &device_id, merged)
                    .await?;
                SignatureOutcome::Applied
            }
            Err(msg) => SignatureOutcome::Invalid(msg),
        });
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
        if cross_signing_key_id(&existing) != Some(key_id) {
            continue;
        }

        let verify = if key_type == CrossSigningKeyType::Master && signer != target_user {
            // The one case where a signature vouches for a *different* user's identity: it must
            // resolve specifically to the signer's user-signing key, not just any of their keys.
            let Some(usk) = state
                .store
                .get_cross_signing_key(signer, CrossSigningKeyType::UserSigning)
                .await?
            else {
                return Ok(SignatureOutcome::Invalid(format!(
                    "{signer} has no user-signing key to sign another user's master key with"
                )));
            };
            let Some((usk_key_id, usk_verifying_key)) = own_key_id_and_verifying_key(&usk) else {
                return Ok(SignatureOutcome::Invalid(
                    "signer's user-signing key is malformed".to_string(),
                ));
            };
            verify_submitted_entries_as(submitted, signer, &usk_key_id, &usk_verifying_key)
        } else {
            if signer != target_user {
                return Ok(SignatureOutcome::Invalid(
                    "a self-signing or user-signing key can only be signed by its own user"
                        .to_string(),
                ));
            }
            verify_submitted_entries_by_signer(state, submitted, signer).await
        };

        return Ok(match verify {
            Ok(()) => {
                let merged = merge_signatures(existing, submitted, signer);
                state
                    .store
                    .put_cross_signing_key(target_user, key_type, merged)
                    .await?;
                SignatureOutcome::Applied
            }
            Err(msg) => SignatureOutcome::Invalid(msg),
        });
    }

    Ok(SignatureOutcome::NotFound)
}
