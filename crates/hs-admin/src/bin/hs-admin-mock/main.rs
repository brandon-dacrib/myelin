//! `hs-admin-mock`: a fixture server for track 16 (the management web interface) to develop and
//! test against before any real `hs-admin` handler exists. Serves realistic data for every
//! resource in `openapi/openapi.yaml`, with working cursor pagination, a live SSE stream, and a
//! fake login that issues a scoped bearer token.
//!
//! This binary is deliberately *not* built on `hs_admin::router` (the real skeleton, which
//! answers `501` for everything): it is a separate, self-contained implementation so it can
//! return realistic bodies instead. It does reuse `hs_admin::model` (so its JSON shapes match the
//! real crate's) and `hs_admin::events::EventBus` / `hs_admin::audit` (so the SSE stream and audit
//! log behave like the real thing, not a second, divergent implementation of the same logic).
//!
//! Run with `cargo run -p hs-admin --bin hs-admin-mock` (default `127.0.0.1:8090`; override with
//! `HS_ADMIN_MOCK_ADDR`). See `docs/status/15-admin-api-and-modules.md` for the moment this became
//! usable and what it covers.

mod fixtures;

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use tokio::sync::RwLock;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as AxumSseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use hs_admin::audit::{AuditFilter, AuditSink, InMemoryAuditSink};
use hs_admin::model::{
    Actor, ActorKind, AuditEntry, AuditOutcome, Event, Principal, PrincipalKind, ResourceRef, Scope,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::cors::CorsLayer;

type Db = Arc<RwLock<HashMap<String, Vec<Value>>>>;

#[derive(Clone)]
struct MockState {
    db: Db,
    tokens: Arc<StdRwLock<HashMap<String, Principal>>>,
    events: Arc<hs_admin::events::EventBus>,
    audit: Arc<InMemoryAuditSink>,
}

const DEV_TOKEN: &str = "mock-admin-token";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("hs_admin_mock=info".parse().unwrap()),
        )
        .init();

    let mut tokens = HashMap::new();
    tokens.insert(
        DEV_TOKEN.to_string(),
        Principal {
            kind: PrincipalKind::User,
            id: "@ops:example.org".to_string(),
            display_name: Some("Operations (mock)".to_string()),
            scopes: vec![
                Scope::AdminWrite,
                Scope::BridgesWrite,
                Scope::ModerationWrite,
            ],
            token_id: Some("mock-dev-token".to_string()),
            expires_at: None,
            issued_by: Some("hs-admin-mock".to_string()),
        },
    );

    let state = MockState {
        db: Arc::new(RwLock::new(fixtures::seed())),
        tokens: Arc::new(StdRwLock::new(tokens)),
        events: Arc::new(hs_admin::events::EventBus::new()),
        audit: Arc::new(InMemoryAuditSink::new()),
    };

    spawn_stats_snapshots(state.clone());

    let app = router(state);
    let addr = std::env::var("HS_ADMIN_MOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:8090".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("could not bind HS_ADMIN_MOCK_ADDR");
    tracing::info!(%addr, dev_token = DEV_TOKEN, "hs-admin-mock listening");
    axum::serve(listener, app)
        .await
        .expect("mock server crashed");
}

/// Every ten seconds, publish a `stats.snapshot` event, matching RFC 0004 section 10's taxonomy
/// ("`stats.snapshot`: the `StatisticsOverview`, every 10 seconds while a client is connected").
fn spawn_stats_snapshots(state: MockState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            state.events.publish(Event::new(
                "stats.snapshot",
                fixtures::statistics_overview(),
            ));
        }
    });
}

