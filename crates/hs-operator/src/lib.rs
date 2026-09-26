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
//! - [`reconcile`]: stub reconcile loops for the other four kinds; not run by anything.
//!
//! # Status
//!
//! The builders, the status mapping and the manifest rendering are unit-tested; the controller
//! and the client compile against `kube` but, as of 2026-09-26, have not yet run against a
//! cluster (`docs/status/12-platform-and-kubernetes.md`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bridge;
pub mod controller;
pub mod crds;
pub mod deploy;
pub mod reconcile;

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
