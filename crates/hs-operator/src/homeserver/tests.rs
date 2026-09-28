//! The reconciler against an in-memory cluster: [`FakeKube`] keeps what the operator applies
//! and plays the StatefulSet controller (pods created, removed and replaced by ordinal,
//! `partition` honoured, revisions by template hash, a pod ready one tick after it is created),
//! and [`FakeAdmin`] plays the server (a drained replica hands off a few shards per tick, a
//! drained replica whose pod stops stays listed until undrained, `409` when no other replica is
//! active). Every test records each pod the fake StatefulSet controller removes, with the
//! shards its replica still owned at that moment: the property under test is that a drained
//! pod goes only once it owns nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetStatus};
use k8s_openapi::api::core::v1::{Pod, PodCondition, PodStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use k8s_openapi::chrono::{DateTime, TimeZone as _, Utc};
use kube::Resource as _;
use sha2::{Digest as _, Sha256};

use super::admin::{AdminApi, AdminEndpoint, AdminError, ReplicaView, TaskView};
use super::objects::{DesiredObjects, pod_name, pod_ordinal, selector_labels};
use super::reconciler::{
    CONDITION_DRAIN_AVAILABLE, CONDITION_DRAINING, CONDITION_READY, CONDITION_SPEC_VALID,
    FINALIZER, HomeserverKube, Note, Outcome, REVISION_LABEL, Reconciler,
};
use super::testing::{cluster_homeserver, single_node_homeserver};
use crate::crds::{
    DrainReason, DrainTimeoutPolicy, Homeserver, HomeserverStatus, Phase, SecretKeyRef,
};
use crate::metrics::OperatorMetrics;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One pod of the fake StatefulSet.
#[derive(Debug, Clone)]
struct FakePod {
    revision: String,
    ready: bool,
}

/// A pod the fake StatefulSet controller removed, and what its replica still owned then.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Removal {
    pod: String,
    shards: u64,
    drained: bool,
    replaced: bool,
}

#[derive(Debug, Default)]
struct World {
    name: String,
    sts: Option<StatefulSet>,
    generation: i64,
    current_revision: Option<String>,
    pods: BTreeMap<String, FakePod>,
    secrets: BTreeMap<(String, String), String>,
    status: Option<HomeserverStatus>,
    finalizers: Vec<String>,
    events: Vec<Note>,
    pdb: bool,
    removals: Vec<Removal>,
    applied: Vec<(i32, i32)>,
}

#[derive(Debug, Clone, Default)]
struct FakeKube(Arc<Mutex<World>>);

