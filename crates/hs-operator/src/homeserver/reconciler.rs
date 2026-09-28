//! The `Homeserver` reconcile step: applies [`super::objects::build`]'s objects, and decides the
//! StatefulSet's replica count and update partition so that no pod is removed or replaced
//! before its replica has handed off its shards.
//!
//! # How a pod goes away
//!
//! The StatefulSet runs `RollingUpdate` with a `partition`, and the operator owns both
//! `replicas` and `partition`. In steady state the partition equals the replica count, so a new
//! pod template replaces nothing by itself. For every pod that must go, highest ordinal first:
//!
//! 1. The operator asks the admin API to drain its replica (`POST
//!    /api/v1/cluster/replicas/{id}/drain`, decision 0012) and records the drain in
//!    `status.drain`.
//! 2. On each reconcile (every five seconds while draining) it reads the replica back until it
//!    owns no shards, reporting what is left in the `Draining` condition, alongside the drain
//!    task's status.
//! 3. Then it lets the pod go: for a scale-down it lowers `replicas` by one; for a rolling
//!    update it lowers `partition` to that ordinal, and the StatefulSet controller replaces the
//!    pod.
//! 4. It undrains the replica: a scale-down's once the pod is gone (so a later scale-up does
//!    not bring that ordinal back drained), a rolling update's once the replacement (same name,
//!    same replica id, so it inherits the drain request) runs the new template and is ready.
//!    The next pod is not drained before that undrain, so two replicas are never drained at
//!    once by the operator.
//!
//! A drain that outlives `spec.drain.timeoutSeconds` either lets the pod go anyway
//! (`Proceed`, the default; the pod's own `SIGTERM` handoff releases the rest) or waits
//! (`Hold`); both raise a Warning event. A drain the spec no longer needs (replicas raised back
//! during a scale-down, a template reverted during a rolling update) is aborted: the replica is
//! undrained and keeps its pod. A drain the server refuses (`409`, no other active replica) is
//! retried and reported. Without `spec.adminApi`, or with one replica, there is nothing to
//! drain through and pods go as the StatefulSet controller would remove them.
//!
//! [`HomeserverKube`] and [`AdminApi`] are the seams; `super::tests` runs this step against an
//! in-memory cluster that plays the StatefulSet controller and the server.

use std::future::Future;
use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use k8s_openapi::chrono::{DateTime, Utc};
use kube::ResourceExt as _;

use super::admin::{AdminApi, AdminEndpoint, AdminError, find_replica};
use super::objects::{
    ANNOTATION_TEMPLATE_HASH, BuildError, DesiredObjects, build, desired_replicas, is_cluster,
    pod_name, pod_ordinal,
};
use crate::crds::{
    DrainReason, DrainStatus, DrainTimeoutPolicy, Homeserver, HomeserverStatus, PendingUndrain,
    Phase, SecretKeyRef, UndrainWhen,
};
use crate::metrics::OperatorMetrics;

/// The finalizer the operator puts on a `Homeserver` it may drain replicas of, so that deleting
/// the resource mid-drain undrains them first (a drain request lives in the database and would
/// otherwise outlive the resource, decision 0012).
pub const FINALIZER: &str = "hs.matrix.org/undrain";
/// The pod label carrying the StatefulSet revision a pod runs.
pub const REVISION_LABEL: &str = "controller-revision-hash";

/// Condition: every desired replica runs the current template and is ready.
pub const CONDITION_READY: &str = "Ready";
/// Condition: scaling, rolling or creating.
pub const CONDITION_PROGRESSING: &str = "Progressing";
/// Condition: a drain is in flight.
pub const CONDITION_DRAINING: &str = "Draining";
/// Condition: the operator can drain replicas through the admin API.
pub const CONDITION_DRAIN_AVAILABLE: &str = "DrainAvailable";
/// Condition: the spec can be turned into a workload.
pub const CONDITION_SPEC_VALID: &str = "SpecValid";

/// How often a drain in flight is looked at.
pub const REQUEUE_DRAINING: Duration = Duration::from_secs(5);
/// How often a homeserver that is changing is looked at, besides the watches.
pub const REQUEUE_PROGRESSING: Duration = Duration::from_secs(10);
/// How often a steady homeserver is looked at, besides the watches.
pub const REQUEUE_STEADY: Duration = Duration::from_secs(300);

/// A Kubernetes event about a `Homeserver`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// `Warning` rather than `Normal`.
    pub warning: bool,
    /// `PascalCase` reason (`Draining`, `DrainTimedOut`, ...).
    pub reason: &'static str,
    /// The human-readable note.
    pub message: String,
}

