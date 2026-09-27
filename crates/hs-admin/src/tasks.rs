//! Tasks: the admin face of work that outlives the request that started it (`tasks.*`, RFC
//! 0004 section 3.7).
//!
//! [`TaskRegistry`] is the one place long-running work reports into. A job either
//!
//! - runs under the registry ([`TaskRegistry::spawn`]): the registry records it as `running`,
//!   runs it on the runtime, lets it report progress through its [`TaskContext`], records how it
//!   ended, and can cancel it; or
//! - finishes on its own and is recorded afterwards ([`TaskRegistry::record_finished`]), for work
//!   that is quick but still answered with a `Task` by the contract (an appservice replay
//!   re-queues transactions in one step; the delivery it causes is somebody else's job).
//!
//! Every change of status is written to a [`TaskStore`] -- durable in a real server (`hs-cli`'s
//! `TablesTaskStore`), in memory here for tests -- and published as a `task.changed` event.
//!
//! # Restarts and replicas
//!
//! A task's future lives in one process. When that process stops, nothing is running the task
//! any more, and saying `running` forever would be a lie; [`TaskRegistry::recover_interrupted`],
//! called once at startup, marks every task this runner left `scheduled` or `running` as
//! `failed`, with a problem saying it was interrupted. Each record carries the runner (replica)
//! that owns it, so one replica restarting never fails another replica's work.
//!
//! # Cancelling
//!
//! Best effort, as the contract says. A task running in this process is stopped at its next
//! `.await` (its future is dropped) and recorded `cancelled` at once. A task running on another
//! replica is recorded `cancelled`; that replica notices at the task's next progress report and
//! stops it, and never overwrites the `cancelled` with its own outcome. A task that has already
//! ended is answered as it is: cancelling it is not an error, and changes nothing.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::{Problem, ValidationError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;

use crate::events::EventBus;
use crate::handler_kit::{authorize, check_replay, record, respond_and_remember, unwired};
use crate::model::{
    Actor, AuditChange, Event, Page, ResourceRef, Scope, Task, TaskProgress, TaskStatus,
};
use crate::router::AdminState;
use crate::sources::SourceError;

/// How long a finished task is kept before [`TaskRegistry::recover_interrupted`] prunes it.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A task as stored: the wire [`Task`] and the runner that owns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// The task, exactly as the API answers it.
    pub task: Task,
    /// The runner (replica) whose process is running it. `None` for a task recorded already
    /// finished.
    #[serde(default)]
    pub runner: Option<String>,
}

/// Where tasks are kept. `hs-cli` implements it over `hs-kv`; [`InMemoryTaskStore`] is for tests.
#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    /// Inserts or replaces a task.
    async fn put(&self, record: &TaskRecord) -> Result<(), SourceError>;
    /// One task.
    async fn get(&self, id: &str) -> Result<Option<TaskRecord>, SourceError>;
    /// Every task, in any order.
    async fn list(&self) -> Result<Vec<TaskRecord>, SourceError>;
    /// Removes a task (pruning). Removing one that is not there is not an error.
    async fn delete(&self, id: &str) -> Result<(), SourceError>;
}

/// A [`TaskStore`] in memory: not durable.
#[derive(Default)]
pub struct InMemoryTaskStore {
    tasks: RwLock<HashMap<String, TaskRecord>>,
}

impl InMemoryTaskStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn put(&self, record: &TaskRecord) -> Result<(), SourceError> {
        self.tasks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(record.task.id.clone(), record.clone());
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<TaskRecord>, SourceError> {
        Ok(self
            .tasks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned())
    }