impl HomeserverKube for FakeKube {
    async fn get_stateful_set(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<Option<StatefulSet>, kube::Error> {
        Ok(lock(&self.0).sts.clone())
    }

    async fn list_pods(&self, _namespace: &str, _selector: &str) -> Result<Vec<Pod>, kube::Error> {
        let world = lock(&self.0);
        Ok(world
            .pods
            .iter()
            .map(|(name, p)| {
                let mut labels = selector_labels(&world.name);
                labels.insert(REVISION_LABEL.to_owned(), p.revision.clone());
                Pod {
                    metadata: ObjectMeta {
                        name: Some(name.clone()),
                        labels: Some(labels),
                        ..ObjectMeta::default()
                    },
                    spec: None,
                    status: Some(PodStatus {
                        conditions: Some(vec![PodCondition {
                            type_: "Ready".to_owned(),
                            status: if p.ready { "True" } else { "False" }.to_owned(),
                            ..PodCondition::default()
                        }]),
                        ..PodStatus::default()
                    }),
                }
            })
            .collect())
    }

    async fn secret_value(
        &self,
        _namespace: &str,
        secret: &SecretKeyRef,
    ) -> Result<Option<String>, kube::Error> {
        Ok(lock(&self.0)
            .secrets
            .get(&(secret.name.clone(), secret.key.clone()))
            .cloned())
    }

    async fn apply(&self, _namespace: &str, objects: &DesiredObjects) -> Result<(), kube::Error> {
        let mut world = lock(&self.0);
        let mut sts = objects.stateful_set.clone();
        let spec = sts.spec.clone();
        let changed = world.sts.as_ref().map(|s| &s.spec) != Some(&spec);
        if changed {
            world.generation += 1;
        }
        sts.metadata.generation = Some(world.generation);
        sts.status = world.sts.as_ref().and_then(|s| s.status.clone());
        let replicas = spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
        let partition = partition_of(&sts);
        world.applied.push((replicas, partition));
        world.sts = Some(sts);
        world.pdb = objects.pod_disruption_budget.is_some();
        Ok(())
    }

    async fn patch_status(
        &self,
        _namespace: &str,
        _name: &str,
        status: &HomeserverStatus,
    ) -> Result<(), kube::Error> {
        lock(&self.0).status = Some(status.clone());
        Ok(())
    }

    async fn set_finalizer(&self, _hs: &Homeserver, present: bool) -> Result<(), kube::Error> {
        let mut world = lock(&self.0);
        world.finalizers.retain(|f| f != FINALIZER);
        if present {
            world.finalizers.push(FINALIZER.to_owned());
        }
        Ok(())
    }

    async fn publish(&self, _hs: &Homeserver, note: &Note) {
        lock(&self.0).events.push(note.clone());
    }
}

fn partition_of(sts: &StatefulSet) -> i32 {
    sts.spec
        .as_ref()
        .and_then(|s| s.update_strategy.as_ref())
        .and_then(|u| u.rolling_update.as_ref())
        .and_then(|r| r.partition)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
struct FakeReplica {
    pod: String,
    shards: u64,
    drain_requested: bool,
}

#[derive(Debug, Default)]
struct Server {
    replicas: BTreeMap<String, FakeReplica>,
    stuck: BTreeSet<String>,
    refuse_next: u32,
    shards_per_tick: u64,
    calls: Vec<String>,
}

impl Server {
    fn view(&self, id: &str) -> Option<ReplicaView> {
        self.replicas.get(id).map(|r| ReplicaView {
            id: id.to_owned(),
            status: match (r.drain_requested, r.shards) {
                (false, _) => "active",
                (true, 0) => "drained",
                (true, _) => "draining",
            }
            .to_owned(),
            shard_count: r.shards,
            drain_task_id: r.drain_requested.then(|| format!("task-{}", r.pod)),
            drain_requested_at: r.drain_requested.then(|| "2026-09-28T00:00:00Z".to_owned()),
        })
    }
}

#[derive(Debug, Clone, Default)]
struct FakeAdmin(Arc<Mutex<Server>>);

impl AdminApi for FakeAdmin {
    async fn list_replicas(&self, _e: &AdminEndpoint) -> Result<Vec<ReplicaView>, AdminError> {
        let server = lock(&self.0);
        Ok(server
            .replicas
            .keys()
            .filter_map(|id| server.view(id))
            .collect())
    }

    async fn drain(&self, _e: &AdminEndpoint, id: &str) -> Result<ReplicaView, AdminError> {
        let mut server = lock(&self.0);
        server.calls.push(format!("drain {}", short(id)));
        if !server.replicas.contains_key(id) {
            return Err(AdminError::NotFound);
        }
        if server.refuse_next > 0 {
            server.refuse_next -= 1;
            return Err(AdminError::Conflict(
                "no other replica is active to take its shards".to_owned(),
            ));
        }
        let others_active = server
            .replicas
            .iter()
            .any(|(other, r)| other != id && !r.drain_requested);
        if !others_active {
            return Err(AdminError::Conflict(
                "no other replica is active".to_owned(),
            ));
        }
        if let Some(r) = server.replicas.get_mut(id) {
            r.drain_requested = true;
        }
        server.view(id).ok_or(AdminError::NotFound)
    }

    async fn undrain(&self, _e: &AdminEndpoint, id: &str) -> Result<ReplicaView, AdminError> {
        let mut server = lock(&self.0);
        server.calls.push(format!("undrain {}", short(id)));
        let Some(r) = server.replicas.get_mut(id) else {
            return Err(AdminError::NotFound);
        };
        r.drain_requested = false;
        server.view(id).ok_or(AdminError::NotFound)
    }

    async fn task(&self, _e: &AdminEndpoint, task_id: &str) -> Result<TaskView, AdminError> {
        let server = lock(&self.0);
        let pod = task_id.trim_start_matches("task-");
        let r = server.replicas.values().find(|r| r.pod == pod);
        Ok(TaskView {
            id: task_id.to_owned(),
            status: match r {
                Some(r) if r.drain_requested && r.shards > 0 => "running",
                Some(r) if r.drain_requested => "succeeded",
                _ => "cancelled",
            }
            .to_owned(),
        })
    }
}

/// The pod name out of a replica id, for readable call logs.
fn short(id: &str) -> &str {
    id.split('.').next().unwrap_or(id)
}

/// The simulated cluster, the reconciler, and the resource being reconciled.
struct Sim {
    kube: FakeKube,
    admin: FakeAdmin,
    reconciler: Reconciler<FakeKube, FakeAdmin>,
    hs: Homeserver,
    now: DateTime<Utc>,
}

const TICK_SECS: i64 = 5;
const SHARDS_PER_REPLICA: u64 = 12;

impl Sim {
    fn new(hs: Homeserver) -> Self {
        let kube = FakeKube::default();
        let admin = FakeAdmin::default();
        {
            let mut world = lock(&kube.0);
            world.name = hs.metadata.name.clone().unwrap_or_default();
            world.secrets.insert(
                ("hs-admin".to_owned(), "token".to_owned()),
                "operator-token".to_owned(),
            );
        }
        lock(&admin.0).shards_per_tick = 5;
        let reconciler = Reconciler {
            kube: kube.clone(),
            admin: admin.clone(),
            metrics: OperatorMetrics::default(),
        };
        Self {
            kube,
            admin,
            reconciler,
            hs,
            now: Utc
                .with_ymd_and_hms(2026, 9, 28, 12, 0, 0)
                .single()
                .unwrap_or_default(),
        }
    }

    fn world(&self) -> MutexGuard<'_, World> {
        lock(&self.kube.0)
    }

    fn server(&self) -> MutexGuard<'_, Server> {
        lock(&self.admin.0)
    }