fn router(state: MockState) -> Router {
    Router::new()
        .route("/api/v1/me", get(get_me))
        .route("/api/v1/server", get(|| async { Json(json!({"name": "hs (mock)", "version": "0.0.1-mock", "build": "mock", "supported_room_versions": ["9", "10", "11"], "enabled_components": ["admin-mock"], "uptime_ms": 0, "contract_version": "1.0-draft"})) }))
        .route("/api/v1/server/health", get(|| async { Json(json!({"status": "ok", "checks": {"mock": "ok"}})) }))
        .route("/api/v1/openapi.yaml", get(get_openapi_yaml))
        .route("/api/v1/openapi.json", get(get_openapi_json))
        .route("/api/v1/mock/login", post(mock_login))
        .route("/api/v1/events", get(sse_events))
        // users
        .route("/api/v1/users", get(list_users).post(create_user))
        .route("/api/v1/users/{user_id}", get(get_user).patch(patch_user))
        .route("/api/v1/users/{user_id}/devices", get(list_user_devices))
        .route("/api/v1/users/{user_id}/sessions", get(list_user_sessions))
        .route("/api/v1/users/{user_id}/memberships", get(list_user_memberships))
        .route("/api/v1/users/{user_id}/media", get(list_user_media))
        .route("/api/v1/users/{user_id}/statistics", get(get_user_statistics))
        .route("/api/v1/users/{user_id}/suspend", post(user_action_suspend))
        .route("/api/v1/users/{user_id}/unsuspend", post(user_action_unsuspend))
        .route("/api/v1/users/{user_id}/lock", post(user_action_lock))
        .route("/api/v1/users/{user_id}/unlock", post(user_action_unlock))
        .route("/api/v1/users/{user_id}/shadow-ban", post(user_action_shadow_ban))
        .route("/api/v1/users/{user_id}/unshadow-ban", post(user_action_unshadow_ban))
        .route("/api/v1/users/{user_id}/deactivate", post(user_action_deactivate))
        .route("/api/v1/users/{user_id}/reactivate", post(user_action_reactivate))
        .route("/api/v1/users/{user_id}/logout", post(user_action_logout))
        .route("/api/v1/users/{user_id}/reset-password", post(user_action_reset_password))
        .route("/api/v1/users/{user_id}/login-as", post(user_action_login_as))
        .route("/api/v1/users/{user_id}/redact-events", post(user_action_redact_events))
        // rooms
        .route("/api/v1/rooms", get(list_rooms))
        .route("/api/v1/rooms/{room_id}", get(get_room))
        .route("/api/v1/rooms/{room_id}/members", get(list_room_members))
        .route("/api/v1/rooms/{room_id}/state", get(list_room_state))
        .route("/api/v1/rooms/{room_id}/messages", get(list_room_messages))
        .route("/api/v1/rooms/{room_id}/block", post(room_action_block))
        .route("/api/v1/rooms/{room_id}/unblock", post(room_action_unblock))
        .route("/api/v1/rooms/{room_id}/make-admin", post(room_action_make_admin))
        .route("/api/v1/rooms/{room_id}/join", post(room_action_join))
        .route("/api/v1/rooms/{room_id}/delete", post(room_action_delete))
        .route("/api/v1/rooms/{room_id}/purge-history", post(room_action_purge_history))
        // media
        .route("/api/v1/media", get(list_media))
        .route("/api/v1/media/{server_name}/{media_id}", get(get_media).delete(delete_media))
        .route("/api/v1/media/{server_name}/{media_id}/quarantine", post(media_action_quarantine))
        .route("/api/v1/media/{server_name}/{media_id}/unquarantine", post(media_action_unquarantine))
        .route("/api/v1/media/{server_name}/{media_id}/protect", post(media_action_protect))
        .route("/api/v1/media/{server_name}/{media_id}/unprotect", post(media_action_unprotect))
        // federation
        .route("/api/v1/federation/destinations", get(list_destinations))
        .route("/api/v1/federation/destinations/{server_name}", get(get_destination))
        .route("/api/v1/federation/destinations/{server_name}/reset", post(destination_reset))
        .route("/api/v1/federation/keys", get(list_server_keys))
        // reports
        .route("/api/v1/reports", get(list_reports))
        .route("/api/v1/reports/{id}", get(get_report).delete(delete_report))
        .route("/api/v1/reports/{id}/resolve", post(resolve_report))
        // registration tokens
        .route("/api/v1/registration-tokens", get(list_registration_tokens).post(create_registration_token))
        .route("/api/v1/registration-tokens/{token}", get(get_registration_token).patch(patch_registration_token).delete(delete_registration_token))
        // tasks
        .route("/api/v1/tasks", get(list_tasks))
        .route("/api/v1/tasks/{id}", get(get_task))
        .route("/api/v1/tasks/{id}/cancel", post(cancel_task))
        // statistics
        .route("/api/v1/statistics/overview", get(|| async { Json(fixtures::statistics_overview()) }))
        .route("/api/v1/statistics/rooms", get(statistics_rooms))
        .route("/api/v1/statistics/users/media", get(statistics_users_media))
        .route("/api/v1/statistics/timeseries", get(statistics_timeseries))
        // server notices
        .route("/api/v1/server-notices", get(list_server_notices).post(send_server_notice))
        // bridges
        .route("/api/v1/bridge-types", get(list_bridge_types))
        .route("/api/v1/bridge-types/{type}", get(get_bridge_type))
        .route("/api/v1/bridge-types/{type}/render", post(render_bridge_type))
        .route("/api/v1/appservices", get(list_appservices).post(create_appservice))
        .route("/api/v1/appservices/{id}", get(get_appservice).patch(patch_appservice).delete(delete_appservice))
        .route("/api/v1/appservices/{id}/registration", get(appservice_registration))
        .route("/api/v1/appservices/{id}/health", get(appservice_health))
        .route("/api/v1/appservices/{id}/backlog", get(appservice_backlog))
        .route("/api/v1/appservices/{id}/pause", post(appservice_pause))
        .route("/api/v1/appservices/{id}/resume", post(appservice_resume))
        .route("/api/v1/appservices/{id}/ping", post(appservice_ping))
        .route("/api/v1/appservices/{id}/rotate-tokens", post(appservice_rotate_tokens))
        .route("/api/v1/appservices/{id}/replay", post(appservice_replay))
        // cluster
        .route("/api/v1/cluster", get(|| async { Json(fixtures::cluster_status()) }))
        .route("/api/v1/cluster/replicas", get(list_replicas))
        .route("/api/v1/cluster/replicas/{id}", get(get_replica))
        .route("/api/v1/cluster/replicas/{id}/drain", post(replica_drain))
        .route("/api/v1/cluster/replicas/{id}/undrain", post(replica_undrain))
        .route("/api/v1/cluster/shards", get(list_shards))
        // migration
        .route("/api/v1/migration", get(get_migration))
        .route("/api/v1/migration/log", get(|| async { Json(page_json(&[], &PageQuery::default())) }))
        .route("/api/v1/migration/start", post(migration_action("copying")))
        .route("/api/v1/migration/pause", post(migration_action("paused")))
        .route("/api/v1/migration/resume", post(migration_action("copying")))
        .route("/api/v1/migration/abort", post(migration_action("aborted")))
        // config
        .route("/api/v1/config", get(list_config))
        .route("/api/v1/config/{section}", get(get_config_section).patch(patch_config_section))
        .route("/api/v1/config/reload", post(config_reload))
        // audit log
        .route("/api/v1/audit-log", get(list_audit_log))
        .route("/api/v1/audit-log/{id}", get(get_audit_entry))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ---------------------------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct PageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    #[serde(default)]
    include_total: bool,
    q: Option<String>,
}

/// A substring, case-insensitive, all-fields search: adequate for a mock's `q` parameter without
/// per-resource search logic.
fn matches_query(item: &Value, q: &str) -> bool {
    fn walk(v: &Value, q: &str) -> bool {
        match v {
            Value::String(s) => s.to_lowercase().contains(q),
            Value::Object(map) => map.values().any(|v| walk(v, q)),
            Value::Array(arr) => arr.iter().any(|v| walk(v, q)),
            Value::Number(n) => n.to_string().contains(q),
            _ => false,
        }
    }
    walk(item, &q.to_lowercase())
}

/// Cursors are plain offsets, base64-free (a real implementation's cursors are opaque and encode
/// the sort key; a mock has no need to hide that). `next_cursor`/`prev_cursor` still round-trip
/// correctly, which is what a UI built against this needs to exercise.
fn page_json(items: &[Value], query: &PageQuery) -> Value {
    let filtered: Vec<Value> = match &query.q {
        Some(q) if !q.is_empty() => items
            .iter()
            .filter(|i| matches_query(i, q))
            .cloned()
            .collect(),
        _ => items.to_vec(),
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let offset = query
        .cursor
        .as_deref()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0)
        .min(filtered.len());
    let end = (offset + limit).min(filtered.len());
    let page: Vec<Value> = filtered[offset..end].to_vec();
    let next_cursor = if end < filtered.len() {
        Some(end.to_string())
    } else {
        None
    };
    let prev_cursor = if offset > 0 {
        Some(offset.saturating_sub(limit).to_string())
    } else {
        None
    };
    let mut envelope =
        json!({"items": page, "next_cursor": next_cursor, "prev_cursor": prev_cursor});
    if query.include_total {
        envelope["total"] = json!(filtered.len());
    }
    envelope
}

fn problem(status: StatusCode, slug: &str, detail: impl Into<String>) -> Response {
    hs_http::Problem::new(slug, slug, status)
        .with_detail(detail.into())
        .into_response()
}

fn not_found(kind: &str, id: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "not-found",
        format!("no such {kind}: {id}"),
    )
}

fn find_index(
    db: &HashMap<String, Vec<Value>>,
    collection: &str,
    id_field: &str,
    id: &str,
) -> Option<usize> {
    db.get(collection)?
        .iter()
        .position(|item| item.get(id_field).and_then(|v| v.as_str()) == Some(id))
}

// Response is a large Err variant, but this is a mock server's auth check, not a hot path;
// boxing it would mean unwrapping a Box<Response> at every one of the ~40 call sites below
// for no real benefit here.
#[allow(clippy::result_large_err)]
fn require_auth(state: &MockState, headers: &HeaderMap) -> Result<Principal, Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| {
            problem(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "missing or malformed Authorization header",
            )
        })?;
    state
        .tokens
        .read()
        .unwrap()
        .get(token)
        .cloned()
        .ok_or_else(|| problem(StatusCode::UNAUTHORIZED, "unauthenticated", "unrecognized bearer token; POST /api/v1/mock/login to get one, or use the dev token"))
}