    async fn list(&self) -> Result<Vec<TaskRecord>, SourceError> {
        Ok(self
            .tasks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<(), SourceError> {
        self.tasks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        Ok(())
    }
}

/// Whether a status is final.
#[must_use]
pub fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

/// The `GET /tasks` filters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskFilter {
    /// Only tasks in this status.
    pub status: Option<TaskStatus>,
    /// Only tasks whose action is this (`media.purge_remote_cache`) or, ending in `.`, starts
    /// with it (`media.`).
    pub action: Option<String>,
}

impl TaskFilter {
    fn matches(&self, task: &Task) -> bool {
        self.status.is_none_or(|s| task.status == s)
            && self.action.as_deref().is_none_or(|a| {
                if a.ends_with('.') {
                    task.action.starts_with(a)
                } else {
                    task.action == a
                }
            })
    }
}

fn parse_status(s: &str) -> Option<TaskStatus> {
    match s {
        "scheduled" => Some(TaskStatus::Scheduled),
        "running" => Some(TaskStatus::Running),
        "succeeded" => Some(TaskStatus::Succeeded),
        "failed" => Some(TaskStatus::Failed),
        "cancelled" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

/// The registry every long-running job reports into. See the module docs.
pub struct TaskRegistry {
    store: Arc<dyn TaskStore>,
    runner: String,
    events: OnceLock<Arc<EventBus>>,
    /// Tasks running in this process, and how to stop each.
    live: Mutex<HashMap<String, watch::Sender<bool>>>,
    retention: Duration,
}

impl TaskRegistry {
    /// A registry over `store`, running tasks as `runner` (this replica's name; any fixed string
    /// in single-node mode).
    #[must_use]
    pub fn new(store: Arc<dyn TaskStore>, runner: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            store,
            runner: runner.into(),
            events: OnceLock::new(),
            live: Mutex::new(HashMap::new()),
            retention: DEFAULT_RETENTION,
        })
    }

    /// A registry over an [`InMemoryTaskStore`], for tests.
    #[must_use]
    pub fn in_memory() -> Arc<Self> {
        Self::new(Arc::new(InMemoryTaskStore::new()), "local")
    }

    /// Where `task.changed` events are published. Set by [`AdminState::with_tasks`]; later
    /// calls are ignored.
    pub fn attach_events(&self, events: Arc<EventBus>) {
        let _ = self.events.set(events);
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, watch::Sender<bool>>> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn save(&self, task: &Task, runner: Option<String>) -> Result<(), SourceError> {
        self.store
            .put(&TaskRecord {
                task: task.clone(),
                runner,
            })
            .await?;
        if let Some(events) = self.events.get() {
            events.publish(
                Event::new(
                    "task.changed",
                    serde_json::to_value(task).unwrap_or_default(),
                )
                .with_resource(ResourceRef::new("task", task.id.clone())),
            );
        }
        Ok(())
    }

    /// Records a task that has already ended (its `status` should be terminal), so that it can
    /// be listed and fetched like any other.
    ///
    /// # Errors
    /// The store's.
    pub async fn record_finished(&self, task: Task) -> Result<Task, SourceError> {
        self.save(&task, None).await?;
        Ok(task)
    }

    /// Starts `work` as a task: recorded `running` before this returns, run on the runtime, and
    /// recorded `succeeded` (with the value it returns as `result`) or `failed` (with its
    /// problem as `error`) when it ends.
    ///
    /// # Errors
    /// The store's, in which case nothing is started.
    pub async fn spawn<F, Fut>(
        self: &Arc<Self>,
        action: impl Into<String>,
        resource: Option<ResourceRef>,
        created_by: Actor,
        work: F,
    ) -> Result<Task, SourceError>
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<serde_json::Value, Problem>> + Send + 'static,
    {
        let mut task = Task::scheduled(action, resource, created_by);
        task.status = TaskStatus::Running;
        task.started_at = Some(task.created_at.clone());
        let (stop, stopped) = watch::channel(false);
        self.live().insert(task.id.clone(), stop);
        if let Err(e) = self.save(&task, Some(self.runner.clone())).await {
            self.live().remove(&task.id);
            return Err(e);
        }
        let registry = Arc::clone(self);
        let id = task.id.clone();
        let context = TaskContext {
            id: id.clone(),
            registry: Arc::clone(self),
            stopped: stopped.clone(),
        };
        tokio::spawn(async move {
            let mut stopped = stopped;
            let outcome = tokio::select! {
                outcome = work(context) => Some(outcome),
                () = wait_until_stopped(&mut stopped) => None,
            };
            registry.finish(&id, outcome).await;
        });
        Ok(task)
    }

    /// Records how a spawned task ended, unless it was cancelled meanwhile (here, or by another
    /// replica: a stored `cancelled` is never overwritten).
    async fn finish(&self, id: &str, outcome: Option<Result<serde_json::Value, Problem>>) {
        let still_live = self.live().remove(id).is_some();
        let Some(outcome) = outcome else {
            // Stopped by `cancel`, which recorded it.
            return;
        };
        if !still_live {
            return;
        }
        let mut task = match self.store.get(id).await {
            Ok(Some(record)) => record.task,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(task = id, %error, "could not read a finished task back to record its outcome");
                return;
            }
        };
        if is_terminal(task.status) {
            return;
        }
        task.finished_at = Some(hs_http::time::now_rfc3339());
        match outcome {
            Ok(result) => {
                task.status = TaskStatus::Succeeded;
                task.result = Some(result);
            }
            Err(problem) => {
                task.status = TaskStatus::Failed;
                task.error = Some(problem);
            }
        }
        if let Err(error) = self.save(&task, Some(self.runner.clone())).await {
            tracing::error!(task = id, %error, "could not record how a task ended");
        }
    }

    /// Cancels a task (best effort; see the module docs) and answers it as it now stands.
    ///
    /// # Errors
    /// [`SourceError::NotFound`] if there is no such task; the store's otherwise.
    pub async fn cancel(&self, id: &str) -> Result<(Task, bool), SourceError> {
        let record = self.store.get(id).await?.ok_or(SourceError::NotFound)?;
        let mut task = record.task;
        if is_terminal(task.status) {
            return Ok((task, false));
        }
        if let Some(stop) = self.live().remove(id) {
            let _ = stop.send(true);
        }
        task.status = TaskStatus::Cancelled;
        task.finished_at = Some(hs_http::time::now_rfc3339());
        self.save(&task, record.runner).await?;
        Ok((task, true))
    }

    /// One task.
    ///
    /// # Errors
    /// The store's.
    pub async fn get(&self, id: &str) -> Result<Option<Task>, SourceError> {
        Ok(self.store.get(id).await?.map(|r| r.task))
    }

    /// Every task `filter` admits, newest first.
    ///
    /// # Errors
    /// The store's.
    pub async fn list(&self, filter: &TaskFilter) -> Result<Vec<Task>, SourceError> {
        let mut tasks: Vec<Task> = self
            .store
            .list()
            .await?
            .into_iter()
            .map(|r| r.task)
            .filter(|t| filter.matches(t))
            .collect();
        tasks.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(tasks)
    }

    /// At startup: every task this runner left unfinished is marked `failed` (nothing is running
    /// it any more), and finished tasks older than the retention period are removed. Returns how
    /// many were marked failed.
    ///
    /// # Errors
    /// The store's.
    pub async fn recover_interrupted(&self) -> Result<usize, SourceError> {
        let now = time::OffsetDateTime::now_utc();
        let cutoff = now - self.retention;
        let mut interrupted = 0;
        for record in self.store.list().await? {
            let mut task = record.task;
            let mine = record.runner.as_deref().is_none_or(|r| r == self.runner);
            if !is_terminal(task.status) && mine && !self.live().contains_key(&task.id) {
                task.status = TaskStatus::Failed;
                task.finished_at = Some(hs_http::time::format_rfc3339(now));
                task.error = Some(Problem::unavailable().with_detail(
                    "interrupted: the server stopped while this task was running, and it was \
                     not resumed",
                ));
                self.save(&task, record.runner).await?;
                interrupted += 1;
                continue;
            }
            let expired = task
                .finished_at
                .as_deref()
                .and_then(|f| hs_http::time::parse_rfc3339(f).ok())
                .is_some_and(|finished| finished < cutoff);
            if is_terminal(task.status) && expired {
                self.store.delete(&task.id).await?;
            }
        }
        Ok(interrupted)
    }
}

async fn wait_until_stopped(stopped: &mut watch::Receiver<bool>) {
    loop {
        if *stopped.borrow_and_update() {
            return;
        }
        if stopped.changed().await.is_err() {
            // The registry forgot the task without stopping it (it finished): never resolve.
            std::future::pending::<()>().await;
        }
    }
}

/// What a spawned task's work is handed: its id, a way to report progress, and whether it has
/// been asked to stop.
#[derive(Clone)]
pub struct TaskContext {
    id: String,
    registry: Arc<TaskRegistry>,
    stopped: watch::Receiver<bool>,
}

impl TaskContext {
    /// The task's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the task has been cancelled. Work that would rather stop at a point of its
    /// choosing than at an arbitrary `.await` checks this between steps.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.stopped.borrow()
    }

