//! The `Bridge` controller `hs operator` runs: watches `Bridge` objects in one namespace and
//! keeps each one's PersistentVolumeClaim, Deployment and Service applied (server-side apply,
//! field manager [`MANAGER`]), then writes what the Deployment and its pods say back into the
//! `Bridge`'s status. `docs/rfcs/0017-the-server-deploys-its-own-bridges.md` section 4.4.
//!
//! It watches what it owns (Deployments, Services, claims) and, by label, the bridge pods
//! themselves, so an image pull failure or a crash loop reaches the status as it happens rather
//! than at the next periodic requeue. While a bridge is not Ready it is also requeued every
//! [`REQUEUE_NOT_READY`] as a backstop.
//!
//! The other kinds' reconcilers in [`crate::reconcile`] are stubs and are not run here.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Service};
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, Client, ResourceExt as _};

use crate::bridge::{
    MANAGER, desired_deployment, desired_pvc, desired_service, pod_selector, status_from,
};
use crate::crds::{Bridge, Phase};
use crate::metrics::OperatorMetrics;

/// How soon a bridge that is not Ready yet is looked at again, besides the watches.
pub const REQUEUE_NOT_READY: Duration = Duration::from_secs(15);
/// How soon a Ready bridge is looked at again, besides the watches.
pub const REQUEUE_READY: Duration = Duration::from_secs(300);
/// How soon a failed reconcile is retried.
pub const REQUEUE_AFTER_ERROR: Duration = Duration::from_secs(30);

/// What a `Bridge` reconcile can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// A call to the Kubernetes API failed.
    #[error("kubernetes API: {0}")]
    Kube(#[from] kube::Error),
    /// The object has no name or namespace (never true of one read from the API server).
    #[error("the Bridge has no {0}")]
    Missing(&'static str),
}

/// Settings of the controller beyond its namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The StorageClass a bridge's claim asks for when its `Bridge` names none (the chart's
    /// `bridges.defaultStorageClassName`); `None` for the cluster's default.
    pub default_storage_class: Option<String>,
}

/// The label selector of everything the `Bridge` controller watches besides the `Bridge`s:
/// objects and pods the operator made for a bridge (`app.kubernetes.io/name=myelin-bridge`,
/// `managed-by=myelin-operator`).
///
/// `managed-by` alone is not enough: `hs operator --homeservers` labels a `Homeserver`'s
/// StatefulSet, Services and pods `managed-by=myelin-operator` too, and the pod watch maps a pod
/// to the `Bridge` named by its `app.kubernetes.io/instance`. On the first run against an API
/// server (2026-10-01, `deploy/operator/ci/kind-smoke.sh`) every change to a `Homeserver` named
/// `hs` logged "tried to reconcile object Bridge.../hs that was not found in local store".
#[must_use]
pub fn watch_selector() -> String {
    format!(
        "app.kubernetes.io/name={},app.kubernetes.io/managed-by={MANAGER}",
        crate::bridge::APP_NAME
    )
}

/// Whether a controller's run stream reports a watched object (a pod, a Deployment) that maps
/// to a resource no longer in the store: the pods of a `Bridge` or `Homeserver` that was just
/// deleted keep changing while they terminate. Nothing failed, so the controllers log it at
/// debug rather than as a failed reconcile (on the first cluster run every deleted bridge left
/// three to ten such warnings).
#[must_use]
pub fn is_stale_trigger<E1, E2>(error: &kube::runtime::controller::Error<E1, E2>) -> bool {
    matches!(error, kube::runtime::controller::Error::ObjectNotFound(_))
}

/// Shared state of every reconcile.
struct Context {
    client: Client,
    options: Options,
    metrics: OperatorMetrics,
}

/// Runs the `Bridge` controller in `namespace` until SIGTERM or Ctrl-C, then finishes the
/// reconciles in flight and returns. The same as [`run_with`] and default [`Options`].
///
/// # Errors
/// Never today: reconcile errors are logged and retried ([`REQUEUE_AFTER_ERROR`]) rather than
/// ending the controller. The `Result` leaves room for a startup check that can fail.
pub async fn run(client: Client, namespace: String) -> Result<(), ControllerError> {
    run_with(client, namespace, Options::default()).await
}

/// [`run`] with [`Options`].
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    client: Client,
    namespace: String,
    options: Options,
) -> Result<(), ControllerError> {
    run_with_metrics(client, namespace, options, OperatorMetrics::default()).await
}

