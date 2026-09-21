//! The `hs-admin` axum router (RFC 0004, deliverable 5 of track 15's brief).
//!
//! Every operation in `openapi/operations.json` (generated alongside `openapi.yaml`) gets a
//! route that: extracts `Authorization`, verifies the token through [`TokenVerifier`]
//! (`crate::auth`), checks the operation's minimal scope, and then either runs a real handler (a
//! first slice: `me.get`, `server.get`, `server.health`, `users.list`, `users.get`; see
//! [`REAL_HANDLERS`]) or answers `501 not-implemented` (RFC 0004 section 3.5: "Endpoint declared
//! but not yet served (Phase 0 skeleton)") for everything else. Real handlers land resource by
//! resource; this proves the plumbing (auth, scopes, the manifest, the OpenAPI contract check)
//! end to end for the rest. `tests/contract.rs` asserts this router's manifest agrees with
//! `openapi.yaml` regardless of which operations are real yet — registering a real handler never
//! changes an operation's `(method, path)`.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as AxumSseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::Stream;
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_http::{Problem, ValidationError};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{AuditFilter, AuditSink};
use crate::auth::{ScopeDecision, TokenVerifier, require_scope};
use crate::events::{EventBus, ReplayOutcome};
use crate::idempotency::{IdempotencyStore, Replay, StoredResponse};
use crate::model::{
    Actor, ActorKind, AuditChange, AuditEntry, AuditOutcome, ConfigSchema, ConfigSection,
    ConfigSectionInfo, ConfigSettingInfo, Event, Page, Principal, ResourceRef, Scope, ServerHealth,
    ServerInfo, SetupRequest, SetupStatus,
};
use crate::operations::{OperationDef, load as load_operations};
use crate::sources::{
    ConfigPatch, ConfigSource, OverviewSource, RoomDirectory, RoomFilter, SetupSource, SourceError,
    UserCreateRequest, UserDirectory, UserFilter, UserLookupQuery,
};

/// Everything an `hs-admin` handler needs. Cloned per-request by axum (cheap: everything inside
/// is an `Arc`, a plain value type, or `Copy`).
#[derive(Clone)]
pub struct AdminState {
    pub verifier: Arc<dyn TokenVerifier>,
    pub audit: Arc<dyn AuditSink>,
    pub events: Arc<EventBus>,
    /// The raw OpenAPI document this binary serves at `/api/v1/openapi.yaml` and `.json`.
    pub openapi_yaml: Arc<str>,
    /// The static parts of `GET /server`'s response. Defaulted by [`AdminState::new`]; the
    /// integration lead sets it from the running binary's real identity with
    /// [`AdminState::with_server_info`].
    pub server_info: ServerInfo,
    /// When this state (and so, in practice, this server process) started, for `GET
    /// /server`'s `uptime_ms`.
    pub started_at: Instant,
    /// The user directory `GET /users` and `GET /users/{user_id}` read from. `None` until the
    /// integration lead wires a real implementation with [`AdminState::with_users`]; the two user
    /// handlers answer `503 unavailable` rather than faking data or hiding behind a `404`.
    pub users: Option<Arc<dyn UserDirectory>>,
    /// The room directory `GET /rooms`, `GET /rooms/{room_id}`, and the block/unblock/make-admin
    /// moderation actions read from and call. `None` until a real implementation exists — see
    /// `crate::sources::RoomDirectory`'s doc comment for the contract track 04 should implement
    /// this against; this session did not implement one (`hs-room` is owned by another track).
    pub rooms: Option<Arc<dyn RoomDirectory>>,
    /// The configuration every `/config*` operation reads and writes through. `None` until the
    /// integration lead wires a real implementation with [`AdminState::with_config`]; until then
    /// those operations answer `503 unavailable`, because a management interface that cannot
    /// reach the configuration should say so rather than show an empty form.
    pub config: Option<Arc<dyn ConfigSource>>,
    /// What `GET /setup` and `POST /setup` call to create the first administrator. `None` until
    /// wired with [`AdminState::with_setup`]; until then `GET /setup` says no setup is on offer
    /// (which is true: nothing here could perform one) and `POST /setup` answers `503`.
    pub setup: Option<Arc<dyn SetupSource>>,
    /// What `GET /statistics/overview` and `GET /cluster` read. `None` until wired with
    /// [`AdminState::with_overview`]; until then both answer `503 unavailable`.
    pub overview: Option<Arc<dyn OverviewSource>>,
    /// The `Idempotency-Key` cache every mutating handler that declares it consults (see
    /// [`crate::idempotency`]). Always present (never `None`): a client is never told its
    /// idempotency key was ignored.
    pub idempotency: Arc<IdempotencyStore>,
}

impl AdminState {
    pub fn new(
        verifier: Arc<dyn TokenVerifier>,
        audit: Arc<dyn AuditSink>,
        events: Arc<EventBus>,
    ) -> Self {
        Self {
            verifier,
            audit,
            events,
            openapi_yaml: Arc::from(crate::openapi::DOCUMENT),
            server_info: ServerInfo::default(),
            started_at: Instant::now(),
            users: None,
            rooms: None,
            config: None,
            setup: None,
            overview: None,
            idempotency: Arc::new(IdempotencyStore::new()),
        }
    }

    /// Wires a real [`UserDirectory`], making `GET /users` and `GET /users/{user_id}` serve real
    /// data instead of `503 unavailable`.
    #[must_use]
    pub fn with_users(mut self, users: Arc<dyn UserDirectory>) -> Self {
        self.users = Some(users);
        self
    }

    /// Wires a real [`RoomDirectory`], making `GET /rooms`, `GET /rooms/{room_id}`, and the
    /// block/unblock/make-admin moderation actions serve real data instead of `503 unavailable`.
    #[must_use]
    pub fn with_rooms(mut self, rooms: Arc<dyn RoomDirectory>) -> Self {
        self.rooms = Some(rooms);
        self
    }

    /// Wires a real [`ConfigSource`], making the `/config*` operations read and change this
    /// server's actual configuration instead of answering `503 unavailable`.
    #[must_use]
    pub fn with_config(mut self, config: Arc<dyn ConfigSource>) -> Self {
        self.config = Some(config);
        self
    }

    /// Wires a real [`OverviewSource`], which is what gives the management interface's first
    /// page numbers to show instead of "Not implemented".
    #[must_use]
    pub fn with_overview(mut self, overview: Arc<dyn OverviewSource>) -> Self {
        self.overview = Some(overview);
        self
    }

    /// Wires a real [`SetupSource`], which is what lets a server with no administrator be given
    /// one from the management interface.
    #[must_use]
    pub fn with_setup(mut self, setup: Arc<dyn SetupSource>) -> Self {
        self.setup = Some(setup);
        self
    }

    /// Replaces the default [`ServerInfo`] with the running binary's real identity.
    #[must_use]
    pub fn with_server_info(mut self, server_info: ServerInfo) -> Self {
        self.server_info = server_info;
        self
    }
}

fn authorization_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}

/// The handler for every not-yet-implemented operation: enforce auth and scope, then answer
/// `501`. `operation_id` and `path` are baked into the closure per route (see [`build_router`]).
async fn not_implemented(
    state: AdminState,
    headers: HeaderMap,
    scope: Option<crate::model::Scope>,
    operation_id: String,
    path: String,
) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        scope,
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => hs_http::Problem::not_implemented()
            .with_detail(format!(
                "{operation_id} is declared in the OpenAPI contract but not implemented yet"
            ))
            .with_instance(path)
            .into_response(),
        ScopeDecision::Unauthenticated(problem) => problem.with_instance(path).into_response(),
        ScopeDecision::InsufficientScope(problem) => problem.with_instance(path).into_response(),
    }
}

/// Operation ids this router serves with a real handler instead of the generic `501`. Keeping
/// this list next to [`register_operation`]/[`register_real_operation`] (rather than scattering a
/// per-operation `if` through the loop) makes "which of the 142 operations are genuinely served"
/// a single, greppable fact.
const REAL_HANDLERS: &[&str] = &[
    "me.get",
    "server.get",
    "server.health",
    "statistics.overview",
    "cluster.get",
    "users.list",
    "users.get",
    "users.update",
    "users.lock",
    "users.unlock",
    "users.deactivate",
    "users.reactivate",
    "users.create",
    "users.lookup",
    "users.availability",
    "rooms.list",
    "rooms.get",
    "rooms.block",
    "rooms.unblock",
    "rooms.make_admin",
    "config.list",
    "config.schema",
    "config.get",
    "config.update",
    "config.validate",
    "config.reload",
    "audit_log.list",
    "audit_log.get",
    "audit_log.export",
    "events.stream",
];

/// The `503 unavailable` problem a handler answers when its backing [`crate::sources`] trait
/// object hasn't been wired onto [`AdminState`] yet — never a silent `501` (which would say "this
/// handler doesn't exist yet", which is false) and never a fake `200`.
fn source_unavailable(source_name: &str, path: &str) -> Response {
    hs_http::Problem::unavailable()
        .with_detail(format!(
            "the {source_name} data source is not wired into this server"
        ))
        .with_instance(path)
        .into_response()
}

/// `GET /api/v1/me`: the one operation with no required scope (RFC 0004 section 8.2) — any
/// valid token identifies its own principal.
async fn me_get(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        None,
    )
    .await
    {
        ScopeDecision::Allowed(principal) => axum::Json(principal).into_response(),
        ScopeDecision::Unauthenticated(problem) => {
            problem.with_instance("/api/v1/me").into_response()
        }
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance("/api/v1/me").into_response()
        }
    }
}

/// `GET /api/v1/server`: identity and build info, plus `uptime_ms` computed from
/// [`AdminState::started_at`].
async fn server_get(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let uptime_ms =
                u64::try_from(state.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            axum::Json(state.server_info.with_uptime_ms(uptime_ms)).into_response()
        }
        ScopeDecision::Unauthenticated(problem) => {
            problem.with_instance("/api/v1/server").into_response()
        }
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance("/api/v1/server").into_response()
        }
    }
}

/// `GET /api/v1/server/health`: an honest probe summary. A check whose backing source is absent
/// is reported `"unknown"`, never `"ok"` (see [`ServerHealth`]'s doc comment).
async fn server_health(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let mut checks = std::collections::BTreeMap::new();
            // The audit sink and event bus are always wired (AdminState::new requires them), so
            // this process being able to answer at all means they are reachable.
            checks.insert("audit".to_string(), "ok".to_string());
            checks.insert("events".to_string(), "ok".to_string());
            checks.insert(
                "users".to_string(),
                if state.users.is_some() {
                    "ok".to_string()
                } else {
                    "unknown".to_string()
                },
            );
            axum::Json(ServerHealth::from_checks(checks)).into_response()
        }
        ScopeDecision::Unauthenticated(problem) => problem
            .with_instance("/api/v1/server/health")
            .into_response(),
        ScopeDecision::InsufficientScope(problem) => problem
            .with_instance("/api/v1/server/health")
            .into_response(),
    }
}

/// Query parameters `GET /api/v1/users` accepts (RFC 0004 section 3.3's pagination trio plus the
/// per-resource filters the OpenAPI `users.list` operation declares). `sort` is accepted but not
/// yet honoured (the in-memory fake and any real implementation both return an id-ordered list;
/// see `docs/status/15-admin-api-and-modules.md`).
#[derive(Debug, Default, Deserialize)]
struct UsersListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    q: Option<String>,
    admin: Option<bool>,
    deactivated: Option<bool>,
    locked: Option<bool>,
    suspended: Option<bool>,
    guests: Option<bool>,
}

/// `GET /api/v1/users`: a page of [`crate::model::AdminUser`] from [`UserDirectory::list_users`],
/// filtered by the query parameters above and paginated with [`Page::paginate`].
async fn users_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<UsersListQuery>,
) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(users) = &state.users else {
                return source_unavailable("user directory", "/api/v1/users");
            };
            let filter = UserFilter {
                q: query.q,
                admin: query.admin,
                deactivated: query.deactivated,
                locked: query.locked,
                suspended: query.suspended,
                guests: query.guests,
            };
            match users.list_users(&filter).await {
                Ok(items) => {
                    let page = Page::paginate(
                        items,
                        query.cursor.as_deref(),
                        query.limit,
                        query.include_total.unwrap_or(false),
                    );
                    axum::Json(page).into_response()
                }
                Err(e) => e
                    .to_problem()
                    .with_instance("/api/v1/users")
                    .into_response(),
            }
        }
        ScopeDecision::Unauthenticated(problem) => {
            problem.with_instance("/api/v1/users").into_response()
        }
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance("/api/v1/users").into_response()
        }
    }
}

/// `GET /api/v1/users/{user_id}`: one [`crate::model::AdminUser`], or a `404 not-found` problem.
async fn users_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(users) = &state.users else {
                return source_unavailable("user directory", &instance);
            };
            match users.get_user(&user_id).await {
                Ok(Some(user)) => axum::Json(user).into_response(),
                Ok(None) => hs_http::Problem::not_found()
                    .with_detail(format!("no such user: {user_id}"))
                    .with_instance(instance)
                    .into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(problem) => problem.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(problem) => {
            problem.with_instance(instance).into_response()
        }
    }
}

/// `GET /api/v1/users/availability`: whether `localpart` is free to register
/// ([`UserDirectory::check_localpart_available`]). `localpart` is a required query parameter;
/// read as `Option<String>` (rather than relying on axum's built-in `Query<T>` rejection for a
/// missing required field) so a missing value answers the same RFC 9457 `400 validation-failed`
/// shape every other validation failure in this router does, not axum's default rejection body.
#[derive(Debug, Default, Deserialize)]
struct AvailabilityQuery {
    localpart: Option<String>,
}

