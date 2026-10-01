//! The long tail of the Rooms area (RFC 0004 section 4.3): reading a room's state, timeline and
//! events, its aliases, its space hierarchy and the media it refers to; making a local user join
//! it; its forward extremities; and the three long-running operations -- purging its history,
//! quarantining its media and deleting it -- which run as tasks ([`crate::tasks`]).
//!
//! [`RoomContentSource`] is what these operations read and act through. `hs-room` implements it
//! over the room registry (`hs_room::admin::RoomRegistryDirectory`); [`InMemoryRoomContent`] is
//! for this crate's tests.
//!
//! # Scopes (decision 0013)
//!
//! A room's metadata -- its state, members, aliases, hierarchy and media -- is readable with
//! `moderation:read`. Message content -- the timeline and any single event -- needs `admin:read`,
//! and every such read is recorded in the audit log as `rooms.content.read`, with the path that
//! was read, although it changes nothing: who read whose messages is exactly what an audit log is
//! for. A moderator sees the content of what was reported to them through `reports.*`.
//!
//! # Tasks
//!
//! `rooms.purge_history`, `rooms.delete` and `rooms.media.quarantine` answer `202` with a task
//! started through [`crate::tasks::TaskRegistry::spawn`]: it reports progress, can be cancelled
//! (between batches), and ends `succeeded` with a result or `failed` with a problem. The request
//! itself is audited and published when it is accepted; the task publishes a `room.*` event of
//! its own when it finishes. In cluster mode every `/rooms/{room_id}/...` request reaches the
//! replica that owns the room (the room shard gate forwards it), so the task runs there.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::{Problem, ValidationError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::handler_kit::{authorize, check_replay, record, respond_and_remember, unwired};
use crate::model::{
    AdminRoomMember, AuditEntry, AuditOutcome, AuditRequest, Event, Page, Principal, ResourceRef,
    Scope,
};
use crate::router::AdminState;
use crate::sources::SourceError;
use crate::tasks::TaskContext;

// -------------------------------------------------------------------------------------------
// Wire shapes.
// -------------------------------------------------------------------------------------------

/// The OpenAPI `RoomEvent` schema: one event, as an administrator reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminRoomEvent {
    pub event_id: String,
    pub room_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub sender: String,
    pub content: serde_json::Value,
    pub origin_server_ts: i64,
    pub state_key: Option<String>,
    /// Redacted: `content` is what redaction left.
    pub redacted: bool,
    /// For an `m.room.redaction`, the event it redacts.
    pub redacts: Option<String>,
}

/// The OpenAPI `StateEvent` schema: one event of a room's current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminStateEvent {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub state_key: String,
    pub sender: String,
    pub content: serde_json::Value,
    pub origin_server_ts: i64,
}

/// The OpenAPI `EventContext` schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminEventContext {
    pub event: AdminRoomEvent,
    /// Older than `event`, nearest first.
    pub events_before: Vec<AdminRoomEvent>,
    /// Newer than `event`, nearest first.
    pub events_after: Vec<AdminRoomEvent>,
    /// The room's state as of `event`.
    pub state: Vec<AdminStateEvent>,
}

/// The OpenAPI `RoomAlias` schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRoomAlias {
    pub alias: String,
    /// Not recorded by this server; always `None` today.
    pub created_at: Option<String>,
    pub creator: Option<String>,
    /// Whether the room's `m.room.canonical_alias` names it.
    pub canonical: bool,
}

/// The OpenAPI `RoomHierarchyNode` schema: one room of a space's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminHierarchyNode {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub canonical_alias: Option<String>,
    pub room_type: Option<String>,
    pub join_rule: Option<String>,
    pub joined_members_count: Option<u64>,
    /// 0 for the room asked about, 1 for its children, and so on.
    pub depth: u32,
    /// Whether this server holds the room; one it does not is listed by id only.
    pub known: bool,
    /// The room ids its `m.space.child` state names.
    pub children: Vec<String>,
}

/// The OpenAPI `ForwardExtremity` schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminForwardExtremity {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub sender: String,
    pub depth: i64,
    pub origin_server_ts: i64,
    pub state_key: Option<String>,
}

/// The OpenAPI `ForwardExtremitiesPruned` schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardExtremitiesPruned {
    pub deleted: Vec<String>,
    pub remaining: Vec<AdminForwardExtremity>,
}

/// Which way a timeline read goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineDirection {
    /// Oldest first, towards newer events.
    Forward,
    /// Newest first, towards older events.
    Backward,
}

impl TimelineDirection {
    #[allow(clippy::result_large_err)] // `Problem` is the crate's error shape; see `handler_kit`.
    fn parse(raw: Option<&str>, default: Self) -> Result<Self, Problem> {
        match raw {
            None => Ok(default),
            Some("f") => Ok(Self::Forward),
            Some("b") => Ok(Self::Backward),
            Some(other) => Err(field_problem(
                "/dir",
                format!("dir is f or b, not {other:?}"),
            )),
        }
    }
}

/// One page of a room's timeline.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimelinePage {
    pub events: Vec<AdminRoomEvent>,
    /// Where the next page in the same direction starts; `None` at the end.
    pub next: Option<String>,
}

/// What `rooms.purge_history` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PurgeHistoryRequest {
    /// Purge events sent before this instant (milliseconds since the epoch).
    pub before_ts: Option<i64>,
    /// Purge events older than this one (it is kept).
    pub before_event_id: Option<String>,
    /// Purge events this server's own users sent too.
    pub delete_local_events: bool,
}

/// How a purge ended: the task's `result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PurgeHistoryOutcome {
    /// Events removed from the room's history.
    pub purged: u64,
    /// Events before the point kept because they are state.
    pub kept_state: u64,
    /// Events before the point kept because a local user sent them.
    pub kept_local: u64,
}

/// `rooms.delete`'s `new_room`: where the members go.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct NewRoomRequest {
    pub name: Option<String>,
    /// A local user, who creates the room.
    pub creator: String,
}

/// What `rooms.delete` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeleteRoomRequest {
    pub block: bool,
    pub purge: bool,
    pub message: Option<String>,
    pub new_room: Option<NewRoomRequest>,
    /// The administrator deleting it (recorded as the block's reason).
    pub requested_by: String,
}

/// How a deletion ended: the task's `result` (Synapse's delete-room answer, plus what was done).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeleteRoomOutcome {
    pub kicked_users: Vec<String>,
    pub failed_to_kick_users: Vec<String>,
    pub local_aliases: Vec<String>,
    pub new_room_id: Option<String>,
    pub blocked: bool,
    pub purged: bool,
    pub events_deleted: u64,
}

