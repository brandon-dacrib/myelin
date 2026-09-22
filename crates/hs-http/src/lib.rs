//! hs-http: shared HTTP conventions used by every listener in the homeserver.
//!
//! Owned jointly by tracks 07, 14 and 15 (track 15 wrote the initial version). See
//! `docs/workstreams/15-admin-api-and-modules.md` and `docs/rfcs/0004-admin-api.md` (the admin
//! API contract that motivates several of these conventions) and `docs/rfcs/0005-routes-json-manifest.md`
//! (the `routes.json` format `router` emits).
//!
//! - [`error`]: the Matrix client-server error shape (`{errcode, error, ...}`) for `/_matrix/*`
//!   and `/_synapse/*`.
//! - [`problem`]: RFC 9457 problem details for `/api/v1`.
//! - [`body`]: permissive JSON parsing for Matrix routes, strict JSON parsing for the admin API.
//! - [`router`]: builds an `axum::Router` while recording a `routes.json` manifest, and a helper
//!   to assert that manifest agrees with an OpenAPI document.
//! - [`cors`]: the admin API's CORS layer.
//! - [`listener`]: TCP, TLS and unix-socket listener configuration.
//! - [`ratelimit`]: the `RateLimiter` trait every listener enforces against.
//! - [`time`]: RFC 3339 timestamp formatting at millisecond precision.

pub mod body;
pub mod client;
pub mod cors;
pub mod error;
pub mod fallback;
pub mod listener;
pub mod problem;
pub mod ratelimit;
pub mod router;
pub mod time;

pub use error::{MatrixError, MatrixErrorCode};
pub use fallback::apply as apply_fallbacks;
pub use problem::{Problem, ValidationError};
pub use router::{AuthKind, Builder, Route, RouteManifest, RouteMeta, Surface};
