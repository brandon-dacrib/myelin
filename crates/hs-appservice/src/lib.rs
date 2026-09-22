//! `hs-appservice`: the application-service registry, registration parser and transaction
//! scheduler.
//!
//! Owned by track 11 (`docs/workstreams/11-appservices-and-bridges.md`). See `PLAN.md` section 8
//! and Appendix B for the design and the measured mautrix inventory this crate is built from.
//!
//! - [`registration`]: the registration file format ([`registration::Registration`]), covering
//!   every field Appendix B lists, including the vendor-prefixed ones.
//! - [`namespace`] and [`regexp`]: namespace declarations and Python-`re`-compatible pattern
//!   matching (`regex` first, `fancy_regex` fallback for lookaround/backreferences).
//! - [`store`]: the `hs-tables`/`hs-kv` storage layer ([`store::AppserviceStore`]).
//! - [`registry`]: the operational façade ([`registry::Registry`]) — add, list, update, pause,
//!   resume, remove, rotate tokens, import, namespace conflict detection, health and backlog.
//! - [`transaction`]: the transaction body ([`transaction::Transaction`]) with every stable and
//!   legacy key spelling Appendix B lists.
//! - [`scheduler`]: the per-appservice delivery scheduler ([`scheduler::Scheduler`]) — ordered
//!   delivery, batching, retry with backoff, dead-letter and replay.
//! - [`ping`]: ping in both directions ([`ping::PingService`]) plus the inbound axum route
//!   ([`routes::ping_router`]).
//! - [`query`]: the outbound user/room-alias query protocol and third-party lookups
//!   ([`query::QueryService`]).
//! - [`auth_registry`]: the [`hs_auth::appservice::AppserviceRegistry`] implementation over
//!   [`registry::Registry`], replacing track 07's stub.
//! - [`error`]: [`error::AppserviceError`], the crate's error type.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admin_directory;
pub mod auth_registry;
pub mod delivery;
pub mod error;
pub mod namespace;
pub mod ping;
pub mod pump;
pub mod query;
pub mod regexp;
pub mod registration;
pub mod registry;
pub mod routes;
pub mod scheduler;
pub mod store;
pub mod transaction;

pub use error::AppserviceError;
pub use registration::Registration;
pub use registry::Registry;
pub use scheduler::Scheduler;
