//! The devices-and-identity half of a user's long tail in the admin API: one device read,
//! renamed or several signed out at once (`users.devices.get/update/bulk_delete`), the 3PIDs
//! bound to the account (`users.threepids.*`), the upstream identity-provider subjects linked to
//! it (`users.external_ids.*`), its per-user experimental-feature flags
//! (`users.experimental_features.*`), and two read-only views an administrator needs when
//! helping someone: their global account data (`users.account_data.list`) and their pushers
//! (`users.pushers.list`).
//!
//! This module owns the admin API's half -- scopes, request validation, idempotency, the audit
//! entry, the event and the log line -- and reaches the data through two seams:
//!
//! - [`UserIdentitySource`], implemented by track 07 (`hs_auth::admin_directory`) over the auth
//!   store: devices, 3PIDs, external ids and experimental features all live there.
//! - [`UserDataSource`], implemented in `hs serve` over `hs-user`'s account-data store and
//!   `hs-push`'s pusher store.
//!
//! Every write is audited and published: `user.device_updated`, `user.devices_deleted`,
//! `user.threepid_added`, `user.threepid_removed`, `user.external_id_added`,
//! `user.external_id_removed`, `user.experimental_features_changed`.

// The helpers below return `Result<_, Response>` so a handler can answer with the error
// directly, as `router::record_mutation` does: a `Response` is built once per request, not on a
// hot path, so boxing it would only add noise at every call site.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{ScopeDecision, require_scope};
use crate::idempotency::{Replay, StoredResponse};
use crate::model::{
    AdminDevice, AuditChange, ExternalId, Page, Principal, ResourceRef, Scope, ThreePid,
};
use crate::router::{
    AdminState, authorization_header, idempotency_key, record_mutation, replay_response,
    source_unavailable,
};
use crate::sources::SourceError;

/// The most devices one `users.devices.bulk_delete` request may name.
pub const MAX_BULK_DEVICES: usize = 1000;

/// The per-user experimental features this server accepts, the same names Synapse's
/// `ExperimentalFeature` uses so a flag set there means the same thing here. Setting any other
/// name is a `400` that lists these.
///
/// - `msc3881`: remotely toggling push notifications for another device.
/// - `msc3575`: sliding sync (the pre-MSC4186 proxy-era API).
/// - `msc4222`: `state_after` in `/sync`.
///
/// They are stored and reported; none of them yet changes what this server does for the user,
/// because none of the three unstable behaviours is gated per user here.
pub const KNOWN_EXPERIMENTAL_FEATURES: &[&str] = &["msc3575", "msc3881", "msc4222"];

/// Devices, 3PIDs, external ids and experimental features of one account.
///
/// Every method answers [`SourceError::NotFound`] when `user_id` is not an account on this
/// server, and the more specific cases its own doc comment names.
#[async_trait::async_trait]
pub trait UserIdentitySource: Send + Sync + 'static {
    /// One device. [`SourceError::NotFound`] for a device the user does not have.
    async fn get_device(&self, user_id: &str, device_id: &str) -> Result<AdminDevice, SourceError>;

    /// Renames one device (`None` clears the name), telling everybody who shares a room with the
    /// user that their device list changed. Returns the device as it now is.
    async fn rename_device(
        &self,
        user_id: &str,
        device_id: &str,
        display_name: Option<String>,
    ) -> Result<AdminDevice, SourceError>;

    /// Signs out and deletes every one of `device_ids`, removing their keys: all or nothing.
    /// [`SourceError::NotFound`] naming the missing ones if any is not one of the user's
    /// devices, and then nothing has been deleted.
    async fn delete_devices(&self, user_id: &str, device_ids: &[String])
    -> Result<(), SourceError>;

    /// The 3PIDs bound to the account.
    async fn list_threepids(&self, user_id: &str) -> Result<Vec<ThreePid>, SourceError>;

    /// Binds a 3PID, answering it as stored (with `added_at`). [`SourceError::Conflict`] if
    /// another account has it. Binding one the account already has answers the existing one.
    async fn add_threepid(
        &self,
        user_id: &str,
        threepid: ThreePid,
    ) -> Result<ThreePid, SourceError>;

    /// Unbinds a 3PID. [`SourceError::NotFound`] if the account does not have it.
    async fn remove_threepid(
        &self,
        user_id: &str,
        medium: &str,
        address: &str,
    ) -> Result<(), SourceError>;

    /// The upstream subjects linked to the account.
    async fn list_external_ids(&self, user_id: &str) -> Result<Vec<ExternalId>, SourceError>;

    /// Links an upstream subject. [`SourceError::Conflict`] if another account has it.
    async fn add_external_id(
        &self,
        user_id: &str,
        external_id: ExternalId,
    ) -> Result<ExternalId, SourceError>;

    /// Removes a link. [`SourceError::NotFound`] if the account does not have it.
    async fn remove_external_id(
        &self,
        user_id: &str,
        provider: &str,
        external_id: &str,
    ) -> Result<(), SourceError>;

    /// The account's experimental-feature flags.
    async fn experimental_features(
        &self,
        user_id: &str,
    ) -> Result<BTreeMap<String, bool>, SourceError>;

    /// Replaces the account's experimental-feature flags with `features` (already validated and
    /// merged by the handler). Returns them as stored.
    async fn set_experimental_features(
        &self,
        user_id: &str,
        features: BTreeMap<String, bool>,
    ) -> Result<BTreeMap<String, bool>, SourceError>;
}