    /// The resource as the API server would hand it to the reconciler now.
    fn current(&self) -> Homeserver {
        let mut hs = self.hs.clone();
        let world = self.world();
        hs.status = world.status.clone();
        hs.meta_mut().finalizers = Some(world.finalizers.clone()).filter(|f| !f.is_empty());
        hs
    }

    /// Edits the spec, bumping the generation as the API server would.
    fn edit(&mut self, f: impl FnOnce(&mut Homeserver)) {
        f(&mut self.hs);
        let meta = self.hs.meta_mut();
        meta.generation = Some(meta.generation.unwrap_or(1) + 1);
    }

    /// One reconcile, then one tick of the controllers and the clock.
    fn step(&mut self) -> Outcome {
        let hs = self.current();
        let outcome = block_on(self.reconciler.reconcile(&hs, self.now)).expect("reconcile");
        self.tick();
        outcome
    }

    /// Steps until the resource is Ready and steady, or panics after `max` steps.
    fn settle(&mut self, max: usize) -> usize {
        for i in 0..max {
            let outcome = self.step();
            if outcome.status.phase == Phase::Ready
                && outcome.status.drain.is_none()
                && outcome.status.pending_undrains.is_empty()
            {
                return i + 1;
            }
        }
        panic!(
            "did not settle in {max} steps: status {:#?}\nevents {:#?}\ncalls {:?}",
            self.world().status,
            self.world().events,
            self.server().calls
        );
    }

    fn replica_id(&self, pod: &str) -> String {
        super::objects::replica_id(pod, &self.world().name, "matrix", &self.hs.spec)
    }