    /// Records progress (`current` of `total` `unit`s, with an optional message). A cancellation
    /// recorded by another replica is noticed here, and stops the task.
    pub async fn progress(
        &self,
        current: u64,
        total: Option<u64>,
        unit: Option<&str>,
        message: Option<&str>,
    ) {
        let record = match self.registry.store.get(&self.id).await {
            Ok(Some(record)) => record,
            _ => return,
        };
        let mut task = record.task;
        if task.status == TaskStatus::Cancelled {
            if let Some(stop) = self.registry.live().remove(&self.id) {
                let _ = stop.send(true);
            }
            return;
        }
        if is_terminal(task.status) {
            return;
        }
        task.progress = Some(TaskProgress {
            current,
            total,
            unit: unit.map(str::to_owned),
            message: message.map(str::to_owned),
        });
        if let Err(error) = self.registry.save(&task, record.runner).await {
            tracing::warn!(task = self.id, %error, "could not record a task's progress");
        }
    }
}

// -------------------------------------------------------------------------------------------
// Handlers.
// -------------------------------------------------------------------------------------------

/// `GET /tasks`'s query string.
#[derive(Debug, Deserialize)]
pub(crate) struct TasksQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    status: Option<String>,
    action: Option<String>,
}

fn no_such_task(id: &str, instance: &str) -> Response {
    Problem::not_found()
        .with_detail(format!("there is no task {id}"))
        .with_instance(instance.to_owned())
        .into_response()
}

