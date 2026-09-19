//! `hs-push`: the push rules engine, notification counts, and pushers (track 10,
//! `docs/workstreams/10-push.md`).
//!
//! Built on Ruma's `ruma::push` types rather than a reimplementation of them, per
//! `docs/decisions/0007-build-less-reuse-more.md`: `Ruleset::iter` already yields rules in the
//! spec's priority order and `AnyPushRuleRef::applies` already implements every condition kind.
//! What this crate owns is everything around that — where a user's rules live, how they are cached
//! so a busy room does not re-parse them once per recipient, what the outcome of an evaluation
//! means, and where the resulting notification goes.
//!
//! # Modules
//!
//! - [`engine`]: [`engine::evaluate`], one event against one recipient's ruleset, yielding
//!   [`engine::EvaluationOutcome`] (notify, highlight, sound, tweaks).
//! - [`context`]: [`context::PushEvaluationInput`], the shape the room actor must publish for an
//!   event to be evaluated, and [`context::build_room_ctx`], which narrows it to one recipient's
//!   `ruma::push::PushConditionRoomCtx`.
//! - [`compiled`]: [`compiled::RuleCache`], the per-user `Arc<Ruleset>` cache that keeps the
//!   per-recipient hot path off the store, with write-through invalidation.
//! - [`rulesets`]: [`rulesets::RulesetStore`] and the cached read path evaluation actually calls
//!   ([`rulesets::CachedRulesetStore`]), plus the in-memory and `hs-tables` implementations.
//! - [`counts`]: [`counts::CountsStore`], the single source of truth for the notification and
//!   highlight counts `/sync` reports (see that module's "One source of truth" note — nothing else
//!   may compute them).
//! - [`notification_log`]: the append-only per-user log `/notifications` pages over, deliberately
//!   separate from the aggregate counts.
//! - [`pushers`]: [`pushers::PusherStore`] for `/pushers`, and [`pushers::http`], the Push Gateway
//!   API client with retry and backoff.
//! - [`routes`]: the client-server HTTP endpoints, as a router fragment ([`routes::router`]).
//! - [`state`]: [`state::PushState`], this crate's axum shared state, and
//!   [`state::PushRequester`], the `hs-auth` `Requester` bridge.
//! - [`error`]: [`error::StoreError`], mirroring `hs_auth::store::StoreError`'s shape.

#![allow(
    clippy::result_large_err,
    reason = "MatrixError is the workspace's standard Matrix-shaped error response type (hs-http). \
              It is 144 bytes, over clippy's 128-byte threshold, so every handler returning \
              `Result<_, MatrixError>` trips this lint; crates/hs-appservice/src/routes.rs and \
              crates/hs-admin/src/router.rs carry the same allow per call site. The real fix is to \
              shrink MatrixError itself (its `extra` map is the bulk of it), which is an hs-http \
              change touching every crate and wants its own pass."
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod compiled;
pub mod context;
pub mod counts;
pub mod engine;
pub mod error;
pub mod notification_log;
pub mod pushers;
pub mod routes;
pub mod rulesets;
pub mod state;

pub use error::StoreError;