    /// The StatefulSet controller and the server, one tick.
    fn tick(&mut self) {
        self.now += k8s_openapi::chrono::Duration::seconds(TICK_SECS);
        let mut world = self.world();
        let mut server = self.server();
        let name = world.name.clone();
        let Some(sts) = world.sts.clone() else { return };
        let spec = sts.spec.clone().unwrap_or_default();
        let template = serde_json::to_vec(&spec.template).unwrap_or_default();
        let update_revision = format!("{name}-{}", hex::encode(&Sha256::digest(&template)[..4]));
        let current_revision = world
            .current_revision
            .get_or_insert_with(|| update_revision.clone())
            .clone();
        let replicas = spec.replicas.unwrap_or(1);
        let partition = partition_of(&sts);

        // Remove pods above the replica count, and replace stale pods at or above the partition.
        let mut removed = Vec::new();
        for (pod, p) in &world.pods {
            let ordinal = pod_ordinal(&name, pod).unwrap_or(0);
            let scale_down = ordinal >= replicas;
            let replace = !scale_down && ordinal >= partition && p.revision != update_revision;
            if scale_down || replace {
                removed.push((pod.clone(), replace));
            }
        }
        for (pod, replaced) in removed {
            world.pods.remove(&pod);
            let id = super::objects::replica_id(&pod, &name, "matrix", &self.hs.spec);
            let (shards, drained) = server
                .replicas
                .get(&id)
                .map_or((0, false), |r| (r.shards, r.drain_requested));
            world.removals.push(Removal {
                pod: pod.clone(),
                shards,
                drained,
                replaced,
            });
            // A stopped replica deregisters, unless a drain request keeps it listed.
            if drained {
                if let Some(r) = server.replicas.get_mut(&id) {
                    r.shards = 0;
                }
            } else {
                server.replicas.remove(&id);
            }
        }
        // Pods created last tick become ready; missing pods are created.
        for p in world.pods.values_mut() {
            p.ready = true;
        }
        for ordinal in 0..replicas {
            let pod = pod_name(&name, ordinal);
            if !world.pods.contains_key(&pod) {
                let revision = if ordinal >= partition {
                    update_revision.clone()
                } else {
                    current_revision.clone()
                };
                world.pods.insert(
                    pod.clone(),
                    FakePod {
                        revision,
                        ready: false,
                    },
                );
                if super::objects::is_cluster(&self.hs.spec) {
                    let id = super::objects::replica_id(&pod, &name, "matrix", &self.hs.spec);
                    server.replicas.entry(id).or_insert(FakeReplica {
                        pod,
                        shards: 0,
                        drain_requested: false,
                    });
                }
            }
        }
        // A stopped replica with no drain request is no longer listed.
        let live: BTreeSet<String> = world.pods.keys().cloned().collect();
        server
            .replicas
            .retain(|_, r| live.contains(&r.pod) || r.drain_requested);
        // The server: drained replicas hand off, active ready ones hold their share.
        let per_tick = server.shards_per_tick;
        let stuck = server.stuck.clone();
        let ready: BTreeSet<String> = world
            .pods
            .iter()
            .filter(|(_, p)| p.ready)
            .map(|(n, _)| n.clone())
            .collect();
        for r in server.replicas.values_mut() {
            if r.drain_requested {
                if !stuck.contains(&r.pod) {
                    r.shards = r.shards.saturating_sub(per_tick);
                }
            } else if ready.contains(&r.pod) {
                r.shards = SHARDS_PER_REPLICA;
            }
        }
        if world.pods.values().all(|p| p.revision == update_revision) {
            world.current_revision = Some(update_revision.clone());
        }
        let generation = world.generation;
        let pods = world.pods.clone();
        let current_revision = world.current_revision.clone();
        if let Some(sts) = world.sts.as_mut() {
            sts.status = Some(StatefulSetStatus {
                observed_generation: Some(generation),
                replicas: i32::try_from(pods.len()).unwrap_or(0),
                ready_replicas: Some(
                    i32::try_from(pods.values().filter(|p| p.ready).count()).unwrap_or(0),
                ),
                updated_replicas: Some(
                    i32::try_from(
                        pods.values()
                            .filter(|p| p.revision == update_revision)
                            .count(),
                    )
                    .unwrap_or(0),
                ),
                update_revision: Some(update_revision),
                current_revision,
                ..StatefulSetStatus::default()
            });
        }
    }

    fn events(&self, reason: &str) -> Vec<Note> {
        self.world()
            .events
            .iter()
            .filter(|n| n.reason == reason)
            .cloned()
            .collect()
    }

    fn pods(&self) -> Vec<String> {
        self.world().pods.keys().cloned().collect()
    }

