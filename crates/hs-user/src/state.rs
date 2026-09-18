//! [`UserState`]: this crate's axum shared state, and [`UserRequester`], the bridge that lets a
//! handler mounted on `Router<UserState<B, R>>` take [`hs_auth::requester::Requester`] as a
//! parameter. Mirrors `hs_room::state::RoomState`/`RoomRequester` exactly -- see that module's
//! doc comment for why this same one-line bridge exists in every crate that composes `hs-auth`
//! into its own state.

use std::sync::Arc;

use axum::extract::FromRef;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_kv::KvBackend;

use crate::hub::SessionHub;
use crate::room_source::RoomSource;

/// This crate's axum shared state: the session hub and an embedded [`AuthState`] so
/// [`UserRequester`] can authenticate requests.
pub struct UserState<B: KvBackend, R: RoomSource<B>> {
    /// Authentication (`hs-auth`'s `AuthState`), embedded so [`AuthState`] can be produced from
    /// `&UserState<B, R>` via [`FromRef`].
    pub auth: AuthState,
    /// The session hub.
    pub hub: Arc<SessionHub<B, R>>,
}

impl<B: KvBackend, R: RoomSource<B>> Clone for UserState<B, R> {
    fn clone(&self) -> Self {
        Self {
            auth: self.auth.clone(),
            hub: Arc::clone(&self.hub),
        }
    }
}

impl<B: KvBackend, R: RoomSource<B>> FromRef<UserState<B, R>> for AuthState {
    fn from_ref(state: &UserState<B, R>) -> AuthState {
        state.auth.clone()
    }
}

/// Wraps [`Requester`] so it can be used as an extractor on `Router<UserState<B, R>>`.
#[derive(Debug)]
pub struct UserRequester(pub Requester);

impl<B: KvBackend, R: RoomSource<B>> FromRequestParts<UserState<B, R>> for UserRequester {
    type Rejection = hs_http::error::MatrixError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &UserState<B, R>,
    ) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        Requester::from_request_parts(parts, &auth_state)
            .await
            .map(UserRequester)
            .map_err(auth_error_to_matrix_error)
    }
}

/// See `hs_room::state`'s identical helper: `hs-auth`'s `MatrixError` and `hs-http`'s
/// `MatrixError` are two distinct types (predates the `hs-http` error-type freeze).
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
    use crate::room_source::test_support::registry;
    use crate::store::tables::TablesUserStore;
    use hs_kv::memory::MemoryBackend;

    #[test]
    fn user_state_is_cloneable() {
        let rooms = registry("state.test");
        let store: crate::store::DynUserStore =
            Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        let state = UserState {
            auth: AuthState::in_memory(),
            hub: Arc::new(SessionHub::new(store, rooms, 500)),
        };
        let _ = state.clone();
    }
}
