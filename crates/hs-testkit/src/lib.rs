//! `hs-testkit`: the in-process test harness every other track's integration tests are meant to
//! run against (`docs/decisions/0002-workspace-conventions.md`: "Integration tests use
//! `hs-testkit`").
//!
//! Owned by track 14 (`docs/workstreams/14-test-and-conformance.md`), mirroring the intent of
//! Synapse's `HomeserverTestCase` without its internals: a fake clock, a scenario DSL for
//! scripted multi-user HTTP flows against a real axum `Router`, and recording fake doubles for
//! the systems a homeserver talks to besides its own database (an appservice, a federation peer,
//! an SMTP server, a push gateway).
//!
//! # Modules
//!
//! - [`clock`]: [`clock::FakeClock`], compatible with `tokio::time`'s paused-time mode.
//! - [`scenario`]: [`scenario::Scenario`], the multi-user HTTP scenario DSL, and
//!   [`scenario::ScenarioResponse`].
//! - [`matrix_error`]: constructing and asserting the spec's standard error body shape.
//! - [`record_log`]: [`record_log::RecordLog`], an append-only log backed by
//!   `hs_kv::memory::MemoryBackend` (not a hand-rolled `Vec<Mutex<_>>>`) that every fake double
//!   below is built on.
//! - [`fake_appservice`], [`fake_federation`], [`fake_pushgw`], [`fake_smtp`]: recording doubles
//!   for the four external systems a homeserver pushes to.
//!
//! # Proving the DSL works
//!
//! `tests/hs_auth_round_trip.rs` drives `hs-auth`'s real router (`hs_auth::routes::router()`)
//! through register, login, whoami, refresh and logout using nothing but this crate's public API,
//! which is this crate's own acceptance test: if that file breaks, the DSL is not fit for
//! purpose.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clock;
pub mod fake_appservice;
pub mod fake_federation;
pub mod fake_pushgw;
pub mod fake_smtp;
pub mod matrix_error;
pub mod record_log;
pub mod scenario;

pub use clock::FakeClock;
pub use fake_appservice::FakeAppservice;
pub use fake_federation::FakeFederationPeer;
pub use fake_pushgw::FakePushGateway;
pub use fake_smtp::FakeSmtpSink;
pub use matrix_error::{MatrixErrorExpectation, assert_matrix_error};
pub use record_log::RecordLog;
pub use scenario::{Scenario, ScenarioResponse, UserSession};

/// `hs-kv` re-export so consumers building on [`RecordLog`] (or wiring their own fakes to the
/// same store) pin the same version this crate was built against.
pub use hs_kv;
