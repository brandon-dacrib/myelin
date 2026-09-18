//! `GET .../download/{serverName}/{mediaId}[/{fileName}]`: the authenticated download route.
//! Every response goes through [`crate::security::response_headers`] — see that module for the
//! normative rules this handler exists to enforce, not reinvent.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;

use crate::error::MediaError;
use crate::metadata::MediaRecord;
use crate::repository::MediaRepository;
use crate::security;
use crate::state::{MediaRequester, MediaState};

/// Builds the actual HTTP response for a servable [`MediaRecord`], given an optional `Range`
/// header and an optional path-supplied filename override (falls back to the uploader's own
/// filename, then to none). Shared between the authenticated handlers in this module and the
/// legacy, unauthenticated handlers in [`crate::routes::legacy`].
pub(crate) async fn build_response<B: KvBackend>(
    repository: &MediaRepository<B>,
    record: &MediaRecord,
    filename_override: Option<&str>,
    range_header: Option<&str>,
) -> Result<Response, MediaError> {
    let content = repository.get_content(record, range_header).await?;
    let filename = filename_override.or(record.upload_name.as_deref());
    let mut headers = security::response_headers(&record.content_type, filename);
    let status = if content.range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    if let Some(range) = &content.range
        && let Ok(value) = HeaderValue::from_str(&range.content_range_header())
    {
        headers.insert(header::CONTENT_RANGE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&content.bytes.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    Ok((status, headers, content.bytes).into_response())
}

fn range_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `GET .../download/{serverName}/{mediaId}`.
pub(crate) async fn download<B: KvBackend>(
    State(state): State<MediaState<B>>,
    _requester: MediaRequester,
    Path((server_name, media_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, MediaError> {
    let record = state.repository.get_record(&server_name, &media_id)?;
    build_response(
        &state.repository,
        &record,
        None,
        range_header(&headers).as_deref(),
    )
    .await
}

/// `GET .../download/{serverName}/{mediaId}/{fileName}`: the same content, with the path's
/// `fileName` taking priority over the uploader's own filename for `Content-Disposition`.
pub(crate) async fn download_with_filename<B: KvBackend>(
    State(state): State<MediaState<B>>,
    _requester: MediaRequester,
    Path((server_name, media_id, file_name)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, MediaError> {
    let record = state.repository.get_record(&server_name, &media_id)?;
    build_response(
        &state.repository,
        &record,
        Some(&file_name),
        range_header(&headers).as_deref(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{router, seed_token, upload_fixture};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn download_returns_content_with_security_headers() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/example.org/{media_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
    }

    #[tokio::test]
    async fn html_masquerading_as_image_is_never_served_inline() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;
        // Overwrite the record's content type directly to simulate a client that lied about
        // Content-Type at upload time (the spec requires the server accept and store whatever the
        // client declares -- rejecting mismatched bytes at upload time is not this crate's job;
        // what matters is this response's Content-Disposition, checked below).
        state
            .repository
            .metadata()
            .complete_upload("example.org", &media_id, "text/html", 10)
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/example.org/{media_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment"
        );
    }

    #[tokio::test]
    async fn range_request_returns_206_with_content_range() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/example.org/{media_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::RANGE, "bytes=0-3")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(response.headers().get(header::CONTENT_RANGE).is_some());
    }

    #[tokio::test]
    async fn unknown_media_is_404() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/download/example.org/doesnotexist")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn download_without_auth_is_rejected() {
        let (app, state) = router();
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
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn filename_in_path_overrides_upload_name() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let media_id = upload_fixture(
            &state,
            "@alice:example.org",
            "image/png",
            Some("original.png"),
        )
        .await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/example.org/{media_id}/override.png"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disposition.contains("override.png"));
    }
}