/// Where a long-running room operation reports progress. [`TaskContext`] is the real one.
#[async_trait]
pub trait Progress: Send + Sync {
    /// `current` of `total` done, and what is happening.
    async fn report(&self, current: u64, total: Option<u64>, message: &str);
    /// Whether the task has been cancelled; work stops at its next step when it has.
    fn cancelled(&self) -> bool {
        false
    }
}

#[async_trait]
impl Progress for TaskContext {
    async fn report(&self, current: u64, total: Option<u64>, message: &str) {
        self.progress(current, total, Some("steps"), Some(message))
            .await;
    }

    fn cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

/// A [`Progress`] that reports nowhere, for callers that are not a task.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProgress;

#[async_trait]
impl Progress for NoProgress {
    async fn report(&self, _current: u64, _total: Option<u64>, _message: &str) {}
}

/// What the room long-tail operations read and act through. Every method naming a room answers
/// [`SourceError::NotFound`] for a room this server does not hold.
#[async_trait]
pub trait RoomContentSource: Send + Sync + 'static {
    /// Whether the room exists here.
    async fn exists(&self, room_id: &str) -> Result<bool, SourceError>;
    /// The room's current state.
    async fn state(&self, room_id: &str) -> Result<Vec<AdminStateEvent>, SourceError>;
    /// One page of the timeline from `from` (a token an earlier page handed out; the newest or
    /// oldest end when `None`). A token this source did not hand out is
    /// [`SourceError::InvalidField`] on `/cursor`.
    async fn timeline(
        &self,
        room_id: &str,
        from: Option<&str>,
        direction: TimelineDirection,
        limit: usize,
    ) -> Result<TimelinePage, SourceError>;
    /// One event; `room_id` `None` looks in every room. `None` for one this server does not hold
    /// (or has purged).
    async fn event(
        &self,
        room_id: Option<&str>,
        event_id: &str,
    ) -> Result<Option<AdminRoomEvent>, SourceError>;
    /// The event nearest `ts` (see `rooms.events.at`), if any.
    async fn event_at(
        &self,
        room_id: &str,
        ts: i64,
        direction: TimelineDirection,
    ) -> Result<Option<AdminRoomEvent>, SourceError>;
    /// Up to `limit` events either side of `event_id`, and the state at it.
    async fn context(
        &self,
        room_id: &str,
        event_id: &str,
        limit: usize,
    ) -> Result<Option<AdminEventContext>, SourceError>;
    /// The room's local aliases.
    async fn aliases(&self, room_id: &str) -> Result<Vec<AdminRoomAlias>, SourceError>;
    /// Adds a local alias, made by `by`. [`SourceError::Conflict`] if it is in use;
    /// [`SourceError::InvalidField`] (`/alias`) if it is malformed or not on this server.
    async fn add_alias(
        &self,
        room_id: &str,
        alias: &str,
        by: &str,
    ) -> Result<AdminRoomAlias, SourceError>;
    /// Removes a local alias of this room. [`SourceError::NotFound`] if the room has no such
    /// alias.
    async fn remove_alias(&self, room_id: &str, alias: &str) -> Result<(), SourceError>;
    /// The room and, if it is a space, the rooms below it, breadth first, to `max_depth`.
    async fn hierarchy(
        &self,
        room_id: &str,
        max_depth: u32,
    ) -> Result<Vec<AdminHierarchyNode>, SourceError>;
    /// Makes local user `user_id` join the room (inviting them first when the join rules need
    /// it). [`SourceError::InvalidField`] (`/user_id`) for a user who is not local or does not
    /// exist; [`SourceError::Conflict`] when the room will not have them (banned, say).
    async fn join(&self, room_id: &str, user_id: &str) -> Result<AdminRoomMember, SourceError>;
    /// The room's forward extremities, newest first.
    async fn forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<Vec<AdminForwardExtremity>, SourceError>;
    /// Keeps the newest forward extremity and forgets the rest.
    async fn prune_forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<ForwardExtremitiesPruned, SourceError>;
    /// The media the room refers to, as `(server_name, media_id)`.
    async fn media(&self, room_id: &str) -> Result<Vec<(String, String)>, SourceError>;
    /// Purges the room's history (see the OpenAPI description of `rooms.purge_history`).
    async fn purge_history(
        &self,
        room_id: &str,
        request: PurgeHistoryRequest,
        progress: &dyn Progress,
    ) -> Result<PurgeHistoryOutcome, SourceError>;
    /// Deletes the room (see the OpenAPI description of `rooms.delete`).
    async fn delete_room(
        &self,
        room_id: &str,
        request: DeleteRoomRequest,
        progress: &dyn Progress,
    ) -> Result<DeleteRoomOutcome, SourceError>;
    /// Told when a purge or deletion ends, for metrics: `operation` is `purge_history` or
    /// `delete`, `outcome` is `succeeded` or `failed`.
    fn observe(&self, _operation: &str, _outcome: &str, _elapsed: std::time::Duration) {}
}

// -------------------------------------------------------------------------------------------
// Handlers.
// -------------------------------------------------------------------------------------------

fn field_problem(pointer: &str, detail: String) -> Problem {
    Problem::validation_failed()
        .with_detail(detail.clone())
        .with_errors(vec![ValidationError::new(pointer, detail)])
}

fn source_error(e: SourceError, what: &str, instance: &str) -> Response {
    let problem = match e {
        SourceError::NotFound => Problem::not_found().with_detail(format!("{what} was not found")),
        other => other.to_problem(),
    };
    problem.with_instance(instance.to_owned()).into_response()
}

fn room_path(room_id: &str) -> String {
    format!("/api/v1/rooms/{room_id}")
}

/// The source, or the `503` saying it is not wired.
#[allow(clippy::result_large_err)]
fn content(state: &AdminState, instance: &str) -> Result<Arc<dyn RoomContentSource>, Response> {
    state
        .room_content
        .clone()
        .ok_or_else(|| unwired("room content", instance))
}

/// Records a read of message content (see the module docs). A read whose record cannot be
/// written is refused, like a mutation whose record cannot be.
#[allow(clippy::result_large_err)]
async fn audit_content_read(
    state: &AdminState,
    principal: &Principal,
    target: ResourceRef,
    path: &str,
) -> Result<(), Response> {
    let mut entry = AuditEntry::new(
        "rooms.content.read",
        principal.to_actor(),
        target,
        AuditOutcome::success(200),
    );
    entry.request = Some(AuditRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        request_id: None,
        idempotency_key: None,
        body: None,
    });
    state
        .audit
        .append(entry)
        .await
        .map_err(|e| e.to_problem().into_response())
}