/// Read-only views of what a user's clients have stored on the server. The handler checks the
/// account exists (through [`crate::sources::UserDirectory`]) before asking.
#[async_trait::async_trait]
pub trait UserDataSource: Send + Sync + 'static {
    /// Global account data, event type to content.
    async fn account_data(&self, user_id: &str) -> Result<BTreeMap<String, Value>, SourceError>;

    /// Every pusher the user's clients registered, as `GET /_matrix/client/v3/pushers` shows them.
    async fn pushers(&self, user_id: &str) -> Result<Vec<Value>, SourceError>;
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
}

/// Checks the scope; the principal, or the response to answer with.
async fn authorize(
    state: &AdminState,
    headers: &HeaderMap,
    scope: Scope,
    instance: &str,
) -> Result<Principal, Response> {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(headers),
        Some(scope),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => Ok(principal),
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            Err(p.with_instance(instance).into_response())
        }
    }
}

fn identity_source<'a>(
    state: &'a AdminState,
    instance: &str,
) -> Result<&'a dyn UserIdentitySource, Response> {
    state
        .user_identity
        .as_deref()
        .ok_or_else(|| source_unavailable("user identity", instance))
}

fn problem(error: SourceError, instance: &str) -> Response {
    error.to_problem().with_instance(instance).into_response()
}

fn not_found(detail: String, instance: &str) -> Response {
    Problem::not_found()
        .with_detail(detail)
        .with_instance(instance)
        .into_response()
}

fn invalid(pointer: &'static str, detail: impl Into<String>, instance: &str) -> Response {
    problem(
        SourceError::InvalidField {
            pointer,
            detail: detail.into(),
        },
        instance,
    )
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8], instance: &str) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|e| {
        Problem::validation_failed()
            .with_detail(format!("invalid JSON body: {e}"))
            .with_instance(instance)
            .into_response()
    })
}

/// `Replay::Fresh` goes on; a replay or a mismatch is the answer.
fn check_idempotency(
    state: &AdminState,
    headers: &HeaderMap,
    operation_id: &str,
    body: &[u8],
    instance: &str,
) -> Result<(), Response> {
    let Some(key) = idempotency_key(headers) else {
        return Ok(());
    };
    match state.idempotency.check(operation_id, key, body) {
        Replay::Same(stored) => Err(replay_response(stored)),
        Replay::Mismatch => Err(Problem::idempotency_key_payload_mismatch()
            .with_instance(instance)
            .into_response()),
        Replay::Fresh => Ok(()),
    }
}