    fn sts_scale(&self) -> (i32, i32) {
        let world = self.world();
        let sts = world.sts.as_ref().expect("a StatefulSet");
        (
            sts.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0),
            partition_of(sts),
        )
    }

    fn condition(&self, type_: &str) -> Option<(String, String)> {
        self.world().status.as_ref().and_then(|s| {
            s.conditions
                .iter()
                .find(|c| c.type_ == type_)
                .map(|c| (c.status.clone(), c.reason.clone()))
        })
    }

    /// Every pod removed while its replica still owned shards, when it had been drained.
    fn evicted_while_owning(&self) -> Vec<Removal> {
        self.world()
            .removals
            .iter()
            .filter(|r| r.drained && r.shards > 0)
            .cloned()
            .collect()
    }
}

/// Runs a future to completion on a fresh single-threaded runtime (the fakes never wait).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(f)
}

/// A cluster of `replicas`, created and settled.
fn ready(replicas: i32) -> Sim {
    let mut sim = Sim::new(cluster_homeserver("hs", replicas));
    sim.settle(20);
    sim
}

#[test]
fn create_applies_everything_and_reaches_ready() {
    let mut sim = Sim::new(cluster_homeserver("hs", 3));
    let first = sim.step();
    assert_eq!(first.applied, Some((3, 0)), "created at full size");
    assert_eq!(sim.events("Created").len(), 1);
    assert!(sim.world().finalizers.contains(&FINALIZER.to_owned()));
    assert!(sim.world().pdb);
    sim.settle(10);
    assert_eq!(sim.pods(), ["hs-0", "hs-1", "hs-2"]);
    assert_eq!(sim.sts_scale(), (3, 3), "the partition is held at the top");
    assert_eq!(
        sim.condition(CONDITION_READY),
        Some(("True".to_owned(), "AllReplicasReady".to_owned()))
    );
    assert_eq!(
        sim.condition(CONDITION_DRAIN_AVAILABLE),
        Some(("True".to_owned(), "AdminApiConfigured".to_owned()))
    );
    assert_eq!(
        sim.condition(CONDITION_SPEC_VALID),
        Some(("True".to_owned(), "Valid".to_owned()))
    );
    let status = sim.world().status.clone().unwrap();
    assert_eq!(status.ready_replicas, Some(3));
    assert_eq!(status.observed_generation, Some(1));
    assert!(sim.server().calls.is_empty(), "nothing drained on create");
}

#[test]
fn scale_up_adds_pods_without_draining() {
    let mut sim = ready(3);
    sim.edit(|hs| hs.spec.replicas = 5);
    let outcome = sim.step();
    assert_eq!(outcome.applied.map(|a| a.0), Some(5));
    assert_eq!(sim.events("ScalingUp").len(), 1);
    sim.settle(10);
    assert_eq!(sim.pods().len(), 5);
    assert!(sim.server().calls.is_empty());
    assert_eq!(sim.sts_scale(), (5, 5));
}

#[test]
fn scale_down_drains_each_replica_before_its_pod_goes() {
    let mut sim = ready(3);
    sim.edit(|hs| hs.spec.replicas = 1);

    // The first step drains hs-2 and keeps three replicas.
    let outcome = sim.step();
    assert_eq!(outcome.applied, Some((3, 3)));
    let drain = outcome.status.drain.clone().expect("a drain in flight");
    assert_eq!(drain.pod, "hs-2");
    assert_eq!(drain.reason, DrainReason::ScaleDown);
    assert_eq!(drain.replica_id, sim.replica_id("hs-2"));
    assert_eq!(drain.task_id.as_deref(), Some("task-hs-2"));
    assert_eq!(
        sim.condition(CONDITION_DRAINING),
        Some(("True".to_owned(), "DrainInProgress".to_owned()))
    );
    assert_eq!(sim.reconciler.metrics.in_flight(), 1);
    assert!(
        sim.pods().contains(&"hs-2".to_owned()),
        "not evicted while draining"
    );

    sim.settle(30);
    assert_eq!(sim.pods(), ["hs-0"]);
    assert_eq!(sim.sts_scale(), (1, 1));
    assert!(
        sim.evicted_while_owning().is_empty(),
        "{:?}",
        sim.world().removals
    );
    let calls = sim.server().calls.clone();
    assert_eq!(
        calls,
        ["drain hs-2", "undrain hs-2", "drain hs-1", "undrain hs-1"],
        "one at a time, highest first, each undrained once its pod is gone"
    );
    assert!(sim.server().replicas.values().all(|r| !r.drain_requested));
    assert_eq!(sim.events("Draining").len(), 2);
    assert_eq!(sim.events("Drained").len(), 2);
    assert_eq!(sim.events("Undrained").len(), 2);
    assert_eq!(sim.reconciler.metrics.in_flight(), 0);
    assert_eq!(sim.reconciler.metrics.drain_count("completed"), 2);
    assert!(!sim.world().pdb, "one replica needs no disruption budget");
    assert_eq!(
        sim.condition(CONDITION_DRAINING),
        Some(("False".to_owned(), "NoDrain".to_owned()))
    );
}

