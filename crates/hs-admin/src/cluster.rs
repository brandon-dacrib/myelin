//! The cluster's replicas and shards (`cluster.replicas.*` and `cluster.shards.list`, the
//! `Cluster` tag of `openapi/openapi.yaml`; RFC 0004 section 4.11). `cluster.get`, the summary,
//! is served from [`crate::sources::OverviewSource`] and predates this module.
//!
//! [`ClusterSource`] is what the handlers read and act through. `hs-cli` implements it over
//! `hs-cluster`'s replica registry, its shard rows and the answering replica's ownership view;
//! [`InMemoryCluster`] implements it for tests.
//!
//! # A single node is a cluster of one
//!
//! A server not running as a cluster answers with one replica (`role: single-node`) owning every
//! shard of the layout. Draining it is refused with `409`: there is no other replica to take its
//! shards. Undraining it changes nothing.
//!
//! # Draining
//!
//! `POST /cluster/replicas/{id}/drain` records a drain request for the replica in the shared
//! store (any replica can take the request; the drained one honours it at its next heartbeat),
//! answers the replica as `draining`, and starts a task, `cluster.replicas.drain`, that follows
//! the handoff and succeeds once the replica owns no shards (see [`follow_drain`]). The replica
//! is then `drained`: it keeps serving, forwarding each request to the shard's new owner, and a
//! restart does not undo the drain. `POST .../undrain` withdraws the request, cancels the task if
//! it is still running, and the replica takes back its share.
//!
//! Draining is refused (`409`) when no *other* replica is active, since nothing would take the
//! shards. Each drain that starts, and each undrain that withdraws one, is written to the audit
//! log (`cluster.replicas.drain`, `cluster.replicas.undrain`) and published on the event stream
//! (`cluster.replica_draining`, `cluster.replica_undrained`). A drain or undrain that changes
//! nothing (already draining, not drained) answers the replica as it is and records nothing.
//! [`ClusterSource::observe`] is told how each drain goes, which is where `hs-cli` counts them
//! for Prometheus.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

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
use crate::tasks::TaskContext;

/// The task action a drain runs under.
pub const DRAIN_TASK_ACTION: &str = "cluster.replicas.drain";

/// Where a replica is in its life: the OpenAPI `Replica.status` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaStatus {
    /// Heartbeating, not yet taking shards.
    Joining,
    /// Serving and taking shards.
    Active,
    /// Out of hashing and handing its shards off; still owns some.
    Draining,
    /// Asked to drain, and owns nothing. Still serving, by forwarding.
    Drained,
    /// Its heartbeats stopped; the others are taking its shards.
    Unreachable,
}

impl ReplicaStatus {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Joining => "joining",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Drained => "drained",
            Self::Unreachable => "unreachable",
        }
    }
}

/// The OpenAPI `Replica` schema: one replica, as the answering replica sees it.
///
/// Every optional member is serialized, as `null` when absent, as the contract's
/// `type: [string, 'null']` members say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replica {
    /// The replica id (a pod name in Kubernetes).
    pub id: String,
    /// `single-node` for the one replica of a server not running as a cluster, `replica`
    /// otherwise.
    pub role: String,
    /// Where it is in its life.
    pub status: ReplicaStatus,
    /// Shards it owns.
    pub shard_count: u64,
    /// Its generation, which changes every time it starts.
    pub epoch: u64,
    /// Whether it is the replica that answered.
    pub this_replica: bool,
    /// `host:port` of its mesh listener; `None` for a single node.
    pub mesh_addr: Option<String>,
    /// Its server version, as it reported it.
    pub version: Option<String>,
    /// Its topology zone.
    pub zone: Option<String>,
    /// Its last heartbeat (RFC 3339); `None` for a single node.
    pub last_heartbeat_at: Option<String>,
    /// When an administrator asked it to drain (RFC 3339), while that request is in force.
    pub drain_requested_at: Option<String>,
    /// Who asked it to drain.
    pub drain_requested_by: Option<String>,
    /// The task following its drain.
    pub drain_task_id: Option<String>,
}

impl Replica {
    /// Whether an administrator's drain request is in force for it.
    #[must_use]
    pub fn drain_requested(&self) -> bool {
        self.drain_requested_at.is_some()
    }
}

/// The OpenAPI `Shard` schema: one unit of ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    /// One of [`SHARD_KINDS`].
    pub kind: String,
    /// `kind/index`.
    pub id: String,
    /// The owning replica, if any.
    pub owner: Option<String>,
    /// `owned`, `released` (it had an owner and has none now) or `unassigned` (never owned).
    pub state: String,
    /// The fencing epoch, bumped on every change of owner.
    pub epoch: u64,
}

/// The shard kinds, in layout order.
pub const SHARD_KINDS: [&str; 5] = ["room", "user", "federation", "appservice", "global"];

