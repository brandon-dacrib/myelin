//! The `Homeserver` reconciler (queue item 5 of `docs/next-steps.md`): a `Homeserver` resource
//! becomes the objects `deploy/helm/hs` renders for the same values, and its replicas are scaled
//! and rolled with every departing replica drained through the admin API first
//! (`docs/decisions/0012-a-drain-is-a-request-in-the-shared-store.md`).
//!
//! - [`objects`]: pure builders, and [`objects::chart_values`], the equivalent chart values.
//! - [`reconciler`]: one reconcile step over two seams, [`reconciler::HomeserverKube`] and
//!   [`admin::AdminApi`]; its module documentation describes the drain-before-evict protocol.
//! - [`admin`]: the admin API client.
//! - [`kube_ops`]: the Kubernetes side over a `kube::Client`.
//! - [`controller`]: the watch loop `hs operator --homeservers` runs.
//!
//! `docs/crds/homeserver.md` is the resource's reference.

pub mod admin;
pub mod controller;
pub mod kube_ops;
pub mod objects;
pub mod reconciler;

pub use objects::{build, chart_values};

#[cfg(test)]
mod helm_equivalence;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