#[test]
fn a_drain_that_times_out_lets_the_pod_go_by_default() {
    let mut sim = ready(2);
    sim.edit(|hs| {
        hs.spec.replicas = 1;
        hs.spec.drain.timeout_seconds = 30;
    });
    sim.server().stuck.insert("hs-1".to_owned());
    let outcome = sim.step();
    assert!(outcome.status.drain.is_some());
    // Within the timeout the pod stays.
    for _ in 0..5 {
        sim.step();
        assert!(sim.pods().contains(&"hs-1".to_owned()));
    }
    sim.settle(10);
    assert_eq!(sim.pods(), ["hs-0"]);
    let timed_out = sim.events("DrainTimedOut");
    assert_eq!(timed_out.len(), 1);
    assert!(timed_out[0].warning);
    assert_eq!(sim.reconciler.metrics.drain_count("timed_out"), 1);
    let removal = sim.world().removals[0].clone();
    assert_eq!(removal.pod, "hs-1");
    assert!(
        removal.shards > 0,
        "it went still owning shards, as documented"
    );
    assert!(sim.server().calls.contains(&"undrain hs-1".to_owned()));
}

#[test]
fn a_drain_that_times_out_holds_the_pod_with_hold() {
    let mut sim = ready(2);
    sim.edit(|hs| {
        hs.spec.replicas = 1;
        hs.spec.drain.timeout_seconds = 20;
        hs.spec.drain.on_timeout = DrainTimeoutPolicy::Hold;
    });
    sim.server().stuck.insert("hs-1".to_owned());
    for _ in 0..12 {
        sim.step();
    }
    assert_eq!(sim.pods(), ["hs-0", "hs-1"], "held");
    assert_eq!(sim.sts_scale().0, 2);
    let status = sim.world().status.clone().unwrap();
    assert!(status.drain.as_ref().unwrap().timed_out);
    assert_eq!(status.phase, Phase::Degraded);
    assert_eq!(
        sim.condition(CONDITION_DRAINING),
        Some(("True".to_owned(), "DrainTimedOut".to_owned()))
    );
    assert_eq!(
        sim.events("DrainTimedOut").len(),
        1,
        "warned once, not every step"
    );

    // The replica gets unstuck: the drain completes and the pod goes.
    sim.server().stuck.clear();
    sim.settle(10);
    assert_eq!(sim.pods(), ["hs-0"]);
    assert!(sim.evicted_while_owning().is_empty());
}

#[test]
fn raising_replicas_during_a_scale_down_aborts_the_drain() {
    let mut sim = ready(3);
    sim.edit(|hs| hs.spec.replicas = 2);
    sim.server().stuck.insert("hs-2".to_owned());
    sim.step();
    sim.step();
    assert!(sim.world().status.as_ref().unwrap().drain.is_some());
    sim.edit(|hs| hs.spec.replicas = 3);
    let outcome = sim.step();
    assert!(outcome.status.drain.is_none());
    assert_eq!(outcome.applied.map(|a| a.0), Some(3));
    assert_eq!(sim.events("DrainAborted").len(), 1);
    assert_eq!(sim.server().calls, ["drain hs-2", "undrain hs-2"]);
    assert!(!sim.server().replicas[&sim.replica_id("hs-2")].drain_requested);
    sim.server().stuck.clear();
    sim.settle(10);
    assert_eq!(sim.pods(), ["hs-0", "hs-1", "hs-2"]);
    assert!(sim.world().removals.is_empty(), "nothing was evicted");
    assert_eq!(sim.reconciler.metrics.drain_count("aborted"), 1);
}