/// The refusal for a `kind` that is not one of [`SHARD_KINDS`].
#[must_use]
pub fn unknown_kind(kind: &str) -> SourceError {
    SourceError::InvalidField {
        pointer: "/kind",
        detail: format!(
            "{kind:?} is not a shard kind; the kinds are {}",
            SHARD_KINDS.join(", ")
        ),
    }
}

/// What a drain request did: the replica as it stands after it, and whether this request is the
/// one that started the drain (`false` when it was already draining or drained).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainStarted {
    /// The replica, `draining` or `drained`.
    pub replica: Replica,
    /// Whether this call created the drain request.
    pub started: bool,
}

/// What an undrain did: the replica as it stands after it, and the drain request it withdrew,
/// if there was one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undrained {
    /// The replica.
    pub replica: Replica,
    /// Whether a drain request was in force and has been withdrawn.
    pub withdrawn: bool,
    /// The task that was following the withdrawn drain.
    pub task_id: Option<String>,
}

/// How [`follow_drain`] polls a drain, and when it gives up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainFollow {
    /// How often the replica is read.
    pub poll: Duration,
    /// How long the replica may take to own nothing before the task fails. The drain request
    /// stays in force after that; only the following stops.
    pub timeout: Duration,
}

impl Default for DrainFollow {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(1),
            timeout: Duration::from_secs(15 * 60),
        }
    }
}

/// How a drain went, for [`ClusterSource::observe`] (metrics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainEvent {
    /// A drain request was recorded.
    Requested {
        /// The replica.
        replica: String,
    },
    /// The replica owns no shards any more.
    Completed {
        /// The replica.
        replica: String,
        /// How many shards it handed off while followed.
        handed_off: u64,
        /// From the request to owning nothing.
        elapsed: Duration,
    },
    /// The follow timed out with shards still owned.
    TimedOut {
        /// The replica.
        replica: String,
        /// Shards it still owned.
        remaining: u64,
    },
    /// A drain request was withdrawn.
    Undrained {
        /// The replica.
        replica: String,
    },
}

/// Replicas and shards, and the two lifecycle actions. A method naming a replica that is not
/// registered answers [`SourceError::NotFound`].
#[async_trait]
pub trait ClusterSource: Send + Sync + 'static {
    /// Every registered replica, by id.
    async fn replicas(&self) -> Result<Vec<Replica>, SourceError>;

    /// One replica, or `None` if it is not registered.
    async fn replica(&self, id: &str) -> Result<Option<Replica>, SourceError> {
        Ok(self.replicas().await?.into_iter().find(|r| r.id == id))
    }

    /// Every shard of the layout in layout order, or only those of `kind`.
    /// [`SourceError::InvalidField`] (see [`unknown_kind`]) for a kind that does not exist.
    async fn shards(&self, kind: Option<&str>) -> Result<Vec<Shard>, SourceError>;

    /// Records a drain request for replica `id` from `requested_by`, unless one is in force
    /// already. [`SourceError::Conflict`] when no other replica is active to take its shards
    /// (always, for a single node).
    async fn drain(&self, id: &str, requested_by: &str) -> Result<DrainStarted, SourceError>;

    /// Notes the task following replica `id`'s drain on the drain request, so that any replica
    /// answering for it can name the task. Does nothing if the request was withdrawn meanwhile.
    async fn set_drain_task(&self, id: &str, task_id: &str) -> Result<(), SourceError>;

    /// Withdraws replica `id`'s drain request, if it has one.
    async fn undrain(&self, id: &str) -> Result<Undrained, SourceError>;

    /// How a drain started here is followed.
    fn drain_follow(&self) -> DrainFollow {
        DrainFollow::default()
    }

    /// Told how each drain goes (to count it). Does nothing by default.
    fn observe(&self, _event: DrainEvent) {}
}

