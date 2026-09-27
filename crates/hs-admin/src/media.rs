//! The Media area of the admin API (RFC 0004 section 4, "Media"): the nine `media.*`
//! operations, and the [`MediaSource`] they read and change.
//!
//! [`MediaSource`] is deliberately small -- list, get, delete, quarantine, protect -- and
//! everything an operator can ask for beyond that is decided here, once, for every source:
//! filtering, searching and sorting the list, the rule that protection and quarantine exclude
//! each other, and which items the two bulk deletions (`POST /media/delete`, `POST
//! /media/purge-remote-cache`) select. The real source is `hs-media`'s, over its repository;
//! [`InMemoryMediaSource`] is for tests and `hs-admin-mock`.
//!
//! # The bulk deletions
//!
//! Both are declared as Tasks (RFC 0004 section 3.7) and answer `202` with one. They run to
//! completion inside the request -- a deletion is a metadata transaction and an object-store
//! delete per item -- so the Task comes back already `succeeded`, with what was deleted in its
//! `result`. There is no task store yet (the Tasks area is its own item), so `GET
//! /tasks/{id}` cannot look it up afterwards; the audit log and the `media.deleted` and
//! `task.succeeded` events are the durable record.
//!
//! "Before" means *unused since*: an item's last access if it has been served, its creation
//! otherwise (Synapse's `last_access_ts` semantics). Protected items are never selected. Cached
//! copies of remote media that are quarantined are not selected either: deleting the local copy
//! would let the next request fetch it again from its origin, quietly undoing the quarantine.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::{Problem, ValidationError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::auth::{ScopeDecision, require_scope};
use crate::idempotency::{Replay, StoredResponse};
use crate::model::{
    AuditChange, Event, Page, Principal, ResourceRef, Scope, Task, TaskProgress, TaskStatus,
};
use crate::router::{
    AdminState, authorization_header, idempotency_key, parse_optional_json, record_mutation,
    replay_response, source_unavailable,
};
use crate::sources::SourceError;

/// The OpenAPI `MediaItem` schema: one piece of content this server holds, uploaded here
/// (`origin: local`) or a cached copy of another server's (`origin: remote`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminMediaItem {
    pub server_name: String,
    pub media_id: String,
    /// `local` or `remote`.
    pub origin: String,
    /// The uploading user; `None` for a cached remote copy.
    pub uploader: Option<String>,
    pub upload_name: Option<String>,
    pub content_type: Option<String>,
    pub size_bytes: u64,
    /// RFC 3339 millisecond-precision UTC.
    pub created_at: String,
    /// When it was last served (download or thumbnail), at an hour's resolution; `None` if it
    /// never has been.
    pub last_accessed_at: Option<String>,
    pub quarantined: bool,
    /// Exempt from quarantine and from the bulk deletions.
    pub protected: bool,
}

impl AdminMediaItem {
    fn is_remote(&self) -> bool {
        self.origin == "remote"
    }

    /// Its last access, or its creation if it has never been served. `None` only for a
    /// timestamp that does not parse, which a bulk deletion then leaves alone.
    fn last_used(&self) -> Option<OffsetDateTime> {
        let at = self.last_accessed_at.as_deref().unwrap_or(&self.created_at);
        hs_http::time::parse_rfc3339(at).ok()
    }

    fn resource(&self) -> ResourceRef {
        ResourceRef::new(
            "media",
            format!("mxc://{}/{}", self.server_name, self.media_id),
        )
    }
}

/// Where the `media.*` operations read and change media. Implemented for real by `hs-media`
/// over its repository. Every method naming an item answers [`SourceError::NotFound`] for one
/// that does not exist.
#[async_trait]
pub trait MediaSource: Send + Sync + 'static {
    /// Every item, local and cached remote. The handlers filter, sort and page it.
    async fn list(&self) -> Result<Vec<AdminMediaItem>, SourceError>;
    async fn get(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<Option<AdminMediaItem>, SourceError>;
    /// Removes the item's bytes, thumbnails and metadata; answers the item as it was.
    async fn delete(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<AdminMediaItem, SourceError>;
    /// Quarantines the item (`by` is the administrator) or lifts its quarantine (`None`).
    async fn set_quarantined(
        &self,
        server_name: &str,
        media_id: &str,
        by: Option<&str>,
    ) -> Result<AdminMediaItem, SourceError>;
    async fn set_protected(
        &self,
        server_name: &str,
        media_id: &str,
        protected: bool,
    ) -> Result<AdminMediaItem, SourceError>;
}

/// A [`MediaSource`] over items held in memory.
#[derive(Debug, Default)]
pub struct InMemoryMediaSource {
    items: RwLock<BTreeMap<(String, String), AdminMediaItem>>,
}

impl InMemoryMediaSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_item(self, item: AdminMediaItem) -> Self {
        self.items
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((item.server_name.clone(), item.media_id.clone()), item);
        self
    }

    fn change(
        &self,
        server_name: &str,
        media_id: &str,
        f: impl FnOnce(&mut AdminMediaItem),
    ) -> Result<AdminMediaItem, SourceError> {
        let mut items = self
            .items
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let item = items
            .get_mut(&(server_name.to_owned(), media_id.to_owned()))
            .ok_or(SourceError::NotFound)?;
        f(item);
        Ok(item.clone())
    }
}

