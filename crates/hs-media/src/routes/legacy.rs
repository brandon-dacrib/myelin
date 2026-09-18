//! The legacy, unauthenticated `/_matrix/media/v3/*` download and thumbnail routes, mounted only
//! when `hs_config::media::MediaConfig::allow_legacy_unauthenticated_media` is set
//! ([`crate::router::legacy_router`]), with Synapse's freeze semantics.
//!
//! # Freeze semantics
//!
//! The spec's authenticated-media transition (v1.11) leaves what to do with the old,
//! unauthenticated `/_matrix/media/v3/download` and `/_matrix/media/v3/thumbnail` up to the
//! server. Synapse's answer (`enable_authenticated_media`, observed 1.161 behavior, no code
//! copied): once authenticated media is the primary path, the legacy endpoints do not simply keep
//! serving everything unauthenticated forever — that would make the authentication requirement on
//! the new endpoints pointless, since any client could fall back to the old ones. Instead, the
//! legacy endpoints are **frozen**: they keep serving media that existed *before* the cutover
//! (so old links embedded in already-sent messages, old clients that only know the legacy path,
//! and federated servers that cached a legacy URL keep working), but any media created *after*
//! the cutover is `404` on the legacy path — it is only reachable through the authenticated
//! route. [`crate::state::MediaState::legacy_freeze_ms`] is that cutover instant; `None` means "no
//! freeze configured", which this crate treats as "serve everything on the legacy path too"
//! (matching `allow_legacy_unauthenticated_media: true` with no freeze date, the permissive
//! development-friendly default — see `docs/status/09-media.md` for why this default was chosen
//! over always requiring an explicit freeze date).

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::header;
use axum::response::Response;
use hs_kv::KvBackend;

use crate::error::MediaError;
use crate::metadata::MediaRecord;
use crate::state::MediaState;

use super::download::build_response as build_download_response;
use super::thumbnail::{ThumbnailQuery, build_response as build_thumbnail_response};

fn range_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Rejects (as [`MediaError::NotFound`], matching what an authenticated caller would see for
/// media that genuinely does not exist — never distinguishing "exists but frozen" from "does not
/// exist" to an unauthenticated caller) any record created at or after the configured freeze.
fn check_not_frozen<B: KvBackend>(
    state: &MediaState<B>,
    record: &MediaRecord,
) -> Result<(), MediaError> {
    if let Some(freeze) = state.legacy_freeze_ms
        && record.created_ms >= freeze
    {
        return Err(MediaError::NotFound);
    }
    Ok(())
}

/// `GET /_matrix/media/v3/download/{serverName}/{mediaId}` (unauthenticated, frozen).
pub(crate) async fn legacy_download<B: KvBackend>(
    State(state): State<MediaState<B>>,
    Path((server_name, media_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, MediaError> {
    let record = state.repository.get_record(&server_name, &media_id)?;
    check_not_frozen(&state, &record)?;
    build_download_response(
        &state.repository,
        &record,
        None,
        range_header(&headers).as_deref(),
    )
    .await
}

/// `GET /_matrix/media/v3/download/{serverName}/{mediaId}/{fileName}` (unauthenticated, frozen).
pub(crate) async fn legacy_download_with_filename<B: KvBackend>(
    State(state): State<MediaState<B>>,
    Path((server_name, media_id, file_name)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, MediaError> {
    let record = state.repository.get_record(&server_name, &media_id)?;
    check_not_frozen(&state, &record)?;
    build_download_response(
        &state.repository,
        &record,
        Some(&file_name),
        range_header(&headers).as_deref(),
    )
    .await
}

/// `GET /_matrix/media/v3/thumbnail/{serverName}/{mediaId}` (unauthenticated, frozen).
pub(crate) async fn legacy_thumbnail<B: KvBackend>(
    State(state): State<MediaState<B>>,
    Path((server_name, media_id)): Path<(String, String)>,
    Query(query): Query<ThumbnailQuery>,
) -> Result<Response, MediaError> {
    let record = state.repository.get_record(&server_name, &media_id)?;
    check_not_frozen(&state, &record)?;
    build_thumbnail_response(&state.repository, &record, &query).await
}

#[cfg(test)]
mod tests {
    use crate::test_support::{legacy_router, seed_token, upload_fixture};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    #[tokio::test]
    async fn legacy_download_works_without_authentication() {
        let (app, state) = legacy_router(None);
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/example.org/{media_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn legacy_upload_still_requires_authentication() {
        let (app, _state) = legacy_router(None);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .body(Body::from(crate::test_fixtures::valid_png()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn media_created_after_the_freeze_is_hidden_on_the_legacy_path() {
        // Freeze at time 0: anything "created" (this test harness's clock always returns a fixed
        // instant >= 0) after the cutover is 404 on the legacy path.
        let (app, state) = legacy_router(Some(0));
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/example.org/{media_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn media_created_before_the_freeze_still_serves_on_the_legacy_path() {
        // Freeze far in the future: the fixture (created "now") predates it.
        let (app, state) = legacy_router(Some(u64::MAX));
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/example.org/{media_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn legacy_authenticated_token_still_works_for_upload() {
        let (app, state) = legacy_router(None);
        let token = seed_token(&state, "@alice:example.org").await;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(crate::test_fixtures::valid_png()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
