//! `hs-model`: the Matrix domain model over Ruma.
//!
//! Owned by track 02 (`docs/workstreams/02-state-and-model.md`). This crate is the week-2 seam
//! every other track builds on: identifiers and interned short IDs ([`ids`]), the room-version
//! capability table ([`room_version`]), canonical JSON ([`canonical`]), the redaction algorithm
//! per version ([`redaction`]), content and reference hashes ([`hash`]), the event wrapper with
//! cached canonical bytes and hashes plus the internal metadata flags ([`event`]),
//! version-aware power-level parsing ([`power_levels`]) and signing helpers ([`signing`]).
//!
//! Everything here is written from the specification text (room version pages v1 to v12 and the
//! appendices). Ruma is used for identifier parsing, ed25519 signatures and, in tests, as a second
//! implementation to cross-check canonical JSON, redaction and hashing against.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

// TODO(track owner): not yet written -- pub mod canonical;
pub mod error;
// TODO(track owner): not yet written -- pub mod event;
// TODO(track owner): not yet written -- pub mod hash;
pub mod ids;
// TODO(track owner): not yet written -- pub mod power_levels;
// TODO(track owner): not yet written -- pub mod redaction;
// TODO(track owner): not yet written -- pub mod room_version;
// TODO(track owner): not yet written -- pub mod signing;

pub use error::{CanonicalJsonError, EventError, PowerLevelsError, RedactionError};
// TODO(track owner): re-export blocked until the module exists -- pub use event::{Event, EventFlags, EventHeader};
pub use ids::{EventSn, RoomSn, ServerSn, StateKeyId, TypeId, UserSn};
// TODO(track owner): re-export blocked until the module exists -- pub use room_version::{RoomVersion, RoomVersionRules};

/// Ruma re-export so consumers pin the same version this crate was built against.
pub use ruma;