#[async_trait]
impl MediaSource for InMemoryMediaSource {
    async fn list(&self) -> Result<Vec<AdminMediaItem>, SourceError> {
        Ok(self
            .items
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect())
    }

    async fn get(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<Option<AdminMediaItem>, SourceError> {
        Ok(self
            .items
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(server_name.to_owned(), media_id.to_owned()))
            .cloned())
    }

    async fn delete(
        &self,
        server_name: &str,
        media_id: &str,
    ) -> Result<AdminMediaItem, SourceError> {
        self.items
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(server_name.to_owned(), media_id.to_owned()))
            .ok_or(SourceError::NotFound)
    }

    async fn set_quarantined(
        &self,
        server_name: &str,
        media_id: &str,
        by: Option<&str>,
    ) -> Result<AdminMediaItem, SourceError> {
        self.change(server_name, media_id, |item| {
            item.quarantined = by.is_some()
        })
    }

    async fn set_protected(
        &self,
        server_name: &str,
        media_id: &str,
        protected: bool,
    ) -> Result<AdminMediaItem, SourceError> {
        self.change(server_name, media_id, |item| item.protected = protected)
    }
}

// -------------------------------------------------------------------------------------------
// the list
// -------------------------------------------------------------------------------------------

/// `GET /media`'s query. Enumerated values arrive as strings and are checked here, so a bad one
/// is a `400` problem naming the parameter rather than axum's plain-text rejection.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MediaListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    q: Option<String>,
    sort: Option<String>,
    origin: Option<String>,
    quarantined: Option<bool>,
    protected: Option<bool>,
    uploader: Option<String>,
}

/// The fields `GET /media` sorts by, `-` first for descending. The default is newest first.
const SORT_FIELDS: &[&str] = &["created_at", "last_accessed_at", "size_bytes", "media_id"];

fn invalid_query(pointer: &str, detail: String) -> Problem {
    Problem::validation_failed()
        .with_detail(detail.clone())
        .with_errors(vec![ValidationError::new(pointer, detail)])
}

/// Filters, searches and sorts `items` as `query` asks. `Err` is the `400` for a bad
/// parameter.
#[allow(clippy::result_large_err)]
fn select_for_list(
    mut items: Vec<AdminMediaItem>,
    query: &MediaListQuery,
) -> Result<Vec<AdminMediaItem>, Problem> {
    if let Some(origin) = query.origin.as_deref()
        && !matches!(origin, "local" | "remote")
    {
        return Err(invalid_query(
            "/origin",
            format!("origin must be local or remote, not {origin:?}"),
        ));
    }
    let sort = query.sort.as_deref().unwrap_or("-created_at");
    let (descending, field) = match sort.strip_prefix('-') {
        Some(field) => (true, field),
        None => (false, sort),
    };
    if !SORT_FIELDS.contains(&field) {
        return Err(invalid_query(
            "/sort",
            format!(
                "media can be sorted by {}, not {sort:?}",
                SORT_FIELDS.join(", ")
            ),
        ));
    }
    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);
    items.retain(|item| {
        query.origin.as_deref().is_none_or(|o| item.origin == o)
            && query.quarantined.is_none_or(|q| item.quarantined == q)
            && query.protected.is_none_or(|p| item.protected == p)
            && query
                .uploader
                .as_deref()
                .is_none_or(|u| item.uploader.as_deref() == Some(u))
            && needle.as_deref().is_none_or(|needle| {
                [
                    Some(item.media_id.as_str()),
                    Some(item.server_name.as_str()),
                    item.upload_name.as_deref(),
                    item.uploader.as_deref(),
                    item.content_type.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|field| field.to_lowercase().contains(needle))
            })
    });
    items.sort_by(|a, b| {
        let ordering = match field {
            "last_accessed_at" => a.last_used().cmp(&b.last_used()),
            "size_bytes" => a.size_bytes.cmp(&b.size_bytes),
            "media_id" => a.media_id.cmp(&b.media_id),
            _ => a.created_at.cmp(&b.created_at),
        }
        .then_with(|| a.server_name.cmp(&b.server_name))
        .then_with(|| a.media_id.cmp(&b.media_id));
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(items)
}

// -------------------------------------------------------------------------------------------
// the bulk deletions
// -------------------------------------------------------------------------------------------

/// What a bulk deletion deletes: see the module docs.
#[derive(Debug, Clone)]
struct PurgeCriteria {
    /// Only items from this server; `None` for every server of the right origin.
    server_name: Option<String>,
    /// Only local items (`Some(false)`), only remote (`Some(true)`), or either.
    remote: Option<bool>,
    unused_since: OffsetDateTime,
    min_size_bytes: u64,
}

/// What [`select_for_purge`] picked, and what it left alone that an operator might have
/// expected it to take.
#[derive(Debug, Default)]
struct PurgeSelection {
    selected: Vec<AdminMediaItem>,
    skipped_protected: u64,
    skipped_quarantined: u64,
}

fn select_for_purge(items: Vec<AdminMediaItem>, criteria: &PurgeCriteria) -> PurgeSelection {
    let mut selection = PurgeSelection::default();
    for item in items {
        let in_scope = criteria
            .server_name
            .as_deref()
            .is_none_or(|s| item.server_name == s)
            && criteria.remote.is_none_or(|r| item.is_remote() == r)
            && item.size_bytes >= criteria.min_size_bytes
            && item.last_used().is_some_and(|t| t < criteria.unused_since);
        if !in_scope {
            continue;
        }
        if item.protected {
            selection.skipped_protected += 1;
        } else if item.is_remote() && item.quarantined {
            selection.skipped_quarantined += 1;
        } else {
            selection.selected.push(item);
        }
    }
    selection
}

/// `POST /media/delete`'s body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkDeleteBody {
    before: Option<String>,
    min_size_bytes: Option<u64>,
    server_name: Option<String>,
}

