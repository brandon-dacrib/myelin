//! The `Homeserver` controller: watches `Homeserver`s in one namespace, and what they own
//! (StatefulSets, Services, ConfigMaps, disruption budgets) and their pods by label, and runs
//! [`Reconciler::reconcile`] on each change, timing it into
//! `hs_operator_reconcile_duration_seconds` and counting failures into
//! `hs_operator_reconcile_errors_total`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::chrono::Utc;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, Client, ResourceExt as _};

use super::admin::HttpAdminApi;
use super::kube_ops::KubeHomeserverOps;
use super::objects::APP_NAME;
use super::reconciler::{ReconcileError, Reconciler};
use crate::bridge::MANAGER;
use crate::crds::Homeserver;
use crate::metrics::OperatorMetrics;

/// How soon a failed reconcile is retried.
pub const REQUEUE_AFTER_ERROR: Duration = Duration::from_secs(30);

type Ctx = Reconciler<KubeHomeserverOps, HttpAdminApi>;

/// Runs the `Homeserver` controller in `namespace` until SIGTERM or Ctrl-C.
///
/// # Errors
/// When the admin API client cannot be built.
pub async fn run(
    client: Client,
    namespace: String,
    metrics: OperatorMetrics,
) -> Result<(), ReconcileError> {
    let reconciler = Reconciler {
        kube: KubeHomeserverOps::new(client.clone()),
        admin: HttpAdminApi::new()?,
        metrics,
    };
    let homeservers: Api<Homeserver> = Api::namespaced(client.clone(), &namespace);
    let owned =
        || watcher::Config::default().labels(&format!("app.kubernetes.io/managed-by={MANAGER}"));
    let pods = watcher::Config::default().labels(&format!(
        "app.kubernetes.io/name={APP_NAME},app.kubernetes.io/managed-by={MANAGER}"
    ));
    let pod_namespace = namespace.clone();
    tracing::info!(namespace = %namespace, "homeserver operator starting");
    Controller::new(homeservers, watcher::Config::default())
        .owns(
            Api::<StatefulSet>::namespaced(client.clone(), &namespace),
            owned(),
        )
        .owns(
            Api::<Service>::namespaced(client.clone(), &namespace),
            owned(),
        )
        .owns(
            Api::<ConfigMap>::namespaced(client.clone(), &namespace),
            owned(),
        )
        .owns(
            Api::<PodDisruptionBudget>::namespaced(client.clone(), &namespace),
            owned(),
        )
        .watches(
            Api::<Pod>::namespaced(client.clone(), &namespace),
            pods,
            move |pod: Pod| {
                pod.labels()
                    .get("app.kubernetes.io/instance")
                    .map(|name| ObjectRef::<Homeserver>::new(name).within(&pod_namespace))
            },
        )
        .shutdown_on_signal()
        .run(reconcile, error_policy, Arc::new(reconciler))
        .for_each(|result| async move {
            match result {
                Ok((object, _)) => tracing::debug!(homeserver = %object.name, "reconciled"),
                Err(e) if crate::controller::is_stale_trigger(&e) => {
                    tracing::debug!(error = %e, "a change to an object of a deleted homeserver");
                }
                Err(e) => tracing::warn!(error = %e, "homeserver reconcile failed"),
            }
        })
        .await;
    tracing::info!(namespace = %namespace, "homeserver operator stopped");
    Ok(())
}

async fn reconcile(hs: Arc<Homeserver>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
    let started = Instant::now();
    let result = ctx.reconcile(&hs, Utc::now()).await;
    let elapsed = started.elapsed().as_secs_f64();
    match result {
        Ok(outcome) => {
            ctx.metrics.record_reconcile("Homeserver", elapsed, None);
            Ok(outcome
                .requeue
                .map_or_else(Action::await_change, Action::requeue))
        }
        Err(e) => {
            ctx.metrics
                .record_reconcile("Homeserver", elapsed, Some(e.metric_reason()));
            Err(e)
        }
    }
}

fn error_policy(hs: Arc<Homeserver>, error: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!(homeserver = %hs.name_any(), error = %error, "homeserver reconcile failed, retrying");
    Action::requeue(REQUEUE_AFTER_ERROR)
}
