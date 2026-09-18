//! `hs-cli`: the `hs` binary. Owned by the integration lead's extension of track 12
//! (`docs/compat/cli-shims.md` is track 13's specification for this crate; see that document and
//! `docs/status/12-platform-and-kubernetes.md` for what is implemented, what is stubbed, and the
//! seam gaps discovered wiring this crate up for the first time).
//!
//! This crate is split into a library (this file and its modules) and a thin `main.rs` so the
//! end-to-end test (`tests/e2e.rs`) can boot a real server in-process without a subprocess.

pub mod auth_manifest;
pub mod capabilities;
pub mod cli;
pub mod config_bridge;
pub mod generate_config;
pub mod hash_password;
pub mod metrics_layer;
pub mod register;
pub mod serve;
pub mod signing_key;
pub mod storage;
pub mod synapse_serve;
pub mod versions;

pub use cli::{Cli, Command};
