//! [`PushState`]: this crate's axum shared state, and [`PushRequester`], the bridge that lets a
//! handler mounted on `Router<PushState<B>>` take [`hs_auth::requester::Requester`] as a
//! parameter. Follows the same pattern `hs-room` and `hs-media` use
//! (`crates/hs-room/src/state.rs`'s own doc comment explains why the bridge type exists at all:
//! `hs-auth`'s `Requester: FromRequestParts<AuthState>` is written against the concrete
//! `AuthState` type).

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_kv::KvBackend;

use crate::compiled::RuleCache;
use crate::counts::CountsStore;
use crate::pushers::PusherStore;
use crate::pushers::http::HttpPusherClient;
use crate::rulesets::CachedRulesetStore;
use crate::rulesets::tables::TablesRulesetStore;

/// This crate's axum shared state: authentication, and every store the `/pushrules`, `/pushers`
/// and `/notifications` handlers need.
#[derive(Clone)]
pub struct PushState<B: KvBackend> {
    /// Authentication, embedded so [`AuthState`] can be produced from `&PushState<B>` via
    /// [`FromRef`].
    pub auth: AuthState,
    /// Per-user push rulesets, cached (`crate::compiled`).
    pub rulesets: Arc<CachedRulesetStore<TablesRulesetStore<B>>>,
    /// Pusher storage.
    pub pushers: Arc<dyn PusherStore>,
    /// Notification and highlight counts.
    pub counts: Arc<dyn CountsStore>,
    /// The HTTP pusher client (retry/backoff against a Push Gateway API-compatible gateway).
    pub http_pushers: Arc<HttpPusherClient>,
}

impl<B: KvBackend> FromRef<PushState<B>> for AuthState {
    fn from_ref(state: &PushState<B>) -> AuthState {
        state.auth.clone()
    }
}

/// The shared rule cache, exposed for the room-update consumer (`crate::pushers` evaluation loop)
/// to invalidate independently of `PushState`, kept as a bare re-export of the type rather than a
/// second wrapper.
pub type SharedRuleCache = Arc<RuleCache>;

/// Wraps [`Requester`] so it can be used as an extractor on `Router<PushState<B>>`.
#[derive(Debug)]
pub struct PushRequester(pub Requester);

impl<B: KvBackend> FromRequestParts<PushState<B>> for PushRequester {
    type Rejection = hs_http::error::MatrixError;

    async fn from_request_parts(parts: &mut Parts, state: &PushState<B>) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        Requester::from_request_parts(parts, &auth_state)
            .await
            .map(PushRequester)
            .map_err(auth_error_to_matrix_error)
    }
}

fn auth_error_to_matrix_error(e: hs_auth::error::MatrixError) -> hs_http::error::MatrixError {
    hs_http::error::MatrixError::custom(
        e.status(),
        hs_http::error::MatrixErrorCode::Other(e.errcode().as_str().to_owned()),
        e.to_string(),
    )
}