/// [`run_with`], timing each reconcile into `metrics` (`hs_operator_reconcile_duration_seconds`
/// and `hs_operator_reconcile_errors_total` with `kind="Bridge"`).
///
/// # Errors
/// As [`run`].
pub async fn run_with_metrics(
    client: Client,
    namespace: String,
    options: Options,
    metrics: OperatorMetrics,
) -> Result<(), ControllerError> {
    let bridges: Api<Bridge> = Api::namespaced(client.clone(), &namespace);
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    let services: Api<Service> = Api::namespaced(client.clone(), &namespace);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let owned = || watcher::Config::default().labels(&watch_selector());

    tracing::info!(namespace = %namespace, "bridge operator starting");
    let pod_namespace = namespace.clone();
    Controller::new(bridges, watcher::Config::default())
        .owns(deployments, owned())
        .owns(services, owned())
        .owns(claims, owned())
        .watches(pods, owned(), move |pod: Pod| {
            pod.labels()
                .get("app.kubernetes.io/instance")
                .map(|name| ObjectRef::<Bridge>::new(name).within(&pod_namespace))
        })
        .shutdown_on_signal()
        .run(
            reconcile,
            error_policy,
            Arc::new(Context {
                client: client.clone(),
                options,
                metrics,
            }),
        )
        .for_each(|result| async move {
            match result {
                Ok((object, _action)) => tracing::debug!(bridge = %object.name, "reconciled"),
                Err(e) if is_stale_trigger(&e) => {
                    tracing::debug!(error = %e, "a change to an object of a deleted bridge");
                }
                Err(e) => tracing::warn!(error = %e, "bridge reconcile failed"),
            }
        })
        .await;
    tracing::info!(namespace = %namespace, "bridge operator stopped");
    Ok(())
}

async fn reconcile(bridge: Arc<Bridge>, ctx: Arc<Context>) -> Result<Action, ControllerError> {
    let started = std::time::Instant::now();
    let result = reconcile_bridge(bridge, &ctx).await;
    let reason = result.as_ref().err().map(|e| match e {
        ControllerError::Kube(_) => "kube",
        ControllerError::Missing(_) => "missing",
    });
    ctx.metrics
        .record_reconcile("Bridge", started.elapsed().as_secs_f64(), reason);
    result
}

async fn reconcile_bridge(bridge: Arc<Bridge>, ctx: &Context) -> Result<Action, ControllerError> {
    let name = bridge
        .metadata
        .name
        .clone()
        .ok_or(ControllerError::Missing("name"))?;
    let namespace = bridge
        .metadata
        .namespace
        .clone()
        .ok_or(ControllerError::Missing("namespace"))?;
    if bridge.metadata.deletion_timestamp.is_some() {
        // Owner references let the garbage collector remove everything built from it.
        return Ok(Action::await_change());
    }
    let client = &ctx.client;
    let apply = PatchParams::apply(MANAGER).force();

    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let pvc = pvc_with_default_class(&bridge, ctx.options.default_storage_class.as_deref());
    claims
        .patch(&crate::bridge::pvc_name(&name), &apply, &Patch::Apply(&pvc))
        .await?;

    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    let deployment = deployments
        .patch(&name, &apply, &Patch::Apply(&desired_deployment(&bridge)))
        .await?;

    let services: Api<Service> = Api::namespaced(client.clone(), &namespace);
    services
        .patch(&name, &apply, &Patch::Apply(&desired_service(&bridge)))
        .await?;

    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let pod_list = pods
        .list(&ListParams::default().labels(&pod_selector(&name)))
        .await?;

    let status = status_from(&bridge, Some(&deployment), &pod_list.items);
    let phase = status.phase;
    if bridge.status.as_ref() != Some(&status) {
        let bridges: Api<Bridge> = Api::namespaced(client.clone(), &namespace);
        bridges
            .patch_status(
                &name,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({ "status": status })),
            )
            .await?;
        tracing::info!(bridge = %name, phase = ?phase, "bridge status changed");
    }

    Ok(match phase {
        Phase::Ready => Action::requeue(REQUEUE_READY),
        Phase::Pending | Phase::Degraded => Action::requeue(REQUEUE_NOT_READY),
    })
}

/// The bridge's claim, asking for `default_class` when the `Bridge` names no StorageClass.
fn pvc_with_default_class(bridge: &Bridge, default_class: Option<&str>) -> PersistentVolumeClaim {
    let mut pvc = desired_pvc(bridge);
    if let (Some(spec), Some(class)) = (pvc.spec.as_mut(), default_class)
        && spec.storage_class_name.is_none()
        && !class.is_empty()
    {
        spec.storage_class_name = Some(class.to_owned());
    }
    pvc
}

