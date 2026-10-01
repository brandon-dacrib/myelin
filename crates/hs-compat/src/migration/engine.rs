//! [`Migrator`]: the migration's state machine, the copy, verification and cutover, and the
//! admin API's [`MigrationSource`] over them.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use hs_admin::events::EventBus;
use hs_admin::migration::{
    DEFAULT_SOURCE_REF, MigrationChange, MigrationLogEntry, MigrationSource, MigrationStartRequest,
    MigrationStatus, MigrationStreamStatus, MigrationStreamVerification, MigrationVerification,
};
use hs_admin::model::{Actor, Event, ResourceRef, Task};
use hs_admin::sources::SourceError;
use hs_admin::tasks::{TaskContext, TaskRegistry};
use hs_config::migration::SynapseSourceConfig;
use hs_http::Problem;
use serde_json::{Value, json};

use super::MigrationError;
use super::model::{
    LogEntry, LogLevel, MigrationRecord, Phase, Stream, StreamProgress, StreamVerification,
    VerificationReport,
};
use super::rooms::{RoomFailure, SynapseRoomPages, copy_room};
use super::source::{
    SynapseSource, account_data_key, device_key, device_pair_key, pair_key, parse_account_data_key,
    parse_device_key, parse_pair_key,
};
use super::store::MigrationStore;
use super::target::{Check, Imported, MigrationTarget, TargetError};
use super::throughput::{ImportStats, RoomStats, peak_rss_bytes};

/// The task actions each step runs under.
pub const COPY_ACTION: &str = "migration.copy";
/// See [`COPY_ACTION`].
pub const VERIFY_ACTION: &str = "migration.verify";
/// See [`COPY_ACTION`].
pub const CUTOVER_ACTION: &str = "migration.cutover";

/// Rooms per batch: each room's events are read a page of `batch_size` at a time, so a room is
/// far more than a row of another stream.
const ROOMS_PER_BATCH: i64 = 10;
/// Refused events logged per room; the rest are counted.
const REFUSALS_LOGGED_PER_ROOM: usize = 20;
/// Mismatch lines kept per stream in a verification.
const MISMATCHES_KEPT: usize = 25;

/// Reads a migration source out of the running configuration.
#[async_trait]
pub trait SourceConfigs: Send + Sync + 'static {
    /// The [`SynapseSourceConfig`] at `pointer` (`/migration/synapse`) in the current,
    /// unredacted configuration: `Ok(None)` when nothing is set there, `Err` with why when what
    /// is there is not a source.
    ///
    /// # Errors
    /// The value at `pointer` is not a `SynapseSourceConfig`.
    async fn source(&self, pointer: &str) -> Result<Option<SynapseSourceConfig>, String>;
}

/// Told about every change to the record, and about each room copied, for metrics.
pub trait MigrationObserver: Send + Sync + 'static {
    /// The record as it now is.
    fn observe(&self, record: &MigrationRecord);

    /// A room has been copied: how many events, how many bytes, how long, and the process's
    /// peak memory then.
    fn room_copied(&self, _stats: &RoomStats) {}
}

struct NoObserver;

impl MigrationObserver for NoObserver {
    fn observe(&self, _record: &MigrationRecord) {}
}

/// What a [`Migrator`] is built from.
pub struct MigratorParts {
    /// Where its record and log are kept.
    pub store: Arc<dyn MigrationStore>,
    /// This server's stores.
    pub target: Arc<dyn MigrationTarget>,
    /// Where it reads its source from.
    pub configs: Arc<dyn SourceConfigs>,
    /// Where its steps run.
    pub tasks: Arc<TaskRegistry>,
    /// Where it announces what happens inside its tasks (`migration.ready_for_cutover`,
    /// `migration.completed`, `migration.failed`, `migration.verified`).
    pub events: Option<Arc<EventBus>>,
    /// Told about every change, for metrics.
    pub observer: Option<Arc<dyn MigrationObserver>>,
    /// Rows compared field by field per stream in a verification.
    pub sample_size: i64,
}

/// The migration from Synapse. See the module docs of [`crate::migration`].
pub struct Migrator {
    me: Weak<Migrator>,
    store: Arc<dyn MigrationStore>,
    target: Arc<dyn MigrationTarget>,
    configs: Arc<dyn SourceConfigs>,
    tasks: Arc<TaskRegistry>,
    events: Option<Arc<EventBus>>,
    observer: Arc<dyn MigrationObserver>,
    sample_size: i64,
    /// Serializes every read-modify-write of the record in this process.
    lock: tokio::sync::Mutex<()>,
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn entry(stream: &str, level: LogLevel, message: impl Into<String>) -> LogEntry {
    LogEntry {
        recorded_at: now_rfc3339(),
        stream: stream.to_owned(),
        message: message.into(),
        level,
    }
}

fn store_error(e: MigrationError) -> SourceError {
    SourceError::Unavailable(e.to_string())
}

/// The wire status of a record.
#[must_use]
pub fn to_status(record: &MigrationRecord) -> MigrationStatus {
    MigrationStatus {
        status: record.phase.as_str().to_owned(),
        source: record.source.clone(),
        streams: record
            .streams
            .iter()
            .map(|s| MigrationStreamStatus {
                name: s.stream.as_str().to_owned(),
                copied_count: s.copied,
                total_count: s.total,
                rate_per_second: s.rate_per_second,
                skipped_count: s.skipped,
                failed_count: s.failed,
                done: s.done,
            })
            .collect(),
        estimated_remaining_ms: if record.phase == Phase::Copying {
            record.estimated_remaining_ms()
        } else {
            None
        },
        errors: record.errors.clone(),
        task_id: record.task_id.clone(),
        started_at: record.started_at.clone(),
        started_by: record.started_by.clone(),
        updated_at: record.updated_at.clone(),
        completed_at: record.completed_at.clone(),
        cutover_by: record.cutover_by.clone(),
        verification: record.verification.as_ref().map(|v| MigrationVerification {
            passed: v.passed,
            checked_at: v.checked_at.clone(),
            streams: v
                .streams
                .iter()
                .map(|s| MigrationStreamVerification {
                    name: s.name.clone(),
                    source_count: s.source_count,
                    target_count: s.target_count,
                    skipped_count: s.skipped_count,
                    sampled: s.sampled,
                    mismatches: s.mismatches.clone(),
                })
                .collect(),
        }),
    }
}

/// How a copy ended.
enum Copied {
    /// Every stream was read to its end.
    Finished,
    /// A newer run replaced this one (a pause, an abort): it stopped without writing more.
    Superseded,
}

/// What one batch did.
#[derive(Default)]
struct Batch {
    handled: u64,
    copied: u64,
    skipped: u64,
    failed: u64,
    next: Option<String>,
    log: Vec<LogEntry>,
    fatal: Option<String>,
    /// Things counted inside rows (one-time keys of a device, room keys of a backup), by name.
    detail: BTreeMap<&'static str, u64>,
    /// Each room copied.
    rooms: Vec<RoomStats>,
}

impl Batch {
    fn count(&mut self, what: &'static str, n: u64) {
        *self.detail.entry(what).or_default() += n;
    }