/// Follows replica `id`'s drain until it owns no shards, reporting progress to `ctx` as shards
/// handed off out of the number it owned at the start. The task's result is
/// `{replica, handed_off, elapsed_ms}`; it fails if the drain is withdrawn from under it or
/// `follow.timeout` passes first (the drain itself carries on; only the following stops). A
/// replica that leaves the registry meanwhile (stopped: its shutdown releases its shards) counts
/// as drained.
///
/// # Errors
/// The problem the task fails with.
// `Problem` is what `TaskRegistry::spawn` records as a failed task's error; boxing it here would
// only be unboxed again there.
#[allow(clippy::result_large_err)]
pub async fn follow_drain(
    source: Arc<dyn ClusterSource>,
    id: String,
    owned_at_start: u64,
    ctx: TaskContext,
) -> Result<serde_json::Value, Problem> {
    let follow = source.drain_follow();
    let started = tokio::time::Instant::now();
    let mut total = owned_at_start;
    let mut last_reported: Option<u64> = None;
    loop {
        match source.replica(&id).await {
            Ok(None) => {
                return Ok(finish(&*source, &id, total, started.elapsed(), true));
            }
            Ok(Some(replica)) => {
                if !replica.drain_requested() {
                    return Err(Problem::conflict().with_detail(format!(
                        "the drain of {id} was withdrawn before it finished"
                    )));
                }
                total = total.max(replica.shard_count);
                if replica.shard_count == 0 {
                    return Ok(finish(&*source, &id, total, started.elapsed(), false));
                }
                if last_reported != Some(replica.shard_count) {
                    last_reported = Some(replica.shard_count);
                    let left = format!("{} shards left on {id}", replica.shard_count);
                    ctx.progress(
                        total - replica.shard_count,
                        Some(total),
                        Some("shards"),
                        Some(&left),
                    )
                    .await;
                }
                if started.elapsed() >= follow.timeout {
                    source.observe(DrainEvent::TimedOut {
                        replica: id.clone(),
                        remaining: replica.shard_count,
                    });
                    tracing::warn!(
                        replica = %id,
                        remaining = replica.shard_count,
                        waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "a drain did not finish in time; it stays requested, but is no longer followed"
                    );
                    return Err(Problem::unavailable().with_detail(format!(
                        "{id} still owned {} of its {total} shards after {} s; the drain is still \
                         requested. Is another replica active and heartbeating to take them?",
                        replica.shard_count,
                        follow.timeout.as_secs()
                    )));
                }
            }
            Err(error) => {
                // A store hiccup is not the drain failing; try again at the next poll.
                tracing::warn!(replica = %id, %error, "could not read a draining replica");
            }
        }
        tokio::time::sleep(follow.poll).await;
    }
}

fn finish(
    source: &dyn ClusterSource,
    id: &str,
    handed_off: u64,
    elapsed: Duration,
    left_registry: bool,
) -> serde_json::Value {
    source.observe(DrainEvent::Completed {
        replica: id.to_owned(),
        handed_off,
        elapsed,
    });
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        replica = %id,
        handed_off,
        elapsed_ms,
        left_registry,
        "a replica drained: it owns no shards"
    );
    json!({
        "replica": id,
        "handed_off": handed_off,
        "elapsed_ms": elapsed_ms,
        "left_registry": left_registry,
    })
}

// -------------------------------------------------------------------------------------------
// In memory.
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MemoryDrain {
    requested_at: String,
    requested_by: String,
    task_id: Option<String>,
}