async fn emit_and_audit(
    state: &MockState,
    action: &str,
    actor: &Principal,
    target: ResourceRef,
    data: Value,
) {
    let actor_ref = Actor {
        kind: ActorKind::User,
        id: actor.id.clone(),
        display_name: actor.display_name.clone(),
        token_id: actor.token_id.clone(),
        ip: None,
        user_agent: None,
    };
    state.events.publish(
        Event::new(action, data.clone())
            .with_resource(target.clone())
            .with_actor(actor_ref.clone()),
    );
    let entry = AuditEntry::new(action, actor_ref, target, AuditOutcome::success(200));
    let _ = state.audit.append(entry).await;
}

// ---------------------------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------------------------

async fn get_me(State(state): State<MockState>, headers: HeaderMap) -> Response {
    match require_auth(&state, &headers) {
        Ok(principal) => Json(principal).into_response(),
        Err(response) => response,
    }
}

async fn get_openapi_yaml() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/yaml")],
        hs_admin::openapi::DOCUMENT,
    )
        .into_response()
}

async fn get_openapi_json() -> Response {
    match serde_yaml_ng::from_str::<Value>(hs_admin::openapi::DOCUMENT) {
        Ok(v) => Json(v).into_response(),
        Err(e) => problem(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Debug, Deserialize, Default)]
struct LoginRequest {
    principal_id: Option<String>,
    display_name: Option<String>,
    scopes: Option<Vec<String>>,
}

/// `POST /api/v1/mock/login`: not part of the real contract (the real API authenticates through
/// 07's OAuth issuer), but the mock needs *some* way to hand out a token, so it exposes this
/// convenience endpoint. The well-known `mock-admin-token` (see `DEV_TOKEN`) skips even this.
async fn mock_login(State(state): State<MockState>, Json(body): Json<LoginRequest>) -> Response {
    let scopes = body
        .scopes
        .unwrap_or_else(|| vec!["admin:write".to_string()])
        .iter()
        .filter_map(|s| Scope::parse(s))
        .collect::<Vec<_>>();
    let scopes = if scopes.is_empty() {
        vec![Scope::AdminWrite]
    } else {
        scopes
    };
    let principal = Principal {
        kind: PrincipalKind::User,
        id: body
            .principal_id
            .unwrap_or_else(|| "@ops:example.org".to_string()),
        display_name: body.display_name.or_else(|| Some("Mock user".to_string())),
        scopes,
        token_id: Some(hs_admin::model::new_id()),
        expires_at: None,
        issued_by: Some("hs-admin-mock".to_string()),
    };
    let token = format!("mock-{}", hs_admin::model::new_id());
    state
        .tokens
        .write()
        .unwrap()
        .insert(token.clone(), principal.clone());
    Json(json!({"access_token": token, "token_type": "Bearer", "principal": principal}))
        .into_response()
}

/// `GET /api/v1/events`: replays the buffer (honoring `Last-Event-ID`/`?last_event_id=`) then
/// streams live events, with a `stream.hello` frame first and periodic keepalive comments,
/// matching RFC 0004 section 10.
async fn sse_events(
    State(state): State<MockState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<AxumSseEvent, std::convert::Infallible>>> {
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| q.get("last_event_id").cloned());
    let (_outcome, backlog) = state.events.replay_since(last_event_id.as_deref());
    let mut rx = state.events.subscribe();

    let hello = Event::new(
        "stream.hello",
        json!({"server": "hs (mock)", "contract": "1.0-draft", "replica": "mock", "buffer_oldest_id": state.events.oldest_id()}),
    );

    let stream = async_stream::stream! {
        yield Ok(AxumSseEvent::default().id(hello.id.clone()).event(hello.r#type.clone()).data(serde_json::to_string(&hello).unwrap_or_default()));
        for event in backlog {
            yield Ok(AxumSseEvent::default().id(event.id.clone()).event(event.r#type.clone()).data(serde_json::to_string(&event).unwrap_or_default()));
        }
        loop {
            match rx.recv().await {
                Ok(event) => yield Ok(AxumSseEvent::default().id(event.id.clone()).event(event.r#type.clone()).data(serde_json::to_string(&event).unwrap_or_default())),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keepalive"),
    )
}

// ---------------------------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------------------------

async fn list_users(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("users").cloned().unwrap_or_default().as_slice(),
        &q,
    ))
}

#[derive(Debug, Deserialize)]
struct CreateUserRequest {
    #[serde(alias = "localpart")]
    user_id: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    admin: bool,
}

async fn create_user(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<CreateUserRequest>,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let user_id = body.user_id.unwrap_or_else(|| {
        format!(
            "@newuser{}:example.org",
            hs_admin::model::new_id().to_lowercase()
        )
    });
    let user = json!({
        "user_id": user_id, "display_name": body.display_name, "avatar_url": null, "admin": body.admin,
        "deactivated": false, "erased": false, "locked": false, "suspended": false, "shadow_banned": false,
        "user_type": null, "consent_version": null, "appservice_id": null,
        "created_at": hs_http::time::now_rfc3339(), "last_seen_at": null, "device_count": 0, "room_count": 0, "media_count": 0
    });
    state
        .db
        .write()
        .await
        .get_mut("users")
        .unwrap()
        .push(user.clone());
    emit_and_audit(
        &state,
        "users.create",
        &actor,
        ResourceRef::new("user", user_id),
        user.clone(),
    )
    .await;
    (StatusCode::CREATED, Json(user)).into_response()
}

async fn get_user(State(state): State<MockState>, Path(user_id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "users", "user_id", &user_id) {
        Some(idx) => Json(db["users"][idx].clone()).into_response(),
        None => not_found("user", &user_id),
    }
}

async fn patch_user(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
    Json(patch): Json<Value>,
) -> Response {
    let mut db = state.db.write().await;
    match find_index(&db, "users", "user_id", &user_id) {
        Some(idx) => {
            if let (Some(target), Some(patch_obj)) = (
                db.get_mut("users").unwrap()[idx].as_object_mut(),
                patch.as_object(),
            ) {
                for (k, v) in patch_obj {
                    target.insert(k.clone(), v.clone());
                }
            }
            Json(db["users"][idx].clone()).into_response()
        }
        None => not_found("user", &user_id),
    }
}

async fn list_user_devices(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
) -> Response {
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    Json(json!({"items": [
        {"device_id": "DEVICE001", "display_name": "Element (desktop)", "last_seen_ip": "203.0.113.7", "last_seen_at": "2026-09-18T08:00:00.000Z"},
        {"device_id": "DEVICE002", "display_name": "Element X (mobile)", "last_seen_ip": "203.0.113.9", "last_seen_at": "2026-09-17T22:00:00.000Z"}
    ], "next_cursor": null, "prev_cursor": null}))
    .into_response()
}

async fn list_user_sessions(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
) -> Response {
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    Json(json!({"items": [
        {"device_id": "DEVICE001", "ip": "203.0.113.7", "user_agent": "Element/1.11.0 (Macintosh)", "created_at": "2026-01-05T09:00:00.000Z", "last_seen_at": "2026-09-18T08:00:00.000Z"}
    ], "next_cursor": null, "prev_cursor": null}))
    .into_response()
}

async fn list_user_memberships(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    let rooms = state
        .db
        .read()
        .await
        .get("rooms")
        .cloned()
        .unwrap_or_default();
    let members: Vec<Value> = rooms.iter().map(|r| json!({"user_id": user_id, "membership": "join", "display_name": null, "avatar_url": null, "room_id": r["room_id"]})).collect();
    Json(page_json(&members, &q)).into_response()
}

async fn list_user_media(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    let media = state
        .db
        .read()
        .await
        .get("media")
        .cloned()
        .unwrap_or_default();
    let mine: Vec<Value> = media
        .into_iter()
        .filter(|m| m.get("uploader").and_then(|u| u.as_str()) == Some(user_id.as_str()))
        .collect();
    Json(page_json(&mine, &q)).into_response()
}

async fn get_user_statistics(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
) -> Response {
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    Json(json!({"invites_sent_count": 12, "joins_count": 34})).into_response()
}

/// Builds a user-action handler that flips one boolean field (and its paired `*_at` timestamp, if
/// named) and audits/emits under `action`.
macro_rules! user_bool_action {
    ($fn_name:ident, $field:literal, $value:expr, $action:literal) => {
        async fn $fn_name(
            State(state): State<MockState>,
            Path(user_id): Path<String>,
            headers: HeaderMap,
        ) -> Response {
            let actor = match require_auth(&state, &headers) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut db = state.db.write().await;
            let Some(idx) = find_index(&db, "users", "user_id", &user_id) else {
                return not_found("user", &user_id);
            };
            let item = &mut db.get_mut("users").unwrap()[idx];
            item[$field] = json!($value);
            let updated = item.clone();
            drop(db);
            emit_and_audit(
                &state,
                $action,
                &actor,
                ResourceRef::new("user", user_id),
                updated.clone(),
            )
            .await;
            Json(updated).into_response()
        }
    };
}

user_bool_action!(user_action_suspend, "suspended", true, "users.suspend");
user_bool_action!(user_action_unsuspend, "suspended", false, "users.unsuspend");
user_bool_action!(user_action_lock, "locked", true, "users.lock");
user_bool_action!(user_action_unlock, "locked", false, "users.unlock");
user_bool_action!(
    user_action_shadow_ban,
    "shadow_banned",
    true,
    "users.shadow_ban"
);
user_bool_action!(
    user_action_unshadow_ban,
    "shadow_banned",
    false,
    "users.unshadow_ban"
);
user_bool_action!(
    user_action_deactivate,
    "deactivated",
    true,
    "users.deactivate"
);
user_bool_action!(
    user_action_reactivate,
    "deactivated",
    false,
    "users.reactivate"
);

async fn user_action_logout(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    emit_and_audit(
        &state,
        "users.logout",
        &actor,
        ResourceRef::new("user", user_id.clone()),
        json!({}),
    )
    .await;
    Json(json!({"user_id": user_id, "logged_out": true})).into_response()
}

async fn user_action_reset_password(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    emit_and_audit(
        &state,
        "users.reset_password",
        &actor,
        ResourceRef::new("user", user_id.clone()),
        json!({}),
    )
    .await;
    Json(json!({"user_id": user_id})).into_response()
}

async fn user_action_login_as(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    emit_and_audit(
        &state,
        "users.login_as",
        &actor,
        ResourceRef::new("user", user_id.clone()),
        json!({}),
    )
    .await;
    (StatusCode::CREATED, Json(json!({"access_token": format!("mock-impersonation-{}", hs_admin::model::new_id()), "device_id": "IMPERSONATE1", "expires_at": null}))).into_response()
}

async fn user_action_redact_events(
    State(state): State<MockState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "users", "user_id", &user_id).is_none() {
        return not_found("user", &user_id);
    }
    let task = mock_task(
        "user.redact_events",
        Some(ResourceRef::new("user", user_id.clone())),
    );
    state
        .db
        .write()
        .await
        .get_mut("tasks")
        .unwrap()
        .push(task.clone());
    emit_and_audit(
        &state,
        "users.redact_events",
        &actor,
        ResourceRef::new("user", user_id),
        task.clone(),
    )
    .await;
    (
        StatusCode::ACCEPTED,
        [(
            header::LOCATION,
            format!("/api/v1/tasks/{}", task["id"].as_str().unwrap()),
        )],
        Json(task),
    )
        .into_response()
}

fn mock_task(action: &str, resource: Option<ResourceRef>) -> Value {
    json!({
        "id": hs_admin::model::new_id(), "action": action, "status": "scheduled", "resource": resource,
        "progress": null, "result": null, "error": null, "created_at": hs_http::time::now_rfc3339(),
        "started_at": null, "finished_at": null, "scheduled_for": null,
        "created_by": {"kind": "user", "id": "@ops:example.org"}
    })
}

// ---------------------------------------------------------------------------------------------
// rooms
// ---------------------------------------------------------------------------------------------

async fn list_rooms(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("rooms").cloned().unwrap_or_default().as_slice(),
        &q,
    ))
}

async fn get_room(State(state): State<MockState>, Path(room_id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "rooms", "room_id", &room_id) {
        Some(idx) => Json(db["rooms"][idx].clone()).into_response(),
        None => not_found("room", &room_id),
    }
}

async fn list_room_members(
    State(state): State<MockState>,
    Path(room_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    if find_index(&*state.db.read().await, "rooms", "room_id", &room_id).is_none() {
        return not_found("room", &room_id);
    }
    let members = vec![
        json!({"user_id": "@alice:example.org", "membership": "join", "display_name": "Alice", "avatar_url": null}),
        json!({"user_id": "@bob:example.org", "membership": "join", "display_name": "Bob", "avatar_url": null}),
        json!({"user_id": "@mallory:example.org", "membership": "ban", "display_name": "Mallory", "avatar_url": null}),
    ];
    Json(page_json(&members, &q)).into_response()
}

async fn list_room_state(
    State(state): State<MockState>,
    Path(room_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    if find_index(&*state.db.read().await, "rooms", "room_id", &room_id).is_none() {
        return not_found("room", &room_id);
    }
    let state_events = vec![
        json!({"event_id": "$state1", "type": "m.room.name", "state_key": "", "sender": "@ops:example.org", "content": {"name": "General"}, "origin_server_ts": 1_767_000_000_000i64}),
        json!({"event_id": "$state2", "type": "m.room.join_rules", "state_key": "", "sender": "@ops:example.org", "content": {"join_rule": "public"}, "origin_server_ts": 1_767_000_001_000i64}),
    ];
    Json(page_json(&state_events, &q)).into_response()
}

async fn list_room_messages(
    State(state): State<MockState>,
    Path(room_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    if find_index(&*state.db.read().await, "rooms", "room_id", &room_id).is_none() {
        return not_found("room", &room_id);
    }
    let messages = vec![
        json!({"event_id": "$m1", "type": "m.room.message", "sender": "@alice:example.org", "content": {"msgtype": "m.text", "body": "Good morning!"}, "origin_server_ts": 1_767_100_000_000i64, "state_key": null}),
        json!({"event_id": "$m2", "type": "m.room.message", "sender": "@bob:example.org", "content": {"msgtype": "m.text", "body": "Morning"}, "origin_server_ts": 1_767_100_005_000i64, "state_key": null}),
    ];
    Json(page_json(&messages, &q)).into_response()
}

macro_rules! room_bool_action {
    ($fn_name:ident, $field:literal, $value:expr, $action:literal) => {
        async fn $fn_name(
            State(state): State<MockState>,
            Path(room_id): Path<String>,
            headers: HeaderMap,
        ) -> Response {
            let actor = match require_auth(&state, &headers) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut db = state.db.write().await;
            let Some(idx) = find_index(&db, "rooms", "room_id", &room_id) else {
                return not_found("room", &room_id);
            };
            db.get_mut("rooms").unwrap()[idx][$field] = json!($value);
            let updated = db["rooms"][idx].clone();
            drop(db);
            emit_and_audit(
                &state,
                $action,
                &actor,
                ResourceRef::new("room", room_id),
                updated.clone(),
            )
            .await;
            Json(updated).into_response()
        }
    };
}

room_bool_action!(room_action_block, "blocked", true, "rooms.block");
room_bool_action!(room_action_unblock, "blocked", false, "rooms.unblock");

async fn room_action_make_admin(
    State(state): State<MockState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "rooms", "room_id", &room_id).is_none() {
        return not_found("room", &room_id);
    }
    emit_and_audit(
        &state,
        "rooms.make_admin",
        &actor,
        ResourceRef::new("room", room_id.clone()),
        json!({}),
    )
    .await;
    Json(json!({"room_id": room_id})).into_response()
}

async fn room_action_join(
    State(state): State<MockState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "rooms", "room_id", &room_id).is_none() {
        return not_found("room", &room_id);
    }
    let user_id = body
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("@unknown:example.org");
    emit_and_audit(
        &state,
        "rooms.join",
        &actor,
        ResourceRef::new("room", room_id),
        json!({"user_id": user_id}),
    )
    .await;
    Json(json!({"user_id": user_id, "membership": "join"})).into_response()
}

async fn room_action_delete(
    State(state): State<MockState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "rooms", "room_id", &room_id).is_none() {
        return not_found("room", &room_id);
    }
    let task = mock_task(
        "room.delete",
        Some(ResourceRef::new("room", room_id.clone())),
    );
    state
        .db
        .write()
        .await
        .get_mut("tasks")
        .unwrap()
        .push(task.clone());
    emit_and_audit(
        &state,
        "rooms.delete",
        &actor,
        ResourceRef::new("room", room_id),
        task.clone(),
    )
    .await;
    (
        StatusCode::ACCEPTED,
        [(
            header::LOCATION,
            format!("/api/v1/tasks/{}", task["id"].as_str().unwrap()),
        )],
        Json(task),
    )
        .into_response()
}

async fn room_action_purge_history(
    State(state): State<MockState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "rooms", "room_id", &room_id).is_none() {
        return not_found("room", &room_id);
    }
    let task = mock_task(
        "room.purge_history",
        Some(ResourceRef::new("room", room_id.clone())),
    );
    state
        .db
        .write()
        .await
        .get_mut("tasks")
        .unwrap()
        .push(task.clone());
    emit_and_audit(
        &state,
        "rooms.purge_history",
        &actor,
        ResourceRef::new("room", room_id),
        task.clone(),
    )
    .await;
    (
        StatusCode::ACCEPTED,
        [(
            header::LOCATION,
            format!("/api/v1/tasks/{}", task["id"].as_str().unwrap()),
        )],
        Json(task),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// media
// ---------------------------------------------------------------------------------------------

async fn list_media(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("media").cloned().unwrap_or_default().as_slice(),
        &q,
    ))
}

fn media_key(server_name: &str, media_id: &str) -> String {
    format!("{server_name}/{media_id}")
}

fn find_media_index(
    db: &HashMap<String, Vec<Value>>,
    server_name: &str,
    media_id: &str,
) -> Option<usize> {
    db.get("media")?.iter().position(|m| {
        m.get("server_name").and_then(|v| v.as_str()) == Some(server_name)
            && m.get("media_id").and_then(|v| v.as_str()) == Some(media_id)
    })
}

async fn get_media(
    State(state): State<MockState>,
    Path((server_name, media_id)): Path<(String, String)>,
) -> Response {
    let db = state.db.read().await;
    match find_media_index(&db, &server_name, &media_id) {
        Some(idx) => Json(db["media"][idx].clone()).into_response(),
        None => not_found("media", &media_key(&server_name, &media_id)),
    }
}

async fn delete_media(
    State(state): State<MockState>,
    Path((server_name, media_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    match find_media_index(&db, &server_name, &media_id) {
        Some(idx) => {
            db.get_mut("media").unwrap().remove(idx);
            drop(db);
            emit_and_audit(
                &state,
                "media.delete_one",
                &actor,
                ResourceRef::new("media", media_key(&server_name, &media_id)),
                json!({}),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found("media", &media_key(&server_name, &media_id)),
    }
}

macro_rules! media_bool_action {
    ($fn_name:ident, $field:literal, $value:expr, $action:literal) => {
        async fn $fn_name(
            State(state): State<MockState>,
            Path((server_name, media_id)): Path<(String, String)>,
            headers: HeaderMap,
        ) -> Response {
            let actor = match require_auth(&state, &headers) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut db = state.db.write().await;
            let Some(idx) = find_media_index(&db, &server_name, &media_id) else {
                return not_found("media", &media_key(&server_name, &media_id));
            };
            db.get_mut("media").unwrap()[idx][$field] = json!($value);
            let updated = db["media"][idx].clone();
            drop(db);
            emit_and_audit(
                &state,
                $action,
                &actor,
                ResourceRef::new("media", media_key(&server_name, &media_id)),
                updated.clone(),
            )
            .await;
            Json(updated).into_response()
        }
    };
}

media_bool_action!(
    media_action_quarantine,
    "quarantined",
    true,
    "media.quarantine"
);
media_bool_action!(
    media_action_unquarantine,
    "quarantined",
    false,
    "media.unquarantine"
);
media_bool_action!(media_action_protect, "protected", true, "media.protect");
media_bool_action!(
    media_action_unprotect,
    "protected",
    false,
    "media.unprotect"
);

// ---------------------------------------------------------------------------------------------
// federation
// ---------------------------------------------------------------------------------------------

async fn list_destinations(
    State(state): State<MockState>,
    Query(q): Query<PageQuery>,
) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("destinations")
            .cloned()
            .unwrap_or_default()
            .as_slice(),
        &q,
    ))
}

async fn get_destination(
    State(state): State<MockState>,
    Path(server_name): Path<String>,
) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "destinations", "server_name", &server_name) {
        Some(idx) => Json(db["destinations"][idx].clone()).into_response(),
        None => not_found("destination", &server_name),
    }
}

async fn destination_reset(
    State(state): State<MockState>,
    Path(server_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    let Some(idx) = find_index(&db, "destinations", "server_name", &server_name) else {
        return not_found("destination", &server_name);
    };
    let item = &mut db.get_mut("destinations").unwrap()[idx];
    item["failing_since"] = Value::Null;
    item["retry_interval_ms"] = Value::Null;
    let updated = item.clone();
    drop(db);
    emit_and_audit(
        &state,
        "federation.destinations.reset",
        &actor,
        ResourceRef::new("destination", server_name),
        updated.clone(),
    )
    .await;
    Json(updated).into_response()
}

async fn list_server_keys(State(state): State<MockState>) -> Json<Value> {
    Json(json!(
        state
            .db
            .read()
            .await
            .get("server_keys")
            .cloned()
            .unwrap_or_default()
    ))
}

// ---------------------------------------------------------------------------------------------
// reports
// ---------------------------------------------------------------------------------------------

async fn list_reports(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("reports").cloned().unwrap_or_default().as_slice(),
        &q,
    ))
}

async fn get_report(State(state): State<MockState>, Path(id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "reports", "id", &id) {
        Some(idx) => Json(db["reports"][idx].clone()).into_response(),
        None => not_found("report", &id),
    }
}

async fn delete_report(
    State(state): State<MockState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    match find_index(&db, "reports", "id", &id) {
        Some(idx) => {
            db.get_mut("reports").unwrap().remove(idx);
            drop(db);
            emit_and_audit(
                &state,
                "reports.delete",
                &actor,
                ResourceRef::new("report", id),
                json!({}),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found("report", &id),
    }
}

async fn resolve_report(
    State(state): State<MockState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    let Some(idx) = find_index(&db, "reports", "id", &id) else {
        return not_found("report", &id);
    };
    let item = &mut db.get_mut("reports").unwrap()[idx];
    item["status"] = json!("resolved");
    item["resolution"] = body.get("resolution").cloned().unwrap_or(json!("other"));
    item["resolution_note"] = body.get("note").cloned().unwrap_or(Value::Null);
    let updated = item.clone();
    drop(db);
    emit_and_audit(
        &state,
        "reports.resolve",
        &actor,
        ResourceRef::new("report", id),
        updated.clone(),
    )
    .await;
    Json(updated).into_response()
}

// ---------------------------------------------------------------------------------------------
// registration tokens
// ---------------------------------------------------------------------------------------------

async fn list_registration_tokens(
    State(state): State<MockState>,
    Query(q): Query<PageQuery>,
) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("registration_tokens")
            .cloned()
            .unwrap_or_default()
            .as_slice(),
        &q,
    ))
}

