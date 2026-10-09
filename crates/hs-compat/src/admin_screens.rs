//! The rest of the `/_synapse/admin` routes `synapse-admin`'s screens call, shimmed onto the
//! native `/api/v1` router the way [`crate::admin_proxy`] shims the first eight: each request is
//! forwarded in-process into the native router behind the caller's own token and scopes, and
//! the answer is reshaped into Synapse's JSON. Nothing here decides anything about data or
//! access; it only translates shapes. `docs/compat/synapse-admin-routes.md` lists every route
//! and each one's difference from Synapse.
//!
//! The screens, and what they call:
//!
//! - **Users** (list, detail, edit): `GET`/`PUT /v2/users/{id}` (create or modify: name, avatar,
//!   administrator, password, deactivation, lock, user type, 3PIDs, external identities),
//!   `GET`/`PUT /v1/users/{id}/admin`, devices (list, one, rename, delete, bulk delete),
//!   `joined_rooms`, `pushers`, `media`, `accountdata`, `whois`, `reset_password`, `login`
//!   (act as), `shadow_ban`, `suspend`, `username_available`, `experimental_features`, and
//!   the lookups by third-party identifier and by upstream identity.
//! - **Rooms** (list, detail, members, state, delete, block, make admin): `members`, `state`,
//!   `DELETE /v1|v2/rooms/{id}`, `delete_status`, `block`, `make_room_admin`, `join`,
//!   `messages`, `context`, `forward_extremities`, `room/{id}/media`.
//! - **Registration tokens**: all five routes. **Reports**: list, one, delete. **Media**:
//!   `statistics/users/media`, delete, quarantine, unquarantine, protect, unprotect.
//!   **Federation**: destinations (list, one, rooms, reset connection).
//!
//! Every route is in [`ROUTES`], which `hs-cli`'s manifest mirrors, and the unit tests here
//! hold the router to that list: a route named there answers something other than "not
//! found", and a route mounted is named there.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::admin_proxy::{
    AdminProxyState, forward, full_record, parse_rfc3339_ms, translate_error_body,
    urlencoding_light as enc,
};

/// Every `/_synapse/admin` route this crate mounts, with the operation id `routes.json` names
/// it by: the first eight from [`crate::admin_proxy`], the rest from here.
pub const ROUTES: &[(&str, &str, &str)] = &[
    (
        "GET",
        "/_synapse/admin/v1/server_version",
        "synapseAdminServerVersion",
    ),
    ("GET", "/_synapse/admin/v2/users", "synapseAdminUsersList"),
    (
        "GET",
        "/_synapse/admin/v2/users/{user_id}",
        "synapseAdminUsersGet",
    ),
    ("GET", "/_synapse/admin/v1/rooms", "synapseAdminRoomsList"),
    (
        "GET",
        "/_synapse/admin/v1/rooms/{room_id}",
        "synapseAdminRoomsGet",
    ),
    (
        "POST",
        "/_synapse/admin/v1/send_server_notice",
        "synapseAdminSendServerNotice",
    ),
    (
        "PUT",
        "/_synapse/admin/v1/send_server_notice/{txn_id}",
        "synapseAdminSendServerNoticeTxn",
    ),
    (
        "POST",
        "/_synapse/admin/v1/deactivate/{user_id}",
        "synapseAdminDeactivateUser",
    ),
    // Users.
    (
        "PUT",
        "/_synapse/admin/v2/users/{user_id}",
        "synapseAdminUsersPut",
    ),
    (
        "GET",
        "/_synapse/admin/v1/users/{user_id}/admin",
        "synapseAdminUserIsAdmin",
    ),
    (
        "PUT",
        "/_synapse/admin/v1/users/{user_id}/admin",
        "synapseAdminUserSetAdmin",
    ),
    (
        "GET",
        "/_synapse/admin/v2/users/{user_id}/devices",
        "synapseAdminDevicesList",
    ),
    (
        "POST",
        "/_synapse/admin/v2/users/{user_id}/delete_devices",
        "synapseAdminDevicesDeleteMany",
    ),
    (
        "GET",
        "/_synapse/admin/v2/users/{user_id}/devices/{device_id}",
        "synapseAdminDeviceGet",
    ),
    (
        "PUT",
        "/_synapse/admin/v2/users/{user_id}/devices/{device_id}",
        "synapseAdminDevicePut",
    ),
    (
        "DELETE",
        "/_synapse/admin/v2/users/{user_id}/devices/{device_id}",
        "synapseAdminDeviceDelete",
    ),
    (
        "GET",
        "/_synapse/admin/v1/users/{user_id}/joined_rooms",
        "synapseAdminUserJoinedRooms",
    ),
    (
        "GET",
        "/_synapse/admin/v1/users/{user_id}/pushers",
        "synapseAdminUserPushers",
    ),
    (
        "GET",
        "/_synapse/admin/v1/users/{user_id}/media",
        "synapseAdminUserMedia",
    ),
    (
        "GET",
        "/_synapse/admin/v1/users/{user_id}/accountdata",
        "synapseAdminUserAccountData",
    ),
    (
        "GET",
        "/_synapse/admin/v1/whois/{user_id}",
        "synapseAdminUserWhois",
    ),
    (
        "POST",
        "/_synapse/admin/v1/reset_password/{user_id}",
        "synapseAdminResetPassword",
    ),
    (
        "POST",
        "/_synapse/admin/v1/users/{user_id}/login",
        "synapseAdminUserLogin",
    ),
    (
        "PUT",
        "/_synapse/admin/v1/users/{user_id}/shadow_ban",
        "synapseAdminShadowBan",
    ),
    (
        "DELETE",
        "/_synapse/admin/v1/users/{user_id}/shadow_ban",
        "synapseAdminUnshadowBan",
    ),
    (
        "PUT",
        "/_synapse/admin/v1/suspend/{user_id}",
        "synapseAdminSuspend",
    ),
    (
        "GET",
        "/_synapse/admin/v1/username_available",
        "synapseAdminUsernameAvailable",
    ),
    (
        "GET",
        "/_synapse/admin/v1/experimental_features/{user_id}",
        "synapseAdminExperimentalFeaturesGet",
    ),
    (
        "PUT",
        "/_synapse/admin/v1/experimental_features/{user_id}",
        "synapseAdminExperimentalFeaturesPut",
    ),
    (
        "GET",
        "/_synapse/admin/v1/auth_providers/{provider}/users/{external_id}",
        "synapseAdminUserByExternalId",
    ),
    (
        "GET",
        "/_synapse/admin/v1/threepid/{medium}/users/{address}",
        "synapseAdminUserByThreepid",
    ),
    (
        "POST",
        "/_synapse/admin/v1/user/{user_id}/redact",
        "synapseAdminUserRedact",
    ),
    (
        "GET",
        "/_synapse/admin/v1/user/redact_status/{redact_id}",
        "synapseAdminUserRedactStatus",
    ),
    // Rooms.
    (
        "GET",
        "/_synapse/admin/v1/rooms/{room_id}/members",
        "synapseAdminRoomMembers",
    ),
    (
        "GET",
        "/_synapse/admin/v1/rooms/{room_id}/state",
        "synapseAdminRoomState",
    ),
    (
        "DELETE",
        "/_synapse/admin/v1/rooms/{room_id}",
        "synapseAdminRoomDeleteV1",
    ),
    (
        "DELETE",
        "/_synapse/admin/v2/rooms/{room_id}",
        "synapseAdminRoomDelete",
    ),
    (
        "GET",
        "/_synapse/admin/v2/rooms/{room_id}/delete_status",
        "synapseAdminRoomDeleteStatusByRoom",
    ),
    (
        "GET",
        "/_synapse/admin/v2/rooms/delete_status/{delete_id}",
        "synapseAdminRoomDeleteStatus",
    ),
    (
        "GET",
        "/_synapse/admin/v1/rooms/{room_id}/block",
        "synapseAdminRoomBlockGet",
    ),
    (
        "PUT",
        "/_synapse/admin/v1/rooms/{room_id}/block",
        "synapseAdminRoomBlockPut",
    ),
    (
        "POST",
        "/_synapse/admin/v1/rooms/{room_id}/make_room_admin",
        "synapseAdminRoomMakeAdmin",
    ),
    (
        "POST",
        "/_synapse/admin/v1/join/{room_id}",
        "synapseAdminRoomJoin",
    ),
    (
        "GET",
        "/_synapse/admin/v1/rooms/{room_id}/messages",
        "synapseAdminRoomMessages",
    ),
    (
        "GET",
        "/_synapse/admin/v1/rooms/{room_id}/context/{event_id}",
        "synapseAdminRoomContext",
    ),
    (
        "GET",
        "/_synapse/admin/v1/rooms/{room_id}/forward_extremities",
        "synapseAdminRoomForwardExtremities",
    ),
    (
        "DELETE",
        "/_synapse/admin/v1/rooms/{room_id}/forward_extremities",
        "synapseAdminRoomForwardExtremitiesDelete",
    ),
    (
        "GET",
        "/_synapse/admin/v1/room/{room_id}/media",
        "synapseAdminRoomMedia",
    ),
    // Registration tokens.
    (
        "GET",
        "/_synapse/admin/v1/registration_tokens",
        "synapseAdminRegistrationTokensList",
    ),
    (
        "POST",
        "/_synapse/admin/v1/registration_tokens/new",
        "synapseAdminRegistrationTokensNew",
    ),
    (
        "GET",
        "/_synapse/admin/v1/registration_tokens/{token}",
        "synapseAdminRegistrationTokenGet",
    ),
    (
        "PUT",
        "/_synapse/admin/v1/registration_tokens/{token}",
        "synapseAdminRegistrationTokenPut",
    ),
    (
        "DELETE",
        "/_synapse/admin/v1/registration_tokens/{token}",
        "synapseAdminRegistrationTokenDelete",
    ),
    // Reports.
    (
        "GET",
        "/_synapse/admin/v1/event_reports",
        "synapseAdminEventReportsList",
    ),
    (
        "GET",
        "/_synapse/admin/v1/event_reports/{report_id}",
        "synapseAdminEventReportGet",
    ),
    (
        "DELETE",
        "/_synapse/admin/v1/event_reports/{report_id}",
        "synapseAdminEventReportDelete",
    ),
    // Media.
    (
        "GET",
        "/_synapse/admin/v1/statistics/users/media",
        "synapseAdminStatisticsUsersMedia",
    ),
    (
        "DELETE",
        "/_synapse/admin/v1/media/{server_name}/{media_id}",
        "synapseAdminMediaDelete",
    ),
    (
        "POST",
        "/_synapse/admin/v1/media/quarantine/{server_name}/{media_id}",
        "synapseAdminMediaQuarantine",
    ),
    (
        "POST",
        "/_synapse/admin/v1/media/unquarantine/{server_name}/{media_id}",
        "synapseAdminMediaUnquarantine",
    ),
    (
        "POST",
        "/_synapse/admin/v1/media/protect/{media_id}",
        "synapseAdminMediaProtect",
    ),
    (
        "POST",
        "/_synapse/admin/v1/media/unprotect/{media_id}",
        "synapseAdminMediaUnprotect",
    ),
    // Federation.
    (
        "GET",
        "/_synapse/admin/v1/federation/destinations",
        "synapseAdminDestinationsList",
    ),
    (
        "GET",
        "/_synapse/admin/v1/federation/destinations/{destination}",
        "synapseAdminDestinationGet",
    ),
    (
        "GET",
        "/_synapse/admin/v1/federation/destinations/{destination}/rooms",
        "synapseAdminDestinationRooms",
    ),
    (
        "POST",
        "/_synapse/admin/v1/federation/destinations/{destination}/reset_connection",
        "synapseAdminDestinationReset",
    ),
];