/// A [`ClusterSource`] held in memory, for tests. Replicas are stored as given; a drain request
/// releases the replica's shards at once (so it goes straight to `drained`) unless
/// [`InMemoryCluster::set_release_on_drain`] turned that off, in which case the test moves the
/// shards itself with [`InMemoryCluster::insert_shard`].
#[derive(Debug, Default)]
pub struct InMemoryCluster {
    replicas: RwLock<BTreeMap<String, Replica>>,
    shards: RwLock<Vec<Shard>>,
    drains: RwLock<BTreeMap<String, MemoryDrain>>,
    keep_shards_on_drain: std::sync::atomic::AtomicBool,
    follow: RwLock<DrainFollow>,
    observed: RwLock<Vec<DrainEvent>>,
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl InMemoryCluster {
    /// A server not running as a cluster: one replica `id` (`single-node`) owning
    /// `shards_per_kind` shards of each kind but `global`, which has one.
    #[must_use]
    pub fn single_node(id: &str, shards_per_kind: u32) -> Self {
        let cluster = Self::default();
        cluster.insert_replica(Replica {
            id: id.to_owned(),
            role: "single-node".into(),
            status: ReplicaStatus::Active,
            shard_count: 0,
            epoch: 1,
            this_replica: true,
            mesh_addr: None,
            version: None,
            zone: None,
            last_heartbeat_at: None,
            drain_requested_at: None,
            drain_requested_by: None,
            drain_task_id: None,
        });
        for kind in SHARD_KINDS {
            let count = if kind == "global" { 1 } else { shards_per_kind };
            for index in 0..count {
                cluster.insert_shard(Shard {
                    kind: kind.to_owned(),
                    id: format!("{kind}/{index}"),
                    owner: Some(id.to_owned()),
                    state: "owned".into(),
                    epoch: 1,
                });
            }
        }
        cluster
    }

    /// A clustered replica row, `active`, for [`InMemoryCluster::insert_replica`].
    #[must_use]
    pub fn replica(id: &str, this_replica: bool) -> Replica {
        Replica {
            id: id.to_owned(),
            role: "replica".into(),
            status: ReplicaStatus::Active,
            shard_count: 0,
            epoch: 1,
            this_replica,
            mesh_addr: Some(format!("{id}:9000")),
            version: Some("0.0.1".into()),
            zone: None,
            last_heartbeat_at: Some(hs_http::time::now_rfc3339()),
            drain_requested_at: None,
            drain_requested_by: None,
            drain_task_id: None,
        }
    }

    /// Adds or replaces a replica.
    pub fn insert_replica(&self, replica: Replica) {
        write(&self.replicas).insert(replica.id.clone(), replica);
    }

    /// Adds or replaces a shard (by id), keeping layout order.
    pub fn insert_shard(&self, shard: Shard) {
        let mut shards = write(&self.shards);
        shards.retain(|s| s.id != shard.id);
        shards.push(shard);
        shards.sort_by_key(|s| {
            let kind = SHARD_KINDS.iter().position(|k| *k == s.kind);
            let index: u32 =
                s.id.rsplit('/')
                    .next()
                    .and_then(|i| i.parse().ok())
                    .unwrap_or(0);
            (kind, index)
        });
    }

    /// Removes a replica (it stopped and deregistered).
    pub fn remove_replica(&self, id: &str) {
        write(&self.replicas).remove(id);
    }

    /// When `false`, a drain request leaves the replica's shards where they are.
    pub fn set_release_on_drain(&self, release: bool) {
        self.keep_shards_on_drain
            .store(!release, std::sync::atomic::Ordering::SeqCst);
    }

    /// How drains are followed.
    pub fn set_drain_follow(&self, follow: DrainFollow) {
        *write(&self.follow) = follow;
    }

    /// Every [`DrainEvent`] observed so far.
    #[must_use]
    pub fn observed(&self) -> Vec<DrainEvent> {
        read(&self.observed).clone()
    }

    fn view(&self, mut replica: Replica) -> Replica {
        replica.shard_count = read(&self.shards)
            .iter()
            .filter(|s| s.owner.as_deref() == Some(replica.id.as_str()))
            .count() as u64;
        match read(&self.drains).get(&replica.id) {
            Some(drain) => {
                replica.status = if replica.shard_count == 0 {
                    ReplicaStatus::Drained
                } else {
                    ReplicaStatus::Draining
                };
                replica.drain_requested_at = Some(drain.requested_at.clone());
                replica.drain_requested_by = Some(drain.requested_by.clone());
                replica.drain_task_id = drain.task_id.clone();
            }
            None => {
                replica.drain_requested_at = None;
                replica.drain_requested_by = None;
                replica.drain_task_id = None;
                if matches!(
                    replica.status,
                    ReplicaStatus::Draining | ReplicaStatus::Drained
                ) {
                    replica.status = ReplicaStatus::Active;
                }
            }
        }
        replica
    }
}

#[async_trait]
impl ClusterSource for InMemoryCluster {
    async fn replicas(&self) -> Result<Vec<Replica>, SourceError> {
        let replicas: Vec<Replica> = read(&self.replicas).values().cloned().collect();
        Ok(replicas.into_iter().map(|r| self.view(r)).collect())
    }

    async fn shards(&self, kind: Option<&str>) -> Result<Vec<Shard>, SourceError> {
        if let Some(kind) = kind
            && !SHARD_KINDS.contains(&kind)
        {
            return Err(unknown_kind(kind));
        }
        Ok(read(&self.shards)
            .iter()
            .filter(|s| kind.is_none_or(|k| s.kind == k))
            .cloned()
            .collect())
    }

    async fn drain(&self, id: &str, requested_by: &str) -> Result<DrainStarted, SourceError> {
        let replicas = self.replicas().await?;
        let target = replicas
            .iter()
            .find(|r| r.id == id)
            .ok_or(SourceError::NotFound)?;
        if target.drain_requested() {
            return Ok(DrainStarted {
                replica: target.clone(),
                started: false,
            });
        }
        if target.role == "single-node" {
            return Err(SourceError::Conflict(
                "this server is not running as a cluster: there is no other replica to take its \
                 shards"
                    .into(),
            ));
        }
        if !replicas
            .iter()
            .any(|r| r.id != id && r.status == ReplicaStatus::Active)
        {
            return Err(SourceError::Conflict(format!(
                "no other replica is active to take {id}'s shards"
            )));
        }
        write(&self.drains).insert(
            id.to_owned(),
            MemoryDrain {
                requested_at: hs_http::time::now_rfc3339(),
                requested_by: requested_by.to_owned(),
                task_id: None,
            },
        );
        if !self
            .keep_shards_on_drain
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            for shard in write(&self.shards).iter_mut() {
                if shard.owner.as_deref() == Some(id) {
                    shard.owner = None;
                    shard.state = "released".into();
                    shard.epoch += 1;
                }
            }
        }
        let replica = self.replica(id).await?.ok_or(SourceError::NotFound)?;
        Ok(DrainStarted {
            replica,
            started: true,
        })
    }

