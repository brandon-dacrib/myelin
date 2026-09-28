//! Stub reconcile loops for `AppService`, `PushGateway` and `IdentityService` (and an unused
//! `Bridge` stub; the real `Bridge` controller is [`crate::controller`], the real `Homeserver`
//! one [`crate::homeserver`]). Each `reconcile_*` function is what a
//! `kube::runtime::Controller` for that kind would call on every watch event; today each one only
//! logs and returns an [`Action`] telling the controller when to look again. Turning these into
//! real controllers is tracked in `docs/status/12-platform-and-kubernetes.md`.
//!
//! # Why stubs are still useful
//!
//! Even without touching the Kubernetes API, these functions are the seam where "what should this
//! resource's `status` say, given its current `spec`" lives, and that logic is exactly what the
//! unit tests below exercise — independent of `kube::Client`, `kind`, or any cluster at all. A
//! real controller wires [`error_policy`] and these functions into
//! `kube::runtime::Controller::new(...).run(reconcile_appservice, error_policy, context)`.

use std::sync::Arc;
use std::time::Duration;

use kube::runtime::controller::Action;

use crate::crds::{AppService, Bridge, IdentityService, OperatorStatus, Phase, PushGateway};

/// How long to wait before the next reconcile when nothing went wrong and there is nothing more
/// to do right now.
const REQUEUE_STEADY_STATE: Duration = Duration::from_secs(300);
/// How long to wait before retrying after a reconcile error.
const REQUEUE_AFTER_ERROR: Duration = Duration::from_secs(30);

/// Errors a reconcile function can return. Deliberately minimal for the stub: a real controller's
/// error type would also carry the Kubernetes API errors from create/patch calls.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// The resource has no `.metadata.name` (should not happen for anything read from the API
    /// server; guarded against explicitly rather than `.unwrap()`ing, per this workspace's quality
    /// bar — see `docs/decisions/0002-workspace-conventions.md`).
    #[error("resource has no name")]
    MissingName,
}

/// The error policy every stub controller in this module would use: always retry after
/// [`REQUEUE_AFTER_ERROR`], regardless of error kind. A real controller likely wants to
/// distinguish retryable from terminal errors; left uniform here since there is only one error
/// variant today.
pub fn error_policy<K>(_object: Arc<K>, _error: &ReconcileError, _ctx: Arc<()>) -> Action {
    Action::requeue(REQUEUE_AFTER_ERROR)
}

/// Computes the next status for an `AppService`. Real logic (writing the registration into the
/// referenced `Homeserver`'s appservice registry, per `hs_compat`/track 11's registry API) is not
/// implemented.
///
/// # Errors
/// Returns [`ReconcileError::MissingName`] if the resource has no name.
pub async fn reconcile_appservice(
    appservice: Arc<AppService>,
    _ctx: Arc<()>,
) -> Result<Action, ReconcileError> {
    let name = appservice
        .metadata
        .name
        .as_deref()
        .ok_or(ReconcileError::MissingName)?;
    tracing::info!(
        appservice = name,
        homeserver_ref = %appservice.spec.homeserver_ref,
        "reconcile_appservice: stub, no registration written"
    );
    Ok(Action::requeue(REQUEUE_STEADY_STATE))
}

/// Computes the next status for a `Bridge`. Real logic (creating the bridge's `Deployment`,
/// mounting its config, watching the referenced `AppService`) is not implemented.
///
/// # Errors
/// Returns [`ReconcileError::MissingName`] if the resource has no name.
pub async fn reconcile_bridge(
    bridge: Arc<Bridge>,
    _ctx: Arc<()>,
) -> Result<Action, ReconcileError> {
    let name = bridge
        .metadata
        .name
        .as_deref()
        .ok_or(ReconcileError::MissingName)?;
    tracing::info!(
        bridge = name,
        bridge_type = %bridge.spec.bridge_type,
        "reconcile_bridge: stub, no workload created"
    );
    Ok(Action::requeue(REQUEUE_STEADY_STATE))
}

/// Computes the next status for a `PushGateway`. Real logic (`Deployment`, `Service`) is not
/// implemented.
///
/// # Errors
/// Returns [`ReconcileError::MissingName`] if the resource has no name.
pub async fn reconcile_pushgateway(
    pgw: Arc<PushGateway>,
    _ctx: Arc<()>,
) -> Result<Action, ReconcileError> {
    let name = pgw
        .metadata
        .name
        .as_deref()
        .ok_or(ReconcileError::MissingName)?;
    tracing::info!(
        pushgateway = name,
        replicas = pgw.spec.replicas,
        "reconcile_pushgateway: stub, no workload created"
    );
    Ok(Action::requeue(REQUEUE_STEADY_STATE))
}

/// Computes the next status for an `IdentityService`. Real logic (`Deployment`, `Service`) is not
/// implemented.
///
/// # Errors
/// Returns [`ReconcileError::MissingName`] if the resource has no name.
pub async fn reconcile_identityservice(
    identity: Arc<IdentityService>,
    _ctx: Arc<()>,
) -> Result<Action, ReconcileError> {
    let name = identity
        .metadata
        .name
        .as_deref()
        .ok_or(ReconcileError::MissingName)?;
    tracing::info!(
        identity_service = name,
        public_base_url = %identity.spec.public_base_url,
        "reconcile_identityservice: stub, no workload created"
    );
    Ok(Action::requeue(REQUEUE_STEADY_STATE))
}

/// Builds the `Pending` status a freshly observed spec starts from, before any real
/// reconciliation logic exists to advance it to `Ready`. Exposed so tests (and, later, the real
/// reconcile functions above) share one "what does a not-yet-reconciled status look like"
/// definition.
#[must_use]
pub fn initial_status(observed_generation: Option<i64>) -> OperatorStatus {
    OperatorStatus {
        phase: Phase::Pending,
        observed_generation,
        ready_replicas: Some(0),
        conditions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_status_is_pending_with_zero_ready_replicas() {
        let status = initial_status(Some(1));
        assert_eq!(status.phase, Phase::Pending);
        assert_eq!(status.ready_replicas, Some(0));
        assert_eq!(status.observed_generation, Some(1));
    }

    #[test]
    fn error_policy_requeues_after_the_error_backoff() {
        let action = error_policy(Arc::new(()), &ReconcileError::MissingName, Arc::new(()));
        assert_eq!(action, Action::requeue(REQUEUE_AFTER_ERROR));
    }

    fn _also_type_checks_the_other_stub_functions_against_their_kinds() {
        // Not executed; exists so the compiler proves `reconcile_appservice`,
        // `reconcile_bridge`, `reconcile_pushgateway` and `reconcile_identityservice` all have
        // the shape `kube::runtime::Controller::run` expects, without needing a live object of
        // each kind in a test.
        fn assert_reconciler<K, Fut>(_f: impl Fn(Arc<K>, Arc<()>) -> Fut)
        where
            Fut: std::future::Future<Output = Result<Action, ReconcileError>>,
        {
        }
        assert_reconciler(reconcile_appservice);
        assert_reconciler(reconcile_bridge);
        assert_reconciler(reconcile_pushgateway);
        assert_reconciler(reconcile_identityservice);
    }
}