/// Adds this module's routes to `router`.
pub(crate) fn add_routes(router: Router<AdminProxyState>) -> Router<AdminProxyState> {
    router
        // Users.
        .route("/_synapse/admin/v2/users/{user_id}", put(user_put))
        .route(
            "/_synapse/admin/v1/users/{user_id}/admin",
            get(user_is_admin).put(user_set_admin),
        )
        .route(
            "/_synapse/admin/v2/users/{user_id}/devices",
            get(devices_list),
        )
        .route(
            "/_synapse/admin/v2/users/{user_id}/delete_devices",
            post(devices_delete_many),
        )
        .route(
            "/_synapse/admin/v2/users/{user_id}/devices/{device_id}",
            get(device_get).put(device_put).delete(device_delete),
        )
        .route(
            "/_synapse/admin/v1/users/{user_id}/joined_rooms",
            get(user_joined_rooms),
        )
        .route(
            "/_synapse/admin/v1/users/{user_id}/pushers",
            get(user_pushers),
        )
        .route("/_synapse/admin/v1/users/{user_id}/media", get(user_media))
        .route(
            "/_synapse/admin/v1/users/{user_id}/accountdata",
            get(user_account_data),
        )
        .route("/_synapse/admin/v1/whois/{user_id}", get(user_whois))
        .route(
            "/_synapse/admin/v1/reset_password/{user_id}",
            post(reset_password),
        )
        .route("/_synapse/admin/v1/users/{user_id}/login", post(user_login))
        .route(
            "/_synapse/admin/v1/users/{user_id}/shadow_ban",
            put(shadow_ban).delete(unshadow_ban),
        )
        .route("/_synapse/admin/v1/suspend/{user_id}", put(suspend))
        .route(
            "/_synapse/admin/v1/username_available",
            get(username_available),
        )
        .route(
            "/_synapse/admin/v1/experimental_features/{user_id}",
            get(experimental_features_get).put(experimental_features_put),
        )
        .route(
            "/_synapse/admin/v1/auth_providers/{provider}/users/{external_id}",
            get(user_by_external_id),
        )
        .route(
            "/_synapse/admin/v1/threepid/{medium}/users/{address}",
            get(user_by_threepid),
        )
        .route(
            "/_synapse/admin/v1/user/{user_id}/redact",
            post(user_redact),
        )
        .route(
            "/_synapse/admin/v1/user/redact_status/{redact_id}",
            get(user_redact_status),
        )
        // Rooms.
        .route(
            "/_synapse/admin/v1/rooms/{room_id}/members",
            get(room_members),
        )
        .route("/_synapse/admin/v1/rooms/{room_id}/state", get(room_state))
        .route("/_synapse/admin/v1/rooms/{room_id}", delete(room_delete))
        .route("/_synapse/admin/v2/rooms/{room_id}", delete(room_delete))
        .route(
            "/_synapse/admin/v2/rooms/{room_id}/delete_status",
            get(room_delete_status_by_room),
        )
        .route(
            "/_synapse/admin/v2/rooms/delete_status/{delete_id}",
            get(room_delete_status),
        )
        .route(
            "/_synapse/admin/v1/rooms/{room_id}/block",
            get(room_block_get).put(room_block_put),
        )
        .route(
            "/_synapse/admin/v1/rooms/{room_id}/make_room_admin",
            post(room_make_admin),
        )
        .route("/_synapse/admin/v1/join/{room_id}", post(room_join))
        .route(
            "/_synapse/admin/v1/rooms/{room_id}/messages",
            get(room_messages),
        )
        .route(
            "/_synapse/admin/v1/rooms/{room_id}/context/{event_id}",
            get(room_context),
        )
        .route(
            "/_synapse/admin/v1/rooms/{room_id}/forward_extremities",
            get(room_forward_extremities).delete(room_forward_extremities_delete),
        )
        .route("/_synapse/admin/v1/room/{room_id}/media", get(room_media))
        // Registration tokens.
        .route(
            "/_synapse/admin/v1/registration_tokens",
            get(registration_tokens_list),
        )
        .route(
            "/_synapse/admin/v1/registration_tokens/new",
            post(registration_tokens_new),
        )
        .route(
            "/_synapse/admin/v1/registration_tokens/{token}",
            get(registration_token_get)
                .put(registration_token_put)
                .delete(registration_token_delete),
        )
        // Reports.
        .route("/_synapse/admin/v1/event_reports", get(event_reports_list))
        .route(
            "/_synapse/admin/v1/event_reports/{report_id}",
            get(event_report_get).delete(event_report_delete),
        )
        // Media.
        .route(
            "/_synapse/admin/v1/statistics/users/media",
            get(statistics_users_media),
        )
        .route(
            "/_synapse/admin/v1/media/{server_name}/{media_id}",
            delete(media_delete),
        )
        .route(
            "/_synapse/admin/v1/media/quarantine/{server_name}/{media_id}",
            post(media_quarantine),
        )
        .route(
            "/_synapse/admin/v1/media/unquarantine/{server_name}/{media_id}",
            post(media_unquarantine),
        )
        .route(
            "/_synapse/admin/v1/media/protect/{media_id}",
            post(media_protect),
        )
        .route(
            "/_synapse/admin/v1/media/unprotect/{media_id}",
            post(media_unprotect),
        )
        // Federation.
        .route(
            "/_synapse/admin/v1/federation/destinations",
            get(destinations_list),
        )
        .route(
            "/_synapse/admin/v1/federation/destinations/{destination}",
            get(destination_get),
        )
        .route(
            "/_synapse/admin/v1/federation/destinations/{destination}/rooms",
            get(destination_rooms),
        )
        .route(
            "/_synapse/admin/v1/federation/destinations/{destination}/reset_connection",
            post(destination_reset),
        )
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn ok(body: Value) -> Response {
    (StatusCode::OK, axum::Json(body)).into_response()
}

fn empty() -> Response {
    ok(json!({}))
}

fn body_json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_| json!({}))
}

