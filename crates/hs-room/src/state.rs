//! [`RoomState`]: this crate's axum shared state, and [`RoomRequester`], the bridge that lets a
//! handler mounted on `Router<RoomState<B>>` take [`hs_auth::requester::Requester`] as a
//! parameter.
//!
//! Follows the pattern `hs-media` established (`crates/hs-media/src/state.rs`) for the same
//! problem: `hs-auth`'s `Requester: FromRequestParts<AuthState>` is written against the concrete
//! `AuthState` type (frozen that way before a generic-over-state version existed -- see
//! `docs/status/07-auth-and-identity.md`'s "Interfaces needed"), so every crate composing
//! `hs-auth` into its own state defines the same one-line bridge: embed `AuthState`, implement
//! `FromRef`, and wrap `Requester` in a newtype implementing `FromRequestParts<OwnState>` by
//! delegating through `AuthState::from_ref`.

use std::sync::Arc;

use axum::extract::FromRef;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_kv::KvBackend;

use crate::identity::HomeserverIdentity;
use crate::registry::RoomRegistry;

/// This crate's axum shared state: the room registry, this server's identity for originating
/// events, and an embedded [`AuthState`] so [`RoomRequester`] can authenticate requests.
#[derive(Clone)]
pub struct RoomState<B: KvBackend> {
    /// Authentication (`hs-auth`'s `AuthState`), embedded so [`AuthState`] can be produced from
    /// `&RoomState<B>` via [`FromRef`] -- see the module docs.
    pub auth: AuthState,
    /// The room registry.
    pub rooms: Arc<RoomRegistry<B>>,
    /// This server's identity, for constructing new rooms and events.
    pub identity: HomeserverIdentity,
}

impl<B: KvBackend> FromRef<RoomState<B>> for AuthState {
    fn from_ref(state: &RoomState<B>) -> AuthState {
        state.auth.clone()
    }
}

/// Wraps [`Requester`] so it can be used as an extractor on `Router<RoomState<B>>`. See the module
/// docs.
#[derive(Debug)]
pub struct RoomRequester(pub Requester);

impl<B: KvBackend> FromRequestParts<RoomState<B>> for RoomRequester {
    type Rejection = hs_http::error::MatrixError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RoomState<B>,
    ) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        Requester::from_request_parts(parts, &auth_state)
            .await
            .map(RoomRequester)
            .map_err(auth_error_to_matrix_error)
    }
}

/// `hs-auth`'s `MatrixError` and `hs-http`'s `MatrixError` are two distinct types (see
/// `docs/status/07-auth-and-identity.md`'s "Interfaces needed": `hs-auth` predates the `hs-http`
/// error-type freeze). This crate is built against `hs-http`'s (the frozen one every other track's
/// handlers use, per this track's own instructions), so every place `hs-auth` hands back its own
/// error type needs this one conversion.
pub(crate) fn auth_error_to_matrix_error(
    e: hs_auth::error::MatrixError,
) -> hs_http::error::MatrixError {
    // `hs-auth`'s `MatrixError` does not expose whether `soft_logout` was set (it is folded into
    // a private `extra` map with no public accessor); this conversion therefore loses that one
    // bit for a rejected `Requester` extraction on this crate's routes. Worth revisiting if a
    // client-visible difference is ever reported -- recorded in
    // `docs/status/04-room-and-events.md`.
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
    fn room_state_is_cloneable() {
        let backend = MemoryBackend::new();
        let registry = RoomRegistry::open(backend, HomeserverIdentity::for_tests("hs1")).unwrap();
        let state = RoomState {
            auth: AuthState::in_memory(),
            rooms: Arc::new(registry),
            identity: HomeserverIdentity::for_tests("hs1"),
        };
        let _ = state.clone();
    }
}
