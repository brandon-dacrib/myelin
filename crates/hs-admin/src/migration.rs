//! Migrating from Synapse (`migration.*`, the `Migration` tag of `openapi/openapi.yaml`).
//!
//! [`MigrationSource`] is what the handlers act through. `hs-compat`'s `migration::Migrator`
//! implements it over a Synapse database and this server's stores, running each long step as a
//! task in the [`crate::tasks::TaskRegistry`]; a server that has not wired one answers `503`.
//!
//! # The life of a migration
//!
//! A server has at most one migration. `start` reads the source from the configuration
//! (`source_secret_ref`, `/migration/synapse` by default: the connection string and its password
//! are never in this API), checks that it is a Synapse database for this server's own name, and
//! starts the copy as a task; `status` goes `copying`. `pause` stops the copy between two
//! batches (`paused`), `resume` carries on from there, and a restart of this server carries on by
//! itself. When everything has been copied once the status is `ready_for_cutover`. `verify`
//! compares counts and samples with Synapse (a task, `202`), and can be run as often as wanted.
//! `cutover` -- once Synapse has been stopped -- copies whatever changed since, verifies, and
//! ends the migration (`completed`) if verification passes. `abort` abandons it (`aborted`):
//! Synapse is never written to, so nothing needs undoing there, and what was copied stays.
//!
//! Every one of these that changes something is written to the audit log (`migration.start`,
//! `.pause`, `.resume`, `.abort`, `.verify`, `.cutover`) and published on the event stream
//! (`migration.started`, `.paused`, `.resumed`, `.aborted`, `.verifying`, `.cutting_over`).

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::Problem;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::handler_kit::{authorize, check_replay, record, respond_and_remember, unwired};
use crate::model::{Actor, AuditChange, Page, ResourceRef, Scope, Task};
use crate::router::AdminState;
use crate::sources::SourceError;

/// The configuration pointer `start` reads the source from when the request names none.
pub const DEFAULT_SOURCE_REF: &str = "/migration/synapse";

/// One stream's progress (the OpenAPI `MigrationStatus.streams[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationStreamStatus {
    /// `users`, `devices`, `access_tokens`, `account_data`, `rooms` or `media`.
    pub name: String,
    /// Rows now present here.
    pub copied_count: u64,
    /// Rows in Synapse, once counted.
    pub total_count: Option<u64>,
    /// Rows per second over the stream's last run.
    pub rate_per_second: f64,
    /// Rows deliberately not copied (each is in the log, with why).
    pub skipped_count: u64,
    /// Rows that could not be copied (each is in the log, with the error).
    pub failed_count: u64,
    /// Every row has been read.
    pub done: bool,
}

/// What a verification found for one stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStreamVerification {
    /// The stream.
    pub name: String,
    /// Rows in Synapse that are meant to be copied.
    pub source_count: u64,
    /// Of those, how many are here.
    pub target_count: u64,
    /// Rows deliberately not copied.
    pub skipped_count: u64,
    /// Rows compared field by field.
    pub sampled: u64,
    /// What the samples found different.
    pub mismatches: Vec<String>,
}

/// The last verification (`MigrationStatus.verification`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationVerification {
    /// Every count matches and no sample differs.
    pub passed: bool,
    /// When it finished.
    pub checked_at: String,
    /// Per stream.
    pub streams: Vec<MigrationStreamVerification>,
}

/// The OpenAPI `MigrationStatus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationStatus {
    /// `idle`, `copying`, `paused`, `ready_for_cutover`, `cutting_over`, `verifying`,
    /// `completed`, `failed` or `aborted`.
    pub status: String,
    /// The source database, without its password.
    pub source: Option<String>,
    /// Per stream, in copy order.
    pub streams: Vec<MigrationStreamStatus>,
    /// How long the rest of the copy should take.
    pub estimated_remaining_ms: Option<u64>,
    /// Why the last run failed, most recent last.
    pub errors: Vec<String>,
    /// The task running the current step, if any.
    pub task_id: Option<String>,
    /// When the migration was first started (RFC 3339).
    pub started_at: Option<String>,
    /// Who started it.
    pub started_by: Option<String>,
    /// When it last changed (RFC 3339).
    pub updated_at: Option<String>,
    /// When the cutover finished (RFC 3339).
    pub completed_at: Option<String>,
    /// Who cut over.
    pub cutover_by: Option<String>,
    /// The last verification.
    pub verification: Option<MigrationVerification>,
}

