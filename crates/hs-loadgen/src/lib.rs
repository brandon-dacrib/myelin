//! hs-loadgen: `matrix-rust-sdk` driving a real `hs serve` process over real HTTP.
//!
//! Owned by track 05 (`docs/workstreams/05-sync.md`), which names this crate the load-generation
//! and real-client-conformance driver alongside its `/sync` work: `docs/next-steps.md` calls this
//! "the single best test of whether this is a homeserver" — a scripted stand-in for pointing
//! Element Web at the server, using the client library real users actually run instead of this
//! workspace's own test helpers, which necessarily speak the server's own dialect.
//!
//! # Modules
//!
//! - [`harness`]: boots and tears down a real `hs` binary subprocess on a temp data directory and
//!   a free port.
//! - [`scenario`]: the end-to-end scenario itself (register, log in, room lifecycle, messaging,
//!   sync, profile, membership, pagination, logout), built on `matrix-sdk`.
//! - [`scenario_encrypted`]: track 08's encrypted scenario (device/one-time key upload, key
//!   query, atomic key claim under concurrency, cross-signing, an encrypted room, and a real
//!   decrypt), built on the same `matrix-sdk` with its `e2e-encryption` feature enabled.
//!
//! See `tests/real_client.rs` and `tests/real_client_encrypted.rs` for the runnable entry points,
//! and `docs/status/05-sync.md`/`docs/status/08-e2ee.md` for what these have found running
//! against the real binary.

#![forbid(unsafe_code)]

pub mod harness;
pub mod scenario;
pub mod scenario_encrypted;
