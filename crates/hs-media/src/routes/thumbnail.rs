//! `GET .../thumbnail/{serverName}/{mediaId}?width=&height=&method=`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use serde::Deserialize;

use crate::error::MediaError;
use crate::metadata::ThumbnailRecord;
use crate::repository::MediaRepository;
use crate::security;
use crate::state::{MediaRequester, MediaState};
use crate::thumbnail;

#[derive(Debug, Deserialize)]
pub(crate) struct ThumbnailQuery {
    width: u32,
    height: u32,
    #[serde(default = "default_method")]
    method: String,
}

fn default_method() -> String {
    "scale".to_string()
}

pub(crate) async fn build_response<B: KvBackend>(
    repository: &MediaRepository<B>,
    record: &crate::metadata::MediaRecord,
    query: &ThumbnailQuery,
) -> Result<Response, MediaError> {
    let method = thumbnail::parse_method(&query.method)?;
    let (thumb, bytes): (ThumbnailRecord, bytes::Bytes) = repository
        .get_thumbnail(record, query.width, query.height, method)
        .await?;
    let headers = security::response_headers(&thumb.content_type, None);
    Ok((StatusCode::OK, headers, bytes).into_response())
}

/// `GET .../thumbnail/{serverName}/{mediaId}` (authenticated).
pub(crate) async fn thumbnail<B: KvBackend>(
    State(state): State<MediaState<B>>,
    _requester: MediaRequester,
    Path((server_name, media_id)): Path<(String, String)>,
    Query(query): Query<ThumbnailQuery>,
) -> Result<Response, MediaError> {
    let record = state.repository.get_record(&server_name, &media_id)?;
    build_response(&state.repository, &record, &query).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{router, seed_token, upload_fixture};
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    #[tokio::test]
    async fn default_size_thumbnail_is_generated() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/thumbnail/example.org/{media_id}?width=32&height=32&method=crop"
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
    }

    #[tokio::test]
    async fn unconfigured_dynamic_size_is_rejected() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/thumbnail/example.org/{media_id}?width=13&height=13&method=crop"
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn bad_method_query_param_is_rejected() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let media_id = upload_fixture(&state, "@alice:example.org", "image/png", None).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/thumbnail/example.org/{media_id}?width=32&height=32&method=stretch"
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