impl MigrationStatus {
    /// A server with nothing migrated and nothing configured.
    #[must_use]
    pub fn idle() -> Self {
        Self {
            status: "idle".to_owned(),
            source: None,
            streams: Vec::new(),
            estimated_remaining_ms: None,
            errors: Vec::new(),
            task_id: None,
            started_at: None,
            started_by: None,
            updated_at: None,
            completed_at: None,
            cutover_by: None,
            verification: None,
        }
    }
}

/// One log entry (the OpenAPI `MigrationLogEntry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationLogEntry {
    /// When (RFC 3339).
    pub recorded_at: String,
    /// The stream it is about, or `migration`.
    pub stream: String,
    /// What happened.
    pub message: String,
    /// `info`, `warning` or `error`.
    pub level: String,
}

/// The OpenAPI `MigrationStartRequest`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStartRequest {
    /// The configuration pointer holding the source (a `SynapseSourceConfig`);
    /// [`DEFAULT_SOURCE_REF`] when absent.
    #[serde(default)]
    pub source_secret_ref: Option<String>,
}

/// A phase change a control operation made, for the audit entry: `None` when it changed
/// nothing (pausing a paused migration), which is answered but not recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationChange {
    /// The status before.
    pub from: String,
    /// The status now.
    pub status: MigrationStatus,
}

/// What the `migration.*` handlers act through. See the module docs for the life of a
/// migration; `SourceError::Conflict` for an operation the current status does not allow
/// (answered `409` with the reason), `SourceError::Invalid` for a source that is missing or
/// unusable.
#[async_trait]
pub trait MigrationSource: Send + Sync + 'static {
    /// Where the migration is.
    async fn status(&self) -> Result<MigrationStatus, SourceError>;
    /// Starts (or, after a failure or an abort, restarts) the copy.
    async fn start(
        &self,
        request: &MigrationStartRequest,
        actor: &Actor,
    ) -> Result<MigrationChange, SourceError>;
    /// Stops the copy between two batches. `Ok(None)` if it was already paused.
    async fn pause(&self, actor: &Actor) -> Result<Option<MigrationChange>, SourceError>;
    /// Carries a paused copy on. `Ok(None)` if it was already copying.
    async fn resume(&self, actor: &Actor) -> Result<Option<MigrationChange>, SourceError>;
    /// Abandons the migration. `Ok(None)` if it was already aborted.
    async fn abort(&self, actor: &Actor) -> Result<Option<MigrationChange>, SourceError>;
    /// Starts a verification task.
    async fn verify(&self, actor: &Actor) -> Result<Task, SourceError>;
    /// Starts the cutover task.
    async fn cutover(&self, actor: &Actor) -> Result<Task, SourceError>;
    /// The log, oldest first.
    async fn log(&self) -> Result<Vec<MigrationLogEntry>, SourceError>;
}

fn migration_ref() -> ResourceRef {
    ResourceRef::new("migration", "synapse")
}

fn problem(error: &SourceError, instance: &str) -> Response {
    error.to_problem().with_instance(instance).into_response()
}

/// `GET /api/v1/migration` (`admin:read`).
pub(crate) async fn get(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let instance = "/api/v1/migration";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(migration) = &state.migration else {
        return unwired("migration", instance);
    };
    match migration.status().await {
        Ok(status) => axum::Json(status).into_response(),
        Err(e) => problem(&e, instance),
    }
}

/// `GET /api/v1/migration/log`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct LogQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
}

/// `GET /api/v1/migration/log` (`admin:read`): oldest first, paged.
pub(crate) async fn log(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    let instance = "/api/v1/migration/log";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(migration) = &state.migration else {
        return unwired("migration", instance);
    };
    if let Some(cursor) = query.cursor.as_deref()
        && !cursor.is_empty()
        && cursor.parse::<usize>().is_err()
    {
        let detail = format!("{cursor:?} is not a cursor this listing handed out");
        return Problem::validation_failed()
            .with_detail(detail.clone())
            .with_errors(vec![hs_http::ValidationError::new("/cursor", detail)])
            .with_instance(instance)
            .into_response();
    }
    match migration.log().await {
        Ok(entries) => axum::Json(Page::paginate(
            entries,
            query.cursor.as_deref(),
            query.limit,
            query.include_total.unwrap_or(false),
        ))
        .into_response(),
        Err(e) => problem(&e, instance),
    }
}

/// What a control operation records: its audit action, its event, and the new status.
struct Control {
    operation_id: &'static str,
    event_type: &'static str,
    instance: &'static str,
}