    async fn set_drain_task(&self, id: &str, task_id: &str) -> Result<(), SourceError> {
        if let Some(drain) = write(&self.drains).get_mut(id) {
            drain.task_id = Some(task_id.to_owned());
        }
        Ok(())
    }

    async fn undrain(&self, id: &str) -> Result<Undrained, SourceError> {
        if !read(&self.replicas).contains_key(id) {
            return Err(SourceError::NotFound);
        }
        let withdrawn = write(&self.drains).remove(id);
        let replica = self.replica(id).await?.ok_or(SourceError::NotFound)?;
        Ok(Undrained {
            replica,
            withdrawn: withdrawn.is_some(),
            task_id: withdrawn.and_then(|d| d.task_id),
        })
    }

    fn drain_follow(&self) -> DrainFollow {
        *read(&self.follow)
    }

    fn observe(&self, event: DrainEvent) {
        write(&self.observed).push(event);
    }
}

// -------------------------------------------------------------------------------------------
// Handlers.
// -------------------------------------------------------------------------------------------

/// The paged listings' query string.
#[derive(Debug, Deserialize)]
pub(crate) struct ClusterPageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    kind: Option<String>,
}

fn invalid_cursor(cursor: &str, instance: &str) -> Option<Response> {
    if cursor.is_empty() || cursor.parse::<usize>().is_ok() {
        return None;
    }
    let detail = format!("{cursor:?} is not a cursor this listing handed out");
    Some(
        Problem::validation_failed()
            .with_detail(detail.clone())
            .with_errors(vec![ValidationError::new("/cursor", detail)])
            .with_instance(instance.to_owned())
            .into_response(),
    )
}

fn no_such_replica(id: &str, instance: &str) -> Response {
    Problem::not_found()
        .with_detail(format!("there is no replica {id} in the registry"))
        .with_instance(instance.to_owned())
        .into_response()
}

fn source_error(error: SourceError, id: &str, instance: &str) -> Response {
    match error {
        SourceError::NotFound => no_such_replica(id, instance),
        other => other.to_problem().with_instance(instance).into_response(),
    }
}

/// `GET /api/v1/cluster/replicas` (`admin:read`): every registered replica, by id.
pub(crate) async fn replicas_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ClusterPageQuery>,
) -> Response {
    let instance = "/api/v1/cluster/replicas";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(cluster) = &state.cluster else {
        return unwired("cluster", instance);
    };
    if let Some(response) = query
        .cursor
        .as_deref()
        .and_then(|c| invalid_cursor(c, instance))
    {
        return response;
    }
    let mut replicas = match cluster.replicas().await {
        Ok(replicas) => replicas,
        Err(e) => return e.to_problem().with_instance(instance).into_response(),
    };
    replicas.sort_by(|a, b| a.id.cmp(&b.id));
    axum::Json(Page::paginate(
        replicas,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    ))
    .into_response()
}

/// `GET /api/v1/cluster/replicas/{id}` (`admin:read`).
pub(crate) async fn replicas_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let instance = format!("/api/v1/cluster/replicas/{id}");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let Some(cluster) = &state.cluster else {
        return unwired("cluster", &instance);
    };
    match cluster.replica(&id).await {
        Ok(Some(replica)) => axum::Json(replica).into_response(),
        Ok(None) => no_such_replica(&id, &instance),
        Err(e) => source_error(e, &id, &instance),
    }
}

