//! `GET .../download/{serverName}/{mediaId}[/{fileName}]`: the authenticated download route.
//! Every response goes through [`crate::security::response_headers`] — see that module for the
//! normative rules this handler exists to enforce, not reinvent.

use axum::extract::{Path, Query, State};
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

/// The query parameters a download takes. `allow_remote` (default `true`) set to `false` asks
/// for another server's media only if a copy is already held: servers asking each other over
/// the legacy path set it, so a request cannot bounce between them.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct DownloadQuery {
    #[serde(default)]
    pub(crate) allow_remote: Option<bool>,
}

impl DownloadQuery {
    pub(crate) fn allow_remote(&self) -> bool {
        self.allow_remote.unwrap_or(true)
    }
}

fn range_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `GET .../download/{serverName}/{mediaId}`. Another server's media is fetched from it on the
/// first request and served from the held copy after that (`crate::remote`).
pub(crate) async fn download<B: KvBackend>(
    State(state): State<MediaState<B>>,
    _requester: MediaRequester,
    Path((server_name, media_id)): Path<(String, String)>,
    Query(query): Query<DownloadQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, MediaError> {
    let record = state
        .repository
        .resolve_record(&server_name, &media_id, query.allow_remote())
        .await?;
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
    Query(query): Query<DownloadQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, MediaError> {
    let record = state
        .repository
        .resolve_record(&server_name, &media_id, query.allow_remote())
        .await?;
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

/// Another server's media through the client download and thumbnail routes: fetched once over
/// the (scripted) federation transport, then served from the held copy.
#[cfg(test)]
mod remote_tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use crate::remote::tests::{ScriptedTransport, multipart_answer};
    use crate::test_support::{router_with_remote, seed_token};

    const PATH: &str = "/_matrix/federation/v1/media/download/remoteid";

    async fn get(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, body)
    }

    fn scripted() -> Arc<ScriptedTransport> {
        let transport = Arc::new(ScriptedTransport::default());
        transport.answer(
            PATH,
            multipart_answer("image/png", "cat.png", &crate::test_fixtures::valid_png()),
        );
        transport
    }

    fn count(metrics: &crate::remote::RemoteMediaMetrics, result: &str) -> u64 {
        metrics
            .requests
            .get_or_create(&crate::remote::CacheLabels {
                result: result.into(),
            })
            .get()
    }

    #[tokio::test]
    async fn remote_media_is_fetched_once_then_served_from_the_copy_with_its_origin_down() {
        let transport = scripted();
        let (app, state, metrics) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;

        let (status, body) = get(&app, &token, "/download/remote.example/remoteid").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, crate::test_fixtures::valid_png());
        let record = state
            .repository
            .metadata()
            .get_media("remote.example", "remoteid")
            .unwrap()
            .expect("the copy is held");
        assert_eq!(record.uploader, None);
        assert_eq!(record.upload_name.as_deref(), Some("cat.png"));

        transport.down.store(true, Ordering::SeqCst);
        let (status, body) = get(&app, &token, "/download/remote.example/remoteid").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, crate::test_fixtures::valid_png());
        let (status, _) = get(
            &app,
            &token,
            "/thumbnail/remote.example/remoteid?width=32&height=32&method=crop",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        assert_eq!(transport.asked().len(), 1, "the origin was asked once");
        assert_eq!(count(&metrics, "miss"), 1);
        assert_eq!(count(&metrics, "hit"), 2);
    }

    #[tokio::test]
    async fn a_remote_thumbnail_is_made_here_from_the_fetched_original() {
        let transport = scripted();
        let (app, state, _) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;
        let (status, body) = get(
            &app,
            &token,
            "/thumbnail/remote.example/remoteid?width=320&height=240&method=scale",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());
        assert_eq!(transport.asked()[0].0, PATH, "the original was fetched");
    }

    #[tokio::test]
    async fn allow_remote_false_does_not_ask_the_origin() {
        let transport = scripted();
        let (app, state, _) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;
        let (status, _) = get(
            &app,
            &token,
            "/download/remote.example/remoteid?allow_remote=false",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(transport.asked().is_empty());
    }

    #[tokio::test]
    async fn a_quarantined_copy_is_not_found_and_not_fetched_again() {
        let transport = scripted();
        let (app, state, _) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;
        let (status, _) = get(&app, &token, "/download/remote.example/remoteid").await;
        assert_eq!(status, StatusCode::OK);
        state
            .repository
            .quarantine("remote.example", "remoteid", Some("@admin:example.org"))
            .unwrap();
        let (status, _) = get(&app, &token, "/download/remote.example/remoteid").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(transport.asked().len(), 1);
    }

    #[tokio::test]
    async fn a_purged_copy_is_fetched_afresh() {
        let transport = scripted();
        let (app, state, _) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;
        let (status, _) = get(&app, &token, "/download/remote.example/remoteid").await;
        assert_eq!(status, StatusCode::OK);
        state
            .repository
            .delete_media("remote.example", "remoteid")
            .await
            .unwrap();
        let (status, _) = get(&app, &token, "/download/remote.example/remoteid").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(transport.asked().len(), 2);
    }

    #[tokio::test]
    async fn an_unreachable_origin_is_502_and_nothing_is_cached() {
        let transport = scripted();
        transport.down.store(true, Ordering::SeqCst);
        let (app, state, _) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;
        let (status, _) = get(&app, &token, "/download/remote.example/remoteid").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            state
                .repository
                .metadata()
                .get_media("remote.example", "remoteid")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_requests_for_one_item_fetch_it_once() {
        let transport = scripted();
        let (app, state, _) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (app, token) = (app.clone(), token.clone());
                tokio::spawn(
                    async move { get(&app, &token, "/download/remote.example/remoteid").await },
                )
            })
            .collect();
        for handle in handles {
            let (status, _) = handle.await.unwrap();
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(transport.asked().len(), 1);
    }

    #[tokio::test]
    async fn a_malformed_server_name_is_refused_before_anything_is_asked() {
        let transport = scripted();
        let (app, state, _) = router_with_remote(transport.clone());
        let token = seed_token(&state, "@alice:example.org").await;
        let (status, _) = get(&app, &token, "/download/bad%20name/remoteid").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(transport.asked().is_empty());
    }
}