async fn finish_control(
    state: &AdminState,
    headers: &HeaderMap,
    principal: &crate::model::Principal,
    control: &Control,
    body: &[u8],
    outcome: Result<Option<MigrationChange>, SourceError>,
    migration: &dyn MigrationSource,
) -> Response {
    let status = match outcome {
        Ok(Some(change)) => {
            tracing::info!(
                operation = control.operation_id,
                actor = %principal.id,
                from = %change.from,
                to = %change.status.status,
                "migration status changed"
            );
            if let Err(response) = record(
                state,
                principal,
                control.operation_id,
                control.event_type,
                migration_ref(),
                vec![AuditChange {
                    pointer: "/status".into(),
                    from: Some(json!(change.from)),
                    to: Some(json!(change.status.status)),
                }],
                json!({
                    "status": change.status.status,
                    "source": change.status.source,
                    "task_id": change.status.task_id,
                }),
                200,
            )
            .await
            {
                return response;
            }
            change.status
        }
        Ok(None) => match migration.status().await {
            Ok(status) => status,
            Err(e) => return problem(&e, control.instance),
        },
        Err(e) => {
            if let SourceError::Conflict(detail) | SourceError::Invalid(detail) = &e {
                tracing::info!(operation = control.operation_id, actor = %principal.id, %detail, "a migration operation was refused");
            }
            return problem(&e, control.instance);
        }
    };
    respond_and_remember(
        state,
        headers,
        control.operation_id,
        body,
        StatusCode::OK,
        &status,
        &[],
    )
}

/// `POST /api/v1/migration/start` (`admin:write`).
pub(crate) async fn start(
    State(state): State<AdminState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let control = Control {
        operation_id: "migration.start",
        event_type: "migration.started",
        instance: "/api/v1/migration/start",
    };
    let principal = match authorize(&state, &headers, Scope::AdminWrite, control.instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(migration) = state.migration.clone() else {
        return unwired("migration", control.instance);
    };
    let request: MigrationStartRequest = if body.iter().all(u8::is_ascii_whitespace) {
        MigrationStartRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(e) => {
                return Problem::validation_failed()
                    .with_detail(format!(
                        "the request body is not a MigrationStartRequest: {e}"
                    ))
                    .with_instance(control.instance)
                    .into_response();
            }
        }
    };
    if let Err(response) = check_replay(
        &state,
        &headers,
        control.operation_id,
        &body,
        control.instance,
    ) {
        return response;
    }
    let outcome = migration
        .start(&request, &principal.to_actor())
        .await
        .map(Some);
    finish_control(
        &state,
        &headers,
        &principal,
        &control,
        &body,
        outcome,
        migration.as_ref(),
    )
    .await
}