/// `GET /api/v1/tasks` (`admin:read`): newest first; `status` and `action` narrow it.
pub(crate) async fn tasks_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<TasksQuery>,
) -> Response {
    let instance = "/api/v1/tasks";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(tasks) = &state.tasks else {
        return unwired("task registry", instance);
    };
    let mut filter = TaskFilter {
        action: query.action.clone().filter(|a| !a.is_empty()),
        ..TaskFilter::default()
    };
    if let Some(status) = query.status.as_deref().filter(|s| !s.is_empty()) {
        match parse_status(status) {
            Some(status) => filter.status = Some(status),
            None => {
                let detail = format!(
                    "status must be scheduled, running, succeeded, failed or cancelled, not \
                     {status:?}"
                );
                return Problem::validation_failed()
                    .with_detail(detail.clone())
                    .with_errors(vec![ValidationError::new("/status", detail)])
                    .with_instance(instance)
                    .into_response();
            }
        }
    }
    match tasks.list(&filter).await {
        Ok(items) => axum::Json(Page::paginate(
            items,
            query.cursor.as_deref(),
            query.limit,
            query.include_total.unwrap_or(false),
        ))
        .into_response(),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/tasks/{id}` (`admin:read`).
pub(crate) async fn tasks_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/tasks/{id}");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let Some(tasks) = &state.tasks else {
        return unwired("task registry", &instance);
    };
    match tasks.get(&id).await {
        Ok(Some(task)) => axum::Json(task).into_response(),
        Ok(None) => no_such_task(&id, &instance),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// `POST /api/v1/tasks/{id}/cancel` (`admin:write`): best-effort cancel. A task that had not
/// ended is audited as `tasks.cancel` and published as `task.cancelled`; one that had is
/// answered unchanged, with no audit entry, because nothing was done.
pub(crate) async fn tasks_cancel(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/tasks/{id}/cancel");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(tasks) = &state.tasks else {
        return unwired("task registry", &instance);
    };
    if let Err(response) = check_replay(&state, &headers, "tasks.cancel", &body, &instance) {
        return response;
    }
    let before = match tasks.get(&id).await {
        Ok(Some(task)) => task.status,
        Ok(None) => return no_such_task(&id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    let (task, changed) = match tasks.cancel(&id).await {
        Ok(outcome) => outcome,
        Err(SourceError::NotFound) => return no_such_task(&id, &instance),
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    if changed
        && let Err(response) = record(
            &state,
            &principal,
            "tasks.cancel",
            "task.cancelled",
            ResourceRef::new("task", id.clone()),
            vec![AuditChange {
                pointer: "/status".to_owned(),
                from: Some(json!(before)),
                to: Some(json!(task.status)),
            }],
            json!({ "id": id, "action": task.action }),
            200,
        )
        .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        "tasks.cancel",
        &body,
        StatusCode::OK,
        &task,
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ActorKind;

    fn actor() -> Actor {
        Actor {
            kind: ActorKind::User,
            id: "@ops:example.org".to_owned(),
            display_name: None,
            token_id: None,
            ip: None,
            user_agent: None,
        }
    }

    async fn settled(registry: &TaskRegistry, id: &str) -> Task {
        for _ in 0..200 {
            let task = registry.get(id).await.unwrap().unwrap();
            if is_terminal(task.status) {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("task {id} never ended");
    }

    #[tokio::test]
    async fn a_spawned_task_is_running_then_records_its_result_and_progress() {
        let registry = TaskRegistry::in_memory();
        let (go, wait) = tokio::sync::oneshot::channel::<()>();
        let task = registry
            .spawn(
                "media.purge_remote_cache",
                Some(ResourceRef::new("media", "all")),
                actor(),
                |ctx| async move {
                    ctx.progress(1, Some(2), Some("files"), Some("halfway"))
                        .await;
                    let _ = wait.await;
                    Ok(json!({ "deleted": 2 }))
                },
            )
            .await
            .unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert!(task.started_at.is_some());

        // Progress lands while it runs.
        for _ in 0..200 {
            let now = registry.get(&task.id).await.unwrap().unwrap();
            if let Some(progress) = now.progress {
                assert_eq!(progress.current, 1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        go.send(()).unwrap();
        let done = settled(&registry, &task.id).await;
        assert_eq!(done.status, TaskStatus::Succeeded);
        assert_eq!(done.result, Some(json!({ "deleted": 2 })));
        assert!(done.finished_at.is_some());
    }

    #[tokio::test]
    async fn a_failing_task_records_its_problem() {
        let registry = TaskRegistry::in_memory();
        let task = registry
            .spawn("rooms.delete", None, actor(), |_ctx| async {
                Err(Problem::conflict().with_detail("the room is still in use"))
            })
            .await
            .unwrap();
        let done = settled(&registry, &task.id).await;
        assert_eq!(done.status, TaskStatus::Failed);
        assert_eq!(
            done.error.unwrap().detail.as_deref(),
            Some("the room is still in use")
        );
    }

    #[tokio::test]
    async fn cancelling_stops_the_work_and_is_never_overwritten() {
        let registry = TaskRegistry::in_memory();
        let ran_to_the_end = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran_to_the_end.clone();
        let task = registry
            .spawn(
                "users.redact_events",
                None,
                actor(),
                move |_ctx| async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(json!({}))
                },
            )
            .await
            .unwrap();
        let (cancelled, changed) = registry.cancel(&task.id).await.unwrap();
        assert!(changed);
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let now = registry.get(&task.id).await.unwrap().unwrap();
        assert_eq!(now.status, TaskStatus::Cancelled);
        assert!(!ran_to_the_end.load(std::sync::atomic::Ordering::SeqCst));

        // Cancelling again changes nothing and is not an error.
        let (again, changed) = registry.cancel(&task.id).await.unwrap();
        assert!(!changed);
        assert_eq!(again.finished_at, cancelled.finished_at);
        assert!(matches!(
            registry.cancel("nope").await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_cancel_recorded_elsewhere_stops_the_task_at_its_next_progress_report() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let here = TaskRegistry::new(store.clone(), "replica-a");
        let there = TaskRegistry::new(store, "replica-b");
        let (step, mut steps) = tokio::sync::mpsc::channel::<()>(1);
        let task = here
            .spawn(
                "rooms.purge_history",
                None,
                actor(),
                move |ctx| async move {
                    loop {
                        let _ = step.send(()).await;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        ctx.progress(1, None, None, None).await;
                    }
                },
            )
            .await
            .unwrap();
        steps.recv().await.unwrap();
        // Replica b records the cancel; replica a is not told directly.
        let (cancelled, changed) = there.cancel(&task.id).await.unwrap();
        assert!(changed);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(here.live().is_empty(), "replica a stopped the work");
        assert_eq!(
            here.get(&task.id).await.unwrap().unwrap().finished_at,
            cancelled.finished_at
        );
    }

    #[tokio::test]
    async fn a_restart_fails_this_runners_unfinished_tasks_only_and_prunes_old_ones() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let mut mine = Task::scheduled("media.delete", None, actor());
        mine.status = TaskStatus::Running;
        let mut theirs = Task::scheduled("media.delete", None, actor());
        theirs.status = TaskStatus::Running;
        let mut ancient = Task::scheduled("appservices.replay", None, actor());
        ancient.status = TaskStatus::Succeeded;
        ancient.finished_at = Some("2020-01-01T00:00:00Z".to_owned());
        let mut recent = Task::scheduled("appservices.replay", None, actor());
        recent.status = TaskStatus::Succeeded;
        recent.finished_at = Some(hs_http::time::now_rfc3339());
        for (task, runner) in [
            (&mine, Some("a")),
            (&theirs, Some("b")),
            (&ancient, None),
            (&recent, None),
        ] {
            store
                .put(&TaskRecord {
                    task: task.clone(),
                    runner: runner.map(str::to_owned),
                })
                .await
                .unwrap();
        }
        let registry = TaskRegistry::new(store, "a");
        assert_eq!(registry.recover_interrupted().await.unwrap(), 1);
        let after = registry.get(&mine.id).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Failed);
        assert!(after.error.unwrap().detail.unwrap().contains("interrupted"));
        assert_eq!(
            registry.get(&theirs.id).await.unwrap().unwrap().status,
            TaskStatus::Running
        );
        assert!(registry.get(&ancient.id).await.unwrap().is_none());
        assert!(registry.get(&recent.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_is_newest_first_and_filters_by_status_and_action_prefix() {
        let registry = TaskRegistry::in_memory();
        let mut first = Task::scheduled("media.delete", None, actor());
        first.created_at = "2026-09-27T10:00:00Z".to_owned();
        first.status = TaskStatus::Failed;
        let mut second = Task::scheduled("appservices.replay", None, actor());
        second.created_at = "2026-09-27T11:00:00Z".to_owned();
        second.status = TaskStatus::Succeeded;
        registry.record_finished(first.clone()).await.unwrap();
        registry.record_finished(second.clone()).await.unwrap();

        let all = registry.list(&TaskFilter::default()).await.unwrap();
        assert_eq!(all[0].id, second.id);
        let media = registry
            .list(&TaskFilter {
                action: Some("media.".to_owned()),
                ..TaskFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(media.len(), 1);
        let failed = registry
            .list(&TaskFilter {
                status: Some(TaskStatus::Failed),
                ..TaskFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(failed[0].id, first.id);
    }

    #[tokio::test]
    async fn the_http_operations_list_get_and_cancel_with_an_audit_entry() {
        use crate::audit::{AuditFilter, AuditSink};
        use crate::handler_kit::testing::{call, state};
        use axum::http::StatusCode;

        let (state, audit) = state();
        let (status, _, _) = call(&state, "GET", "/api/v1/tasks", Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let registry = TaskRegistry::in_memory();
        let state = state.with_tasks(registry.clone());
        let running = registry
            .spawn(
                "rooms.delete",
                Some(ResourceRef::new("room", "!r:example.org")),
                actor(),
                |_| async { std::future::pending::<Result<serde_json::Value, Problem>>().await },
            )
            .await
            .unwrap();
        let mut done = Task::scheduled("appservices.replay", None, actor());
        done.status = TaskStatus::Succeeded;
        registry.record_finished(done.clone()).await.unwrap();

        let (status, _, page) = call(
            &state,
            "GET",
            "/api/v1/tasks?status=running",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["id"], running.id.as_str());
        let (status, _, _) = call(
            &state,
            "GET",
            "/api/v1/tasks?status=paused",
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, one) = call(
            &state,
            "GET",
            &format!("/api/v1/tasks/{}", done.id),
            Some("read"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(one["action"], "appservices.replay");

        // Cancelling needs admin:write.
        let cancel = format!("/api/v1/tasks/{}/cancel", running.id);
        let (status, _, _) = call(&state, "POST", &cancel, Some("read"), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, cancelled) = call(&state, "POST", &cancel, Some("admin"), None, None).await;
        assert_eq!(status, StatusCode::OK, "{cancelled}");
        assert_eq!(cancelled["status"], "cancelled");
        // Cancelling what has ended answers it unchanged, and records nothing.
        let (status, _, _) = call(
            &state,
            "POST",
            &format!("/api/v1/tasks/{}/cancel", done.id),
            Some("admin"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let entries = audit
            .query(&AuditFilter {
                action: Some("tasks.cancel".to_owned()),
                limit: 10,
                ..AuditFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target.id, running.id);
        let (status, _, _) = call(
            &state,
            "POST",
            "/api/v1/tasks/nope/cancel",
            Some("admin"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn every_change_is_published_as_task_changed() {
        let registry = TaskRegistry::in_memory();
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        registry.attach_events(bus);
        let task = registry
            .spawn("rooms.delete", None, actor(), |_| async { Ok(json!({})) })
            .await
            .unwrap();
        settled(&registry, &task.id).await;
        let first = events.recv().await.unwrap();
        assert_eq!(first.r#type, "task.changed");
        assert_eq!(first.data["status"], "running");
        let second = events.recv().await.unwrap();
        assert_eq!(second.data["status"], "succeeded");
    }
}
