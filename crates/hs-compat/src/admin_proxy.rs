//! A first slice of `/_synapse/admin` routes, shimmed onto the native `/api/v1` admin router
//! (`hs_admin::router::build_router`) that track 15 ships, so `synapse-admin`, Draupnir, Mjolnir
//! and existing operator scripts keep working against this compat surface while the real logic
//! and storage live entirely in `hs-admin`.
//!
//! `docs/compat/synapse-admin-routes.md` maps all 77 Synapse admin routes onto a native
//! `/api/v1` resource; this module *implements* the handful the task that produced it called out
//! explicitly (a capability probe, plus user and room queries): `GET
//! /_synapse/admin/v1/server_version`, `GET /_synapse/admin/v2/users`, `GET
//! /_synapse/admin/v2/users/{user_id}`, `GET /_synapse/admin/v1/rooms`, and `GET
//! /_synapse/admin/v1/rooms/{room_id}`. Every other row in that table remains exactly what it
//! already said (mapped/mapped-diff/unsupported) — this module does not change any
//! classification, it just makes five of the "mapped (diff)" rows real. See
//! `docs/status/13-config-compat-and-migration.md` for the mounting instructions and the list of
//! rows this module does *not* yet implement (writes — `PUT`/`POST`/`PATCH` — are out of scope
//! for this pass; every route here is read-only).
//!
//! # Why forward in-process rather than over HTTP
//!
//! `hs_admin::router::build_router` returns an already-built, state-erased `axum::Router`
//! (`Router<()>`) — the exact same router `hs serve` mounts at `/api/v1`. `axum::Router`
//! implements `tower::Service<http::Request<axum::body::Body>>`, so a request can be dispatched
//! straight into it with [`tower::ServiceExt::oneshot`], in-process, with no TCP connection, no
//! loopback-address configuration, and no risk of the shim and the real API drifting because they
//! were wired up separately. This is the same technique axum's own test suite uses to exercise a
//! router without binding a socket. The cost is one `Router` clone per forwarded request (an
//! `axum::Router` is a handful of `Arc`s internally, so this is cheap — not a real allocation of
//! the route table).
//!
//! This module never constructs an `hs_admin::router::AdminState` and does not depend on the
//! `hs-admin` crate at all: every native response is parsed generically as [`serde_json::Value`]
//! and re-shaped into the Synapse response body by field name. That keeps this crate's
//! dependency tree lean (per this track's own instructions) and, more importantly, decouples this
//! shim from `hs-admin`'s internal Rust types — only its public JSON contract
//! (`crates/hs-admin/openapi/openapi.yaml`) needs to stay stable for this module to keep working.
//!
//! # Auth
//!
//! The incoming request's `Authorization` header is forwarded to the native router verbatim, so
//! the native router's own scope enforcement (`AdminRead`/... — see `hs_admin::router`) is what
//! actually authorizes the request; this module performs no authorization decision of its own; a
//! caller with no admin token gets exactly the native `401`/`403` Problem body, once translated
//! (see [`translate_error_body`]) into the flatter shape Synapse's admin API clients expect.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The largest native response body this module will buffer into memory before parsing it as
/// JSON. Every route this module forwards to returns a bounded page (native `Page::paginate`
/// clamps `limit` to 500 items) or a single resource, so this is generous headroom, not a tuned
/// limit — a native response that ever legitimately exceeds it is a bug in the native handler,
/// not a case this shim needs to stream around.
const MAX_NATIVE_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Shared state for the router [`router`] builds: a handle to the already-built native `/api/v1`
/// router this module forwards every request into.
#[derive(Clone)]
pub struct AdminProxyState {
    native: Router,
}

impl AdminProxyState {
    /// Wraps the fully-built native admin router — the first element of
    /// `hs_admin::router::build_router(state)`'s return value. `hs-cli` (or whichever crate
    /// mounts both routers) is expected to build the native router once at startup and pass it
    /// here; this module only ever reads from it via `Service::call`, never mutates it.
    #[must_use]
    pub fn new(native: Router) -> Self {
        Self { native }
    }
}