fn remember(
    state: &AdminState,
    headers: &HeaderMap,
    operation_id: &str,
    body: &[u8],
    status: StatusCode,
    response_body: &[u8],
) {
    if let Some(key) = idempotency_key(headers) {
        state.idempotency.record(
            operation_id,
            key,
            body,
            StoredResponse {
                status: status.as_u16(),
                content_type: "application/json".to_string(),
                body: response_body.to_vec(),
            },
        );
    }
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn user_ref(user_id: &str) -> ResourceRef {
    ResourceRef::new("user", user_id.to_owned())
}

// -------------------------------------------------------------------------------------------
// Devices
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/users/{user_id}/devices/{device_id}` (`admin:read`).
pub(crate) async fn devices_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((user_id, device_id)): Path<(String, String)>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/devices/{device_id}");
    if let Err(resp) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return resp;
    }
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match source.get_device(&user_id, &device_id).await {
        Ok(device) => axum::Json(device).into_response(),
        Err(SourceError::NotFound) => not_found(
            format!("no such device: {device_id} of {user_id}"),
            &instance,
        ),
        Err(e) => problem(e, &instance),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceUpdateBody {
    /// `None` when absent, which is a validation failure; `Some(None)` for `null`, which (like a
    /// blank name) clears it.
    #[serde(default, deserialize_with = "present")]
    display_name: Option<Option<String>>,
}

/// Tells a `null` field (`Some(None)`) from an absent one (`None`, through `default`).
fn present<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

/// `PATCH /api/v1/users/{user_id}/devices/{device_id}` (`admin:write`): renames a device.
pub(crate) async fn devices_update(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((user_id, device_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/devices/{device_id}");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let request: DeviceUpdateBody = match parse_body(&body, &instance) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let Some(display_name) = request.display_name else {
        return invalid(
            "/display_name",
            "display_name is required (null clears it)",
            &instance,
        );
    };
    let display_name = display_name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty());
    if display_name
        .as_ref()
        .is_some_and(|n| n.chars().count() > 256)
    {
        return invalid("/display_name", "at most 256 characters", &instance);
    }
    let before = match source.get_device(&user_id, &device_id).await {
        Ok(d) => d,
        Err(SourceError::NotFound) => {
            return not_found(
                format!("no such device: {device_id} of {user_id}"),
                &instance,
            );
        }
        Err(e) => return problem(e, &instance),
    };
    let after = match source
        .rename_device(&user_id, &device_id, display_name)
        .await
    {
        Ok(d) => d,
        Err(SourceError::NotFound) => {
            return not_found(
                format!("no such device: {device_id} of {user_id}"),
                &instance,
            );
        }
        Err(e) => return problem(e, &instance),
    };
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "users.devices.update",
        "user.device_updated",
        user_ref(&user_id),
        vec![AuditChange {
            pointer: format!("/devices/{device_id}/display_name"),
            from: Some(json!(before.display_name)),
            to: Some(json!(after.display_name)),
        }],
        json!({ "device_id": device_id, "display_name": after.display_name }),
    )
    .await
    {
        return resp;
    }
    tracing::info!(
        actor = %principal.id,
        %user_id,
        %device_id,
        "an administrator renamed a device"
    );
    axum::Json(after).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkDeleteBody {
    device_ids: Vec<String>,
}

/// `POST /api/v1/users/{user_id}/devices/bulk-delete` (`admin:write`): signs out several
/// devices at once, all or nothing. `204`. Honors `Idempotency-Key`.
pub(crate) async fn devices_bulk_delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    const OP: &str = "users.devices.bulk_delete";
    let instance = format!("/api/v1/users/{user_id}/devices/bulk-delete");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(resp) = check_idempotency(&state, &headers, OP, &body, &instance) {
        return resp;
    }
    let request: BulkDeleteBody = match parse_body(&body, &instance) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let mut device_ids: Vec<String> = Vec::with_capacity(request.device_ids.len());
    for id in request.device_ids {
        let id = id.trim().to_owned();
        if id.is_empty() {
            return invalid("/device_ids", "a device ID cannot be empty", &instance);
        }
        if !device_ids.contains(&id) {
            device_ids.push(id);
        }
    }
    if device_ids.is_empty() {
        return invalid("/device_ids", "name at least one device", &instance);
    }
    if device_ids.len() > MAX_BULK_DEVICES {
        return invalid(
            "/device_ids",
            format!("at most {MAX_BULK_DEVICES} devices per request"),
            &instance,
        );
    }
    match source.delete_devices(&user_id, &device_ids).await {
        Ok(()) => {}
        Err(SourceError::NotFound) => {
            return not_found(
                format!("{user_id} does not have every one of these devices"),
                &instance,
            );
        }
        Err(e) => return problem(e, &instance),
    }
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        OP,
        "user.devices_deleted",
        user_ref(&user_id),
        Vec::new(),
        json!({ "device_ids": device_ids }),
    )
    .await
    {
        return resp;
    }
    tracing::info!(
        actor = %principal.id,
        %user_id,
        count = device_ids.len(),
        "an administrator signed out devices"
    );
    remember(&state, &headers, OP, &body, StatusCode::NO_CONTENT, &[]);
    StatusCode::NO_CONTENT.into_response()
}

// -------------------------------------------------------------------------------------------
// 3PIDs
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/users/{user_id}/threepids` (`admin:read`).
pub(crate) async fn threepids_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/threepids");
    if let Err(resp) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return resp;
    }
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match source.list_threepids(&user_id).await {
        Ok(items) => axum::Json(items).into_response(),
        Err(SourceError::NotFound) => not_found(format!("no such user: {user_id}"), &instance),
        Err(e) => problem(e, &instance),
    }
}

/// Normalises a 3PID the way Synapse does: an email address is trimmed and lower-cased and
/// must look like one; a phone number is its digits (a leading `+` and spaces, dots, dashes
/// and brackets are dropped).
fn normalise_threepid(threepid: ThreePid) -> Result<ThreePid, SourceError> {
    let address = threepid.address.trim();
    let address = match threepid.medium.as_str() {
        "email" => {
            let lower = address.to_lowercase();
            let valid = lower
                .split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
                && !lower.chars().any(char::is_whitespace);
            if !valid {
                return Err(SourceError::InvalidField {
                    pointer: "/address",
                    detail: format!("{address:?} is not an email address"),
                });
            }
            lower
        }
        "msisdn" => {
            let digits: String = address
                .chars()
                .filter(|c| !matches!(c, '+' | ' ' | '-' | '.' | '(' | ')'))
                .collect();
            if digits.len() < 5 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit())
            {
                return Err(SourceError::InvalidField {
                    pointer: "/address",
                    detail: format!(
                        "{address:?} is not a phone number (international format, digits only)"
                    ),
                });
            }
            digits
        }
        other => {
            return Err(SourceError::InvalidField {
                pointer: "/medium",
                detail: format!("{other:?} is not a medium; use email or msisdn"),
            });
        }
    };
    Ok(ThreePid {
        medium: threepid.medium,
        address,
        added_at: None,
    })
}

