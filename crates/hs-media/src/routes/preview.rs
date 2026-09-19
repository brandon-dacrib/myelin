//! `GET .../preview_url` (spec: "Getting URL previews"). See [`crate::preview`]'s module doc for
//! the SSRF guard, size cap, timeout and cache this delegates to.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::error::MediaError;
use crate::state::{MediaRequester, MediaState};
use hs_kv::KvBackend;

#[derive(Debug, Deserialize)]
pub(crate) struct PreviewQuery {
    url: String,
    /// The spec's `ts` parameter (the point in time the URL was included in a message, used as a
    /// hint for which snapshot of the page to prefer). Accepted for wire compatibility but not
    /// yet used to select between multiple cached snapshots of the same URL — this server caches
    /// one response per URL regardless of `ts` (see `crate::preview`'s module doc). Recorded as a
    /// known simplification in `docs/status/09-media.md`, not silently dropped.
    #[serde(default)]
    #[allow(dead_code)]
    ts: Option<u64>,
}

/// `GET .../preview_url?url=...&ts=...`.
pub(crate) async fn preview_url<B: KvBackend>(
    State(state): State<MediaState<B>>,
    _requester: MediaRequester,
    Query(query): Query<PreviewQuery>,
) -> Result<Json<serde_json::Value>, MediaError> {
    let value = state.repository.preview_url(&query.url).await?;
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use crate::test_support::{legacy_router, router, seed_token};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    #[tokio::test]
    async fn preview_url_is_403_when_disabled() {
        // `crate::test_support::router` builds a repository from `MediaConfig::default()`, which
        // has `url_preview_enabled: false` — this is the "an operator never turned it on" case,
        // not the SSRF guard (that is `crate::preview`'s own test module).
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/preview_url?url=https://example.org/")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn preview_url_requires_authentication() {
        let (app, _state) = router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/preview_url?url=https://example.org/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Was a 404 before this session (`docs/status/14-test-and-conformance.md`'s "Track 09
    /// (media)" gap) because no route existed under either mount at all. Proves the route now
    /// exists and reaches this crate's own logic (a `403` from `preview_url` being disabled, not
    /// a router-level `404`) on both the authenticated and the legacy mount.
    #[tokio::test]
    async fn preview_url_route_exists_on_both_routers() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/preview_url?url=https://example.org/")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::NOT_FOUND);

        let (legacy_app, legacy_state) = legacy_router(None);
        let legacy_token = seed_token(&legacy_state, "@bob:example.org").await;
        let legacy_response = legacy_app
            .oneshot(
                Request::builder()
                    .uri("/preview_url?url=https://example.org/")
                    .header(header::AUTHORIZATION, format!("Bearer {legacy_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(legacy_response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn preview_url_requires_the_url_query_parameter() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/preview_url")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