fn ms(value: &Value) -> Value {
    value
        .as_str()
        .and_then(parse_rfc3339_ms)
        .map(Value::from)
        .unwrap_or(Value::Null)
}

/// Milliseconds since the epoch as the RFC 3339 timestamp the native API takes.
fn rfc3339(ms: i64) -> Option<String> {
    let seconds = ms.div_euclid(1000);
    let nanos = u32::try_from(ms.rem_euclid(1000) * 1_000_000).ok()?;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .replace_nanosecond(nanos)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

fn items(page: &Value) -> Vec<Value> {
    page.get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn total(page: &Value, items: usize) -> Value {
    page.get("total").cloned().unwrap_or(Value::from(items))
}

/// Every item of a native collection, page after page (the native API clamps a page to 500).
async fn all_items(
    state: &AdminProxyState,
    headers: &HeaderMap,
    path: &str,
) -> Result<Vec<Value>, Box<Response>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut page_path = format!("{path}{separator}limit=500");
        if let Some(c) = &cursor {
            page_path.push_str("&cursor=");
            page_path.push_str(&enc(c));
        }
        let (_, page) = forward(&state.native, Method::GET, &page_path, headers, None).await?;
        let got = items(&page);
        let n = got.len();
        out.extend(got);
        match page.get("next_cursor").and_then(Value::as_str) {
            Some(next) if n > 0 && out.len() < 50_000 => cursor = Some(next.to_owned()),
            _ => return Ok(out),
        }
    }
}

/// Synapse's status of a background job, from a native task's.
fn task_status(task: &Value) -> &'static str {
    match task.get("status").and_then(Value::as_str) {
        Some("scheduled") => "scheduled",
        Some("succeeded") => "complete",
        Some("failed" | "cancelled") => "failed",
        _ => "active",
    }
}

#[derive(Deserialize, Default)]
struct PageQuery {
    from: Option<String>,
    limit: Option<u64>,
}

fn paged_path(base: &str, q: &PageQuery, extra: &[(&str, Option<String>)]) -> String {
    let mut path = format!(
        "{base}?include_total=true&limit={}",
        q.limit.unwrap_or(100).min(500)
    );
    if let Some(from) = &q.from {
        path.push_str("&cursor=");
        path.push_str(&enc(from));
    }
    for (key, value) in extra {
        if let Some(value) = value {
            path.push('&');
            path.push_str(key);
            path.push('=');
            path.push_str(&enc(value));
        }
    }
    path
}

// ---------------------------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------------------------

/// `PUT /_synapse/admin/v2/users/{user_id}`: Synapse's create-or-modify. An account that is
/// not here is created with the native `users.create` (`201`); one that is gets each field
/// the body names applied through the native operation for it (`users.update` for the name,
/// avatar, administrator flag and user type; `reset-password`; `deactivate`/`reactivate`;
/// `lock`/`unlock`; the 3PID and external-identity sets, made equal to the body's by adding
/// and removing), then answers the record (`200`).
async fn user_put(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let native = &state.native;
    let path = format!("/api/v1/users/{}", enc(&user_id));
    let existing = match forward(native, Method::GET, &path, &headers, None).await {
        Ok((_, user)) => Some(user),
        Err(response) if response.status() == StatusCode::NOT_FOUND => None,
        Err(response) => return *response,
    };
    let field = |name: &str| body.get(name).cloned();
    let Some(existing) = existing else {
        let mut create = json!({ "user_id": user_id });
        if let Some(password) = field("password") {
            create["password"] = password;
        }
        if let Some(name) = field("displayname") {
            create["display_name"] = name;
        }
        if let Some(admin) = field("admin") {
            create["admin"] = admin;
        }
        if let Some(user_type) = field("user_type") {
            create["user_type"] = user_type;
        }
        if let Some(threepids) = field("threepids") {
            create["threepids"] = threepids;
        }
        if let Some(ids) = field("external_ids") {
            create["external_ids"] = Value::Array(
                ids.as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| {
                        json!({"provider": x.get("auth_provider").cloned().unwrap_or(Value::Null),
                               "external_id": x.get("external_id").cloned().unwrap_or(Value::Null)})
                    })
                    .collect(),
            );
        }
        let created = match forward(
            native,
            Method::POST,
            "/api/v1/users",
            &headers,
            Some(create),
        )
        .await
        {
            Ok((_, user)) => user,
            Err(response) => return *response,
        };
        // Fields `users.create` does not take are applied as to an existing account.
        for (key, value) in [
            ("avatar_url", field("avatar_url")),
            ("locked", field("locked")),
            ("deactivated", field("deactivated")),
        ] {
            if let Some(value) = value
                && let Err(response) =
                    apply_user_field(&state, &headers, &user_id, key, &value, &created).await
            {
                return *response;
            }
        }
        return match full_record(&state, &headers, &user_id).await {
            Ok(record) => (StatusCode::CREATED, axum::Json(record)).into_response(),
            Err(response) => *response,
        };
    };
    for key in [
        "displayname",
        "avatar_url",
        "admin",
        "user_type",
        "password",
        "deactivated",
        "locked",
        "threepids",
        "external_ids",
    ] {
        if let Some(value) = field(key)
            && let Err(response) =
                apply_user_field(&state, &headers, &user_id, key, &value, &existing).await
        {
            return *response;
        }
    }
    match full_record(&state, &headers, &user_id).await {
        Ok(record) => ok(record),
        Err(response) => *response,
    }
}

/// One field of Synapse's user body, applied through the native operation for it.
async fn apply_user_field(
    state: &AdminProxyState,
    headers: &HeaderMap,
    user_id: &str,
    key: &str,
    value: &Value,
    current: &Value,
) -> Result<(), Box<Response>> {
    let native = &state.native;
    let base = format!("/api/v1/users/{}", enc(user_id));
    let post = |suffix: &str, body: Value| {
        let path = format!("{base}/{suffix}");
        async move { forward(native, Method::POST, &path, headers, Some(body)).await }
    };
    match key {
        "displayname" | "avatar_url" | "admin" | "user_type" => {
            let native_key = match key {
                "displayname" => "display_name",
                other => other,
            };
            let patch = json!({ native_key: value });
            forward(native, Method::PATCH, &base, headers, Some(patch)).await?;
        }
        "password" => {
            let logout = current
                .get("__logout_devices")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            post(
                "reset-password",
                json!({"password": value, "logout_devices": logout}),
            )
            .await?;
        }
        "deactivated" => {
            let now = current
                .get("deactivated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match value.as_bool() {
                Some(true) if !now => {
                    post("deactivate", json!({"erase": false})).await?;
                }
                Some(false) if now => {
                    post("reactivate", json!({})).await?;
                }
                _ => {}
            }
        }
        "locked" => {
            let now = current
                .get("locked")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match value.as_bool() {
                Some(true) if !now => {
                    post("lock", json!({})).await?;
                }
                Some(false) if now => {
                    post("unlock", json!({})).await?;
                }
                _ => {}
            }
        }
        "threepids" => {
            let wanted: Vec<(String, String)> = value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    Some((
                        t.get("medium")?.as_str()?.to_owned(),
                        t.get("address")?.as_str()?.to_owned(),
                    ))
                })
                .collect();
            let path = format!("{base}/threepids");
            let (_, have) = forward(native, Method::GET, &path, headers, None).await?;
            let have: Vec<(String, String)> = have
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    Some((
                        t.get("medium")?.as_str()?.to_owned(),
                        t.get("address")?.as_str()?.to_owned(),
                    ))
                })
                .collect();
            for (medium, address) in &have {
                if !wanted.iter().any(|(m, a)| m == medium && a == address) {
                    let remove = format!("{path}/{}/{}", enc(medium), enc(address));
                    forward(native, Method::DELETE, &remove, headers, None).await?;
                }
            }
            for (medium, address) in &wanted {
                if !have.iter().any(|(m, a)| m == medium && a == address) {
                    forward(
                        native,
                        Method::POST,
                        &path,
                        headers,
                        Some(json!({"medium": medium, "address": address})),
                    )
                    .await?;
                }
            }
        }
        "external_ids" => {
            let wanted: Vec<(String, String)> = value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| {
                    Some((
                        x.get("auth_provider")?.as_str()?.to_owned(),
                        x.get("external_id")?.as_str()?.to_owned(),
                    ))
                })
                .collect();
            let path = format!("{base}/external-ids");
            let (_, have) = forward(native, Method::GET, &path, headers, None).await?;
            let have: Vec<(String, String)> = have
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| {
                    Some((
                        x.get("provider")?.as_str()?.to_owned(),
                        x.get("external_id")?.as_str()?.to_owned(),
                    ))
                })
                .collect();
            for (provider, id) in &have {
                if !wanted.iter().any(|(p, i)| p == provider && i == id) {
                    let remove = format!("{path}/{}/{}", enc(provider), enc(id));
                    forward(native, Method::DELETE, &remove, headers, None).await?;
                }
            }
            for (provider, id) in &wanted {
                if !have.iter().any(|(p, i)| p == provider && i == id) {
                    forward(
                        native,
                        Method::POST,
                        &path,
                        headers,
                        Some(json!({"provider": provider, "external_id": id})),
                    )
                    .await?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn user_is_admin(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}", enc(&user_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, user)) => {
            ok(json!({"admin": user.get("admin").cloned().unwrap_or(Value::Bool(false))}))
        }
        Err(response) => *response,
    }
}

