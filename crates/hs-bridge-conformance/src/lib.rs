//! `hs-bridge-conformance`: a synthetic appservice that replays the mautrix call patterns
//! measured in `PLAN.md` Appendix B and asserts what a real bridge would observe.
//!
//! Owned by track 11 (`docs/workstreams/11-appservices-and-bridges.md`). See
//! `docs/status/11-appservices-and-bridges.md` for exactly what this suite covers today, what it
//! does not yet (and why — mostly: the endpoint or the other track's component it would exercise
//! does not exist yet), and the Docker/Synapse-control note.
//!
//! # Shape
//!
//! [`Harness`] wires together this crate's real components — an `hs_appservice::Registry`, its
//! `Scheduler` and `PingService`, and `hs_auth`'s `AuthState`/`Requester` machinery with
//! `hs_appservice::auth_registry::RegistryAppserviceAdapter` as the appservice token source — the
//! same way a real server assembling these crates would, and a real HTTP receiver (bound to a
//! loopback port with `axum::serve`, not an in-process `oneshot`) standing in for the bridge, so
//! that transactions and pings genuinely cross an HTTP boundary and get parsed back out of JSON
//! the way a real `mautrix-go` bridge's own HTTP server would. This is what makes the suite an
//! actual conformance check on wire format rather than a unit test of Rust structs.
//!
//! Each scenario in [`scenarios`] is a `#[tokio::test]` (run them with `cargo test -p
//! hs-bridge-conformance`) that builds a [`Harness`], drives it the way the named mautrix call
//! pattern from Appendix B drives a real homeserver, and asserts the observable result.
//!
//! # Running against Synapse as a control
//!
//! `PLAN.md`'s day-one work calls for validating this suite against Synapse 1.161 in Docker before
//! trusting it as a checker for us. That requires a running Synapse instance behind a homeserver
//! whose appservice config points at this crate's fake-bridge receiver, driven by Synapse's own
//! client API to generate the traffic under test — which needs Docker, unavailable in this
//! environment (see `docs/status/11-appservices-and-bridges.md`). The scenarios here therefore
//! exercise the *sender* side (this crate's own `Scheduler`/`PingService`/`Requester` wiring)
//! directly, which is the half achievable without Synapse; the receiver they drive against is
//! always this crate's own fake bridge, not Synapse's.

pub mod fake_bridge;
pub mod harness;

pub use fake_bridge::FakeBridge;
pub use harness::Harness;