/// `POST /api/v1/users/{user_id}/threepids` (`admin:write`): binds an email address or phone
/// number to the account. `201` with it as stored. Honors `Idempotency-Key`.
pub(crate) async fn threepids_add(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    const OP: &str = "users.threepids.add";
    let instance = format!("/api/v1/users/{user_id}/threepids");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(resp) = check_idempotency(&state, &headers, OP, &body, &instance) {
        return resp;
    }
    let request: ThreePid = match parse_body(&body, &instance) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let threepid = match normalise_threepid(request) {
        Ok(t) => t,
        Err(e) => return problem(e, &instance),
    };
    let added = match source.add_threepid(&user_id, threepid).await {
        Ok(t) => t,
        Err(SourceError::NotFound) => {
            return not_found(format!("no such user: {user_id}"), &instance);
        }
        Err(e) => return problem(e, &instance),
    };
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        OP,
        "user.threepid_added",
        user_ref(&user_id),
        vec![AuditChange {
            pointer: "/threepids/-".to_owned(),
            from: None,
            to: Some(json!({"medium": added.medium, "address": added.address})),
        }],
        json!({ "medium": added.medium, "address": added.address }),
    )
    .await
    {
        return resp;
    }
    tracing::info!(
        actor = %principal.id,
        %user_id,
        medium = %added.medium,
        "an administrator bound a 3PID"
    );
    let response_body = serde_json::to_vec(&added).unwrap_or_default();
    remember(
        &state,
        &headers,
        OP,
        &body,
        StatusCode::CREATED,
        &response_body,
    );
    json_response(StatusCode::CREATED, response_body)
}

/// `DELETE /api/v1/users/{user_id}/threepids/{medium}/{address}` (`admin:write`). `204`.
pub(crate) async fn threepids_remove(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((user_id, medium, address)): Path<(String, String, String)>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/threepids/{medium}/{address}");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match source.remove_threepid(&user_id, &medium, &address).await {
        Ok(()) => {}
        Err(SourceError::NotFound) => {
            return not_found(
                format!("{user_id} does not have {medium} {address}"),
                &instance,
            );
        }
        Err(e) => return problem(e, &instance),
    }
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "users.threepids.remove",
        "user.threepid_removed",
        user_ref(&user_id),
        vec![AuditChange {
            pointer: "/threepids".to_owned(),
            from: Some(json!({"medium": medium, "address": address})),
            to: None,
        }],
        json!({ "medium": medium, "address": address }),
    )
    .await
    {
        return resp;
    }
    tracing::info!(actor = %principal.id, %user_id, %medium, "an administrator unbound a 3PID");
    StatusCode::NO_CONTENT.into_response()
}

