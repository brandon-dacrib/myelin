//! `PUT /_matrix/federation/v1/send/{txnId}`: inbound transaction dispatch, wired against
//! [`crate::inbound::process_transaction`]. See that module's doc for exactly what is and is not
//! real here.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_http::router::Builder;

use crate::inbound::{TransactionError, process_transaction};
use crate::transport::FederationState;
use crate::xmatrix::{self, XMatrixContext};

pub(super) fn add_routes(builder: Builder<FederationState>) -> Builder<FederationState> {
    builder.add(
        axum::http::Method::PUT,
        "/send/{txnId}",
        send,
        super::matrix_federation("federationSend"),
    )
}

async fn send(
    State(state): State<FederationState>,
    Extension(ctx): Extension<std::sync::Arc<XMatrixContext>>,
    Path(txn_id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let origin = match xmatrix::parse_x_matrix_header(&headers) {
        Ok(auth) => auth.origin,
        Err(_) => {
            return MatrixError::forbidden("could not determine requesting server").into_response();
        }
    };

    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return MatrixError::bad_json("invalid transaction body").into_response(),
    };

    match process_transaction(
        &origin,
        &txn_id,
        &parsed,
        state.rooms.as_ref(),
        state.write_sink.as_ref(),
        &ctx.key_cache,
        state.transactions.as_ref(),
        state.ancestor_fetcher.as_deref(),
        &state.backfill_limits,
    )
    .await
    {
        Ok(response) => axum::Json(response).into_response(),
        Err(TransactionError::TooManyPdus | TransactionError::TooManyEdus) => MatrixError::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::TooLarge,
            "transaction exceeds the resource limits for pdus/edus",
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::{InMemoryTransactionStore, StaticWriteSink};
    use crate::keys::{DynRemoteKeyCache, KeyServerFetcher, RemoteKeyCache};
    use crate::room_source::{FakeRoom, InMemoryRoomSource};
    use crate::transport::InMemoryQuerySource;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct EmptyFetcher;
    #[async_trait]
    impl KeyServerFetcher for EmptyFetcher {
        async fn fetch_server_key(&self, _server_name: &str) -> Option<serde_json::Value> {
            None
        }
    }

    fn build() -> axum::Router<FederationState> {
        add_routes(Builder::<FederationState>::new())
            .build()
            .0
            .layer(axum::Extension(ctx()))
    }

    fn ctx() -> std::sync::Arc<XMatrixContext> {
        let key_cache: std::sync::Arc<DynRemoteKeyCache> = std::sync::Arc::new(
            RemoteKeyCache::new(Box::new(EmptyFetcher) as Box<dyn KeyServerFetcher>),
        );
        std::sync::Arc::new(XMatrixContext {
            own_server_name: "us.example.org".to_string(),
            key_cache,
        })
    }

    fn state() -> FederationState {
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            "!r:example.org",
            FakeRoom {
                room_version: Some("11".to_owned()),
                ..FakeRoom::default()
            },
        );
        FederationState {
            own_server_name: std::sync::Arc::from("us.example.org"),
            rooms: std::sync::Arc::new(rooms),
            queries: std::sync::Arc::new(InMemoryQuerySource::default()),
            allow_public_rooms_over_federation: false,
            allow_device_name_lookup_over_federation: false,
            write_sink: std::sync::Arc::new(StaticWriteSink::new(Vec::new(), "not supported yet")),
            transactions: std::sync::Arc::new(InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
            sender: None,
        }
    }

    fn signed_header(origin: &str) -> String {
        format!(
            "X-Matrix origin=\"{origin}\",destination=\"us.example.org\",key=\"ed25519:1\",sig=\"AAAA\",method=\"PUT\",uri=\"/send/1\""
        )
    }

    #[tokio::test]
    async fn empty_transaction_is_accepted() {
        let app = build().with_state(state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/send/1")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        signed_header("origin.example.org"),
                    )
                    .body(Body::from(r#"{"pdus": [], "edus": []}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn too_many_pdus_is_rejected() {
        let app = build().with_state(state());
        let too_many: Vec<serde_json::Value> = (0..51).map(|_| serde_json::json!({})).collect();
        let body =
            serde_json::to_string(&serde_json::json!({"pdus": too_many, "edus": []})).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/send/1")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        signed_header("origin.example.org"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn no_x_matrix_header_is_forbidden() {
        let app = build().with_state(state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/send/1")
                    .body(Body::from(r#"{"pdus": [], "edus": []}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
