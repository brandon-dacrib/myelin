//! The cluster's replicas and shards as the admin API shows them:
//! [`hs_admin::cluster::ClusterSource`] over `hs-cluster`'s replica registry, its shard rows and
//! its drain requests, all in the shared store every replica reads.
//!
//! A server not running as a cluster (`cluster.single_node`, the default) is a cluster of one:
//! this process, owning every shard of the layout, with no registry, no heartbeats and nothing
//! to drain to.
//!
//! # Draining
//!
//! A drain is a row in the shared store ([`ClusterStore::request_drain`]), not a message to a
//! process, so whichever replica the administrator's request reaches can record it, and the
//! drained replica's own ownership manager honours it at its next heartbeat: it reports itself
//! `Draining`, the others stop hashing shards onto it, and it releases each shard for them to
//! take (RFC 0001 section 10, without the shutdown at the end). Undraining deletes the row. A
//! drained replica that is stopped leaves the registry (its shutdown deregisters it) but its
//! drain request stays, so it is still listed, `drained`, and comes back drained; that is what
//! lets an operator take a replica out of service across a restart or a node replacement.
//!
//! # Metrics
//!
//! [`DrainMetrics`] counts drains into the shared Prometheus registry:
//! `hs_cluster_admin_drains_total{event}` (`requested`, `completed`, `timed_out`, `undrained`)
//! and `hs_cluster_admin_drain_duration_seconds`, from the request to the replica owning
//! nothing.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use hs_admin::cluster::{
    ClusterSource, DrainEvent, DrainFollow, DrainStarted, Replica, ReplicaStatus, Shard, Undrained,
    unknown_kind,
};
use hs_admin::sources::SourceError;
use hs_cluster::store::ClusterStore;
use hs_cluster::types::{ReplicaRecord, ReplicaState, ShardRecord};
use hs_cluster::{DrainRequest, ReplicaId, ShardId, ShardKind, ShardLayout};
use hs_kv::KvBackend;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};

/// The drain metric families (see the module docs).
#[derive(Clone)]
pub struct DrainMetrics {
    drains: Family<DrainLabels, Counter>,
    duration: Histogram,
}

/// The `event` label of `hs_cluster_admin_drains_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct DrainLabels {
    event: String,
}

impl DrainMetrics {
    /// Registers the families into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let drains = Family::<DrainLabels, Counter>::default();
        // From a second (a handful of shards) to an hour (a large replica on a slow store).
        let duration = Histogram::new(exponential_buckets(0.5, 2.0, 14));
        metrics.with_registry(|registry| {
            // Registered without `_total`: the text encoder appends it.
            registry.register(
                "hs_cluster_admin_drains",
                "Replica drains asked for through the admin API, by event: requested, completed, \
                 timed_out, undrained",
                drains.clone(),
            );
            registry.register(
                "hs_cluster_admin_drain_duration_seconds",
                "From an administrator's drain request to the replica owning no shards",
                duration.clone(),
            );
        });
        Self { drains, duration }
    }

    fn count(&self, event: &str) {
        self.drains
            .get_or_create(&DrainLabels {
                event: event.to_owned(),
            })
            .inc();
    }
}

/// [`ClusterSource`] for a running `hs serve`.
pub struct ClusterAdmin<B: KvBackend> {
    me: ReplicaId,
    generation: u64,
    layout: ShardLayout,
    /// The shared store, when clustered. `None` in single-node mode.
    store: Option<ClusterStore<B>>,
    /// A replica whose last heartbeat is older than this is `unreachable`.
    lease_ttl: Duration,
    follow: DrainFollow,
    metrics: Option<DrainMetrics>,
}