#[test]
fn a_rolling_update_drains_and_replaces_one_pod_at_a_time() {
    let mut sim = ready(3);
    let before: BTreeMap<String, String> = sim
        .world()
        .pods
        .iter()
        .map(|(n, p)| (n.clone(), p.revision.clone()))
        .collect();
    sim.edit(|hs| hs.spec.image.tag = Some("sha-def".to_owned()));

    // Applying the new template replaces nothing by itself.
    let outcome = sim.step();
    assert_eq!(outcome.applied, Some((3, 3)));
    assert!(sim.world().removals.is_empty());

    sim.settle(60);
    let removals = sim.world().removals.clone();
    assert_eq!(
        removals.iter().map(|r| r.pod.as_str()).collect::<Vec<_>>(),
        ["hs-2", "hs-1", "hs-0"],
        "highest ordinal first"
    );
    assert!(
        removals
            .iter()
            .all(|r| r.replaced && r.drained && r.shards == 0)
    );
    let after: BTreeMap<String, String> = sim
        .world()
        .pods
        .iter()
        .map(|(n, p)| (n.clone(), p.revision.clone()))
        .collect();
    assert!(
        before.iter().all(|(n, r)| after[n] != *r),
        "every pod replaced"
    );
    assert_eq!(
        sim.server().calls,
        [
            "drain hs-2",
            "undrain hs-2",
            "drain hs-1",
            "undrain hs-1",
            "drain hs-0",
            "undrain hs-0"
        ]
    );
    assert!(sim.server().replicas.values().all(|r| !r.drain_requested));
    assert_eq!(sim.sts_scale(), (3, 3));
    assert_eq!(
        sim.condition(CONDITION_READY),
        Some(("True".to_owned(), "AllReplicasReady".to_owned()))
    );
}

#[test]
fn reverting_a_template_during_a_rolling_update_aborts_the_drain() {
    let mut sim = ready(3);
    let original = sim.hs.spec.image.tag.clone();
    sim.edit(|hs| hs.spec.image.tag = Some("sha-def".to_owned()));
    sim.server().stuck.insert("hs-2".to_owned());
    sim.step(); // apply the template, frozen
    sim.step(); // drain hs-2
    assert_eq!(
        sim.world()
            .status
            .as_ref()
            .unwrap()
            .drain
            .as_ref()
            .map(|d| d.pod.clone()),
        Some("hs-2".to_owned())
    );
    sim.edit(|hs| hs.spec.image.tag = original);
    sim.step(); // the reverted template is applied; the drain waits for the new revision
    sim.step(); // the pod is at the update revision again: no longer needed
    assert!(sim.world().status.as_ref().unwrap().drain.is_none());
    assert_eq!(sim.events("DrainAborted").len(), 1);
    assert_eq!(sim.server().calls, ["drain hs-2", "undrain hs-2"]);
    sim.server().stuck.clear();
    sim.settle(10);
    assert!(sim.world().removals.is_empty(), "no pod was replaced");
}

#[test]
fn a_refused_drain_is_reported_and_retried() {
    let mut sim = ready(2);
    sim.server().refuse_next = 2;
    sim.edit(|hs| hs.spec.replicas = 1);
    sim.step();
    sim.step();
    assert_eq!(
        sim.pods(),
        ["hs-0", "hs-1"],
        "a refused drain keeps the pod"
    );
    assert_eq!(sim.events("DrainRefused").len(), 2);
    assert!(sim.events("DrainRefused")[0].warning);
    sim.settle(10);
    assert_eq!(sim.pods(), ["hs-0"]);
    assert!(sim.evicted_while_owning().is_empty());
    assert_eq!(sim.reconciler.metrics.drain_count("refused"), 2);
}

#[test]
fn without_an_admin_api_pods_go_without_a_drain_and_the_condition_says_so() {
    let mut hs = cluster_homeserver("hs", 3);
    hs.spec.admin_api = None;
    let mut sim = Sim::new(hs);
    sim.settle(10);
    assert!(
        sim.world().finalizers.is_empty(),
        "nothing to clean up, no finalizer"
    );
    assert_eq!(
        sim.condition(CONDITION_DRAIN_AVAILABLE),
        Some(("False".to_owned(), "NoAdminApi".to_owned()))
    );
    sim.edit(|hs| hs.spec.replicas = 2);
    let outcome = sim.step();
    assert_eq!(outcome.applied.map(|a| a.0), Some(2));
    assert_eq!(sim.events("ScaledDownWithoutDrain").len(), 1);
    assert!(sim.server().calls.is_empty());
}

