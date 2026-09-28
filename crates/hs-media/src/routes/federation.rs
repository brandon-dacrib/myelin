//! `GET /_matrix/federation/v1/media/download/{mediaId}` and `.../thumbnail/{mediaId}`: this
//! server's own media, served to other servers (spec v1.11, MSC3916).
//!
//! Both answer `multipart/mixed` ([`crate::multipart::build_media_response`]): a JSON part
//! (`{}`), then the content with its `Content-Type` and `Content-Disposition`. Only media this
//! server owns is served -- a server asks the origin, never a server holding a copy -- and a
//! quarantined item is not-found here as it is to a local client.
//!
//! These handlers do no authentication of their own: the `X-Matrix` verification layer is
//! applied over the router they are built into by whoever mounts it (`hs-cli`, with
//! `hs_federation::transport::behind_x_matrix`), the same way every other federation route is
//! covered. [`crate::router::federation_router`] registers them.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;

use crate::error::MediaError;
use crate::multipart;
use crate::security;
use crate::state::MediaState;
use crate::thumbnail;

use super::thumbnail::ThumbnailQuery;

fn multipart_response(content_type: &str, filename: Option<&str>, content: &[u8]) -> Response {
    let safe = security::response_headers(content_type, filename);
    let mut part_headers: Vec<(&str, &str)> = Vec::new();
    if let Some(ct) = safe.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        part_headers.push(("Content-Type", ct));
    }
    if let Some(cd) = safe
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
    {
        part_headers.push(("Content-Disposition", cd));
    }
    let (response_type, body) = multipart::build_media_response(&part_headers, content);
    let mut response = (StatusCode::OK, body).into_response();
    if let Ok(value) = HeaderValue::from_str(&response_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// `GET /_matrix/federation/v1/media/download/{mediaId}`.
pub(crate) async fn federation_download<B: KvBackend>(
    State(state): State<MediaState<B>>,
    Path(media_id): Path<String>,
) -> Result<Response, MediaError> {
    let repository = &state.repository;
    let record = repository.get_record(repository.server_name(), &media_id)?;
    let content = repository.get_content(&record, None).await?;
    tracing::debug!(
        media_id,
        bytes = content.bytes.len(),
        "served media over federation"
    );
    Ok(multipart_response(
        &record.content_type,
        record.upload_name.as_deref(),
        &content.bytes,
    ))
}

/// `GET /_matrix/federation/v1/media/thumbnail/{mediaId}?width=&height=&method=`.
pub(crate) async fn federation_thumbnail<B: KvBackend>(
    State(state): State<MediaState<B>>,
    Path(media_id): Path<String>,
    Query(query): Query<ThumbnailQuery>,
) -> Result<Response, MediaError> {
    let repository = &state.repository;
    let record = repository.get_record(repository.server_name(), &media_id)?;
    let method = thumbnail::parse_method(query.method())?;
    let (thumb, bytes) = repository
        .get_thumbnail(&record, query.width(), query.height(), method)
        .await?;
    Ok(multipart_response(&thumb.content_type, None, &bytes))
}

#[cfg(test)]
mod tests {
    use crate::multipart;
    use crate::test_support::{federation_router, upload_fixture};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    async fn get(app: axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, content_type, body)
    }

    #[tokio::test]
    async fn a_download_is_two_parts_json_then_the_content() {
        let (app, state) = federation_router();
        let media_id =
            upload_fixture(&state, "@alice:example.org", "image/png", Some("cat.png")).await;
        let (status, content_type, body) = get(app, &format!("/download/{media_id}")).await;
        assert_eq!(status, StatusCode::OK);
        let boundary = multipart::boundary_from_content_type(&content_type).unwrap();
        let parsed = multipart::parse(&boundary, &body).unwrap();
        assert_eq!(parsed.parts[0].body, b"{}");
        assert_eq!(parsed.parts[1].header("Content-Type"), Some("image/png"));
        assert!(
            parsed.parts[1]
                .header("Content-Disposition")
                .unwrap()
                .contains("cat.png")
        );
        assert_eq!(parsed.parts[1].body, &crate::test_fixtures::valid_png()[..]);
    }

    #[tokio::test]
    async fn a_thumbnail_is_generated_and_sent_as_the_second_part() {
        let (app, state) = federation_router();
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;
        let (status, content_type, body) = get(
            app,
            &format!("/thumbnail/{media_id}?width=32&height=32&method=crop"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let boundary = multipart::boundary_from_content_type(&content_type).unwrap();
        let parsed = multipart::parse(&boundary, &body).unwrap();
        let thumb = &parsed.parts[1];
        assert!(thumb.header("Content-Type").unwrap().starts_with("image/"));
        assert!(!thumb.body.is_empty());
    }

    #[tokio::test]
    async fn quarantined_and_unknown_media_are_not_found() {
        let (app, state) = federation_router();
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;
        state
            .repository
            .quarantine("example.org", &media_id, Some("@admin:example.org"))
            .unwrap();
        let (status, _, _) = get(app.clone(), &format!("/download/{media_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = get(app, "/download/nosuchmedia").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