async fn user_set_admin(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let admin = body_json(&bytes)
        .get("admin")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = format!("/api/v1/users/{}", enc(&user_id));
    match forward(
        &state.native,
        Method::PATCH,
        &path,
        &headers,
        Some(json!({"admin": admin})),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

fn synapse_device(user_id: &str, d: &Value) -> Value {
    json!({
        "device_id": d.get("device_id").cloned().unwrap_or(Value::Null),
        "display_name": d.get("display_name").cloned().unwrap_or(Value::Null),
        "last_seen_ip": d.get("last_seen_ip").cloned().unwrap_or(Value::Null),
        "last_seen_ts": ms(d.get("last_seen_at").unwrap_or(&Value::Null)),
        "last_seen_user_agent": Value::Null,
        "user_id": user_id,
    })
}

async fn devices_list(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}/devices", enc(&user_id));
    match all_items(&state, &headers, &path).await {
        Ok(devices) => {
            let devices: Vec<Value> = devices
                .iter()
                .map(|d| synapse_device(&user_id, d))
                .collect();
            ok(json!({"total": devices.len(), "devices": devices}))
        }
        Err(response) => *response,
    }
}

async fn devices_delete_many(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let devices = body_json(&bytes)
        .get("devices")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let path = format!("/api/v1/users/{}/devices/bulk-delete", enc(&user_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(json!({"device_ids": devices})),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

async fn device_get(
    State(state): State<AdminProxyState>,
    Path((user_id, device_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!(
        "/api/v1/users/{}/devices/{}",
        enc(&user_id),
        enc(&device_id)
    );
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, d)) => ok(synapse_device(&user_id, &d)),
        Err(response) => *response,
    }
}

async fn device_put(
    State(state): State<AdminProxyState>,
    Path((user_id, device_id)): Path<(String, String)>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let display_name = body_json(&bytes)
        .get("display_name")
        .cloned()
        .unwrap_or(Value::Null);
    let path = format!(
        "/api/v1/users/{}/devices/{}",
        enc(&user_id),
        enc(&device_id)
    );
    match forward(
        &state.native,
        Method::PATCH,
        &path,
        &headers,
        Some(json!({"display_name": display_name})),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

async fn device_delete(
    State(state): State<AdminProxyState>,
    Path((user_id, device_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!(
        "/api/v1/users/{}/devices/{}",
        enc(&user_id),
        enc(&device_id)
    );
    match forward(&state.native, Method::DELETE, &path, &headers, None).await {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

async fn user_joined_rooms(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!(
        "/api/v1/users/{}/memberships?membership=join",
        enc(&user_id)
    );
    match all_items(&state, &headers, &path).await {
        Ok(memberships) => {
            let rooms: Vec<Value> = memberships
                .iter()
                .filter(|m| m.get("membership").and_then(Value::as_str) != Some("leave"))
                .filter_map(|m| m.get("room_id").cloned())
                .collect();
            ok(json!({"total": rooms.len(), "joined_rooms": rooms}))
        }
        Err(response) => *response,
    }
}

async fn user_pushers(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}/pushers", enc(&user_id));
    match all_items(&state, &headers, &path).await {
        Ok(pushers) => ok(json!({"total": pushers.len(), "pushers": pushers})),
        Err(response) => *response,
    }
}

fn synapse_media_item(m: &Value) -> Value {
    json!({
        "media_id": m.get("media_id").cloned().unwrap_or(Value::Null),
        "media_type": m.get("content_type").cloned().unwrap_or(Value::Null),
        "media_length": m.get("size_bytes").cloned().unwrap_or(Value::Null),
        "upload_name": m.get("upload_name").cloned().unwrap_or(Value::Null),
        "created_ts": ms(m.get("created_at").unwrap_or(&Value::Null)),
        "last_access_ts": ms(m.get("last_accessed_at").unwrap_or(&Value::Null)),
        // Who quarantined it is not kept natively; Synapse's clients test the field for null.
        "quarantined_by": if m.get("quarantined").and_then(Value::as_bool).unwrap_or(false) {
            Value::String("admin".to_owned())
        } else {
            Value::Null
        },
        "safe_from_quarantine": m.get("protected").cloned().unwrap_or(Value::Bool(false)),
    })
}

async fn user_media(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    let path = paged_path(&format!("/api/v1/users/{}/media", enc(&user_id)), &q, &[]);
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, page)) => {
            let media: Vec<Value> = items(&page).iter().map(synapse_media_item).collect();
            let mut body = json!({"media": media, "total": total(&page, media.len())});
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["next_token"] = Value::String(next.to_owned());
            }
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn user_account_data(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}/account-data", enc(&user_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        // The native listing is the global account data by type; per-room account data has no
        // native listing yet, so `rooms` is empty.
        Ok((_, data)) => ok(json!({"account_data": {"global": data, "rooms": {}}})),
        Err(response) => *response,
    }
}

async fn user_whois(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}/sessions", enc(&user_id));
    match all_items(&state, &headers, &path).await {
        Ok(sessions) => {
            let mut devices = Map::new();
            for s in &sessions {
                let device = s
                    .get("device_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let connection = json!({
                    "ip": s.get("ip").cloned().unwrap_or(Value::Null),
                    "last_seen": ms(s.get("last_seen_at").unwrap_or(&Value::Null)),
                    "user_agent": s.get("user_agent").cloned().unwrap_or(Value::Null),
                });
                let entry = devices
                    .entry(device)
                    .or_insert_with(|| json!({"sessions": [{"connections": []}]}));
                if let Some(connections) = entry["sessions"][0]["connections"].as_array_mut() {
                    connections.push(connection);
                }
            }
            ok(json!({"user_id": user_id, "devices": devices}))
        }
        Err(response) => *response,
    }
}

async fn reset_password(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let path = format!("/api/v1/users/{}/reset-password", enc(&user_id));
    let native_body = json!({
        "password": body.get("new_password").cloned().unwrap_or(Value::Null),
        "logout_devices": body.get("logout_devices").and_then(Value::as_bool).unwrap_or(true),
    });
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(native_body),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

/// `POST /_synapse/admin/v1/users/{user_id}/login` (act as the user): the native `login-as`,
/// with `valid_until_ms` turned into seconds from now.
async fn user_login(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let mut native_body = json!({"reason": "Synapse admin API: login as user"});
    if let Some(until) = body.get("valid_until_ms").and_then(Value::as_i64) {
        let now = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let seconds = (until - now).max(1000) / 1000;
        native_body["valid_for_seconds"] = json!(seconds);
    }
    let path = format!("/api/v1/users/{}/login-as", enc(&user_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(native_body),
    )
    .await
    {
        Ok((_, session)) => ok(json!({
            "access_token": session.get("access_token").cloned().unwrap_or(Value::Null),
        })),
        Err(response) => *response,
    }
}

async fn shadow_ban(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}/shadow-ban", enc(&user_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(json!({})),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) if response.status() == StatusCode::CONFLICT => empty(),
        Err(response) => *response,
    }
}

async fn unshadow_ban(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}/unshadow-ban", enc(&user_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(json!({})),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) if response.status() == StatusCode::CONFLICT => empty(),
        Err(response) => *response,
    }
}

async fn suspend(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let suspend = body_json(&bytes)
        .get("suspend")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let path = format!(
        "/api/v1/users/{}/{}",
        enc(&user_id),
        if suspend { "suspend" } else { "unsuspend" }
    );
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(json!({})),
    )
    .await
    {
        Ok(_) => ok(json!({"user_id": user_id, "suspended": suspend})),
        Err(response) if response.status() == StatusCode::CONFLICT => {
            ok(json!({"user_id": user_id, "suspended": suspend}))
        }
        Err(response) => *response,
    }
}

