//! [`E2eState`]: this crate's axum shared state, and [`E2eRequester`], the bridge that lets a
//! handler mounted on `Router<E2eState<B>>` take [`hs_auth::requester::Requester`] as a
//! parameter.
//!
//! Follows the pattern `hs-room` established for the same problem (`crates/hs-room/src/state.rs`,
//! itself following `hs-media`'s): `hs-auth`'s `Requester: FromRequestParts<AuthState>` is
//! written against the concrete `AuthState` type, so every crate composing `hs-auth` into its own
//! state defines the same one-line bridge: embed `AuthState`, implement `FromRef`, and wrap
//! `Requester` in a newtype implementing `FromRequestParts<OwnState>` by delegating through
//! `AuthState::from_ref`.

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_kv::KvBackend;

use crate::store::E2eStore;

/// This crate's axum shared state: the e2e store and an embedded [`AuthState`] so
/// [`E2eRequester`] can authenticate requests.
#[derive(Clone)]
pub struct E2eState<B: KvBackend> {
    /// Authentication (`hs-auth`'s `AuthState`), embedded so [`AuthState`] can be produced from
    /// `&E2eState<B>` via [`FromRef`] — see the module docs.
    pub auth: AuthState,
    /// Device keys, one-time/fallback keys, cross-signing keys, backups and to-device storage.
    pub store: Arc<dyn E2eStore>,
    /// Marker so `B` (the backend `E2eState` was constructed over) is nameable in code that
    /// otherwise only touches `store` through the trait object — kept even though `store` itself
    /// erases `B`, so `E2eState<B>: FromRequestParts` bounds line up the same way `RoomState<B>`'s
    /// do.
    _backend: std::marker::PhantomData<B>,
}

impl<B: KvBackend> E2eState<B> {
    /// Builds an `E2eState` around an already-open store and an existing [`AuthState`] (sharing
    /// its appservice registry, rate limiter and clock with whatever other crate's state also
    /// embeds it in the same process).
    #[must_use]
    pub fn new(auth: AuthState, store: Arc<dyn E2eStore>) -> Self {
        Self {
            auth,
            store,
            _backend: std::marker::PhantomData,
        }
    }
}

impl<B: KvBackend> FromRef<E2eState<B>> for AuthState {
    fn from_ref(state: &E2eState<B>) -> AuthState {
        state.auth.clone()
    }
}

/// Wraps [`Requester`] so it can be used as an extractor on `Router<E2eState<B>>`. See the module
/// docs.
#[derive(Debug)]
pub struct E2eRequester(pub Requester);

impl<B: KvBackend> FromRequestParts<E2eState<B>> for E2eRequester {
    type Rejection = hs_http::error::MatrixError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &E2eState<B>,
    ) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        Requester::from_request_parts(parts, &auth_state)
            .await
            .map(E2eRequester)
            .map_err(auth_error_to_matrix_error)
    }
}

/// `hs-auth`'s `MatrixError` and `hs-http`'s `MatrixError` are two distinct types (see
/// `docs/status/07-auth-and-identity.md`'s "Interfaces needed"). This crate is built against
/// `hs-http`'s, so every place `hs-auth` hands back its own error type needs this one conversion
/// — copied from `hs-room`'s identical bridge rather than re-derived, so the two stay consistent.
fn auth_error_to_matrix_error(e: hs_auth::error::MatrixError) -> hs_http::error::MatrixError {
    hs_http::error::MatrixError::custom(
        e.status(),
        hs_http::error::MatrixErrorCode::Other(e.errcode().as_str().to_owned()),
        e.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    #[test]
    fn e2e_state_is_cloneable() {
        let backend = MemoryBackend::new();
        let store = crate::store::tables::TablesE2eStore::open(backend).unwrap();
        let state: E2eState<MemoryBackend> = E2eState::new(AuthState::in_memory(), Arc::new(store));
        let _ = state.clone();
    }
}
