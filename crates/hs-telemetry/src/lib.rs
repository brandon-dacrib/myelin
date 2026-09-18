//! `hs-telemetry`: tracing initialization, structured logs, request-id propagation, Prometheus
//! metrics, and (behind feature flags) OpenTelemetry OTLP export and Sentry error reporting.
//!
//! Owned by track 12 (`docs/workstreams/12-platform-and-kubernetes.md`). This crate provides:
//!
//! - [`init::init`]: installs the global `tracing` subscriber (JSON or a Synapse-like text
//!   format, see [`init::LogFormat`]), optionally layering in OTLP export (`otlp` feature) and
//!   Sentry reporting (`sentry` feature).
//! - [`request_id`]: a [`tower::Layer`] that stamps every request with an `X-Request-Id` (reusing
//!   a caller-supplied one) and attaches it to the span every downstream log line runs inside.
//! - [`metrics::Metrics`]: a shared Prometheus [`prometheus_client::registry::Registry`] plus the
//!   process-wide HTTP metrics this crate registers itself. See [`metrics`]'s module docs for the
//!   binding metric- and span-naming conventions every other track's crate follows when it
//!   registers its own metrics into the same registry — also recorded in
//!   `docs/decisions/0004-telemetry-conventions.md` for tracks that read decisions docs rather
//!   than crate rustdoc.
//!
//! This crate deliberately does not depend on `hs-config`: [`init::Options`] is this crate's own
//! plain struct, and `hs-cli` (the only binary that wires configuration to telemetry) is
//! responsible for building one from `hs_config::TelemetryConfig`. That keeps the dependency
//! direction one-way and lets any other crate depend on `hs-telemetry` for its metrics registry
//! or span-naming helpers without pulling in the whole config schema.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod init;
pub mod metrics;
pub mod request_id;

#[cfg(feature = "otlp")]
mod otlp;
#[cfg(feature = "sentry")]
mod sentry_integration;

pub use error::TelemetryError;
pub use init::{Guard, Level, LogFormat, Options, init};
pub use metrics::{HttpLabels, Metrics};
pub use request_id::{
    REQUEST_ID_HEADER, RequestIdLayer, RequestIdService, request_id_from_headers,
};
