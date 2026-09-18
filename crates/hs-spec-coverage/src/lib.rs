//! `hs-spec-coverage`: OpenAPI-driven route coverage for the five Matrix APIs.
//!
//! Owned by track 14 (`docs/workstreams/14-test-and-conformance.md`), this is layer L2 of
//! `PLAN.md` section 12's test strategy: parse the Matrix spec's OpenAPI trees
//! (`refs/matrix-spec/data/api/{client-server,server-server,application-service,identity,push-gateway}/`),
//! enumerate every `(method, path)` the spec declares, compare against a `routes.json` router
//! manifest (`docs/rfcs/0005-routes-json-manifest.md`, owned by track 15's `hs-http::router`),
//! and report what is registered, missing, and extra.
//!
//! # Modules
//!
//! - [`spec`]: parses the spec's OpenAPI YAML trees into [`spec::SpecRoute`]s.
//! - [`manifest`]: [`manifest::RouteManifest`], this crate's own copy of the RFC 0005 schema.
//! - [`coverage`]: [`coverage::CoverageReport`], the diff between the two.
//! - [`report`]: [`report::render_markdown`], the Markdown report generator.
//!
//! See the `hs-spec-coverage` binary (`src/bin/hs-spec-coverage.rs`) for the CLI, and
//! `tests/real_spec.rs` for coverage computed against the actual spec checkout in `refs/`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod coverage;
pub mod error;
pub mod manifest;
pub mod report;
pub mod spec;

pub use coverage::{ApiCoverage, CoverageReport};
pub use error::CoverageError;
pub use manifest::RouteManifest;
pub use report::render_markdown;
pub use spec::{ApiFamily, SpecRoute, load_all, load_family};