impl Note {
    fn normal(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            warning: false,
            reason,
            message: message.into(),
        }
    }
    fn warning(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            warning: true,
            reason,
            message: message.into(),
        }
    }
}

/// The Kubernetes calls the reconciler makes. [`super::kube_ops::KubeHomeserverOps`] is the
/// real one.
pub trait HomeserverKube: Send + Sync {
    /// The StatefulSet, if it exists.
    fn get_stateful_set(
        &self,
        namespace: &str,
        name: &str,
    ) -> impl Future<Output = Result<Option<StatefulSet>, kube::Error>> + Send;
    /// The pods matching `selector`.
    fn list_pods(
        &self,
        namespace: &str,
        selector: &str,
    ) -> impl Future<Output = Result<Vec<Pod>, kube::Error>> + Send;
    /// One key of a Secret, if both exist.
    fn secret_value(
        &self,
        namespace: &str,
        secret: &SecretKeyRef,
    ) -> impl Future<Output = Result<Option<String>, kube::Error>> + Send;
    /// Applies every object (server-side apply), deleting the disruption budget when there
    /// should be none.
    fn apply(
        &self,
        namespace: &str,
        objects: &DesiredObjects,
    ) -> impl Future<Output = Result<(), kube::Error>> + Send;
    /// Writes the status sub-resource.
    fn patch_status(
        &self,
        namespace: &str,
        name: &str,
        status: &HomeserverStatus,
    ) -> impl Future<Output = Result<(), kube::Error>> + Send;
    /// Adds or removes [`FINALIZER`].
    fn set_finalizer(
        &self,
        hs: &Homeserver,
        present: bool,
    ) -> impl Future<Output = Result<(), kube::Error>> + Send;
    /// Publishes an event on the `Homeserver`.
    fn publish(&self, hs: &Homeserver, note: &Note) -> impl Future<Output = ()> + Send;
}

/// What a reconcile can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// A Kubernetes API call failed.
    #[error("kubernetes API: {0}")]
    Kube(#[from] kube::Error),
    /// An admin API call failed in a way worth retrying.
    #[error("admin API: {0}")]
    Admin(#[from] AdminError),
    /// The objects could not be built.
    #[error("{0}")]
    Build(#[from] BuildError),
}

impl ReconcileError {
    /// A short, bounded label for `hs_operator_reconcile_errors_total{reason}`.
    #[must_use]
    pub fn metric_reason(&self) -> &'static str {
        match self {
            Self::Kube(_) => "kube",
            Self::Admin(_) => "admin_api",
            Self::Build(_) => "build",
        }
    }
}

/// What one reconcile decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// When to look again.
    pub requeue: Option<Duration>,
    /// The status written.
    pub status: HomeserverStatus,
    /// The StatefulSet's replica count and partition applied, if it was applied.
    pub applied: Option<(i32, i32)>,
}

/// The reconciler, over its two seams.
#[derive(Debug, Clone)]
pub struct Reconciler<K, A> {
    /// Kubernetes.
    pub kube: K,
    /// The admin API.
    pub admin: A,
    /// Metrics.
    pub metrics: OperatorMetrics,
}

/// A pod, as far as the reconciler reads it.
#[derive(Debug, Clone)]
struct PodView {
    name: String,
    ordinal: i32,
    revision: Option<String>,
    ready: bool,
    terminating: bool,
}

impl PodView {
    fn from(name: &str, pod: &Pod) -> Option<Self> {
        let pod_name = pod.metadata.name.clone()?;
        let ordinal = pod_ordinal(name, &pod_name)?;
        let ready = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
        Some(Self {
            name: pod_name,
            ordinal,
            revision: pod.labels().get(REVISION_LABEL).cloned(),
            ready,
            terminating: pod.metadata.deletion_timestamp.is_some(),
        })
    }
}

/// The StatefulSet, as far as the reconciler reads it.
#[derive(Debug, Clone)]
struct StsView {
    replicas: i32,
    partition: i32,
    template_hash: Option<String>,
    settled: bool,
    update_revision: Option<String>,
    ready_replicas: i32,
    status_replicas: i32,
    updated_replicas: i32,
}