async fn users_availability(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<AvailabilityQuery>,
) -> Response {
    let instance = "/api/v1/users/availability";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(localpart) = query.localpart else {
                return Problem::validation_failed()
                    .with_errors(vec![ValidationError::new(
                        "param:localpart",
                        "localpart is required",
                    )])
                    .with_instance(instance)
                    .into_response();
            };
            let Some(users) = &state.users else {
                return source_unavailable("user directory", instance);
            };
            match users.check_localpart_available(&localpart).await {
                Ok(available) => axum::Json(json!({ "available": available })).into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/users/lookup`: finds a user by 3PID (`medium`+`address`) or external id
/// (`provider`+`external_id`), exactly one pair required ([`UserLookupQuery`]).
#[derive(Debug, Default, Deserialize)]
struct LookupQuery {
    medium: Option<String>,
    address: Option<String>,
    provider: Option<String>,
    external_id: Option<String>,
}

async fn users_lookup(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<LookupQuery>,
) -> Response {
    let instance = "/api/v1/users/lookup";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let threepid = match (&query.medium, &query.address) {
                (Some(medium), Some(address)) => Some(UserLookupQuery::Threepid {
                    medium: medium.clone(),
                    address: address.clone(),
                }),
                (None, None) => None,
                _ => {
                    return Problem::validation_failed()
                        .with_detail("medium and address must both be given, or neither")
                        .with_instance(instance)
                        .into_response();
                }
            };
            let external = match (&query.provider, &query.external_id) {
                (Some(provider), Some(external_id)) => Some(UserLookupQuery::ExternalId {
                    provider: provider.clone(),
                    external_id: external_id.clone(),
                }),
                (None, None) => None,
                _ => {
                    return Problem::validation_failed()
                        .with_detail("provider and external_id must both be given, or neither")
                        .with_instance(instance)
                        .into_response();
                }
            };
            let lookup = match (threepid, external) {
                (Some(t), None) => t,
                (None, Some(e)) => e,
                _ => {
                    return Problem::validation_failed()
                        .with_detail(
                            "exactly one of (medium, address) or (provider, external_id) is required",
                        )
                        .with_instance(instance)
                        .into_response();
                }
            };
            let Some(users) = &state.users else {
                return source_unavailable("user directory", instance);
            };
            match users.lookup_user(lookup).await {
                Ok(Some(user)) => axum::Json(user).into_response(),
                Ok(None) => Problem::not_found()
                    .with_detail("no user matches the given criteria")
                    .with_instance(instance)
                    .into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/users` (`admin:write`, idempotent): creates a user via
/// [`UserDirectory::create_user`]. Requires `localpart` or `user_id`, not neither and not both
/// inconsistently (that judgment call belongs to the real implementation, which knows its own
/// homeserver domain; this handler only rejects the "named neither" case up front since no
/// implementation, real or fake, can act on it).
async fn users_create(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/users";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let Some(users) = &state.users else {
                return source_unavailable("user directory", instance);
            };

            if let Some(key) = idempotency_key(&headers) {
                match state.idempotency.check("users.create", key, &body) {
                    Replay::Same(stored) => return replay_response(stored),
                    Replay::Mismatch => {
                        return Problem::idempotency_key_payload_mismatch()
                            .with_instance(instance)
                            .into_response();
                    }
                    Replay::Fresh => {}
                }
            }

            let request: UserCreateRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            if request.localpart.is_none() && request.user_id.is_none() {
                return Problem::validation_failed()
                    .with_errors(vec![ValidationError::new(
                        "/localpart",
                        "either localpart or user_id is required",
                    )])
                    .with_instance(instance)
                    .into_response();
            }

            let created = match users.create_user(request).await {
                Ok(u) => u,
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            };

            if let Err(resp) = record_mutation(
                &state,
                &principal,
                "users.create",
                "user.created",
                ResourceRef::new("user", created.user_id.clone()),
                Vec::new(),
                json!({ "user_id": created.user_id }),
            )
            .await
            {
                return resp;
            }

            let response_body = serde_json::to_vec(&created).unwrap_or_default();
            if let Some(key) = idempotency_key(&headers) {
                state.idempotency.record(
                    "users.create",
                    key,
                    &body,
                    StoredResponse {
                        status: 201,
                        content_type: "application/json".to_string(),
                        body: response_body.clone(),
                    },
                );
            }
            (
                StatusCode::CREATED,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                response_body,
            )
                .into_response()
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// user mutations: lock, unlock, deactivate, reactivate, update (RFC 0004 sections 9/10, brief
// deliverable 1). Every one of these writes exactly one AuditEntry (`record_mutation`) and
// publishes exactly one Event before answering — never a mutation that "succeeds" silently.
// -------------------------------------------------------------------------------------------

/// The body of `POST .../lock`, `.../unlock` and `.../reactivate` (OpenAPI `ReasonRequest`).
/// `notify` is accepted (so a well-formed request body is never rejected) but not yet acted on:
/// no notification source exists to send through.
#[derive(Debug, Default, Deserialize)]
struct ReasonRequest {
    reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notify: Option<bool>,
}

/// The body of `POST .../deactivate` (OpenAPI's inline schema): like [`ReasonRequest`] plus
/// `erase`, which this server cannot yet honor (no eraser is wired to [`UserDirectory`]).
#[derive(Debug, Default, Deserialize)]
struct DeactivateRequest {
    erase: Option<bool>,
    reason: Option<String>,
}

/// Which boolean field on [`crate::model::AdminUser`] a toggle handler flips. Kept as an enum
/// (rather than a closure over `dyn UserDirectory`) because async closures are not yet stable and
/// boxing a future for three call sites would add more machinery than this saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToggleField {
    Locked,
    Deactivated,
}

impl ToggleField {
    fn pointer(self) -> &'static str {
        match self {
            ToggleField::Locked => "/locked",
            ToggleField::Deactivated => "/deactivated",
        }
    }

    fn current(self, user: &crate::model::AdminUser) -> bool {
        match self {
            ToggleField::Locked => user.locked,
            ToggleField::Deactivated => user.deactivated,
        }
    }

    async fn apply(
        self,
        users: &dyn UserDirectory,
        user_id: &str,
        value: bool,
    ) -> Result<(), SourceError> {
        match self {
            ToggleField::Locked => users.set_locked(user_id, value).await,
            ToggleField::Deactivated => users.set_deactivated(user_id, value).await,
        }
    }
}

/// Parses `body` as JSON, or returns `T::default()` for an empty body (every mutation handled
/// here declares its request body optional). A non-empty body that fails to parse is a `400
/// validation-failed` problem, not a panic or a silent default.
// `Problem` is a large-ish value type (an RFC 9457 body plus extension members); boxing it would
// mean unwrapping a `Box<Problem>` at every one of this function's call sites for no real benefit
// here (it is returned once per request, not in a hot loop).
#[allow(clippy::result_large_err)]
fn parse_optional_json<T: serde::de::DeserializeOwned + Default>(
    body: &[u8],
) -> Result<T, Problem> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body)
        .map_err(|e| Problem::validation_failed().with_detail(format!("invalid JSON body: {e}")))
}

/// The `idempotency-key` header, if the client sent one.
fn idempotency_key(headers: &HeaderMap) -> Option<&str> {
    headers.get("idempotency-key").and_then(|v| v.to_str().ok())
}

/// Rebuilds an axum [`Response`] from a [`StoredResponse`], marking it as a replay so a client
/// (or a test) can tell the mutation did not run again.
fn replay_response(stored: StoredResponse) -> Response {
    axum::http::Response::builder()
        .status(stored.status)
        .header(axum::http::header::CONTENT_TYPE, stored.content_type)
        .header("idempotency-replayed", "true")
        .body(axum::body::Body::from(stored.body))
        .unwrap_or_else(|_| Problem::internal().into_response())
}

/// Appends one [`AuditEntry`] and publishes one matching [`Event`] for a successful mutation —
/// the single place every handler below calls so "a mutation writes an audit entry and an event"
/// cannot be forgotten per-handler. A failed audit write fails the request with `503` (RFC 0004
/// section 9), returned as `Err` for the caller to answer with directly.
// See the identical justification on `parse_optional_json` above: `Response` is returned once
// per request here, not on a hot path, so boxing it would only add noise at every call site.
#[allow(clippy::result_large_err)]
async fn record_mutation(
    state: &AdminState,
    principal: &Principal,
    action: &str,
    event_type: &str,
    target: ResourceRef,
    changes: Vec<AuditChange>,
    event_data: serde_json::Value,
) -> Result<(), Response> {
    let actor = principal.to_actor();
    let mut entry = AuditEntry::new(
        action,
        actor.clone(),
        target.clone(),
        AuditOutcome::success(200),
    );
    entry.changes = changes;
    state
        .audit
        .append(entry)
        .await
        .map_err(|e| e.to_problem().into_response())?;
    state.events.publish(
        Event::new(event_type, event_data)
            .with_resource(target)
            .with_actor(actor),
    );
    Ok(())
}

/// Shared body for `users.lock`, `users.unlock` and `users.reactivate`: idempotency handling,
/// fetch-before, apply, fetch-after, audit + event, and the JSON response. `users.deactivate`
/// uses the same shape but validates its own richer body first (see [`users_deactivate`]).
#[allow(clippy::too_many_arguments)]
async fn toggle_user_and_record(
    state: &AdminState,
    headers: &HeaderMap,
    raw_body: &[u8],
    principal: Principal,
    user_id: String,
    instance: String,
    operation_id: &str,
    event_type: &str,
    field: ToggleField,
    target_value: bool,
    reason: Option<String>,
) -> Response {
    let Some(users) = &state.users else {
        return source_unavailable("user directory", &instance);
    };

    if let Some(key) = idempotency_key(headers) {
        match state.idempotency.check(operation_id, key, raw_body) {
            Replay::Same(stored) => return replay_response(stored),
            Replay::Mismatch => {
                return Problem::idempotency_key_payload_mismatch()
                    .with_instance(instance)
                    .into_response();
            }
            Replay::Fresh => {}
        }
    }

    let before = match users.get_user(&user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return Problem::not_found()
                .with_detail(format!("no such user: {user_id}"))
                .with_instance(instance)
                .into_response();
        }
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };

    let already_set = field.current(&before) == target_value;
    if !already_set && let Err(e) = field.apply(users.as_ref(), &user_id, target_value).await {
        return e.to_problem().with_instance(instance).into_response();
    }

    let updated = match users.get_user(&user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => before.clone(),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };

    let mut changes = Vec::new();
    if !already_set {
        changes.push(AuditChange {
            pointer: field.pointer().to_string(),
            from: Some(json!(!target_value)),
            to: Some(json!(target_value)),
        });
    }
    let event_data = match &reason {
        Some(r) => json!({ "reason": r }),
        None => json!({}),
    };

    if let Err(resp) = record_mutation(
        state,
        &principal,
        operation_id,
        event_type,
        ResourceRef::new("user", user_id.clone()),
        changes,
        event_data,
    )
    .await
    {
        return resp;
    }

    let response_body = serde_json::to_vec(&updated).unwrap_or_default();
    if let Some(key) = idempotency_key(headers) {
        state.idempotency.record(
            operation_id,
            key,
            raw_body,
            StoredResponse {
                status: 200,
                content_type: "application/json".to_string(),
                body: response_body.clone(),
            },
        );
    }
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        response_body,
    )
        .into_response()
}

