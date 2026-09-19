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
//! CRD schema generation round-trips through YAML (`crds::tests`), reconcile stub logic is
//! exercised directly as plain async functions against hand-built resource values
//! (`reconcile::tests`), and `deploy/crds/*.yaml` applies cleanly to a real Kubernetes API server
//! (verified 2026-09-19 against a real cluster, not `kind` — see
//! `docs/status/12-platform-and-kubernetes.md`). `src/bin/live_smoke.rs` additionally proved that
//! a real `kube::runtime::Controller` watching a real API server delivers events into
//! [`reconcile::reconcile_homeserver`] and produces the expected `Action` — the stub reconcile
//! functions have run against a live cluster, not just in-process unit tests. What has *not* been
//! built yet: the reconcile functions still only compute a `status`, with no create/patch calls
//! against owned resources (`StatefulSet`/`ConfigMap`/`Service`) — see `reconcile`'s module doc
//! for exactly what remains Phase 1/2 work.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod crds;
pub mod reconcile;