/// `POST /media/purge-remote-cache`'s body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeRemoteBody {
    before: Option<String>,
    server_name: Option<String>,
}

#[allow(clippy::result_large_err)]
fn parse_before(before: &str) -> Result<OffsetDateTime, Problem> {
    hs_http::time::parse_rfc3339(before).map_err(|_| {
        invalid_query(
            "/before",
            format!("before must be an RFC 3339 date-time, not {before:?}"),
        )
    })
}

/// One bulk deletion in progress: everything [`PurgeRun::run`] needs besides what to select.
struct PurgeRun<'a> {
    state: &'a AdminState,
    headers: &'a HeaderMap,
    raw_body: &'a [u8],
    media: Arc<dyn MediaSource>,
    principal: Principal,
    instance: &'a str,
    /// The audit action and idempotency scope.
    operation_id: &'a str,
    /// The Task's `action`.
    task_action: &'a str,
}

impl PurgeRun<'_> {
    /// Deletes what `criteria` selects, one item at a time, and answers the finished Task. An
    /// item that cannot be deleted is counted and the rest carry on: a purge that stops at the
    /// first stubborn object would leave everything after it.
    async fn run(self, criteria: PurgeCriteria) -> Response {
        let PurgeRun {
            state,
            headers,
            raw_body,
            media,
            principal,
            instance,
            operation_id,
            task_action,
        } = self;
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
        let items = match media.list().await {
            Ok(items) => items,
            Err(e) => return e.to_problem().with_instance(instance).into_response(),
        };
        let mut task = Task::scheduled(task_action, None, principal.to_actor());
        task.started_at = Some(hs_http::time::now_rfc3339());
        let selection = select_for_purge(items, &criteria);
        let mut deleted_count = 0u64;
        let mut deleted_bytes = 0u64;
        let mut failed = Vec::new();
        for item in &selection.selected {
            match media.delete(&item.server_name, &item.media_id).await {
                Ok(gone) => {
                    deleted_count += 1;
                    deleted_bytes += gone.size_bytes;
                }
                // Deleted by someone else in the meantime: what was asked for is true.
                Err(SourceError::NotFound) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        media_id = %item.media_id,
                        "bulk media deletion: could not delete an item"
                    );
                    failed.push(format!("mxc://{}/{}", item.server_name, item.media_id));
                }
            }
        }
        let total = selection.selected.len() as u64;
        task.status = if failed.is_empty() || deleted_count > 0 {
            TaskStatus::Succeeded
        } else {
            TaskStatus::Failed
        };
        task.progress = Some(TaskProgress {
            current: total,
            total: Some(total),
            unit: Some("items".to_owned()),
            message: None,
        });
        task.result = Some(json!({
            "deleted_count": deleted_count,
            "deleted_bytes": deleted_bytes,
            "skipped_protected": selection.skipped_protected,
            "skipped_quarantined": selection.skipped_quarantined,
            "failed": failed,
        }));
        if task.status == TaskStatus::Failed {
            task.error = Some(Problem::unavailable().with_detail(format!(
                "none of the {total} selected items could be deleted"
            )));
        }
        task.finished_at = Some(hs_http::time::now_rfc3339());

        let target = ResourceRef::new("task", task.id.clone());
        if let Err(resp) = record_mutation(
            state,
            &principal,
            operation_id,
            "media.deleted",
            target.clone(),
            Vec::new(),
            json!({
                "count": deleted_count,
                "bytes": deleted_bytes,
                "server_name": criteria.server_name,
                "before": hs_http::time::format_rfc3339(criteria.unused_since),
            }),
        )
        .await
        {
            return resp;
        }
        let task_event = if task.status == TaskStatus::Succeeded {
            "task.succeeded"
        } else {
            "task.failed"
        };
        state.events.publish(
            Event::new(task_event, serde_json::to_value(&task).unwrap_or_default())
                .with_resource(target)
                .with_actor(principal.to_actor()),
        );

        let body = serde_json::to_vec(&task).unwrap_or_default();
        if let Some(key) = idempotency_key(headers) {
            state.idempotency.record(
                operation_id,
                key,
                raw_body,
                StoredResponse {
                    status: StatusCode::ACCEPTED.as_u16(),
                    content_type: "application/json".to_owned(),
                    body: body.clone(),
                },
            );
        }
        (
            StatusCode::ACCEPTED,
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/json".to_owned(),
                ),
                (
                    axum::http::header::LOCATION,
                    format!("/api/v1/tasks/{}", task.id),
                ),
            ],
            body,
        )
            .into_response()
    }
}