#[derive(Deserialize)]
struct UsernameQuery {
    username: Option<String>,
}

/// `GET /_synapse/admin/v1/username_available?username=`: `{"available": true}`, or Synapse's
/// `400 M_USER_IN_USE` when it is taken.
async fn username_available(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Query(q): Query<UsernameQuery>,
) -> Response {
    let Some(username) = q.username else {
        return translate_error_body(
            StatusCode::BAD_REQUEST,
            b"{\"detail\":\"username is required\"}",
        );
    };
    let path = format!("/api/v1/users/availability?localpart={}", enc(&username));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, answer)) if answer.get("available").and_then(Value::as_bool) == Some(true) => {
            ok(json!({"available": true}))
        }
        Ok(_) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"errcode": "M_USER_IN_USE", "error": "User ID already taken."})),
        )
            .into_response(),
        Err(response) => *response,
    }
}

async fn experimental_features_get(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/users/{}/experimental-features", enc(&user_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, features)) => ok(json!({
            "features": features.get("features").cloned().unwrap_or_else(|| json!({}))
        })),
        Err(response) => *response,
    }
}

async fn experimental_features_put(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let path = format!("/api/v1/users/{}/experimental-features", enc(&user_id));
    match forward(&state.native, Method::PUT, &path, &headers, Some(body)).await {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

async fn user_by_external_id(
    State(state): State<AdminProxyState>,
    Path((provider, external_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!(
        "/api/v1/users/lookup?provider={}&external_id={}",
        enc(&provider),
        enc(&external_id)
    );
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, user)) => {
            ok(json!({"user_id": user.get("user_id").cloned().unwrap_or(Value::Null)}))
        }
        Err(response) => *response,
    }
}

async fn user_by_threepid(
    State(state): State<AdminProxyState>,
    Path((medium, address)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!(
        "/api/v1/users/lookup?medium={}&address={}",
        enc(&medium),
        enc(&address)
    );
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, user)) => {
            ok(json!({"user_id": user.get("user_id").cloned().unwrap_or(Value::Null)}))
        }
        Err(response) => *response,
    }
}

async fn user_redact(
    State(state): State<AdminProxyState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    // Synapse takes a list of rooms (empty for all); the native takes one room or none.
    let mut native_body = json!({});
    if let Some(reason) = body.get("reason") {
        native_body["reason"] = reason.clone();
    }
    if let Some(limit) = body.get("limit") {
        native_body["limit"] = limit.clone();
    }
    if let Some(room) = body
        .get("rooms")
        .and_then(Value::as_array)
        .and_then(|rooms| rooms.first())
    {
        native_body["room_id"] = room.clone();
    }
    let path = format!("/api/v1/users/{}/redact-events", enc(&user_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(native_body),
    )
    .await
    {
        Ok((_, task)) => ok(json!({"redact_id": task.get("id").cloned().unwrap_or(Value::Null)})),
        Err(response) => *response,
    }
}

async fn user_redact_status(
    State(state): State<AdminProxyState>,
    Path(redact_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/tasks/{}", enc(&redact_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, task)) => ok(json!({
            "status": task_status(&task),
            "failed_redactions": {},
        })),
        Err(response) => *response,
    }
}

// ---------------------------------------------------------------------------------------------
// Rooms
// ---------------------------------------------------------------------------------------------

async fn room_members(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/rooms/{}/members", enc(&room_id));
    match all_items(&state, &headers, &path).await {
        Ok(members) => {
            let ids: Vec<Value> = members
                .iter()
                .filter(|m| m.get("membership").and_then(Value::as_str) == Some("join"))
                .filter_map(|m| m.get("user_id").cloned())
                .collect();
            ok(json!({"total": ids.len(), "members": ids}))
        }
        Err(response) => *response,
    }
}

fn client_event(room_id: &str, e: &Value) -> Value {
    let mut event = json!({
        "event_id": e.get("event_id").cloned().unwrap_or(Value::Null),
        "room_id": e.get("room_id").cloned().unwrap_or_else(|| Value::String(room_id.to_owned())),
        "type": e.get("type").cloned().unwrap_or(Value::Null),
        "sender": e.get("sender").cloned().unwrap_or(Value::Null),
        "content": e.get("content").cloned().unwrap_or_else(|| json!({})),
        "origin_server_ts": e.get("origin_server_ts").cloned().unwrap_or(Value::Null),
        "unsigned": {},
    });
    if let Some(state_key) = e.get("state_key").filter(|s| !s.is_null()) {
        event["state_key"] = state_key.clone();
    }
    if let Some(redacts) = e.get("redacts").filter(|s| !s.is_null()) {
        event["redacts"] = redacts.clone();
    }
    event
}

async fn room_state(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/rooms/{}/state", enc(&room_id));
    match all_items(&state, &headers, &path).await {
        Ok(events) => {
            let events: Vec<Value> = events.iter().map(|e| client_event(&room_id, e)).collect();
            ok(json!({"state": events}))
        }
        Err(response) => *response,
    }
}

/// `DELETE /_synapse/admin/v1|v2/rooms/{room_id}`: the native `rooms.delete`, a task. Both
/// versions answer the v2 shape, `{"delete_id"}` (the task's id, for `delete_status`): the
/// deletion runs in the background here, so v1's synchronous list of kicked users cannot be
/// answered.
async fn room_delete(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let mut native_body = json!({
        "block": body.get("block").and_then(Value::as_bool).unwrap_or(false),
        "purge": body.get("purge").and_then(Value::as_bool).unwrap_or(true),
    });
    if let Some(message) = body.get("message") {
        native_body["message"] = message.clone();
    }
    if let Some(creator) = body.get("new_room_user_id").filter(|v| !v.is_null()) {
        native_body["new_room"] = json!({
            "creator": creator,
            "name": body.get("room_name").cloned().unwrap_or_else(|| json!("Content Violation Notification")),
        });
    }
    let path = format!("/api/v1/rooms/{}/delete", enc(&room_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(native_body),
    )
    .await
    {
        Ok((_, task)) => ok(json!({"delete_id": task.get("id").cloned().unwrap_or(Value::Null)})),
        Err(response) => *response,
    }
}

fn delete_status_entry(task: &Value) -> Value {
    let result = task.get("result").cloned().unwrap_or_else(|| json!({}));
    json!({
        "delete_id": task.get("id").cloned().unwrap_or(Value::Null),
        "status": task_status(task),
        "error": task.get("error").cloned().unwrap_or(Value::Null),
        "shutdown_room": {
            "kicked_users": result.get("kicked_users").cloned().unwrap_or_else(|| json!([])),
            "failed_to_kick_users": result.get("failed_to_kick_users").cloned().unwrap_or_else(|| json!([])),
            "local_aliases": result.get("local_aliases").cloned().unwrap_or_else(|| json!([])),
            "new_room_id": result.get("new_room_id").cloned().unwrap_or(Value::Null),
        },
    })
}

async fn room_delete_status(
    State(state): State<AdminProxyState>,
    Path(delete_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/tasks/{}", enc(&delete_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, task)) => {
            let mut entry = delete_status_entry(&task);
            if let Some(object) = entry.as_object_mut() {
                object.remove("delete_id");
            }
            ok(entry)
        }
        Err(response) => *response,
    }
}

async fn room_delete_status_by_room(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match all_items(&state, &headers, "/api/v1/tasks").await {
        Ok(tasks) => {
            let results: Vec<Value> = tasks
                .iter()
                .filter(|t| t.get("action").and_then(Value::as_str) == Some("rooms.delete"))
                .filter(|t| {
                    t.get("resource")
                        .map(|r| r.to_string())
                        .is_some_and(|r| r.contains(&room_id))
                })
                .map(delete_status_entry)
                .collect();
            if results.is_empty() {
                return translate_error_body(
                    StatusCode::NOT_FOUND,
                    b"{\"detail\":\"No delete task for this room\"}",
                );
            }
            ok(json!({"results": results}))
        }
        Err(response) => *response,
    }
}