async fn create_registration_token(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let token = body
        .get("token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| hs_admin::model::new_id().to_lowercase());
    let item = json!({"token": token, "uses_allowed": body.get("uses_allowed"), "pending": 0, "completed": 0, "expires_at": body.get("expires_at"), "created_at": hs_http::time::now_rfc3339()});
    state
        .db
        .write()
        .await
        .get_mut("registration_tokens")
        .unwrap()
        .push(item.clone());
    emit_and_audit(
        &state,
        "registration_tokens.create",
        &actor,
        ResourceRef::new("registration_token", item["token"].as_str().unwrap()),
        item.clone(),
    )
    .await;
    (StatusCode::CREATED, Json(item)).into_response()
}

async fn get_registration_token(
    State(state): State<MockState>,
    Path(token): Path<String>,
) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "registration_tokens", "token", &token) {
        Some(idx) => Json(db["registration_tokens"][idx].clone()).into_response(),
        None => not_found("registration_token", &token),
    }
}

async fn patch_registration_token(
    State(state): State<MockState>,
    Path(token): Path<String>,
    Json(patch): Json<Value>,
) -> Response {
    let mut db = state.db.write().await;
    match find_index(&db, "registration_tokens", "token", &token) {
        Some(idx) => {
            if let (Some(target), Some(patch_obj)) = (
                db.get_mut("registration_tokens").unwrap()[idx].as_object_mut(),
                patch.as_object(),
            ) {
                for (k, v) in patch_obj {
                    target.insert(k.clone(), v.clone());
                }
            }
            Json(db["registration_tokens"][idx].clone()).into_response()
        }
        None => not_found("registration_token", &token),
    }
}