/// `POST /api/v1/cluster/replicas/{id}/drain` (`admin:write`): see the module docs. `200` with
/// the replica, `draining` (or already `drained`); `409` when nothing would take its shards.
pub(crate) async fn replicas_drain(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/cluster/replicas/{id}/drain");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(cluster) = state.cluster.clone() else {
        return unwired("cluster", &instance);
    };
    let fingerprint = [id.as_bytes(), b"\n", &body].concat();
    if let Err(response) = check_replay(
        &state,
        &headers,
        "cluster.replicas.drain",
        &fingerprint,
        &instance,
    ) {
        return response;
    }
    let DrainStarted {
        mut replica,
        started,
    } = match cluster.drain(&id, &principal.id).await {
        Ok(outcome) => outcome,
        Err(e) => {
            if let SourceError::Conflict(detail) = &e {
                tracing::info!(replica = %id, requested_by = %principal.id, %detail, "a drain was refused");
            }
            return source_error(e, &id, &instance);
        }
    };
    if started {
        cluster.observe(DrainEvent::Requested {
            replica: id.clone(),
        });
        tracing::info!(
            replica = %id,
            requested_by = %principal.id,
            shards = replica.shard_count,
            "an administrator asked a replica to drain"
        );
        if let Some(tasks) = &state.tasks {
            let source = cluster.clone();
            let follow_id = id.clone();
            let owned = replica.shard_count;
            match tasks
                .spawn(
                    DRAIN_TASK_ACTION,
                    Some(ResourceRef::new("replica", id.clone())),
                    principal.to_actor(),
                    move |ctx| follow_drain(source, follow_id, owned, ctx),
                )
                .await
            {
                Ok(task) => {
                    if let Err(error) = cluster.set_drain_task(&id, &task.id).await {
                        tracing::warn!(replica = %id, task = %task.id, %error, "could not note the drain's task on its request");
                    }
                    replica.drain_task_id = Some(task.id);
                }
                Err(error) => {
                    // The drain goes ahead regardless; only nobody follows it.
                    tracing::warn!(replica = %id, %error, "could not start the task that follows a drain");
                }
            }
        }
        if let Err(response) = record(
            &state,
            &principal,
            "cluster.replicas.drain",
            "cluster.replica_draining",
            ResourceRef::new("replica", id.clone()),
            vec![AuditChange {
                pointer: "/status".into(),
                from: Some(json!("active")),
                to: Some(json!(replica.status.as_str())),
            }],
            json!({
                "replica": id,
                "shard_count": replica.shard_count,
                "task_id": replica.drain_task_id,
            }),
            200,
        )
        .await
        {
            return response;
        }
    }
    respond_and_remember(
        &state,
        &headers,
        "cluster.replicas.drain",
        &fingerprint,
        StatusCode::OK,
        &replica,
        &[],
    )
}

/// `POST /api/v1/cluster/replicas/{id}/undrain` (`admin:write`): withdraws the drain request and
/// cancels the task following it. A replica that was not drained is answered as it is.
pub(crate) async fn replicas_undrain(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = format!("/api/v1/cluster/replicas/{id}/undrain");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(cluster) = state.cluster.clone() else {
        return unwired("cluster", &instance);
    };
    let fingerprint = [id.as_bytes(), b"\n", &body].concat();
    if let Err(response) = check_replay(
        &state,
        &headers,
        "cluster.replicas.undrain",
        &fingerprint,
        &instance,
    ) {
        return response;
    }
    let undrained = match cluster.undrain(&id).await {
        Ok(undrained) => undrained,
        Err(e) => return source_error(e, &id, &instance),
    };
    if undrained.withdrawn {
        cluster.observe(DrainEvent::Undrained {
            replica: id.clone(),
        });
        tracing::info!(replica = %id, requested_by = %principal.id, "an administrator undrained a replica");
        if let (Some(tasks), Some(task_id)) = (&state.tasks, &undrained.task_id)
            && let Err(error) = tasks.cancel(task_id).await
        {
            tracing::warn!(replica = %id, task = %task_id, %error, "could not cancel the task following an undrained replica");
        }
        if let Err(response) = record(
            &state,
            &principal,
            "cluster.replicas.undrain",
            "cluster.replica_undrained",
            ResourceRef::new("replica", id.clone()),
            vec![AuditChange {
                pointer: "/status".into(),
                from: Some(json!("drained")),
                to: Some(json!(undrained.replica.status.as_str())),
            }],
            json!({ "replica": id, "task_id": undrained.task_id }),
            200,
        )
        .await
        {
            return response;
        }
    }
    respond_and_remember(
        &state,
        &headers,
        "cluster.replicas.undrain",
        &fingerprint,
        StatusCode::OK,
        &undrained.replica,
        &[],
    )
}

