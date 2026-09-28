//! Reports: what users have flagged, and what an operator did about it (`reports.*`, the
//! `Reports` tag of `openapi/openapi.yaml`).
//!
//! A report is filed through the client-server API -- `POST /rooms/{roomId}/report/{eventId}`,
//! `POST /rooms/{roomId}/report` or `POST /users/{userId}/report` -- and kept by whoever owns
//! rooms (`hs-room`'s `reports` module implements [`ReportSource`] over the same durable store
//! those endpoints write). This module is the admin side: the wire shapes, the source trait, a
//! test fake, and the four handlers.
//!
//! # Resolving and dismissing
//!
//! The contract has one action, `POST /reports/{id}/resolve`, carrying a `resolution`. A report
//! resolved with `no_action` is **dismissed** (nothing was wrong); any other resolution marks it
//! **resolved** (something was done: a warning, a redaction, a suspension...). Both close it,
//! and a closed report cannot be resolved again (`409`): a second decision about the same
//! report would rewrite the record of the first. Deleting a report removes it outright, for a
//! report filed in error or one that must not be kept.

use std::collections::BTreeMap;
use std::sync::RwLock;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::{Problem, ValidationError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::handler_kit::{authorize, check_replay, record, respond_and_remember, unwired};
use crate::model::{AuditChange, Page, ResourceRef, Scope};
use crate::router::AdminState;
use crate::sources::SourceError;

/// A new report id: a ULID that sorts after every id this process has handed out before, so
/// "newest first" is key order even for two reports filed in the same millisecond (plain
/// [`crate::model::new_id`] ULIDs are random within a millisecond).
#[must_use]
pub fn new_report_id() -> String {
    static GENERATOR: std::sync::Mutex<Option<ulid::Generator>> = std::sync::Mutex::new(None);
    let mut generator = GENERATOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    generator
        .get_or_insert_with(ulid::Generator::new)
        .generate()
        // Only fails when 2^80 ids were made in one millisecond; a fresh random one is fine then.
        .unwrap_or_else(|_| ulid::Ulid::new())
        .to_string()
}

/// What a report is about. `room` is a report about a whole room rather than one event in it
/// (`POST /rooms/{roomId}/report`, client-server API v1.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    /// One event in a room.
    Event,
    /// A whole room.
    Room,
    /// A user (`POST /users/{userId}/report`, client-server API v1.14).
    User,
}

impl ReportKind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "event" => Some(Self::Event),
            "room" => Some(Self::Room),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// Where a report stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Nobody has decided anything yet.
    Open,
    /// Closed with an action taken.
    Resolved,
    /// Closed with no action (`resolution: no_action`).
    Dismissed,
}

impl ReportStatus {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "resolved" => Some(Self::Resolved),
            "dismissed" => Some(Self::Dismissed),
            _ => None,
        }
    }
}

/// What the operator did about a report (the OpenAPI `ReportResolve.resolution` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportResolution {
    /// Nothing: the report is dismissed.
    NoAction,
    /// The reported user was warned.
    Warned,
    /// The reported content was redacted.
    Redacted,
    /// The reported user was suspended.
    Suspended,
    /// The reported user was deactivated.
    Deactivated,
    /// The room was blocked.
    RoomBlocked,
    /// Something else, described in the note.
    Other,
}

impl ReportResolution {
    /// The status a report resolved this way ends in.
    #[must_use]
    pub fn closes_as(self) -> ReportStatus {
        match self {
            Self::NoAction => ReportStatus::Dismissed,
            _ => ReportStatus::Resolved,
        }
    }
}