async fn room_block_get(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/rooms/{}", enc(&room_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, room)) => {
            let blocked = room
                .get("blocked")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut body = json!({"block": blocked});
            if blocked {
                body["user_id"] = Value::Null;
            }
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn room_block_put(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let block = body_json(&bytes)
        .get("block")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let path = format!(
        "/api/v1/rooms/{}/{}",
        enc(&room_id),
        if block { "block" } else { "unblock" }
    );
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(json!({})),
    )
    .await
    {
        Ok(_) => ok(json!({"block": block})),
        Err(response) if response.status() == StatusCode::CONFLICT => ok(json!({"block": block})),
        Err(response) => *response,
    }
}

async fn room_make_admin(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let mut native_body = json!({});
    if let Some(user_id) = body.get("user_id") {
        native_body["user_id"] = user_id.clone();
    }
    let path = format!("/api/v1/rooms/{}/make-admin", enc(&room_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(native_body),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

async fn room_join(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let path = format!("/api/v1/rooms/{}/join", enc(&room_id));
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(json!({"user_id": body.get("user_id").cloned().unwrap_or(Value::Null)})),
    )
    .await
    {
        Ok((_, member)) => ok(json!({
            "room_id": member.get("room_id").cloned().unwrap_or_else(|| Value::String(room_id))
        })),
        Err(response) => *response,
    }
}

#[derive(Deserialize, Default)]
struct MessagesQuery {
    from: Option<String>,
    limit: Option<u64>,
    dir: Option<String>,
}

async fn room_messages(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<MessagesQuery>,
) -> Response {
    let page = PageQuery {
        from: q.from.clone(),
        limit: q.limit,
    };
    let path = paged_path(
        &format!("/api/v1/rooms/{}/messages", enc(&room_id)),
        &page,
        &[("dir", Some(q.dir.clone().unwrap_or_else(|| "b".to_owned())))],
    );
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, page)) => {
            let chunk: Vec<Value> = items(&page)
                .iter()
                .map(|e| client_event(&room_id, e))
                .collect();
            let mut body = json!({"chunk": chunk, "start": q.from.unwrap_or_default()});
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["end"] = Value::String(next.to_owned());
            }
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn room_context(
    State(state): State<AdminProxyState>,
    Path((room_id, event_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    let path = format!(
        "/api/v1/rooms/{}/events/{}/context?limit={}",
        enc(&room_id),
        enc(&event_id),
        q.limit.unwrap_or(10)
    );
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, context)) => {
            let list = |key: &str| -> Vec<Value> {
                context
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|e| client_event(&room_id, e))
                    .collect()
            };
            ok(json!({
                "event": context.get("event").map(|e| client_event(&room_id, e)).unwrap_or(Value::Null),
                "events_before": list("events_before"),
                "events_after": list("events_after"),
                "state": list("state"),
                "start": "",
                "end": "",
            }))
        }
        Err(response) => *response,
    }
}

