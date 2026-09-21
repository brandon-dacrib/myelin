//! `hs-auth`: authentication and identity.
//!
//! Owned by track 07 (`docs/workstreams/07-auth-and-identity.md`). This crate provides:
//!
//! - Token formats and hashing at rest ([`token`]), matching Synapse's `syt_`/`syr_`/`syl_`
//!   shapes so tokens imported from a Synapse database keep working
//!   (`docs/rfcs/0002-auth-tokens-and-requester.md`).
//! - The [`requester::Requester`] type and the [`middleware`] axum extractor every HTTP handler
//!   in the workspace is meant to use.
//! - Storage traits for users, devices, tokens and UIA sessions ([`store`]), with two
//!   implementations: an in-memory one for tests ([`store::memory::InMemoryAuthStore`]) and a
//!   persistent one over `hs-kv`/`hs-tables` ([`store::tables::TablesAuthStore`]) for a real
//!   `hs serve` process that must survive a restart.
//! - Password hashing: Argon2id native, bcrypt verification for imported hashes ([`password`]).
//! - The user-interactive-auth state machine ([`uia`]) and the shared re-auth check
//!   ([`reauth`]) built on it.
//! - The legacy client-server auth endpoints ([`routes`]): `/login`, `/logout`, `/refresh`,
//!   `/register`, `/account/*`, `/devices*`.
//! - [`admin_verifier::AdminTokenVerifier`]: the `hs_admin::auth::TokenVerifier` implementation
//!   `hs serve` wires into the admin API, over this crate's own user/token storage.
//! - [`admin_directory::AuthStoreUserDirectory`]: the `hs_admin::sources::UserDirectory`
//!   implementation `hs serve` wires into the admin API's `/users` surface, over the same store.
//! - [`synapse_admin_router`]: the `/_synapse/admin/v1/register` shared-secret admin registration
//!   router fragment, mounted separately from [`routes::router`] (see that function's own doc
//!   comment for why).
//!
//! The native OAuth 2.0 authorization server is design-only for now:
//! `docs/rfcs/0003-native-oauth-issuer.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admin_directory;
pub mod admin_verifier;
pub mod appservice;
pub mod clock;
pub mod config;
pub mod error;
pub mod local_user;
pub mod middleware;
pub mod password;
pub mod ratelimit;
pub mod reauth;
pub mod requester;
pub mod routes;
pub mod session;
pub mod setup;
pub mod shared_secret_auth;
pub mod state;
pub mod store;
pub mod token;
pub mod uia;

pub use error::{ErrCode, MatrixError};
pub use requester::{AppserviceIdentity, Requester, RequesterContext};
pub use state::AuthState;

/// The `/_synapse/admin/v1/register` shared-secret registration router fragment
/// ([`routes::synapse_admin::router`]), re-exported at the crate root so callers do not need to
/// reach into `routes::synapse_admin` directly. Mount this **separately** from
/// [`routes::router()`] -- it is not nested under `/_matrix/client/v3`, it is an absolute path at
/// the server root. See `docs/status/07-auth-and-identity.md` for the exact `hs serve` line.
pub fn synapse_admin_router() -> axum::Router<AuthState> {
    routes::synapse_admin::router()
}

/// Ruma re-export so consumers pin the same version this crate was built against.
pub use ruma;