/// Builds the `/_synapse/admin/*` router this module implements. Merge this into the same
/// `axum::Router` that serves the rest of `/_synapse/admin` (today, just
/// `hs_auth::synapse_admin_router()`'s `/_synapse/admin/v1/register`) — see
/// `docs/status/13-config-compat-and-migration.md` for the exact merge call `hs-cli` needs.
pub fn router(state: AdminProxyState) -> Router {
    Router::new()
        .route("/_synapse/admin/v1/server_version", get(server_version))
        .route("/_synapse/admin/v2/users", get(users_list))
        .route("/_synapse/admin/v2/users/{user_id}", get(users_get))
        .route("/_synapse/admin/v1/rooms", get(rooms_list))
        .route("/_synapse/admin/v1/rooms/{room_id}", get(rooms_get))
        .with_state(state)
}

/// Forwards a `GET` request with no body to the native router at `path`, carrying over the
/// caller's `Authorization` header (native scope enforcement is what actually authorizes the
/// request — see the module doc's "Auth" section), and returns the parsed JSON body on `2xx`, or
/// `Err` with a Synapse-shaped error response already built from the native error body otherwise.
async fn forward_get(
    native: &Router,
    path: &str,
    headers: &HeaderMap,
) -> Result<Value, Box<Response>> {
    let mut builder = Request::builder().method(Method::GET).uri(path);
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        builder = builder.header(header::AUTHORIZATION, auth.clone());
    }
    // `Body::empty()` and a `GET` to a path this module builds itself: both infallible by
    // construction, hence the `expect`s rather than propagating a build error a caller could
    // never actually trigger.
    let request = builder
        .body(Body::empty())
        .expect("GET request to a fixed, valid path never fails to build");
    // `Router<()>`'s `Service` impl is infallible (`Error = Infallible`) -- this can never
    // actually return `Err`, matching every other axum-router caller in this workspace.
    let response = native
        .clone()
        .oneshot(request)
        .await
        .unwrap_or_else(|infallible: std::convert::Infallible| match infallible {});
    let status = response.status();
    let body = to_bytes(response.into_body(), MAX_NATIVE_RESPONSE_BYTES)
        .await
        .unwrap_or_default();
    if status.is_success() {
        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        Ok(value)
    } else {
        Err(Box::new(translate_error_body(status, &body)))
    }
}