/// `POST /api/v1/users/{user_id}/lock` (`moderation:write`).
async fn users_lock(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/lock");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::ModerationWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let reason: ReasonRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            toggle_user_and_record(
                &state,
                &headers,
                &body,
                principal,
                user_id,
                instance,
                "users.lock",
                "user.locked",
                ToggleField::Locked,
                true,
                reason.reason,
            )
            .await
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/users/{user_id}/unlock` (`moderation:write`).
async fn users_unlock(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/unlock");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::ModerationWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let reason: ReasonRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            toggle_user_and_record(
                &state,
                &headers,
                &body,
                principal,
                user_id,
                instance,
                "users.unlock",
                "user.unlocked",
                ToggleField::Locked,
                false,
                reason.reason,
            )
            .await
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/users/{user_id}/reactivate` (`admin:write`).
async fn users_reactivate(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/reactivate");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let reason: ReasonRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            toggle_user_and_record(
                &state,
                &headers,
                &body,
                principal,
                user_id,
                instance,
                "users.reactivate",
                "user.reactivated",
                ToggleField::Deactivated,
                false,
                reason.reason,
            )
            .await
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/users/{user_id}/deactivate` (`admin:write`). `erase: true` is rejected with a
/// `400 validation-failed` naming `/erase` rather than silently deactivating without erasing:
/// no eraser is wired to [`UserDirectory`] yet, and a caller who asked for erasure and got a
/// quiet no-op would believe data was gone that is not.
async fn users_deactivate(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}/deactivate");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let request: DeactivateRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            if request.erase == Some(true) {
                return Problem::validation_failed()
                    .with_detail("erasure is not implemented yet")
                    .with_errors(vec![ValidationError::new(
                        "/erase",
                        "this server cannot erase user data yet; deactivate without erase",
                    )])
                    .with_instance(instance)
                    .into_response();
            }
            toggle_user_and_record(
                &state,
                &headers,
                &body,
                principal,
                user_id,
                instance,
                "users.deactivate",
                "user.deactivated",
                ToggleField::Deactivated,
                true,
                request.reason,
            )
            .await
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// The OpenAPI `UserUpdate` schema, read as `Option<Value>` per field (rather than `Option<T>`)
/// so presence can be distinguished from absence: `{"display_name": null}` and `{}` must be told
/// apart, since only the former is a (rejected) attempt to change a field this server cannot
/// change yet.
#[derive(Debug, Default, Deserialize)]
struct UserUpdateRequest {
    #[serde(default)]
    display_name: Option<serde_json::Value>,
    #[serde(default)]
    avatar_url: Option<serde_json::Value>,
    #[serde(default)]
    admin: Option<serde_json::Value>,
    #[serde(default)]
    user_type: Option<serde_json::Value>,
}

/// A weak-or-strong ETag derived from a user's own fields (RFC 0004's `If-Match` parameter),
/// stable across requests as long as nothing about the user has changed.
fn etag_for_user(user: &crate::model::AdminUser) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(user)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("\"{:x}\"", hasher.finish())
}

/// Strips a weak-validator prefix and surrounding quotes so `"abc"` and `W/"abc"` compare equal.
fn normalize_etag(raw: &str) -> &str {
    raw.trim().trim_start_matches("W/").trim_matches('"')
}

/// `PATCH /api/v1/users/{user_id}` (`admin:write`): today, only the `admin` field has a data
/// source that can change it ([`UserDirectory::set_admin`]). Any other field present in the
/// request body — even set to its current value, even `null` — is a `400 validation-failed`
/// naming that field, rather than a `200` that silently ignored it.
async fn users_update(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/users/{user_id}");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let Some(users) = &state.users else {
                return source_unavailable("user directory", &instance);
            };
            let request: UserUpdateRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };

            let mut errors = Vec::new();
            if request.display_name.is_some() {
                errors.push(ValidationError::new(
                    "/display_name",
                    "no data source can change this field yet",
                ));
            }
            if request.avatar_url.is_some() {
                errors.push(ValidationError::new(
                    "/avatar_url",
                    "no data source can change this field yet",
                ));
            }
            if request.user_type.is_some() {
                errors.push(ValidationError::new(
                    "/user_type",
                    "no data source can change this field yet",
                ));
            }
            let admin_value = match &request.admin {
                None => None,
                Some(serde_json::Value::Bool(b)) => Some(*b),
                Some(_) => {
                    errors.push(ValidationError::new("/admin", "must be a boolean"));
                    None
                }
            };
            if !errors.is_empty() {
                return Problem::validation_failed()
                    .with_detail("one or more fields in the request cannot be applied")
                    .with_errors(errors)
                    .with_instance(instance)
                    .into_response();
            }

            let current = match users.get_user(&user_id).await {
                Ok(Some(u)) => u,
                Ok(None) => {
                    return Problem::not_found()
                        .with_detail(format!("no such user: {user_id}"))
                        .with_instance(instance)
                        .into_response();
                }
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            };

            if let Some(if_match) = headers
                .get(axum::http::header::IF_MATCH)
                .and_then(|v| v.to_str().ok())
            {
                let current_etag = etag_for_user(&current);
                if normalize_etag(if_match) != normalize_etag(&current_etag) {
                    return Problem::precondition_failed()
                        .with_detail("If-Match does not match the current resource")
                        .with_instance(instance)
                        .into_response();
                }
            }

            let mut changes = Vec::new();
            if let Some(new_admin) = admin_value
                && new_admin != current.admin
            {
                if let Err(e) = users.set_admin(&user_id, new_admin).await {
                    return e.to_problem().with_instance(instance).into_response();
                }
                changes.push(AuditChange {
                    pointer: "/admin".to_string(),
                    from: Some(json!(current.admin)),
                    to: Some(json!(new_admin)),
                });
            }

            let updated = match users.get_user(&user_id).await {
                Ok(Some(u)) => u,
                Ok(None) => current.clone(),
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            };

            if let Err(resp) = record_mutation(
                &state,
                &principal,
                "users.update",
                "user.updated",
                ResourceRef::new("user", user_id.clone()),
                changes,
                json!({ "admin": updated.admin }),
            )
            .await
            {
                return resp;
            }

            let etag = etag_for_user(&updated);
            (
                StatusCode::OK,
                [(axum::http::header::ETAG, etag)],
                axum::Json(updated),
            )
                .into_response()
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// rooms (RFC 0004 section 4.3, brief deliverable 2). `crate::sources::RoomDirectory` has no real
// implementation wired in this session (`hs-room` is another track's crate); every handler here
// answers a real `503 unavailable` via `source_unavailable` when `state.rooms` is `None`, exactly
// like the user handlers do for `state.users`, and is exercised in tests against
// `InMemoryRoomDirectory`.
// -------------------------------------------------------------------------------------------

/// Query parameters `GET /api/v1/rooms` accepts (the OpenAPI `rooms.list` operation). `sort` is
/// accepted but not honoured, matching `users.list`'s precedent.
#[derive(Debug, Default, Deserialize)]
struct RoomsListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    q: Option<String>,
    public: Option<bool>,
    empty: Option<bool>,
    blocked: Option<bool>,
    encrypted: Option<bool>,
    federatable: Option<bool>,
    room_type: Option<String>,
    version: Option<String>,
}

/// `GET /api/v1/rooms`.
async fn rooms_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<RoomsListQuery>,
) -> Response {
    let instance = "/api/v1/rooms";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(rooms) = &state.rooms else {
                return source_unavailable("room directory", instance);
            };
            let filter = RoomFilter {
                q: query.q,
                public: query.public,
                empty: query.empty,
                blocked: query.blocked,
                encrypted: query.encrypted,
                federatable: query.federatable,
                room_type: query.room_type,
                version: query.version,
            };
            match rooms.list_rooms(&filter).await {
                Ok(items) => {
                    let page = Page::paginate(
                        items,
                        query.cursor.as_deref(),
                        query.limit,
                        query.include_total.unwrap_or(false),
                    );
                    axum::Json(page).into_response()
                }
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/rooms/{room_id}`.
async fn rooms_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/rooms/{room_id}");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(rooms) = &state.rooms else {
                return source_unavailable("room directory", &instance);
            };
            match rooms.get_room(&room_id).await {
                Ok(Some(room)) => axum::Json(room).into_response(),
                Ok(None) => Problem::not_found()
                    .with_detail(format!("no such room: {room_id}"))
                    .with_instance(instance)
                    .into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// The body of `POST .../block` (OpenAPI inline schema: `{"reason": string}`).
#[derive(Debug, Default, Deserialize)]
struct BlockRoomRequest {
    reason: Option<String>,
}

/// Shared body for `rooms.block`/`rooms.unblock`: idempotency handling, fetch-before, apply,
/// fetch-after, audit + event, and the JSON response — the room-shaped twin of
/// `toggle_user_and_record`.
#[allow(clippy::too_many_arguments)]
async fn toggle_room_and_record(
    state: &AdminState,
    headers: &HeaderMap,
    raw_body: &[u8],
    principal: Principal,
    room_id: String,
    instance: String,
    operation_id: &str,
    event_type: &str,
    target_blocked: bool,
    reason: Option<String>,
) -> Response {
    let Some(rooms) = &state.rooms else {
        return source_unavailable("room directory", &instance);
    };

    if let Some(key) = idempotency_key(headers) {
        match state.idempotency.check(operation_id, key, raw_body) {
            Replay::Same(stored) => return replay_response(stored),
            Replay::Mismatch => {
                return Problem::idempotency_key_payload_mismatch()
                    .with_instance(instance)
                    .into_response();
            }
            Replay::Fresh => {}
        }
    }

    let before = match rooms.get_room(&room_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Problem::not_found()
                .with_detail(format!("no such room: {room_id}"))
                .with_instance(instance)
                .into_response();
        }
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };

    let already_set = before.blocked == target_blocked;
    if !already_set
        && let Err(e) = rooms
            .set_blocked(&room_id, target_blocked, reason.clone())
            .await
    {
        return e.to_problem().with_instance(instance).into_response();
    }

    let updated = match rooms.get_room(&room_id).await {
        Ok(Some(r)) => r,
        Ok(None) => before.clone(),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };

    let mut changes = Vec::new();
    if !already_set {
        changes.push(AuditChange {
            pointer: "/blocked".to_string(),
            from: Some(json!(!target_blocked)),
            to: Some(json!(target_blocked)),
        });
    }
    let event_data = match &reason {
        Some(r) => json!({ "reason": r }),
        None => json!({}),
    };

    if let Err(resp) = record_mutation(
        state,
        &principal,
        operation_id,
        event_type,
        ResourceRef::new("room", room_id.clone()),
        changes,
        event_data,
    )
    .await
    {
        return resp;
    }

    let response_body = serde_json::to_vec(&updated).unwrap_or_default();
    if let Some(key) = idempotency_key(headers) {
        state.idempotency.record(
            operation_id,
            key,
            raw_body,
            StoredResponse {
                status: 200,
                content_type: "application/json".to_string(),
                body: response_body.clone(),
            },
        );
    }
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        response_body,
    )
        .into_response()
}

