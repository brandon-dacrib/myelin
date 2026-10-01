//! `hs-cli`: the `hs` binary. Owned by the integration lead's extension of track 12
//! (`docs/compat/cli-shims.md` is track 13's specification for this crate; see that document and
//! `docs/status/12-platform-and-kubernetes.md` for what is implemented, what is stubbed, and the
//! seam gaps discovered wiring this crate up for the first time).
//!
//! This crate is split into a library (this file and its modules) and a thin `main.rs` so the
//! end-to-end test (`tests/e2e.rs`) can boot a real server in-process without a subprocess.

pub mod appservice_delivery;
pub mod appservice_manifest;
pub mod appservices;
pub mod audit;
pub mod auth_manifest;
pub mod backfill;
pub mod boot;
pub mod bootstrap;
pub mod bridges;
pub mod capabilities;
pub mod cli;
pub mod cluster;
pub mod cluster_admin;
pub mod config_bridge;
pub mod config_cmd;
pub mod config_source;
pub mod edu_forward;
pub mod edus;
pub mod federation;
pub mod federation_sender;
pub mod generate_config;
pub mod hash_password;
pub mod hierarchy;
pub mod identity;
pub mod identity_service;
pub mod live_config;
pub mod media;
pub mod metrics_layer;
pub mod migration;
pub mod overview;
pub mod recover;
pub mod register;
pub mod remote_join;
pub mod room_admin;
pub mod serve;
pub mod server_notices;
pub mod signing_key;
pub mod statistics;
pub mod storage;
pub mod synapse_serve;
pub mod synapse_shims;
pub mod sync_cluster;
pub mod tasks;
pub mod user_data;
pub mod versions;
pub mod well_known;

pub use cli::{Cli, Command};