/// `GET /api/v1/cluster/shards` (`admin:read`): the layout, or one `kind` of it, in layout order.
pub(crate) async fn shards_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ClusterPageQuery>,
) -> Response {
    let instance = "/api/v1/cluster/shards";
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, instance).await {
        return response;
    }
    let Some(cluster) = &state.cluster else {
        return unwired("cluster", instance);
    };
    if let Some(response) = query
        .cursor
        .as_deref()
        .and_then(|c| invalid_cursor(c, instance))
    {
        return response;
    }
    let kind = query.kind.as_deref().filter(|k| !k.is_empty());
    if let Some(kind) = kind
        && !SHARD_KINDS.contains(&kind)
    {
        return unknown_kind(kind)
            .to_problem()
            .with_instance(instance)
            .into_response();
    }
    match cluster.shards(kind).await {
        Ok(shards) => axum::Json(Page::paginate(
            shards,
            query.cursor.as_deref(),
            query.limit,
            query.include_total.unwrap_or(false),
        ))
        .into_response(),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditFilter, AuditSink, InMemoryAuditSink};
    use crate::handler_kit::testing::{call, state};
    use crate::model::{AuditEntry, TaskStatus};
    use crate::tasks::TaskRegistry;

    async fn request(
        state: &AdminState,
        method: &str,
        uri: &str,
        token: &str,
        key: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let (status, _, body) = call(state, method, uri, Some(token), None, key).await;
        (status, body)
    }

    /// Every audit entry, oldest first.
    async fn entries(audit: &InMemoryAuditSink) -> Vec<AuditEntry> {
        let mut entries = audit.query(&AuditFilter::default()).await.unwrap();
        entries.reverse();
        entries
    }

    fn two_replicas() -> Arc<InMemoryCluster> {
        let cluster = InMemoryCluster::default();
        cluster.insert_replica(InMemoryCluster::replica("hs-0", true));
        cluster.insert_replica(InMemoryCluster::replica("hs-1", false));
        for index in 0..4u32 {
            cluster.insert_shard(Shard {
                kind: "room".into(),
                id: format!("room/{index}"),
                owner: Some(if index % 2 == 0 { "hs-0" } else { "hs-1" }.into()),
                state: "owned".into(),
                epoch: 1,
            });
        }
        cluster.set_drain_follow(DrainFollow {
            poll: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
        });
        Arc::new(cluster)
    }

    fn with(cluster: Arc<InMemoryCluster>) -> (AdminState, Arc<crate::audit::InMemoryAuditSink>) {
        let (state, audit) = state();
        let mut state = state.with_tasks(TaskRegistry::in_memory());
        state.cluster = Some(cluster);
        (state, audit)
    }

    async fn wait_for_task(state: &AdminState, id: &str, status: TaskStatus) -> crate::model::Task {
        let tasks = state.tasks.clone().unwrap();
        for _ in 0..500 {
            if let Some(task) = tasks.get(id).await.unwrap()
                && task.status == status
            {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {id} never became {status:?}");
    }

    #[tokio::test]
    async fn a_single_node_is_one_replica_owning_every_shard_and_cannot_be_drained() {
        let (state, audit) = with(Arc::new(InMemoryCluster::single_node("hs-solo", 4)));
        let (status, body) = request(&state, "GET", "/api/v1/cluster/replicas", "read", None).await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "hs-solo");
        assert_eq!(items[0]["role"], "single-node");
        assert_eq!(items[0]["status"], "active");
        assert_eq!(items[0]["shard_count"], 17);
        assert_eq!(items[0]["this_replica"], true);
        assert!(items[0]["drain_requested_at"].is_null());

        let (status, body) = request(
            &state,
            "GET",
            "/api/v1/cluster/shards?kind=room&include_total=true",
            "read",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 4);
        assert_eq!(body["items"][3]["id"], "room/3");
        assert_eq!(body["items"][3]["owner"], "hs-solo");

        let (status, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-solo/drain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("not running as a cluster"),
            "{body}"
        );
        let (status, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-solo/undrain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "active");
        // Nothing changed, so nothing was recorded.
        assert!(entries(&audit).await.is_empty());
    }

    #[tokio::test]
    async fn reads_need_a_token_and_bad_queries_are_named() {
        let (state, _) = with(two_replicas());
        let (status, _) = request(&state, "GET", "/api/v1/cluster/replicas", "nope", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/drain",
            "read",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = request(
            &state,
            "GET",
            "/api/v1/cluster/shards?kind=planet",
            "read",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["errors"][0]["pointer"], "/kind");
        let (status, _) = request(
            &state,
            "GET",
            "/api/v1/cluster/shards?cursor=zzz",
            "read",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) =
            request(&state, "GET", "/api/v1/cluster/replicas/hs-9", "read", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["detail"].as_str().unwrap().contains("hs-9"));
        let (status, _) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-9/drain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = request(
            &state,
            "GET",
            "/api/v1/cluster/shards?limit=3",
            "read",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 3);
        assert_eq!(body["next_cursor"], "3");
    }

    #[tokio::test]
    async fn an_unwired_cluster_answers_503() {
        let (state, _) = state();
        for uri in [
            "/api/v1/cluster/replicas",
            "/api/v1/cluster/shards",
            "/api/v1/cluster/replicas/x",
        ] {
            let (status, _) = request(&state, "GET", uri, "read", None).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
    }

    #[tokio::test]
    async fn a_drain_is_followed_by_a_task_audited_and_undone_by_undrain() {
        let cluster = two_replicas();
        cluster.set_release_on_drain(false);
        let (state, audit) = with(cluster.clone());
        let mut events = state.events.subscribe();

        let (status, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/drain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "draining");
        assert_eq!(body["shard_count"], 2);
        assert_eq!(body["drain_requested_by"], "@ops:example.org");
        let task_id = body["drain_task_id"].as_str().unwrap().to_owned();

        // The task reports progress as the shards move, and succeeds when none are left.
        cluster.insert_shard(Shard {
            kind: "room".into(),
            id: "room/1".into(),
            owner: Some("hs-0".into()),
            state: "owned".into(),
            epoch: 2,
        });
        let (_, got) = request(&state, "GET", "/api/v1/cluster/replicas/hs-1", "read", None).await;
        assert_eq!(got["status"], "draining");
        assert_eq!(got["drain_task_id"], task_id.as_str());
        cluster.insert_shard(Shard {
            kind: "room".into(),
            id: "room/3".into(),
            owner: Some("hs-0".into()),
            state: "owned".into(),
            epoch: 2,
        });
        let task = wait_for_task(&state, &task_id, TaskStatus::Succeeded).await;
        assert_eq!(task.action, DRAIN_TASK_ACTION);
        assert_eq!(task.result.as_ref().unwrap()["handed_off"], 2);
        let (_, got) = request(&state, "GET", "/api/v1/cluster/replicas/hs-1", "read", None).await;
        assert_eq!(got["status"], "drained");
        assert_eq!(got["shard_count"], 0);

        // Asking again changes nothing and records nothing more.
        let (status, again) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/drain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again["drain_task_id"], task_id.as_str());

        // The last active replica cannot be drained while hs-1 is.
        let (status, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-0/drain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("no other replica")
        );

        let (status, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/undrain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "active");
        assert!(body["drain_requested_at"].is_null());

        let recorded = entries(&audit).await;
        let actions: Vec<String> = recorded.iter().map(|e| e.action.clone()).collect();
        assert_eq!(
            actions,
            ["cluster.replicas.drain", "cluster.replicas.undrain"]
        );
        let entry = &recorded[0];
        assert_eq!(entry.target.id, "hs-1");
        assert_eq!(entry.actor.id, "@ops:example.org");
        let mut types = Vec::new();
        while let Ok(event) = events.try_recv() {
            types.push(event.r#type.clone());
        }
        assert!(
            types.contains(&"cluster.replica_draining".to_owned()),
            "{types:?}"
        );
        assert!(
            types.contains(&"cluster.replica_undrained".to_owned()),
            "{types:?}"
        );
        assert!(matches!(
            cluster.observed().as_slice(),
            [
                DrainEvent::Requested { .. },
                DrainEvent::Completed { handed_off: 2, .. },
                DrainEvent::Undrained { .. }
            ]
        ));
    }

    #[tokio::test]
    async fn undraining_cancels_a_drain_still_in_progress_and_a_stuck_drain_times_out() {
        let cluster = two_replicas();
        cluster.set_release_on_drain(false);
        let (state, _) = with(cluster.clone());
        let (_, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/drain",
            "admin",
            None,
        )
        .await;
        let task_id = body["drain_task_id"].as_str().unwrap().to_owned();
        wait_for_task(&state, &task_id, TaskStatus::Running).await;
        let (status, _) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/undrain",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wait_for_task(&state, &task_id, TaskStatus::Cancelled).await;

        cluster.set_drain_follow(DrainFollow {
            poll: Duration::from_millis(5),
            timeout: Duration::from_millis(50),
        });
        let (_, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/drain",
            "admin",
            None,
        )
        .await;
        let task_id = body["drain_task_id"].as_str().unwrap().to_owned();
        let task = wait_for_task(&state, &task_id, TaskStatus::Failed).await;
        assert!(
            task.error
                .unwrap()
                .detail
                .unwrap()
                .contains("still owned 2 of its 2 shards"),
        );
        assert!(
            cluster
                .observed()
                .iter()
                .any(|e| matches!(e, DrainEvent::TimedOut { remaining: 2, .. }))
        );
    }

    #[tokio::test]
    async fn a_replica_that_leaves_the_registry_while_draining_counts_as_drained() {
        let cluster = two_replicas();
        cluster.set_release_on_drain(false);
        let (state, _) = with(cluster.clone());
        let (_, body) = request(
            &state,
            "POST",
            "/api/v1/cluster/replicas/hs-1/drain",
            "admin",
            None,
        )
        .await;
        let task_id = body["drain_task_id"].as_str().unwrap().to_owned();
        cluster.remove_replica("hs-1");
        let task = wait_for_task(&state, &task_id, TaskStatus::Succeeded).await;
        assert_eq!(task.result.unwrap()["left_registry"], true);
    }

    #[tokio::test]
    async fn an_idempotent_retry_of_a_drain_is_answered_the_same_way() {
        let (state, audit) = with(two_replicas());
        let uri = "/api/v1/cluster/replicas/hs-1/drain";
        let (status, headers, first) =
            call(&state, "POST", uri, Some("admin"), None, Some("k1")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("idempotency-replayed").is_none());
        let (status, headers, second) =
            call(&state, "POST", uri, Some("admin"), None, Some("k1")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("idempotency-replayed").unwrap(), "true");
        assert_eq!(first, second);
        assert_eq!(entries(&audit).await.len(), 1);
    }
}