impl StsView {
    fn from(sts: &StatefulSet) -> Self {
        let spec = sts.spec.as_ref();
        let status = sts.status.as_ref();
        let generation = sts.metadata.generation.unwrap_or(0);
        Self {
            replicas: spec.and_then(|s| s.replicas).unwrap_or(1),
            partition: spec
                .and_then(|s| s.update_strategy.as_ref())
                .and_then(|u| u.rolling_update.as_ref())
                .and_then(|r| r.partition)
                .unwrap_or(0),
            template_hash: sts.annotations().get(ANNOTATION_TEMPLATE_HASH).cloned(),
            settled: status
                .and_then(|s| s.observed_generation)
                .is_some_and(|g| g >= generation),
            update_revision: status.and_then(|s| s.update_revision.clone()),
            ready_replicas: status.and_then(|s| s.ready_replicas).unwrap_or(0),
            status_replicas: status.map_or(0, |s| s.replicas),
            updated_replicas: status.and_then(|s| s.updated_replicas).unwrap_or(0),
        }
    }
}

/// How starting a drain went.
enum Started {
    /// The drain is in flight.
    Draining(DrainStatus),
    /// The pod has no replica in the registry, or it already owns nothing: nothing to wait for.
    Nothing,
    /// The server refused (409); reported as an event and retried.
    Refused,
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(k8s_openapi::chrono::SecondsFormat::Secs, true)
}

/// Sets a condition, keeping its transition time unless its status changes.
fn set_condition(
    conditions: &mut Vec<Condition>,
    type_: &str,
    status: bool,
    reason: &str,
    message: impl Into<String>,
    generation: Option<i64>,
    now: DateTime<Utc>,
) {
    let status = if status { "True" } else { "False" }.to_owned();
    let message = message.into();
    if let Some(c) = conditions.iter_mut().find(|c| c.type_ == type_) {
        if c.status != status {
            c.last_transition_time = Time(now);
        }
        c.status = status;
        c.reason = reason.to_owned();
        c.message = message;
        c.observed_generation = generation;
    } else {
        conditions.push(Condition {
            type_: type_.to_owned(),
            status,
            reason: reason.to_owned(),
            message,
            observed_generation: generation,
            last_transition_time: Time(now),
        });
    }
}

/// Whether a condition is set with this status and reason already.
fn condition_is(conditions: &[Condition], type_: &str, status: bool, reason: &str) -> bool {
    let status = if status { "True" } else { "False" };
    conditions
        .iter()
        .any(|c| c.type_ == type_ && c.status == status && c.reason == reason)
}