async fn delete_registration_token(
    State(state): State<MockState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    match find_index(&db, "registration_tokens", "token", &token) {
        Some(idx) => {
            db.get_mut("registration_tokens").unwrap().remove(idx);
            drop(db);
            emit_and_audit(
                &state,
                "registration_tokens.delete",
                &actor,
                ResourceRef::new("registration_token", token),
                json!({}),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found("registration_token", &token),
    }
}

// ---------------------------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------------------------

async fn list_tasks(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("tasks").cloned().unwrap_or_default().as_slice(),
        &q,
    ))
}

async fn get_task(State(state): State<MockState>, Path(id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "tasks", "id", &id) {
        Some(idx) => Json(db["tasks"][idx].clone()).into_response(),
        None => not_found("task", &id),
    }
}

async fn cancel_task(
    State(state): State<MockState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    let Some(idx) = find_index(&db, "tasks", "id", &id) else {
        return not_found("task", &id);
    };
    db.get_mut("tasks").unwrap()[idx]["status"] = json!("cancelled");
    let updated = db["tasks"][idx].clone();
    drop(db);
    emit_and_audit(
        &state,
        "tasks.cancel",
        &actor,
        ResourceRef::new("task", id),
        updated.clone(),
    )
    .await;
    Json(updated).into_response()
}