// -------------------------------------------------------------------------------------------
// handlers
// -------------------------------------------------------------------------------------------

/// Checks `scope` and finds the media source: the first step every handler here shares. `Err`
/// is the response to answer with.
#[allow(clippy::result_large_err)]
async fn authorize(
    state: &AdminState,
    headers: &HeaderMap,
    instance: &str,
    scope: Scope,
) -> Result<(Arc<dyn MediaSource>, Principal), Response> {
    match require_scope(
        state.verifier.as_ref(),
        authorization_header(headers),
        Some(scope),
    )
    .await
    {
        ScopeDecision::Allowed(principal) => match state.media.clone() {
            Some(media) => Ok((media, principal)),
            None => Err(source_unavailable("media repository", instance)),
        },
        ScopeDecision::Unauthenticated(p) | ScopeDecision::InsufficientScope(p) => {
            Err(p.with_instance(instance).into_response())
        }
    }
}

fn not_found(server_name: &str, media_id: &str, instance: &str) -> Response {
    Problem::not_found()
        .with_detail(format!(
            "this server holds no media mxc://{server_name}/{media_id}"
        ))
        .with_instance(instance)
        .into_response()
}

/// `GET /api/v1/media`: newest first unless `sort` says otherwise.
pub(crate) async fn media_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<MediaListQuery>,
) -> Response {
    let instance = "/api/v1/media";
    let (media, _) = match authorize(&state, &headers, instance, Scope::AdminRead).await {
        Ok(v) => v,
        Err(response) => return response,
    };
    let items = match media.list().await {
        Ok(items) => items,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    match select_for_list(items, &query) {
        Ok(items) => axum::Json(Page::paginate(
            items,
            query.cursor.as_deref(),
            query.limit,
            query.include_total.unwrap_or(false),
        ))
        .into_response(),
        Err(problem) => problem.with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/media/{server_name}/{media_id}`.
pub(crate) async fn media_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((server_name, media_id)): Path<(String, String)>,
) -> Response {
    let instance = format!("/api/v1/media/{server_name}/{media_id}");
    let (media, _) = match authorize(&state, &headers, &instance, Scope::AdminRead).await {
        Ok(v) => v,
        Err(response) => return response,
    };
    match media.get(&server_name, &media_id).await {
        Ok(Some(item)) => axum::Json(item).into_response(),
        Ok(None) => not_found(&server_name, &media_id, &instance),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `DELETE /api/v1/media/{server_name}/{media_id}` (`moderation:write`): gone for good, bytes,
/// thumbnails and all. Deleting a protected item is allowed -- protection guards against the
/// bulk deletions and quarantine, not against an administrator naming the item.
pub(crate) async fn media_delete_one(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((server_name, media_id)): Path<(String, String)>,
) -> Response {
    let instance = format!("/api/v1/media/{server_name}/{media_id}");
    let (media, principal) =
        match authorize(&state, &headers, &instance, Scope::ModerationWrite).await {
            Ok(v) => v,
            Err(response) => return response,
        };
    let item = match media.delete(&server_name, &media_id).await {
        Ok(item) => item,
        Err(SourceError::NotFound) => return not_found(&server_name, &media_id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        "media.delete_one",
        "media.deleted",
        item.resource(),
        Vec::new(),
        json!({
            "server_name": item.server_name,
            "media_id": item.media_id,
            "count": 1,
            "bytes": item.size_bytes,
        }),
    )
    .await
    {
        return resp;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Which flag a per-item action changes, and to what.
#[derive(Debug, Clone, Copy)]
enum FlagChange {
    Quarantine(bool),
    Protect(bool),
}

impl FlagChange {
    fn operation_id(self) -> &'static str {
        match self {
            FlagChange::Quarantine(true) => "media.quarantine",
            FlagChange::Quarantine(false) => "media.unquarantine",
            FlagChange::Protect(true) => "media.protect",
            FlagChange::Protect(false) => "media.unprotect",
        }
    }

    fn path_suffix(self) -> &'static str {
        match self {
            FlagChange::Quarantine(true) => "quarantine",
            FlagChange::Quarantine(false) => "unquarantine",
            FlagChange::Protect(true) => "protect",
            FlagChange::Protect(false) => "unprotect",
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            FlagChange::Quarantine(true) => "media.quarantined",
            FlagChange::Quarantine(false) => "media.unquarantined",
            FlagChange::Protect(true) => "media.protected",
            FlagChange::Protect(false) => "media.unprotected",
        }
    }

    fn pointer(self) -> &'static str {
        match self {
            FlagChange::Quarantine(_) => "/quarantined",
            FlagChange::Protect(_) => "/protected",
        }
    }

    fn current(self, item: &AdminMediaItem) -> bool {
        match self {
            FlagChange::Quarantine(_) => item.quarantined,
            FlagChange::Protect(_) => item.protected,
        }
    }

    fn target(self) -> bool {
        match self {
            FlagChange::Quarantine(v) | FlagChange::Protect(v) => v,
        }
    }

    /// Why this change is refused for `item`, if it is: protection and quarantine exclude each
    /// other, so an item is never both, and "protected" never silently means "quarantined but
    /// served anyway".
    fn refusal(self, item: &AdminMediaItem) -> Option<&'static str> {
        match self {
            FlagChange::Quarantine(true) if item.protected => {
                Some("this media is protected; unprotect it before quarantining it")
            }
            FlagChange::Protect(true) if item.quarantined => {
                Some("this media is quarantined; lift the quarantine before protecting it")
            }
            _ => None,
        }
    }
}

/// The shared body of quarantine, unquarantine, protect and unprotect.
async fn change_flag(
    state: AdminState,
    headers: HeaderMap,
    server_name: String,
    media_id: String,
    raw_body: axum::body::Bytes,
    change: FlagChange,
) -> Response {
    let instance = format!(
        "/api/v1/media/{server_name}/{media_id}/{}",
        change.path_suffix()
    );
    let (media, principal) =
        match authorize(&state, &headers, &instance, Scope::ModerationWrite).await {
            Ok(v) => v,
            Err(response) => return response,
        };
    let operation_id = change.operation_id();
    if let Some(key) = idempotency_key(&headers) {
        match state.idempotency.check(operation_id, key, &raw_body) {
            Replay::Same(stored) => return replay_response(stored),
            Replay::Mismatch => {
                return Problem::idempotency_key_payload_mismatch()
                    .with_instance(instance)
                    .into_response();
            }
            Replay::Fresh => {}
        }
    }
    let before = match media.get(&server_name, &media_id).await {
        Ok(Some(item)) => item,
        Ok(None) => return not_found(&server_name, &media_id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if let Some(reason) = change.refusal(&before) {
        return Problem::conflict()
            .with_detail(reason)
            .with_instance(instance)
            .into_response();
    }
    let result = match change {
        FlagChange::Quarantine(on) => {
            let by = on.then_some(principal.id.as_str());
            media.set_quarantined(&server_name, &media_id, by).await
        }
        FlagChange::Protect(on) => media.set_protected(&server_name, &media_id, on).await,
    };
    let item = match result {
        Ok(item) => item,
        Err(SourceError::NotFound) => return not_found(&server_name, &media_id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let changes = if change.current(&before) == change.target() {
        Vec::new()
    } else {
        vec![AuditChange {
            pointer: change.pointer().to_owned(),
            from: Some(json!(change.current(&before))),
            to: Some(json!(change.target())),
        }]
    };
    if let Err(resp) = record_mutation(
        &state,
        &principal,
        operation_id,
        change.event_type(),
        item.resource(),
        changes,
        json!({ "server_name": item.server_name, "media_id": item.media_id }),
    )
    .await
    {
        return resp;
    }
    let body = serde_json::to_vec(&item).unwrap_or_default();
    if let Some(key) = idempotency_key(&headers) {
        state.idempotency.record(
            operation_id,
            key,
            &raw_body,
            StoredResponse {
                status: 200,
                content_type: "application/json".to_owned(),
                body: body.clone(),
            },
        );
    }
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// `POST /api/v1/media/{server_name}/{media_id}/quarantine` (`moderation:write`): nobody but
/// an administrator can fetch it until the quarantine is lifted. `409` for protected media.
pub(crate) async fn media_quarantine(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((server_name, media_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let change = FlagChange::Quarantine(true);
    change_flag(state, headers, server_name, media_id, body, change).await
}

/// `POST /api/v1/media/{server_name}/{media_id}/unquarantine` (`moderation:write`).
pub(crate) async fn media_unquarantine(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((server_name, media_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let change = FlagChange::Quarantine(false);
    change_flag(state, headers, server_name, media_id, body, change).await
}

/// `POST /api/v1/media/{server_name}/{media_id}/protect` (`moderation:write`): exempt from
/// quarantine and the bulk deletions. `409` for quarantined media.
pub(crate) async fn media_protect(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((server_name, media_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let change = FlagChange::Protect(true);
    change_flag(state, headers, server_name, media_id, body, change).await
}

/// `POST /api/v1/media/{server_name}/{media_id}/unprotect` (`moderation:write`).
pub(crate) async fn media_unprotect(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((server_name, media_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let change = FlagChange::Protect(false);
    change_flag(state, headers, server_name, media_id, body, change).await
}

/// `POST /api/v1/media/delete` (`moderation:write`, Task): deletes media unused since `before`
/// (required: a bulk deletion never means "everything"), at least `min_size_bytes` large, from
/// `server_name` -- this server's own uploads when it is left out.
pub(crate) async fn media_delete_bulk(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/media/delete";
    let (media, principal) =
        match authorize(&state, &headers, instance, Scope::ModerationWrite).await {
            Ok(v) => v,
            Err(response) => return response,
        };
    let request: BulkDeleteBody = match parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let Some(before) = request.before.as_deref() else {
        return invalid_query(
            "/before",
            "before is required: a bulk deletion names how long media must have gone unused"
                .to_owned(),
        )
        .with_instance(instance)
        .into_response();
    };
    let unused_since = match parse_before(before) {
        Ok(t) => t,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let criteria = PurgeCriteria {
        remote: if request.server_name.is_some() {
            None
        } else {
            Some(false)
        },
        server_name: request.server_name,
        unused_since,
        min_size_bytes: request.min_size_bytes.unwrap_or(0),
    };
    let run = PurgeRun {
        state: &state,
        headers: &headers,
        raw_body: &body,
        media,
        principal,
        instance,
        operation_id: "media.delete_bulk",
        task_action: "media.delete",
    };
    run.run(criteria).await
}

/// `POST /api/v1/media/purge-remote-cache` (`admin:write`, Task): drops this server's copies
/// of other servers' media unused since `before` (now, when left out), optionally only those
/// from `server_name`. They are fetched again when next asked for.
pub(crate) async fn media_purge_remote_cache(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let instance = "/api/v1/media/purge-remote-cache";
    let (media, principal) = match authorize(&state, &headers, instance, Scope::AdminWrite).await {
        Ok(v) => v,
        Err(response) => return response,
    };
    let request: PurgeRemoteBody = match parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let unused_since = match request.before.as_deref() {
        Some(before) => match parse_before(before) {
            Ok(t) => t,
            Err(p) => return p.with_instance(instance).into_response(),
        },
        None => OffsetDateTime::now_utc(),
    };
    let criteria = PurgeCriteria {
        server_name: request.server_name,
        remote: Some(true),
        unused_since,
        min_size_bytes: 0,
    };
    let run = PurgeRun {
        state: &state,
        headers: &headers,
        raw_body: &body,
        media,
        principal,
        instance,
        operation_id: "media.purge_remote_cache",
        task_action: "media.purge_remote_cache",
    };
    run.run(criteria).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::audit::InMemoryAuditSink;
    use crate::auth::StaticVerifier;
    use crate::events::EventBus;
    use crate::model::{AuditEntry, PrincipalKind};
    use crate::router::build_router;

    fn principal(id: &str, scopes: Vec<Scope>) -> Principal {
        Principal {
            kind: PrincipalKind::User,
            id: id.into(),
            display_name: None,
            scopes,
            token_id: None,
            expires_at: None,
            issued_by: None,
        }
    }

    fn item(server: &str, id: &str, created: &str, size: u64) -> AdminMediaItem {
        AdminMediaItem {
            server_name: server.into(),
            media_id: id.into(),
            origin: if server == "example.org" {
                "local"
            } else {
                "remote"
            }
            .into(),
            uploader: (server == "example.org").then(|| "@alice:example.org".to_owned()),
            upload_name: Some(format!("{id}.png")),
            content_type: Some("image/png".into()),
            size_bytes: size,
            created_at: created.into(),
            last_accessed_at: None,
            quarantined: false,
            protected: false,
        }
    }

    fn source() -> InMemoryMediaSource {
        let mut accessed = item(
            "example.org",
            "old_but_used",
            "2026-01-01T00:00:00.000Z",
            50,
        );
        accessed.last_accessed_at = Some("2026-09-01T00:00:00.000Z".into());
        let mut protected = item("example.org", "protected", "2026-01-01T00:00:00.000Z", 10);
        protected.protected = true;
        let mut quarantined_remote = item("matrix.org", "bad", "2026-01-01T00:00:00.000Z", 5);
        quarantined_remote.quarantined = true;
        InMemoryMediaSource::new()
            .with_item(item("example.org", "old", "2026-01-01T00:00:00.000Z", 100))
            .with_item(item("example.org", "new", "2026-09-20T00:00:00.000Z", 200))
            .with_item(accessed)
            .with_item(protected)
            .with_item(item("matrix.org", "cached", "2026-02-01T00:00:00.000Z", 30))
            .with_item(item("other.org", "cached2", "2026-02-01T00:00:00.000Z", 40))
            .with_item(quarantined_remote)
    }

    struct Harness {
        router: axum::Router,
        events: Arc<EventBus>,
    }

    fn harness(media: Option<InMemoryMediaSource>) -> Harness {
        let verifier = StaticVerifier::new()
            .with_token(
                "admin-token",
                principal("@ops:example.org", vec![Scope::AdminWrite]),
            )
            .with_token(
                "moderator",
                principal("@mod:example.org", vec![Scope::ModerationWrite]),
            );
        let events = Arc::new(EventBus::new());
        let mut state = AdminState::new(
            Arc::new(verifier),
            Arc::new(InMemoryAuditSink::new()),
            events.clone(),
        );
        if let Some(media) = media {
            state = state.with_media(Arc::new(media));
        }
        let (router, _manifest) = build_router(state);
        Harness { router, events }
    }

    async fn call(
        h: &Harness,
        token: &str,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = h.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, headers, json)
    }

    async fn audit(h: &Harness, action: &str) -> Vec<AuditEntry> {
        let (status, _, body) = call(
            h,
            "admin-token",
            "GET",
            &format!("/api/v1/audit-log?action={action}"),
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_value(body["items"].clone()).unwrap()
    }

    fn ids(page: &serde_json::Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["media_id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn every_media_operation_is_503_until_a_source_is_wired() {
        let h = harness(None);
        for (method, uri) in [
            ("GET", "/api/v1/media"),
            ("GET", "/api/v1/media/example.org/old"),
            ("DELETE", "/api/v1/media/example.org/old"),
            ("POST", "/api/v1/media/example.org/old/quarantine"),
            ("POST", "/api/v1/media/example.org/old/unquarantine"),
            ("POST", "/api/v1/media/example.org/old/protect"),
            ("POST", "/api/v1/media/example.org/old/unprotect"),
            ("POST", "/api/v1/media/delete"),
            ("POST", "/api/v1/media/purge-remote-cache"),
        ] {
            let (status, _, _) = call(&h, "admin-token", method, uri, None, &[]).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn the_list_is_newest_first_and_filters_searches_and_sorts() {
        let h = harness(Some(source()));
        let (status, _, page) = call(&h, "admin-token", "GET", "/api/v1/media", None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ids(&page)[0], "new");
        assert_eq!(page["items"].as_array().unwrap().len(), 7);

        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?origin=remote&sort=size_bytes",
            None,
            &[],
        )
        .await;
        assert_eq!(ids(&page), ["bad", "cached", "cached2"]);

        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?quarantined=true",
            None,
            &[],
        )
        .await;
        assert_eq!(ids(&page), ["bad"]);

        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?protected=true",
            None,
            &[],
        )
        .await;
        assert_eq!(ids(&page), ["protected"]);

        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?q=CACHED2",
            None,
            &[],
        )
        .await;
        assert_eq!(ids(&page), ["cached2"]);

        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?uploader=%40alice%3Aexample.org&sort=-size_bytes&limit=2&include_total=true",
            None,
            &[],
        )
        .await;
        assert_eq!(ids(&page), ["new", "old"]);
        assert_eq!(page["total"], 4);
        assert_eq!(page["next_cursor"], "2");

        // Last used: an item never served counts from its creation.
        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?origin=local&sort=-last_accessed_at",
            None,
            &[],
        )
        .await;
        assert_eq!(ids(&page)[..2], ["new", "old_but_used"]);
    }

    #[tokio::test]
    async fn a_bad_sort_or_origin_is_a_400_naming_the_parameter() {
        let h = harness(Some(source()));
        for (uri, pointer) in [
            ("/api/v1/media?sort=colour", "/sort"),
            ("/api/v1/media?origin=mars", "/origin"),
        ] {
            let (status, _, problem) = call(&h, "admin-token", "GET", uri, None, &[]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(problem["errors"][0]["pointer"], pointer, "{problem}");
        }
    }

    #[tokio::test]
    async fn get_answers_the_item_or_404() {
        let h = harness(Some(source()));
        let (status, _, body) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media/matrix.org/cached",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["origin"], "remote");
        assert_eq!(body["uploader"], serde_json::Value::Null);
        assert_eq!(body["size_bytes"], 30);
        let (status, _, _) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media/example.org/nope",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_moderator_can_act_on_media_but_not_list_it_and_not_purge_the_cache() {
        let h = harness(Some(source()));
        let (status, _, _) = call(&h, "moderator", "GET", "/api/v1/media", None, &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "media.list needs admin:read");
        let (status, _, _) = call(
            &h,
            "moderator",
            "POST",
            "/api/v1/media/example.org/old/quarantine",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = call(
            &h,
            "moderator",
            "POST",
            "/api/v1/media/purge-remote-cache",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "the purge needs admin:write");
    }

    #[tokio::test]
    async fn quarantine_and_its_lifting_are_audited_and_published() {
        let h = harness(Some(source()));
        let mut rx = h.events.subscribe();
        let (status, _, body) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/example.org/old/quarantine",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["quarantined"], true);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.r#type, "media.quarantined");
        assert_eq!(event.resource.as_ref().unwrap().id, "mxc://example.org/old");
        let entries = audit(&h, "media.quarantine").await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].changes[0].pointer, "/quarantined");
        assert_eq!(entries[0].changes[0].to, Some(json!(true)));

        let (status, _, body) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/example.org/old/unquarantine",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["quarantined"], false);
        assert_eq!(rx.recv().await.unwrap().r#type, "media.unquarantined");
        assert_eq!(audit(&h, "media.unquarantine").await.len(), 1);
    }

    #[tokio::test]
    async fn protection_and_quarantine_exclude_each_other() {
        let h = harness(Some(source()));
        let (status, _, problem) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/example.org/protected/quarantine",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{problem}");
        assert!(audit(&h, "media.quarantine").await.is_empty());

        let (status, _, _) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/matrix.org/bad/protect",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, _, body) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/example.org/protected/unprotect",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["protected"], false);
        let (status, _, body) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/example.org/protected/quarantine",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["quarantined"], true);

        let (status, _, body) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/example.org/new/protect",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["protected"], true);
        assert_eq!(audit(&h, "media.protect").await.len(), 1);
        assert_eq!(audit(&h, "media.unprotect").await.len(), 1);
    }

    #[tokio::test]
    async fn an_idempotent_retry_replays_without_a_second_audit_entry() {
        let h = harness(Some(source()));
        for _ in 0..2 {
            let (status, _, body) = call(
                &h,
                "admin-token",
                "POST",
                "/api/v1/media/example.org/old/protect",
                None,
                &[("idempotency-key", "k1")],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["protected"], true);
        }
        assert_eq!(audit(&h, "media.protect").await.len(), 1);
    }

    #[tokio::test]
    async fn delete_one_removes_the_item_and_says_so() {
        let h = harness(Some(source()));
        let mut rx = h.events.subscribe();
        let (status, _, _) = call(
            &h,
            "moderator",
            "DELETE",
            "/api/v1/media/example.org/old",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.r#type, "media.deleted");
        assert_eq!(event.data["bytes"], 100);
        assert_eq!(audit(&h, "media.delete_one").await.len(), 1);
        let (status, _, _) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media/example.org/old",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = call(
            &h,
            "admin-token",
            "DELETE",
            "/api/v1/media/example.org/old",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn bulk_delete_takes_local_media_unused_since_the_date_and_spares_the_protected() {
        let h = harness(Some(source()));
        let mut rx = h.events.subscribe();
        let (status, headers, task) = call(
            &h,
            "moderator",
            "POST",
            "/api/v1/media/delete",
            Some(json!({"before": "2026-06-01T00:00:00Z"})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{task}");
        assert_eq!(task["status"], "succeeded");
        assert_eq!(task["action"], "media.delete");
        assert_eq!(
            headers.get("location").unwrap(),
            &format!("/api/v1/tasks/{}", task["id"].as_str().unwrap())
        );
        // `old` only: `old_but_used` was served in September, `new` is new, `protected` is
        // protected, and the remote copies are not this server's uploads.
        assert_eq!(task["result"]["deleted_count"], 1);
        assert_eq!(task["result"]["deleted_bytes"], 100);
        assert_eq!(task["result"]["skipped_protected"], 1);
        assert_eq!(rx.recv().await.unwrap().r#type, "media.deleted");
        assert_eq!(rx.recv().await.unwrap().r#type, "task.succeeded");
        assert_eq!(audit(&h, "media.delete_bulk").await.len(), 1);

        let (_, _, page) = call(&h, "admin-token", "GET", "/api/v1/media", None, &[]).await;
        assert!(!ids(&page).contains(&"old".to_owned()));
        assert!(ids(&page).contains(&"cached".to_owned()));
    }

    #[tokio::test]
    async fn bulk_delete_honours_the_size_floor_and_needs_a_date() {
        let h = harness(Some(source()));
        let (status, _, task) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/delete",
            Some(json!({"before": "2026-12-01T00:00:00Z", "min_size_bytes": 150})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(
            task["result"]["deleted_count"], 1,
            "only `new` is 150 or more"
        );

        let (status, _, problem) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/delete",
            Some(json!({})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(problem["errors"][0]["pointer"], "/before");
        let (status, _, problem) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/delete",
            Some(json!({"before": "last tuesday"})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(problem["errors"][0]["pointer"], "/before");
    }

    #[tokio::test]
    async fn the_remote_purge_drops_cached_copies_but_keeps_quarantined_ones() {
        let h = harness(Some(source()));
        let (status, _, task) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/purge-remote-cache",
            Some(json!({"server_name": "matrix.org"})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{task}");
        assert_eq!(task["action"], "media.purge_remote_cache");
        assert_eq!(task["result"]["deleted_count"], 1);
        assert_eq!(task["result"]["skipped_quarantined"], 1);

        // No body at all: every remote server, unused since now.
        let (status, _, task) = call(
            &h,
            "admin-token",
            "POST",
            "/api/v1/media/purge-remote-cache",
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(task["result"]["deleted_count"], 1, "other.org's copy");
        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?origin=remote",
            None,
            &[],
        )
        .await;
        assert_eq!(ids(&page), ["bad"]);
        let (_, _, page) = call(
            &h,
            "admin-token",
            "GET",
            "/api/v1/media?origin=local",
            None,
            &[],
        )
        .await;
        assert_eq!(
            page["items"].as_array().unwrap().len(),
            4,
            "local untouched"
        );
        assert_eq!(audit(&h, "media.purge_remote_cache").await.len(), 2);
    }
}