/// Re-shapes a native RFC 7807 Problem response (`hs_admin::router`'s error bodies — `{type,
/// title, status, detail, instance, ...}`) into the flat `{errcode, error}` shape every Synapse
/// admin API error uses, preserving the native HTTP status code exactly. `errcode` is a best
/// effort, coarse mapping (`404` -> `M_NOT_FOUND`, `401`/`403` -> `M_FORBIDDEN`, anything else ->
/// `M_UNKNOWN`) — Synapse's admin API has finer-grained errcodes in a few places this does not
/// attempt to reproduce; a caller that branches on `errcode` rather than the HTTP status for one
/// of those cases should treat this as a known, coarser gap, not a bug to route around silently.
fn translate_error_body(status: StatusCode, native_body: &[u8]) -> Response {
    let detail = serde_json::from_slice::<Value>(native_body)
        .ok()
        .and_then(|v| {
            v.get("detail")
                .or_else(|| v.get("title"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_owned());
    let errcode = match status {
        StatusCode::NOT_FOUND => "M_NOT_FOUND",
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "M_FORBIDDEN",
        StatusCode::TOO_MANY_REQUESTS => "M_LIMIT_EXCEEDED",
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "M_INVALID_PARAM",
        _ => "M_UNKNOWN",
    };
    (
        status,
        axum::Json(json!({"errcode": errcode, "error": detail})),
    )
        .into_response()
}

/// Parses an RFC 3339 timestamp (the shape `hs_admin::model::AdminUser::created_at`/
/// `last_seen_at` carry) into milliseconds since the Unix epoch. Returns `None` rather than
/// panicking on a native response this shim did not expect the shape of — a translation gap
/// should degrade to a missing field, never a crashed handler.
fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let dt = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from((dt - time::OffsetDateTime::UNIX_EPOCH).whole_milliseconds()).ok()
}

// ---------------------------------------------------------------------------------------------
// GET /_synapse/admin/v1/server_version  ->  GET /api/v1/server
// ---------------------------------------------------------------------------------------------

/// `GET /_synapse/admin/v1/server_version`. Almost every Synapse admin tool's first call (a
/// capability probe), so this is the highest-value single route to shim even on its own.
/// `docs/compat/synapse-admin-routes.md`'s row for this route (checked against
/// `crates/hs-admin/src/router.rs`'s `server_get`/`ServerInfoResponse`, which is implemented, not
/// a `501` stub).
async fn server_version(State(state): State<AdminProxyState>, headers: HeaderMap) -> Response {
    match forward_get(&state.native, "/api/v1/server", &headers).await {
        Ok(info) => {
            let version = info
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            // Synapse's `python_version` has no meaning for a process that is not Python
            // (checked: `crates/hs-admin/src/model.rs`'s `ServerInfo` carries no such field, and
            // nothing in this workspace runs on a Python interpreter). Reported literally rather
            // than omitted, since a client that unconditionally reads this field would otherwise
            // get a JSON decode/`KeyError` surprise instead of an honest "not applicable" value.
            axum::Json(json!({
                "server_version": version,
                "python_version": "n/a (this server is not implemented in Python)",
            }))
            .into_response()
        }
        Err(resp) => *resp,
    }
}

// ---------------------------------------------------------------------------------------------
// GET /_synapse/admin/v2/users, GET /_synapse/admin/v2/users/{user_id}
//   ->  GET /api/v1/users, GET /api/v1/users/{user_id}
// ---------------------------------------------------------------------------------------------

/// Query parameters `GET /_synapse/admin/v2/users` accepts
/// (`refs/synapse/docs/admin_api/user_admin_api.md`'s "List Accounts (V2)"). `order_by`, `dir`
/// and `not_user_type` are accepted (so a client sending them does not get a `400`) but not
/// honoured — the native `users.list` operation has no sort or type-exclusion parameter yet
/// (`crates/hs-admin/src/router.rs`'s `UsersListQuery`, checked); noted as a stated limitation,
/// not silently dropped.
#[derive(Debug, Default, Deserialize)]
struct SynapseUsersListQuery {
    from: Option<String>,
    limit: Option<usize>,
    guests: Option<bool>,
    admins: Option<bool>,
    deactivated: Option<bool>,
    locked: Option<bool>,
    user_id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    order_by: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    dir: Option<String>,
}

/// `GET /_synapse/admin/v2/users`. Forwards to `GET /api/v1/users`, translating the query
/// parameters and the `Page<AdminUser>` response into Synapse's `{users, next_token, total}`
/// shape (`refs/synapse/docs/admin_api/user_admin_api.md`'s "List Accounts (V2)" response,
/// checked).
async fn users_list(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Query(q): Query<SynapseUsersListQuery>,
) -> Response {
    // `name` takes precedence over `user_id` per Synapse's own documented behavior ("This
    // parameter is ignored when using the name parameter"); both map onto the native free-text
    // filter, which searches across the same fields Synapse's `name`/`user_id` params together
    // cover (checked: `crates/hs-admin/src/sources.rs`'s `UserFilter::q` doc, if present, or
    // `router.rs`'s `users_list` passing `query.q` straight to `UserDirectory::list_users`).
    let free_text = q.name.or(q.user_id);
    let mut path = format!(
        "/api/v1/users?include_total=true&limit={}",
        q.limit.unwrap_or(100)
    );
    if let Some(from) = &q.from {
        path.push_str("&cursor=");
        path.push_str(&urlencoding_light(from));
    }
    if let Some(text) = &free_text {
        path.push_str("&q=");
        path.push_str(&urlencoding_light(text));
    }
    if let Some(admins) = q.admins {
        path.push_str(if admins {
            "&admin=true"
        } else {
            "&admin=false"
        });
    }
    if let Some(deactivated) = q.deactivated {
        path.push_str(if deactivated {
            "&deactivated=true"
        } else {
            "&deactivated=false"
        });
    }
    if let Some(locked) = q.locked {
        path.push_str(if locked {
            "&locked=true"
        } else {
            "&locked=false"
        });
    }
    if let Some(guests) = q.guests {
        path.push_str(if guests {
            "&guests=true"
        } else {
            "&guests=false"
        });
    }

    match forward_get(&state.native, &path, &headers).await {
        Ok(page) => {
            let items = page
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let users: Vec<Value> = items.iter().map(admin_user_to_synapse_list_item).collect();
            let mut body = json!({
                "users": users,
                "total": page.get("total").cloned().unwrap_or(Value::from(users.len())),
            });
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["next_token"] = Value::String(next.to_owned());
            }
            axum::Json(body).into_response()
        }
        Err(resp) => *resp,
    }
}

/// `GET /_synapse/admin/v2/users/{user_id}`. Forwards to `GET /api/v1/users/{user_id}`.
async fn users_get(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let path = format!("/api/v1/users/{}", urlencoding_light(&user_id));
    match forward_get(&state.native, &path, &headers).await {
        Ok(user) => axum::Json(admin_user_to_synapse_full_record(&user)).into_response(),
        Err(resp) => *resp,
    }
}

/// One item of `GET /_synapse/admin/v2/users`' `users` array
/// (`refs/synapse/docs/admin_api/user_admin_api.md`'s "List Accounts (V2)" example, checked).
/// `is_guest` is always `false`: `hs_admin::model::AdminUser` (checked,
/// `crates/hs-admin/src/model.rs`) has no guest-account field to read one from yet — a real gap,
/// not an assumption that no deployment has guests.
fn admin_user_to_synapse_list_item(u: &Value) -> Value {
    json!({
        "name": u.get("user_id").cloned().unwrap_or(Value::Null),
        "is_guest": false,
        "admin": u.get("admin").cloned().unwrap_or(Value::Bool(false)),
        "user_type": u.get("user_type").cloned().unwrap_or(Value::Null),
        "deactivated": u.get("deactivated").cloned().unwrap_or(Value::Bool(false)),
        "erased": u.get("erased").cloned().unwrap_or(Value::Bool(false)),
        "shadow_banned": u.get("shadow_banned").cloned().unwrap_or(Value::Bool(false)),
        "displayname": u.get("display_name").cloned().unwrap_or(Value::Null),
        "avatar_url": u.get("avatar_url").cloned().unwrap_or(Value::Null),
        // Milliseconds, per the List Accounts (V2) documented example (`refs/synapse/docs/
        // admin_api/user_admin_api.md`: `"creation_ts": 1560432668000`) -- taken from that
        // documented example, not independently re-verified against
        // `get_users_paginate`'s actual serializer the way the single-user endpoint's
        // seconds-scale `creation_ts` was (see `admin_user_to_synapse_full_record`'s doc).
        "creation_ts": u
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
            .map(Value::from)
            .unwrap_or(Value::Null),
        "locked": u.get("locked").cloned().unwrap_or(Value::Bool(false)),
    })
}

/// The full user record `GET /_synapse/admin/v2/users/{user_id}` returns
/// (`refs/synapse/docs/admin_api/user_admin_api.md`'s "Query User Account", checked). `threepids`
/// and `external_ids` are always empty arrays: `hs_admin::model::AdminUser` (checked) carries
/// neither — those live behind separate native resources
/// (`GET /users/{user_id}/threepids`-equivalent, `GET /users/lookup`) this first pass does not
/// call out to. `creation_ts` here is **seconds**, not milliseconds, matching
/// `refs/synapse/synapse/handlers/admin.py`'s `get_user` (checked: `is_trial = (now -
/// info.creation_ts * 1000) < trial_duration_ms` only type-checks if `creation_ts` is
/// seconds) -- deliberately different from the list endpoint's milliseconds above, because
/// Synapse itself is inconsistent between the two endpoints, and this shim reproduces Synapse's
/// actual behavior rather than "fixing" it into a shape that looks more consistent.
fn admin_user_to_synapse_full_record(u: &Value) -> Value {
    let created_at_ms = u
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms);
    let last_seen_ms = u
        .get("last_seen_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms);
    json!({
        "name": u.get("user_id").cloned().unwrap_or(Value::Null),
        "displayname": u.get("display_name").cloned().unwrap_or(Value::Null),
        "threepids": Value::Array(Vec::new()),
        "avatar_url": u.get("avatar_url").cloned().unwrap_or(Value::Null),
        "is_guest": false,
        "admin": u.get("admin").cloned().unwrap_or(Value::Bool(false)),
        "deactivated": u.get("deactivated").cloned().unwrap_or(Value::Bool(false)),
        "erased": u.get("erased").cloned().unwrap_or(Value::Bool(false)),
        "shadow_banned": u.get("shadow_banned").cloned().unwrap_or(Value::Bool(false)),
        "creation_ts": created_at_ms.map(|ms| ms / 1000).map(Value::from).unwrap_or(Value::Null),
        "last_seen_ts": last_seen_ms.map(Value::from).unwrap_or(Value::Null),
        "appservice_id": u.get("appservice_id").cloned().unwrap_or(Value::Null),
        "consent_server_notice_sent": Value::Null,
        "consent_version": u.get("consent_version").cloned().unwrap_or(Value::Null),
        "consent_ts": Value::Null,
        "external_ids": Value::Array(Vec::new()),
        "user_type": u.get("user_type").cloned().unwrap_or(Value::Null),
        "locked": u.get("locked").cloned().unwrap_or(Value::Bool(false)),
        "suspended": u.get("suspended").cloned().unwrap_or(Value::Bool(false)),
    })
}

// ---------------------------------------------------------------------------------------------
// GET /_synapse/admin/v1/rooms, GET /_synapse/admin/v1/rooms/{room_id}
//   ->  GET /api/v1/rooms, GET /api/v1/rooms/{room_id}
// ---------------------------------------------------------------------------------------------

/// Query parameters `GET /_synapse/admin/v1/rooms` accepts (`refs/synapse/docs/admin_api/
/// rooms.md`'s "List Room API", checked). `order_by` and `dir` are accepted but not honoured —
/// the native `rooms.list` operation has no sort parameter yet (`crates/hs-admin/src/router.rs`'s
/// `RoomsListQuery`, checked).
#[derive(Debug, Default, Deserialize)]
struct SynapseRoomsListQuery {
    from: Option<String>,
    limit: Option<usize>,
    search_term: Option<String>,
    public_rooms: Option<bool>,
    empty_rooms: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)]
    order_by: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    dir: Option<String>,
}