macro_rules! control_handler {
    ($name:ident, $op:literal, $event:literal, $path:literal, $method:ident, $doc:literal) => {
        #[doc = $doc]
        pub(crate) async fn $name(
            State(state): State<AdminState>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> Response {
            let control = Control {
                operation_id: $op,
                event_type: $event,
                instance: $path,
            };
            let principal =
                match authorize(&state, &headers, Scope::AdminWrite, control.instance).await {
                    Ok(principal) => principal,
                    Err(response) => return response,
                };
            let Some(migration) = state.migration.clone() else {
                return unwired("migration", control.instance);
            };
            if let Err(response) = check_replay(
                &state,
                &headers,
                control.operation_id,
                &body,
                control.instance,
            ) {
                return response;
            }
            let outcome = migration.$method(&principal.to_actor()).await;
            finish_control(
                &state,
                &headers,
                &principal,
                &control,
                &body,
                outcome,
                migration.as_ref(),
            )
            .await
        }
    };
}

control_handler!(
    pause,
    "migration.pause",
    "migration.paused",
    "/api/v1/migration/pause",
    pause,
    "`POST /api/v1/migration/pause` (`admin:write`)."
);
control_handler!(
    resume,
    "migration.resume",
    "migration.resumed",
    "/api/v1/migration/resume",
    resume,
    "`POST /api/v1/migration/resume` (`admin:write`)."
);
control_handler!(
    abort,
    "migration.abort",
    "migration.aborted",
    "/api/v1/migration/abort",
    abort,
    "`POST /api/v1/migration/abort` (`admin:write`)."
);

macro_rules! task_handler {
    ($name:ident, $op:literal, $event:literal, $path:literal, $method:ident, $doc:literal) => {
        #[doc = $doc]
        pub(crate) async fn $name(
            State(state): State<AdminState>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> Response {
            let instance = $path;
            let principal = match authorize(&state, &headers, Scope::AdminWrite, instance).await {
                Ok(principal) => principal,
                Err(response) => return response,
            };
            let Some(migration) = state.migration.clone() else {
                return unwired("migration", instance);
            };
            if let Err(response) = check_replay(&state, &headers, $op, &body, instance) {
                return response;
            }
            let task = match migration.$method(&principal.to_actor()).await {
                Ok(task) => task,
                Err(e) => {
                    if let SourceError::Conflict(detail) = &e {
                        tracing::info!(operation = $op, actor = %principal.id, %detail, "a migration operation was refused");
                    }
                    return problem(&e, instance);
                }
            };
            tracing::info!(operation = $op, actor = %principal.id, task = %task.id, "a migration step started");
            if let Err(response) = record(
                &state,
                &principal,
                $op,
                $event,
                migration_ref(),
                Vec::new(),
                json!({ "task_id": task.id }),
                202,
            )
            .await
            {
                return response;
            }
            let location = format!("/api/v1/tasks/{}", task.id);
            respond_and_remember(
                &state,
                &headers,
                $op,
                &body,
                StatusCode::ACCEPTED,
                &task,
                &[("location", location)],
            )
        }
    };
}

task_handler!(
    verify,
    "migration.verify",
    "migration.verifying",
    "/api/v1/migration/verify",
    verify,
    "`POST /api/v1/migration/verify` (`admin:write`): `202` and the verification task."
);
task_handler!(
    cutover,
    "migration.cutover",
    "migration.cutting_over",
    "/api/v1/migration/cutover",
    cutover,
    "`POST /api/v1/migration/cutover` (`admin:write`): `202` and the cutover task."
);

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::audit::{AuditFilter, AuditSink, InMemoryAuditSink};
    use crate::handler_kit::testing::{call, state};
    use crate::model::{AuditEntry, TaskStatus};

    /// A migration that only moves between statuses: enough for the handlers' rules.
    #[derive(Default)]
    struct Fake {
        status: Mutex<Option<MigrationStatus>>,
    }

    impl Fake {
        fn current(&self) -> MigrationStatus {
            self.status
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(MigrationStatus::idle)
        }

        fn set(&self, status: &str) -> MigrationChange {
            let from = self.current().status;
            let mut next = self.current();
            next.status = status.to_owned();
            next.source = Some("postgresql://synapse@db:5432/synapse".to_owned());
            *self.status.lock().unwrap() = Some(next.clone());
            MigrationChange { from, status: next }
        }
    }

    #[async_trait]
    impl MigrationSource for Fake {
        async fn status(&self) -> Result<MigrationStatus, SourceError> {
            Ok(self.current())
        }
        async fn start(
            &self,
            request: &MigrationStartRequest,
            _actor: &Actor,
        ) -> Result<MigrationChange, SourceError> {
            if request.source_secret_ref.as_deref() == Some("/nowhere") {
                return Err(SourceError::Invalid(
                    "nothing is configured at /nowhere".into(),
                ));
            }
            if self.current().status == "copying" {
                return Err(SourceError::Conflict("already copying".into()));
            }
            Ok(self.set("copying"))
        }
        async fn pause(&self, _actor: &Actor) -> Result<Option<MigrationChange>, SourceError> {
            match self.current().status.as_str() {
                "paused" => Ok(None),
                "copying" => Ok(Some(self.set("paused"))),
                other => Err(SourceError::Conflict(format!("cannot pause while {other}"))),
            }
        }
        async fn resume(&self, _actor: &Actor) -> Result<Option<MigrationChange>, SourceError> {
            match self.current().status.as_str() {
                "copying" => Ok(None),
                "paused" => Ok(Some(self.set("copying"))),
                other => Err(SourceError::Conflict(format!(
                    "cannot resume while {other}"
                ))),
            }
        }
        async fn abort(&self, _actor: &Actor) -> Result<Option<MigrationChange>, SourceError> {
            match self.current().status.as_str() {
                "aborted" => Ok(None),
                "completed" => Err(SourceError::Conflict("already cut over".into())),
                _ => Ok(Some(self.set("aborted"))),
            }
        }
        async fn verify(&self, actor: &Actor) -> Result<Task, SourceError> {
            Ok(Task::scheduled(
                "migration.verify",
                Some(migration_ref()),
                actor.clone(),
            ))
        }
        async fn cutover(&self, actor: &Actor) -> Result<Task, SourceError> {
            if self.current().status != "ready_for_cutover" {
                return Err(SourceError::Conflict("the copy has not finished".into()));
            }
            Ok(Task::scheduled(
                "migration.cutover",
                Some(migration_ref()),
                actor.clone(),
            ))
        }
        async fn log(&self) -> Result<Vec<MigrationLogEntry>, SourceError> {
            Ok((0..5)
                .map(|i| MigrationLogEntry {
                    recorded_at: "2026-09-28T00:00:00Z".into(),
                    stream: "users".into(),
                    message: format!("entry {i}"),
                    level: "info".into(),
                })
                .collect())
        }
    }

    async fn actions(audit: &InMemoryAuditSink) -> Vec<String> {
        let entries: Vec<AuditEntry> = audit.query(&AuditFilter::default()).await.unwrap();
        let mut actions: Vec<String> = entries.into_iter().map(|e| e.action).collect();
        actions.sort();
        actions
    }

    #[tokio::test]
    async fn an_unwired_migration_answers_503_and_reads_need_a_token() {
        let (state, _audit) = state();
        let (status, _, _) =
            call(&state, "GET", "/api/v1/migration", Some("read"), None, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let state = state.with_migration(Arc::new(Fake::default()));
        let (status, _, _) = call(&state, "GET", "/api/v1/migration", None, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, body) =
            call(&state, "GET", "/api/v1/migration", Some("read"), None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "idle");
        let (status, _, _) = call(
            &state,
            "POST",
            "/api/v1/migration/start",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn each_control_changes_the_status_once_and_is_audited_once() {
        let (state, audit) = state();
        let state = state.with_migration(Arc::new(Fake::default()));
        let (status, _, body) = call(
            &state,
            "POST",
            "/api/v1/migration/start",
            Some("admin"),
            Some(json!({"source_secret_ref": "/migration/synapse"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "copying");
        let (status, _, body) = call(
            &state,
            "POST",
            "/api/v1/migration/start",
            Some("admin"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        for (path, expected) in [
            ("/api/v1/migration/pause", "paused"),
            ("/api/v1/migration/pause", "paused"),
            ("/api/v1/migration/resume", "copying"),
            ("/api/v1/migration/abort", "aborted"),
        ] {
            let (status, _, body) = call(&state, "POST", path, Some("admin"), None, None).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert_eq!(body["status"], expected, "{path}");
        }
        // The second pause changed nothing and was not recorded.
        assert_eq!(
            actions(&audit).await,
            vec![
                "migration.abort",
                "migration.pause",
                "migration.resume",
                "migration.start"
            ]
        );
        let (status, _, _) = call(
            &state,
            "POST",
            "/api/v1/migration/resume",
            Some("admin"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_missing_source_is_a_validation_error() {
        let (state, audit) = state();
        let state = state.with_migration(Arc::new(Fake::default()));
        let (status, _, body) = call(
            &state,
            "POST",
            "/api/v1/migration/start",
            Some("admin"),
            Some(json!({"source_secret_ref": "/nowhere"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(actions(&audit).await.is_empty());
    }

    #[tokio::test]
    async fn verify_and_cutover_answer_202_with_the_task() {
        let (state, audit) = state();
        let fake = Arc::new(Fake::default());
        let state = state.with_migration(fake.clone());
        let (status, _, body) = call(
            &state,
            "POST",
            "/api/v1/migration/cutover",
            Some("admin"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        fake.set("ready_for_cutover");
        for (path, action) in [
            ("/api/v1/migration/verify", "migration.verify"),
            ("/api/v1/migration/cutover", "migration.cutover"),
        ] {
            let (status, _, body) = call(&state, "POST", path, Some("admin"), None, None).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{path}: {body}");
            assert_eq!(body["action"], action);
            assert_eq!(
                serde_json::from_value::<TaskStatus>(body["status"].clone()).unwrap(),
                TaskStatus::Scheduled
            );
        }
        assert_eq!(
            actions(&audit).await,
            vec!["migration.cutover", "migration.verify"]
        );
    }

    #[tokio::test]
    async fn the_log_is_paged() {
        let (state, _audit) = state();
        let state = state.with_migration(Arc::new(Fake::default()));
        let (status, _, body) = call(
            &state,
            "GET",
            "/api/v1/migration/log?limit=2&include_total=true",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["total"], 5);
        let next = body["next_cursor"].as_str().unwrap().to_owned();
        let (_, _, body) = call(
            &state,
            "GET",
            &format!("/api/v1/migration/log?limit=10&cursor={next}"),
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(body["items"].as_array().unwrap().len(), 3);
        assert_eq!(body["items"][0]["message"], "entry 2");
        let (status, _, _) = call(
            &state,
            "GET",
            "/api/v1/migration/log?cursor=nonsense",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
