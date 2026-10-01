//! `hs-operator`: kube-rs custom resource definitions (`Homeserver`, `AppService`, `Bridge`,
//! `PushGateway`, `IdentityService`, all in the `hs.matrix.org/v1alpha1` API group), the `Bridge`
//! controller `hs operator` runs, and the client the homeserver deploys bridges with.
//!
//! Owned by track 12 (`docs/workstreams/12-platform-and-kubernetes.md`).
//!
//! - [`crds`]: the CRD schemas ([`crds::all_crds`] enumerates all five) and the `gen-crds` binary
//!   (`src/bin/gen_crds.rs`) that renders them to `deploy/crds/*.yaml`.
//! - [`bridge`]: pure builders for what a `Bridge` becomes (claim, Deployment, Service) and the
//!   status read back from it (`docs/rfcs/0017-the-server-deploys-its-own-bridges.md`, 4.4).
//! - [`controller`]: the `Bridge` controller, applying those objects and writing the status.
//! - [`deploy`]: [`deploy::KubeBridgeClient`], which the homeserver's bridge manager
//!   (`crates/hs-bridges`) uses to write, read and delete `Bridge`s and their files Secrets.
//! - [`homeserver`]: the `Homeserver` reconciler: the chart's objects, scaling, and draining
//!   each departing replica through the admin API before its pod goes (decision 0012).
//! - [`metrics`]: `hs_operator_*` Prometheus metrics and their `/metrics` listener.
//! - [`reconcile`]: stub reconcile loops for the remaining kinds; not run by anything.
//! - [`run`]: what `hs operator` runs: the `Bridge` controller, optionally the `Homeserver`
//!   controller, and the metrics listener.
//!
//! # Status
//!
//! The builders, the status mapping and the manifest rendering are unit-tested; the
//! `Homeserver` reconciler is tested against an in-memory cluster and the chart (`helm
//! template`). Both controllers have run against a real API server since 2026-10-01:
//! `deploy/operator/ci/kind-smoke.sh` (in CD's amd64 image leg) drives a `Bridge` through
//! Ready, Degraded, Ready and deletion, and a single-node `Homeserver` through Ready, an image
//! roll and deletion. Cluster mode (draining through the admin API) has not run on a cluster
//! yet (`docs/status/12-platform-and-kubernetes.md`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bridge;
pub mod controller;
pub mod crds;
pub mod deploy;
pub mod homeserver;
pub mod metrics;
pub mod reconcile;

use std::net::SocketAddr;

/// What `hs operator` runs.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// The `Bridge` controller's settings.
    pub bridges: controller::Options,
    /// Also run the `Homeserver` controller (needs the `Homeserver` CRD installed and the RBAC
    /// in `deploy/operator/`).
    pub homeservers: bool,
    /// Serve `/metrics` here.
    pub metrics_address: Option<SocketAddr>,
}

/// Runs the operator in `namespace` until SIGTERM or Ctrl-C: the `Bridge` controller, the
/// `Homeserver` controller when asked, and the metrics listener when given an address.
///
/// # Errors
/// When a controller cannot start (the admin API client cannot be built), or the metrics
/// address cannot be bound.
pub async fn run(
    client: kube::Client,
    namespace: String,
    options: RunOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metrics = metrics::OperatorMetrics::default();
    let metrics_server = async {
        match options.metrics_address {
            Some(address) => metrics::serve(metrics.clone(), address).await,
            None => std::future::pending().await,
        }
    };
    let bridges = controller::run_with_metrics(
        client.clone(),
        namespace.clone(),
        options.bridges.clone(),
        metrics.clone(),
    );
    let homeservers = async {
        if options.homeservers {
            homeserver::controller::run(client.clone(), namespace.clone(), metrics.clone()).await
        } else {
            Ok(())
        }
    };
    let controllers = async {
        let (b, h) = tokio::join!(bridges, homeservers);
        b?;
        h?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };
    tokio::select! {
        result = controllers => result,
        result = metrics_server => Err(Box::new(result.err().unwrap_or_else(|| std::io::Error::other("metrics listener stopped")))),
    }
}

/// A Kubernetes client from the ambient configuration: the local kubeconfig when there is one,
/// else the pod's service account.
///
/// Installs `ring` as the process's rustls crypto provider first when none is installed: `kube`
/// connects with rustls, and a binary that links more than one provider (the `hs` binary does)
/// has no default until one is chosen. Installing fails harmlessly when another is already set.
///
/// # Errors
/// When no configuration can be found or the client cannot be built.
pub async fn connect() -> Result<kube::Client, kube::Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    kube::Client::try_default().await
}