/// The OpenAPI `Report` schema.
///
/// Every optional field is serialized (as `null` when absent), as the contract's
/// `type: [string, 'null']` fields say, so a client can tell "no reason given" from an old
/// server that does not know the field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminReport {
    /// A ULID.
    pub id: String,
    /// What is reported.
    pub kind: ReportKind,
    /// Open, resolved or dismissed.
    pub status: ReportStatus,
    /// The room, for an event or room report.
    pub room_id: Option<String>,
    /// The event, for an event report.
    pub event_id: Option<String>,
    /// Who filed it.
    pub reporter_id: String,
    /// Whose conduct is reported: the reported user, or an event's sender.
    pub reported_user_id: Option<String>,
    /// The reporter's words, if any.
    pub reason: Option<String>,
    /// The client-supplied score (-100 most offensive .. 0 inoffensive), for event reports from
    /// clients that still send one.
    pub score: Option<i64>,
    /// When the server received it (RFC 3339).
    pub received_at: String,
    /// What was done, once it is closed.
    pub resolution: Option<ReportResolution>,
    /// The operator's note on the resolution.
    pub resolution_note: Option<String>,
    /// When it was closed (RFC 3339).
    #[serde(default)]
    pub resolved_at: Option<String>,
    /// Who closed it.
    #[serde(default)]
    pub resolved_by: Option<String>,
    /// The reported event as this server holds it now (`type`, `sender`, `origin_server_ts`,
    /// `content`), on `GET /reports/{id}` for an event report whose event this server has.
    /// Redacted content is shown redacted. `null` in lists and when the event is not held.
    #[serde(default)]
    pub event: Option<serde_json::Value>,
}

/// The `GET /reports` filters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportFilter {
    /// Only this kind.
    pub kind: Option<ReportKind>,
    /// Only this status.
    pub status: Option<ReportStatus>,
    /// Only reports about this room.
    pub room_id: Option<String>,
    /// Only reports whose conduct is this user's: reports about them, and reports of events
    /// they sent.
    pub reported_user_id: Option<String>,
    /// Only reports this user filed.
    pub reporter_id: Option<String>,
}

impl ReportFilter {
    /// Whether `report` passes every filter set.
    #[must_use]
    pub fn matches(&self, report: &AdminReport) -> bool {
        self.kind.is_none_or(|k| report.kind == k)
            && self.status.is_none_or(|s| report.status == s)
            && self
                .room_id
                .as_deref()
                .is_none_or(|r| report.room_id.as_deref() == Some(r))
            && self
                .reported_user_id
                .as_deref()
                .is_none_or(|u| report.reported_user_id.as_deref() == Some(u))
            && self
                .reporter_id
                .as_deref()
                .is_none_or(|u| report.reporter_id == u)
    }
}

/// The OpenAPI `ReportResolve` schema: `POST /reports/{id}/resolve`'s body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportResolve {
    /// What was done.
    pub resolution: ReportResolution,
    /// Why, or what exactly, in the operator's words.
    #[serde(default)]
    pub note: Option<String>,
}

/// Where the admin API reads and closes reports. Implemented by `hs-room` over the durable store
/// the client-server reporting endpoints write.
#[async_trait]
pub trait ReportSource: Send + Sync + 'static {
    /// Every report `filter` admits, newest first.
    async fn list(&self, filter: &ReportFilter) -> Result<Vec<AdminReport>, SourceError>;
    /// One report, with [`AdminReport::event`] filled in when the implementation holds the
    /// reported event.
    async fn get(&self, id: &str) -> Result<Option<AdminReport>, SourceError>;
    /// Closes an open report. [`SourceError::NotFound`] if there is no such report,
    /// [`SourceError::Conflict`] if it is already closed.
    async fn resolve(
        &self,
        id: &str,
        resolve: &ReportResolve,
        resolved_by: &str,
    ) -> Result<AdminReport, SourceError>;
    /// Removes a report. [`SourceError::NotFound`] if there is no such report.
    async fn delete(&self, id: &str) -> Result<(), SourceError>;
    /// How many reports are open: the Overview's "reports awaiting action".
    async fn open_count(&self) -> Result<u64, SourceError> {
        Ok(self
            .list(&ReportFilter {
                status: Some(ReportStatus::Open),
                ..ReportFilter::default()
            })
            .await?
            .len() as u64)
    }
}

/// Closes `report` per `resolve` as `resolved_by`, at `now` (RFC 3339): the one place the
/// open-to-closed rule is written, shared by every [`ReportSource`] implementation.
///
/// # Errors
/// [`SourceError::Conflict`] if the report is not open.
pub fn apply_resolution(
    report: &mut AdminReport,
    resolve: &ReportResolve,
    resolved_by: &str,
    now: String,
) -> Result<(), SourceError> {
    if report.status != ReportStatus::Open {
        return Err(SourceError::Conflict(format!(
            "report {} is already {}",
            report.id,
            match report.status {
                ReportStatus::Resolved => "resolved",
                _ => "dismissed",
            }
        )));
    }
    report.status = resolve.resolution.closes_as();
    report.resolution = Some(resolve.resolution);
    report.resolution_note = resolve.note.clone().filter(|n| !n.trim().is_empty());
    report.resolved_at = Some(now);
    report.resolved_by = Some(resolved_by.to_owned());
    Ok(())
}