#[test]
fn a_missing_token_secret_is_a_condition_not_an_error() {
    let mut sim = Sim::new(cluster_homeserver("hs", 2));
    sim.world().secrets.clear();
    sim.settle(10);
    assert_eq!(
        sim.condition(CONDITION_DRAIN_AVAILABLE),
        Some(("False".to_owned(), "TokenSecretMissing".to_owned()))
    );
}

#[test]
fn a_single_node_rolls_without_a_drain() {
    let mut sim = Sim::new(single_node_homeserver("hs"));
    sim.settle(10);
    assert_eq!(sim.sts_scale(), (1, 1));
    assert_eq!(
        sim.condition(CONDITION_DRAIN_AVAILABLE),
        Some(("False".to_owned(), "SingleNode".to_owned()))
    );
    sim.edit(|hs| hs.spec.image.tag = Some("sha-def".to_owned()));
    sim.settle(10);
    assert_eq!(sim.world().removals.len(), 1);
    assert!(sim.world().removals[0].replaced);
    assert!(sim.server().calls.is_empty());
    assert_eq!(sim.sts_scale(), (1, 1));
}

#[test]
fn an_invalid_spec_is_degraded_and_applies_nothing() {
    let mut hs = cluster_homeserver("hs", 3);
    hs.spec.cluster.mesh_tls_secret = None;
    let mut sim = Sim::new(hs);
    let outcome = sim.step();
    assert_eq!(outcome.applied, None);
    assert_eq!(outcome.status.phase, Phase::Degraded);
    assert!(sim.world().sts.is_none());
    assert_eq!(
        sim.condition(CONDITION_SPEC_VALID),
        Some(("False".to_owned(), "InvalidSpec".to_owned()))
    );
    sim.step();
    assert_eq!(sim.events("InvalidSpec").len(), 1, "warned once");
}

#[test]
fn deleting_mid_drain_undrains_and_releases_the_finalizer() {
    let mut sim = ready(3);
    sim.edit(|hs| hs.spec.replicas = 2);
    sim.server().stuck.insert("hs-2".to_owned());
    sim.step();
    assert!(sim.world().status.as_ref().unwrap().drain.is_some());
    sim.hs.meta_mut().deletion_timestamp = Some(Time(sim.now));
    let outcome = sim.step();
    assert_eq!(outcome.requeue, None);
    assert!(sim.world().finalizers.is_empty());
    assert_eq!(sim.server().calls, ["drain hs-2", "undrain hs-2"]);
    assert_eq!(sim.reconciler.metrics.in_flight(), 0);
}

#[test]
fn a_replica_undrained_by_hand_is_drained_again() {
    let mut sim = ready(2);
    sim.edit(|hs| hs.spec.replicas = 1);
    sim.server().stuck.insert("hs-1".to_owned());
    sim.step();
    let id = sim.replica_id("hs-1");
    if let Some(r) = sim.server().replicas.get_mut(&id) {
        r.drain_requested = false;
        r.shards = SHARDS_PER_REPLICA;
    }
    sim.step();
    assert_eq!(sim.events("DrainReissued").len(), 1);
    assert!(sim.server().replicas[&id].drain_requested);
    sim.server().stuck.clear();
    sim.settle(10);
    assert_eq!(sim.pods(), ["hs-0"]);
}

#[test]
fn every_status_the_reconciler_writes_is_valid_crd_json() {
    let mut sim = ready(2);
    sim.edit(|hs| hs.spec.replicas = 1);
    sim.step();
    let status = sim.world().status.clone().unwrap();
    let json = serde_json::to_value(&status).unwrap();
    let back: HomeserverStatus = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(back, status);
    assert_eq!(json["drain"]["reason"], "ScaleDown");
    assert!(
        json["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["lastTransitionTime"].is_string())
    );
}