/// `GET /_synapse/admin/v1/rooms`. Forwards to `GET /api/v1/rooms`, translating the `Page<
/// AdminRoom>` response into Synapse's `{rooms, offset, total_rooms, next_batch, prev_batch}`
/// shape. The native cursor is a plain decimal offset string
/// (`crates/hs-admin/src/model.rs`'s `Page::paginate`, checked: `cursor.and_then(|c|
/// c.parse::<usize>().ok())`), which is numerically the same thing Synapse's own `from`/
/// `next_batch` are — this is a real behavioral match, not just both sides agreeing to treat an
/// opaque string as opaque.
async fn rooms_list(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Query(q): Query<SynapseRoomsListQuery>,
) -> Response {
    let from = q.from.as_deref().unwrap_or("0");
    let mut path = format!(
        "/api/v1/rooms?include_total=true&limit={}&cursor={}",
        q.limit.unwrap_or(100),
        urlencoding_light(from)
    );
    if let Some(term) = &q.search_term {
        path.push_str("&q=");
        path.push_str(&urlencoding_light(term));
    }
    if let Some(public) = q.public_rooms {
        path.push_str(if public {
            "&public=true"
        } else {
            "&public=false"
        });
    }
    if let Some(empty) = q.empty_rooms {
        path.push_str(if empty { "&empty=true" } else { "&empty=false" });
    }

    match forward_get(&state.native, &path, &headers).await {
        Ok(page) => {
            let items = page
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let rooms: Vec<Value> = items.iter().map(admin_room_to_synapse_list_item).collect();
            let mut body = json!({
                "rooms": rooms,
                "offset": from.parse::<u64>().unwrap_or(0),
                "total_rooms": page.get("total").cloned().unwrap_or(Value::from(rooms.len())),
            });
            if let Some(next) = page.get("next_cursor").and_then(Value::as_str) {
                body["next_batch"] = Value::String(next.to_owned());
            }
            if let Some(prev) = page.get("prev_cursor").and_then(Value::as_str) {
                body["prev_batch"] = Value::String(prev.to_owned());
            }
            axum::Json(body).into_response()
        }
        Err(resp) => *resp,
    }
}

