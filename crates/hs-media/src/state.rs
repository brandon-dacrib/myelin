//! [`MediaState`]: this crate's axum shared state, and the bridge that lets a handler mounted on
//! `Router<MediaState<B>>` take [`hs_auth::requester::Requester`] as a parameter even though that
//! type's [`axum::extract::FromRequestParts`] impl is written against `hs_auth::state::AuthState`
//! specifically (frozen that way at `docs/workstreams/README.md`'s week-6 seam, before a
//! generic-over-state version existed — see [`MediaRequester`]'s doc for the decision this
//! records).

use std::sync::Arc;

use axum::extract::FromRef;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_kv::KvBackend;

use crate::error::MediaError;
use crate::repository::MediaRepository;

/// This crate's axum shared state: the media repository, this crate's own config knobs, and an
/// embedded [`AuthState`] so [`MediaRequester`] can authenticate requests.
#[derive(Clone)]
pub struct MediaState<B: KvBackend> {
    /// Authentication (`hs-auth`'s `AuthState`), embedded rather than referenced so
    /// [`AuthState`] can be produced from `&MediaState<B>` via [`FromRef`] — see the module docs.
    pub auth: AuthState,
    /// The media repository.
    pub repository: Arc<MediaRepository<B>>,
    /// Whether the legacy, unauthenticated `/_matrix/media/v3/*` routes are mounted at all
    /// (`hs_config::media::MediaConfig::allow_legacy_unauthenticated_media`).
    pub legacy_media_enabled: bool,
    /// The freeze cutover: on the legacy routes, media created at or after this time is not
    /// servable (only pre-existing media stays reachable through the unauthenticated path). See
    /// `crate::routes::legacy`'s module doc for the full freeze-semantics rationale.
    pub legacy_freeze_ms: Option<u64>,
}

impl<B: KvBackend> FromRef<MediaState<B>> for AuthState {
    fn from_ref(state: &MediaState<B>) -> AuthState {
        state.auth.clone()
    }
}

/// Wraps [`Requester`] so it can be used as an extractor on `Router<MediaState<B>>`.
///
/// # Why this wrapper exists
///
/// `hs-auth`'s `Requester: FromRequestParts<AuthState>` is written against the concrete
/// `AuthState` type (see `docs/status/07-auth-and-identity.md`'s "Interfaces needed": track 07
/// deliberately left mounting and state composition to whichever crate owns a listener, since
/// that seam was not yet frozen when this crate was built). A handler on `Router<MediaState<B>>`
/// needs an extractor implementing `FromRequestParts<MediaState<B>>`, not
/// `FromRequestParts<AuthState>` — so this crate defines a one-line bridge: build an `AuthState`
/// from `MediaState<B>` via [`FromRef`] (cheap — every field is an `Arc`) and delegate.
///
/// Recorded as a decision other tracks composing `hs-auth` into their own state should know about
/// (`docs/status/09-media.md`): the same pattern (a thin per-crate wrapper implementing
/// `FromRequestParts<OwnState>` by delegating through `AuthState::from_ref`) works for any crate
/// in the same position, and is preferable to changing `hs-auth`'s own extractor to be generic
/// over state, which would need every existing caller to specify a state type parameter.
#[derive(Debug)]
pub struct MediaRequester(pub Requester);

impl<B: KvBackend> FromRequestParts<MediaState<B>> for MediaRequester {
    type Rejection = MediaError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &MediaState<B>,
    ) -> Result<Self, Self::Rejection> {
        let auth_state = AuthState::from_ref(state);
        Requester::from_request_parts(parts, &auth_state)
            .await
            .map(MediaRequester)
            .map_err(MediaError::Auth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_config::MediaConfig;
    use hs_kv::memory::MemoryBackend;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;

    fn state() -> MediaState<MemoryBackend> {
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metadata = crate::metadata::MetadataStore::open(MemoryBackend::new()).unwrap();
        let repository = MediaRepository::new(
            object_store,
            metadata,
            Arc::new(MediaConfig::default()),
            Arc::new(crate::policy::InMemoryQuotaPolicy::unlimited()),
            crate::thumbnail::ThumbnailPolicy::default(),
            "example.org".to_string(),
            || 1_000_000,
        );
        MediaState {
            auth: AuthState::in_memory(),
            repository: Arc::new(repository),
            legacy_media_enabled: true,
            legacy_freeze_ms: None,
        }
    }

    #[tokio::test]
    async fn missing_token_is_rejected_through_the_bridge() {
        use axum::body::Body;
        use axum::http::Request;

        let s = state();
        let (mut parts, _) = Request::builder()
            .uri("/x")
            .body(Body::empty())
            .unwrap()
            .into_parts();
        let err = MediaRequester::from_request_parts(&mut parts, &s)
            .await
            .unwrap_err();
        let MediaError::Auth(inner) = &err else {
            panic!("expected MediaError::Auth");
        };
        assert_eq!(inner.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(
            err.to_matrix_error().status,
            axum::http::StatusCode::UNAUTHORIZED
        );
    }
}