async fn room_forward_extremities(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/rooms/{}/forward-extremities", enc(&room_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, answer)) => {
            let list = answer
                .as_array()
                .cloned()
                .or_else(|| answer.get("items").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            let results: Vec<Value> = list
                .iter()
                .map(|e| {
                    json!({
                        "event_id": e.get("event_id").cloned().unwrap_or(Value::Null),
                        "state_group": Value::Null,
                        "depth": e.get("depth").cloned().unwrap_or(Value::Null),
                        "received_ts": e.get("origin_server_ts").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            ok(json!({"count": results.len(), "results": results}))
        }
        Err(response) => *response,
    }
}

async fn room_forward_extremities_delete(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/rooms/{}/forward-extremities", enc(&room_id));
    match forward(&state.native, Method::DELETE, &path, &headers, None).await {
        Ok((_, answer)) => {
            ok(json!({"deleted": answer.get("deleted").cloned().unwrap_or(json!(0))}))
        }
        Err(response) => *response,
    }
}

async fn room_media(
    State(state): State<AdminProxyState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/rooms/{}/media", enc(&room_id));
    match all_items(&state, &headers, &path).await {
        Ok(media) => {
            let mut local = Vec::new();
            let mut remote = Vec::new();
            for m in &media {
                let server = m.get("server_name").and_then(Value::as_str).unwrap_or("");
                let id = m.get("media_id").and_then(Value::as_str).unwrap_or("");
                let mxc = Value::String(format!("mxc://{server}/{id}"));
                if m.get("origin").is_some_and(|o| !o.is_null()) {
                    remote.push(mxc);
                } else {
                    local.push(mxc);
                }
            }
            ok(json!({"local": local, "remote": remote}))
        }
        Err(response) => *response,
    }
}

// ---------------------------------------------------------------------------------------------
// Registration tokens
// ---------------------------------------------------------------------------------------------

fn synapse_registration_token(t: &Value) -> Value {
    json!({
        "token": t.get("token").cloned().unwrap_or(Value::Null),
        "uses_allowed": t.get("uses_allowed").cloned().unwrap_or(Value::Null),
        "pending": t.get("pending").cloned().unwrap_or(json!(0)),
        "completed": t.get("completed").cloned().unwrap_or(json!(0)),
        "expiry_time": ms(t.get("expires_at").unwrap_or(&Value::Null)),
    })
}

#[derive(Deserialize, Default)]
struct ValidQuery {
    valid: Option<bool>,
}

async fn registration_tokens_list(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Query(q): Query<ValidQuery>,
) -> Response {
    match all_items(&state, &headers, "/api/v1/registration-tokens").await {
        Ok(tokens) => {
            let tokens: Vec<Value> = tokens
                .iter()
                .filter(|t| match q.valid {
                    Some(valid) => t.get("valid").and_then(Value::as_bool) == Some(valid),
                    None => true,
                })
                .map(synapse_registration_token)
                .collect();
            ok(json!({"registration_tokens": tokens}))
        }
        Err(response) => *response,
    }
}

async fn registration_tokens_new(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let mut native_body = json!({});
    for key in ["token", "uses_allowed", "length"] {
        if let Some(value) = body.get(key) {
            native_body[key] = value.clone();
        }
    }
    if let Some(expiry) = body.get("expiry_time").and_then(Value::as_i64) {
        native_body["expires_at"] = rfc3339(expiry).map(Value::String).unwrap_or(Value::Null);
    }
    match forward(
        &state.native,
        Method::POST,
        "/api/v1/registration-tokens",
        &headers,
        Some(native_body),
    )
    .await
    {
        Ok((_, token)) => ok(synapse_registration_token(&token)),
        Err(response) => *response,
    }
}

async fn registration_token_get(
    State(state): State<AdminProxyState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/registration-tokens/{}", enc(&token));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, token)) => ok(synapse_registration_token(&token)),
        Err(response) => *response,
    }
}

async fn registration_token_put(
    State(state): State<AdminProxyState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let body = body_json(&bytes);
    let mut native_body = json!({});
    if let Some(uses) = body.get("uses_allowed") {
        native_body["uses_allowed"] = uses.clone();
    }
    if let Some(expiry) = body.get("expiry_time") {
        native_body["expires_at"] = expiry
            .as_i64()
            .and_then(rfc3339)
            .map(Value::String)
            .unwrap_or(Value::Null);
    }
    let path = format!("/api/v1/registration-tokens/{}", enc(&token));
    match forward(
        &state.native,
        Method::PATCH,
        &path,
        &headers,
        Some(native_body),
    )
    .await
    {
        Ok((_, token)) => ok(synapse_registration_token(&token)),
        Err(response) => *response,
    }
}

async fn registration_token_delete(
    State(state): State<AdminProxyState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/registration-tokens/{}", enc(&token));
    match forward(&state.native, Method::DELETE, &path, &headers, None).await {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

// ---------------------------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------------------------

fn synapse_report(r: &Value) -> Value {
    let event = r.get("event").cloned().unwrap_or(Value::Null);
    json!({
        "id": r.get("id").cloned().unwrap_or(Value::Null),
        "received_ts": ms(r.get("received_at").unwrap_or(&Value::Null)),
        "room_id": r.get("room_id").cloned().unwrap_or(Value::Null),
        "event_id": r.get("event_id").cloned().unwrap_or(Value::Null),
        "user_id": r.get("reporter_id").cloned().unwrap_or(Value::Null),
        "reason": r.get("reason").cloned().unwrap_or(Value::Null),
        "score": r.get("score").cloned().unwrap_or(Value::Null),
        "sender": event.get("sender").cloned().unwrap_or(Value::Null),
        "canonical_alias": Value::Null,
        "name": Value::Null,
    })
}

async fn event_reports_list(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    let path = paged_path("/api/v1/reports", &q, &[]);
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, page)) => {
            let reports: Vec<Value> = items(&page).iter().map(synapse_report).collect();
            let mut body = json!({"event_reports": reports, "total": total(&page, reports.len())});
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["next_token"] = Value::String(next.to_owned());
            }
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn event_report_get(
    State(state): State<AdminProxyState>,
    Path(report_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/reports/{}", enc(&report_id));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, report)) => {
            let mut body = synapse_report(&report);
            body["event_json"] = report.get("event").cloned().unwrap_or(Value::Null);
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn event_report_delete(
    State(state): State<AdminProxyState>,
    Path(report_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/reports/{}", enc(&report_id));
    match forward(&state.native, Method::DELETE, &path, &headers, None).await {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

// ---------------------------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------------------------

async fn statistics_users_media(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    let path = paged_path("/api/v1/statistics/users/media", &q, &[]);
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, page)) => {
            let users: Vec<Value> = items(&page)
                .iter()
                .map(|u| {
                    json!({
                        "user_id": u.get("user_id").cloned().unwrap_or(Value::Null),
                        "displayname": u.get("display_name").cloned().unwrap_or(Value::Null),
                        "media_count": u.get("media_count").cloned().unwrap_or(json!(0)),
                        "media_length": u.get("media_bytes").cloned().unwrap_or(json!(0)),
                    })
                })
                .collect();
            let mut body = json!({"users": users, "total": total(&page, users.len())});
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["next_token"] = Value::String(next.to_owned());
            }
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn media_delete(
    State(state): State<AdminProxyState>,
    Path((server_name, media_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/media/{}/{}", enc(&server_name), enc(&media_id));
    match forward(&state.native, Method::DELETE, &path, &headers, None).await {
        Ok(_) => ok(json!({"deleted_media": [media_id], "total": 1})),
        Err(response) => *response,
    }
}

async fn media_action(
    state: &AdminProxyState,
    headers: &HeaderMap,
    server_name: &str,
    media_id: &str,
    action: &str,
) -> Response {
    let path = format!(
        "/api/v1/media/{}/{}/{action}",
        enc(server_name),
        enc(media_id)
    );
    match forward(&state.native, Method::POST, &path, headers, Some(json!({}))).await {
        Ok(_) => empty(),
        Err(response) if response.status() == StatusCode::CONFLICT => empty(),
        Err(response) => *response,
    }
}

async fn media_quarantine(
    State(state): State<AdminProxyState>,
    Path((server_name, media_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    media_action(&state, &headers, &server_name, &media_id, "quarantine").await
}

async fn media_unquarantine(
    State(state): State<AdminProxyState>,
    Path((server_name, media_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    media_action(&state, &headers, &server_name, &media_id, "unquarantine").await
}

/// `POST /_synapse/admin/v1/media/protect/{media_id}`: Synapse names no server (only local
/// media can be protected), so the native route is asked for this server's own media. The
/// server name is read from `/api/v1/server`.
async fn media_protect(
    State(state): State<AdminProxyState>,
    Path(media_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match own_server_name(&state, &headers).await {
        Ok(server_name) => media_action(&state, &headers, &server_name, &media_id, "protect").await,
        Err(response) => *response,
    }
}

async fn media_unprotect(
    State(state): State<AdminProxyState>,
    Path(media_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match own_server_name(&state, &headers).await {
        Ok(server_name) => {
            media_action(&state, &headers, &server_name, &media_id, "unprotect").await
        }
        Err(response) => *response,
    }
}

async fn own_server_name(
    state: &AdminProxyState,
    headers: &HeaderMap,
) -> Result<String, Box<Response>> {
    let (_, info) = forward(&state.native, Method::GET, "/api/v1/server", headers, None).await?;
    Ok(info
        .get("server_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

// ---------------------------------------------------------------------------------------------
// Federation
// ---------------------------------------------------------------------------------------------

fn synapse_destination(d: &Value) -> Value {
    json!({
        "destination": d.get("server_name").cloned().unwrap_or(Value::Null),
        "retry_last_ts": d.get("retry_last_at").map(ms).unwrap_or(json!(0)),
        "retry_interval": d.get("retry_interval_ms").cloned().unwrap_or(json!(0)),
        "failure_ts": d.get("failing_since").map(ms).unwrap_or(Value::Null),
        "last_successful_stream_ordering": Value::Null,
    })
}

async fn destinations_list(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    let path = paged_path("/api/v1/federation/destinations", &q, &[]);
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, page)) => {
            let destinations: Vec<Value> = items(&page).iter().map(synapse_destination).collect();
            let mut body = json!({
                "destinations": destinations,
                "total": total(&page, destinations.len()),
            });
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["next_token"] = Value::String(next.to_owned());
            }
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn destination_get(
    State(state): State<AdminProxyState>,
    Path(destination): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/v1/federation/destinations/{}", enc(&destination));
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, d)) => ok(synapse_destination(&d)),
        Err(response) => *response,
    }
}

async fn destination_rooms(
    State(state): State<AdminProxyState>,
    Path(destination): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    let path = paged_path(
        &format!(
            "/api/v1/federation/destinations/{}/rooms",
            enc(&destination)
        ),
        &q,
        &[],
    );
    match forward(&state.native, Method::GET, &path, &headers, None).await {
        Ok((_, page)) => {
            let rooms: Vec<Value> = items(&page)
                .iter()
                .map(|r| {
                    json!({
                        "room_id": r.get("room_id").cloned().unwrap_or(Value::Null),
                        "stream_ordering": Value::Null,
                    })
                })
                .collect();
            let mut body = json!({"rooms": rooms, "total": total(&page, rooms.len())});
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["next_token"] = Value::String(next.to_owned());
            }
            ok(body)
        }
        Err(response) => *response,
    }
}

async fn destination_reset(
    State(state): State<AdminProxyState>,
    Path(destination): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!(
        "/api/v1/federation/destinations/{}/reset",
        enc(&destination)
    );
    match forward(
        &state.native,
        Method::POST,
        &path,
        &headers,
        Some(json!({})),
    )
    .await
    {
        Ok(_) => empty(),
        Err(response) => *response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use axum::routing::{get as axum_get, post as axum_post};
    use tower::ServiceExt;

    /// A stand-in for the native router with just the shapes these shims read.
    fn fake_native() -> Router {
        Router::new()
            .route(
                "/api/v1/server",
                axum_get(|| async {
                    axum::Json(json!({"server_name": "example.org", "version": "0"}))
                }),
            )
            .route(
                "/api/v1/users/{user_id}",
                axum_get(|Path(user_id): Path<String>| async move {
                    if user_id == "@nobody:example.org" {
                        return (
                            StatusCode::NOT_FOUND,
                            axum::Json(json!({"detail": "no such user"})),
                        )
                            .into_response();
                    }
                    axum::Json(json!({
                        "user_id": user_id, "admin": true, "deactivated": false, "locked": false,
                        "display_name": "Alice", "created_at": "2026-10-09T10:00:00Z",
                    }))
                    .into_response()
                })
                .patch(
                    |Path(user_id): Path<String>, axum::Json(body): axum::Json<Value>| async move {
                        axum::Json(json!({"user_id": user_id, "patched": body}))
                    },
                ),
            )
            .route(
                "/api/v1/users/{user_id}/devices",
                axum_get(|| async {
                    axum::Json(
                        json!({"items": [{"device_id": "D1", "display_name": "phone",
                        "last_seen_ip": "10.0.0.1", "last_seen_at": "2026-10-09T10:00:00Z"}],
                        "next_cursor": null}),
                    )
                }),
            )
            .route(
                "/api/v1/users/{user_id}/memberships",
                axum_get(|| async {
                    axum::Json(json!({"items": [
                        {"room_id": "!a:example.org", "membership": "join"},
                        {"room_id": "!b:example.org", "membership": "leave"}]}))
                }),
            )
            .route(
                "/api/v1/users/{user_id}/sessions",
                axum_get(|| async {
                    axum::Json(json!({"items": [{"device_id": "D1", "ip": "10.0.0.1",
                        "user_agent": "Element", "last_seen_at": "2026-10-09T10:00:00Z"}]}))
                }),
            )
            .route(
                "/api/v1/users/{user_id}/reset-password",
                axum_post(|axum::Json(body): axum::Json<Value>| async move {
                    axum::Json(json!({"user_id": "@alice:example.org", "got": body}))
                }),
            )
            .route(
                "/api/v1/users/{user_id}/login-as",
                axum_post(|| async { axum::Json(json!({"access_token": "syt_acting"})) }),
            )
            .route(
                "/api/v1/users/availability",
                axum_get(
                    |Query(q): Query<std::collections::HashMap<String, String>>| async move {
                        axum::Json(json!({"available": q["localpart"] == "free"}))
                    },
                ),
            )
            .route(
                "/api/v1/users/lookup",
                axum_get(|| async { axum::Json(json!({"user_id": "@alice:example.org"})) }),
            )
            .route(
                "/api/v1/rooms/{room_id}/members",
                axum_get(|| async {
                    axum::Json(json!({"items": [
                        {"user_id": "@alice:example.org", "membership": "join"},
                        {"user_id": "@gone:example.org", "membership": "leave"}]}))
                }),
            )
            .route(
                "/api/v1/rooms/{room_id}/delete",
                axum_post(|| async {
                    (
                        StatusCode::ACCEPTED,
                        axum::Json(json!({"id": "task-1", "status": "scheduled"})),
                    )
                }),
            )
            .route(
                "/api/v1/tasks/{id}",
                axum_get(|| async {
                    axum::Json(
                        json!({"id": "task-1", "status": "succeeded", "action": "rooms.delete",
                        "result": {"new_room_id": null}}),
                    )
                }),
            )
            .route(
                "/api/v1/registration-tokens",
                axum_get(|| async {
                    axum::Json(
                        json!({"items": [{"token": "t1", "valid": true, "uses_allowed": 3,
                        "pending": 0, "completed": 1, "expires_at": null,
                        "created_at": "2026-10-09T10:00:00Z"}]}),
                    )
                })
                .post(|axum::Json(body): axum::Json<Value>| async move {
                    axum::Json(
                        json!({"token": body["token"], "valid": true, "uses_allowed": null,
                        "pending": 0, "completed": 0, "expires_at": body["expires_at"],
                        "created_at": "2026-10-09T10:00:00Z"}),
                    )
                }),
            )
            .route(
                "/api/v1/reports",
                axum_get(|| async {
                    axum::Json(json!({"items": [{"id": "r1", "room_id": "!a:example.org",
                        "event_id": "$e", "reporter_id": "@bob:example.org", "reason": "spam",
                        "score": -100, "received_at": "2026-10-09T10:00:00Z",
                        "event": {"sender": "@alice:example.org"}}], "total": 1}))
                }),
            )
            .route(
                "/api/v1/federation/destinations",
                axum_get(|| async {
                    axum::Json(json!({"items": [{"server_name": "other.org",
                        "retry_last_at": "2026-10-09T10:00:00Z", "retry_interval_ms": 5000,
                        "failing_since": null}], "total": 1}))
                }),
            )
    }

    async fn call(method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let router = crate::admin_proxy::router(AdminProxyState::new(fake_native()));
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer admin");
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let request = request
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn every_named_route_is_mounted_and_answers_in_synapses_shapes() {
        for (method, path, _) in ROUTES {
            let path = path
                .replace("{user_id}", "@alice:example.org")
                .replace("{device_id}", "D1")
                .replace("{room_id}", "!a:example.org")
                .replace("{event_id}", "$e")
                .replace("{delete_id}", "task-1")
                .replace("{redact_id}", "task-1")
                .replace("{token}", "t1")
                .replace("{report_id}", "r1")
                .replace("{server_name}", "example.org")
                .replace("{media_id}", "m1")
                .replace("{destination}", "other.org")
                .replace("{provider}", "oidc")
                .replace("{external_id}", "sub")
                .replace("{medium}", "email")
                .replace("{address}", "a@b.c")
                .replace("{txn_id}", "t");
            let (status, body) = call(method, &path, Some(json!({}))).await;
            // An unmounted path or method is axum's bare `404`/`405` with no JSON; a mounted
            // route that forwards to something the stand-in lacks answers a translated `404`
            // with an `errcode`.
            assert!(
                !(status == StatusCode::NOT_FOUND && body.is_null())
                    && status != StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is named but not mounted: {status} {body}"
            );
            assert_ne!(body["errcode"], "M_UNRECOGNIZED", "{method} {path}: {body}");
        }
    }

    #[tokio::test]
    async fn the_user_screens_answer_as_synapse_does() {
        let (status, admin) = call(
            "GET",
            "/_synapse/admin/v1/users/@alice:example.org/admin",
            None,
        )
        .await;
        assert_eq!((status, admin), (StatusCode::OK, json!({"admin": true})));
        let (status, devices) = call(
            "GET",
            "/_synapse/admin/v2/users/@alice:example.org/devices",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(devices["total"], 1);
        assert_eq!(devices["devices"][0]["device_id"], "D1");
        assert_eq!(devices["devices"][0]["last_seen_ts"], 1_791_540_000_000_i64);
        let (_, joined) = call(
            "GET",
            "/_synapse/admin/v1/users/@alice:example.org/joined_rooms",
            None,
        )
        .await;
        assert_eq!(
            joined,
            json!({"total": 1, "joined_rooms": ["!a:example.org"]})
        );
        let (_, whois) = call("GET", "/_synapse/admin/v1/whois/@alice:example.org", None).await;
        assert_eq!(
            whois["devices"]["D1"]["sessions"][0]["connections"][0]["ip"],
            "10.0.0.1"
        );
        let (status, reset) = call(
            "POST",
            "/_synapse/admin/v1/reset_password/@alice:example.org",
            Some(json!({"new_password": "pw", "logout_devices": false})),
        )
        .await;
        assert_eq!((status, reset), (StatusCode::OK, json!({})));
        let (_, login) = call(
            "POST",
            "/_synapse/admin/v1/users/@alice:example.org/login",
            Some(json!({})),
        )
        .await;
        assert_eq!(login, json!({"access_token": "syt_acting"}));
        let (status, _) = call(
            "GET",
            "/_synapse/admin/v1/username_available?username=free",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, taken) = call(
            "GET",
            "/_synapse/admin/v1/username_available?username=taken",
            None,
        )
        .await;
        assert_eq!(
            (status, taken["errcode"].clone()),
            (StatusCode::BAD_REQUEST, json!("M_USER_IN_USE"))
        );
        let (_, by_threepid) =
            call("GET", "/_synapse/admin/v1/threepid/email/users/a@b.c", None).await;
        assert_eq!(by_threepid, json!({"user_id": "@alice:example.org"}));
        let (status, nobody) = call(
            "GET",
            "/_synapse/admin/v1/users/@nobody:example.org/admin",
            None,
        )
        .await;
        assert_eq!(
            (status, nobody["errcode"].clone()),
            (StatusCode::NOT_FOUND, json!("M_NOT_FOUND"))
        );
    }

    #[tokio::test]
    async fn the_room_token_report_and_federation_screens_answer_as_synapse_does() {
        let (_, members) = call(
            "GET",
            "/_synapse/admin/v1/rooms/!a:example.org/members",
            None,
        )
        .await;
        assert_eq!(
            members,
            json!({"total": 1, "members": ["@alice:example.org"]})
        );
        let (status, deleted) = call(
            "DELETE",
            "/_synapse/admin/v2/rooms/!a:example.org",
            Some(json!({"purge": true})),
        )
        .await;
        assert_eq!(
            (status, deleted),
            (StatusCode::OK, json!({"delete_id": "task-1"}))
        );
        let (_, delete_status) =
            call("GET", "/_synapse/admin/v2/rooms/delete_status/task-1", None).await;
        assert_eq!(delete_status["status"], "complete");
        assert_eq!(delete_status["shutdown_room"]["kicked_users"], json!([]));
        let (_, tokens) = call("GET", "/_synapse/admin/v1/registration_tokens", None).await;
        assert_eq!(tokens["registration_tokens"][0]["token"], "t1");
        assert_eq!(tokens["registration_tokens"][0]["expiry_time"], Value::Null);
        let (_, made) = call(
            "POST",
            "/_synapse/admin/v1/registration_tokens/new",
            Some(json!({"token": "t2", "expiry_time": 1_791_540_000_000_i64})),
        )
        .await;
        assert_eq!(made["token"], "t2");
        assert_eq!(made["expiry_time"], 1_791_540_000_000_i64);
        let (_, reports) = call("GET", "/_synapse/admin/v1/event_reports", None).await;
        assert_eq!(reports["total"], 1);
        assert_eq!(reports["event_reports"][0]["user_id"], "@bob:example.org");
        assert_eq!(reports["event_reports"][0]["sender"], "@alice:example.org");
        let (_, destinations) =
            call("GET", "/_synapse/admin/v1/federation/destinations", None).await;
        assert_eq!(destinations["destinations"][0]["destination"], "other.org");
        assert_eq!(destinations["destinations"][0]["retry_interval"], 5000);
    }

    #[test]
    fn milliseconds_round_trip_through_rfc3339() {
        let ms = 1_791_626_400_123_i64;
        let text = rfc3339(ms).unwrap();
        assert_eq!(parse_rfc3339_ms(&text), Some(ms));
    }
}