// -------------------------------------------------------------------------------------------
// External ids
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/users/{user_id}/external-ids` (`admin:read`).
pub(crate) async fn external_ids_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/external-ids");
    if let Err(resp) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return resp;
    }
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match source.list_external_ids(&user_id).await {
        Ok(items) => axum::Json(items).into_response(),
        Err(SourceError::NotFound) => not_found(format!("no such user: {user_id}"), &instance),
        Err(e) => problem(e, &instance),
    }
}

/// `POST /api/v1/users/{user_id}/external-ids` (`admin:write`): links an upstream subject to
/// the account. `201`. Honors `Idempotency-Key`.
pub(crate) async fn external_ids_add(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    const OP: &str = "users.external_ids.add";
    let instance = format!("/api/v1/users/{user_id}/external-ids");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(resp) = check_idempotency(&state, &headers, OP, &body, &instance) {
        return resp;
    }
    let request: ExternalId = match parse_body(&body, &instance) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let provider = request.provider.trim().to_owned();
    // A subject is case-sensitive and may carry meaningful spaces inside it, but never at the
    // ends of a pasted value.
    let external_id = request.external_id.trim().to_owned();
    if provider.is_empty() {
        return invalid("/provider", "name the identity provider", &instance);
    }
    if provider.len() > 255 || provider.contains('/') {
        return invalid(
            "/provider",
            "at most 255 characters, without '/'",
            &instance,
        );
    }
    if external_id.is_empty() {
        return invalid("/external_id", "the subject cannot be empty", &instance);
    }
    if external_id.len() > 1024 {
        return invalid("/external_id", "at most 1024 characters", &instance);
    }
    let added = match source
        .add_external_id(
            &user_id,
            ExternalId {
                provider,
                external_id,
            },
        )
        .await
    {
        Ok(e) => e,
        Err(SourceError::NotFound) => {
            return not_found(format!("no such user: {user_id}"), &instance);
        }
        Err(e) => return problem(e, &instance),
    };
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        OP,
        "user.external_id_added",
        user_ref(&user_id),
        vec![AuditChange {
            pointer: "/external_ids/-".to_owned(),
            from: None,
            to: Some(json!(added)),
        }],
        json!({ "provider": added.provider, "external_id": added.external_id }),
    )
    .await
    {
        return resp;
    }
    tracing::info!(
        actor = %principal.id,
        %user_id,
        provider = %added.provider,
        "an administrator linked an external id"
    );
    let response_body = serde_json::to_vec(&added).unwrap_or_default();
    remember(
        &state,
        &headers,
        OP,
        &body,
        StatusCode::CREATED,
        &response_body,
    );
    json_response(StatusCode::CREATED, response_body)
}

/// `DELETE /api/v1/users/{user_id}/external-ids/{provider}/{external_id}` (`admin:write`).
/// `204`.
pub(crate) async fn external_ids_remove(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((user_id, provider, external_id)): Path<(String, String, String)>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/external-ids/{provider}/{external_id}");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match source
        .remove_external_id(&user_id, &provider, &external_id)
        .await
    {
        Ok(()) => {}
        Err(SourceError::NotFound) => {
            return not_found(
                format!("{user_id} is not linked to {external_id} at {provider}"),
                &instance,
            );
        }
        Err(e) => return problem(e, &instance),
    }
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "users.external_ids.remove",
        "user.external_id_removed",
        user_ref(&user_id),
        vec![AuditChange {
            pointer: "/external_ids".to_owned(),
            from: Some(json!({"provider": provider, "external_id": external_id})),
            to: None,
        }],
        json!({ "provider": provider, "external_id": external_id }),
    )
    .await
    {
        return resp;
    }
    tracing::info!(
        actor = %principal.id,
        %user_id,
        %provider,
        "an administrator unlinked an external id"
    );
    StatusCode::NO_CONTENT.into_response()
}

// -------------------------------------------------------------------------------------------
// Experimental features
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/users/{user_id}/experimental-features` (`admin:read`): every known feature,
/// `false` for one never set, so the page can offer a switch for each.
pub(crate) async fn experimental_features_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/experimental-features");
    if let Err(resp) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return resp;
    }
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match source.experimental_features(&user_id).await {
        Ok(stored) => axum::Json(with_every_known_feature(stored)).into_response(),
        Err(SourceError::NotFound) => not_found(format!("no such user: {user_id}"), &instance),
        Err(e) => problem(e, &instance),
    }
}

fn with_every_known_feature(mut features: BTreeMap<String, bool>) -> BTreeMap<String, bool> {
    for known in KNOWN_EXPERIMENTAL_FEATURES {
        features.entry((*known).to_owned()).or_insert(false);
    }
    features
}

/// `PUT /api/v1/users/{user_id}/experimental-features` (`admin:write`): sets the flags the body
/// names and leaves the others as they were (Synapse's semantics for the same operation), and
/// answers every known flag. A name that is not one of [`KNOWN_EXPERIMENTAL_FEATURES`] is a
/// `400` and nothing changes.
pub(crate) async fn experimental_features_put(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/experimental-features");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let source = match identity_source(&state, &instance) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let request: BTreeMap<String, bool> = match parse_body(&body, &instance) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    if let Some(unknown) = request
        .keys()
        .find(|k| !KNOWN_EXPERIMENTAL_FEATURES.contains(&k.as_str()))
    {
        return Problem::validation_failed()
            .with_detail(format!(
                "{unknown:?} is not an experimental feature this server knows; it knows {}",
                KNOWN_EXPERIMENTAL_FEATURES.join(", ")
            ))
            .with_errors(vec![hs_http::ValidationError::new(
                format!("/{unknown}"),
                "unknown experimental feature",
            )])
            .with_instance(instance)
            .into_response();
    }
    let before = match source.experimental_features(&user_id).await {
        Ok(f) => f,
        Err(SourceError::NotFound) => {
            return not_found(format!("no such user: {user_id}"), &instance);
        }
        Err(e) => return problem(e, &instance),
    };
    let mut merged = before.clone();
    merged.extend(request);
    let changes: Vec<AuditChange> = merged
        .iter()
        .filter(|(k, v)| before.get(*k).copied().unwrap_or(false) != **v)
        .map(|(k, v)| AuditChange {
            pointer: format!("/{k}"),
            from: Some(json!(before.get(k).copied().unwrap_or(false))),
            to: Some(json!(v)),
        })
        .collect();
    let stored = match source.set_experimental_features(&user_id, merged).await {
        Ok(f) => f,
        Err(SourceError::NotFound) => {
            return not_found(format!("no such user: {user_id}"), &instance);
        }
        Err(e) => return problem(e, &instance),
    };
    let answer = with_every_known_feature(stored);
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "users.experimental_features.put",
        "user.experimental_features_changed",
        user_ref(&user_id),
        changes,
        json!({ "features": answer }),
    )
    .await
    {
        return resp;
    }
    tracing::info!(
        actor = %principal.id,
        %user_id,
        "an administrator set experimental features"
    );
    axum::Json(answer).into_response()
}

