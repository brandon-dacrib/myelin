//! `POST /user/keys/query` and `POST /user/keys/claim`: another server asking for this server's
//! users' device keys (so its users can encrypt to them) and claiming their one-time keys (so
//! its users can open an Olm session with them). The answers come from
//! [`crate::transport::FederationQuerySource::keys_query`] and `keys_claim`, which in `hs serve`
//! read `hs-e2e`'s store; this module only checks the request's shape and who is asking.

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_http::router::Builder;
use serde_json::Value;

use crate::transport::FederationState;
use crate::xmatrix;

pub(super) fn add_routes(builder: Builder<FederationState>) -> Builder<FederationState> {
    builder
        .add(
            Method::POST,
            "/user/keys/query",
            keys_query,
            super::matrix_federation("federationUserKeysQuery"),
        )
        .add(
            Method::POST,
            "/user/keys/claim",
            keys_claim,
            super::matrix_federation("federationUserKeysClaim"),
        )
}

/// The requesting server and the named object field of the body, or the error response.
fn origin_and_field(
    headers: &HeaderMap,
    body: &[u8],
    field: &str,
) -> Result<(String, Value), Box<MatrixError>> {
    let origin = xmatrix::parse_x_matrix_header(headers)
        .map_err(|_| {
            Box::new(MatrixError::forbidden(
                "could not determine requesting server",
            ))
        })?
        .origin;
    let parsed: Value = serde_json::from_slice(body)
        .map_err(|_| Box::new(MatrixError::bad_json("the body is not JSON")))?;
    let value = parsed
        .get(field)
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| {
            Box::new(MatrixError::custom(
                StatusCode::BAD_REQUEST,
                MatrixErrorCode::BadJson,
                format!("`{field}` must be an object"),
            ))
        })?;
    Ok((origin, value))
}

fn unrecognized() -> Response {
    MatrixError::custom(
        StatusCode::NOT_FOUND,
        MatrixErrorCode::Unrecognized,
        "this server does not answer key queries",
    )
    .into_response()
}

async fn keys_query(
    State(state): State<FederationState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (origin, device_keys) = match origin_and_field(&headers, &body, "device_keys") {
        Ok(parts) => parts,
        Err(error) => return error.into_response(),
    };
    match state.queries.keys_query(&origin, &device_keys).await {
        Some(answer) => axum::Json(answer).into_response(),
        None => unrecognized(),
    }
}

async fn keys_claim(
    State(state): State<FederationState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (origin, one_time_keys) = match origin_and_field(&headers, &body, "one_time_keys") {
        Ok(parts) => parts,
        Err(error) => return error.into_response(),
    };
    match state.queries.keys_claim(&origin, &one_time_keys).await {
        Some(answer) => axum::Json(answer).into_response(),
        None => unrecognized(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_source::InMemoryRoomSource;
    use crate::transport::FederationQuerySource;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// Answers every query with what it was asked and by whom.
    #[derive(Default)]
    struct Echo(Mutex<Vec<String>>);

    #[async_trait]
    impl FederationQuerySource for Echo {
        async fn profile(&self, _: &str, _: Option<&str>) -> Option<Value> {
            None
        }
        async fn resolve_alias(&self, _: &str) -> Option<(String, Vec<String>)> {
            None
        }
        async fn devices(&self, _: &str) -> Option<Value> {
            None
        }
        async fn openid_userinfo(&self, _: &str) -> Option<String> {
            None
        }
        async fn keys_query(&self, origin: &str, device_keys: &Value) -> Option<Value> {
            self.0.lock().unwrap().push(origin.to_owned());
            Some(serde_json::json!({"device_keys": device_keys}))
        }
    }

    fn state(queries: Arc<dyn FederationQuerySource>) -> FederationState {
        FederationState {
            own_server_name: Arc::from("us.example.org"),
            rooms: Arc::new(InMemoryRoomSource::new()),
            queries,
            allow_public_rooms_over_federation: false,
            allow_device_name_lookup_over_federation: false,
            write_sink: Arc::new(crate::inbound::StaticWriteSink::new(Vec::new(), "n/a")),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
            invites: None,
            edu_sink: None,
        }
    }

    async fn post(queries: Arc<dyn FederationQuerySource>, path: &str, body: &str) -> Response {
        let (router, _) = add_routes(Builder::<FederationState>::new()).build();
        router
            .with_state(state(queries))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(
                        axum::http::header::AUTHORIZATION,
                        "X-Matrix origin=\"them.example.org\",destination=\"us.example.org\",key=\"ed25519:1\",sig=\"AAAA\"",
                    )
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_key_query_is_answered_by_the_source_with_the_asking_server_named() {
        let echo = Arc::new(Echo::default());
        let response = post(
            echo.clone(),
            "/user/keys/query",
            r#"{"device_keys": {"@alice:us.example.org": []}}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"device_keys": {"@alice:us.example.org": []}})
        );
        assert_eq!(*echo.0.lock().unwrap(), vec!["them.example.org".to_owned()]);
    }

    #[tokio::test]
    async fn a_malformed_body_is_a_bad_request_and_a_source_that_cannot_answer_is_unrecognized() {
        let echo = Arc::new(Echo::default());
        let response = post(echo.clone(), "/user/keys/query", r#"{"device_keys": []}"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = post(
            echo,
            "/user/keys/claim",
            r#"{"one_time_keys": {"@alice:us.example.org": {"D": "signed_curve25519"}}}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