// ---------------------------------------------------------------------------------------------
// statistics
// ---------------------------------------------------------------------------------------------

async fn statistics_rooms(
    State(state): State<MockState>,
    Query(q): Query<PageQuery>,
) -> Json<Value> {
    let rooms = state
        .db
        .read()
        .await
        .get("rooms")
        .cloned()
        .unwrap_or_default();
    let stats: Vec<Value> = rooms.iter().map(|r| json!({"room_id": r["room_id"], "name": r["name"], "joined_members_count": r["joined_members_count"], "state_events_count": r["state_events_count"]})).collect();
    Json(page_json(&stats, &q))
}

async fn statistics_users_media(
    State(state): State<MockState>,
    Query(q): Query<PageQuery>,
) -> Json<Value> {
    let media = state
        .db
        .read()
        .await
        .get("media")
        .cloned()
        .unwrap_or_default();
    let mut by_user: HashMap<String, (u64, u64)> = HashMap::new();
    for item in &media {
        if let Some(uploader) = item.get("uploader").and_then(|v| v.as_str()) {
            let entry = by_user.entry(uploader.to_string()).or_default();
            entry.0 += 1;
            entry.1 += item.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        }
    }
    let stats: Vec<Value> = by_user.into_iter().map(|(user_id, (count, bytes))| json!({"user_id": user_id, "media_count": count, "media_bytes": bytes})).collect();
    Json(page_json(&stats, &q))
}

async fn statistics_timeseries(Query(params): Query<HashMap<String, String>>) -> Json<Value> {
    let metric = params
        .get("metric")
        .cloned()
        .unwrap_or_else(|| "messages_per_minute".to_string());
    let points: Vec<Value> = (0..12).map(|i| json!({"at": format!("2026-09-18T{:02}:00:00.000Z", i), "value": 10.0 + (i as f64) * 1.5})).collect();
    Json(json!({"metric": metric, "step_ms": 3_600_000, "points": points}))
}