/// An in-process [`ReportSource`] for tests and the mock: not durable.
#[derive(Default)]
pub struct InMemoryReportSource {
    reports: RwLock<BTreeMap<String, AdminReport>>,
}

impl InMemoryReportSource {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a report.
    pub fn insert(&self, report: AdminReport) {
        self.reports
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(report.id.clone(), report);
    }
}

#[async_trait]
impl ReportSource for InMemoryReportSource {
    async fn list(&self, filter: &ReportFilter) -> Result<Vec<AdminReport>, SourceError> {
        let reports = self
            .reports
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // ULIDs sort by time, so reverse id order is newest first.
        Ok(reports
            .values()
            .rev()
            .filter(|r| filter.matches(r))
            .cloned()
            .collect())
    }

    async fn get(&self, id: &str) -> Result<Option<AdminReport>, SourceError> {
        Ok(self
            .reports
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned())
    }

    async fn resolve(
        &self,
        id: &str,
        resolve: &ReportResolve,
        resolved_by: &str,
    ) -> Result<AdminReport, SourceError> {
        let mut reports = self
            .reports
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let report = reports.get_mut(id).ok_or(SourceError::NotFound)?;
        apply_resolution(report, resolve, resolved_by, hs_http::time::now_rfc3339())?;
        Ok(report.clone())
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        self.reports
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .map(|_| ())
            .ok_or(SourceError::NotFound)
    }
}

/// The event published when somebody files a report.
pub const REPORT_CREATED: &str = "report.created";

