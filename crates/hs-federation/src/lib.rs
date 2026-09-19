//! `hs-federation`: server-to-server (federation) transport, security and discovery.
//!
//! Track 06. See `docs/workstreams/06-federation.md` for the brief,
//! `docs/design/06-federation-threat-model.md` for the threat model every defence in this crate
//! traces back to, and `docs/status/06-federation.md` for what is implemented, what is a seam, and
//! what is untested.

pub mod acl;
pub mod backfill;
pub mod client;
pub mod destination_store;
pub mod discovery;
pub mod edu;
pub mod error;
pub mod inbound;
pub mod join;
pub mod keys;
pub mod outbound_join;
pub mod room_source;
pub mod transport;
pub mod xmatrix;
