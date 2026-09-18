//! `hs-state`: event authorization, state resolution and the chain-cover auth index.
//!
//! Owned by track 02 (`docs/workstreams/02-state-and-model.md`). Built on [`hs_model`]'s event
//! model and room-version capability table. This crate owns:
//!
//! - [`auth`]: event authorization for room versions 1 to 12, written from the spec text and
//!   parameterized by [`hs_model::room_version::RoomVersionRules`] rather than duplicated per
//!   version.
//! - [`state_fetch`]: [`state_fetch::StateFetch`], the narrow "look up one state event" interface
//!   authorization and (eventually) resolution read state through.
//!
//! State resolution, the frozen `hs-state` API trait and the chain-cover index land in this crate
//! as later modules of the same track; see `docs/status/02-state-and-model.md` for what has
//! landed so far.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod api;
pub mod auth;
pub mod chain_cover;
pub mod error;
pub mod state_fetch;
pub mod state_res;
pub mod store;

pub use api::{StateDiff, StateStore};
pub use error::{AuthError, AuthResult, StateResError};
