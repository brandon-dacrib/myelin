//! hs-admin: the native admin API under `/api/v1`.
//!
//! See `docs/rfcs/0004-admin-api.md` for the design (resource model, naming, pagination,
//! filtering, errors, idempotency, scopes, versioning, audit log, event stream) and
//! `docs/status/15-admin-api-and-modules.md` for what is built so far.
//!
//! - [`model`]: the common schemas (`Page`, `Task`, `Principal`, `AuditEntry`, `Event`, `Scope`, ...).
//! - [`auth`]: the [`auth::TokenVerifier`] trait track 07 implements, and scope enforcement.
//! - [`audit`]: the [`audit::AuditSink`] trait and an in-memory implementation.
//! - [`events`]: the SSE event bus (publish, subscribe, replay buffer).
//! - [`idempotency`]: the in-process `Idempotency-Key` cache mutating handlers use.
//! - [`operations`]: the operation table generated alongside `openapi/openapi.yaml`.
//! - [`router`]: the axum router built from that table (real handlers for a first slice of
//!   operations, `501` for the rest).
//! - [`sources`]: consumer-defined data-source traits (`UserDirectory`, ...) the real handlers
//!   call, implemented elsewhere and wired onto [`router::AdminState`].
//! - [`assets`]: serves the management interface's built assets at `/admin/`.
//! - [`openapi`]: the embedded OpenAPI document.
//!
//! The `hs-admin-mock` binary (`src/bin/hs-admin-mock.rs`) is a separate, self-contained fixture
//! server for track 16 to develop against; it does not depend on this library's router skeleton
//! (which answers `501` for every operation) so that it can return realistic data instead.

pub mod assets;
pub mod audit;
pub mod auth;
pub mod events;
pub mod idempotency;
pub mod model;
pub mod openapi;
pub mod operations;
pub mod router;
pub mod sources;