/// Publishes every report `filed` carries as a [`REPORT_CREATED`] event on `events` (resource
/// `{type: report, id}`, data the report as `GET /reports` lists it), until `filed` closes.
/// The owner of the reports store hands its subscription here (`hs-cli` does, with `hs-room`'s
/// `ReportStore::subscribe`). Runs on the current Tokio runtime.
pub fn forward_filed_reports(
    mut filed: tokio::sync::broadcast::Receiver<AdminReport>,
    events: std::sync::Arc<crate::events::EventBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match filed.recv().await {
                Ok(report) => publish_filed(&events, report),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(
                        missed,
                        "report.created: fell behind; some filed reports were not announced (they \
                         are kept, and listed)"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Publishes one filed report as a [`REPORT_CREATED`] event.
pub fn publish_filed(events: &crate::events::EventBus, mut report: AdminReport) {
    report.event = None;
    tracing::info!(
        report = %report.id,
        kind = ?report.kind,
        reporter = %report.reporter_id,
        "a report was filed"
    );
    let id = report.id.clone();
    events.publish(
        crate::model::Event::new(
            REPORT_CREATED,
            serde_json::to_value(&report).unwrap_or_default(),
        )
        .with_resource(ResourceRef::new("report", id)),
    );
}

// -------------------------------------------------------------------------------------------
// Handlers.
// -------------------------------------------------------------------------------------------

/// `GET /reports`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct ReportsQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    sort: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    room_id: Option<String>,
    reported_user_id: Option<String>,
    reporter_id: Option<String>,
}

fn invalid_query(pointer: &str, detail: String, instance: &str) -> Response {
    Problem::validation_failed()
        .with_detail(detail.clone())
        .with_errors(vec![ValidationError::new(pointer, detail)])
        .with_instance(instance.to_owned())
        .into_response()
}

fn no_such_report(id: &str, instance: &str) -> Response {
    Problem::not_found()
        .with_detail(format!("there is no report {id}"))
        .with_instance(instance.to_owned())
        .into_response()
}

/// `GET /api/v1/reports` (`moderation:read`): newest first by default; `sort` takes
/// `received_at`, `-received_at`, `score` or `-score` (most offensive first is `score`, since
/// scores run from -100 to 0).
pub(crate) async fn reports_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ReportsQuery>,
) -> Response {
    let instance = "/api/v1/reports";
    if let Err(response) = authorize(&state, &headers, Scope::ModerationRead, instance).await {
        return response;
    }
    let Some(reports) = &state.reports else {
        return unwired("reports", instance);
    };
    let mut filter = ReportFilter {
        room_id: query.room_id.clone().filter(|r| !r.is_empty()),
        reported_user_id: query.reported_user_id.clone().filter(|u| !u.is_empty()),
        reporter_id: query.reporter_id.clone().filter(|u| !u.is_empty()),
        ..ReportFilter::default()
    };
    if let Some(kind) = query.kind.as_deref().filter(|k| !k.is_empty()) {
        match ReportKind::parse(kind) {
            Some(kind) => filter.kind = Some(kind),
            None => {
                return invalid_query(
                    "/kind",
                    format!("kind must be event, room or user, not {kind:?}"),
                    instance,
                );
            }
        }
    }
    if let Some(status) = query.status.as_deref().filter(|s| !s.is_empty()) {
        match ReportStatus::parse(status) {
            Some(status) => filter.status = Some(status),
            None => {
                return invalid_query(
                    "/status",
                    format!("status must be open, resolved or dismissed, not {status:?}"),
                    instance,
                );
            }
        }
    }
    let mut items = match reports.list(&filter).await {
        Ok(items) => items,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    match query.sort.as_deref().unwrap_or("-received_at") {
        "-received_at" => {}
        "received_at" => items.reverse(),
        // Most offensive (lowest) first; unscored reports last.
        "score" => items.sort_by_key(|r| r.score.unwrap_or(i64::MAX)),
        "-score" => items.sort_by_key(|r| std::cmp::Reverse(r.score.unwrap_or(i64::MIN))),
        other => {
            return invalid_query(
                "/sort",
                format!(
                    "reports sort by received_at or score (prefix - for descending), not {other:?}"
                ),
                instance,
            );
        }
    }
    for item in &mut items {
        item.event = None;
    }
    axum::Json(Page::paginate(
        items,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    ))
    .into_response()
}

/// `GET /api/v1/reports/{id}` (`moderation:read`).
pub(crate) async fn reports_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/reports/{id}");
    if let Err(response) = authorize(&state, &headers, Scope::ModerationRead, &instance).await {
        return response;
    }
    let Some(reports) = &state.reports else {
        return unwired("reports", &instance);
    };
    match reports.get(&id).await {
        Ok(Some(report)) => axum::Json(report).into_response(),
        Ok(None) => no_such_report(&id, &instance),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `DELETE /api/v1/reports/{id}` (`moderation:write`): audited as `reports.delete`, published
/// as `report.deleted`.
pub(crate) async fn reports_delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/reports/{id}");
    let principal = match authorize(&state, &headers, Scope::ModerationWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(reports) = &state.reports else {
        return unwired("reports", &instance);
    };
    let before = match reports.get(&id).await {
        Ok(Some(report)) => report,
        Ok(None) => return no_such_report(&id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    match reports.delete(&id).await {
        Ok(()) => {}
        Err(SourceError::NotFound) => return no_such_report(&id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    }
    if let Err(response) = record(
        &state,
        &principal,
        "reports.delete",
        "report.deleted",
        ResourceRef::new("report", id.clone()),
        Vec::new(),
        json!({
            "id": id,
            "kind": before.kind,
            "status": before.status,
            "room_id": before.room_id,
            "event_id": before.event_id,
            "reporter_id": before.reporter_id,
        }),
        204,
    )
    .await
    {
        return response;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/v1/reports/{id}/resolve` (`moderation:write`): closes an open report, as
/// resolved or (with `no_action`) dismissed. Audited as `reports.resolve` with the status and
/// resolution change, published as `report.resolved`. `409` if it is already closed.
pub(crate) async fn reports_resolve(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/reports/{id}/resolve");
    let principal = match authorize(&state, &headers, Scope::ModerationWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(reports) = &state.reports else {
        return unwired("reports", &instance);
    };
    let request: ReportResolve = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return invalid_query(
                "/resolution",
                format!(
                    "the body must be {{\"resolution\": one of no_action, warned, redacted, \
                     suspended, deactivated, room_blocked, other; \"note\": optional text}}: {e}"
                ),
                &instance,
            );
        }
    };
    if let Err(response) = check_replay(&state, &headers, "reports.resolve", &body, &instance) {
        return response;
    }
    let resolved = match reports.resolve(&id, &request, &principal.id).await {
        Ok(report) => report,
        Err(SourceError::NotFound) => return no_such_report(&id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if let Err(response) = record(
        &state,
        &principal,
        "reports.resolve",
        "report.resolved",
        ResourceRef::new("report", id.clone()),
        vec![
            AuditChange {
                pointer: "/status".to_owned(),
                from: Some(json!("open")),
                to: Some(json!(resolved.status)),
            },
            AuditChange {
                pointer: "/resolution".to_owned(),
                from: None,
                to: Some(json!(resolved.resolution)),
            },
        ],
        json!({
            "id": id,
            "status": resolved.status,
            "resolution": resolved.resolution,
            "note": resolved.resolution_note,
        }),
        200,
    )
    .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        "reports.resolve",
        &body,
        StatusCode::OK,
        &resolved,
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report as `hs-room` would file one.
    fn report(id: &str, kind: ReportKind) -> AdminReport {
        AdminReport {
            id: id.to_owned(),
            kind,
            status: ReportStatus::Open,
            room_id: Some("!room:example.org".to_owned()),
            event_id: (kind == ReportKind::Event).then(|| "$event".to_owned()),
            reporter_id: "@alice:example.org".to_owned(),
            reported_user_id: Some("@mallory:example.org".to_owned()),
            reason: Some("spam".to_owned()),
            score: None,
            received_at: "2026-09-27T10:00:00Z".to_owned(),
            resolution: None,
            resolution_note: None,
            resolved_at: None,
            resolved_by: None,
            event: None,
        }
    }

    #[test]
    fn no_action_dismisses_and_anything_else_resolves() {
        let mut dismissed = report("01A", ReportKind::Event);
        apply_resolution(
            &mut dismissed,
            &ReportResolve {
                resolution: ReportResolution::NoAction,
                note: Some("  ".to_owned()),
            },
            "@ops:example.org",
            "now".to_owned(),
        )
        .unwrap();
        assert_eq!(dismissed.status, ReportStatus::Dismissed);
        assert_eq!(dismissed.resolution_note, None, "a blank note is no note");
        assert_eq!(dismissed.resolved_by.as_deref(), Some("@ops:example.org"));

        let mut resolved = report("01B", ReportKind::User);
        apply_resolution(
            &mut resolved,
            &ReportResolve {
                resolution: ReportResolution::Suspended,
                note: Some("third strike".to_owned()),
            },
            "@ops:example.org",
            "now".to_owned(),
        )
        .unwrap();
        assert_eq!(resolved.status, ReportStatus::Resolved);
        assert_eq!(resolved.resolution_note.as_deref(), Some("third strike"));
    }

    #[test]
    fn a_closed_report_cannot_be_decided_again() {
        let mut closed = report("01A", ReportKind::Room);
        let resolve = ReportResolve {
            resolution: ReportResolution::RoomBlocked,
            note: None,
        };
        apply_resolution(&mut closed, &resolve, "@a:example.org", "t1".to_owned()).unwrap();
        let again = apply_resolution(&mut closed, &resolve, "@b:example.org", "t2".to_owned());
        assert!(matches!(again, Err(SourceError::Conflict(_))), "{again:?}");
        assert_eq!(closed.resolved_by.as_deref(), Some("@a:example.org"));
        assert_eq!(closed.resolved_at.as_deref(), Some("t1"));
    }

    #[test]
    fn a_report_serializes_every_nullable_field() {
        let value = serde_json::to_value(report("01A", ReportKind::User)).unwrap();
        for field in [
            "score",
            "resolution",
            "resolution_note",
            "resolved_at",
            "event",
        ] {
            assert_eq!(value[field], serde_json::Value::Null, "{field} in {value}");
            assert!(value.get(field).is_some(), "{field} missing from {value}");
        }
        assert_eq!(value["kind"], "user");
        assert_eq!(value["status"], "open");
    }

    #[tokio::test]
    async fn a_filed_report_is_published_as_report_created() {
        let events = std::sync::Arc::new(crate::events::EventBus::new());
        let mut live = events.subscribe();
        let (filed, receiver) = tokio::sync::broadcast::channel(4);
        let forwarder = forward_filed_reports(receiver, events.clone());
        let mut with_event = report("01A", ReportKind::Event);
        with_event.event = Some(serde_json::json!({"content": {"body": "x"}}));
        filed.send(with_event).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), live.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.r#type, REPORT_CREATED);
        let resource = event.resource.clone().unwrap();
        assert_eq!(
            (resource.r#type.as_str(), resource.id.as_str()),
            ("report", "01A")
        );
        assert_eq!(event.data["reporter_id"], "@alice:example.org");
        assert_eq!(event.data["event"], serde_json::Value::Null);
        drop(filed);
        forwarder.await.unwrap();
    }

    #[test]
    fn the_resolve_body_refuses_what_it_does_not_know() {
        assert!(serde_json::from_str::<ReportResolve>(r#"{"resolution":"banished"}"#).is_err());
        assert!(serde_json::from_str::<ReportResolve>(r#"{"note":"x"}"#).is_err());
        assert!(
            serde_json::from_str::<ReportResolve>(r#"{"resolution":"warned","extra":1}"#).is_err()
        );
    }

    mod http {
        use std::sync::Arc;

        use axum::http::StatusCode;
        use serde_json::json;

        use super::report;
        use crate::audit::{AuditFilter, AuditSink};
        use crate::handler_kit::testing::{call, state};
        use crate::reports::{InMemoryReportSource, ReportKind};

        fn wired() -> (
            crate::router::AdminState,
            Arc<crate::audit::InMemoryAuditSink>,
        ) {
            let (state, audit) = state();
            let source = Arc::new(InMemoryReportSource::new());
            source.insert(report("01A", ReportKind::Event));
            let mut scored = report("01B", ReportKind::Event);
            scored.score = Some(-100);
            source.insert(scored);
            source.insert(report("01C", ReportKind::User));
            (state.with_reports(source), audit)
        }

        #[tokio::test]
        async fn unwired_reports_are_503_not_an_empty_inbox() {
            let (state, _) = state();
            let (status, _, body) =
                call(&state, "GET", "/api/v1/reports", Some("admin"), None, None).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        }

        #[tokio::test]
        async fn a_moderator_lists_filters_and_sorts_reports() {
            let (state, _) = wired();
            let (status, _, page) = call(
                &state,
                "GET",
                "/api/v1/reports?include_total=true",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(page["total"], 3);
            assert_eq!(page["items"][0]["id"], "01C", "newest first: {page}");

            let (_, _, users) = call(
                &state,
                "GET",
                "/api/v1/reports?kind=user",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(users["items"].as_array().unwrap().len(), 1);

            let (_, _, by_score) = call(
                &state,
                "GET",
                "/api/v1/reports?sort=score",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(by_score["items"][0]["id"], "01B", "{by_score}");

            let (status, _, problem) = call(
                &state,
                "GET",
                "/api/v1/reports?status=pending",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(problem["errors"][0]["pointer"], "/status");

            // admin:read reads every resource (decision 0013); a write-only moderation token
            // cannot be had, so the refusal is shown with no token at all.
            let (status, _, _) =
                call(&state, "GET", "/api/v1/reports", Some("read"), None, None).await;
            assert_eq!(status, StatusCode::OK);
            let (status, _, _) = call(&state, "GET", "/api/v1/reports", None, None, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn reports_filter_by_the_reported_user_and_by_the_reporter() {
            let (state, _) = wired();
            let source = Arc::new(InMemoryReportSource::new());
            source.insert(report("01A", ReportKind::Event));
            let mut other = report("01B", ReportKind::User);
            other.reported_user_id = Some("@eve:example.org".to_owned());
            source.insert(other);
            let mut by_bob = report("01C", ReportKind::Room);
            by_bob.reporter_id = "@bob:example.org".to_owned();
            source.insert(by_bob);
            let state = state.with_reports(source);

            let ids = |page: &serde_json::Value| -> Vec<String> {
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_str().unwrap().to_owned())
                    .collect()
            };
            let (status, _, page) = call(
                &state,
                "GET",
                "/api/v1/reports?reported_user_id=%40mallory%3Aexample.org",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(ids(&page), ["01C", "01A"]);

            let (_, _, page) = call(
                &state,
                "GET",
                "/api/v1/reports?reporter_id=%40bob%3Aexample.org",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(ids(&page), ["01C"]);

            // Both at once narrow to reports matching both.
            let (_, _, page) = call(
                &state,
                "GET",
                "/api/v1/reports?reported_user_id=%40mallory%3Aexample.org&reporter_id=%40alice%3Aexample.org",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(ids(&page), ["01A"]);

            // An empty value is no filter, as for room_id.
            let (_, _, page) = call(
                &state,
                "GET",
                "/api/v1/reports?reporter_id=",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(ids(&page).len(), 3);
        }

        #[tokio::test]
        async fn resolving_closes_audits_and_refuses_a_second_decision() {
            let (state, audit) = wired();
            let mut events = state.events.subscribe();
            // Reading is not enough.
            let (status, _, _) = call(
                &state,
                "POST",
                "/api/v1/reports/01A/resolve",
                Some("mod-read"),
                Some(json!({"resolution": "no_action"})),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);

            let (status, _, resolved) = call(
                &state,
                "POST",
                "/api/v1/reports/01A/resolve",
                Some("mod-write"),
                Some(json!({"resolution": "redacted", "note": "removed the link"})),
                Some("k1"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{resolved}");
            assert_eq!(resolved["status"], "resolved");
            assert_eq!(resolved["resolution"], "redacted");
            assert_eq!(resolved["resolved_by"], "@mod:example.org");

            let entries = audit
                .query(&AuditFilter {
                    action: Some("reports.resolve".to_owned()),
                    limit: 10,
                    ..AuditFilter::default()
                })
                .await
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].target.id, "01A");
            assert_eq!(entries[0].changes[0].to, Some(json!("resolved")));
            assert_eq!(events.recv().await.unwrap().r#type, "report.resolved");

            // The same key and body is the same answer, not a 409 and not a second entry.
            let (status, headers, replayed) = call(
                &state,
                "POST",
                "/api/v1/reports/01A/resolve",
                Some("mod-write"),
                Some(json!({"resolution": "redacted", "note": "removed the link"})),
                Some("k1"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers["idempotency-replayed"], "true");
            assert_eq!(replayed, resolved);

            // A new decision about a closed report is refused.
            let (status, _, problem) = call(
                &state,
                "POST",
                "/api/v1/reports/01A/resolve",
                Some("mod-write"),
                Some(json!({"resolution": "no_action"})),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{problem}");

            // Dismissing is resolving with no action.
            let (_, _, dismissed) = call(
                &state,
                "POST",
                "/api/v1/reports/01C/resolve",
                Some("admin"),
                Some(json!({"resolution": "no_action"})),
                None,
            )
            .await;
            assert_eq!(dismissed["status"], "dismissed");

            let (status, _, problem) = call(
                &state,
                "POST",
                "/api/v1/reports/01B/resolve",
                Some("mod-write"),
                Some(json!({"resolution": "exiled"})),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
            let (status, _, _) = call(
                &state,
                "POST",
                "/api/v1/reports/nope/resolve",
                Some("mod-write"),
                Some(json!({"resolution": "warned"})),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn deleting_removes_the_report_and_is_audited() {
            let (state, audit) = wired();
            let (status, _, body) = call(
                &state,
                "DELETE",
                "/api/v1/reports/01B",
                Some("mod-write"),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
            let (status, _, _) = call(
                &state,
                "GET",
                "/api/v1/reports/01B",
                Some("mod-read"),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = call(
                &state,
                "DELETE",
                "/api/v1/reports/01B",
                Some("mod-write"),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let entries = audit
                .query(&AuditFilter {
                    action: Some("reports.delete".to_owned()),
                    limit: 10,
                    ..AuditFilter::default()
                })
                .await
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].outcome.status, 204);
        }
    }

    #[tokio::test]
    async fn the_in_memory_source_lists_newest_first_and_filters() {
        let source = InMemoryReportSource::new();
        source.insert(report("01A", ReportKind::Event));
        source.insert(report("01B", ReportKind::User));
        let mut other_room = report("01C", ReportKind::Room);
        other_room.room_id = Some("!other:example.org".to_owned());
        source.insert(other_room);

        let all = source.list(&ReportFilter::default()).await.unwrap();
        let ids: Vec<_> = all.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["01C", "01B", "01A"]);

        let users = source
            .list(&ReportFilter {
                kind: Some(ReportKind::User),
                ..ReportFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(users.len(), 1);

        let in_room = source
            .list(&ReportFilter {
                room_id: Some("!other:example.org".to_owned()),
                ..ReportFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(in_room.len(), 1);
        assert_eq!(source.open_count().await.unwrap(), 3);

        source
            .resolve(
                "01A",
                &ReportResolve {
                    resolution: ReportResolution::Warned,
                    note: None,
                },
                "@ops:example.org",
            )
            .await
            .unwrap();
        assert_eq!(source.open_count().await.unwrap(), 2);
        source.delete("01B").await.unwrap();
        assert!(matches!(
            source.delete("01B").await,
            Err(SourceError::NotFound)
        ));
    }
}