/// `GET /_synapse/admin/v1/rooms/{room_id}`. Forwards to `GET /api/v1/rooms/{room_id}`.
async fn rooms_get(
    State(state): State<AdminProxyState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Response {
    let path = format!("/api/v1/rooms/{}", urlencoding_light(&room_id));
    match forward_get(&state.native, &path, &headers).await {
        Ok(room) => axum::Json(admin_room_to_synapse_full_record(&room)).into_response(),
        Err(resp) => *resp,
    }
}

/// One item of `GET /_synapse/admin/v1/rooms`' `rooms` array (`refs/synapse/docs/admin_api/
/// rooms.md`'s "List Room API" response, checked). `encryption` reports a fixed algorithm name
/// when the room is encrypted (native only tracks a bool, not which algorithm —
/// `hs_admin::model::AdminRoom::encrypted`, checked) since every room version this project
/// supports that enables encryption uses Megolm; flagged as a stated approximation, not a
/// verified per-room fact.
fn admin_room_to_synapse_list_item(r: &Value) -> Value {
    json!({
        "room_id": r.get("room_id").cloned().unwrap_or(Value::Null),
        "name": r.get("name").cloned().unwrap_or(Value::Null),
        "canonical_alias": r.get("canonical_alias").cloned().unwrap_or(Value::Null),
        "joined_members": r.get("joined_members_count").cloned().unwrap_or(Value::from(0)),
        "joined_local_members": r.get("local_members_count").cloned().unwrap_or(Value::from(0)),
        "version": r.get("version").cloned().unwrap_or(Value::Null),
        "creator": r.get("creator").cloned().unwrap_or(Value::Null),
        "encryption": encryption_algorithm_or_null(r),
        "federatable": r.get("federatable").cloned().unwrap_or(Value::Bool(false)),
        "public": r.get("public").cloned().unwrap_or(Value::Bool(false)),
        "join_rules": r.get("join_rule").cloned().unwrap_or(Value::Null),
        "guest_access": r.get("guest_access").cloned().unwrap_or(Value::Null),
        "history_visibility": r.get("history_visibility").cloned().unwrap_or(Value::Null),
        "state_events": r.get("state_events_count").cloned().unwrap_or(Value::from(0)),
        "room_type": r.get("room_type").cloned().unwrap_or(Value::Null),
    })
}

/// The full room record `GET /_synapse/admin/v1/rooms/{room_id}` returns (`refs/synapse/docs/
/// admin_api/rooms.md`'s "Room Details API", checked). `joined_local_devices` is always `0`: no
/// native field carries a per-room local-device count yet (`hs_admin::model::AdminRoom`, checked)
/// — a stated gap, not an assertion that no local devices are in the room. `replacement_room`
/// spells its key differently from the native `replacement_room_id` it reads (Synapse's own
/// field name, kept exact for client compatibility).
fn admin_room_to_synapse_full_record(r: &Value) -> Value {
    json!({
        "room_id": r.get("room_id").cloned().unwrap_or(Value::Null),
        "name": r.get("name").cloned().unwrap_or(Value::Null),
        "topic": r.get("topic").cloned().unwrap_or(Value::Null),
        "avatar": r.get("avatar_url").cloned().unwrap_or(Value::Null),
        "canonical_alias": r.get("canonical_alias").cloned().unwrap_or(Value::Null),
        "joined_members": r.get("joined_members_count").cloned().unwrap_or(Value::from(0)),
        "joined_local_members": r.get("local_members_count").cloned().unwrap_or(Value::from(0)),
        "joined_local_devices": Value::from(0),
        "version": r.get("version").cloned().unwrap_or(Value::Null),
        "creator": r.get("creator").cloned().unwrap_or(Value::Null),
        "encryption": encryption_algorithm_or_null(r),
        "federatable": r.get("federatable").cloned().unwrap_or(Value::Bool(false)),
        "public": r.get("public").cloned().unwrap_or(Value::Bool(false)),
        "join_rules": r.get("join_rule").cloned().unwrap_or(Value::Null),
        "guest_access": r.get("guest_access").cloned().unwrap_or(Value::Null),
        "history_visibility": r.get("history_visibility").cloned().unwrap_or(Value::Null),
        "state_events": r.get("state_events_count").cloned().unwrap_or(Value::from(0)),
        "room_type": r.get("room_type").cloned().unwrap_or(Value::Null),
        "forgotten": r.get("forgotten").cloned().unwrap_or(Value::Bool(false)),
        "tombstoned": r.get("tombstoned").cloned().unwrap_or(Value::Bool(false)),
        "replacement_room": r.get("replacement_room_id").cloned().unwrap_or(Value::Null),
    })
}

fn encryption_algorithm_or_null(r: &Value) -> Value {
    match r.get("encrypted").and_then(Value::as_bool) {
        Some(true) => Value::String("m.megolm.v1.aes-sha2".to_owned()),
        _ => Value::Null,
    }
}

/// A minimal query-string value encoder: escapes exactly the bytes that would otherwise break the
/// URL this module builds by hand (space, `&`, `%`, `#`, `?`, `+`, `/`). Not a general
/// `application/x-www-form-urlencoded` implementation — this workspace has no `urlencoding`-style
/// crate in `[workspace.dependencies]` yet, and pulling one in for five call sites that only ever
/// see user IDs, room IDs and simple search terms was judged not worth a new shared dependency.
/// Revisit if a value with characters outside this set turns out to matter in practice.
fn urlencoding_light(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'&' | b'%' | b'#' | b'?' | b'+' | b'/' => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
            _ => out.push(b as char),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::{get as axum_get, post};

    /// A tiny stand-in for the native `/api/v1` router: just enough of the real shapes
    /// (`server`, `users`, `users/{id}`, `rooms`, `rooms/{id}`) to exercise this module's
    /// forwarding and translation logic without depending on `hs-admin` at all, exactly as this
    /// module's own doc explains it is designed to.
    fn fake_native_router() -> Router {
        Router::new()
            .route(
                "/api/v1/server",
                axum_get(|| async {
                    axum::Json(json!({
                        "name": "hs",
                        "version": "0.1.0-test",
                        "build": "dev",
                        "supported_room_versions": ["11"],
                        "enabled_components": [],
                        "uptime_ms": 1234,
                        "contract_version": "1.0",
                    }))
                }),
            )
            .route(
                "/api/v1/users",
                axum_get(|| async {
                    axum::Json(json!({
                        "items": [{
                            "user_id": "@alice:example.org",
                            "display_name": "Alice",
                            "avatar_url": null,
                            "admin": true,
                            "deactivated": false,
                            "erased": false,
                            "locked": false,
                            "suspended": false,
                            "shadow_banned": false,
                            "user_type": null,
                            "consent_version": null,
                            "appservice_id": null,
                            "created_at": "2024-01-01T00:00:00.000Z",
                            "last_seen_at": "2024-06-01T12:00:00.000Z",
                            "device_count": 2,
                            "room_count": 3,
                            "media_count": 0,
                        }],
                        "next_cursor": "1",
                        "prev_cursor": null,
                        "total": 1,
                    }))
                }),
            )
            .route(
                "/api/v1/users/{user_id}",
                axum_get(|Path(user_id): Path<String>| async move {
                    if user_id == "@alice:example.org" {
                        axum::Json(json!({
                            "user_id": "@alice:example.org",
                            "display_name": "Alice",
                            "avatar_url": null,
                            "admin": true,
                            "deactivated": false,
                            "erased": false,
                            "locked": false,
                            "suspended": false,
                            "shadow_banned": false,
                            "user_type": null,
                            "consent_version": null,
                            "appservice_id": null,
                            "created_at": "2024-01-01T00:00:00.000Z",
                            "last_seen_at": "2024-06-01T12:00:00.000Z",
                            "device_count": 2,
                            "room_count": 3,
                            "media_count": 0,
                        }))
                        .into_response()
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            axum::Json(json!({"title": "not found", "status": 404})),
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/api/v1/rooms",
                axum_get(|| async {
                    axum::Json(json!({
                        "items": [{
                            "room_id": "!abc:example.org",
                            "name": "General",
                            "topic": null,
                            "avatar_url": null,
                            "canonical_alias": "#general:example.org",
                            "joined_members_count": 5,
                            "local_members_count": 3,
                            "state_events_count": 42,
                            "version": "11",
                            "creator": "@alice:example.org",
                            "encrypted": true,
                            "join_rule": "invite",
                            "guest_access": "forbidden",
                            "history_visibility": "shared",
                            "federatable": true,
                            "public": false,
                            "room_type": null,
                            "blocked": false,
                            "blocked_reason": null,
                            "tombstoned": false,
                            "replacement_room_id": null,
                            "forgotten": false,
                        }],
                        "next_cursor": null,
                        "prev_cursor": null,
                        "total": 1,
                    }))
                }),
            )
            .route(
                "/api/v1/rooms/{room_id}",
                axum_get(|Path(room_id): Path<String>| async move {
                    if room_id == "!abc:example.org" {
                        axum::Json(json!({
                            "room_id": "!abc:example.org",
                            "name": "General",
                            "topic": "chat",
                            "avatar_url": "mxc://example.org/abc",
                            "canonical_alias": "#general:example.org",
                            "joined_members_count": 5,
                            "local_members_count": 3,
                            "state_events_count": 42,
                            "version": "11",
                            "creator": "@alice:example.org",
                            "encrypted": false,
                            "join_rule": "invite",
                            "guest_access": "forbidden",
                            "history_visibility": "shared",
                            "federatable": true,
                            "public": false,
                            "room_type": null,
                            "blocked": false,
                            "blocked_reason": null,
                            "tombstoned": false,
                            "replacement_room_id": null,
                            "forgotten": false,
                        }))
                        .into_response()
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            axum::Json(json!({"title": "not found", "status": 404})),
                        )
                            .into_response()
                    }
                }),
            )
            // Present to prove the shim never touches anything but GET on these paths.
            .route(
                "/api/v1/users",
                post(|| async { StatusCode::NOT_IMPLEMENTED }),
            )
    }

    async fn call(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .header(header::AUTHORIZATION, "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_NATIVE_RESPONSE_BYTES)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    fn app() -> Router {
        router(AdminProxyState::new(fake_native_router()))
    }

    #[tokio::test]
    async fn server_version_reports_the_native_version() {
        let (status, body) = call(app(), "/_synapse/admin/v1/server_version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["server_version"], "0.1.0-test");
        assert!(body["python_version"].is_string());
    }

    #[tokio::test]
    async fn users_list_translates_the_native_page() {
        let (status, body) = call(app(), "/_synapse/admin/v2/users").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["users"][0]["name"], "@alice:example.org");
        assert_eq!(body["users"][0]["displayname"], "Alice");
        assert_eq!(body["users"][0]["admin"], true);
        assert_eq!(body["users"][0]["is_guest"], false);
        assert_eq!(body["next_token"], "1");
        assert_eq!(body["total"], 1);
        // Milliseconds: 2024-01-01T00:00:00.000Z.
        assert_eq!(body["users"][0]["creation_ts"], 1_704_067_200_000i64);
    }

    #[tokio::test]
    async fn users_get_translates_the_full_record_and_uses_seconds_for_creation_ts() {
        let (status, body) = call(app(), "/_synapse/admin/v2/users/@alice:example.org").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "@alice:example.org");
        assert_eq!(body["threepids"], json!([]));
        assert_eq!(body["external_ids"], json!([]));
        // Seconds, not milliseconds -- see this route's own doc comment for why that is
        // deliberately different from the list endpoint above.
        assert_eq!(body["creation_ts"], 1_704_067_200i64);
        // Milliseconds: 2024-06-01T12:00:00.000Z.
        assert_eq!(body["last_seen_ts"], 1_717_243_200_000i64);
    }

    #[tokio::test]
    async fn users_get_missing_user_translates_the_404() {
        let (status, body) = call(app(), "/_synapse/admin/v2/users/@nobody:example.org").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["errcode"], "M_NOT_FOUND");
    }

    #[tokio::test]
    async fn rooms_list_translates_the_native_page() {
        let (status, body) = call(app(), "/_synapse/admin/v1/rooms").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rooms"][0]["room_id"], "!abc:example.org");
        assert_eq!(body["rooms"][0]["joined_members"], 5);
        assert_eq!(body["rooms"][0]["joined_local_members"], 3);
        assert_eq!(body["rooms"][0]["join_rules"], "invite");
        assert_eq!(body["rooms"][0]["encryption"], "m.megolm.v1.aes-sha2");
        assert_eq!(body["total_rooms"], 1);
        assert_eq!(body["offset"], 0);
        assert!(body.get("next_batch").is_none());
    }

    #[tokio::test]
    async fn rooms_get_translates_the_full_record() {
        let (status, body) = call(app(), "/_synapse/admin/v1/rooms/!abc:example.org").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["room_id"], "!abc:example.org");
        assert_eq!(body["avatar"], "mxc://example.org/abc");
        assert_eq!(body["joined_local_devices"], 0);
        assert_eq!(body["encryption"], Value::Null);
        assert_eq!(body["replacement_room"], Value::Null);
    }

    #[tokio::test]
    async fn rooms_get_missing_room_translates_the_404() {
        let (status, body) = call(app(), "/_synapse/admin/v1/rooms/!nobody:example.org").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["errcode"], "M_NOT_FOUND");
    }

    #[test]
    fn parses_rfc3339_millis() {
        assert_eq!(
            parse_rfc3339_ms("2024-01-01T00:00:00.000Z"),
            Some(1_704_067_200_000)
        );
        assert_eq!(parse_rfc3339_ms("not a timestamp"), None);
    }

    #[test]
    fn urlencoding_light_escapes_the_bytes_that_matter() {
        assert_eq!(
            urlencoding_light("@alice:example.org"),
            "@alice:example.org"
        );
        assert_eq!(urlencoding_light("a b"), "a+b");
        assert_eq!(urlencoding_light("100% sure"), "100%25+sure");
        assert_eq!(urlencoding_light("a&b"), "a%26b");
    }
}
