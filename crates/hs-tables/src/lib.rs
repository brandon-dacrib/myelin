//! `hs-tables`: the typed layer above `hs-kv`.
//!
//! Owned by track 01 (`docs/workstreams/01-storage-engine.md`), frozen at week 4 in
//! `docs/workstreams/README.md`'s seam table: every track that stores anything builds on this
//! crate rather than hand-rolling key encoding or index maintenance, which is where the Conduit
//! lineage's chronic hand-maintained-index bugs come from (`PLAN.md` section 4, D1).
//!
//! - [`key`]: order-preserving tuple key encoding ([`key::TupleKey`], [`key::KeyEncode`],
//!   [`key::KeyDecode`]) — integers sort numerically, strings and blobs lexicographically, and a
//!   tuple can nest another tuple as one of its components.
//! - [`keyspace`]: [`keyspace::TypedKeyspace`], a thin, typed wrapper over an `hs-kv` keyspace
//!   handle that encodes/decodes keys through [`key::TupleKey`] instead of raw bytes.
//! - [`index`]: declarative unique and non-unique composite indexes
//!   ([`index::IndexDef`], [`index::maintain_index`]), maintained inside the same transaction as
//!   the row they index.
//! - [`migrations`]: a migration runner with a version table ([`migrations::Migration`],
//!   [`migrations::run_migrations`]).
//! - [`interning`]: get-or-create string/bytes interning with reverse lookups and an in-process
//!   cache ([`interning::InternTable`]), preconfigured for the six short IDs `PLAN.md` section 6.1
//!   names (`room_sn`, `user_sn`, `server_sn`, `event_sn`, `state_key_id`, `type_id`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod index;
pub mod interning;
pub mod key;
pub mod keyspace;
pub mod migrations;

pub use index::{IndexDef, IndexError, maintain_index};
pub use interning::{InternTable, ShortId};
pub use key::{KeyCodecError, KeyDecode, KeyEncode, TupleKey};
pub use keyspace::{TableError, TypedKeyspace};
pub use migrations::{Migration, MigrationError, run_migrations};