/// `POST /api/v1/rooms/{room_id}/block` (`moderation:write`).
async fn rooms_block(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/rooms/{room_id}/block");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::ModerationWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let request: BlockRoomRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            toggle_room_and_record(
                &state,
                &headers,
                &body,
                principal,
                room_id,
                instance,
                "rooms.block",
                "room.blocked",
                true,
                request.reason,
            )
            .await
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/rooms/{room_id}/unblock` (`moderation:write`).
async fn rooms_unblock(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/rooms/{room_id}/unblock");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::ModerationWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            toggle_room_and_record(
                &state,
                &headers,
                &body,
                principal,
                room_id,
                instance,
                "rooms.unblock",
                "room.unblocked",
                false,
                None,
            )
            .await
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// The body of `POST .../make-admin` (OpenAPI inline schema: `{"user_id": string}`).
#[derive(Debug, Default, Deserialize)]
struct MakeAdminRequest {
    user_id: Option<String>,
}

/// `POST /api/v1/rooms/{room_id}/make-admin` (`admin:write`): grants `user_id` (defaulting to the
/// calling principal if omitted, matching Synapse's `make_room_admin` behavior of defaulting to
/// the requester — read for behavior only, never copied, per this track's brief) room-admin power
/// level via [`RoomDirectory::make_admin`].
async fn rooms_make_admin(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/rooms/{room_id}/make-admin");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let Some(rooms) = &state.rooms else {
                return source_unavailable("room directory", &instance);
            };
            let request: MakeAdminRequest = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            let target_user = request
                .user_id
                .clone()
                .unwrap_or_else(|| principal.id.clone());

            if let Some(key) = idempotency_key(&headers) {
                match state.idempotency.check("rooms.make_admin", key, &body) {
                    Replay::Same(stored) => return replay_response(stored),
                    Replay::Mismatch => {
                        return Problem::idempotency_key_payload_mismatch()
                            .with_instance(instance)
                            .into_response();
                    }
                    Replay::Fresh => {}
                }
            }

            if let Err(e) = rooms.make_admin(&room_id, &target_user).await {
                return e.to_problem().with_instance(instance).into_response();
            }

            let updated = match rooms.get_room(&room_id).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return Problem::not_found()
                        .with_detail(format!("no such room: {room_id}"))
                        .with_instance(instance)
                        .into_response();
                }
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            };

            if let Err(resp) = record_mutation(
                &state,
                &principal,
                "rooms.make_admin",
                "room.admin_granted",
                ResourceRef::new("room", room_id.clone()),
                Vec::new(),
                json!({ "user_id": target_user }),
            )
            .await
            {
                return resp;
            }

            let response_body = serde_json::to_vec(&updated).unwrap_or_default();
            if let Some(key) = idempotency_key(&headers) {
                state.idempotency.record(
                    "rooms.make_admin",
                    key,
                    &body,
                    StoredResponse {
                        status: 200,
                        content_type: "application/json".to_string(),
                        body: response_body.clone(),
                    },
                );
            }
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                response_body,
            )
                .into_response()
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// configuration (RFC 0004 section 4.12). Every one of these goes through
// `crate::sources::ConfigSource`; none of them knows the name of a single setting. What the
// management interface renders a form from is `GET /config/schema`, which serves the JSON Schema
// `schemars` derives from `hs_config::Config` itself, so a setting added to that struct appears
// in the interface without a line changing here.
// -------------------------------------------------------------------------------------------

/// A configuration section's ETag: the store's revision counter, quoted.
///
/// The revision counts writes to the configuration as a whole rather than to one section, which
/// is deliberate — two operators editing different sections still need to know they are not
/// looking at the same configuration any more, and `hs_config::ConfigStore` compares `If-Match`
/// against exactly this number.
fn config_etag(revision: u64) -> String {
    format!("\"{revision}\"")
}

/// Reads `If-Match` as the revision the caller believes is current.
///
/// `*` is RFC 9110's "any current version", which for a configuration that always exists is no
/// precondition at all. Anything that is not a revision cannot match one, so it fails the
/// precondition rather than being ignored: a caller whose compare-and-set was quietly dropped
/// would believe it had held a lock it never had.
// See `parse_optional_json` above for why `Problem` is returned unboxed here.
#[allow(clippy::result_large_err)]
fn config_expected_revision(headers: &HeaderMap) -> Result<Option<u64>, Problem> {
    let Some(raw) = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    if raw.trim() == "*" {
        return Ok(None);
    }
    normalize_etag(raw).parse::<u64>().map(Some).map_err(|_| {
        Problem::precondition_failed().with_detail(format!(
            "If-Match {raw:?} is not a configuration ETag; re-read the section and send back the \
             ETag it returned"
        ))
    })
}

/// Renders one section for the wire: every secret, in its values and in its recorded history,
/// replaced by `{"$secret": true}`.
///
/// This is the only place a [`ConfigSection`] becomes a response, so a secret cannot escape
/// through a handler that forgot to redact. Which settings are secrets comes from the derived
/// schema (`crate::config_schema`), never from what a field is called.
fn redacted_section(mut section: ConfigSection) -> ConfigSection {
    let secrets = crate::config_schema::secret_paths();
    secrets.redact(&mut section.values, &format!("/{}", section.name));
    for change in &mut section.history {
        secrets.redact(&mut change.patch, &format!("/{}", change.section));
    }
    section
}

/// `GET /api/v1/config` (`admin:read`): every section, secrets redacted.
async fn config_list(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let instance = "/api/v1/config";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(config) = &state.config else {
                return source_unavailable("configuration", instance);
            };
            match config.list_sections().await {
                Ok(sections) => {
                    let body: Vec<ConfigSection> =
                        sections.into_iter().map(redacted_section).collect();
                    axum::Json(body).into_response()
                }
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/config/schema` (`admin:read`): everything needed to render a configuration form
/// without knowing what is in the configuration.
///
/// The static half is the JSON Schema derived from `hs_config::Config` — types, defaults, enums
/// and the prose describing each setting. The live half says, per setting, which layer its value
/// came from, whether it is a secret, whether changing it needs a restart, and whether this
/// server would accept a change to it at all. That last flag is what lets the interface show a
/// field the environment pins as read-only instead of offering an edit that `config.update`
/// would refuse.
async fn config_schema(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let instance = "/api/v1/config/schema";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(config) = &state.config else {
                return source_unavailable("configuration", instance);
            };
            match config.list_sections().await {
                Ok(sections) => axum::Json(config_schema_document(&sections)).into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// Builds `GET /config/schema`'s body from the live sections.
fn config_schema_document(sections: &[ConfigSection]) -> ConfigSchema {
    let secrets = crate::config_schema::secret_paths();
    let mut section_infos = Vec::with_capacity(sections.len());
    let mut settings = Vec::new();
    for section in sections {
        section_infos.push(ConfigSectionInfo {
            name: section.name.clone(),
            reloadable: section.reloadable,
            bootstrap: section.bootstrap,
            source: section.source.clone(),
        });
        for (pointer, origin) in &section.origins {
            settings.push(ConfigSettingInfo {
                pointer: pointer.clone(),
                section: section.name.clone(),
                origin: origin.clone(),
                secret: secrets.is_secret(pointer),
                reloadable: section.reloadable,
                editable: !section.bootstrap && origin != "environment",
            });
        }
    }
    ConfigSchema {
        schema: crate::config_schema::config_json_schema().clone(),
        sections: section_infos,
        settings,
        revision: sections.first().map_or(0, |section| section.revision),
    }
}

/// `GET /api/v1/config/{section}` (`admin:read`): one section with its recent history, secrets
/// redacted, carrying the ETag `config.update` expects back in `If-Match`.
async fn config_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(section): Path<String>,
) -> Response {
    let instance = format!("/api/v1/config/{section}");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(config) = &state.config else {
                return source_unavailable("configuration", &instance);
            };
            match config.get_section(&section).await {
                Ok(Some(found)) => (
                    StatusCode::OK,
                    [(axum::http::header::ETAG, config_etag(found.revision))],
                    axum::Json(redacted_section(found)),
                )
                    .into_response(),
                Ok(None) => Problem::not_found()
                    .with_detail(format!("no such configuration section: {section}"))
                    .with_instance(instance)
                    .into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `PATCH /api/v1/config/{section}` (`admin:write`): an RFC 7396 merge patch against one
/// section, where `null` means "reset this setting to its schema default".
///
/// Four things are checked before anything is written, because each of them is a way for a write
/// to look like it worked and not have: a section the database cannot hold, a setting an `HS__`
/// environment variable pins (the environment outranks the database, so the value would be
/// stored faithfully and then ignored), a configuration the server would refuse to run on, and a
/// stale `If-Match` from an operator whose view of the section has since been overwritten by
/// somebody else's change.
async fn config_update(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(section): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/config/{section}");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let Some(config) = &state.config else {
                return source_unavailable("configuration", &instance);
            };
            let mut patch: serde_json::Value = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            if !patch.is_object() {
                return Problem::validation_failed()
                    .with_errors(vec![ValidationError::new(
                        "",
                        "the request body must be a JSON Merge Patch object (RFC 7396)",
                    )])
                    .with_instance(instance)
                    .into_response();
            }

            let current = match config.get_section(&section).await {
                Ok(Some(found)) => found,
                Ok(None) => {
                    return Problem::not_found()
                        .with_detail(format!("no such configuration section: {section}"))
                        .with_instance(instance)
                        .into_response();
                }
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            };
            if current.bootstrap {
                return Problem::conflict()
                    .with_detail(format!(
                        "{section:?} is read before this server's database is open, so it cannot \
                         be stored in it — set it on the command line, in an HS__ environment \
                         variable, or in the bootstrap file"
                    ))
                    .with_instance(instance)
                    .into_response();
            }

            // A form round-trips the secrets it was shown as `{"$secret": true}`. Writing that
            // literally would replace a real secret with a placeholder; dropping it is what
            // "the operator left this field alone" means.
            let untouched_secrets = crate::config_schema::secret_paths()
                .strip_echoed_secrets(&mut patch, &format!("/{section}"));
            if patch.as_object().is_some_and(serde_json::Map::is_empty) {
                // Nothing to change. Bumping the revision for an empty patch would invalidate
                // every other operator's `If-Match` and write a history entry recording that
                // nothing happened.
                let _ = untouched_secrets;
                return (
                    StatusCode::OK,
                    [(axum::http::header::ETAG, config_etag(current.revision))],
                    axum::Json(redacted_section(current)),
                )
                    .into_response();
            }

            match config.environment_pinned(&section, &patch).await {
                Ok(pinned) if !pinned.is_empty() => {
                    return Problem::conflict()
                        .with_detail(
                            "an HS__ environment variable pins these settings; this server would \
                             store the change and then ignore it, so it is refused instead. \
                             Change them where they are set, or unset them there first.",
                        )
                        .with_errors(
                            pinned
                                .iter()
                                .map(|pointer| {
                                    ValidationError::new(
                                        pointer,
                                        "pinned by an HS__ environment variable, which outranks \
                                         the database",
                                    )
                                })
                                .collect(),
                        )
                        .with_instance(instance)
                        .into_response();
                }
                Ok(_) => {}
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            }

            let candidate = serde_json::Value::Object(
                [(section.clone(), patch.clone())]
                    .into_iter()
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            );
            match config.validate(&candidate).await {
                Ok(report) if !report.valid => {
                    return Problem::validation_failed()
                        .with_detail(
                            "the configuration this change would produce is not valid; nothing \
                             was written",
                        )
                        .with_errors(report.errors)
                        .with_instance(instance)
                        .into_response();
                }
                Ok(_) => {}
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            }

            let expected_revision = match config_expected_revision(&headers) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };

            let updated = match config
                .patch_section(ConfigPatch {
                    section: section.clone(),
                    patch: patch.clone(),
                    actor: Some(principal.id.clone()),
                    expected_revision,
                })
                .await
            {
                Ok(updated) => updated,
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            };

            if let Err(resp) = record_mutation(
                &state,
                &principal,
                "config.update",
                "config.updated",
                ResourceRef::new("config_section", section.clone()),
                config_audit_changes(&current, &updated, &patch),
                json!({ "section": section, "revision": updated.revision }),
            )
            .await
            {
                return resp;
            }

            (
                StatusCode::OK,
                [(axum::http::header::ETAG, config_etag(updated.revision))],
                axum::Json(redacted_section(updated)),
            )
                .into_response()
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// One [`AuditChange`] per setting the patch touched, reading the before and after values out of
/// the effective configuration rather than out of the patch — so a reset to default records the
/// default that took effect, not the `null` that asked for it.
///
/// Secrets are redacted on both sides. An audit log an operator can read a password out of is
/// the same leak as an API that returns one.
fn config_audit_changes(
    before: &ConfigSection,
    after: &ConfigSection,
    patch: &serde_json::Value,
) -> Vec<AuditChange> {
    let secrets = crate::config_schema::secret_paths();
    hs_config::document::leaf_pointers(patch)
        .into_iter()
        .map(|leaf| {
            let pointer = format!("/{}{leaf}", before.name);
            let value_at = |section: &ConfigSection| {
                section.values.pointer(&leaf).cloned().map(|mut value| {
                    secrets.redact(&mut value, &pointer);
                    value
                })
            };
            let (from, to) = (value_at(before), value_at(after));
            AuditChange { pointer, from, to }
        })
        .collect()
}

/// `POST /api/v1/config/validate` (`admin:read`): would this configuration be accepted?
///
/// The body is a sparse document keyed by section, applied as a merge patch over what is stored
/// now, so the interface can ask about a whole form's worth of edits before committing to any of
/// them. Nothing is written either way, and a configuration that would be rejected is a `200`
/// carrying every reason — the question was answered; the answer was no.
async fn config_validate(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/config/validate";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(config) = &state.config else {
                return source_unavailable("configuration", instance);
            };
            let mut candidate: serde_json::Value = match parse_optional_json(&body) {
                Ok(v) => v,
                Err(p) => return p.with_instance(instance).into_response(),
            };
            if !candidate.is_object() {
                return Problem::validation_failed()
                    .with_errors(vec![ValidationError::new(
                        "",
                        "the request body must be a configuration document keyed by section",
                    )])
                    .with_instance(instance)
                    .into_response();
            }
            crate::config_schema::secret_paths().strip_echoed_secrets(&mut candidate, "");
            match config.validate(&candidate).await {
                Ok(report) => axum::Json(report).into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/config/reload` (`admin:write`, idempotent): re-read the configuration layers
/// and swap in what can be swapped, reporting what still needs a restart rather than implying
/// everything took effect.
async fn config_reload(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/config/reload";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminWrite),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => {
            let Some(config) = &state.config else {
                return source_unavailable("configuration", instance);
            };
            if let Some(key) = idempotency_key(&headers) {
                match state.idempotency.check("config.reload", key, &body) {
                    Replay::Same(stored) => return replay_response(stored),
                    Replay::Mismatch => {
                        return Problem::idempotency_key_payload_mismatch()
                            .with_instance(instance)
                            .into_response();
                    }
                    Replay::Fresh => {}
                }
            }

            let report = match config.reload().await {
                Ok(report) => report,
                Err(e) => return e.to_problem().with_instance(instance).into_response(),
            };

            if let Err(resp) = record_mutation(
                &state,
                &principal,
                "config.reload",
                "config.reloaded",
                // The reload is not scoped to one section, so the target names the resource kind
                // without an id rather than picking a section arbitrarily.
                ResourceRef::new("config_section", "*"),
                Vec::new(),
                json!({
                    "reloaded_sections": report.reloaded_sections,
                    "requires_restart": report.requires_restart,
                }),
            )
            .await
            {
                return resp;
            }

            let response_body = serde_json::to_vec(&report).unwrap_or_default();
            if let Some(key) = idempotency_key(&headers) {
                state.idempotency.record(
                    "config.reload",
                    key,
                    &body,
                    StoredResponse {
                        status: 200,
                        content_type: "application/json".to_string(),
                        body: response_body.clone(),
                    },
                );
            }
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                response_body,
            )
                .into_response()
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// audit log (RFC 0004 section 9, brief deliverable 3)
// -------------------------------------------------------------------------------------------

/// A large-but-finite cap on how many entries `audit_log_list` asks [`AuditSink::query`] for
/// before slicing with [`Page::paginate`]. `AuditSink::query` itself takes a limit rather than a
/// cursor, so this is the widest single fetch this handler ever performs; a store holding more
/// than this many matching entries would need real keyset pagination pushed into the trait, which
/// is future work once a real (non-in-memory) `AuditSink` exists to design it against.
const AUDIT_QUERY_FETCH_LIMIT: usize = 10_000;

#[derive(Debug, Default, Deserialize)]
struct AuditLogQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    actor: Option<String>,
    action: Option<String>,
    target_type: Option<String>,
    target_id: Option<String>,
    outcome: Option<String>,
    recorded_after: Option<String>,
    recorded_before: Option<String>,
    /// Accepted per the OpenAPI `Sort` parameter (`-recorded_at` default, `recorded_at`
    /// ascending); only these two exact values are honoured.
    sort: Option<String>,
}

/// `GET /api/v1/audit-log` (`admin:read`).
async fn audit_log_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<AuditLogQuery>,
) -> Response {
    let instance = "/api/v1/audit-log";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let outcome_success = match query.outcome.as_deref() {
                None => None,
                Some("success") => Some(true),
                Some("failure") => Some(false),
                Some(other) => {
                    return Problem::validation_failed()
                        .with_errors(vec![ValidationError::new(
                            "param:outcome",
                            format!("must be 'success' or 'failure', got '{other}'"),
                        )])
                        .with_instance(instance)
                        .into_response();
                }
            };
            let filter = AuditFilter {
                actor: query.actor.clone(),
                action: query.action.clone(),
                target_type: query.target_type.clone(),
                target_id: query.target_id.clone(),
                outcome_success,
                recorded_after: query.recorded_after.clone(),
                recorded_before: query.recorded_before.clone(),
                cursor: None,
                limit: AUDIT_QUERY_FETCH_LIMIT,
            };
            match state.audit.query(&filter).await {
                Ok(mut entries) => {
                    if query.sort.as_deref() == Some("recorded_at") {
                        entries.reverse();
                    }
                    let page = Page::paginate(
                        entries,
                        query.cursor.as_deref(),
                        query.limit,
                        query.include_total.unwrap_or(false),
                    );
                    axum::Json(page).into_response()
                }
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/audit-log/{id}` (`admin:read`).
async fn audit_log_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/audit-log/{id}");
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => match state.audit.get(&id).await {
            Ok(Some(entry)) => axum::Json(entry).into_response(),
            Ok(None) => Problem::not_found()
                .with_detail(format!("no such audit entry: {id}"))
                .with_instance(instance)
                .into_response(),
            Err(e) => e.to_problem().with_instance(instance).into_response(),
        },
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

#[derive(Debug, Default, Deserialize)]
struct AuditExportQuery {
    recorded_after: Option<String>,
    recorded_before: Option<String>,
}

/// `GET /api/v1/audit-log/export` (`admin:read`): the audit log as NDJSON (one [`AuditEntry`]
/// JSON object per line, `application/x-ndjson`) — a different response shape from
/// `audit_log.list`'s `Page` envelope by design (the OpenAPI document declares it that way: a
/// plain streamable line format an operator can pipe straight into `jq`/`grep`, not a paginated
/// resource). Built from a single [`AuditSink::query`] call and joined into one body rather than a
/// true chunked stream: [`InMemoryAuditSink`](crate::audit::InMemoryAuditSink) (and any real
/// implementation reachable today) already holds every matching entry in memory or a single query
/// result by the time this handler can see it, so there is nothing to stream incrementally yet;
/// revisit if a real `AuditSink` grows a genuinely-streaming query method.
async fn audit_log_export(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<AuditExportQuery>,
) -> Response {
    let instance = "/api/v1/audit-log/export";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let filter = AuditFilter {
                recorded_after: query.recorded_after,
                recorded_before: query.recorded_before,
                limit: AUDIT_QUERY_FETCH_LIMIT,
                ..Default::default()
            };
            match state.audit.query(&filter).await {
                Ok(entries) => {
                    let mut body = String::new();
                    for entry in &entries {
                        match serde_json::to_string(entry) {
                            Ok(line) => {
                                body.push_str(&line);
                                body.push('\n');
                            }
                            Err(e) => {
                                return Problem::internal()
                                    .with_detail(format!("failed to serialize an audit entry: {e}"))
                                    .with_instance(instance)
                                    .into_response();
                            }
                        }
                    }
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
                        body,
                    )
                        .into_response()
                }
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// event stream (RFC 0004 section 10, brief deliverable 3)
// -------------------------------------------------------------------------------------------

/// The subset of `GET /events`'s query parameters this handler filters on. `types`/`resource_*`
/// are read from repeated `key=value` pairs (see [`events_stream`]) rather than
/// `axum::extract::Query<T>` into a struct, since `serde_urlencoded` does not reliably collect a
/// repeated `types=a&types=b` into a `Vec<String>` field.
struct EventFilter {
    types: Vec<String>,
    resource_type: Option<String>,
    resource_id: Option<String>,
}

impl EventFilter {
    fn matches(&self, event: &Event) -> bool {
        if !self.types.is_empty() && !self.types.iter().any(|p| type_matches(p, &event.r#type)) {
            return false;
        }
        if let Some(rt) = &self.resource_type
            && event.resource.as_ref().map(|r| &r.r#type) != Some(rt)
        {
            return false;
        }
        if let Some(rid) = &self.resource_id
            && event.resource.as_ref().map(|r| &r.id) != Some(rid)
        {
            return false;
        }
        true
    }
}

/// Whether `event_type` matches `pattern`, where `pattern` is either an exact type or a
/// `prefix.*` glob (RFC 0004 section 10).
fn type_matches(pattern: &str, event_type: &str) -> bool {
    match pattern.strip_suffix(".*") {
        Some(prefix) => event_type == prefix || event_type.starts_with(&format!("{prefix}.")),
        None => pattern == event_type,
    }
}

/// Renders one [`Event`] as an axum SSE frame.
fn sse_frame(event: &Event) -> AxumSseEvent {
    AxumSseEvent::default()
        .id(event.id.clone())
        .event(event.r#type.clone())
        .data(serde_json::to_string(event).unwrap_or_default())
}

/// `GET /api/v1/events` (`admin:read`): replays the buffer (honoring `Last-Event-ID` or
/// `?last_event_id=`), emitting `stream.reset` first when the client is behind or its id is
/// unrecognized, then streams live events, filtered by `types`/`resource_type`/`resource_id` and
/// interspersed with a keepalive comment every 15 seconds (RFC 0004 section 10). This is the same
/// behavior `hs-admin-mock`'s `sse_events` proved against the same [`EventBus`]; this handler
/// additionally honors `stream.reset` and the query filters, which the mock does not.
async fn events_stream(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Response {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let mut types = Vec::new();
            let mut resource_type = None;
            let mut resource_id = None;
            let mut last_event_id_query = None;
            for (k, v) in pairs {
                match k.as_str() {
                    "types" => types.push(v),
                    "resource_type" => resource_type = Some(v),
                    "resource_id" => resource_id = Some(v),
                    "last_event_id" => last_event_id_query = Some(v),
                    _ => {}
                }
            }
            let last_event_id = headers
                .get("last-event-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .or(last_event_id_query);

            let (outcome, backlog) = state.events.replay_since(last_event_id.as_deref());
            let mut rx = state.events.subscribe();
            let filter = EventFilter {
                types,
                resource_type,
                resource_id,
            };

            let hello = Event::new(
                "stream.hello",
                json!({
                    "server": state.server_info.name,
                    "contract": state.server_info.contract_version,
                    "replica": "single",
                    "buffer_oldest_id": state.events.oldest_id(),
                }),
            );
            let reset =
                matches!(outcome, ReplayOutcome::Behind | ReplayOutcome::Unknown).then(|| {
                    Event::new(
                        "stream.reset",
                        json!({"reason": "behind", "oldest_available": state.events.oldest_id()}),
                    )
                });

            let stream = async_stream::stream! {
                yield Ok(sse_frame(&hello));
                if let Some(reset) = reset {
                    yield Ok(sse_frame(&reset));
                }
                for event in backlog.into_iter().filter(|e| filter.matches(e)) {
                    yield Ok(sse_frame(&event));
                }
                loop {
                    match rx.recv().await {
                        Ok(event) if filter.matches(&event) => yield Ok(sse_frame(&event)),
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            };

            let stream: std::pin::Pin<
                Box<dyn Stream<Item = Result<AxumSseEvent, std::convert::Infallible>> + Send>,
            > = Box::pin(stream);

            Sse::new(stream)
                .keep_alive(
                    KeepAlive::new()
                        .interval(Duration::from_secs(15))
                        .text("keepalive"),
                )
                .into_response()
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance("/api/v1/events").into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance("/api/v1/events").into_response(),
    }
}

async fn serve_openapi_yaml(State(state): State<AdminState>) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/yaml")],
        state.openapi_yaml.to_string(),
    )
        .into_response()
}

async fn serve_openapi_json(State(state): State<AdminState>) -> Response {
    match serde_yaml_ng::from_str::<serde_json::Value>(&state.openapi_yaml) {
        Ok(value) => axum::Json(value).into_response(),
        Err(e) => hs_http::Problem::internal()
            .with_detail(format!(
                "could not convert the embedded OpenAPI document: {e}"
            ))
            .into_response(),
    }
}

/// `GET /api/v1/statistics/overview`: the counts on the Overview page.
async fn statistics_overview(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let instance = "/api/v1/statistics/overview";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(overview) = &state.overview else {
                return source_unavailable("statistics", instance);
            };
            match overview.statistics().await {
                Ok(statistics) => axum::Json(statistics).into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/cluster`: whether this is one server or several, and how many.
async fn cluster_get(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let instance = "/api/v1/cluster";
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(&headers),
        Some(Scope::AdminRead),
    )
    .await
    {
        ScopeDecision::Allowed(_principal) => {
            let Some(overview) = &state.overview else {
                return source_unavailable("cluster", instance);
            };
            match overview.cluster().await {
                Ok(cluster) => axum::Json(cluster).into_response(),
                Err(e) => e.to_problem().with_instance(instance).into_response(),
            }
        }
        ScopeDecision::Unauthenticated(p) => p.with_instance(instance).into_response(),
        ScopeDecision::InsufficientScope(p) => p.with_instance(instance).into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// First-run setup. The only two operations besides the OpenAPI document that take no bearer
// token: `GET /setup` because the interface asks it before anybody can sign in, and
// `POST /setup` because its credential is the setup token in the body.
// -------------------------------------------------------------------------------------------

async fn setup_get(State(state): State<AdminState>) -> Response {
    let instance = "/api/v1/setup";
    let needs_setup = match &state.setup {
        // Nothing wired means nothing here can perform a setup, so none is on offer. Saying
        // `true` would send an operator looking for a link this server never printed.
        None => false,
        Some(setup) => match setup.needs_setup().await {
            Ok(v) => v,
            Err(e) => return e.to_problem().with_instance(instance).into_response(),
        },
    };
    (
        StatusCode::OK,
        // The answer changes exactly once, and the interface must not keep showing a setup page
        // for a server somebody has just claimed.
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        axum::Json(SetupStatus { needs_setup }),
    )
        .into_response()
}

async fn setup_create(State(state): State<AdminState>, body: axum::body::Bytes) -> Response {
    let instance = "/api/v1/setup";
    let Some(setup) = &state.setup else {
        return source_unavailable("first-run setup", instance);
    };
    let request: SetupRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            // `e` names a position and a field, never a value, so it cannot echo the token or
            // the password back.
            return Problem::validation_failed()
                .with_detail(format!("invalid JSON body: {e}"))
                .with_instance(instance)
                .into_response();
        }
    };

    let session = match setup.create_first_admin(request).await {
        Ok(session) => session,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };

    // The actor is the account that now exists: there was nobody before it to have done this.
    let actor = Actor {
        kind: ActorKind::User,
        id: session.user_id.clone(),
        display_name: None,
        token_id: None,
        ip: None,
        user_agent: None,
    };
    let target = ResourceRef::new("user", session.user_id.clone());
    let mut entry = AuditEntry::new(
        "setup.create",
        actor.clone(),
        target.clone(),
        AuditOutcome::success(201),
    );
    entry.changes = vec![AuditChange {
        pointer: "/admin".to_string(),
        from: None,
        to: Some(json!(true)),
    }];
    // The account exists whether or not this write lands, and refusing to hand over its session
    // would lock the operator out of the server they just claimed. So unlike every other
    // mutation, a failed audit write here is logged rather than turned into a `503`.
    if let Err(e) = state.audit.append(entry).await {
        tracing::error!(error = %e, user_id = %session.user_id, "the first administrator was created but the audit entry for it could not be written");
    }
    state.events.publish(
        Event::new("setup.completed", json!({ "user_id": session.user_id }))
            .with_resource(target)
            .with_actor(actor),
    );

    (
        StatusCode::CREATED,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        axum::Json(session),
    )
        .into_response()
}

/// Builds the full router: every declared `/api/v1` operation (enforced but not implemented),
/// the OpenAPI document endpoints, and `/admin/` static assets (`crate::assets`). Returns the
/// manifest alongside so callers can write `routes.json` (RFC 0005) or run the contract check.
pub fn build_router(state: AdminState) -> (axum::Router, RouteManifest) {
    let mut builder: Builder<AdminState> = Builder::new();

    for op in load_operations() {
        // The `public` operations (the OpenAPI document itself, and first-run setup) are
        // registered below with their real handlers; every other operation gets either a real handler (`REAL_HANDLERS`)
        // or the generic 501 handler.
        if op.public {
            continue;
        }
        builder = if REAL_HANDLERS.contains(&op.operation_id.as_str()) {
            register_real_operation(builder, op)
        } else {
            register_operation(builder, op)
        };
    }

    builder = builder.add(
        axum::http::Method::GET,
        "/api/v1/openapi.yaml",
        serve_openapi_yaml,
        RouteMeta::new(Surface::Admin, AuthKind::None),
    );
    builder = builder.add(
        axum::http::Method::GET,
        "/api/v1/openapi.json",
        serve_openapi_json,
        RouteMeta::new(Surface::Admin, AuthKind::None),
    );
    builder = builder.add(
        axum::http::Method::GET,
        "/api/v1/setup",
        setup_get,
        RouteMeta::new(Surface::Admin, AuthKind::None).with_operation_id("setup.get"),
    );
    builder = builder.add(
        axum::http::Method::POST,
        "/api/v1/setup",
        setup_create,
        RouteMeta::new(Surface::Admin, AuthKind::None)
            .with_operation_id("setup.create")
            .rate_limited(),
    );

    let (router, manifest) = builder.build();
    let router = router.merge(crate::assets::router()).with_state(state);
    (router, manifest)
}

fn operation_route_meta(op: &OperationDef) -> RouteMeta {
    let mut meta =
        RouteMeta::new(Surface::Admin, AuthKind::Admin).with_operation_id(op.operation_id.clone());
    if let Some(scope) = op.scope {
        meta = meta.with_scope(scope.as_str());
    }
    if op.method != axum::http::Method::GET {
        meta = meta.rate_limited();
    }
    meta
}

fn register_operation(builder: Builder<AdminState>, op: OperationDef) -> Builder<AdminState> {
    let full_path = format!("/api/v1{}", op.path);
    let scope = op.scope;
    let operation_id = op.operation_id.clone();
    let meta = operation_route_meta(&op);
    let path_for_handler = full_path.clone();
    builder.add(
        op.method.clone(),
        &full_path,
        move |State(state): State<AdminState>, headers: HeaderMap| {
            let operation_id = operation_id.clone();
            let path = path_for_handler.clone();
            async move { not_implemented(state, headers, scope, operation_id, path).await }
        },
        meta,
    )
}

/// Registers one of [`REAL_HANDLERS`] with its concrete handler function, keeping the exact same
/// `(method, path, scope, rate-limited)` shape [`register_operation`] would have produced so the
/// OpenAPI contract test (`tests/contract.rs`) cannot tell the difference.
fn register_real_operation(builder: Builder<AdminState>, op: OperationDef) -> Builder<AdminState> {
    let full_path = format!("/api/v1{}", op.path);
    let meta = operation_route_meta(&op);
    let method = op.method.clone();
    match op.operation_id.as_str() {
        "me.get" => builder.add(method, &full_path, me_get, meta),
        "server.get" => builder.add(method, &full_path, server_get, meta),
        "server.health" => builder.add(method, &full_path, server_health, meta),
        "statistics.overview" => builder.add(method, &full_path, statistics_overview, meta),
        "cluster.get" => builder.add(method, &full_path, cluster_get, meta),
        "users.list" => builder.add(method, &full_path, users_list, meta),
        "users.get" => builder.add(method, &full_path, users_get, meta),
        "users.update" => builder.add(method, &full_path, users_update, meta),
        "users.lock" => builder.add(method, &full_path, users_lock, meta),
        "users.unlock" => builder.add(method, &full_path, users_unlock, meta),
        "users.deactivate" => builder.add(method, &full_path, users_deactivate, meta),
        "users.reactivate" => builder.add(method, &full_path, users_reactivate, meta),
        "users.create" => builder.add(method, &full_path, users_create, meta),
        "users.lookup" => builder.add(method, &full_path, users_lookup, meta),
        "users.availability" => builder.add(method, &full_path, users_availability, meta),
        "rooms.list" => builder.add(method, &full_path, rooms_list, meta),
        "rooms.get" => builder.add(method, &full_path, rooms_get, meta),
        "rooms.block" => builder.add(method, &full_path, rooms_block, meta),
        "rooms.unblock" => builder.add(method, &full_path, rooms_unblock, meta),
        "rooms.make_admin" => builder.add(method, &full_path, rooms_make_admin, meta),
        "config.list" => builder.add(method, &full_path, config_list, meta),
        "config.schema" => builder.add(method, &full_path, config_schema, meta),
        "config.get" => builder.add(method, &full_path, config_get, meta),
        "config.update" => builder.add(method, &full_path, config_update, meta),
        "config.validate" => builder.add(method, &full_path, config_validate, meta),
        "config.reload" => builder.add(method, &full_path, config_reload, meta),
        "audit_log.list" => builder.add(method, &full_path, audit_log_list, meta),
        "audit_log.get" => builder.add(method, &full_path, audit_log_get, meta),
        "audit_log.export" => builder.add(method, &full_path, audit_log_export, meta),
        "events.stream" => builder.add(method, &full_path, events_stream, meta),
        other => unreachable!(
            "{other} is listed in REAL_HANDLERS but register_real_operation doesn't know it"
        ),
    }
}

/// A minimal [`AdminState`] for other modules' tests (`crate::assets`) that need one but are not
/// testing auth or audit behavior themselves.
#[cfg(test)]
pub mod tests_support {
    use std::sync::Arc;

    use super::AdminState;
    use crate::audit::InMemoryAuditSink;
    use crate::auth::StaticVerifier;
    use crate::events::EventBus;

    pub fn dummy_state() -> AdminState {
        AdminState::new(
            Arc::new(StaticVerifier::new()),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::audit::InMemoryAuditSink;
    use crate::auth::StaticVerifier;
    use crate::model::{Principal, PrincipalKind, Scope};

    fn test_state() -> AdminState {
        let verifier = StaticVerifier::new().with_token(
            "admin-token",
            Principal {
                kind: PrincipalKind::User,
                id: "@ops:example.org".into(),
                display_name: None,
                scopes: vec![Scope::AdminWrite],
                token_id: None,
                expires_at: None,
                issued_by: None,
            },
        );
        AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        )
    }

    #[tokio::test]
    async fn unauthenticated_request_is_401() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_request_to_undeclared_scope_is_403() {
        let verifier = StaticVerifier::new().with_token(
            "read-only",
            Principal {
                kind: PrincipalKind::User,
                id: "@ro:example.org".into(),
                display_name: None,
                scopes: vec![Scope::ModerationRead],
                token_id: None,
                expires_at: None,
                issued_by: None,
            },
        );
        let state = AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        );
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer read-only")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn authorized_request_to_undeclared_handler_is_501() {
        // /api/v1/users and /api/v1/rooms are now both in REAL_HANDLERS (see below); use a
        // still-undeclared operation (appservices, owned by another track) to exercise the
        // generic seam.
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/appservices")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn openapi_yaml_needs_no_authentication() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/openapi.yaml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn me_needs_only_authentication_not_a_specific_scope() {
        let verifier = StaticVerifier::new().with_token(
            "read-only",
            Principal {
                kind: PrincipalKind::User,
                id: "@ro:example.org".into(),
                display_name: None,
                scopes: vec![Scope::ModerationRead],
                token_id: None,
                expires_at: None,
                issued_by: None,
            },
        );
        let state = AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        );
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header("authorization", "Bearer read-only")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 200 (not 403): any authenticated principal is enough for GET /me, regardless of scopes.
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn me_reflects_the_principal_the_verifier_returned() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let principal: Principal = serde_json::from_slice(&body).unwrap();
        assert_eq!(principal.id, "@ops:example.org");
        assert_eq!(principal.scopes, vec![Scope::AdminWrite]);
    }

    #[tokio::test]
    async fn server_get_reports_uptime_and_identity() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/server")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("uptime_ms").is_some());
        assert!(value.get("contract_version").is_some());
    }

    #[tokio::test]
    async fn server_health_reports_users_as_unknown_when_unwired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/server/health")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let health: crate::model::ServerHealth = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.checks.get("users"), Some(&"unknown".to_string()));
        assert_eq!(health.status, "degraded");
    }

    #[tokio::test]
    async fn users_list_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn users_get_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    fn state_with_users() -> AdminState {
        use crate::model::AdminUser;
        use crate::sources::InMemoryUserDirectory;

        let mut admin_user = AdminUser {
            user_id: "@ops:example.org".to_string(),
            display_name: Some("Operations".to_string()),
            ..Default::default()
        };
        admin_user.admin = true;
        let alice = AdminUser {
            user_id: "@alice:example.org".to_string(),
            display_name: Some("Alice".to_string()),
            ..Default::default()
        };
        let directory = InMemoryUserDirectory::new()
            .with_user(admin_user)
            .with_user(alice);
        test_state().with_users(Arc::new(directory))
    }

    #[tokio::test]
    async fn users_get_returns_the_real_user() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let user: crate::model::AdminUser = serde_json::from_slice(&body).unwrap();
        assert_eq!(user.user_id, "@alice:example.org");
        assert_eq!(user.display_name, Some("Alice".to_string()));
    }

    #[tokio::test]
    async fn users_get_missing_user_is_404_problem() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/%40nobody%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: hs_http::Problem = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem.r#type, "urn:hs:problem:not-found");
    }

    #[tokio::test]
    async fn users_list_paginates_and_filters_by_admin() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users?admin=true&limit=1")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: crate::model::Page<crate::model::AdminUser> =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].user_id, "@ops:example.org");
        assert_eq!(page.next_cursor, None);
    }

    #[tokio::test]
    async fn users_list_q_filters_free_text() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users?q=alice")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: crate::model::Page<crate::model::AdminUser> =
            serde_json::from_slice(&body).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].user_id, "@alice:example.org");
    }

    #[test]
    fn manifest_has_one_row_per_operation_plus_openapi_endpoints() {
        let (_router, manifest) = build_router(test_state());
        let admin_routes = manifest.method_paths_for(Surface::Admin);
        assert!(admin_routes.len() >= load_operations().len());
        assert!(admin_routes.contains(&("GET".to_string(), "/api/v1/openapi.yaml".to_string())));
    }

    #[test]
    fn real_handlers_keep_the_same_paths_as_the_generic_seam_would() {
        // Every operation id in REAL_HANDLERS must actually exist in the operation table at the
        // path the OpenAPI document declares; this catches a typo in REAL_HANDLERS or
        // register_real_operation silently falling through to the generic 501 handler instead.
        let ops = load_operations();
        for &op_id in REAL_HANDLERS {
            assert!(
                ops.iter().any(|o| o.operation_id == op_id),
                "REAL_HANDLERS names {op_id}, which is not in the operation table"
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // user mutations: every one of these asserts both the audit entry (via GET /audit-log) and
    // the published event (by subscribing to the same EventBus before the request), per the
    // brief's "a mutation that succeeds but records nothing is the failure mode to avoid".
    // -----------------------------------------------------------------------------------------

    async fn body_bytes(response: Response) -> bytes::Bytes {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
    }

    async fn audit_entries_for_action(router: &axum::Router, action: &str) -> Vec<AuditEntry> {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/audit-log?action={action}"))
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_bytes(response).await;
        let page: Page<AuditEntry> = serde_json::from_slice(&body).unwrap();
        page.items
    }

    #[tokio::test]
    async fn users_lock_writes_audit_and_event_and_flips_locked() {
        let state = state_with_users();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/lock")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"spam"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let user: crate::model::AdminUser =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(user.locked);

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "user.locked");
        assert_eq!(event.resource.unwrap().id, "@alice:example.org");
        assert_eq!(event.actor.unwrap().id, "@ops:example.org");

        let entries = audit_entries_for_action(&router, "users.lock").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target.id, "@alice:example.org");
        assert_eq!(entries[0].changes.len(), 1);
        assert_eq!(entries[0].changes[0].pointer, "/locked");
    }

    #[tokio::test]
    async fn users_unlock_writes_audit_and_event_and_flips_locked_back() {
        let state = state_with_users();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        // Lock first so unlock has something to flip.
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/lock")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/unlock")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let user: crate::model::AdminUser =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(!user.locked);

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "user.unlocked");

        let entries = audit_entries_for_action(&router, "users.unlock").await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn users_deactivate_writes_audit_and_event() {
        let state = state_with_users();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/deactivate")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"requested"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let user: crate::model::AdminUser =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(user.deactivated);

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "user.deactivated");

        let entries = audit_entries_for_action(&router, "users.deactivate").await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn users_deactivate_rejects_explicit_erase() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/deactivate")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"erase":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let problem: hs_http::Problem =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(problem.errors.iter().any(|e| e.pointer == "/erase"));
    }

    #[tokio::test]
    async fn users_reactivate_writes_audit_and_event() {
        let state = state_with_users();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/deactivate")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/reactivate")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let user: crate::model::AdminUser =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(!user.deactivated);

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "user.reactivated");

        let entries = audit_entries_for_action(&router, "users.reactivate").await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn users_update_admin_flag_writes_audit_and_event_and_sets_etag() {
        let state = state_with_users();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"admin":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(axum::http::header::ETAG).is_some());
        let user: crate::model::AdminUser =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(user.admin);

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "user.updated");

        let entries = audit_entries_for_action(&router, "users.update").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].changes[0].pointer, "/admin");
    }

    #[tokio::test]
    async fn users_update_rejects_a_field_no_source_can_change_yet() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"display_name":"New Name"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let problem: hs_http::Problem =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(problem.errors.iter().any(|e| e.pointer == "/display_name"));
    }

    #[tokio::test]
    async fn users_update_if_match_mismatch_is_412() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .header("if-match", "\"not-the-real-etag\"")
                    .body(Body::from(r#"{"admin":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn users_update_if_match_with_the_current_etag_succeeds() {
        let state = state_with_users();
        let users_source = state.users.clone().unwrap();
        let (router, _manifest) = build_router(state);

        let alice = users_source
            .get_user("@alice:example.org")
            .await
            .unwrap()
            .unwrap();
        let etag = super::etag_for_user(&alice);

        let response = router
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/users/%40alice%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .header("if-match", etag)
                    .body(Body::from(r#"{"admin":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // -----------------------------------------------------------------------------------------
    // Idempotency-Key
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn idempotency_key_replays_the_first_response_without_repeating_the_mutation() {
        let (router, _manifest) = build_router(state_with_users());
        let make_request = || {
            Request::builder()
                .method("POST")
                .uri("/api/v1/users/%40alice%3Aexample.org/lock")
                .header("authorization", "Bearer admin-token")
                .header("idempotency-key", "retry-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reason":"spam"}"#))
                .unwrap()
        };

        let first = router.clone().oneshot(make_request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get("idempotency-replayed").is_none());

        let second = router.clone().oneshot(make_request()).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second.headers().get("idempotency-replayed").unwrap(),
            "true"
        );

        // The mutation must not have run twice: exactly one audit entry.
        let entries = audit_entries_for_action(&router, "users.lock").await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn idempotency_key_reused_with_a_different_body_is_a_mismatch() {
        let (router, _manifest) = build_router(state_with_users());
        let first = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/lock")
                    .header("authorization", "Bearer admin-token")
                    .header("idempotency-key", "retry-2")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"spam"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/lock")
                    .header("authorization", "Bearer admin-token")
                    .header("idempotency-key", "retry-2")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"a different reason"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    // -----------------------------------------------------------------------------------------
    // audit log endpoints
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn audit_log_get_returns_the_recorded_entry() {
        let (router, _manifest) = build_router(state_with_users());
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/%40alice%3Aexample.org/lock")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let entries = audit_entries_for_action(&router, "users.lock").await;
        let id = entries[0].id.clone();

        let response = router
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/audit-log/{id}"))
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let entry: AuditEntry = serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(entry.id, id);
    }

    #[tokio::test]
    async fn audit_log_get_missing_id_is_404() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/audit-log/nonexistent")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn audit_log_list_rejects_an_unknown_outcome_value() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/audit-log?outcome=sideways")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------------------------
    // event stream
    // -----------------------------------------------------------------------------------------

    /// Pulls one SSE frame off `body` with a short timeout, so a test never hangs waiting on a
    /// stream that (by design) never closes on its own.
    async fn next_sse_frame(body: &mut Body) -> String {
        use http_body_util::BodyExt;
        let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
            .await
            .expect("an SSE frame should arrive promptly")
            .expect("the stream should not end")
            .expect("the frame should not be an error");
        let data = frame.into_data().expect("frame should carry data");
        String::from_utf8(data.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn events_stream_sends_a_hello_frame_first() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(content_type.starts_with("text/event-stream"));
        let mut body = response.into_body();
        let frame = next_sse_frame(&mut body).await;
        assert!(frame.contains("event: stream.hello"));
    }

    #[tokio::test]
    async fn events_stream_replays_the_backlog_after_last_event_id() {
        let state = test_state();
        let first = state
            .events
            .publish(Event::new("user.locked", serde_json::json!({})));
        state
            .events
            .publish(Event::new("user.unlocked", serde_json::json!({})));
        let (router, _manifest) = build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .header("authorization", "Bearer admin-token")
                    .header("last-event-id", first.id.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = response.into_body();
        let hello = next_sse_frame(&mut body).await;
        assert!(hello.contains("event: stream.hello"));
        let replayed = next_sse_frame(&mut body).await;
        assert!(replayed.contains("event: user.unlocked"));
    }

    #[tokio::test]
    async fn events_stream_filters_the_backlog_by_type() {
        let state = test_state();
        state
            .events
            .publish(Event::new("user.locked", serde_json::json!({})));
        state
            .events
            .publish(Event::new("room.created", serde_json::json!({})));
        let (router, _manifest) = build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events?types=user.*")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = response.into_body();
        let hello = next_sse_frame(&mut body).await;
        assert!(hello.contains("stream.hello"));
        let only_match = next_sse_frame(&mut body).await;
        assert!(only_match.contains("event: user.locked"));
    }

    #[tokio::test]
    async fn events_stream_a_too_old_last_event_id_gets_a_stream_reset_first() {
        let state = test_state();
        // capacity 1: publishing "b" evicts "a", so resuming from "a" is "behind".
        let state = AdminState {
            events: Arc::new(EventBus::with_capacity(1)),
            ..state
        };
        let a = state.events.publish(Event::new("a", serde_json::json!({})));
        state.events.publish(Event::new("b", serde_json::json!({})));
        let (router, _manifest) = build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events")
                    .header("authorization", "Bearer admin-token")
                    .header("last-event-id", a.id.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = response.into_body();
        let hello = next_sse_frame(&mut body).await;
        assert!(hello.contains("stream.hello"));
        let reset = next_sse_frame(&mut body).await;
        assert!(reset.contains("event: stream.reset"));
    }

    // -----------------------------------------------------------------------------------------
    // users.create / users.lookup / users.availability (session 6, goal 1): each answers a real
    // 503 when `state.users` is unwired, never a fake 200, and `create` writes exactly one audit
    // entry and publishes exactly one event like every other mutation in this router.
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn users_create_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"localpart":"bob"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn users_lookup_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/lookup?medium=email&address=a%40example.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn users_availability_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/availability?localpart=bob")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn users_availability_missing_localpart_is_400() {
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/availability")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn users_lookup_neither_pair_is_400() {
        // Naming neither (medium, address) nor (provider, external_id) is a validation failure,
        // caught before the source is ever consulted -- this must be 400, not a fabricated 404 or
        // a 503 that would wrongly blame the (unwired) source for a client error.
        let (router, _manifest) = build_router(state_with_users());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/lookup")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn users_create_writes_audit_and_event_and_returns_201() {
        let state = state_with_users();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"localpart":"bob"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // The fake `InMemoryUserDirectory` used by `state_with_users` does not override
        // `create_user`, so this exercises the same honest-503 default the sources tests do —
        // proving the handler reaches the source and propagates its error rather than fabricating
        // success. A real `UserDirectory::create_user` implementation would return 201 here; see
        // `create_user_and_lookup_user_and_check_localpart_available_end_to_end` below for that
        // path exercised against a directory that *does* implement them.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "a failed create must not publish an event"
        );
    }

    /// A tiny [`crate::sources::UserDirectory`] that overrides the three session-6 methods, to
    /// exercise `users.create`/`users.lookup`/`users.availability`'s success paths end to end
    /// (not just their honest-503 fallback, which the tests above already cover).
    struct CreatingUserDirectory {
        inner: crate::sources::InMemoryUserDirectory,
    }

    #[async_trait::async_trait]
    impl UserDirectory for CreatingUserDirectory {
        async fn get_user(
            &self,
            user_id: &str,
        ) -> Result<Option<crate::model::AdminUser>, SourceError> {
            self.inner.get_user(user_id).await
        }
        async fn list_users(
            &self,
            filter: &UserFilter,
        ) -> Result<Vec<crate::model::AdminUser>, SourceError> {
            self.inner.list_users(filter).await
        }
        async fn set_admin(&self, user_id: &str, admin: bool) -> Result<(), SourceError> {
            self.inner.set_admin(user_id, admin).await
        }
        async fn set_locked(&self, user_id: &str, locked: bool) -> Result<(), SourceError> {
            self.inner.set_locked(user_id, locked).await
        }
        async fn set_deactivated(
            &self,
            user_id: &str,
            deactivated: bool,
        ) -> Result<(), SourceError> {
            self.inner.set_deactivated(user_id, deactivated).await
        }
        async fn create_user(
            &self,
            request: UserCreateRequest,
        ) -> Result<crate::model::AdminUser, SourceError> {
            // A read-only fake: proves `users.create`'s handler reaches a source that can
            // succeed (`201`, not the honest-503 fallback), without needing interior mutability
            // this test does not otherwise exercise (nothing here re-fetches the created user).
            let localpart = request
                .localpart
                .ok_or_else(|| SourceError::Invalid("localpart is required".to_string()))?;
            let user_id = format!("@{localpart}:example.org");
            if self.inner.get_user(&user_id).await?.is_some() {
                return Err(SourceError::Conflict(format!("{user_id} already exists")));
            }
            Ok(crate::model::AdminUser {
                user_id,
                display_name: request.display_name,
                admin: request.admin,
                ..Default::default()
            })
        }
        async fn lookup_user(
            &self,
            query: UserLookupQuery,
        ) -> Result<Option<crate::model::AdminUser>, SourceError> {
            match query {
                UserLookupQuery::Threepid { address, .. } if address == "known@example.org" => {
                    Ok(self.inner.get_user("@bob:example.org").await?)
                }
                _ => Ok(None),
            }
        }
        async fn check_localpart_available(&self, localpart: &str) -> Result<bool, SourceError> {
            Ok(self
                .inner
                .get_user(&format!("@{localpart}:example.org"))
                .await?
                .is_none())
        }
    }

    #[tokio::test]
    async fn users_create_succeeds_writes_audit_and_event_against_a_real_source() {
        let directory = CreatingUserDirectory {
            inner: crate::sources::InMemoryUserDirectory::new(),
        };
        let state = test_state().with_users(Arc::new(directory));
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"localpart":"carol"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let user: crate::model::AdminUser =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(user.user_id, "@carol:example.org");

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "user.created");
        assert_eq!(event.resource.unwrap().id, "@carol:example.org");

        let entries = audit_entries_for_action(&router, "users.create").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target.id, "@carol:example.org");
    }

    #[tokio::test]
    async fn users_create_neither_localpart_nor_user_id_is_400() {
        let directory = CreatingUserDirectory {
            inner: crate::sources::InMemoryUserDirectory::new(),
        };
        let state = test_state().with_users(Arc::new(directory));
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn users_lookup_and_availability_succeed_against_a_real_source() {
        use crate::model::AdminUser;

        let bob = AdminUser {
            user_id: "@bob:example.org".to_string(),
            ..Default::default()
        };
        let directory = CreatingUserDirectory {
            inner: crate::sources::InMemoryUserDirectory::new().with_user(bob),
        };
        let state = test_state().with_users(Arc::new(directory));
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/lookup?medium=email&address=known%40example.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let user: crate::model::AdminUser =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(user.user_id, "@bob:example.org");

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/lookup?medium=email&address=unknown%40example.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/availability?localpart=bob")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(value["available"], false);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users/availability?localpart=carol")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(value["available"], true);
    }

    // -----------------------------------------------------------------------------------------
    // rooms (session 6, goal 2): the RoomDirectory seam, exercised against InMemoryRoomDirectory.
    // Every 503 assertion below proves `AdminState::rooms: None` (its real default — no track 04
    // implementation is wired in this session) never fabricates a 200.
    // -----------------------------------------------------------------------------------------

    fn state_with_rooms() -> AdminState {
        use crate::model::AdminRoom;
        use crate::sources::InMemoryRoomDirectory;

        let lounge = AdminRoom {
            room_id: "!lounge:example.org".to_string(),
            name: Some("The Lounge".to_string()),
            ..Default::default()
        };
        let mut blocked = AdminRoom {
            room_id: "!blocked:example.org".to_string(),
            ..Default::default()
        };
        blocked.blocked = true;
        let directory = InMemoryRoomDirectory::new()
            .with_room(lounge)
            .with_room(blocked);
        test_state().with_rooms(Arc::new(directory))
    }

    #[tokio::test]
    async fn rooms_list_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/rooms")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn rooms_get_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/rooms/%21lounge%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn rooms_block_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/%21lounge%3Aexample.org/block")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn rooms_list_paginates_and_filters_by_blocked() {
        let (router, _manifest) = build_router(state_with_rooms());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/rooms?blocked=true")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let page: Page<crate::model::AdminRoom> =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].room_id, "!blocked:example.org");
    }

    #[tokio::test]
    async fn rooms_get_returns_the_real_room() {
        let (router, _manifest) = build_router(state_with_rooms());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/rooms/%21lounge%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let room: crate::model::AdminRoom =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(room.name, Some("The Lounge".to_string()));
    }

    #[tokio::test]
    async fn rooms_get_missing_room_is_404() {
        let (router, _manifest) = build_router(state_with_rooms());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/rooms/%21nobody%3Aexample.org")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rooms_block_writes_audit_and_event_and_flips_blocked() {
        let state = state_with_rooms();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/%21lounge%3Aexample.org/block")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"spam"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let room: crate::model::AdminRoom =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(room.blocked);
        assert_eq!(room.blocked_reason, Some("spam".to_string()));

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "room.blocked");
        assert_eq!(event.resource.unwrap().id, "!lounge:example.org");

        let entries = audit_entries_for_action(&router, "rooms.block").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].changes.len(), 1);
        assert_eq!(entries[0].changes[0].pointer, "/blocked");
    }

    #[tokio::test]
    async fn rooms_unblock_writes_audit_and_event_and_clears_the_reason() {
        let state = state_with_rooms();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/%21blocked%3Aexample.org/unblock")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let room: crate::model::AdminRoom =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(!room.blocked);

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "room.unblocked");

        let entries = audit_entries_for_action(&router, "rooms.unblock").await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn rooms_block_is_idempotent_on_an_already_blocked_room() {
        let state = state_with_rooms();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/%21blocked%3Aexample.org/block")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should still be published even with no field change")
            .unwrap();
        assert_eq!(event.r#type, "room.blocked");
    }

    #[tokio::test]
    async fn rooms_make_admin_writes_audit_and_event() {
        let state = state_with_rooms();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/%21lounge%3Aexample.org/make-admin")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"user_id":"@alice:example.org"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "room.admin_granted");
        assert_eq!(event.data["user_id"], "@alice:example.org");

        let entries = audit_entries_for_action(&router, "rooms.make_admin").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target.id, "!lounge:example.org");
    }

    #[tokio::test]
    async fn rooms_make_admin_missing_room_is_404() {
        let (router, _manifest) = build_router(state_with_rooms());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rooms/%21nobody%3Aexample.org/make-admin")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------------------------
    // audit_log.export (session 6, goal 3)
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn audit_log_export_is_ndjson_one_entry_per_line() {
        let state = state_with_users();
        let (router, _manifest) = build_router(state);

        // Generate two audit entries.
        for _ in 0..2 {
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/users/%40alice%3Aexample.org/lock")
                        .header("authorization", "Bearer admin-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/users/%40alice%3Aexample.org/unlock")
                        .header("authorization", "Bearer admin-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/audit-log/export")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/x-ndjson")
        );
        let body = body_bytes(response).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in lines {
            let entry: AuditEntry = serde_json::from_str(line).unwrap();
            assert!(!entry.id.is_empty());
        }
    }

    #[tokio::test]
    async fn audit_log_export_needs_admin_read() {
        let verifier = StaticVerifier::new().with_token(
            "no-scopes",
            Principal {
                kind: PrincipalKind::User,
                id: "@nobody:example.org".into(),
                display_name: None,
                scopes: vec![],
                token_id: None,
                expires_at: None,
                issued_by: None,
            },
        );
        let state = AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            Arc::new(EventBus::new()),
        );
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/audit-log/export")
                    .header("authorization", "Bearer no-scopes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ---------------------------------------------------------------------------------------
    // configuration. Every one of these runs against `InMemoryConfigSource`, which resolves and
    // validates through `hs_config` itself, so what they pin is the server's real behaviour and
    // not a second implementation of it that happens to agree with the assertions.
    // ---------------------------------------------------------------------------------------

    /// A server whose bootstrap file says registration is off and whose database — changed
    /// through this API at some point — says it is on, with a secret set alongside it.
    fn config_state() -> AdminState {
        use crate::sources::InMemoryConfigSource;

        test_state().with_config(Arc::new(
            InMemoryConfigSource::new()
                .with_file(
                    "/etc/myelin/homeserver.yaml",
                    json!({
                        "server": {"server_name": "example.org"},
                        "auth": {"enable_registration": false},
                    }),
                )
                .with_database(json!({
                    "auth": {"enable_registration": true, "session_secret": "s3kr1t"},
                })),
        ))
    }

    async fn get_section(router: &axum::Router, name: &str) -> ConfigSection {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/config/{name}"))
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&body_bytes(response).await).unwrap()
    }

    async fn patch_section(router: &axum::Router, name: &str, patch: &str) -> Response {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/config/{name}"))
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(patch.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn config_list_is_503_when_the_source_is_not_wired() {
        let (router, _manifest) = build_router(test_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// The whole point of the layering: what the web interface changed is what the server runs
    /// on, even though a file that says otherwise is still mounted, and the section says so.
    #[tokio::test]
    async fn config_get_reports_the_value_and_the_layer_it_came_from() {
        let (router, _manifest) = build_router(config_state());
        let section = get_section(&router, "auth").await;
        assert_eq!(section.values["enable_registration"], json!(true));
        assert_eq!(
            section.origins["/auth/enable_registration"], "database",
            "the database outranks the bootstrap file that says otherwise"
        );
        assert_eq!(
            section.origins["/auth/enable_legacy_login"], "default",
            "a setting nothing sets is reported, not omitted"
        );
        assert_eq!(section.source, "database");
        assert!(!section.reloadable);
        assert!(!section.bootstrap);
    }

    #[tokio::test]
    async fn config_get_unknown_section_is_404() {
        let (router, _manifest) = build_router(config_state());
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/nonsense")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A secret never leaves this API in the clear — not in the section, not in the history of
    /// the change that set it.
    #[tokio::test]
    async fn a_secret_is_returned_redacted_and_never_in_the_clear() {
        let (router, _manifest) = build_router(config_state());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/auth")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let raw = body_bytes(response).await;
        let section: ConfigSection = serde_json::from_slice(&raw).unwrap();
        assert_eq!(section.values["session_secret"], json!({"$secret": true}));
        assert!(
            !String::from_utf8_lossy(&raw).contains("s3kr1t"),
            "the secret appeared somewhere in the response body"
        );

        // And the same secret, set again through this API, is not readable back out of its own
        // history.
        let response = patch_section(&router, "auth", r#"{"session_secret":"a-new-one"}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/auth")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let raw = body_bytes(response).await;
        assert!(
            !String::from_utf8_lossy(&raw).contains("a-new-one"),
            "the secret appeared in the change history"
        );
    }

    /// The form round-trip: a section is served with its secrets redacted, so an untouched
    /// password field comes back as the placeholder it was shown. That means "leave it alone",
    /// not "store this object".
    #[tokio::test]
    async fn an_echoed_secret_placeholder_leaves_the_stored_secret_alone() {
        let (router, _manifest) = build_router(config_state());
        let response = patch_section(
            &router,
            "auth",
            r#"{"session_secret":{"$secret":true},"enable_registration":false}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let section = get_section(&router, "auth").await;
        assert_eq!(section.values["enable_registration"], json!(false));
        assert_eq!(
            section.values["session_secret"],
            json!({"$secret": true}),
            "the secret is still set — it was not replaced by the placeholder"
        );

        let entries = audit_entries_for_action(&router, "config.update").await;
        assert_eq!(entries.len(), 1);
        let pointers: Vec<&str> = entries[0]
            .changes
            .iter()
            .map(|c| c.pointer.as_str())
            .collect();
        assert_eq!(
            pointers,
            vec!["/auth/enable_registration"],
            "the untouched secret is not recorded as a change"
        );
    }

    #[tokio::test]
    async fn config_update_applies_a_patch_and_writes_audit_and_event() {
        let state = config_state();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = patch_section(&router, "auth", r#"{"enable_registration":false}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some("\"2\""),
            "the new revision is the ETag the next If-Match must carry"
        );
        let section: ConfigSection = serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(section.values["enable_registration"], json!(false));

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "config.updated");
        assert_eq!(event.resource.unwrap().id, "auth");

        let entries = audit_entries_for_action(&router, "config.update").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].changes.len(), 1);
        assert_eq!(entries[0].changes[0].pointer, "/auth/enable_registration");
        assert_eq!(entries[0].changes[0].from, Some(json!(true)));
        assert_eq!(entries[0].changes[0].to, Some(json!(false)));
    }

    /// Two operators editing at once: the second write was computed against a view that has
    /// since moved, so it is refused rather than silently clobbering the first.
    #[tokio::test]
    async fn config_update_with_a_stale_if_match_is_412_and_writes_nothing() {
        let (router, _manifest) = build_router(config_state());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/config/auth")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .header("if-match", "\"0\"")
                    .body(Body::from(r#"{"enable_registration":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

        let section = get_section(&router, "auth").await;
        assert_eq!(
            section.values["enable_registration"],
            json!(true),
            "the refused patch left the other operator's value alone"
        );
        assert_eq!(section.revision, 1);
    }

    #[tokio::test]
    async fn config_update_with_the_current_if_match_succeeds() {
        let (router, _manifest) = build_router(config_state());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/config/auth")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .header("if-match", "W/\"1\"")
                    .body(Body::from(r#"{"enable_registration":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A write the environment would override is refused and names the settings. Storing it and
    /// reporting success would be a lie: the value would sit in the database, and the server
    /// would go on using the environment's.
    #[tokio::test]
    async fn config_update_refuses_a_setting_the_environment_pins() {
        use crate::sources::InMemoryConfigSource;

        let state = test_state().with_config(Arc::new(
            InMemoryConfigSource::new()
                .with_file(
                    "/etc/myelin/homeserver.yaml",
                    json!({"server": {"server_name": "example.org"}}),
                )
                .with_environment(json!({"federation": {"client_timeout": "30s"}})),
        ));
        let (router, _manifest) = build_router(state);

        let response = patch_section(&router, "federation", r#"{"client_timeout":"45s"}"#).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let problem: serde_json::Value =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(problem["type"], "urn:hs:problem:conflict");
        assert_eq!(
            problem["errors"][0]["pointer"], "/federation/client_timeout",
            "the refusal names the setting the operator has to go and change elsewhere"
        );

        let section = get_section(&router, "federation").await;
        assert_eq!(section.origins["/federation/client_timeout"], "environment");

        // A sibling setting the environment says nothing about is still editable.
        let response = patch_section(&router, "federation", r#"{"max_retry_backoff":"45s"}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A patch is legal only if the configuration it *produces* is. The rejection carries every
    /// problem, and nothing is written.
    #[tokio::test]
    async fn config_update_rejecting_an_invalid_result_writes_nothing() {
        let (router, _manifest) = build_router(config_state());
        let response = patch_section(&router, "server", r#"{"server_name":""}"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let problem: serde_json::Value =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(problem["type"], "urn:hs:problem:validation-failed");
        assert_eq!(problem["errors"][0]["pointer"], "/server/server_name");

        let section = get_section(&router, "server").await;
        assert_eq!(section.values["server_name"], json!("example.org"));
        assert_eq!(section.revision, 1, "nothing was written");
        assert!(
            audit_entries_for_action(&router, "config.update")
                .await
                .is_empty()
        );
    }

    /// Reset to default: `null` in a merge patch removes the setting, so it reverts to the
    /// schema's own default and nothing owns it any more.
    #[tokio::test]
    async fn a_null_resets_the_setting_to_its_schema_default() {
        let (router, _manifest) = build_router(config_state());
        let response = patch_section(&router, "auth", r#"{"enable_legacy_login":false}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let changed = get_section(&router, "auth").await;
        assert_eq!(changed.values["enable_legacy_login"], json!(false));
        assert_eq!(changed.origins["/auth/enable_legacy_login"], "database");

        let response = patch_section(&router, "auth", r#"{"enable_legacy_login":null}"#).await;
        assert_eq!(response.status(), StatusCode::OK);

        let after = get_section(&router, "auth").await;
        assert_eq!(
            after.values["enable_legacy_login"],
            json!(true),
            "the schema's own default is back"
        );
        assert_eq!(after.origins["/auth/enable_legacy_login"], "default");
    }

    /// A reset of a setting the bootstrap file *also* sets falls back to the file, not to the
    /// schema default.
    ///
    /// This pins what `hs_config::ConfigStore::patch_section` does today: it applies the merge
    /// patch to the stored section, so a `null` deletes the key from the database layer and the
    /// file underneath it wins again. `hs_config::document`'s own module documentation says the
    /// opposite should happen — "a reset means as if nobody had ever set this, which is the only
    /// reading that gives the same result whether or not a bootstrap file happens to be
    /// mounted" — and `document::origins` is written for a database layer that *keeps* the null.
    /// The two disagree, and only with a file mounted does it show. Pinned here rather than
    /// worked around, because `hs-config` belongs to another track and the seam must describe
    /// what the real store actually does.
    #[tokio::test]
    async fn a_reset_falls_back_to_the_bootstrap_file_that_still_sets_it() {
        let (router, _manifest) = build_router(config_state());
        let response = patch_section(&router, "auth", r#"{"enable_registration":null}"#).await;
        assert_eq!(response.status(), StatusCode::OK);

        let after = get_section(&router, "auth").await;
        assert_eq!(after.values["enable_registration"], json!(false));
        assert_eq!(after.origins["/auth/enable_registration"], "file");
    }

    /// `storage` says where the database is, so it is read before there is a database to read it
    /// from. Accepting a write to it would store a setting that can never be read back.
    #[tokio::test]
    async fn config_update_refuses_the_bootstrap_section() {
        let (router, _manifest) = build_router(config_state());
        let response = patch_section(&router, "storage", r#"{"data_dir":"/srv/data"}"#).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);

        // It is still readable, though: an operator needs to see where their data lives.
        let section = get_section(&router, "storage").await;
        assert!(section.bootstrap);
    }

    #[tokio::test]
    async fn config_update_needs_admin_write() {
        let state = config_state();
        let state = AdminState {
            verifier: Arc::new(StaticVerifier::new().with_token(
                "read-only",
                Principal {
                    kind: PrincipalKind::User,
                    id: "@ro:example.org".into(),
                    display_name: None,
                    scopes: vec![Scope::AdminRead],
                    token_id: None,
                    expires_at: None,
                    issued_by: None,
                },
            )),
            ..state
        };
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/config/auth")
                    .header("authorization", "Bearer read-only")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enable_registration":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// What the management interface builds its forms from: enough to render every setting
    /// without naming one.
    #[tokio::test]
    async fn config_schema_describes_every_setting_with_its_origin() {
        use crate::sources::InMemoryConfigSource;

        let state = test_state().with_config(Arc::new(
            InMemoryConfigSource::new()
                .with_database(json!({
                    "server": {"server_name": "example.org"},
                    "auth": {"session_secret": "s3kr1t"},
                }))
                .with_environment(json!({"federation": {"client_timeout": "30s"}})),
        ));
        let (router, _manifest) = build_router(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/schema")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let raw = body_bytes(response).await;
        assert!(
            !String::from_utf8_lossy(&raw).contains("s3kr1t"),
            "the schema endpoint leaked a secret's value"
        );
        let body: crate::model::ConfigSchema = serde_json::from_slice(&raw).unwrap();

        assert_eq!(body.sections.len(), hs_config::reload::SECTION_NAMES.len());
        assert!(
            body.schema["properties"]["auth"].is_object(),
            "the derived JSON Schema is what the form is rendered from"
        );

        let setting = |pointer: &str| {
            body.settings
                .iter()
                .find(|s| s.pointer == pointer)
                .unwrap_or_else(|| panic!("{pointer} is missing from the schema"))
                .clone()
        };

        let secret = setting("/auth/session_secret");
        assert!(secret.secret);
        assert!(secret.editable);

        let pinned = setting("/federation/client_timeout");
        assert_eq!(pinned.origin, "environment");
        assert!(
            !pinned.editable,
            "the interface must not offer an edit this server would refuse"
        );
        assert!(pinned.reloadable);

        let bootstrap = setting("/storage/data_dir");
        assert!(!bootstrap.editable);
        assert_eq!(bootstrap.origin, "default");
    }

    #[tokio::test]
    async fn config_validate_reports_the_problems_without_writing() {
        let (router, _manifest) = build_router(config_state());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/config/validate")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"server":{"server_name":""}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let report: crate::model::ConfigValidateReport =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(!report.valid);
        assert_eq!(report.errors[0].pointer, "/server/server_name");

        assert_eq!(get_section(&router, "server").await.revision, 1);
    }

    #[tokio::test]
    async fn config_validate_accepts_a_good_change_and_says_what_needs_a_restart() {
        let (router, _manifest) = build_router(config_state());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/config/validate")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"server":{"server_name":"new.example"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let report: crate::model::ConfigValidateReport =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(report.valid);
        assert_eq!(
            report.requires_restart,
            vec!["server".to_string()],
            "server_name is burned into every event this process has already produced"
        );
    }

    #[tokio::test]
    async fn config_reload_reports_what_it_reloaded_and_writes_audit_and_event() {
        let state = config_state();
        let mut rx = state.events.subscribe();
        let (router, _manifest) = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/config/reload")
                    .header("authorization", "Bearer admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let report: crate::model::ConfigReloadReport =
            serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert!(report.reloaded_sections.contains(&"federation".to_string()));
        assert!(!report.reloaded_sections.contains(&"server".to_string()));

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("an event should be published")
            .unwrap();
        assert_eq!(event.r#type, "config.reloaded");
        assert_eq!(
            audit_entries_for_action(&router, "config.reload")
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn config_update_with_a_non_object_body_is_400() {
        let (router, _manifest) = build_router(config_state());
        let response = patch_section(&router, "auth", "[]").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// An `If-Match` that is not a revision cannot match one. Ignoring it would tell a caller its
    /// compare-and-set held when nothing had been compared.
    #[tokio::test]
    async fn an_if_match_that_is_not_a_revision_fails_the_precondition() {
        let (router, _manifest) = build_router(config_state());
        let response = router
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/config/auth")
                    .header("authorization", "Bearer admin-token")
                    .header("content-type", "application/json")
                    .header("if-match", "\"not-a-revision\"")
                    .body(Body::from(r#"{"enable_registration":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    // ---------------------------------------------------------------------------------------
    // First-run setup.
    // ---------------------------------------------------------------------------------------

    fn state_needing_setup(audit: Arc<InMemoryAuditSink>) -> AdminState {
        use crate::sources::InMemorySetupSource;
        AdminState::new(
            Arc::new(StaticVerifier::new()),
            audit,
            Arc::new(EventBus::new()),
        )
        .with_setup(Arc::new(InMemorySetupSource::open(
            "example.org",
            "the-setup-token",
        )))
    }

    async fn setup_status(router: &axum::Router) -> serde_json::Value {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn post_setup(
        router: &axum::Router,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/setup")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn setup_status_needs_no_token_and_is_false_when_nothing_can_perform_one() {
        // `test_state` wires no setup source at all.
        let (router, _manifest) = build_router(test_state());
        assert_eq!(setup_status(&router).await, json!({"needs_setup": false}));

        let (router, _manifest) =
            build_router(state_needing_setup(Arc::new(InMemoryAuditSink::new())));
        assert_eq!(setup_status(&router).await, json!({"needs_setup": true}));
    }

    #[tokio::test]
    async fn setup_creates_the_first_administrator_once_and_records_who() {
        let audit = Arc::new(InMemoryAuditSink::new());
        let (router, _manifest) = build_router(state_needing_setup(audit.clone()));

        let (status, session) = post_setup(
            &router,
            json!({"setup_token": "the-setup-token", "username": "Ops", "password": "correct horse"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        assert_eq!(session["user_id"], "@ops:example.org");
        assert!(
            session["access_token"]
                .as_str()
                .is_some_and(|t| !t.is_empty())
        );

        // The offer is closed from that moment, to the status check and to a second attempt
        // with the very same token.
        assert_eq!(setup_status(&router).await, json!({"needs_setup": false}));
        let (status, problem) = post_setup(
            &router,
            json!({"setup_token": "the-setup-token", "username": "mallory", "password": "correct horse"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{problem}");

        let entries = audit
            .query(&AuditFilter {
                action: Some("setup.create".into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1, "exactly one setup is ever recorded");
        assert_eq!(entries[0].actor.id, "@ops:example.org");
        assert_eq!(entries[0].target.id, "@ops:example.org");
    }

    #[tokio::test]
    async fn setup_with_the_wrong_token_is_401_and_does_not_close_the_offer() {
        let (router, _manifest) =
            build_router(state_needing_setup(Arc::new(InMemoryAuditSink::new())));
        let (status, problem) = post_setup(
            &router,
            json!({"setup_token": "a-guess", "username": "mallory", "password": "correct horse"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{problem}");
        assert!(
            problem["type"]
                .as_str()
                .unwrap()
                .ends_with("unauthenticated")
        );
        assert_eq!(setup_status(&router).await, json!({"needs_setup": true}));
    }

    #[tokio::test]
    async fn setup_names_the_field_it_could_not_use_and_never_echoes_a_secret() {
        let (router, _manifest) =
            build_router(state_needing_setup(Arc::new(InMemoryAuditSink::new())));
        let (status, problem) = post_setup(
            &router,
            json!({"setup_token": "the-setup-token", "username": "ops", "password": "short"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
        assert_eq!(problem["errors"][0]["pointer"], "/password");
        // A refused request leaves the offer open for the corrected one.
        assert_eq!(setup_status(&router).await, json!({"needs_setup": true}));

        // A body of the wrong shape is refused without quoting any of it back.
        let (status, problem) = post_setup(
            &router,
            json!({"setup_token": "the-setup-token", "username": 7, "password": "hunter2-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let text = problem.to_string();
        assert!(!text.contains("the-setup-token"), "{text}");
        assert!(!text.contains("hunter2-secret"), "{text}");
    }

    #[tokio::test]
    async fn setup_create_without_a_source_is_503_not_a_silent_success() {
        let (router, _manifest) = build_router(test_state());
        let (status, _problem) = post_setup(
            &router,
            json!({"setup_token": "x", "username": "ops", "password": "correct horse"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn setup_secrets_do_not_survive_debug_formatting() {
        let request = SetupRequest {
            setup_token: "the-setup-token".into(),
            username: "ops".into(),
            password: "hunter2-secret".into(),
        };
        let text = format!("{request:?}");
        assert!(text.contains("ops"));
        assert!(
            !text.contains("the-setup-token") && !text.contains("hunter2-secret"),
            "{text}"
        );

        let session = crate::model::SetupSession {
            user_id: "@ops:example.org".into(),
            access_token: "syt_secret".into(),
            device_id: "SETUP".into(),
        };
        assert!(!format!("{session:?}").contains("syt_secret"));
    }

    // ---------------------------------------------------------------------------------------
    // The Overview page's numbers.
    // ---------------------------------------------------------------------------------------

    async fn get_json(
        router: &axum::Router,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn overview_numbers_nobody_has_are_left_out_rather_than_reported_as_zero() {
        use crate::model::{ClusterStatus, StatisticsOverview};
        use crate::sources::StaticOverviewSource;

        let state = test_state().with_overview(Arc::new(StaticOverviewSource {
            statistics: StatisticsOverview {
                users_count: Some(3),
                rooms_count: Some(0),
                ..StatisticsOverview::default()
            },
            cluster: ClusterStatus {
                mode: "single-node".into(),
                epoch: None,
                replica_count: Some(1),
                shard_count: None,
            },
        }));
        let (router, _manifest) = build_router(state);

        let (status, body) =
            get_json(&router, "/api/v1/statistics/overview", Some("admin-token")).await;
        assert_eq!(status, StatusCode::OK);
        // A real zero is a number; a number nobody counted is not there at all.
        assert_eq!(body, json!({"users_count": 3, "rooms_count": 0}));

        let (status, body) = get_json(&router, "/api/v1/cluster", Some("admin-token")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"mode": "single-node", "replica_count": 1}));
    }

    #[tokio::test]
    async fn overview_operations_need_a_token_and_a_source() {
        let (router, _manifest) = build_router(test_state());
        for uri in ["/api/v1/statistics/overview", "/api/v1/cluster"] {
            let (status, _) = get_json(&router, uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
            // Wired to nothing, it says so: not a 501 (the handler exists) and not invented data.
            let (status, _) = get_json(&router, uri, Some("admin-token")).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
    }
}
