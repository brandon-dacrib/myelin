//! Upload handlers: the synchronous `POST .../upload`, and the async-upload pair
//! `POST .../create` + `PUT .../upload/{server}/{mediaId}` (spec since v1.7, formerly MSC2246).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use hs_kv::KvBackend;
use serde::{Deserialize, Serialize};

use crate::error::MediaError;
use crate::policy::UploadContext;
use crate::state::{MediaRequester, MediaState};

use super::{mxc_uri, parse_media_id};

#[derive(Debug, Deserialize, Default)]
pub(crate) struct UploadQuery {
    filename: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct UploadResponse {
    content_uri: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateResponse {
    content_uri: String,
    unused_expires_at: u64,
}

fn content_type_of(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string()
}

async fn collect_body(body: axum::body::Body, limit: u64) -> Result<bytes::Bytes, MediaError> {
    // `to_bytes` rejects once the body exceeds `limit + 1` bytes without buffering unboundedly
    // past that point; the exact ceiling used here (`max_upload_size`) is what turns that
    // rejection into the spec-correct `M_TOO_LARGE` rather than an opaque body-read error.
    let cap = usize::try_from(limit.saturating_add(1)).unwrap_or(usize::MAX);
    axum::body::to_bytes(body, cap)
        .await
        .map_err(|_| MediaError::TooLarge { limit })
}

/// `POST .../upload`: synchronous upload. Accepts `?filename=`, and the request's `Content-Type`
/// (defaulting to `application/octet-stream` if absent, matching Synapse).
pub(crate) async fn upload_sync<B: KvBackend>(
    State(state): State<MediaState<B>>,
    MediaRequester(requester): MediaRequester,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<(StatusCode, Json<UploadResponse>), MediaError> {
    let content_type = content_type_of(&headers);
    let limit = state.repository.max_upload_size();
    let bytes = collect_body(body, limit).await?;
    let ctx = UploadContext {
        user_id: requester.user_id.to_string(),
        server_name: state.repository.server_name().to_string(),
    };
    let media_id = state
        .repository
        .upload(&ctx, &content_type, query.filename, bytes)
        .await?;
    let content_uri = mxc_uri(state.repository.server_name(), media_id.as_str());
    Ok((StatusCode::OK, Json(UploadResponse { content_uri })))
}

/// `POST .../create`: async upload, step 1. Reserves a media ID with no content yet.
pub(crate) async fn create<B: KvBackend>(
    State(state): State<MediaState<B>>,
    MediaRequester(requester): MediaRequester,
) -> Result<Json<CreateResponse>, MediaError> {
    let ctx = UploadContext {
        user_id: requester.user_id.to_string(),
        server_name: state.repository.server_name().to_string(),
    };
    let (media_id, unused_expires_at) = state.repository.create_reservation(&ctx)?;
    let content_uri = mxc_uri(state.repository.server_name(), media_id.as_str());
    Ok(Json(CreateResponse {
        content_uri,
        unused_expires_at,
    }))
}

/// `PUT .../upload/{server}/{mediaId}`: async upload, step 2. Only the user that reserved the ID
/// may fill it, and only for media on this server — a `mediaId` reserved elsewhere is never
/// something this server can complete on the reserving server's behalf.
pub(crate) async fn put_upload<B: KvBackend>(
    State(state): State<MediaState<B>>,
    MediaRequester(requester): MediaRequester,
    axum::extract::Path((server_name, media_id_raw)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<StatusCode, MediaError> {
    if server_name != state.repository.server_name() {
        return Err(MediaError::InvalidInput(
            "this server can only complete an upload reservation it made itself".into(),
        ));
    }
    let media_id = parse_media_id(&media_id_raw)?;

    let existing = state
        .repository
        .metadata()
        .get_media(&server_name, media_id.as_str())?
        .ok_or(MediaError::NotFound)?;
    if existing.uploader.as_deref() != Some(requester.user_id.as_str()) {
        // Deliberately the same shape as "not found": do not reveal that a reservation exists
        // under this ID to a user who does not own it.
        return Err(MediaError::NotFound);
    }

    let content_type = content_type_of(&headers);
    let limit = state.repository.max_upload_size();
    let bytes = collect_body(body, limit).await?;
    let ctx = UploadContext {
        user_id: requester.user_id.to_string(),
        server_name: state.repository.server_name().to_string(),
    };
    state
        .repository
        .complete_reservation(&ctx, &media_id, &content_type, bytes)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{router, seed_token};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn synchronous_upload_returns_a_content_uri() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;

        let png = crate::test_fixtures::valid_png();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload?filename=cat.png")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(png))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["content_uri"]
                .as_str()
                .unwrap()
                .starts_with("mxc://example.org/")
        );
    }

    #[tokio::test]
    async fn upload_without_a_token_is_rejected() {
        let (app, _state) = router();
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
    async fn async_upload_create_then_put_round_trips() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;

        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/create")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(create_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let content_uri = json["content_uri"].as_str().unwrap().to_string();
        assert!(json["unused_expires_at"].as_u64().unwrap() > 0);
        let media_id = content_uri.rsplit('/').next().unwrap();

        let put_response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/upload/example.org/{media_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(crate::test_fixtures::valid_png()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn put_upload_by_a_different_user_is_rejected() {
        let (app, state) = router();
        let alice = seed_token(&state, "@alice:example.org").await;
        let bob = seed_token(&state, "@bob:example.org").await;

        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/create")
                    .header(header::AUTHORIZATION, format!("Bearer {alice}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(create_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let content_uri = json["content_uri"].as_str().unwrap();
        let media_id = content_uri.rsplit('/').next().unwrap();

        let put_response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/upload/example.org/{media_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {bob}"))
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(crate::test_fixtures::valid_png()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oversized_upload_is_rejected_with_413() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        // The test router's repository is built with a tiny max_upload_size -- see test_support.
        let huge = vec![0u8; 10_000];
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(huge))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
