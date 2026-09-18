//! `hs-auth`: authentication and identity.
//!
//! Owned by track 07 (`docs/workstreams/07-auth-and-identity.md`). This crate provides:
//!
//! - Token formats and hashing at rest ([`token`]), matching Synapse's `syt_`/`syr_`/`syl_`
//!   shapes so tokens imported from a Synapse database keep working
//!   (`docs/rfcs/0002-auth-tokens-and-requester.md`).
//! - The [`requester::Requester`] type and the [`middleware`] axum extractor every HTTP handler
//!   in the workspace is meant to use.
//! - Storage traits for users, devices, tokens and UIA sessions ([`store`]), with an in-memory
//!   implementation; a real `hs-tables`-backed one lands later behind an RFC.
//! - Password hashing: Argon2id native, bcrypt verification for imported hashes ([`password`]).
//! - The user-interactive-auth state machine ([`uia`]) and the shared re-auth check
//!   ([`reauth`]) built on it.
//! - The legacy client-server auth endpoints ([`routes`]): `/login`, `/logout`, `/refresh`,
//!   `/register`, `/account/*`, `/devices*`.
//!
//! The native OAuth 2.0 authorization server is design-only for now:
//! `docs/rfcs/0003-native-oauth-issuer.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod appservice;
pub mod clock;
pub mod config;
pub mod error;
pub mod middleware;
pub mod password;
pub mod ratelimit;
pub mod reauth;
pub mod requester;
pub mod routes;
pub mod session;
pub mod shared_secret_auth;
pub mod state;
pub mod store;
pub mod token;
pub mod uia;

pub use error::{ErrCode, MatrixError};
pub use requester::{AppserviceIdentity, Requester, RequesterContext};
pub use state::AuthState;

/// Ruma re-export so consumers pin the same version this crate was built against.
pub use ruma;