impl<K: HomeserverKube, A: AdminApi> Reconciler<K, A> {
    /// One reconcile of `hs` at `now`.
    ///
    /// # Errors
    /// When a Kubernetes call fails, or an admin API call fails in a way that is not a refusal
    /// (a refusal is reported and retried without an error).
    pub async fn reconcile(
        &self,
        hs: &Homeserver,
        now: DateTime<Utc>,
    ) -> Result<Outcome, ReconcileError> {
        let name = hs
            .metadata
            .name
            .clone()
            .ok_or(BuildError::Missing("name"))?;
        let namespace = hs
            .metadata
            .namespace
            .clone()
            .ok_or(BuildError::Missing("namespace"))?;
        let key = format!("{namespace}/{name}");
        let generation = hs.metadata.generation;
        let mut status = hs.status.clone().unwrap_or_default();
        let has_finalizer = hs.finalizers().iter().any(|f| f == FINALIZER);

        if hs.metadata.deletion_timestamp.is_some() {
            if has_finalizer {
                self.finalize(hs, &namespace, &mut status).await?;
            }
            self.metrics.set_draining(&key, false);
            return Ok(Outcome {
                requeue: None,
                status,
                applied: None,
            });
        }

        let objects = match build(hs) {
            Ok(objects) => objects,
            Err(BuildError::Invalid(message)) => {
                if !condition_is(
                    &status.conditions,
                    CONDITION_SPEC_VALID,
                    false,
                    "InvalidSpec",
                ) {
                    self.kube
                        .publish(hs, &Note::warning("InvalidSpec", message.clone()))
                        .await;
                }
                tracing::warn!(homeserver = %key, %message, "the Homeserver's spec is invalid");
                set_condition(
                    &mut status.conditions,
                    CONDITION_SPEC_VALID,
                    false,
                    "InvalidSpec",
                    message,
                    generation,
                    now,
                );
                status.phase = Phase::Degraded;
                status.observed_generation = generation;
                self.write_status(hs, &namespace, &name, &status).await?;
                return Ok(Outcome {
                    requeue: Some(REQUEUE_STEADY),
                    status,
                    applied: None,
                });
            }
            Err(e) => return Err(e.into()),
        };
        set_condition(
            &mut status.conditions,
            CONDITION_SPEC_VALID,
            true,
            "Valid",
            "",
            generation,
            now,
        );

        let desired = desired_replicas(&hs.spec);
        let cluster = is_cluster(&hs.spec);
        let endpoint = self
            .endpoint(hs, &namespace, &name, &mut status, now)
            .await?;
        if cluster && endpoint.is_some() && !has_finalizer {
            self.kube.set_finalizer(hs, true).await?;
        }
        let mut notes: Vec<Note> = Vec::new();

        let Some(sts) = self.kube.get_stateful_set(&namespace, &name).await? else {
            // First apply: nothing runs yet, so there is nothing to drain or roll.
            let mut objects = objects;
            set_scale(&mut objects, desired, 0);
            self.kube.apply(&namespace, &objects).await?;
            notes.push(Note::normal(
                "Created",
                format!("created the StatefulSet with {desired} replica(s)"),
            ));
            set_condition(
                &mut status.conditions,
                CONDITION_PROGRESSING,
                true,
                "Creating",
                "creating the StatefulSet",
                generation,
                now,
            );
            set_condition(
                &mut status.conditions,
                CONDITION_READY,
                false,
                "ReplicasNotReady",
                format!("0 of {desired} ready"),
                generation,
                now,
            );
            set_condition(
                &mut status.conditions,
                CONDITION_DRAINING,
                false,
                "NoDrain",
                "",
                generation,
                now,
            );
            status.phase = Phase::Pending;
            status.observed_generation = generation;
            status.replicas = Some(0);
            status.ready_replicas = Some(0);
            status.updated_replicas = Some(0);
            self.finish(hs, &namespace, &name, &status, &notes).await?;
            return Ok(Outcome {
                requeue: Some(REQUEUE_PROGRESSING),
                status,
                applied: Some((desired, 0)),
            });
        };

        let obs = StsView::from(&sts);
        let pods: Vec<PodView> = self
            .kube
            .list_pods(&namespace, &super::objects::pod_selector(&name))
            .await?
            .iter()
            .filter_map(|p| PodView::from(&name, p))
            .collect();
        let pod = |pod_name: &str| pods.iter().find(|p| p.name == pod_name);
        let current = obs.replicas;
        let template_changed = obs.template_hash.as_deref() != Some(objects.template_hash.as_str());
        let update_revision = (obs.settled && !template_changed)
            .then(|| obs.update_revision.clone())
            .flatten();
        let is_updated = |p: &PodView| {
            update_revision
                .as_ref()
                .is_some_and(|u| p.revision.as_ref() == Some(u))
        };
        let mut replicas_next = current;
        let mut partition_next = obs.partition;
        let mut acted = false;
        let mut progress: Option<(&'static str, String)> = None;

        // 1. Undrain what the operator drained, once its pod is gone or replaced and ready.
        if let Some(ep) = &endpoint {
            let mut kept = Vec::new();
            for p in std::mem::take(&mut status.pending_undrains) {
                let due = match p.when {
                    UndrainWhen::AfterRemoval => pod(&p.pod).is_none(),
                    UndrainWhen::WhenReady => {
                        pod(&p.pod).is_some_and(|v| v.ready && !v.terminating && is_updated(v))
                    }
                };
                if !due {
                    kept.push(p);
                    continue;
                }
                match self.admin.undrain(ep, &p.replica_id).await {
                    Ok(_) | Err(AdminError::NotFound) => {
                        self.metrics.drain_event("undrained");
                        tracing::info!(homeserver = %key, pod = %p.pod, replica = %p.replica_id, "undrained");
                        notes.push(Note::normal(
                            "Undrained",
                            format!("undrained {} ({})", p.pod, p.replica_id),
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(homeserver = %key, pod = %p.pod, error = %e, "undrain failed, will retry");
                        kept.push(p);
                    }
                }
            }
            status.pending_undrains = kept;
        } else if !status.pending_undrains.is_empty() {
            notes.push(Note::warning(
                "UndrainSkipped",
                "the admin API is no longer configured: replicas the operator drained stay \
                 drained until an administrator undrains them",
            ));
            status.pending_undrains.clear();
        }

        // 2. The drain in flight, if any.
        if let Some(mut drain) = status.drain.take() {
            let ordinal = pod_ordinal(&name, &drain.pod).unwrap_or(i32::MAX);
            let needed = match drain.reason {
                DrainReason::ScaleDown => ordinal >= desired && ordinal < current,
                DrainReason::RollingUpdate => {
                    ordinal < current
                        && ordinal < desired
                        && pod(&drain.pod).is_some_and(|p| template_changed || !is_updated(p))
                }
            };
            match &endpoint {
                None => {
                    notes.push(Note::warning(
                        "DrainAbandoned",
                        format!(
                            "stopped following the drain of {}: the admin API is no longer \
                             configured",
                            drain.pod
                        ),
                    ));
                }
                Some(ep) if !needed => {
                    match self.admin.undrain(ep, &drain.replica_id).await {
                        Ok(_) | Err(AdminError::NotFound) => {}
                        Err(e) => {
                            status.drain = Some(drain);
                            return Err(e.into());
                        }
                    }
                    self.metrics.drain_event("aborted");
                    tracing::info!(homeserver = %key, pod = %drain.pod, "drain aborted: no longer needed");
                    notes.push(Note::normal(
                        "DrainAborted",
                        format!(
                            "{} no longer has to go ({:?} is no longer needed); undrained it",
                            drain.pod, drain.reason
                        ),
                    ));
                    acted = true;
                }
                Some(_) if template_changed => {
                    // A new template is being applied this step; decide on the next one, when
                    // the StatefulSet says which revision the pod must reach.
                    status.drain = Some(drain);
                }
                Some(ep) => {
                    let replica = find_replica(&self.admin, ep, &drain.pod).await?;
                    if let Some(r) = &replica
                        && r.drain_requested_at.is_none()
                        && !r.owns_nothing()
                    {
                        // Someone undrained it by hand while the operator still needs it gone.
                        self.admin.drain(ep, &r.id).await?;
                        notes.push(Note::warning(
                            "DrainReissued",
                            format!(
                                "{} was undrained while it still has to go; drained it again",
                                drain.pod
                            ),
                        ));
                    }
                    let remaining = replica.as_ref().map_or(0, |r| r.shard_count);
                    let started = DateTime::parse_from_rfc3339(&drain.started_at)
                        .map_or(now, |t| t.with_timezone(&Utc));
                    let elapsed = (now - started).to_std().unwrap_or_default();
                    let timeout = Duration::from_secs(hs.spec.drain.timeout_seconds);
                    if let Some(id) = &drain.task_id {
                        // Informational: the replica's shard count is what decides.
                        drain.task_status = self.admin.task(ep, id).await.ok().map(|t| t.status);
                    }
                    if remaining == 0 {
                        self.metrics.drain_event("completed");
                        tracing::info!(homeserver = %key, pod = %drain.pod, elapsed_secs = elapsed.as_secs(), "replica drained");
                        notes.push(Note::normal(
                            "Drained",
                            format!(
                                "{} owns no shards after {}s; letting its pod go",
                                drain.pod,
                                elapsed.as_secs()
                            ),
                        ));
                        let (r, p) = complete(&drain, ordinal, &mut status.pending_undrains);
                        replicas_next = r.unwrap_or(replicas_next);
                        partition_next = p.unwrap_or(partition_next);
                        acted = true;
                    } else if elapsed > timeout {
                        match hs.spec.drain.on_timeout {
                            DrainTimeoutPolicy::Proceed => {
                                self.metrics.drain_event("timed_out");
                                tracing::warn!(homeserver = %key, pod = %drain.pod, remaining, "drain timed out, letting the pod go");
                                notes.push(Note::warning(
                                    "DrainTimedOut",
                                    format!(
                                        "{} still owns {remaining} shard(s) after {}s; letting \
                                         its pod go (its shutdown hands off the rest)",
                                        drain.pod,
                                        elapsed.as_secs()
                                    ),
                                ));
                                let (r, p) =
                                    complete(&drain, ordinal, &mut status.pending_undrains);
                                replicas_next = r.unwrap_or(replicas_next);
                                partition_next = p.unwrap_or(partition_next);
                                acted = true;
                            }
                            DrainTimeoutPolicy::Hold => {
                                if !drain.timed_out {
                                    self.metrics.drain_event("timed_out");
                                    tracing::warn!(homeserver = %key, pod = %drain.pod, remaining, "drain timed out, holding");
                                    notes.push(Note::warning(
                                        "DrainTimedOut",
                                        format!(
                                            "{} still owns {remaining} shard(s) after {}s; \
                                             holding its pod (drain.onTimeout: Hold)",
                                            drain.pod,
                                            elapsed.as_secs()
                                        ),
                                    ));
                                }
                                drain.timed_out = true;
                                drain.shards_remaining = Some(remaining);
                                status.drain = Some(drain);
                            }
                        }
                    } else {
                        drain.shards_remaining = Some(remaining);
                        tracing::debug!(homeserver = %key, pod = %drain.pod, remaining, task = ?drain.task_status, "drain in progress");
                        status.drain = Some(drain);
                    }
                }
            }
        }

        // 3. Nothing in flight: take the next step towards the spec.
        let waiting_for_replacement = status
            .pending_undrains
            .iter()
            .any(|p| p.when == UndrainWhen::WhenReady);
        let drain_through = endpoint.as_ref().filter(|_| cluster);
        if status.drain.is_none() && !acted {
            if desired < current {
                let ordinal = current - 1;
                let target = pod_name(&name, ordinal);
                progress = Some(("ScalingDown", format!("{current} -> {desired} replicas")));
                match drain_through.filter(|_| desired >= 1) {
                    Some(ep) => match self
                        .start_drain(hs, ep, &target, DrainReason::ScaleDown, now, &mut notes)
                        .await?
                    {
                        Started::Draining(d) => status.drain = Some(d),
                        Started::Nothing => replicas_next = ordinal,
                        Started::Refused => {}
                    },
                    None => {
                        if cluster && desired >= 1 {
                            notes.push(Note::warning(
                                "ScaledDownWithoutDrain",
                                format!(
                                    "scaling {current} -> {desired} without draining first: \
                                     spec.adminApi is not set, so each pod's own shutdown \
                                     handoff is all there is"
                                ),
                            ));
                        }
                        replicas_next = desired;
                    }
                }
            } else if desired > current {
                progress = Some(("ScalingUp", format!("{current} -> {desired} replicas")));
                notes.push(Note::normal(
                    "ScalingUp",
                    format!("scaling {current} -> {desired} replicas"),
                ));
                replicas_next = desired;
            } else if let Some(update_revision) = &update_revision {
                let stale: Vec<&PodView> = pods
                    .iter()
                    .filter(|p| p.ordinal < current && p.revision.as_ref() != Some(update_revision))
                    .collect();
                if let Some(top) = stale.iter().max_by_key(|p| p.ordinal) {
                    let k = top.ordinal;
                    progress = Some((
                        "RollingUpdate",
                        format!(
                            "{} of {current} pods run the new template",
                            current - i32::try_from(stale.len()).unwrap_or(0)
                        ),
                    ));
                    let above_ready = (k + 1..current).all(|o| {
                        pod(&pod_name(&name, o)).is_some_and(|p| p.ready && is_updated(p))
                    });
                    if above_ready && !waiting_for_replacement {
                        match drain_through.filter(|_| current > 1) {
                            Some(ep) => match self
                                .start_drain(
                                    hs,
                                    ep,
                                    &top.name,
                                    DrainReason::RollingUpdate,
                                    now,
                                    &mut notes,
                                )
                                .await?
                            {
                                Started::Draining(d) => status.drain = Some(d),
                                Started::Nothing => partition_next = k,
                                Started::Refused => {}
                            },
                            None => partition_next = k,
                        }
                    }
                } else {
                    // Every pod runs the current template: hold the partition at the top, so the
                    // next template change replaces nothing until the operator drains first.
                    partition_next = current;
                }
            }
        }
        if template_changed {
            // A new pod template is applied now; nothing may be replaced by the StatefulSet
            // controller on its own.
            partition_next = replicas_next;
        }
        partition_next = partition_next.clamp(0, replicas_next.max(0));

        let mut objects = objects;
        set_scale(&mut objects, replicas_next, partition_next);
        self.kube.apply(&namespace, &objects).await?;
        if replicas_next < current {
            tracing::info!(homeserver = %key, from = current, to = replicas_next, "scaled down");
        }
        if partition_next < obs.partition && !template_changed {
            tracing::info!(homeserver = %key, partition = partition_next, "letting pod {} be replaced", pod_name(&name, partition_next));
        }

        // 4. Status.
        if let Some(d) = &status.drain {
            let mut remaining = d
                .shards_remaining
                .map_or_else(|| "shards".to_owned(), |n| format!("{n} shard(s)"));
            if let (Some(id), Some(task)) = (&d.task_id, &d.task_status) {
                remaining.push_str(&format!(" (task {id} {task})"));
            }
            let (reason, message) = if d.timed_out {
                (
                    "DrainTimedOut",
                    format!(
                        "{} ({:?}) still owns {remaining} after the timeout; holding",
                        d.pod, d.reason
                    ),
                )
            } else {
                (
                    "DrainInProgress",
                    format!(
                        "{} ({:?}): waiting for it to hand off {remaining}",
                        d.pod, d.reason
                    ),
                )
            };
            set_condition(
                &mut status.conditions,
                CONDITION_DRAINING,
                true,
                reason,
                message,
                generation,
                now,
            );
        } else {
            set_condition(
                &mut status.conditions,
                CONDITION_DRAINING,
                false,
                "NoDrain",
                "",
                generation,
                now,
            );
        }
        self.metrics.set_draining(&key, status.drain.is_some());

        let all_updated =
            !template_changed && obs.updated_replicas >= current && obs.status_replicas == current;
        let steady = desired == current
            && replicas_next == current
            && status.drain.is_none()
            && status.pending_undrains.is_empty()
            && all_updated;
        match (&progress, steady) {
            (_, true) => set_condition(
                &mut status.conditions,
                CONDITION_PROGRESSING,
                false,
                "Stable",
                "",
                generation,
                now,
            ),
            (Some((reason, message)), false) => set_condition(
                &mut status.conditions,
                CONDITION_PROGRESSING,
                true,
                reason,
                message.clone(),
                generation,
                now,
            ),
            (None, false) => set_condition(
                &mut status.conditions,
                CONDITION_PROGRESSING,
                true,
                if template_changed || !all_updated {
                    "RollingUpdate"
                } else {
                    "Converging"
                },
                format!(
                    "{} of {current} pods run the current template",
                    obs.updated_replicas
                ),
                generation,
                now,
            ),
        }
        let ready = steady && obs.ready_replicas >= desired;
        set_condition(
            &mut status.conditions,
            CONDITION_READY,
            ready,
            if ready {
                "AllReplicasReady"
            } else {
                "ReplicasNotReady"
            },
            format!("{} of {desired} ready", obs.ready_replicas),
            generation,
            now,
        );
        status.phase = if status.drain.as_ref().is_some_and(|d| d.timed_out) {
            Phase::Degraded
        } else if ready {
            Phase::Ready
        } else {
            Phase::Pending
        };
        status.observed_generation = generation;
        status.replicas = Some(obs.status_replicas);
        status.ready_replicas = Some(obs.ready_replicas);
        status.updated_replicas = Some(obs.updated_replicas);
        self.finish(hs, &namespace, &name, &status, &notes).await?;

        let requeue = if status.drain.is_some() {
            REQUEUE_DRAINING
        } else if ready {
            REQUEUE_STEADY
        } else {
            REQUEUE_PROGRESSING
        };
        Ok(Outcome {
            requeue: Some(requeue),
            status,
            applied: Some((replicas_next, partition_next)),
        })
    }

    /// Resolves the admin API endpoint and sets `DrainAvailable`.
    async fn endpoint(
        &self,
        hs: &Homeserver,
        namespace: &str,
        name: &str,
        status: &mut HomeserverStatus,
        now: DateTime<Utc>,
    ) -> Result<Option<AdminEndpoint>, ReconcileError> {
        let generation = hs.metadata.generation;
        if !is_cluster(&hs.spec) {
            set_condition(
                &mut status.conditions,
                CONDITION_DRAIN_AVAILABLE,
                false,
                "SingleNode",
                "embedded storage runs one replica; there is nothing to hand shards to",
                generation,
                now,
            );
            return Ok(None);
        }
        let Some(api) = &hs.spec.admin_api else {
            set_condition(
                &mut status.conditions,
                CONDITION_DRAIN_AVAILABLE,
                false,
                "NoAdminApi",
                "spec.adminApi is not set: pods go without being drained first",
                generation,
                now,
            );
            return Ok(None);
        };
        let token = self
            .kube
            .secret_value(namespace, &api.token_secret_ref)
            .await?
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        let Some(token) = token else {
            set_condition(
                &mut status.conditions,
                CONDITION_DRAIN_AVAILABLE,
                false,
                "TokenSecretMissing",
                format!(
                    "Secret {} has no key {}: pods go without being drained first",
                    api.token_secret_ref.name, api.token_secret_ref.key
                ),
                generation,
                now,
            );
            return Ok(None);
        };
        set_condition(
            &mut status.conditions,
            CONDITION_DRAIN_AVAILABLE,
            true,
            "AdminApiConfigured",
            "",
            generation,
            now,
        );
        Ok(Some(AdminEndpoint {
            base_url: api.url.clone().unwrap_or_else(|| {
                format!(
                    "http://{name}.{namespace}.svc:{}",
                    super::objects::CLIENT_PORT
                )
            }),
            token,
        }))
    }

    async fn start_drain(
        &self,
        hs: &Homeserver,
        endpoint: &AdminEndpoint,
        pod: &str,
        reason: DrainReason,
        now: DateTime<Utc>,
        notes: &mut Vec<Note>,
    ) -> Result<Started, ReconcileError> {
        let Some(replica) = find_replica(&self.admin, endpoint, pod).await? else {
            tracing::info!(homeserver = %hs.name_any(), %pod, "no replica registered for the pod; nothing to drain");
            return Ok(Started::Nothing);
        };
        match self.admin.drain(endpoint, &replica.id).await {
            Ok(r) => {
                self.metrics.drain_event("started");
                tracing::info!(homeserver = %hs.name_any(), %pod, replica = %r.id, shards = r.shard_count, ?reason, "draining");
                notes.push(Note::normal(
                    "Draining",
                    format!(
                        "draining {pod} ({reason:?}): {} shard(s) to hand off",
                        r.shard_count
                    ),
                ));
                Ok(Started::Draining(DrainStatus {
                    pod: pod.to_owned(),
                    replica_id: r.id.clone(),
                    reason,
                    started_at: rfc3339(now),
                    task_id: r.drain_task_id.clone(),
                    task_status: None,
                    shards_remaining: Some(r.shard_count),
                    timed_out: false,
                }))
            }
            Err(AdminError::Conflict(detail)) => {
                self.metrics.drain_event("refused");
                tracing::warn!(homeserver = %hs.name_any(), %pod, %detail, "drain refused, will retry");
                notes.push(Note::warning(
                    "DrainRefused",
                    format!("the server refused to drain {pod}: {detail}; retrying"),
                ));
                Ok(Started::Refused)
            }
            Err(AdminError::NotFound) => Ok(Started::Nothing),
            Err(e) => Err(e.into()),
        }
    }

    /// Undrains every replica the operator drained, then removes the finalizer.
    async fn finalize(
        &self,
        hs: &Homeserver,
        namespace: &str,
        status: &mut HomeserverStatus,
    ) -> Result<(), ReconcileError> {
        let mut replicas: Vec<String> = status
            .pending_undrains
            .iter()
            .map(|p| p.replica_id.clone())
            .collect();
        if let Some(d) = &status.drain {
            replicas.push(d.replica_id.clone());
        }
        let endpoint = if replicas.is_empty() {
            None
        } else {
            let mut scratch = status.clone();
            self.endpoint(hs, namespace, &hs.name_any(), &mut scratch, Utc::now())
                .await?
        };
        if let Some(ep) = &endpoint {
            for id in &replicas {
                match self.admin.undrain(ep, id).await {
                    Ok(_) | Err(AdminError::NotFound) => {
                        self.metrics.drain_event("undrained");
                        tracing::info!(homeserver = %hs.name_any(), replica = %id, "undrained on deletion");
                    }
                    // Best effort: the server may be going away with the resource.
                    Err(e) => {
                        tracing::warn!(homeserver = %hs.name_any(), replica = %id, error = %e, "undrain on deletion failed")
                    }
                }
            }
        }
        status.drain = None;
        status.pending_undrains.clear();
        self.kube.set_finalizer(hs, false).await?;
        Ok(())
    }

    async fn write_status(
        &self,
        hs: &Homeserver,
        namespace: &str,
        name: &str,
        status: &HomeserverStatus,
    ) -> Result<(), ReconcileError> {
        if hs.status.as_ref() != Some(status) {
            self.kube.patch_status(namespace, name, status).await?;
        }
        Ok(())
    }

    async fn finish(
        &self,
        hs: &Homeserver,
        namespace: &str,
        name: &str,
        status: &HomeserverStatus,
        notes: &[Note],
    ) -> Result<(), ReconcileError> {
        self.write_status(hs, namespace, name, status).await?;
        for note in notes {
            self.kube.publish(hs, note).await;
        }
        Ok(())
    }
}

/// Lets a drained pod go: returns the replica count (scale-down) or partition (rolling update)
/// that does, and records when to undrain its replica.
fn complete(
    drain: &DrainStatus,
    ordinal: i32,
    pending: &mut Vec<PendingUndrain>,
) -> (Option<i32>, Option<i32>) {
    let (when, result) = match drain.reason {
        DrainReason::ScaleDown => (UndrainWhen::AfterRemoval, (Some(ordinal), None)),
        DrainReason::RollingUpdate => (UndrainWhen::WhenReady, (None, Some(ordinal))),
    };
    pending.retain(|p| p.pod != drain.pod);
    pending.push(PendingUndrain {
        pod: drain.pod.clone(),
        replica_id: drain.replica_id.clone(),
        when,
    });
    result
}

/// Sets the StatefulSet's replica count and update partition.
fn set_scale(objects: &mut DesiredObjects, replicas: i32, partition: i32) {
    if let Some(spec) = objects.stateful_set.spec.as_mut() {
        spec.replicas = Some(replicas);
        if let Some(rolling) = spec
            .update_strategy
            .as_mut()
            .and_then(|u| u.rolling_update.as_mut())
        {
            rolling.partition = Some(partition);
        }
    }
}