// -------------------------------------------------------------------------------------------
// Account data and pushers (read-only)
// -------------------------------------------------------------------------------------------

/// `404` for a user this server does not have, via the user directory; `Ok` otherwise (and
/// when no directory is wired, in which case the data source answers for itself).
async fn require_user(state: &AdminState, user_id: &str, instance: &str) -> Result<(), Response> {
    let Some(users) = &state.users else {
        return Ok(());
    };
    match users.get_user(user_id).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) | Err(SourceError::NotFound) => {
            Err(not_found(format!("no such user: {user_id}"), instance))
        }
        Err(e) => Err(problem(e, instance)),
    }
}

/// `GET /api/v1/users/{user_id}/account-data` (`admin:read`): global account data, event type
/// to content, all of it (the contract's paging parameters are accepted and not needed: a user
/// has a handful of types, not pages of them).
pub(crate) async fn account_data_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(_query): Query<PageQuery>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/account-data");
    if let Err(resp) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return resp;
    }
    let Some(source) = &state.user_data else {
        return source_unavailable("user data", &instance);
    };
    if let Err(resp) = require_user(&state, &user_id, &instance).await {
        return resp;
    }
    match source.account_data(&user_id).await {
        Ok(data) => axum::Json(data).into_response(),
        Err(SourceError::NotFound) => not_found(format!("no such user: {user_id}"), &instance),
        Err(e) => problem(e, &instance),
    }
}

