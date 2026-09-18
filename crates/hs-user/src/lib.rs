//! `hs-user`: the user session actor and both sync APIs.
//!
//! Owned by track 05 (`docs/workstreams/05-sync.md`). See `PLAN.md` sections 5.4 and 6.6 for the
//! design this crate implements: a per-user session actor built on a durable, coalesced feed
//! rather than a global stream ordering, so `/sync` scales independently of how many rooms exist
//! and failover never needs to replay the whole server's history.
//!
//! # Modules
//!
//! - [`token`]: [`token::SyncToken`], the opaque, versioned `since`/`next_batch` token.
//! - [`store`]: the durable, `hs-kv`/`hs-tables`-backed feed, membership snapshot, device-cursor
//!   and account-data tables (`store::UserStore`, `store::tables::TablesUserStore`).
//! - [`room_source`]: [`room_source::RoomSource`], the trait this crate depends on for "give me
//!   a room's actor handle" without owning `hs-room`'s registry itself.
//! - [`hub`]: [`hub::SessionHub`], the per-process registry of user session actors, and
//!   [`hub::UserSessionActor`], the actor that turns `hs-room`'s `RoomUpdate` publish stream into
//!   this user's durable feed.
//! - [`filter`]: `/sync`'s `filter`/`filter_id` parsing (`crate::filter::SyncFilter`).
//! - [`sync`]: `/sync` v2's response construction, full and incremental.
//! - [`routes`]: the client-server HTTP endpoints (`/sync`, `/joined_rooms`, `/publicRooms`,
//!   account data), as a router fragment (`routes::router`), following the same shape
//!   `hs-room`'s and `hs-media`'s route modules use.
//! - [`state`]: [`state::UserState`], this crate's axum shared state, and
//!   [`state::UserRequester`], the `hs-auth` `Requester` bridge (mirrors
//!   `hs_room::state::RoomState`/`RoomRequester`).
//! - [`error`]: [`error::UserError`], mapped onto `hs_http::error::MatrixError`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod filter;
pub mod hub;
pub mod room_source;
pub mod routes;
pub mod state;
pub mod store;
pub mod sync;
pub mod token;

pub use error::UserError;
