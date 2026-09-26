//! `hs-room`: the room actor -- the unit of consistency for one Matrix room.
//!
//! Owned by track 04 (`docs/workstreams/04-room-and-events.md`). See
//! `docs/design/04-room-actor-protocol.md` for the full design document (command set, publish
//! stream, hot-state cache and eviction policy) and `docs/status/04-room-and-events.md` for what
//! has landed.
//!
//! # Modules
//!
//! - [`pipeline`]: builds, hashes, signs and authorizes a new locally-originated event against a
//!   room's current state.
//! - [`actor`]: [`actor::RoomActor`], the room actor itself -- hot state, extremities, timeline,
//!   persistence.
//! - [`protocol`]: the room actor's command set, its replies, and [`protocol::RoomUpdate`], the
//!   publish stream tracks 05, 06, 10 and 11 consume.
//! - [`membership`]: the membership state machine (join, invite, leave, kick, ban, unban, knock)
//!   as an explicit transition table.
//! - [`timeline`]: pagination tokens over a room's room-local timeline positions.
//! - [`relations`]: `m.relates_to` indexing and bundled aggregations.
//! - [`history_visibility`]: the `m.room.history_visibility` read-side algorithm (pure logic;
//!   `actor::RoomActor::event_visible_to`/`can_read_room` supply the state snapshots it reasons
//!   about).
//! - [`registry`]: [`registry::RoomRegistry`], the per-process map from room to actor handle, with
//!   idle eviction.
//! - [`state`]: [`state::RoomState`], this crate's axum shared state, and [`state::RoomRequester`].
//! - [`routes`]: the client-server HTTP endpoints, as a router fragment (`routes::router`).
//! - [`admin`]: implements `hs_admin::sources::RoomDirectory` over [`registry::RoomRegistry`], the
//!   seam the admin API's `/rooms` operations call.
//! - [`fencing`]: [`fencing::RoomFencing`], the optional cluster-fencing hook
//!   [`actor::RoomActor::persist`] checks before committing.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actor;
pub mod admin;
pub mod backfill;
pub mod error;
pub mod fencing;
pub mod history_visibility;
pub mod identity;
pub mod membership;
pub mod persist;
pub mod pipeline;
pub mod protocol;
pub mod registry;
pub mod relations;
pub mod remote_join;
pub mod retention;
pub mod routes;
pub mod state;
pub mod timeline;

pub use error::RoomError;