/// `?limit&cursor&include_total` for the listings paged by offset.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct OffsetPageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    state_key: Option<String>,
}

fn invalid_offset_cursor(cursor: Option<&str>, instance: &str) -> Option<Response> {
    let cursor = cursor?;
    if cursor.is_empty() || cursor.parse::<usize>().is_ok() {
        return None;
    }
    let detail = format!("{cursor:?} is not a cursor this listing handed out");
    Some(
        Problem::invalid_cursor()
            .with_detail(detail.clone())
            .with_errors(vec![ValidationError::new("/cursor", detail)])
            .with_instance(instance.to_owned())
            .into_response(),
    )
}

/// `GET /api/v1/rooms/{room_id}/state` (`moderation:read`): the current state, by type then
/// state key; `type` and `state_key` narrow it.
pub(crate) async fn state_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Query(query): Query<OffsetPageQuery>,
) -> Response {
    let instance = format!("{}/state", room_path(&room_id));
    if let Err(r) = authorize(&state, &headers, Scope::ModerationRead, &instance).await {
        return r;
    }
    if let Some(r) = invalid_offset_cursor(query.cursor.as_deref(), &instance) {
        return r;
    }
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mut events = match source.state(&room_id).await {
        Ok(events) => events,
        Err(e) => return source_error(e, &format!("room {room_id}"), &instance),
    };
    events.retain(|e| {
        query.event_type.as_ref().is_none_or(|t| &e.event_type == t)
            && query.state_key.as_ref().is_none_or(|k| &e.state_key == k)
    });
    events.sort_by(|a, b| {
        a.event_type
            .cmp(&b.event_type)
            .then_with(|| a.state_key.cmp(&b.state_key))
    });
    axum::Json(Page::paginate(
        events,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    ))
    .into_response()
}

/// `?limit&cursor&dir` for the timeline.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessagesQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    dir: Option<String>,
}