    fn tally(&mut self, stream: Stream, key: &str, outcome: Result<Imported, TargetError>) {
        self.handled += 1;
        match outcome {
            Ok(Imported::Created | Imported::Updated | Imported::AlreadyThere) => self.copied += 1,
            Ok(Imported::Skipped(why)) => {
                self.skipped += 1;
                self.log.push(entry(
                    stream.as_str(),
                    LogLevel::Warning,
                    format!("{key} was not copied: {why}"),
                ));
            }
            Err(e) if e.fatal => {
                self.handled -= 1;
                self.fatal = Some(e.message);
            }
            Err(e) => {
                self.failed += 1;
                self.log.push(entry(
                    stream.as_str(),
                    LogLevel::Error,
                    format!("{key} could not be copied: {}", e.message),
                ));
            }
        }
    }
}

impl Migrator {
    /// A migrator over `parts`.
    #[must_use]
    pub fn new(parts: MigratorParts) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            store: parts.store,
            target: parts.target,
            configs: parts.configs,
            tasks: parts.tasks,
            events: parts.events,
            observer: parts.observer.unwrap_or_else(|| Arc::new(NoObserver)),
            sample_size: parts.sample_size.max(1),
            lock: tokio::sync::Mutex::new(()),
        })
    }

    fn arc(&self) -> Result<Arc<Self>, SourceError> {
        self.me
            .upgrade()
            .ok_or_else(|| SourceError::Unavailable("the server is shutting down".to_owned()))
    }

    /// The record as it is.
    ///
    /// # Errors
    /// The store's.
    pub async fn record(&self) -> Result<MigrationRecord, MigrationError> {
        self.store.load().await
    }

    async fn write_log(&self, entries: Vec<LogEntry>) {
        if entries.is_empty() {
            return;
        }
        for e in &entries {
            match e.level {
                LogLevel::Info => tracing::info!(stream = %e.stream, "{}", e.message),
                LogLevel::Warning => tracing::warn!(stream = %e.stream, "{}", e.message),
                LogLevel::Error => tracing::error!(stream = %e.stream, "{}", e.message),
            }
        }
        if let Err(error) = self.store.append_log(&entries).await {
            tracing::warn!(%error, "could not keep the migration log");
        }
    }

    fn publish(&self, event_type: &str, record: &MigrationRecord) {
        if let Some(events) = &self.events {
            events.publish(
                Event::new(
                    event_type,
                    json!({
                        "status": record.phase.as_str(),
                        "source": record.source,
                        "task_id": record.task_id,
                    }),
                )
                .with_resource(ResourceRef::new("migration", "synapse"))
                .with_actor(Actor::system()),
            );
        }
    }

    /// Changes the record under the lock, and saves it.
    async fn update<F>(&self, f: F) -> Result<MigrationRecord, MigrationError>
    where
        F: FnOnce(&mut MigrationRecord),
    {
        let _guard = self.lock.lock().await;
        let mut record = self.store.load().await?;
        f(&mut record);
        record.updated_at = Some(now_rfc3339());
        self.store.save(&record).await?;
        self.observer.observe(&record);
        Ok(record)
    }

    /// As [`Migrator::update`], but only while `run` is still the current run: `Ok(None)` (and
    /// nothing written) once a pause, an abort or anything else has replaced it.
    async fn update_run<F>(&self, run: u64, f: F) -> Result<Option<MigrationRecord>, MigrationError>
    where
        F: FnOnce(&mut MigrationRecord),
    {
        let _guard = self.lock.lock().await;
        let mut record = self.store.load().await?;
        if record.run != run {
            return Ok(None);
        }
        f(&mut record);
        record.updated_at = Some(now_rfc3339());
        self.store.save(&record).await?;
        self.observer.observe(&record);
        Ok(Some(record))
    }

    async fn source_config(&self, pointer: &str) -> Result<SynapseSourceConfig, SourceError> {
        match self.configs.source(pointer).await {
            Ok(Some(config)) => Ok(config),
            Ok(None) => Err(SourceError::Invalid(format!(
                "no Synapse database is configured at {pointer}: set one first (the Migration \
                 page's first step, or config.update of the migration section)"
            ))),
            Err(why) => Err(SourceError::Invalid(format!(
                "{pointer} is not a Synapse source: {why}"
            ))),
        }
    }

    /// Connects to the source and checks it is a Synapse for this server's name.
    async fn open_source(
        &self,
        config: &SynapseSourceConfig,
    ) -> Result<SynapseSource, SourceError> {
        let source = SynapseSource::connect(config)
            .await
            .map_err(|e| SourceError::Invalid(e.to_string()))?;
        match source.server_name().await {
            Ok(Some(name)) if name != self.target.server_name() => {
                Err(SourceError::Invalid(format!(
                    "that Synapse is {name}, and this server is {}: a migration keeps the server \
                     name, so it must be the same",
                    self.target.server_name()
                )))
            }
            Ok(_) => Ok(source),
            Err(e) => Err(SourceError::Invalid(e.to_string())),
        }
    }

    /// Starts `action` as a task running `step` for `run`, and notes the task on the record.
    async fn spawn_step(
        &self,
        action: &'static str,
        run: u64,
        actor: Actor,
    ) -> Result<(Task, MigrationRecord), SourceError> {
        let me = self.arc()?;
        let task_actor = actor.clone();
        let task = self
            .tasks
            .spawn(
                action,
                Some(ResourceRef::new("migration", "synapse")),
                actor,
                move |ctx| async move {
                    match action {
                        VERIFY_ACTION => me.verify_task(run, ctx).await,
                        CUTOVER_ACTION => me.cutover_task(run, ctx, task_actor).await,
                        _ => me.copy_task(run, ctx).await,
                    }
                },
            )
            .await?;
        let id = task.id.clone();
        let mut superseded = false;
        let record = self
            .update(|r| {
                if r.run == run {
                    r.task_id = Some(id);
                } else {
                    superseded = true;
                }
            })
            .await
            .map_err(store_error)?;
        // A pause or an abort that landed between starting the task and noting it here could not
        // cancel it: do it now, so it stops before it copies anything more.
        if superseded {
            let _ = self.tasks.cancel(&task.id).await;
        }
        Ok((task, record))
    }

    /// At startup, after the task registry has marked the previous process's tasks
    /// interrupted: a migration that was copying, verifying or cutting over carries on from its
    /// last checkpoint, in a new task. Whatever the status, the observer (the metrics) is told
    /// it first.
    ///
    /// # Errors
    /// The store's, or the task registry's.
    pub async fn recover(&self) -> Result<Option<Phase>, SourceError> {
        let before = self.store.load().await.map_err(store_error)?;
        // What the metrics say starts from what the record says, not from zero.
        self.observer.observe(&before);
        if !before.phase.is_running() {
            return Ok(None);
        }
        let record = self
            .update(|r| {
                r.run += 1;
                r.task_id = None;
            })
            .await
            .map_err(store_error)?;
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!(
                "this server restarted while the migration was {}; carrying on from where it \
                 stopped",
                record.phase.as_str()
            ),
        )])
        .await;
        let action = match record.phase {
            Phase::Verifying => VERIFY_ACTION,
            Phase::CuttingOver => CUTOVER_ACTION,
            _ => COPY_ACTION,
        };
        self.spawn_step(action, record.run, Actor::system()).await?;
        Ok(Some(record.phase))
    }

    // ---------------------------------------------------------------------------------------
    // The copy.
    // ---------------------------------------------------------------------------------------

    // The task registry's contract is `Result<Value, Problem>`.
    #[allow(clippy::result_large_err)]
    async fn copy_task(self: Arc<Self>, run: u64, ctx: TaskContext) -> Result<Value, Problem> {
        match self.copy(run, &ctx).await {
            Ok(Copied::Finished) => {
                let Some(record) = self
                    .update_run(run, |r| {
                        if r.phase == Phase::Copying {
                            r.phase = Phase::ReadyForCutover;
                            r.task_id = None;
                        }
                    })
                    .await
                    .map_err(|e| Problem::unavailable().with_detail(e.to_string()))?
                else {
                    return Ok(json!({"stopped": true}));
                };
                self.write_log(vec![entry(
                    "migration",
                    LogLevel::Info,
                    "everything has been copied once: verify, then stop Synapse and cut over",
                )])
                .await;
                self.publish("migration.ready_for_cutover", &record);
                Ok(json!({ "status": to_status(&record) }))
            }
            Ok(Copied::Superseded) => Ok(json!({"stopped": true})),
            Err(e) => {
                let message = e.to_string();
                if let Ok(Some(record)) = self
                    .update_run(run, |r| {
                        r.phase = Phase::Failed;
                        r.task_id = None;
                        r.push_error(message.clone());
                    })
                    .await
                {
                    self.publish("migration.failed", &record);
                }
                self.write_log(vec![entry(
                    "migration",
                    LogLevel::Error,
                    format!("the copy stopped: {message}. Starting it again carries on from here"),
                )])
                .await;
                Err(Problem::unavailable().with_detail(message))
            }
        }
    }

    /// Copies every stream not yet done, from its checkpoint.
    async fn copy(&self, run: u64, ctx: &TaskContext) -> Result<Copied, MigrationError> {
        let copy_started = std::time::Instant::now();
        let record = self.store.load().await?;
        let pointer = record
            .source_ref
            .clone()
            .unwrap_or_else(|| DEFAULT_SOURCE_REF.to_owned());
        let config = self
            .source_config(&pointer)
            .await
            .map_err(|e| MigrationError::Config(e.to_string()))?;
        let source = self
            .open_source(&config)
            .await
            .map_err(|e| MigrationError::Source(e.to_string()))?;
        let batch_size = i64::from(config.batch_size.max(1));

        for stream in Stream::ALL {
            let Some(record) = self
                .update_run(run, |r| {
                    let s = r.stream_mut(stream);
                    s.run_started_ms = Some(now_ms());
                    s.run_rows = 0;
                })
                .await?
            else {
                return Ok(Copied::Superseded);
            };
            let progress = record
                .stream(stream)
                .cloned()
                .unwrap_or_else(|| StreamProgress::new(stream));
            if progress.done {
                continue;
            }
            let total = source.count(stream).await?;
            if self
                .update_run(run, |r| r.stream_mut(stream).total = Some(total))
                .await?
                .is_none()
            {
                return Ok(Copied::Superseded);
            }
            self.write_log(vec![entry(
                stream.as_str(),
                LogLevel::Info,
                match &progress.checkpoint {
                    None => format!("copying {total} rows"),
                    Some(at) => format!("carrying on after {at} ({total} rows in Synapse)"),
                },
            )])
            .await;
            let mut checkpoint = progress.checkpoint.clone();
            let mut detail: BTreeMap<&'static str, u64> = BTreeMap::new();
            let mut rooms = ImportStats::default();
            loop {
                let batch = self
                    .copy_batch(
                        stream,
                        &source,
                        checkpoint.as_deref(),
                        batch_size,
                        &config,
                        ctx,
                    )
                    .await?;
                if batch.handled == 0 && batch.fatal.is_none() {
                    let Some(record) = self
                        .update_run(run, |r| r.stream_mut(stream).done = true)
                        .await?
                    else {
                        return Ok(Copied::Superseded);
                    };
                    let s = record
                        .stream(stream)
                        .cloned()
                        .unwrap_or_else(|| StreamProgress::new(stream));
                    let mut done = format!(
                        "done: {} copied, {} not copied on purpose, {} failed",
                        s.copied, s.skipped, s.failed
                    );
                    if !detail.is_empty() {
                        let parts: Vec<String> = detail
                            .iter()
                            .map(|(what, n)| format!("{n} {what}"))
                            .collect();
                        done.push_str(&format!(" ({} in this run)", parts.join(", ")));
                    }
                    let mut lines = vec![entry(stream.as_str(), LogLevel::Info, done)];
                    if rooms.rooms > 0 {
                        lines.push(entry(
                            stream.as_str(),
                            LogLevel::Info,
                            format!("throughput: {}", rooms.summary()),
                        ));
                    }
                    self.write_log(lines).await;
                    break;
                }
                let Batch {
                    handled,
                    copied,
                    skipped,
                    failed,
                    next,
                    log,
                    fatal,
                    detail: batch_detail,
                    rooms: batch_rooms,
                } = batch;
                for (what, n) in batch_detail {
                    *detail.entry(what).or_default() += n;
                }
                for room in &batch_rooms {
                    rooms.add(room);
                    self.observer.room_copied(room);
                }
                self.write_log(log).await;
                let now = now_ms();
                let Some(record) = self
                    .update_run(run, |r| {
                        let s = r.stream_mut(stream);
                        s.copied += copied;
                        s.skipped += skipped;
                        s.failed += failed;
                        if next.is_some() {
                            s.checkpoint.clone_from(&next);
                        }
                        s.run_rows += handled;
                        let elapsed = now.saturating_sub(s.run_started_ms.unwrap_or(now)).max(1);
                        #[allow(clippy::cast_precision_loss)]
                        {
                            s.rate_per_second = s.run_rows as f64 * 1000.0 / elapsed as f64;
                        }
                    })
                    .await?
                else {
                    return Ok(Copied::Superseded);
                };
                if let Some(fatal) = fatal {
                    return Err(MigrationError::Target(fatal));
                }
                if next.is_some() {
                    checkpoint = next;
                }
                let (current, total_all) = record.streams.iter().fold((0, 0), |(c, t), s| {
                    (
                        c + s.copied + s.skipped + s.failed,
                        t + s.total.unwrap_or(0),
                    )
                });
                ctx.progress(
                    current,
                    Some(total_all),
                    Some("rows"),
                    Some(&format!(
                        "{}: {} copied",
                        stream.as_str(),
                        record.stream(stream).map_or(0, |s| s.copied)
                    )),
                )
                .await;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let peak = peak_rss_bytes().map_or_else(
            || "unknown".to_owned(),
            |b| format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0)),
        );
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!(
                "this pass over Synapse took {:.1} s; this server's peak memory so far is {peak}",
                copy_started.elapsed().as_secs_f64()
            ),
        )])
        .await;
        Ok(Copied::Finished)
    }

    async fn copy_batch(
        &self,
        stream: Stream,
        source: &SynapseSource,
        after: Option<&str>,
        limit: i64,
        config: &SynapseSourceConfig,
        ctx: &TaskContext,
    ) -> Result<Batch, MigrationError> {
        let mut batch = Batch::default();
        match stream {
            Stream::Users => {
                for user in source.users(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let outcome = self.target.import_user(&user).await;
                    batch.tally(stream, &user.user_id, outcome);
                    batch.next = Some(user.user_id.clone());
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::Devices => {
                let after = after.and_then(parse_device_key);
                let after = after.as_ref().map(|(u, d)| (u.as_str(), d.as_str()));
                for device in source.devices(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!("{}'s device {}", device.user_id, device.device_id);
                    let outcome = if device.hidden {
                        Ok(Imported::Skipped(
                            "a hidden device Synapse keeps for its own purposes".to_owned(),
                        ))
                    } else {
                        self.target.import_device(&device).await
                    };
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(device_key(&device));
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::AccessTokens => {
                let after = after.and_then(|a| a.parse::<i64>().ok());
                for token in source.access_tokens(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!("access token {} of {}", token.id, token.user_id);
                    let outcome = if token.puppets_user_id.is_some() {
                        Ok(Imported::Skipped(
                            "an administrator's token for acting as this user".to_owned(),
                        ))
                    } else {
                        self.target.import_access_token(&token).await
                    };
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(token.id.to_string());
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::AccountData => {
                let after = after.and_then(parse_account_data_key);
                for (data, key) in source.account_data(after.as_ref(), limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let label = match &data.room_id {
                        Some(room) => format!("{}'s {} in {room}", data.user_id, data.data_type),
                        None => format!("{}'s {}", data.user_id, data.data_type),
                    };
                    let outcome = if data.data_type == "m.push_rules" {
                        Ok(Imported::Skipped(
                            "push rules are not account data here".to_owned(),
                        ))
                    } else {
                        self.target.import_account_data(&data).await
                    };
                    batch.tally(stream, &label, outcome);
                    batch.next = Some(account_data_key(&key));
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::E2eKeys => {
                let after = after.and_then(parse_device_key);
                let after = after.as_ref().map(|(u, d)| (u.as_str(), d.as_str()));
                for keys in source.e2e_device_keys(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!("{}'s device {}", keys.user_id, keys.device_id);
                    let outcome = self.target.import_device_keys(&keys).await;
                    if outcome.is_ok() {
                        batch.count("devices with identity keys", u64::from(keys.keys.is_some()));
                        batch.count("one-time keys", keys.one_time_keys.len() as u64);
                        batch.count("fallback keys", keys.fallback_keys.len() as u64);
                    }
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(device_pair_key(&keys.user_id, &keys.device_id));
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::CrossSigning => {
                for keys in source.cross_signing(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!("{}'s cross-signing keys", keys.user_id);
                    let outcome = self.target.import_cross_signing(&keys).await;
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(keys.user_id.clone());
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::KeyBackups => {
                let after = after.and_then(parse_pair_key);
                let after = after.as_ref().map(|(u, v)| (u.as_str(), *v));
                for version in source.backup_versions(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!("{}'s key backup {}", version.user_id, version.version);
                    let mut outcome = self.target.import_backup_version(&version).await;
                    if !version.deleted
                        && matches!(
                            outcome,
                            Ok(Imported::Created | Imported::Updated | Imported::AlreadyThere)
                        )
                    {
                        let mut after_key: Option<(String, String)> = None;
                        loop {
                            let keys = source
                                .backup_keys(
                                    &version.user_id,
                                    version.version,
                                    after_key.as_ref().map(|(r, s)| (r.as_str(), s.as_str())),
                                    limit,
                                )
                                .await?;
                            let Some(last) = keys.last() else { break };
                            after_key = Some((last.room_id.clone(), last.session_id.clone()));
                            match self
                                .target
                                .import_backup_keys(&version.user_id, version.version, &keys)
                                .await
                            {
                                Ok(stored) => {
                                    batch.count("room keys read", keys.len() as u64);
                                    batch.count("room keys stored", stored);
                                    if stored > 0 && outcome == Ok(Imported::AlreadyThere) {
                                        outcome = Ok(Imported::Updated);
                                    }
                                }
                                Err(e) => {
                                    outcome = Err(e);
                                    break;
                                }
                            }
                        }
                    }
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(pair_key(
                        &version.user_id,
                        i64::try_from(version.version).unwrap_or(i64::MAX),
                    ));
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::PushRules => {
                for rules in source.push_rules(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    for why in &rules.unreadable {
                        batch.log.push(entry(
                            stream.as_str(),
                            LogLevel::Warning,
                            format!("{}: a push rule was not copied: {why}", rules.user_id),
                        ));
                    }
                    let key = format!("{}'s push rules", rules.user_id);
                    let outcome = self.target.import_push_rules(&rules).await;
                    if outcome.is_ok() {
                        batch.count("rules of their own", rules.custom.len() as u64);
                        batch.count(
                            "server-default rules changed",
                            (rules.default_actions.len() + rules.enabled.len()) as u64,
                        );
                    }
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(rules.user_id.clone());
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::Pushers => {
                let after = after.and_then(|a| a.parse::<i64>().ok());
                for pusher in source.pushers(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!(
                        "{}'s pusher for {} ({})",
                        pusher.user_id, pusher.app_id, pusher.device_display_name
                    );
                    let outcome = self.target.import_pusher(&pusher).await;
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(pusher.id.to_string());
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::Filters => {
                let after = after.and_then(parse_pair_key);
                let after = after.as_ref().map(|(u, i)| (u.as_str(), *i));
                let server_name = self.target.server_name().to_owned();
                for (filter, (localpart, id)) in source.filters(after, limit, &server_name).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!("{}'s filter {}", filter.user_id, filter.filter_id);
                    let outcome = self.target.import_filter(&filter).await;
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(pair_key(&localpart, id));
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::Receipts => {
                let after = after.and_then(|a| a.parse::<i64>().ok());
                for receipt in source.receipts(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let key = format!(
                        "{}'s {} receipt in {}",
                        receipt.user_id, receipt.receipt_type, receipt.room_id
                    );
                    let outcome = match receipt_not_copied(&receipt) {
                        Some(why) => Ok(Imported::Skipped(why)),
                        None => self.target.import_receipt(&receipt).await,
                    };
                    batch.tally(stream, &key, outcome);
                    batch.next = Some(receipt.stream_id.to_string());
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
            Stream::Rooms => {
                for room_id in source.room_ids(after, limit.min(ROOMS_PER_BATCH)).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    let Some(room) = source.room(&room_id).await? else {
                        batch.next = Some(room_id);
                        continue;
                    };
                    let shape = source.room_shape(&room_id).await?;
                    // A room made on another server, which this server's users joined over
                    // federation: held here from the join, as the join made it held in Synapse,
                    // then its history after the join is copied as any room's is.
                    let mut since = None;
                    let mut before_join = 0;
                    if !shape.has_create {
                        let join = match source.remote_join(&room_id).await? {
                            Ok(join) => join,
                            Err(why) => {
                                batch.tally(stream, &room_id, Ok(Imported::Skipped(why)));
                                batch.next = Some(room_id);
                                continue;
                            }
                        };
                        match self.target.import_remote_join(&room, &join).await {
                            Ok(Imported::Skipped(why)) => {
                                batch.tally(stream, &room_id, Ok(Imported::Skipped(why)));
                                batch.next = Some(room_id);
                                continue;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                batch.tally(stream, &room_id, Err(e));
                                batch.next = Some(room_id);
                                if batch.fatal.is_some() {
                                    break;
                                }
                                continue;
                            }
                        }
                        let after_join = source.history_since(&room_id, join.join_key.1).await?;
                        before_join = shape.history.saturating_sub(after_join + 1);
                        batch.log.push(entry(
                            stream.as_str(),
                            LogLevel::Info,
                            format!(
                                "{room_id}: joined over federation by {}; held from that join with \
                                 the {} events of its state then and {} more of their auth chain",
                                join.join.json["state_key"].as_str().unwrap_or_default(),
                                join.state.len(),
                                join.auth_chain.len()
                            ),
                        ));
                        since = Some(join.join_key.1);
                    }
                    let pages = SynapseRoomPages {
                        source,
                        room_id: &room_id,
                        since,
                    };
                    let cancelled = || ctx.is_cancelled();
                    let copy =
                        match copy_room(&pages, self.target.as_ref(), &room, limit, &cancelled)
                            .await
                        {
                            Ok(copy) => copy,
                            Err(RoomFailure::Source(e)) => return Err(e),
                            Err(RoomFailure::Target(e)) => {
                                batch.tally(stream, &room_id, Err(e));
                                batch.next = Some(room_id);
                                if batch.fatal.is_some() {
                                    break;
                                }
                                continue;
                            }
                        };
                    if copy.stopped {
                        // Not finished: the room is copied again when the copy resumes.
                        break;
                    }
                    let outcome = &copy.outcome;
                    let mut message = format!(
                        "{room_id}: {} events stored, {} already here",
                        outcome.stored, outcome.already_there
                    );
                    if outcome.redactions > 0 {
                        message.push_str(&format!(", {} redactions applied", outcome.redactions));
                    }
                    if outcome.aliases > 0 {
                        message.push_str(&format!(", {} aliases", outcome.aliases));
                    }
                    let left_out = shape.outliers + shape.rejected;
                    if left_out > 0 {
                        message.push_str(&format!(
                            ", {left_out} left out (rejected by Synapse, or outliers)"
                        ));
                    }
                    if before_join > 0 {
                        message.push_str(&format!(
                            ", {before_join} from before this server's users joined left to \
                             backfill (Synapse fetched them from other servers, and so will this \
                             one when someone reads back)"
                        ));
                    }
                    if !outcome.refused.is_empty() {
                        message.push_str(&format!(
                            ", {} refused by this server's authorization",
                            outcome.refused.len()
                        ));
                    }
                    let level = if outcome.refused.is_empty() {
                        LogLevel::Info
                    } else {
                        LogLevel::Warning
                    };
                    batch.log.push(entry(stream.as_str(), level, message));
                    for (event_id, why) in outcome.refused.iter().take(REFUSALS_LOGGED_PER_ROOM) {
                        batch.log.push(entry(
                            stream.as_str(),
                            LogLevel::Warning,
                            format!("{room_id}: event {event_id} was refused: {why}"),
                        ));
                    }
                    batch.log.push(entry(
                        stream.as_str(),
                        LogLevel::Info,
                        format!("throughput: {}", copy.stats.summary()),
                    ));
                    let stored_any = outcome.stored > 0;
                    batch.tally(
                        stream,
                        &room_id,
                        Ok(if stored_any {
                            Imported::Created
                        } else {
                            Imported::AlreadyThere
                        }),
                    );
                    batch.rooms.push(copy.stats);
                    batch.next = Some(room_id);
                }
            }
            Stream::Media => {
                for (media, url_cache) in source.media(after, limit).await? {
                    if ctx.is_cancelled() {
                        break;
                    }
                    batch.next = Some(media.media_id.clone());
                    let key = format!("media {}", media.media_id);
                    if url_cache {
                        batch.tally(
                            stream,
                            &key,
                            Ok(Imported::Skipped(
                                "a URL preview's cached image, fetched again when needed"
                                    .to_owned(),
                            )),
                        );
                        continue;
                    }
                    let bytes = source.media_bytes(&media.media_id).await?;
                    if bytes.is_none() && config.media_store_path.is_some() {
                        batch.log.push(entry(
                            stream.as_str(),
                            LogLevel::Warning,
                            format!(
                                "{key}: its file is not in the media store; the record is copied \
                                 without it"
                            ),
                        ));
                    }
                    let outcome = self.target.import_media(&media, bytes).await;
                    batch.tally(stream, &key, outcome);
                    if batch.fatal.is_some() {
                        break;
                    }
                }
            }
        }
        Ok(batch)
    }

    // ---------------------------------------------------------------------------------------
    // Verification.
    // ---------------------------------------------------------------------------------------

    // The task registry's contract is `Result<Value, Problem>`.
    #[allow(clippy::result_large_err)]
    async fn verify_task(self: Arc<Self>, run: u64, ctx: TaskContext) -> Result<Value, Problem> {
        let outcome = self.verify_now(&ctx).await;
        match outcome {
            Ok(report) => {
                let summary = report_summary(&report);
                let Some(record) = self
                    .update_run(run, |r| {
                        r.verification = Some(report.clone());
                        r.phase = r.resume_phase.take().unwrap_or(Phase::ReadyForCutover);
                        r.task_id = None;
                    })
                    .await
                    .map_err(|e| Problem::unavailable().with_detail(e.to_string()))?
                else {
                    return Ok(json!({"stopped": true}));
                };
                self.write_log(vec![entry(
                    "migration",
                    if report.passed {
                        LogLevel::Info
                    } else {
                        LogLevel::Warning
                    },
                    summary,
                )])
                .await;
                self.publish("migration.verified", &record);
                Ok(serde_json::to_value(to_status(&record).verification).unwrap_or(Value::Null))
            }
            Err(e) => {
                let message = e.to_string();
                let _ = self
                    .update_run(run, |r| {
                        r.phase = r.resume_phase.take().unwrap_or(Phase::ReadyForCutover);
                        r.task_id = None;
                        r.push_error(format!("verification could not finish: {message}"));
                    })
                    .await;
                self.write_log(vec![entry(
                    "migration",
                    LogLevel::Error,
                    format!("verification could not finish: {message}"),
                )])
                .await;
                Err(Problem::unavailable().with_detail(message))
            }
        }
    }

    async fn verify_now(&self, ctx: &TaskContext) -> Result<VerificationReport, MigrationError> {
        let record = self.store.load().await?;
        let pointer = record
            .source_ref
            .clone()
            .unwrap_or_else(|| DEFAULT_SOURCE_REF.to_owned());
        let config = self
            .source_config(&pointer)
            .await
            .map_err(|e| MigrationError::Config(e.to_string()))?;
        let source = self
            .open_source(&config)
            .await
            .map_err(|e| MigrationError::Source(e.to_string()))?;
        self.verify_with(&source, ctx).await
    }

    /// Verifies the copy against `source`: every stream recounted and each row looked up here,
    /// samples compared field by field, every room's current state compared.
    ///
    /// # Errors
    /// Reading Synapse or this server failed.
    pub async fn verify_with(
        &self,
        source: &SynapseSource,
        ctx: &TaskContext,
    ) -> Result<VerificationReport, MigrationError> {
        let target_err = |e: TargetError| MigrationError::Target(e.message);
        let mut streams = Vec::new();
        let steps = 8;

        // Accounts: all looked up, a sample compared.
        let mut users = StreamVerification::new(Stream::Users.as_str());
        let mut after: Option<String> = None;
        loop {
            let page = source.users(after.as_deref(), 500).await?;
            if page.is_empty() {
                break;
            }
            for user in &page {
                users.source_count += 1;
                if self
                    .target
                    .user(&user.user_id)
                    .await
                    .map_err(target_err)?
                    .is_some()
                {
                    users.target_count += 1;
                } else {
                    users.mismatch(format!("{} is missing", user.user_id));
                }
            }
            after = page.last().map(|u| u.user_id.clone());
        }
        for user_id in source.sample_keys(Stream::Users, self.sample_size).await? {
            let (Some(theirs), Some(ours)) = (
                source.user(&user_id).await?,
                self.target.user(&user_id).await.map_err(target_err)?,
            ) else {
                continue;
            };
            users.sampled += 1;
            let mut differs = Vec::new();
            if theirs.password_hash != ours.password_hash {
                differs.push("password hash");
            }
            if theirs.displayname != ours.displayname {
                differs.push("display name");
            }
            if theirs.avatar_url != ours.avatar_url {
                differs.push("avatar");
            }
            if theirs.admin != ours.admin {
                differs.push("administrator flag");
            }
            if theirs.deactivated != ours.deactivated {
                differs.push("deactivation");
            }
            if !differs.is_empty() {
                users.mismatch(format!("{user_id}: {} differ", differs.join(", ")));
            }
        }
        streams.push(users);
        ctx.progress(1, Some(steps), Some("streams"), Some("accounts checked"))
            .await;

        // Devices.
        let mut devices = StreamVerification::new(Stream::Devices.as_str());
        let mut after: Option<(String, String)> = None;
        loop {
            let page = source
                .devices(after.as_ref().map(|(u, d)| (u.as_str(), d.as_str())), 500)
                .await?;
            if page.is_empty() {
                break;
            }
            for device in &page {
                if device.hidden {
                    devices.skipped_count += 1;
                    continue;
                }
                devices.source_count += 1;
                match self
                    .target
                    .device(&device.user_id, &device.device_id)
                    .await
                    .map_err(target_err)?
                {
                    Some(name) => {
                        devices.target_count += 1;
                        devices.sampled += 1;
                        if name != device.display_name {
                            devices.mismatch(format!(
                                "{}'s device {}: display name differs",
                                device.user_id, device.device_id
                            ));
                        }
                    }
                    None => devices.mismatch(format!(
                        "{}'s device {} is missing",
                        device.user_id, device.device_id
                    )),
                }
            }
            after = page
                .last()
                .map(|d| (d.user_id.clone(), d.device_id.clone()));
        }
        streams.push(devices);
        ctx.progress(2, Some(steps), Some("streams"), Some("devices checked"))
            .await;

        // Access tokens: each must sign in the same account and device here.
        let mut tokens = StreamVerification::new(Stream::AccessTokens.as_str());
        let mut after: Option<i64> = None;
        loop {
            let page = source.access_tokens(after, 500).await?;
            if page.is_empty() {
                break;
            }
            for token in &page {
                if token.puppets_user_id.is_some() {
                    tokens.skipped_count += 1;
                    continue;
                }
                tokens.source_count += 1;
                tokens.sampled += 1;
                match self
                    .target
                    .access_token(&token.token)
                    .await
                    .map_err(target_err)?
                {
                    Some((user, device)) if user == token.user_id && device == token.device_id => {
                        tokens.target_count += 1;
                    }
                    Some(_) => tokens.mismatch(format!(
                        "access token {} signs in someone else here",
                        token.id
                    )),
                    None => tokens.mismatch(format!(
                        "access token {} of {} is missing",
                        token.id, token.user_id
                    )),
                }
            }
            after = page.last().map(|t| t.id);
        }
        streams.push(tokens);
        ctx.progress(
            3,
            Some(steps),
            Some("streams"),
            Some("access tokens checked"),
        )
        .await;

        // Account data: each must be here with the same content.
        let mut account = StreamVerification::new(Stream::AccountData.as_str());
        let mut after: Option<[String; 4]> = None;
        loop {
            let page = source.account_data(after.as_ref(), 500).await?;
            if page.is_empty() {
                break;
            }
            for (data, _) in &page {
                if data.data_type == "m.push_rules" {
                    account.skipped_count += 1;
                    continue;
                }
                account.source_count += 1;
                account.sampled += 1;
                match self
                    .target
                    .account_data(&data.user_id, data.room_id.as_deref(), &data.data_type)
                    .await
                    .map_err(target_err)?
                {
                    Some(content) if content == data.content => account.target_count += 1,
                    Some(_) => account.mismatch(format!(
                        "{}'s {}: content differs",
                        data.user_id, data.data_type
                    )),
                    None => account
                        .mismatch(format!("{}'s {} is missing", data.user_id, data.data_type)),
                }
            }
            after = page.last().map(|(_, key)| key.clone());
        }
        streams.push(account);
        ctx.progress(
            4,
            Some(steps),
            Some("streams"),
            Some("account data checked"),
        )
        .await;

        // End-to-end keys of each device.
        let mut device_keys = StreamVerification::new(Stream::E2eKeys.as_str());
        let mut after: Option<(String, String)> = None;
        loop {
            let page = source
                .e2e_device_keys(after.as_ref().map(|(u, d)| (u.as_str(), d.as_str())), 500)
                .await?;
            let Some(last) = page.last() else { break };
            after = Some((last.user_id.clone(), last.device_id.clone()));
            for keys in &page {
                let check = self
                    .target
                    .verify_device_keys(keys)
                    .await
                    .map_err(target_err)?;
                device_keys.checked(
                    &format!("{}'s device {} keys", keys.user_id, keys.device_id),
                    check,
                );
            }
        }
        streams.push(device_keys);

        // Cross-signing keys of each account.
        let mut cross = StreamVerification::new(Stream::CrossSigning.as_str());
        let mut after: Option<String> = None;
        loop {
            let page = source.cross_signing(after.as_deref(), 500).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.user_id.clone());
            for keys in &page {
                let check = self
                    .target
                    .verify_cross_signing(keys)
                    .await
                    .map_err(target_err)?;
                cross.checked(&format!("{}'s cross-signing keys", keys.user_id), check);
            }
        }
        streams.push(cross);

        // Key backups: each version, and how many room keys it holds.
        let mut backups = StreamVerification::new(Stream::KeyBackups.as_str());
        let mut after: Option<(String, i64)> = None;
        loop {
            let page = source
                .backup_versions(after.as_ref().map(|(u, v)| (u.as_str(), *v)), 500)
                .await?;
            let Some(last) = page.last() else { break };
            after = Some((
                last.user_id.clone(),
                i64::try_from(last.version).unwrap_or(i64::MAX),
            ));
            for version in &page {
                let count = if version.deleted {
                    0
                } else {
                    source
                        .backup_key_count(&version.user_id, version.version)
                        .await?
                };
                let check = self
                    .target
                    .verify_backup_version(version, count)
                    .await
                    .map_err(target_err)?;
                backups.checked(
                    &format!("{}'s key backup {}", version.user_id, version.version),
                    check,
                );
            }
        }
        streams.push(backups);
        ctx.progress(
            5,
            Some(steps),
            Some("streams"),
            Some("end-to-end keys checked"),
        )
        .await;

        // Push rules, pushers and filters.
        let mut rules = StreamVerification::new(Stream::PushRules.as_str());
        let mut after: Option<String> = None;
        loop {
            let page = source.push_rules(after.as_deref(), 500).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.user_id.clone());
            for user_rules in &page {
                let check = self
                    .target
                    .verify_push_rules(user_rules)
                    .await
                    .map_err(target_err)?;
                rules.checked(&format!("{}'s push rules", user_rules.user_id), check);
            }
        }
        streams.push(rules);
        let mut pushers = StreamVerification::new(Stream::Pushers.as_str());
        let mut after: Option<i64> = None;
        loop {
            let page = source.pushers(after, 500).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for pusher in &page {
                let check = self
                    .target
                    .verify_pusher(pusher)
                    .await
                    .map_err(target_err)?;
                pushers.checked(
                    &format!("{}'s pusher {}", pusher.user_id, pusher.pushkey),
                    check,
                );
            }
        }
        streams.push(pushers);
        let mut filters = StreamVerification::new(Stream::Filters.as_str());
        let mut after: Option<(String, i64)> = None;
        let server_name = self.target.server_name().to_owned();
        loop {
            let page = source
                .filters(
                    after.as_ref().map(|(u, i)| (u.as_str(), *i)),
                    500,
                    &server_name,
                )
                .await?;
            let Some((_, last)) = page.last() else { break };
            after = Some(last.clone());
            for (filter, _) in &page {
                let check = self
                    .target
                    .verify_filter(filter)
                    .await
                    .map_err(target_err)?;
                filters.checked(
                    &format!("{}'s filter {}", filter.user_id, filter.filter_id),
                    check,
                );
            }
        }
        streams.push(filters);
        ctx.progress(
            6,
            Some(steps),
            Some("streams"),
            Some("push rules and filters checked"),
        )
        .await;

        // Rooms, their events, and each room's current state. A room's events are compared a
        // page at a time, as they were copied.
        let mut rooms = StreamVerification::new(Stream::Rooms.as_str());
        let mut events = StreamVerification::new("events");
        let mut rooms_not_copied = std::collections::HashSet::new();
        let mut after: Option<String> = None;
        loop {
            let ids = source.room_ids(after.as_deref(), 100).await?;
            if ids.is_empty() {
                break;
            }
            for room_id in &ids {
                let shape = source.room_shape(room_id).await?;
                // A room joined over federation is held from its join: its history after it is
                // compared, and what came before is left to backfill, as the copy left it.
                let mut since = None;
                let mut history = shape.history;
                let mut join_id = None;
                if !shape.has_create {
                    match source.remote_join(room_id).await? {
                        Ok(join) => {
                            let after = source.history_since(room_id, join.join_key.1).await?;
                            events.skipped_count += shape.history.saturating_sub(after + 1);
                            history = after + 1;
                            since = Some(join.join_key.1);
                            join_id = Some(join.join.event_id);
                        }
                        Err(_) => {
                            rooms.skipped_count += 1;
                            rooms_not_copied.insert(room_id.clone());
                            continue;
                        }
                    }
                }
                rooms.source_count += 1;
                events.source_count += history;
                events.skipped_count += shape.outliers + shape.rejected;
                let Some(state) = self.target.room_state(room_id).await.map_err(target_err)? else {
                    rooms.mismatch(format!("{room_id} is missing"));
                    continue;
                };
                rooms.target_count += 1;
                let mut missing = 0_u64;
                let mut after_event = None;
                loop {
                    let page = source
                        .room_event_ids(room_id, after_event, 1000, since)
                        .await?;
                    let Some((_, last)) = page.last() else { break };
                    after_event = Some(*last);
                    let ids: Vec<String> = page.into_iter().map(|(id, _)| id).collect();
                    missing += self
                        .target
                        .missing_events(room_id, &ids)
                        .await
                        .map_err(target_err)?
                        .len() as u64;
                }
                if let Some(join_id) = join_id {
                    missing += self
                        .target
                        .missing_events(room_id, &[join_id])
                        .await
                        .map_err(target_err)?
                        .len() as u64;
                }
                events.target_count += history.saturating_sub(missing);
                if missing > 0 {
                    events.mismatch(format!(
                        "{room_id}: {missing} of {} events are missing",
                        history
                    ));
                }
                rooms.sampled += 1;
                let theirs = source.current_state(room_id).await?;
                if theirs != state {
                    rooms.mismatch(format!(
                        "{room_id}: current state differs ({})",
                        state_difference(&theirs, &state)
                    ));
                }
            }
            after = ids.last().cloned();
        }
        streams.push(rooms);
        streams.push(events);

        // Read receipts, in the rooms.
        let mut receipts = StreamVerification::new(Stream::Receipts.as_str());
        let mut after: Option<i64> = None;
        loop {
            let page = source.receipts(after, 500).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.stream_id);
            for receipt in &page {
                if receipt_not_copied(receipt).is_some()
                    || rooms_not_copied.contains(&receipt.room_id)
                {
                    receipts.skipped_count += 1;
                    continue;
                }
                let check = self
                    .target
                    .verify_receipt(receipt)
                    .await
                    .map_err(target_err)?;
                receipts.checked(
                    &format!(
                        "{}'s {} receipt in {}",
                        receipt.user_id, receipt.receipt_type, receipt.room_id
                    ),
                    check,
                );
            }
        }
        streams.push(receipts);
        ctx.progress(7, Some(steps), Some("streams"), Some("rooms checked"))
            .await;

        // Media: each here; a sample's bytes compared.
        let mut media = StreamVerification::new(Stream::Media.as_str());
        let mut after: Option<String> = None;
        let mut bytes_checked = 0;
        loop {
            let page = source.media(after.as_deref(), 500).await?;
            if page.is_empty() {
                break;
            }
            for (item, url_cache) in &page {
                if *url_cache {
                    media.skipped_count += 1;
                    continue;
                }
                media.source_count += 1;
                let Some(here) = self
                    .target
                    .media(&item.media_id)
                    .await
                    .map_err(target_err)?
                else {
                    media.mismatch(format!("media {} is missing", item.media_id));
                    continue;
                };
                media.target_count += 1;
                if let Some(theirs) = &item.content_type
                    && theirs != &here.content_type
                {
                    media.mismatch(format!("media {}: content type differs", item.media_id));
                }
                if bytes_checked < self.sample_size
                    && let Some(theirs) = source.media_bytes(&item.media_id).await?
                {
                    bytes_checked += 1;
                    media.sampled += 1;
                    if here.bytes.as_deref() != Some(theirs.as_slice()) {
                        media.mismatch(format!("media {}: the file differs", item.media_id));
                    }
                }
            }
            after = page.last().map(|(m, _)| m.media_id.clone());
        }
        streams.push(media);
        ctx.progress(8, Some(steps), Some("streams"), Some("media checked"))
            .await;

        let passed = streams
            .iter()
            .all(|s| s.source_count == s.target_count && s.mismatches.is_empty());
        Ok(VerificationReport {
            passed,
            checked_at: now_rfc3339(),
            streams,
        })
    }

    // ---------------------------------------------------------------------------------------
    // Cutover.
    // ---------------------------------------------------------------------------------------

    // The task registry's contract is `Result<Value, Problem>`.
    #[allow(clippy::result_large_err)]
    async fn cutover_task(
        self: Arc<Self>,
        run: u64,
        ctx: TaskContext,
        actor: Actor,
    ) -> Result<Value, Problem> {
        let unavailable = |e: MigrationError| Problem::unavailable().with_detail(e.to_string());
        // The final pass: every stream read again from the start. What is already here is
        // recognized (and counted again), what changed since the bulk copy is brought over.
        let Some(_) = self
            .update_run(run, |r| {
                for s in &mut r.streams {
                    *s = StreamProgress::new(s.stream);
                }
            })
            .await
            .map_err(unavailable)?
        else {
            return Ok(json!({"stopped": true}));
        };
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            "cutover: a final pass over everything Synapse holds, then verification",
        )])
        .await;
        let back = |message: String| {
            let me = self.clone();
            async move {
                let _ = me
                    .update_run(run, |r| {
                        r.phase = Phase::ReadyForCutover;
                        r.task_id = None;
                        r.push_error(message.clone());
                    })
                    .await;
                me.write_log(vec![entry("migration", LogLevel::Error, message.clone())])
                    .await;
                message
            }
        };
        match self.copy(run, &ctx).await {
            Ok(Copied::Finished) => {}
            Ok(Copied::Superseded) => return Ok(json!({"stopped": true})),
            Err(e) => {
                let message = back(format!(
                    "the cutover's final pass stopped: {e}. Nothing was cut over"
                ))
                .await;
                return Err(Problem::unavailable().with_detail(message));
            }
        }
        let report = match self.verify_now(&ctx).await {
            Ok(report) => report,
            Err(e) => {
                let message = back(format!(
                    "the cutover's verification could not finish: {e}. Nothing was cut over"
                ))
                .await;
                return Err(Problem::unavailable().with_detail(message));
            }
        };
        if !report.passed {
            let summary = report_summary(&report);
            let _ = self
                .update_run(run, |r| r.verification = Some(report.clone()))
                .await;
            let message = back(format!(
                "the cutover's verification found differences, so nothing was cut over: {summary}"
            ))
            .await;
            return Err(Problem::conflict().with_detail(message));
        }
        let Some(record) = self
            .update_run(run, |r| {
                r.verification = Some(report.clone());
                r.phase = Phase::Completed;
                r.completed_at = Some(now_rfc3339());
                r.cutover_by = Some(actor.id.clone());
                r.task_id = None;
            })
            .await
            .map_err(unavailable)?
        else {
            return Ok(json!({"stopped": true}));
        };
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!(
                "cut over by {}: verification passed, and this server is the one in service. \
                 Keep Synapse stopped",
                actor.id
            ),
        )])
        .await;
        self.publish("migration.completed", &record);
        Ok(json!({ "status": to_status(&record) }))
    }
}

/// Why a receipt is not copied, or `None` when it is: this server keeps one `m.read` and one
/// `m.read.private` receipt per person in a room, for the room as a whole. A receipt for the
/// room's main timeline (`thread_id` `main`) is taken as one for the room; one in a thread is
/// left out.
#[must_use]
pub fn receipt_not_copied(receipt: &super::model::SynapseReceipt) -> Option<String> {
    if !matches!(receipt.receipt_type.as_str(), "m.read" | "m.read.private") {
        return Some(format!(
            "a {} receipt: this server keeps m.read and m.read.private receipts",
            receipt.receipt_type
        ));
    }
    match receipt.thread_id.as_deref() {
        None | Some("main") => None,
        Some(thread) => Some(format!(
            "a receipt in thread {thread}: this server keeps one receipt per person and type in \
             a room, for the room as a whole"
        )),
    }
}

impl StreamVerification {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            source_count: 0,
            target_count: 0,
            skipped_count: 0,
            sampled: 0,
            mismatches: Vec::new(),
        }
    }

    fn mismatch(&mut self, line: String) {
        if self.mismatches.len() < MISMATCHES_KEPT {
            self.mismatches.push(line);
        }
    }

    /// Counts one row of Synapse's that is meant to be here, and what was found.
    fn checked(&mut self, label: &str, check: Check) {
        self.source_count += 1;
        self.sampled += 1;
        match check {
            Check::Same => self.target_count += 1,
            Check::Missing => self.mismatch(format!("{label} is missing")),
            Check::Differs(what) => self.mismatch(format!("{label}: {what}")),
        }
    }
}

fn state_difference(
    theirs: &BTreeMap<(String, String), String>,
    ours: &BTreeMap<(String, String), String>,
) -> String {
    let mut parts = Vec::new();
    for (key, id) in theirs {
        match ours.get(key) {
            None => parts.push(format!("{} {:?} missing", key.0, key.1)),
            Some(other) if other != id => {
                parts.push(format!("{} {:?} is {other}, not {id}", key.0, key.1))
            }
            Some(_) => {}
        }
    }
    for key in ours.keys() {
        if !theirs.contains_key(key) {
            parts.push(format!("{} {:?} only here", key.0, key.1));
        }
    }
    parts.truncate(5);
    parts.join("; ")
}

fn report_summary(report: &VerificationReport) -> String {
    let counts: Vec<String> = report
        .streams
        .iter()
        .map(|s| format!("{} {}/{}", s.name, s.target_count, s.source_count))
        .collect();
    let mismatches: usize = report.streams.iter().map(|s| s.mismatches.len()).sum();
    if report.passed {
        format!("verification passed: {}", counts.join(", "))
    } else {
        format!(
            "verification found {mismatches} differences: {}",
            counts.join(", ")
        )
    }
}

#[async_trait]
impl MigrationSource for Migrator {
    async fn status(&self) -> Result<MigrationStatus, SourceError> {
        Ok(to_status(&self.store.load().await.map_err(store_error)?))
    }

    async fn start(
        &self,
        request: &MigrationStartRequest,
        actor: &Actor,
    ) -> Result<MigrationChange, SourceError> {
        let pointer = request
            .source_secret_ref
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_SOURCE_REF.to_owned());
        let refuse = |phase: Phase| -> Option<SourceError> {
            let why = match phase {
                Phase::Idle | Phase::Failed | Phase::Aborted => return None,
                Phase::Paused => "the copy is paused: resume it",
                Phase::Copying | Phase::Verifying | Phase::CuttingOver => "it is already running",
                Phase::ReadyForCutover => "everything has been copied: verify it, or cut over",
                Phase::Completed => "this server has already been cut over to",
            };
            Some(SourceError::Conflict(format!(
                "the migration cannot be started now: {why}"
            )))
        };
        let before = self.store.load().await.map_err(store_error)?;
        if let Some(refusal) = refuse(before.phase) {
            return Err(refusal);
        }
        let config = self.source_config(&pointer).await?;
        drop(self.open_source(&config).await?);
        let description = config.database.describe();

        let (from, run) = {
            let _guard = self.lock.lock().await;
            let mut record = self.store.load().await.map_err(store_error)?;
            if let Some(refusal) = refuse(record.phase) {
                return Err(refusal);
            }
            let from = record.phase.as_str().to_owned();
            if record.source.as_deref() != Some(description.as_str()) {
                // A different source: nothing copied from the last one is a checkpoint here.
                record.streams.clear();
                record.verification = None;
            }
            record.phase = Phase::Copying;
            record.run += 1;
            record.source = Some(description.clone());
            record.source_ref = Some(pointer.clone());
            record.errors.clear();
            record.task_id = None;
            record.completed_at = None;
            record.cutover_by = None;
            if record.started_at.is_none() {
                record.started_at = Some(now_rfc3339());
            }
            record.started_by = Some(actor.id.clone());
            record.updated_at = Some(now_rfc3339());
            self.store.save(&record).await.map_err(store_error)?;
            self.observer.observe(&record);
            (from, record.run)
        };
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!("{} started copying from {description}", actor.id),
        )])
        .await;
        let (_, record) = self.spawn_step(COPY_ACTION, run, actor.clone()).await?;
        Ok(MigrationChange {
            from,
            status: to_status(&record),
        })
    }

    async fn pause(&self, actor: &Actor) -> Result<Option<MigrationChange>, SourceError> {
        let (record, task) = {
            let _guard = self.lock.lock().await;
            let mut record = self.store.load().await.map_err(store_error)?;
            match record.phase {
                Phase::Paused => return Ok(None),
                Phase::Copying => {}
                other => {
                    return Err(SourceError::Conflict(format!(
                        "only a copy can be paused, and the migration is {}",
                        other.as_str()
                    )));
                }
            }
            record.phase = Phase::Paused;
            record.run += 1;
            let task = record.task_id.take();
            record.updated_at = Some(now_rfc3339());
            self.store.save(&record).await.map_err(store_error)?;
            self.observer.observe(&record);
            (record, task)
        };
        if let Some(task) = task {
            let _ = self.tasks.cancel(&task).await;
        }
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!("{} paused the copy", actor.id),
        )])
        .await;
        Ok(Some(MigrationChange {
            from: Phase::Copying.as_str().to_owned(),
            status: to_status(&record),
        }))
    }

    async fn resume(&self, actor: &Actor) -> Result<Option<MigrationChange>, SourceError> {
        let run = {
            let _guard = self.lock.lock().await;
            let mut record = self.store.load().await.map_err(store_error)?;
            match record.phase {
                Phase::Copying => return Ok(None),
                Phase::Paused => {}
                other => {
                    return Err(SourceError::Conflict(format!(
                        "only a paused copy can be resumed, and the migration is {}",
                        other.as_str()
                    )));
                }
            }
            record.phase = Phase::Copying;
            record.run += 1;
            record.updated_at = Some(now_rfc3339());
            self.store.save(&record).await.map_err(store_error)?;
            self.observer.observe(&record);
            record.run
        };
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!("{} resumed the copy", actor.id),
        )])
        .await;
        let (_, record) = self.spawn_step(COPY_ACTION, run, actor.clone()).await?;
        Ok(Some(MigrationChange {
            from: Phase::Paused.as_str().to_owned(),
            status: to_status(&record),
        }))
    }

    async fn abort(&self, actor: &Actor) -> Result<Option<MigrationChange>, SourceError> {
        let (from, record, task) = {
            let _guard = self.lock.lock().await;
            let mut record = self.store.load().await.map_err(store_error)?;
            match record.phase {
                Phase::Aborted => return Ok(None),
                Phase::Completed => {
                    return Err(SourceError::Conflict(
                        "this server has already been cut over to; there is nothing to abort"
                            .to_owned(),
                    ));
                }
                Phase::Idle => {
                    return Err(SourceError::Conflict(
                        "no migration has been started".to_owned(),
                    ));
                }
                _ => {}
            }
            let from = record.phase.as_str().to_owned();
            record.phase = Phase::Aborted;
            record.run += 1;
            record.resume_phase = None;
            let task = record.task_id.take();
            record.updated_at = Some(now_rfc3339());
            self.store.save(&record).await.map_err(store_error)?;
            self.observer.observe(&record);
            (from, record, task)
        };
        if let Some(task) = task {
            let _ = self.tasks.cancel(&task).await;
        }
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!(
                "{} aborted the migration. Synapse was only ever read, so it is as it was; what \
                 was copied here stays, and starting again carries on from it",
                actor.id
            ),
        )])
        .await;
        Ok(Some(MigrationChange {
            from,
            status: to_status(&record),
        }))
    }

    async fn verify(&self, actor: &Actor) -> Result<Task, SourceError> {
        let run = {
            let _guard = self.lock.lock().await;
            let mut record = self.store.load().await.map_err(store_error)?;
            match record.phase {
                Phase::ReadyForCutover | Phase::Paused | Phase::Failed | Phase::Completed => {}
                Phase::Idle | Phase::Aborted => {
                    return Err(SourceError::Conflict(
                        "there is no copy to verify: start the migration first".to_owned(),
                    ));
                }
                other => {
                    return Err(SourceError::Conflict(format!(
                        "the migration is {}: verify it once that has finished",
                        other.as_str()
                    )));
                }
            }
            record.resume_phase = Some(record.phase);
            record.phase = Phase::Verifying;
            record.run += 1;
            record.updated_at = Some(now_rfc3339());
            self.store.save(&record).await.map_err(store_error)?;
            self.observer.observe(&record);
            record.run
        };
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!("{} started a verification", actor.id),
        )])
        .await;
        let (task, _) = self.spawn_step(VERIFY_ACTION, run, actor.clone()).await?;
        Ok(task)
    }

    async fn cutover(&self, actor: &Actor) -> Result<Task, SourceError> {
        let run = {
            let _guard = self.lock.lock().await;
            let mut record = self.store.load().await.map_err(store_error)?;
            if record.phase != Phase::ReadyForCutover {
                return Err(SourceError::Conflict(format!(
                    "the migration can only be cut over once everything has been copied, and it \
                     is {}",
                    record.phase.as_str()
                )));
            }
            record.phase = Phase::CuttingOver;
            record.run += 1;
            record.updated_at = Some(now_rfc3339());
            self.store.save(&record).await.map_err(store_error)?;
            self.observer.observe(&record);
            record.run
        };
        self.write_log(vec![entry(
            "migration",
            LogLevel::Info,
            format!("{} started the cutover", actor.id),
        )])
        .await;
        let (task, _) = self.spawn_step(CUTOVER_ACTION, run, actor.clone()).await?;
        Ok(task)
    }

    async fn log(&self) -> Result<Vec<MigrationLogEntry>, SourceError> {
        Ok(self
            .store
            .log()
            .await
            .map_err(store_error)?
            .into_iter()
            .map(|e| MigrationLogEntry {
                recorded_at: e.recorded_at,
                stream: e.stream,
                message: e.message,
                level: match e.level {
                    LogLevel::Info => "info",
                    LogLevel::Warning => "warning",
                    LogLevel::Error => "error",
                }
                .to_owned(),
            })
            .collect())
    }
}
