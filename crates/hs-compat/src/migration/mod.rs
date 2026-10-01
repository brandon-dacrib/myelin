//! The online importer: migrating a Synapse deployment into this server, driven through the
//! admin API's Migration area (`hs_admin::migration`).
//!
//! # What is copied
//!
//! Thirteen streams, in order ([`model::Stream`]): accounts with their password hashes and
//! profiles, devices, access tokens (so that signed-in clients stay signed in), account data and
//! room tags, each device's end-to-end keys (identity, one-time and fallback keys), cross-signing
//! keys with the signatures on them, server-side key backups, push rules, pushers, sync filters
//! (under the ids Synapse gave them), rooms (every event of each room, replayed in order through
//! this server's own authorization a page at a time, then its aliases and directory listing),
//! read receipts, and local media (records and files). The mapping, table by table, is
//! `docs/compat/synapse-importer-mapping.md`; what is not copied (rooms this server's users
//! joined over federation, remote media, presence) is listed in
//! `docs/compat/synapse-migration-runbook.md`.
//!
//! A room is copied in bounded memory ([`rooms`]), and each room's and the whole copy's
//! throughput -- events and bytes per second, and the process's peak memory -- is logged and
//! measured ([`throughput`]).
//!
//! # How it runs
//!
//! [`Migrator`] keeps one durable [`model::MigrationRecord`] in a [`MigrationStore`] and runs each
//! long step -- the copy, a verification, the cutover -- as a task in `hs-admin`'s task registry.
//! The copy reads Synapse in batches keyed on a stable column, writes each row through a
//! [`MigrationTarget`] (this server's stores, in `hs-cli`), and records a checkpoint per stream
//! after every batch, so it can be paused, resumed, interrupted by a restart and resumed again
//! without copying anything twice (every write is idempotent: a row already here is recognized,
//! and counted as copied). Synapse's database is only ever read.
//!
//! # Verification and cutover
//!
//! [`Migrator`]'s verification recounts every stream in Synapse and checks each row is here,
//! then compares samples field by field (password hashes, profiles, tokens, each room's current
//! state, media bytes). Cutover, which an operator runs once Synapse is stopped, makes a final
//! pass over everything (picking up whatever changed since the bulk copy), verifies, and ends
//! the migration only if verification passes.

pub mod engine;
pub mod model;
pub mod rooms;
pub mod rows;
pub mod source;
pub mod store;
pub mod target;
pub mod throughput;

pub use engine::{MigrationObserver, Migrator, SourceConfigs};
pub use model::{LogEntry, LogLevel, MigrationRecord, Phase, Stream, VerificationReport};
pub use source::SynapseSource;
pub use store::{InMemoryMigrationStore, MigrationStore};
pub use target::{
    Check, CurrentState, Imported, MigrationTarget, RoomOutcome, TargetError, TargetMedia,
    TargetUser,
};
pub use throughput::{ImportStats, RoomStats, peak_rss_bytes};

/// Why a migration step could not go on.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// Synapse's database or media store could not be read.
    #[error("reading Synapse: {0}")]
    Source(String),
    /// This server's stores could not be written or read.
    #[error("writing this server: {0}")]
    Target(String),
    /// The migration's own record could not be kept.
    #[error("keeping the migration's record: {0}")]
    Store(String),
    /// The configuration names no usable source.
    #[error("{0}")]
    Config(String),
}
