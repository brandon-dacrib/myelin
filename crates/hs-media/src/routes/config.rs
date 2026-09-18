//! `GET .../config`: advertises the `m.upload.size` capability (spec: "Content repository").

use axum::Json;
use axum::extract::State;
use hs_kv::KvBackend;
use serde::Serialize;

use crate::state::{MediaRequester, MediaState};

#[derive(Debug, Serialize)]
pub(crate) struct ConfigResponse {
    #[serde(rename = "m.upload.size")]
    upload_size: u64,
}

/// `GET .../config`.
pub(crate) async fn config<B: KvBackend>(
    State(state): State<MediaState<B>>,
    _requester: MediaRequester,
) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        upload_size: state.repository.max_upload_size(),
    })
}

#[cfg(test)]
mod tests {
    use crate::test_support::{router, seed_token};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    #[tokio::test]
    async fn config_reports_max_upload_size() {
        let (app, state) = router();
        let token = seed_token(&state, "@alice:example.org").await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/config")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["m.upload.size"].as_u64().unwrap() > 0);
    }
}