/// `GET /api/v1/rooms/{room_id}/messages` (`admin:read`, audited): newest first by default.
pub(crate) async fn messages_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> Response {
    let instance = format!("{}/messages", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::AdminRead, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let direction =
        match TimelineDirection::parse(query.dir.as_deref(), TimelineDirection::Backward) {
            Ok(d) => d,
            Err(p) => return p.with_instance(instance).into_response(),
        };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    let page = match source
        .timeline(&room_id, query.cursor.as_deref(), direction, limit)
        .await
    {
        Ok(page) => page,
        Err(SourceError::InvalidField { detail, .. }) => {
            return Problem::invalid_cursor()
                .with_detail(detail.clone())
                .with_errors(vec![ValidationError::new("/cursor", detail)])
                .with_instance(instance)
                .into_response();
        }
        Err(e) => return source_error(e, &format!("room {room_id}"), &instance),
    };
    if let Err(r) = audit_content_read(
        &state,
        &principal,
        ResourceRef::new("room", room_id.clone()),
        &instance,
    )
    .await
    {
        return r;
    }
    axum::Json(Page {
        items: page.events,
        next_cursor: page.next,
        prev_cursor: None,
        total: None,
    })
    .into_response()
}

/// `GET /api/v1/rooms/{room_id}/events/{event_id}` (`admin:read`, audited).
pub(crate) async fn events_get_in_room(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((room_id, event_id)): Path<(String, String)>,
) -> Response {
    let instance = format!("{}/events/{event_id}", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::AdminRead, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match source.event(Some(&room_id), &event_id).await {
        Ok(Some(event)) => {
            if let Err(r) = audit_content_read(
                &state,
                &principal,
                ResourceRef::new("event", event_id.clone()),
                &instance,
            )
            .await
            {
                return r;
            }
            axum::Json(event).into_response()
        }
        Ok(None) => source_error(
            SourceError::NotFound,
            &format!("event {event_id}"),
            &instance,
        ),
        Err(e) => source_error(e, &format!("room {room_id}"), &instance),
    }
}

/// `GET /api/v1/events/{event_id}` (`admin:read`, audited): an event whose room is not known.
pub(crate) async fn events_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(event_id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/events/{event_id}");
    let principal = match authorize(&state, &headers, Scope::AdminRead, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match source.event(None, &event_id).await {
        Ok(Some(event)) => {
            if let Err(r) = audit_content_read(
                &state,
                &principal,
                ResourceRef::new("event", event_id.clone()),
                &instance,
            )
            .await
            {
                return r;
            }
            axum::Json(event).into_response()
        }
        Ok(None) | Err(SourceError::NotFound) => source_error(
            SourceError::NotFound,
            &format!("event {event_id}"),
            &instance,
        ),
        Err(e) => source_error(e, &format!("event {event_id}"), &instance),
    }
}

/// `?ts&dir` for `rooms.events.at`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct EventsAtQuery {
    ts: Option<String>,
    dir: Option<String>,
}

/// `GET /api/v1/rooms/{room_id}/events/at` (`admin:read`, audited).
pub(crate) async fn events_at(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Query(query): Query<EventsAtQuery>,
) -> Response {
    let instance = format!("{}/events/at", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::AdminRead, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let ts = match query.ts.as_deref().map(str::parse::<i64>) {
        Some(Ok(ts)) => ts,
        Some(Err(_)) => {
            return field_problem("/ts", "ts is milliseconds since the epoch".to_owned())
                .with_instance(instance)
                .into_response();
        }
        None => {
            return field_problem("/ts", "ts is required".to_owned())
                .with_instance(instance)
                .into_response();
        }
    };
    let direction = match TimelineDirection::parse(query.dir.as_deref(), TimelineDirection::Forward)
    {
        Ok(d) => d,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match source.event_at(&room_id, ts, direction).await {
        Ok(Some(event)) => {
            if let Err(r) = audit_content_read(
                &state,
                &principal,
                ResourceRef::new("room", room_id.clone()),
                &instance,
            )
            .await
            {
                return r;
            }
            axum::Json(event).into_response()
        }
        Ok(None) => Problem::not_found()
            .with_detail(format!(
                "room {room_id} has no event {} {ts}",
                if direction == TimelineDirection::Forward {
                    "at or after"
                } else {
                    "at or before"
                }
            ))
            .with_instance(instance)
            .into_response(),
        Err(e) => source_error(e, &format!("room {room_id}"), &instance),
    }
}

/// `?limit` for `rooms.events.context`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ContextQuery {
    limit: Option<usize>,
}

/// `GET /api/v1/rooms/{room_id}/events/{event_id}/context` (`admin:read`, audited).
pub(crate) async fn events_context(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((room_id, event_id)): Path<(String, String)>,
    Query(query): Query<ContextQuery>,
) -> Response {
    let instance = format!("{}/events/{event_id}/context", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::AdminRead, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let limit = query.limit.unwrap_or(10).min(100);
    match source.context(&room_id, &event_id, limit).await {
        Ok(Some(context)) => {
            if let Err(r) = audit_content_read(
                &state,
                &principal,
                ResourceRef::new("event", event_id.clone()),
                &instance,
            )
            .await
            {
                return r;
            }
            axum::Json(context).into_response()
        }
        Ok(None) => source_error(
            SourceError::NotFound,
            &format!("event {event_id}"),
            &instance,
        ),
        Err(e) => source_error(e, &format!("room {room_id}"), &instance),
    }
}

/// `GET /api/v1/rooms/{room_id}/aliases` (`moderation:read`).
pub(crate) async fn aliases_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Response {
    let instance = format!("{}/aliases", room_path(&room_id));
    if let Err(r) = authorize(&state, &headers, Scope::ModerationRead, &instance).await {
        return r;
    }
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match source.aliases(&room_id).await {
        Ok(mut aliases) => {
            aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
            axum::Json(aliases).into_response()
        }
        Err(e) => source_error(e, &format!("room {room_id}"), &instance),
    }
}

#[derive(Debug, Default, Deserialize)]
struct AliasBody {
    alias: Option<String>,
}

/// `POST /api/v1/rooms/{room_id}/aliases` (`moderation:write`): `201` with the alias.
pub(crate) async fn aliases_add(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("{}/aliases", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::ModerationWrite, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fingerprint = [room_id.as_bytes(), b"\n", &body].concat();
    if let Err(r) = check_replay(
        &state,
        &headers,
        "rooms.aliases.add",
        &fingerprint,
        &instance,
    ) {
        return r;
    }
    let request: AliasBody = match crate::router::parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let Some(alias) = request.alias.filter(|a| !a.is_empty()) else {
        return field_problem("/alias", "alias is required".to_owned())
            .with_instance(instance)
            .into_response();
    };
    let added = match source.add_alias(&room_id, &alias, &principal.id).await {
        Ok(added) => added,
        Err(e) => return source_error(e, &format!("room {room_id}"), &instance),
    };
    tracing::info!(room = %room_id, %alias, by = %principal.id, "an administrator added a room alias");
    if let Err(r) = record(
        &state,
        &principal,
        "rooms.aliases.add",
        "room.alias_added",
        ResourceRef::new("room", room_id.clone()),
        vec![crate::model::AuditChange {
            pointer: "/aliases/-".to_owned(),
            from: None,
            to: Some(json!(alias)),
        }],
        json!({ "room_id": room_id, "alias": alias }),
        201,
    )
    .await
    {
        return r;
    }
    respond_and_remember(
        &state,
        &headers,
        "rooms.aliases.add",
        &fingerprint,
        StatusCode::CREATED,
        &added,
        &[],
    )
}

/// `DELETE /api/v1/rooms/{room_id}/aliases/{alias}` (`moderation:write`): `204`.
pub(crate) async fn aliases_remove(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((room_id, alias)): Path<(String, String)>,
) -> Response {
    let instance = format!("{}/aliases/{alias}", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::ModerationWrite, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(e) = source.remove_alias(&room_id, &alias).await {
        return source_error(e, &format!("alias {alias} of room {room_id}"), &instance);
    }
    tracing::info!(room = %room_id, %alias, by = %principal.id, "an administrator removed a room alias");
    if let Err(r) = record(
        &state,
        &principal,
        "rooms.aliases.remove",
        "room.alias_removed",
        ResourceRef::new("room", room_id.clone()),
        vec![crate::model::AuditChange {
            pointer: "/aliases".to_owned(),
            from: Some(json!(alias)),
            to: None,
        }],
        json!({ "room_id": room_id, "alias": alias }),
        204,
    )
    .await
    {
        return r;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// How deep `rooms.hierarchy.get` walks a space.
const HIERARCHY_DEPTH: u32 = 5;

/// `GET /api/v1/rooms/{room_id}/hierarchy` (`moderation:read`): the room first, then the rooms
/// below it breadth first.
pub(crate) async fn hierarchy_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Query(query): Query<OffsetPageQuery>,
) -> Response {
    let instance = format!("{}/hierarchy", room_path(&room_id));
    if let Err(r) = authorize(&state, &headers, Scope::ModerationRead, &instance).await {
        return r;
    }
    if let Some(r) = invalid_offset_cursor(query.cursor.as_deref(), &instance) {
        return r;
    }
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match source.hierarchy(&room_id, HIERARCHY_DEPTH).await {
        Ok(nodes) => axum::Json(Page::paginate(
            nodes,
            query.cursor.as_deref(),
            query.limit,
            query.include_total.unwrap_or(false),
        ))
        .into_response(),
        Err(e) => source_error(e, &format!("room {room_id}"), &instance),
    }
}

#[derive(Debug, Default, Deserialize)]
struct JoinBody {
    user_id: Option<String>,
}

/// `POST /api/v1/rooms/{room_id}/join` (`admin:write`): the user's membership afterwards.
pub(crate) async fn join(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("{}/join", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fingerprint = [room_id.as_bytes(), b"\n", &body].concat();
    if let Err(r) = check_replay(&state, &headers, "rooms.join", &fingerprint, &instance) {
        return r;
    }
    let request: JoinBody = match crate::router::parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let Some(user_id) = request.user_id.filter(|u| !u.is_empty()) else {
        return field_problem("/user_id", "user_id is required".to_owned())
            .with_instance(instance)
            .into_response();
    };
    let member = match source.join(&room_id, &user_id).await {
        Ok(member) => member,
        Err(e) => return source_error(e, &format!("room {room_id}"), &instance),
    };
    tracing::info!(room = %room_id, user = %user_id, by = %principal.id, "an administrator joined a user to a room");
    if let Err(r) = record(
        &state,
        &principal,
        "rooms.join",
        "room.member_joined",
        ResourceRef::new("room", room_id.clone()),
        vec![crate::model::AuditChange {
            pointer: format!("/members/{user_id}/membership"),
            from: None,
            to: Some(json!(member.membership)),
        }],
        json!({ "room_id": room_id, "user_id": user_id }),
        200,
    )
    .await
    {
        return r;
    }
    respond_and_remember(
        &state,
        &headers,
        "rooms.join",
        &fingerprint,
        StatusCode::OK,
        &member,
        &[],
    )
}

/// `GET /api/v1/rooms/{room_id}/forward-extremities` (`admin:read`).
pub(crate) async fn forward_extremities_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Response {
    let instance = format!("{}/forward-extremities", room_path(&room_id));
    if let Err(r) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return r;
    }
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match source.forward_extremities(&room_id).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => source_error(e, &format!("room {room_id}"), &instance),
    }
}

/// `DELETE /api/v1/rooms/{room_id}/forward-extremities` (`admin:write`).
pub(crate) async fn forward_extremities_delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
) -> Response {
    let instance = format!("{}/forward-extremities", room_path(&room_id));
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let pruned = match source.prune_forward_extremities(&room_id).await {
        Ok(pruned) => pruned,
        Err(e) => return source_error(e, &format!("room {room_id}"), &instance),
    };
    tracing::info!(
        room = %room_id,
        by = %principal.id,
        deleted = pruned.deleted.len(),
        "an administrator pruned a room's forward extremities"
    );
    if let Err(r) = record(
        &state,
        &principal,
        "rooms.forward_extremities.delete",
        "room.forward_extremities_pruned",
        ResourceRef::new("room", room_id.clone()),
        vec![crate::model::AuditChange {
            pointer: "/forward_extremities".to_owned(),
            from: Some(json!(pruned.deleted.len() + pruned.remaining.len())),
            to: Some(json!(pruned.remaining.len())),
        }],
        json!({ "room_id": room_id, "deleted": pruned.deleted }),
        200,
    )
    .await
    {
        return r;
    }
    axum::Json(pruned).into_response()
}

/// The media a room refers to that this server holds, in the order the room mentions it.
#[allow(clippy::result_large_err)] // `Problem` is the crate's error shape; see `handler_kit`.
async fn held_media(
    state: &AdminState,
    source: &dyn RoomContentSource,
    room_id: &str,
    instance: &str,
) -> Result<Vec<crate::media::AdminMediaItem>, Response> {
    let Some(media) = &state.media else {
        return Err(unwired("media", instance));
    };
    let referenced = source
        .media(room_id)
        .await
        .map_err(|e| source_error(e, &format!("room {room_id}"), instance))?;
    let mut out = Vec::new();
    for (server, id) in referenced {
        match media.get(&server, &id).await {
            Ok(Some(item)) => out.push(item),
            Ok(None) | Err(SourceError::NotFound) => {}
            Err(e) => return Err(e.to_problem().with_instance(instance).into_response()),
        }
    }
    Ok(out)
}

/// `GET /api/v1/rooms/{room_id}/media` (`moderation:read`): the media the room's messages and
/// state refer to, that this server holds.
pub(crate) async fn media_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Query(query): Query<OffsetPageQuery>,
) -> Response {
    let instance = format!("{}/media", room_path(&room_id));
    if let Err(r) = authorize(&state, &headers, Scope::ModerationRead, &instance).await {
        return r;
    }
    if let Some(r) = invalid_offset_cursor(query.cursor.as_deref(), &instance) {
        return r;
    }
    let source = match content(&state, &instance) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match held_media(&state, source.as_ref(), &room_id, &instance).await {
        Ok(items) => axum::Json(Page::paginate(
            items,
            query.cursor.as_deref(),
            query.limit,
            query.include_total.unwrap_or(false),
        ))
        .into_response(),
        Err(r) => r,
    }
}

/// Answers a started task: `202`, its `Location`, remembered under the idempotency key.
fn accepted(
    state: &AdminState,
    headers: &HeaderMap,
    operation_id: &str,
    fingerprint: &[u8],
    task: &crate::model::Task,
) -> Response {
    respond_and_remember(
        state,
        headers,
        operation_id,
        fingerprint,
        StatusCode::ACCEPTED,
        task,
        &[("location", format!("/api/v1/tasks/{}", task.id))],
    )
}

/// Everything a task-starting handler checks before it starts one: scope, the sources, the
/// idempotency key and that the room exists.
struct TaskStart {
    principal: Principal,
    source: Arc<dyn RoomContentSource>,
    tasks: Arc<crate::tasks::TaskRegistry>,
    fingerprint: Vec<u8>,
}

#[allow(clippy::result_large_err)] // `Problem` is the crate's error shape; see `handler_kit`.
async fn begin_task(
    state: &AdminState,
    headers: &HeaderMap,
    room_id: &str,
    body: &[u8],
    operation_id: &str,
    instance: &str,
) -> Result<TaskStart, Response> {
    let principal = authorize(state, headers, Scope::ModerationWrite, instance).await?;
    let source = content(state, instance)?;
    let Some(tasks) = state.tasks.clone() else {
        return Err(unwired("tasks", instance));
    };
    let fingerprint = [room_id.as_bytes(), b"\n", body].concat();
    check_replay(state, headers, operation_id, &fingerprint, instance)?;
    match source.exists(room_id).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(source_error(
                SourceError::NotFound,
                &format!("room {room_id}"),
                instance,
            ));
        }
        Err(e) => return Err(source_error(e, &format!("room {room_id}"), instance)),
    }
    Ok(TaskStart {
        principal,
        source,
        tasks,
        fingerprint,
    })
}

fn to_task_problem(e: SourceError) -> Problem {
    e.to_problem()
}

/// `POST /api/v1/rooms/{room_id}/media/quarantine` (`moderation:write`, Task): quarantines every
/// piece of media the room refers to that this server holds, except protected media.
pub(crate) async fn media_quarantine(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("{}/media/quarantine", room_path(&room_id));
    let op = "rooms.media.quarantine";
    let start = match begin_task(&state, &headers, &room_id, &body, op, &instance).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let items = match held_media(&state, start.source.as_ref(), &room_id, &instance).await {
        Ok(items) => items,
        Err(r) => return r,
    };
    let Some(media) = state.media.clone() else {
        return unwired("media", &instance);
    };
    let by = start.principal.id.clone();
    let events = state.events.clone();
    let actor = start.principal.to_actor();
    let task_room = room_id.clone();
    let task = match start
        .tasks
        .spawn(
            op,
            Some(ResourceRef::new("room", room_id.clone())),
            start.principal.to_actor(),
            move |ctx| async move {
                let total = items.len() as u64;
                let (mut quarantined, mut already, mut protected) = (0u64, 0u64, 0u64);
                for (done, item) in items.iter().enumerate() {
                    if item.protected {
                        protected += 1;
                    } else if item.quarantined {
                        already += 1;
                    } else {
                        media
                            .set_quarantined(&item.server_name, &item.media_id, Some(&by))
                            .await
                            .map_err(to_task_problem)?;
                        quarantined += 1;
                    }
                    ctx.report(done as u64 + 1, Some(total), "quarantining the room's media")
                        .await;
                }
                tracing::info!(room = %task_room, quarantined, already, protected, "a room's media was quarantined");
                events.publish(
                    Event::new(
                        "room.media_quarantined",
                        json!({ "room_id": task_room, "quarantined": quarantined }),
                    )
                    .with_resource(ResourceRef::new("room", task_room.clone()))
                    .with_actor(actor),
                );
                Ok(json!({
                    "quarantined": quarantined,
                    "already_quarantined": already,
                    "protected": protected,
                }))
            },
        )
        .await
    {
        Ok(task) => task,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if let Err(r) = record(
        &state,
        &start.principal,
        op,
        "room.media_quarantine_started",
        ResourceRef::new("room", room_id.clone()),
        Vec::new(),
        json!({ "room_id": room_id, "task_id": task.id }),
        202,
    )
    .await
    {
        return r;
    }
    accepted(&state, &headers, op, &start.fingerprint, &task)
}

#[derive(Debug, Default, Deserialize)]
struct PurgeBody {
    before: Option<String>,
    before_event_id: Option<String>,
    delete_local_events: Option<bool>,
}

/// `POST /api/v1/rooms/{room_id}/purge-history` (`moderation:write`, Task).
pub(crate) async fn purge_history(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("{}/purge-history", room_path(&room_id));
    let op = "rooms.purge_history";
    // The scope check (in `begin_task`) comes before the body is read, so a caller without
    // `moderation:write` learns that, not what a valid purge request looks like.
    let start = match begin_task(&state, &headers, &room_id, &body, op, &instance).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let request: PurgeBody = match crate::router::parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let before_ts = match request.before.as_deref() {
        Some(raw) => match hs_http::time::parse_rfc3339(raw) {
            Ok(at) => Some(i64::try_from(at.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)),
            Err(_) => {
                return field_problem("/before", format!("{raw:?} is not an RFC 3339 date-time"))
                    .with_instance(instance)
                    .into_response();
            }
        },
        None => None,
    };
    if before_ts.is_none() && request.before_event_id.is_none() {
        return field_problem(
            "/before",
            "before (or before_event_id) is required: a purge names the point it purges before"
                .to_owned(),
        )
        .with_instance(instance)
        .into_response();
    }
    let purge = PurgeHistoryRequest {
        before_ts,
        before_event_id: request.before_event_id.clone(),
        delete_local_events: request.delete_local_events.unwrap_or(false),
    };
    let source = start.source.clone();
    let events = state.events.clone();
    let actor = start.principal.to_actor();
    let task_room = room_id.clone();
    let task = match start
        .tasks
        .spawn(
            op,
            Some(ResourceRef::new("room", room_id.clone())),
            start.principal.to_actor(),
            move |ctx| async move {
                let started = std::time::Instant::now();
                let outcome = source.purge_history(&task_room, purge, &ctx).await;
                let elapsed = started.elapsed();
                match outcome {
                    Ok(outcome) => {
                        source.observe("purge_history", "succeeded", elapsed);
                        tracing::info!(
                            room = %task_room,
                            purged = outcome.purged,
                            kept_state = outcome.kept_state,
                            kept_local = outcome.kept_local,
                            "a room's history was purged"
                        );
                        events.publish(
                            Event::new(
                                "room.history_purged",
                                json!({ "room_id": task_room, "purged": outcome.purged }),
                            )
                            .with_resource(ResourceRef::new("room", task_room.clone()))
                            .with_actor(actor),
                        );
                        serde_json::to_value(outcome).map_err(|_| Problem::internal())
                    }
                    Err(e) => {
                        source.observe("purge_history", "failed", elapsed);
                        tracing::warn!(room = %task_room, error = %e, "purging a room's history failed");
                        Err(e.to_problem())
                    }
                }
            },
        )
        .await
    {
        Ok(task) => task,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    tracing::info!(room = %room_id, by = %start.principal.id, task = %task.id, "an administrator asked to purge a room's history");
    if let Err(r) = record(
        &state,
        &start.principal,
        op,
        "room.purge_started",
        ResourceRef::new("room", room_id.clone()),
        Vec::new(),
        json!({
            "room_id": room_id,
            "task_id": task.id,
            "before": request.before,
            "before_event_id": request.before_event_id,
            "delete_local_events": request.delete_local_events.unwrap_or(false),
        }),
        202,
    )
    .await
    {
        return r;
    }
    accepted(&state, &headers, op, &start.fingerprint, &task)
}

#[derive(Debug, Default, Deserialize)]
struct DeleteBody {
    block: Option<bool>,
    purge: Option<bool>,
    message: Option<String>,
    new_room: Option<NewRoomBody>,
}

#[derive(Debug, Default, Deserialize)]
struct NewRoomBody {
    name: Option<String>,
    creator: Option<String>,
}

/// `POST /api/v1/rooms/{room_id}/delete` (`moderation:write`, Task).
pub(crate) async fn delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("{}/delete", room_path(&room_id));
    let op = "rooms.delete";
    let request: DeleteBody = match crate::router::parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let new_room = match request.new_room {
        Some(NewRoomBody {
            name,
            creator: Some(creator),
        }) if !creator.is_empty() => Some(NewRoomRequest { name, creator }),
        Some(_) => {
            return field_problem(
                "/new_room/creator",
                "new_room.creator is required: a local user makes the new room".to_owned(),
            )
            .with_instance(instance)
            .into_response();
        }
        None => None,
    };
    let start = match begin_task(&state, &headers, &room_id, &body, op, &instance).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let deletion = DeleteRoomRequest {
        block: request.block.unwrap_or(false),
        purge: request.purge.unwrap_or(true),
        message: request.message.clone(),
        new_room,
        requested_by: start.principal.id.clone(),
    };
    let block = deletion.block;
    let purge = deletion.purge;
    let source = start.source.clone();
    let events = state.events.clone();
    let actor = start.principal.to_actor();
    let task_room = room_id.clone();
    let task = match start
        .tasks
        .spawn(
            op,
            Some(ResourceRef::new("room", room_id.clone())),
            start.principal.to_actor(),
            move |ctx| async move {
                let started = std::time::Instant::now();
                let outcome = source.delete_room(&task_room, deletion, &ctx).await;
                let elapsed = started.elapsed();
                match outcome {
                    Ok(outcome) => {
                        source.observe("delete", "succeeded", elapsed);
                        tracing::info!(
                            room = %task_room,
                            kicked = outcome.kicked_users.len(),
                            failed = outcome.failed_to_kick_users.len(),
                            purged = outcome.purged,
                            blocked = outcome.blocked,
                            "a room was deleted"
                        );
                        events.publish(
                            Event::new(
                                "room.deleted",
                                json!({
                                    "room_id": task_room,
                                    "purged": outcome.purged,
                                    "blocked": outcome.blocked,
                                    "new_room_id": outcome.new_room_id,
                                }),
                            )
                            .with_resource(ResourceRef::new("room", task_room.clone()))
                            .with_actor(actor),
                        );
                        serde_json::to_value(outcome).map_err(|_| Problem::internal())
                    }
                    Err(e) => {
                        source.observe("delete", "failed", elapsed);
                        tracing::warn!(room = %task_room, error = %e, "deleting a room failed");
                        Err(e.to_problem())
                    }
                }
            },
        )
        .await
    {
        Ok(task) => task,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    tracing::info!(room = %room_id, by = %start.principal.id, task = %task.id, block, purge, "an administrator asked to delete a room");
    if let Err(r) = record(
        &state,
        &start.principal,
        op,
        "room.delete_started",
        ResourceRef::new("room", room_id.clone()),
        vec![crate::model::AuditChange {
            pointer: "/deleted".to_owned(),
            from: Some(json!(false)),
            to: Some(json!(true)),
        }],
        json!({
            "room_id": room_id,
            "task_id": task.id,
            "block": block,
            "purge": purge,
        }),
        202,
    )
    .await
    {
        return r;
    }
    accepted(&state, &headers, op, &start.fingerprint, &task)
}

// -------------------------------------------------------------------------------------------
// In memory, for tests.
// -------------------------------------------------------------------------------------------

/// One room held by [`InMemoryRoomContent`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryRoom {
    /// Oldest first.
    pub timeline: Vec<AdminRoomEvent>,
    pub state: Vec<AdminStateEvent>,
    pub aliases: Vec<AdminRoomAlias>,
    pub members: Vec<AdminRoomMember>,
    pub extremities: Vec<AdminForwardExtremity>,
    pub media: Vec<(String, String)>,
    pub children: Vec<String>,
    pub name: Option<String>,
    pub room_type: Option<String>,
    pub blocked: bool,
}

/// A [`RoomContentSource`] over rooms held in memory: the timeline is a list, a purge removes
/// messages older than the point, and a deletion removes the room.
#[derive(Debug, Default)]
pub struct InMemoryRoomContent {
    rooms: RwLock<BTreeMap<String, InMemoryRoom>>,
}

impl InMemoryRoomContent {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a room.
    #[must_use]
    pub fn with_room(self, room_id: &str, room: InMemoryRoom) -> Self {
        self.write().insert(room_id.to_owned(), room);
        self
    }

    /// A copy of one room, for assertions.
    #[must_use]
    pub fn room(&self, room_id: &str) -> Option<InMemoryRoom> {
        self.read().get(room_id).cloned()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, InMemoryRoom>> {
        self.rooms
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, InMemoryRoom>> {
        self.rooms
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn with<T>(&self, room_id: &str, f: impl FnOnce(&InMemoryRoom) -> T) -> Result<T, SourceError> {
        self.read().get(room_id).map(f).ok_or(SourceError::NotFound)
    }
}

#[async_trait]
impl RoomContentSource for InMemoryRoomContent {
    async fn exists(&self, room_id: &str) -> Result<bool, SourceError> {
        Ok(self.read().contains_key(room_id))
    }

    async fn state(&self, room_id: &str) -> Result<Vec<AdminStateEvent>, SourceError> {
        self.with(room_id, |r| r.state.clone())
    }

    async fn timeline(
        &self,
        room_id: &str,
        from: Option<&str>,
        direction: TimelineDirection,
        limit: usize,
    ) -> Result<TimelinePage, SourceError> {
        let events = self.with(room_id, |r| r.timeline.clone())?;
        let len = events.len();
        let start = match from {
            Some(raw) => raw
                .parse::<usize>()
                .map_err(|_| SourceError::InvalidField {
                    pointer: "/cursor",
                    detail: format!("{raw:?} is not a cursor this timeline handed out"),
                })?,
            None => match direction {
                TimelineDirection::Backward => len,
                TimelineDirection::Forward => 0,
            },
        }
        .min(len);
        let (page, next): (Vec<AdminRoomEvent>, Option<String>) = match direction {
            TimelineDirection::Backward => {
                let lo = start.saturating_sub(limit);
                let page = events[lo..start].iter().rev().cloned().collect();
                (page, (lo > 0).then(|| lo.to_string()))
            }
            TimelineDirection::Forward => {
                let hi = (start + limit).min(len);
                let page = events[start..hi].to_vec();
                (page, (hi < len).then(|| hi.to_string()))
            }
        };
        Ok(TimelinePage { events: page, next })
    }

    async fn event(
        &self,
        room_id: Option<&str>,
        event_id: &str,
    ) -> Result<Option<AdminRoomEvent>, SourceError> {
        let rooms = self.read();
        if let Some(room_id) = room_id
            && !rooms.contains_key(room_id)
        {
            return Err(SourceError::NotFound);
        }
        Ok(rooms
            .iter()
            .filter(|(id, _)| room_id.is_none_or(|r| r == id.as_str()))
            .flat_map(|(_, r)| r.timeline.iter())
            .find(|e| e.event_id == event_id)
            .cloned())
    }

    async fn event_at(
        &self,
        room_id: &str,
        ts: i64,
        direction: TimelineDirection,
    ) -> Result<Option<AdminRoomEvent>, SourceError> {
        self.with(room_id, |r| match direction {
            TimelineDirection::Forward => r
                .timeline
                .iter()
                .filter(|e| e.origin_server_ts >= ts)
                .min_by_key(|e| e.origin_server_ts)
                .cloned(),
            TimelineDirection::Backward => r
                .timeline
                .iter()
                .filter(|e| e.origin_server_ts <= ts)
                .max_by_key(|e| e.origin_server_ts)
                .cloned(),
        })
    }

    async fn context(
        &self,
        room_id: &str,
        event_id: &str,
        limit: usize,
    ) -> Result<Option<AdminEventContext>, SourceError> {
        self.with(room_id, |r| {
            let at = r.timeline.iter().position(|e| e.event_id == event_id)?;
            Some(AdminEventContext {
                event: r.timeline[at].clone(),
                events_before: r.timeline[at.saturating_sub(limit)..at]
                    .iter()
                    .rev()
                    .cloned()
                    .collect(),
                events_after: r.timeline[at + 1..(at + 1 + limit).min(r.timeline.len())].to_vec(),
                state: r.state.clone(),
            })
        })
    }

    async fn aliases(&self, room_id: &str) -> Result<Vec<AdminRoomAlias>, SourceError> {
        self.with(room_id, |r| r.aliases.clone())
    }

    async fn add_alias(
        &self,
        room_id: &str,
        alias: &str,
        by: &str,
    ) -> Result<AdminRoomAlias, SourceError> {
        if !alias.starts_with('#') || !alias.contains(':') {
            return Err(SourceError::InvalidField {
                pointer: "/alias",
                detail: format!("{alias:?} is not a room alias"),
            });
        }
        let mut rooms = self.write();
        if rooms
            .values()
            .any(|r| r.aliases.iter().any(|a| a.alias == alias))
        {
            return Err(SourceError::Conflict(format!("{alias} is already in use")));
        }
        let room = rooms.get_mut(room_id).ok_or(SourceError::NotFound)?;
        let added = AdminRoomAlias {
            alias: alias.to_owned(),
            created_at: None,
            creator: Some(by.to_owned()),
            canonical: false,
        };
        room.aliases.push(added.clone());
        Ok(added)
    }

    async fn remove_alias(&self, room_id: &str, alias: &str) -> Result<(), SourceError> {
        let mut rooms = self.write();
        let room = rooms.get_mut(room_id).ok_or(SourceError::NotFound)?;
        let before = room.aliases.len();
        room.aliases.retain(|a| a.alias != alias);
        if room.aliases.len() == before {
            return Err(SourceError::NotFound);
        }
        Ok(())
    }

    async fn hierarchy(
        &self,
        room_id: &str,
        max_depth: u32,
    ) -> Result<Vec<AdminHierarchyNode>, SourceError> {
        let rooms = self.read();
        if !rooms.contains_key(room_id) {
            return Err(SourceError::NotFound);
        }
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = std::collections::VecDeque::from([(room_id.to_owned(), 0u32)]);
        while let Some((id, depth)) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let room = rooms.get(&id);
            let children = room.map(|r| r.children.clone()).unwrap_or_default();
            if depth < max_depth {
                for child in &children {
                    queue.push_back((child.clone(), depth + 1));
                }
            }
            out.push(AdminHierarchyNode {
                room_id: id,
                name: room.and_then(|r| r.name.clone()),
                topic: None,
                canonical_alias: None,
                room_type: room.and_then(|r| r.room_type.clone()),
                join_rule: None,
                joined_members_count: room
                    .map(|r| r.members.iter().filter(|m| m.membership == "join").count() as u64),
                depth,
                known: room.is_some(),
                children,
            });
        }
        Ok(out)
    }

    async fn join(&self, room_id: &str, user_id: &str) -> Result<AdminRoomMember, SourceError> {
        let mut rooms = self.write();
        let room = rooms.get_mut(room_id).ok_or(SourceError::NotFound)?;
        if !user_id.starts_with('@') {
            return Err(SourceError::InvalidField {
                pointer: "/user_id",
                detail: format!("{user_id:?} is not a user id"),
            });
        }
        if let Some(member) = room.members.iter_mut().find(|m| m.user_id == user_id) {
            if member.membership == "ban" {
                return Err(SourceError::Conflict(format!(
                    "{user_id} is banned from {room_id}"
                )));
            }
            member.membership = "join".to_owned();
            return Ok(member.clone());
        }
        let member = AdminRoomMember {
            user_id: user_id.to_owned(),
            membership: "join".to_owned(),
            display_name: None,
            avatar_url: None,
        };
        room.members.push(member.clone());
        Ok(member)
    }

    async fn forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<Vec<AdminForwardExtremity>, SourceError> {
        self.with(room_id, |r| r.extremities.clone())
    }

    async fn prune_forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<ForwardExtremitiesPruned, SourceError> {
        let mut rooms = self.write();
        let room = rooms.get_mut(room_id).ok_or(SourceError::NotFound)?;
        let deleted: Vec<String> = room
            .extremities
            .iter()
            .skip(1)
            .map(|e| e.event_id.clone())
            .collect();
        room.extremities.truncate(1);
        Ok(ForwardExtremitiesPruned {
            deleted,
            remaining: room.extremities.clone(),
        })
    }

    async fn media(&self, room_id: &str) -> Result<Vec<(String, String)>, SourceError> {
        self.with(room_id, |r| r.media.clone())
    }

    async fn purge_history(
        &self,
        room_id: &str,
        request: PurgeHistoryRequest,
        progress: &dyn Progress,
    ) -> Result<PurgeHistoryOutcome, SourceError> {
        let outcome = {
            let mut rooms = self.write();
            let room = rooms.get_mut(room_id).ok_or(SourceError::NotFound)?;
            let newest = room.timeline.last().map(|e| e.event_id.clone());
            let mut outcome = PurgeHistoryOutcome::default();
            room.timeline.retain(|e| {
                let before = request.before_ts.is_none_or(|ts| e.origin_server_ts < ts);
                if !before || Some(&e.event_id) == newest.as_ref() {
                    return true;
                }
                if e.state_key.is_some() {
                    outcome.kept_state += 1;
                    return true;
                }
                outcome.purged += 1;
                false
            });
            outcome
        };
        progress
            .report(outcome.purged, Some(outcome.purged), "purged")
            .await;
        Ok(outcome)
    }

    async fn delete_room(
        &self,
        room_id: &str,
        request: DeleteRoomRequest,
        progress: &dyn Progress,
    ) -> Result<DeleteRoomOutcome, SourceError> {
        let (kicked, aliases, events) = {
            let mut rooms = self.write();
            let room = rooms.get_mut(room_id).ok_or(SourceError::NotFound)?;
            let kicked: Vec<String> = room
                .members
                .iter()
                .filter(|m| m.membership == "join")
                .map(|m| m.user_id.clone())
                .collect();
            for member in &mut room.members {
                member.membership = "leave".to_owned();
            }
            let aliases: Vec<String> = room.aliases.drain(..).map(|a| a.alias).collect();
            let events = room.timeline.len() as u64;
            room.blocked = request.block;
            if request.purge {
                rooms.remove(room_id);
            }
            (kicked, aliases, events)
        };
        progress.report(1, Some(1), "deleted").await;
        Ok(DeleteRoomOutcome {
            kicked_users: kicked,
            failed_to_kick_users: Vec::new(),
            local_aliases: aliases,
            new_room_id: None,
            blocked: request.block,
            purged: request.purge,
            events_deleted: if request.purge { events } else { 0 },
        })
    }
}

#[cfg(test)]
mod tests;