// ---------------------------------------------------------------------------------------------
// server notices
// ---------------------------------------------------------------------------------------------

async fn list_server_notices(
    State(state): State<MockState>,
    Query(q): Query<PageQuery>,
) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("server_notices")
            .cloned()
            .unwrap_or_default()
            .as_slice(),
        &q,
    ))
}

async fn send_server_notice(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let recipients = body.get("recipients").cloned().unwrap_or(json!([]));
    let notice = json!({"event_ids": [format!("$notice{}", hs_admin::model::new_id())], "recipients": recipients, "sent_at": hs_http::time::now_rfc3339()});
    state
        .db
        .write()
        .await
        .get_mut("server_notices")
        .unwrap()
        .push(notice.clone());
    emit_and_audit(
        &state,
        "server_notices.send",
        &actor,
        ResourceRef::new("server_notice", notice["event_ids"][0].as_str().unwrap()),
        notice.clone(),
    )
    .await;
    (StatusCode::CREATED, Json(notice)).into_response()
}

// ---------------------------------------------------------------------------------------------
// bridges and appservices
// ---------------------------------------------------------------------------------------------

async fn list_bridge_types(
    State(state): State<MockState>,
    Query(q): Query<PageQuery>,
) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("bridge_types")
            .cloned()
            .unwrap_or_default()
            .as_slice(),
        &q,
    ))
}

async fn get_bridge_type(State(state): State<MockState>, Path(type_id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "bridge_types", "id", &type_id) {
        Some(idx) => Json(db["bridge_types"][idx].clone()).into_response(),
        None => not_found("bridge_type", &type_id),
    }
}

async fn render_bridge_type(
    State(state): State<MockState>,
    Path(type_id): Path<String>,
) -> Response {
    if find_index(&*state.db.read().await, "bridge_types", "id", &type_id).is_none() {
        return not_found("bridge_type", &type_id);
    }
    Json(json!({
        "registration": {"id": type_id, "as_token": "generated-as-token", "hs_token": "generated-hs-token"},
        "registration_yaml": format!("id: {type_id}\nas_token: generated-as-token\nhs_token: generated-hs-token\n"),
        "compose_yaml": format!("services:\n  {type_id}-bridge:\n    image: dock.mau.dev/mautrix/{type_id}:latest\n"),
        "bridge_resource_yaml": format!("apiVersion: hs.example/v1\nkind: Bridge\nmetadata:\n  name: {type_id}\n")
    }))
    .into_response()
}

async fn list_appservices(
    State(state): State<MockState>,
    Query(q): Query<PageQuery>,
) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("appservices")
            .cloned()
            .unwrap_or_default()
            .as_slice(),
        &q,
    ))
}

async fn create_appservice(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let registration = body.get("registration").cloned().unwrap_or(json!({}));
    let id = registration
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| hs_admin::model::new_id().to_lowercase());
    let item = json!({
        "id": id, "sender_localpart": registration.get("sender_localpart").cloned().unwrap_or(json!("bot")),
        "url": registration.get("url"), "namespaces": registration.get("namespaces").cloned().unwrap_or(json!({})),
        "rate_limited": false, "protocols": [], "paused": false, "health": "unknown",
        "created_at": hs_http::time::now_rfc3339(), "links": {"login_url": null}
    });
    state
        .db
        .write()
        .await
        .get_mut("appservices")
        .unwrap()
        .push(item.clone());
    emit_and_audit(
        &state,
        "appservices.create",
        &actor,
        ResourceRef::new("appservice", id),
        item.clone(),
    )
    .await;
    (StatusCode::CREATED, Json(item)).into_response()
}

async fn get_appservice(State(state): State<MockState>, Path(id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "appservices", "id", &id) {
        Some(idx) => Json(db["appservices"][idx].clone()).into_response(),
        None => not_found("appservice", &id),
    }
}

async fn patch_appservice(
    State(state): State<MockState>,
    Path(id): Path<String>,
    Json(patch): Json<Value>,
) -> Response {
    let mut db = state.db.write().await;
    match find_index(&db, "appservices", "id", &id) {
        Some(idx) => {
            if let (Some(target), Some(patch_obj)) = (
                db.get_mut("appservices").unwrap()[idx].as_object_mut(),
                patch.as_object(),
            ) {
                for (k, v) in patch_obj {
                    target.insert(k.clone(), v.clone());
                }
            }
            Json(db["appservices"][idx].clone()).into_response()
        }
        None => not_found("appservice", &id),
    }
}

async fn delete_appservice(
    State(state): State<MockState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    match find_index(&db, "appservices", "id", &id) {
        Some(idx) => {
            db.get_mut("appservices").unwrap().remove(idx);
            drop(db);
            emit_and_audit(
                &state,
                "appservices.delete",
                &actor,
                ResourceRef::new("appservice", id),
                json!({}),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => not_found("appservice", &id),
    }
}

async fn appservice_registration(
    State(state): State<MockState>,
    Path(id): Path<String>,
) -> Response {
    if find_index(&*state.db.read().await, "appservices", "id", &id).is_none() {
        return not_found("appservice", &id);
    }
    Json(json!({"id": id, "as_token": "mock-as-token", "hs_token": "mock-hs-token", "url": "http://bridge.internal:1234"})).into_response()
}

async fn appservice_health(State(state): State<MockState>, Path(id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "appservices", "id", &id) {
        Some(idx) => Json(json!({"status": db["appservices"][idx]["health"], "last_ping_at": hs_http::time::now_rfc3339(), "last_error": null})).into_response(),
        None => not_found("appservice", &id),
    }
}

async fn appservice_backlog(
    State(state): State<MockState>,
    Path(id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    if find_index(&*state.db.read().await, "appservices", "id", &id).is_none() {
        return not_found("appservice", &id);
    }
    let backlog = vec![
        json!({"transaction_id": "txn-42", "age_ms": 12_000, "attempts": 3, "last_error": "connection reset", "dead_lettered": false}),
    ];
    Json(page_json(&backlog, &q)).into_response()
}

macro_rules! appservice_bool_action {
    ($fn_name:ident, $field:literal, $value:expr, $action:literal) => {
        async fn $fn_name(
            State(state): State<MockState>,
            Path(id): Path<String>,
            headers: HeaderMap,
        ) -> Response {
            let actor = match require_auth(&state, &headers) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut db = state.db.write().await;
            let Some(idx) = find_index(&db, "appservices", "id", &id) else {
                return not_found("appservice", &id);
            };
            db.get_mut("appservices").unwrap()[idx][$field] = json!($value);
            let updated = db["appservices"][idx].clone();
            drop(db);
            emit_and_audit(
                &state,
                $action,
                &actor,
                ResourceRef::new("appservice", id),
                updated.clone(),
            )
            .await;
            Json(updated).into_response()
        }
    };
}

appservice_bool_action!(appservice_pause, "paused", true, "appservices.pause");
appservice_bool_action!(appservice_resume, "paused", false, "appservices.resume");

async fn appservice_ping(
    State(state): State<MockState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "appservices", "id", &id).is_none() {
        return not_found("appservice", &id);
    }
    emit_and_audit(
        &state,
        "appservices.ping",
        &actor,
        ResourceRef::new("appservice", id.clone()),
        json!({}),
    )
    .await;
    Json(json!({"id": id, "reachable": true})).into_response()
}

async fn appservice_rotate_tokens(
    State(state): State<MockState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "appservices", "id", &id).is_none() {
        return not_found("appservice", &id);
    }
    emit_and_audit(
        &state,
        "appservices.rotate_tokens",
        &actor,
        ResourceRef::new("appservice", id),
        json!({}),
    )
    .await;
    Json(json!({"as_token": format!("as-{}", hs_admin::model::new_id()), "hs_token": format!("hs-{}", hs_admin::model::new_id())})).into_response()
}

async fn appservice_replay(
    State(state): State<MockState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if find_index(&*state.db.read().await, "appservices", "id", &id).is_none() {
        return not_found("appservice", &id);
    }
    let task = mock_task(
        "appservice.replay",
        Some(ResourceRef::new("appservice", id.clone())),
    );
    state
        .db
        .write()
        .await
        .get_mut("tasks")
        .unwrap()
        .push(task.clone());
    emit_and_audit(
        &state,
        "appservices.replay",
        &actor,
        ResourceRef::new("appservice", id),
        task.clone(),
    )
    .await;
    (
        StatusCode::ACCEPTED,
        [(
            header::LOCATION,
            format!("/api/v1/tasks/{}", task["id"].as_str().unwrap()),
        )],
        Json(task),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// cluster
// ---------------------------------------------------------------------------------------------

async fn list_replicas(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("replicas").cloned().unwrap_or_default().as_slice(),
        &q,
    ))
}

async fn get_replica(State(state): State<MockState>, Path(id): Path<String>) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "replicas", "id", &id) {
        Some(idx) => Json(db["replicas"][idx].clone()).into_response(),
        None => not_found("replica", &id),
    }
}