/// `GET /api/v1/users/{user_id}/pushers` (`admin:read`): a page of the user's pushers.
pub(crate) async fn pushers_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/pushers");
    if let Err(resp) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return resp;
    }
    let Some(source) = &state.user_data else {
        return source_unavailable("user data", &instance);
    };
    if let Err(resp) = require_user(&state, &user_id, &instance).await {
        return resp;
    }
    match source.pushers(&user_id).await {
        Ok(items) => axum::Json(Page::paginate(
            items,
            query.cursor.as_deref(),
            query.limit,
            query.include_total.unwrap_or(false),
        ))
        .into_response(),
        Err(SourceError::NotFound) => not_found(format!("no such user: {user_id}"), &instance),
        Err(e) => problem(e, &instance),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::audit::InMemoryAuditSink;
    use crate::auth::StaticVerifier;
    use crate::events::EventBus;
    use crate::model::PrincipalKind;
    use crate::router::build_router;

    const ALICE: &str = "%40alice%3Aexample.org";

    /// One user, `@alice:example.org`, with the devices, 3PIDs and flags a test gives her.
    #[derive(Default)]
    struct Fake {
        devices: Mutex<Vec<AdminDevice>>,
        threepids: Mutex<Vec<ThreePid>>,
        external_ids: Mutex<Vec<ExternalId>>,
        features: Mutex<BTreeMap<String, bool>>,
    }

    fn known(user_id: &str) -> Result<(), SourceError> {
        if user_id == "@alice:example.org" {
            Ok(())
        } else {
            Err(SourceError::NotFound)
        }
    }

    fn removed(before: usize, after: usize) -> Result<(), SourceError> {
        if before == after {
            Err(SourceError::NotFound)
        } else {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl UserIdentitySource for Fake {
        async fn get_device(
            &self,
            user_id: &str,
            device_id: &str,
        ) -> Result<AdminDevice, SourceError> {
            known(user_id)?;
            let devices = self.devices.lock().unwrap();
            devices
                .iter()
                .find(|d| d.device_id == device_id)
                .cloned()
                .ok_or(SourceError::NotFound)
        }
        async fn rename_device(
            &self,
            user_id: &str,
            device_id: &str,
            display_name: Option<String>,
        ) -> Result<AdminDevice, SourceError> {
            known(user_id)?;
            let mut devices = self.devices.lock().unwrap();
            let device = devices
                .iter_mut()
                .find(|d| d.device_id == device_id)
                .ok_or(SourceError::NotFound)?;
            device.display_name = display_name;
            Ok(device.clone())
        }
        async fn delete_devices(
            &self,
            user_id: &str,
            device_ids: &[String],
        ) -> Result<(), SourceError> {
            known(user_id)?;
            let mut devices = self.devices.lock().unwrap();
            if !device_ids
                .iter()
                .all(|id| devices.iter().any(|d| &d.device_id == id))
            {
                return Err(SourceError::NotFound);
            }
            devices.retain(|d| !device_ids.contains(&d.device_id));
            Ok(())
        }
        async fn list_threepids(&self, user_id: &str) -> Result<Vec<ThreePid>, SourceError> {
            known(user_id)?;
            Ok(self.threepids.lock().unwrap().clone())
        }
        async fn add_threepid(
            &self,
            user_id: &str,
            threepid: ThreePid,
        ) -> Result<ThreePid, SourceError> {
            known(user_id)?;
            let stored = ThreePid {
                added_at: Some("2026-09-28T00:00:00.000Z".into()),
                ..threepid
            };
            self.threepids.lock().unwrap().push(stored.clone());
            Ok(stored)
        }
        async fn remove_threepid(
            &self,
            user_id: &str,
            medium: &str,
            address: &str,
        ) -> Result<(), SourceError> {
            known(user_id)?;
            let mut threepids = self.threepids.lock().unwrap();
            let before = threepids.len();
            threepids.retain(|t| !(t.medium == medium && t.address == address));
            removed(before, threepids.len())
        }
        async fn list_external_ids(&self, user_id: &str) -> Result<Vec<ExternalId>, SourceError> {
            known(user_id)?;
            Ok(self.external_ids.lock().unwrap().clone())
        }
        async fn add_external_id(
            &self,
            user_id: &str,
            external_id: ExternalId,
        ) -> Result<ExternalId, SourceError> {
            known(user_id)?;
            self.external_ids.lock().unwrap().push(external_id.clone());
            Ok(external_id)
        }
        async fn remove_external_id(
            &self,
            user_id: &str,
            provider: &str,
            external_id: &str,
        ) -> Result<(), SourceError> {
            known(user_id)?;
            let mut ids = self.external_ids.lock().unwrap();
            let before = ids.len();
            ids.retain(|e| !(e.provider == provider && e.external_id == external_id));
            removed(before, ids.len())
        }
        async fn experimental_features(
            &self,
            user_id: &str,
        ) -> Result<BTreeMap<String, bool>, SourceError> {
            known(user_id)?;
            Ok(self.features.lock().unwrap().clone())
        }
        async fn set_experimental_features(
            &self,
            user_id: &str,
            features: BTreeMap<String, bool>,
        ) -> Result<BTreeMap<String, bool>, SourceError> {
            known(user_id)?;
            *self.features.lock().unwrap() = features.clone();
            Ok(features)
        }
    }

    fn principal(scopes: Vec<Scope>) -> Principal {
        Principal {
            kind: PrincipalKind::User,
            id: "@ops:example.org".into(),
            display_name: None,
            scopes,
            token_id: None,
            expires_at: None,
            issued_by: None,
        }
    }

    fn state(source: Option<Arc<Fake>>) -> AdminState {
        let verifier = StaticVerifier::new()
            .with_token("write", principal(vec![Scope::AdminWrite]))
            .with_token("read", principal(vec![Scope::AdminRead]));
        let state = AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        );
        match source {
            Some(source) => state.with_user_identity(source),
            None => state,
        }
    }

    fn request(method: &str, uri: &str, token: &str, body: Option<&Value>) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap()
    }

    async fn call(
        router: &axum::Router,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(request(method, uri, token, body.as_ref()))
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn with_devices() -> Arc<Fake> {
        let fake = Fake::default();
        for id in ["PHONE", "LAPTOP"] {
            fake.devices.lock().unwrap().push(AdminDevice {
                device_id: id.into(),
                display_name: Some(format!("{id} name")),
                last_seen_ip: None,
                last_seen_at: None,
            });
        }
        Arc::new(fake)
    }

    #[tokio::test]
    async fn unwired_answers_503_and_a_read_token_cannot_write() {
        let (router, _) = build_router(state(None));
        for uri in [
            format!("/api/v1/users/{ALICE}/threepids"),
            format!("/api/v1/users/{ALICE}/external-ids"),
            format!("/api/v1/users/{ALICE}/experimental-features"),
            format!("/api/v1/users/{ALICE}/devices/PHONE"),
            format!("/api/v1/users/{ALICE}/account-data"),
            format!("/api/v1/users/{ALICE}/pushers"),
        ] {
            let (status, body) = call(&router, "GET", &uri, "read", None).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}: {body}");
        }
        let (router, _) = build_router(state(Some(with_devices())));
        let (status, _) = call(
            &router,
            "POST",
            &format!("/api/v1/users/{ALICE}/threepids"),
            "read",
            Some(json!({"medium": "email", "address": "a@example.org"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_rename_needs_the_field_and_null_clears_it() {
        let (router, _) = build_router(state(Some(with_devices())));
        let uri = format!("/api/v1/users/{ALICE}/devices/PHONE");
        let (status, _) = call(&router, "PATCH", &uri, "write", Some(json!({}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = call(
            &router,
            "PATCH",
            &uri,
            "write",
            Some(json!({"display_name": "  New  "})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["display_name"], "New");
        let (_, body) = call(
            &router,
            "PATCH",
            &uri,
            "write",
            Some(json!({"display_name": null})),
        )
        .await;
        assert_eq!(body["display_name"], Value::Null);
        let (status, _) = call(
            &router,
            "PATCH",
            &format!("/api/v1/users/{ALICE}/devices/NOPE"),
            "write",
            Some(json!({"display_name": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn bulk_delete_is_all_or_nothing_and_replays_on_the_same_key() {
        let fake = with_devices();
        let (router, _) = build_router(state(Some(fake.clone())));
        let uri = format!("/api/v1/users/{ALICE}/devices/bulk-delete");
        let (status, _) = call(
            &router,
            "POST",
            &uri,
            "write",
            Some(json!({"device_ids": ["PHONE", "NOPE"]})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(fake.devices.lock().unwrap().len(), 2);
        let (status, _) = call(
            &router,
            "POST",
            &uri,
            "write",
            Some(json!({"device_ids": [" "]})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let body = json!({"device_ids": ["PHONE", "PHONE"]});
        let keyed = || {
            let mut r = request("POST", &uri, "write", Some(&body));
            r.headers_mut()
                .insert("idempotency-key", "k1".parse().unwrap());
            r
        };
        let first = router.clone().oneshot(keyed()).await.unwrap();
        assert_eq!(first.status(), StatusCode::NO_CONTENT);
        let again = router.clone().oneshot(keyed()).await.unwrap();
        assert_eq!(again.status(), StatusCode::NO_CONTENT);
        assert_eq!(again.headers()["idempotency-replayed"], "true");
        let left: Vec<String> = fake
            .devices
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.device_id.clone())
            .collect();
        assert_eq!(left, ["LAPTOP"]);
    }

    #[test]
    fn threepids_are_normalised_or_refused() {
        let make = |medium: &str, address: &str| ThreePid {
            medium: medium.into(),
            address: address.into(),
            added_at: None,
        };
        assert_eq!(
            normalise_threepid(make("email", " Bob@Example.ORG "))
                .unwrap()
                .address,
            "bob@example.org"
        );
        assert!(normalise_threepid(make("email", "bob")).is_err());
        assert!(normalise_threepid(make("email", "bob@localhost")).is_err());
        assert_eq!(
            normalise_threepid(make("msisdn", "+44 (7700) 900-123"))
                .unwrap()
                .address,
            "447700900123"
        );
        assert!(normalise_threepid(make("msisdn", "call me")).is_err());
        assert!(normalise_threepid(make("fax", "1")).is_err());
    }

    #[tokio::test]
    async fn experimental_features_merge_refuse_unknown_names_and_report_every_known_one() {
        let fake = Arc::new(Fake::default());
        let (router, _) = build_router(state(Some(fake.clone())));
        let uri = format!("/api/v1/users/{ALICE}/experimental-features");
        let (_, body) = call(&router, "GET", &uri, "read", None).await;
        assert_eq!(
            body,
            json!({"msc3575": false, "msc3881": false, "msc4222": false})
        );
        let (status, body) = call(
            &router,
            "PUT",
            &uri,
            "write",
            Some(json!({"msc3881": true, "nope": true})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            fake.features.lock().unwrap().is_empty(),
            "nothing changes on a refusal"
        );
        call(
            &router,
            "PUT",
            &uri,
            "write",
            Some(json!({"msc3881": true})),
        )
        .await;
        let (status, body) = call(
            &router,
            "PUT",
            &uri,
            "write",
            Some(json!({"msc4222": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"msc3575": false, "msc3881": true, "msc4222": true})
        );
        let (status, _) = call(
            &router,
            "GET",
            "/api/v1/users/%40nobody%3Aexample.org/experimental-features",
            "read",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn threepid_and_external_id_writes_publish_events() {
        let state = state(Some(Arc::new(Fake::default())));
        let mut events = state.events.subscribe();
        let (router, _) = build_router(state);
        let (status, body) = call(
            &router,
            "POST",
            &format!("/api/v1/users/{ALICE}/threepids"),
            "write",
            Some(json!({"medium": "email", "address": "Alice@Example.org"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["address"], "alice@example.org");
        assert_eq!(events.recv().await.unwrap().r#type, "user.threepid_added");
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/api/v1/users/{ALICE}/threepids/email/alice%40example.org"),
            "write",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(events.recv().await.unwrap().r#type, "user.threepid_removed");
        let (status, _) = call(
            &router,
            "POST",
            &format!("/api/v1/users/{ALICE}/external-ids"),
            "write",
            Some(json!({"provider": "", "external_id": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &router,
            "POST",
            &format!("/api/v1/users/{ALICE}/external-ids"),
            "write",
            Some(json!({"provider": "oidc", "external_id": "sub"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            events.recv().await.unwrap().r#type,
            "user.external_id_added"
        );
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/api/v1/users/{ALICE}/external-ids/oidc/sub"),
            "write",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            events.recv().await.unwrap().r#type,
            "user.external_id_removed"
        );
    }
}