impl<B: KvBackend + 'static> ClusterAdmin<B> {
    /// A server not running as a cluster: `me`, owning all of `layout`.
    #[must_use]
    pub fn single_node(me: ReplicaId, generation: u64, layout: ShardLayout) -> Self {
        Self {
            me,
            generation,
            layout,
            store: None,
            lease_ttl: Duration::from_secs(3),
            follow: DrainFollow::default(),
            metrics: None,
        }
    }

    /// A replica of a cluster whose registry is `store`.
    #[must_use]
    pub fn clustered(
        me: ReplicaId,
        generation: u64,
        layout: ShardLayout,
        store: ClusterStore<B>,
        lease_ttl: Duration,
    ) -> Self {
        Self {
            me,
            generation,
            layout,
            store: Some(store),
            lease_ttl,
            follow: DrainFollow::default(),
            metrics: None,
        }
    }

    /// Counts drains into `metrics`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: DrainMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Follows drains this way instead of [`DrainFollow::default`].
    #[must_use]
    pub fn with_follow(mut self, follow: DrainFollow) -> Self {
        self.follow = follow;
        self
    }

    fn single_replica(&self) -> Replica {
        Replica {
            id: self.me.as_str().to_owned(),
            role: "single-node".to_owned(),
            status: ReplicaStatus::Active,
            shard_count: u64::from(self.layout.total()),
            epoch: self.generation,
            this_replica: true,
            mesh_addr: None,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            zone: None,
            last_heartbeat_at: None,
            drain_requested_at: None,
            drain_requested_by: None,
            drain_task_id: None,
        }
    }

    /// Runs a blocking store read off the runtime.
    async fn blocking<T: Send + 'static>(
        &self,
        what: &'static str,
        f: impl FnOnce(ClusterStore<B>) -> Result<T, hs_cluster::error::ClusterError> + Send + 'static,
    ) -> Result<T, SourceError> {
        let Some(store) = self.store.clone() else {
            return Err(SourceError::Unavailable(format!(
                "{what}: this server is not running as a cluster"
            )));
        };
        tokio::task::spawn_blocking(move || f(store))
            .await
            .map_err(|e| SourceError::Unavailable(format!("{what}: {e}")))?
            .map_err(|e| SourceError::Unavailable(format!("{what}: {e}")))
    }

    /// Registry rows, shard rows and drain requests, read together.
    async fn snapshot(&self) -> Result<Snapshot, SourceError> {
        self.blocking("reading the replica registry", |store| {
            Ok(Snapshot {
                replicas: store.list_replicas()?,
                shards: store.list_shards()?.into_iter().collect(),
                drains: store.list_drain_requests()?.into_iter().collect(),
            })
        })
        .await
    }

    fn view(&self, snapshot: &Snapshot, now_ms: u64) -> Vec<Replica> {
        let mut owned: HashMap<&ReplicaId, u64> = HashMap::new();
        for record in snapshot.shards.values() {
            if let Some((owner, _)) = &record.owner {
                *owned.entry(owner).or_default() += 1;
            }
        }
        let mut replicas: BTreeMap<String, Replica> = BTreeMap::new();
        for record in &snapshot.replicas {
            let shard_count = owned.get(&record.id).copied().unwrap_or(0);
            let drain = snapshot.drains.get(&record.id);
            replicas.insert(
                record.id.as_str().to_owned(),
                self.replica_of(record, shard_count, drain, now_ms),
            );
        }
        // A drained replica that was stopped is no longer registered, but its drain request
        // outlives it: it is still listed, drained, so that it can be seen and undrained.
        for (id, drain) in &snapshot.drains {
            if replicas.contains_key(id.as_str()) {
                continue;
            }
            let shard_count = owned.get(id).copied().unwrap_or(0);
            let mut replica = Replica {
                id: id.as_str().to_owned(),
                role: "replica".to_owned(),
                status: ReplicaStatus::Drained,
                shard_count,
                epoch: 0,
                this_replica: *id == self.me,
                mesh_addr: None,
                version: None,
                zone: None,
                last_heartbeat_at: None,
                drain_requested_at: None,
                drain_requested_by: None,
                drain_task_id: None,
            };
            apply_drain(&mut replica, drain);
            if shard_count > 0 {
                replica.status = ReplicaStatus::Draining;
            }
            replicas.insert(replica.id.clone(), replica);
        }
        replicas.into_values().collect()
    }

    fn replica_of(
        &self,
        record: &ReplicaRecord,
        shard_count: u64,
        drain: Option<&DrainRequest>,
        now_ms: u64,
    ) -> Replica {
        let is_me = record.id == self.me;
        // This replica is answering, so it is not unreachable whatever its last row says.
        let stale = !is_me
            && now_ms.saturating_sub(record.heartbeat_unix_ms)
                > u64::try_from(self.lease_ttl.as_millis()).unwrap_or(u64::MAX);
        let status = if stale {
            ReplicaStatus::Unreachable
        } else if drain.is_some() {
            if shard_count == 0 {
                ReplicaStatus::Drained
            } else {
                ReplicaStatus::Draining
            }
        } else {
            match record.state {
                ReplicaState::Joining => ReplicaStatus::Joining,
                ReplicaState::Active => ReplicaStatus::Active,
                ReplicaState::Draining => ReplicaStatus::Draining,
                ReplicaState::Left => ReplicaStatus::Drained,
            }
        };
        let mut replica = Replica {
            id: record.id.as_str().to_owned(),
            role: "replica".to_owned(),
            status,
            shard_count,
            epoch: record.generation.0,
            this_replica: is_me,
            mesh_addr: Some(record.mesh_addr.clone()),
            version: Some(record.version.clone()).filter(|v| !v.is_empty()),
            zone: record.zone.clone(),
            last_heartbeat_at: Some(hs_http::time::rfc3339_from_millis(
                i64::try_from(record.heartbeat_unix_ms).unwrap_or(i64::MAX),
            )),
            drain_requested_at: None,
            drain_requested_by: None,
            drain_task_id: None,
        };
        if let Some(drain) = drain {
            apply_drain(&mut replica, drain);
        }
        replica
    }

    async fn clustered_replicas(&self) -> Result<Vec<Replica>, SourceError> {
        let snapshot = self.snapshot().await?;
        Ok(self.view(&snapshot, unix_now_ms()))
    }
}