macro_rules! replica_status_action {
    ($fn_name:ident, $value:expr, $action:literal) => {
        async fn $fn_name(
            State(state): State<MockState>,
            Path(id): Path<String>,
            headers: HeaderMap,
        ) -> Response {
            let actor = match require_auth(&state, &headers) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut db = state.db.write().await;
            let Some(idx) = find_index(&db, "replicas", "id", &id) else {
                return not_found("replica", &id);
            };
            db.get_mut("replicas").unwrap()[idx]["status"] = json!($value);
            let updated = db["replicas"][idx].clone();
            drop(db);
            emit_and_audit(
                &state,
                $action,
                &actor,
                ResourceRef::new("replica", id),
                updated.clone(),
            )
            .await;
            Json(updated).into_response()
        }
    };
}

replica_status_action!(replica_drain, "draining", "cluster.replicas.drain");
replica_status_action!(replica_undrain, "active", "cluster.replicas.undrain");

async fn list_shards(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Json<Value> {
    let db = state.db.read().await;
    Json(page_json(
        db.get("shards").cloned().unwrap_or_default().as_slice(),
        &q,
    ))
}

// ---------------------------------------------------------------------------------------------
// migration
// ---------------------------------------------------------------------------------------------

async fn get_migration(State(state): State<MockState>) -> Json<Value> {
    let db = state.db.read().await;
    Json(
        db.get("migration_status")
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(fixtures::migration_status),
    )
}

fn migration_action(
    status: &'static str,
) -> impl Fn(
    State<MockState>,
    HeaderMap,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
+ Clone {
    move |State(state): State<MockState>, headers: HeaderMap| {
        Box::pin(async move {
            let actor = match require_auth(&state, &headers) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut db = state.db.write().await;
            let mut current = db
                .get("migration_status")
                .and_then(|v| v.first().cloned())
                .unwrap_or_else(fixtures::migration_status);
            current["status"] = json!(status);
            db.insert("migration_status".to_string(), vec![current.clone()]);
            drop(db);
            emit_and_audit(
                &state,
                "migration.status_changed",
                &actor,
                ResourceRef::new("migration", "current"),
                current.clone(),
            )
            .await;
            Json(current).into_response()
        })
    }
}

// ---------------------------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------------------------

async fn list_config(State(state): State<MockState>) -> Json<Value> {
    Json(json!(
        state
            .db
            .read()
            .await
            .get("config_sections")
            .cloned()
            .unwrap_or_default()
    ))
}

async fn get_config_section(
    State(state): State<MockState>,
    Path(section): Path<String>,
) -> Response {
    let db = state.db.read().await;
    match find_index(&db, "config_sections", "name", &section) {
        Some(idx) => Json(db["config_sections"][idx].clone()).into_response(),
        None => not_found("config_section", &section),
    }
}

async fn patch_config_section(
    State(state): State<MockState>,
    Path(section): Path<String>,
    headers: HeaderMap,
    Json(patch): Json<Value>,
) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut db = state.db.write().await;
    let Some(idx) = find_index(&db, "config_sections", "name", &section) else {
        return not_found("config_section", &section);
    };
    if let (Some(values), Some(patch_obj)) = (
        db.get_mut("config_sections").unwrap()[idx]
            .get_mut("values")
            .and_then(|v| v.as_object_mut()),
        patch.as_object(),
    ) {
        for (k, v) in patch_obj {
            values.insert(k.clone(), v.clone());
        }
    }
    let updated = db["config_sections"][idx].clone();
    drop(db);
    emit_and_audit(
        &state,
        "config.update",
        &actor,
        ResourceRef::new("config_section", section),
        updated.clone(),
    )
    .await;
    Json(updated).into_response()
}

async fn config_reload(State(state): State<MockState>, headers: HeaderMap) -> Response {
    let actor = match require_auth(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let sections: Vec<String> = state
        .db
        .read()
        .await
        .get("config_sections")
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect();
    emit_and_audit(
        &state,
        "config.reload",
        &actor,
        ResourceRef::new("config_section", "*"),
        json!({"reloaded_sections": sections}),
    )
    .await;
    Json(json!({"reloaded_sections": sections, "errors": []})).into_response()
}

// ---------------------------------------------------------------------------------------------
// audit log
// ---------------------------------------------------------------------------------------------

async fn list_audit_log(State(state): State<MockState>, Query(q): Query<PageQuery>) -> Response {
    let filter = AuditFilter {
        limit: q.limit.unwrap_or(50),
        ..Default::default()
    };
    match state.audit.query(&filter).await {
        Ok(entries) => Json(json!({"items": entries, "next_cursor": null, "prev_cursor": null}))
            .into_response(),
        Err(e) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            e.to_string(),
        ),
    }
}

async fn get_audit_entry(State(state): State<MockState>, Path(id): Path<String>) -> Response {
    match state.audit.get(&id).await {
        Ok(Some(entry)) => Json(entry).into_response(),
        Ok(None) => not_found("audit_entry", &id),
        Err(e) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            e.to_string(),
        ),
    }
}
