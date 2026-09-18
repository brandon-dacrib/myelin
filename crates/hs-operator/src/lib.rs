//! `hs-operator`: kube-rs custom resource definitions (`Homeserver`, `AppService`, `Bridge`,
//! `PushGateway`, `IdentityService`, all in the `hs.matrix.org/v1alpha1` API group) and their
//! reconcile loops.
//!
//! Owned by track 12 (`docs/workstreams/12-platform-and-kubernetes.md`).
//!
//! - [`crds`]: the CRD schemas ([`crds::all_crds`] enumerates all five) and the `gen-crds` binary
//!   (`src/bin/gen_crds.rs`) that renders them to `deploy/crds/*.yaml`.
//! - [`reconcile`]: stub reconcile loops — see that module's doc comment for exactly what "stub"
//!   means here and what is tracked as follow-up work.
//!
//! # Status
//!
//! No cluster access exists in the environment this crate was written in (no `kind`, no live
//! Kubernetes API). Everything here is therefore validated by: CRD schema generation succeeding
//! and round-tripping through YAML (`crds::tests`), reconcile stub logic exercised directly as
//! plain async functions against hand-built resource values (`reconcile::tests`), and
//! `deploy/crds/*.yaml` reviewed by eye against the OpenAPI v3 schema Kubernetes expects. None of
//! this proves the CRDs actually apply cleanly to a real API server or that a real `Controller`
//! watch loop behaves correctly under real events; `docs/status/12-platform-and-kubernetes.md`
//! records that gap explicitly.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod crds;
pub mod reconcile;