/// What [`ClusterAdmin::snapshot`] reads.
struct Snapshot {
    replicas: Vec<ReplicaRecord>,
    shards: HashMap<ShardId, ShardRecord>,
    drains: HashMap<ReplicaId, DrainRequest>,
}

fn apply_drain(replica: &mut Replica, drain: &DrainRequest) {
    replica.drain_requested_at = Some(hs_http::time::rfc3339_from_millis(
        i64::try_from(drain.requested_unix_ms).unwrap_or(i64::MAX),
    ));
    replica.drain_requested_by = Some(drain.requested_by.clone());
    replica.drain_task_id = drain.task_id.clone();
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Whether some replica other than `id` is active and not asked to drain: something to take
/// `id`'s shards.
fn someone_else_serves(replicas: &[Replica], id: &str) -> bool {
    replicas
        .iter()
        .any(|r| r.id != id && r.status == ReplicaStatus::Active && !r.drain_requested())
}

#[async_trait]
impl<B: KvBackend + 'static> ClusterSource for ClusterAdmin<B> {
    async fn replicas(&self) -> Result<Vec<Replica>, SourceError> {
        if self.store.is_none() {
            return Ok(vec![self.single_replica()]);
        }
        self.clustered_replicas().await
    }

    async fn shards(&self, kind: Option<&str>) -> Result<Vec<Shard>, SourceError> {
        let kind = match kind {
            None => None,
            Some(k) => Some(ShardKind::parse(k).ok_or_else(|| unknown_kind(k))?),
        };
        let rows: HashMap<ShardId, ShardRecord> = if self.store.is_some() {
            self.blocking("reading the shard rows", |store| {
                Ok(store.list_shards()?.into_iter().collect())
            })
            .await?
        } else {
            HashMap::new()
        };
        let single_node = self.store.is_none();
        Ok(self
            .layout
            .all_shards()
            .filter(|s| kind.is_none_or(|k| s.kind == k))
            .map(|shard| {
                let row = rows.get(&shard);
                let owner = if single_node {
                    Some(self.me.as_str().to_owned())
                } else {
                    row.and_then(|r| r.owner.as_ref().map(|(id, _)| id.as_str().to_owned()))
                };
                let epoch = row.map_or(0, |r| r.epoch.0);
                let state = match (&owner, row) {
                    (Some(_), _) => "owned",
                    (None, Some(r)) if r.epoch.0 > 0 => "released",
                    _ => "unassigned",
                };
                Shard {
                    kind: shard.kind.as_str().to_owned(),
                    id: shard.to_string(),
                    owner,
                    state: state.to_owned(),
                    epoch,
                }
            })
            .collect())
    }

    async fn drain(&self, id: &str, requested_by: &str) -> Result<DrainStarted, SourceError> {
        if self.store.is_none() {
            if id != self.me.as_str() {
                return Err(SourceError::NotFound);
            }
            return Err(SourceError::Conflict(
                "this server is not running as a cluster: there is no other replica to take its \
                 shards"
                    .into(),
            ));
        }
        let replicas = self.clustered_replicas().await?;
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
        if !someone_else_serves(&replicas, id) {
            return Err(SourceError::Conflict(format!(
                "no other replica is active to take {id}'s shards; start another replica, or \
                 undrain one, first"
            )));
        }
        let replica_id = ReplicaId::new(id);
        let request = DrainRequest {
            requested_unix_ms: unix_now_ms(),
            requested_by: requested_by.to_owned(),
            task_id: None,
        };
        let (_, started) = {
            let replica_id = replica_id.clone();
            self.blocking("recording the drain request", move |store| {
                store.request_drain(&replica_id, &request)
            })
            .await?
        };
        // Two administrators draining the last two active replicas at once would each have seen
        // the other serving. Look again now that this request is written: if nothing is left to
        // take the shards, withdraw it rather than strand them.
        let after = self.clustered_replicas().await?;
        if started && !someone_else_serves(&after, id) {
            let replica_id = replica_id.clone();
            self.blocking("withdrawing the drain request", move |store| {
                store.withdraw_drain(&replica_id)
            })
            .await?;
            return Err(SourceError::Conflict(format!(
                "no other replica is active to take {id}'s shards: another drain was asked for \
                 at the same time"
            )));
        }
        let replica = after
            .into_iter()
            .find(|r| r.id == id)
            .ok_or(SourceError::NotFound)?;
        Ok(DrainStarted { replica, started })
    }

    async fn set_drain_task(&self, id: &str, task_id: &str) -> Result<(), SourceError> {
        let replica_id = ReplicaId::new(id);
        let task_id = task_id.to_owned();
        self.blocking("noting the drain's task", move |store| {
            if let Some(mut request) = store.drain_request(&replica_id)? {
                request.task_id = Some(task_id);
                store.update_drain(&replica_id, &request)?;
            }
            Ok(())
        })
        .await
    }

    async fn undrain(&self, id: &str) -> Result<Undrained, SourceError> {
        if self.store.is_none() {
            if id != self.me.as_str() {
                return Err(SourceError::NotFound);
            }
            return Ok(Undrained {
                replica: self.single_replica(),
                withdrawn: false,
                task_id: None,
            });
        }
        let replica_id = ReplicaId::new(id);
        let registered = self
            .clustered_replicas()
            .await?
            .into_iter()
            .any(|r| r.id == id);
        if !registered {
            return Err(SourceError::NotFound);
        }
        let withdrawn = self
            .blocking("withdrawing the drain request", move |store| {
                store.withdraw_drain(&replica_id)
            })
            .await?;
        // A stopped replica listed only for its drain request is gone from the list now; answer
        // it as it was last seen, active again as far as the cluster is concerned.
        let replica = match self.replica(id).await? {
            Some(replica) => replica,
            None => Replica {
                id: id.to_owned(),
                role: "replica".to_owned(),
                status: ReplicaStatus::Drained,
                shard_count: 0,
                epoch: 0,
                this_replica: false,
                mesh_addr: None,
                version: None,
                zone: None,
                last_heartbeat_at: None,
                drain_requested_at: None,
                drain_requested_by: None,
                drain_task_id: None,
            },
        };
        Ok(Undrained {
            replica,
            withdrawn: withdrawn.is_some(),
            task_id: withdrawn.and_then(|w| w.task_id),
        })
    }

    fn drain_follow(&self) -> DrainFollow {
        self.follow
    }

    fn observe(&self, event: DrainEvent) {
        let Some(metrics) = &self.metrics else {
            return;
        };
        match event {
            DrainEvent::Requested { .. } => metrics.count("requested"),
            DrainEvent::Completed { elapsed, .. } => {
                metrics.count("completed");
                metrics.duration.observe(elapsed.as_secs_f64());
            }
            DrainEvent::TimedOut { .. } => metrics.count("timed_out"),
            DrainEvent::Undrained { .. } => metrics.count("undrained"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_cluster::Generation;
    use hs_kv::memory::MemoryBackend;

    fn single() -> ClusterAdmin<MemoryBackend> {
        ClusterAdmin::single_node(ReplicaId::new("hs-solo"), 7, ShardLayout::small(3))
    }

    #[tokio::test]
    async fn a_single_node_is_one_replica_owning_the_whole_layout_and_cannot_drain() {
        let admin = single();
        let replicas = admin.replicas().await.unwrap();
        assert_eq!(replicas.len(), 1);
        assert_eq!(replicas[0].id, "hs-solo");
        assert_eq!(replicas[0].role, "single-node");
        assert_eq!(replicas[0].status, ReplicaStatus::Active);
        assert_eq!(
            replicas[0].shard_count,
            u64::from(ShardLayout::small(3).total())
        );
        assert_eq!(replicas[0].epoch, 7);
        assert!(replicas[0].this_replica);
        assert!(admin.replica("hs-other").await.unwrap().is_none());

        let shards = admin.shards(None).await.unwrap();
        assert_eq!(shards.len(), ShardLayout::small(3).total() as usize);
        assert!(shards.iter().all(|s| s.owner.as_deref() == Some("hs-solo")));
        assert!(shards.iter().all(|s| s.state == "owned"));
        let rooms = admin.shards(Some("room")).await.unwrap();
        assert_eq!(rooms.len(), 3);
        assert_eq!(rooms[2].id, "room/2");
        assert!(matches!(
            admin.shards(Some("planet")).await,
            Err(SourceError::InvalidField {
                pointer: "/kind",
                ..
            })
        ));

        assert!(matches!(
            admin.drain("hs-solo", "@ops:example.org").await,
            Err(SourceError::Conflict(_))
        ));
        assert!(matches!(
            admin.drain("hs-other", "@ops:example.org").await,
            Err(SourceError::NotFound)
        ));
        let undrained = admin.undrain("hs-solo").await.unwrap();
        assert!(!undrained.withdrawn);
        assert_eq!(undrained.replica.status, ReplicaStatus::Active);
    }

    fn row(id: &str, state: ReplicaState, at: u64) -> ReplicaRecord {
        ReplicaRecord {
            id: ReplicaId::new(id),
            generation: Generation(3),
            mesh_addr: format!("{id}:9000"),
            zone: Some("a".into()),
            version: "0.1.0".into(),
            state,
            heartbeat_seq: 1,
            heartbeat_unix_ms: at,
        }
    }

    #[tokio::test]
    async fn a_clustered_replica_reads_the_registry_rows_and_drain_requests() {
        let backend = MemoryBackend::new();
        let store = ClusterStore::open(backend.clone()).unwrap();
        let layout = ShardLayout::small(2);
        store.init_layout(layout).unwrap();
        let now = unix_now_ms();
        store
            .heartbeat(&row("hs-0", ReplicaState::Active, now))
            .unwrap();
        store
            .heartbeat(&row("hs-1", ReplicaState::Active, now))
            .unwrap();
        store
            .heartbeat(&row("hs-2", ReplicaState::Active, now - 60_000))
            .unwrap();
        for index in 0..2 {
            store
                .acquire_shard(
                    ShardId::new(ShardKind::Room, index),
                    &ReplicaId::new("hs-1"),
                    Generation(3),
                    |_| false,
                )
                .unwrap();
        }
        let admin = ClusterAdmin::clustered(
            ReplicaId::new("hs-0"),
            3,
            layout,
            store.clone(),
            Duration::from_secs(5),
        );
        let replicas = admin.replicas().await.unwrap();
        let by_id = |id: &str| replicas.iter().find(|r| r.id == id).unwrap().clone();
        assert_eq!(by_id("hs-0").status, ReplicaStatus::Active);
        assert!(by_id("hs-0").this_replica);
        assert_eq!(by_id("hs-0").mesh_addr.as_deref(), Some("hs-0:9000"));
        assert_eq!(by_id("hs-0").zone.as_deref(), Some("a"));
        assert_eq!(by_id("hs-1").shard_count, 2);
        assert_eq!(by_id("hs-2").status, ReplicaStatus::Unreachable);

        let rooms = admin.shards(Some("room")).await.unwrap();
        assert_eq!(rooms[1].id, "room/1");
        assert_eq!(rooms[1].owner.as_deref(), Some("hs-1"));
        assert_eq!(rooms[1].state, "owned");
        assert_eq!(rooms[1].epoch, 1);
        let users = admin.shards(Some("user")).await.unwrap();
        assert_eq!(users[0].owner, None);
        assert_eq!(users[0].state, "unassigned");

        // Draining hs-1 is recorded in the store, and it shows as draining while it owns shards.
        let started = admin.drain("hs-1", "@ops:example.org").await.unwrap();
        assert!(started.started);
        assert_eq!(started.replica.status, ReplicaStatus::Draining);
        assert_eq!(
            started.replica.drain_requested_by.as_deref(),
            Some("@ops:example.org")
        );
        assert!(
            store
                .drain_request(&ReplicaId::new("hs-1"))
                .unwrap()
                .is_some()
        );
        admin.set_drain_task("hs-1", "01TASK").await.unwrap();
        assert_eq!(
            admin
                .replica("hs-1")
                .await
                .unwrap()
                .unwrap()
                .drain_task_id
                .as_deref(),
            Some("01TASK")
        );
        // Asking again keeps the first request.
        let again = admin.drain("hs-1", "@other:example.org").await.unwrap();
        assert!(!again.started);
        assert_eq!(
            again.replica.drain_requested_by.as_deref(),
            Some("@ops:example.org")
        );
        // hs-0 is now the only replica serving: it cannot be drained.
        assert!(matches!(
            admin.drain("hs-0", "@ops:example.org").await,
            Err(SourceError::Conflict(_))
        ));

        // Once hs-1 has released its shards it is drained.
        for index in 0..2 {
            store
                .release_shard(
                    ShardId::new(ShardKind::Room, index),
                    &ReplicaId::new("hs-1"),
                    Generation(3),
                )
                .unwrap();
        }
        assert_eq!(
            admin.replica("hs-1").await.unwrap().unwrap().status,
            ReplicaStatus::Drained
        );
        assert_eq!(
            admin.shards(Some("room")).await.unwrap()[0].state,
            "released"
        );

        // Stopped (deregistered), it is still listed, drained, until undrained.
        store
            .remove_replica(&ReplicaId::new("hs-1"), Generation(3))
            .unwrap();
        let listed = admin.replica("hs-1").await.unwrap().unwrap();
        assert_eq!(listed.status, ReplicaStatus::Drained);
        assert_eq!(listed.last_heartbeat_at, None);
        let undrained = admin.undrain("hs-1").await.unwrap();
        assert!(undrained.withdrawn);
        assert_eq!(undrained.task_id.as_deref(), Some("01TASK"));
        assert!(admin.replica("hs-1").await.unwrap().is_none());
        assert!(matches!(
            admin.undrain("hs-1").await,
            Err(SourceError::NotFound)
        ));
    }

    #[test]
    fn drain_metrics_are_exported_under_their_names() {
        let metrics = hs_telemetry::metrics::Metrics::new();
        let admin = single().with_metrics(DrainMetrics::register(&metrics));
        admin.observe(DrainEvent::Requested {
            replica: "hs-1".into(),
        });
        admin.observe(DrainEvent::Completed {
            replica: "hs-1".into(),
            handed_off: 3,
            elapsed: Duration::from_secs(2),
        });
        let text = metrics.encode_to_string().unwrap();
        assert!(
            text.contains("hs_cluster_admin_drains_total{event=\"requested\"} 1"),
            "{text}"
        );
        assert!(text.contains("hs_cluster_admin_drains_total{event=\"completed\"} 1"));
        assert!(text.contains("hs_cluster_admin_drain_duration_seconds_count 1"));
    }
}