fn error_policy(bridge: Arc<Bridge>, error: &ControllerError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(bridge = %bridge.name_any(), error = %error, "bridge reconcile failed, retrying");
    Action::requeue(REQUEUE_AFTER_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requeue_intervals_are_ordered() {
        assert!(REQUEUE_NOT_READY < REQUEUE_AFTER_ERROR);
        assert!(REQUEUE_AFTER_ERROR < REQUEUE_READY);
    }

    #[test]
    fn the_default_storage_class_fills_only_an_unnamed_class() {
        use crate::crds::{BridgeSpec, BridgeStorage, ImageSpec};
        let mut bridge = Bridge::new(
            "bridge-1",
            BridgeSpec {
                bridge_type: "heisenbridge".to_owned(),
                appservice_id: "heisenbridge".to_owned(),
                image: ImageSpec {
                    repository: "hif1/heisenbridge".to_owned(),
                    tag: None,
                    digest: None,
                    pull_policy: None,
                },
                port: 9898,
                files_secret: "bridge-1-files".to_owned(),
                args: Vec::new(),
                storage: BridgeStorage::default(),
                resources: None,
            },
        );
        let class = |b: &Bridge, d: Option<&str>| {
            pvc_with_default_class(b, d)
                .spec
                .and_then(|s| s.storage_class_name)
        };
        assert_eq!(class(&bridge, None), None);
        assert_eq!(class(&bridge, Some("")), None);
        assert_eq!(class(&bridge, Some("fast")), Some("fast".to_owned()));
        bridge.spec.storage.storage_class_name = Some("longhorn".to_owned());
        assert_eq!(class(&bridge, Some("fast")), Some("longhorn".to_owned()));
    }

    /// Whether `labels` satisfy an equality-only selector `k=v,k=v`, as the API server would.
    fn selects(
        selector: &str,
        labels: Option<&std::collections::BTreeMap<String, String>>,
    ) -> bool {
        selector.split(',').all(|term| {
            let (k, v) = term.split_once('=').expect("equality selector");
            labels.and_then(|l| l.get(k)).is_some_and(|got| got == v)
        })
    }

    #[test]
    fn the_watches_see_a_bridges_objects_and_not_a_homeservers() {
        use crate::crds::{BridgeSpec, BridgeStorage, ImageSpec};
        use kube::Resource as _;
        let mut bridge = Bridge::new(
            "hs",
            BridgeSpec {
                bridge_type: "heisenbridge".to_owned(),
                appservice_id: "heisenbridge".to_owned(),
                image: ImageSpec {
                    repository: "hif1/heisenbridge".to_owned(),
                    tag: None,
                    digest: None,
                    pull_policy: None,
                },
                port: 9898,
                files_secret: "hs-files".to_owned(),
                args: Vec::new(),
                storage: BridgeStorage::default(),
                resources: None,
            },
        );
        bridge.meta_mut().namespace = Some("matrix".to_owned());
        bridge.meta_mut().uid = Some("uid-bridge".to_owned());
        let selector = watch_selector();

        let deployment = desired_deployment(&bridge);
        assert!(selects(&selector, deployment.metadata.labels.as_ref()));
        let pod_labels = deployment
            .spec
            .and_then(|s| s.template.metadata)
            .and_then(|m| m.labels);
        assert!(selects(&selector, pod_labels.as_ref()));
        assert!(selects(
            &selector,
            desired_service(&bridge).metadata.labels.as_ref()
        ));
        assert!(selects(
            &selector,
            desired_pvc(&bridge).metadata.labels.as_ref()
        ));

        // A Homeserver of the same name in the same namespace: its pods carry
        // `managed-by=myelin-operator` and `instance=hs`, and must not reach the Bridge `hs`.
        let objects =
            crate::homeserver::build(&crate::homeserver::testing::single_node_homeserver("hs"))
                .expect("builds");
        let hs_pod_labels = objects
            .stateful_set
            .spec
            .and_then(|s| s.template.metadata)
            .and_then(|m| m.labels);
        assert_eq!(
            hs_pod_labels
                .as_ref()
                .and_then(|l| l.get("app.kubernetes.io/managed-by"))
                .map(String::as_str),
            Some(MANAGER),
            "the premise: a Homeserver's pods are the operator's too"
        );
        assert!(!selects(&selector, hs_pod_labels.as_ref()));
        assert!(!selects(
            &selector,
            objects.service.metadata.labels.as_ref()
        ));
    }

    #[test]
    fn only_a_missing_object_is_a_stale_trigger() {
        use kube::runtime::controller::Error;
        let missing: Error<ControllerError, std::io::Error> =
            Error::ObjectNotFound(ObjectRef::<Bridge>::new("gone").within("matrix").erase());
        assert!(is_stale_trigger(&missing));
        let failed: Error<ControllerError, std::io::Error> = Error::ReconcilerFailed(
            ControllerError::Missing("name"),
            ObjectRef::<Bridge>::new("here").within("matrix").erase(),
        );
        assert!(!is_stale_trigger(&failed));
        let queue: Error<ControllerError, std::io::Error> =
            Error::QueueError(std::io::Error::other("watch failed"));
        assert!(!is_stale_trigger(&queue));
    }

    #[test]
    fn errors_render_their_cause() {
        assert_eq!(
            ControllerError::Missing("namespace").to_string(),
            "the Bridge has no namespace"
        );
    }
}
