//! Pluggable content scanning and adaptation (`docs/rfcs/0008-content-scanning.md`, PLAN.md
//! decision D6b).
//!
//! - [`types`]: the provider interface exactly as RFC section 2 specifies —
//!   [`types::ContentScanner`], [`types::Verdict`] (including [`types::Verdict::Replaced`], RFC
//!   section 3.4), [`types::UnscannableReason`], [`types::ScanSource`], [`types::ScanContext`],
//!   [`types::ScanTicket`], [`types::ScanError`].
//! - [`config`]: [`config::ScanningConfig`] — mode, provider selection, the required (never
//!   defaulted) failure policy, the verdict cache's knobs, and `allow_replacement`.
//! - [`cache`]: [`cache::VerdictCache`], keyed on `(sha256, provider_id, engine_version)` over
//!   `hs-tables` (RFC section 6).
//! - [`providers`]: `icap` (the provider — see that module's doc for why an existing crate,
//!   `icap-rs`, is used rather than a hand-rolled client), `http` (cloud APIs with no ICAP
//!   fronting) and `none` (the default). See `providers`'s module doc and
//!   `docs/decisions/0007-build-less-reuse-more.md` for what was deliberately *not* built (a
//!   direct clamd client, a command runner).
//! - [`engine`]: [`engine::ScanEngine`], the orchestrator every scan point calls — cache lookup,
//!   the provider call (with pending-verdict polling), the mode/fail-policy/replacement rules,
//!   metrics and audit, all in one place. Read that module's doc first: it states the two
//!   guarantees (encrypted media is never reported clean; a down scanner fails only by explicit
//!   policy) structurally, not just as documentation.
//! - [`metrics`]: [`metrics::ScanMetrics`] (RFC section 8's four metrics).
//! - [`audit`]: [`audit::AuditSink`] and implementations (RFC sections 3.4, 4 and 8).
//! - [`admin`]: [`admin::ScanAdmin`], the trait track 15's admin HTTP layer calls into (see that
//!   module's doc for what is and is not implemented yet).
//!
//! # Status
//!
//! This module is complete and independently tested end to end (config -> cache -> providers ->
//! engine -> metrics/audit). Wiring `engine::ScanEngine` into
//! `crate::repository::MediaRepository`'s actual upload/download/appservice call sites (RFC
//! section 4) is **not done in this session** — see `docs/status/09-media.md` for exactly what is
//! left and where to start.

pub mod admin;
pub mod audit;
pub mod cache;
pub mod config;
pub mod engine;
pub mod metrics;
pub mod providers;
pub mod types;

pub use config::ScanningConfig;
pub use engine::{EngineDecision, ScanEngine};
pub use types::{
    ContentScanner, ScanContext, ScanError, ScanSource, ScanSourceKind, ScanTicket,
    UnscannableReason, Verdict,
};
