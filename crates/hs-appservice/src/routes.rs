//! The one inbound HTTP route this crate serves directly: `POST
//! /_matrix/client/v1/appservice/{appserviceId}/ping` (the client-facing half of ping; the
//! outbound half is [`crate::ping::PingService`]).
//!
//! # Why this route's state is `hs_auth::state::AuthState`, not this crate's own state
//!
//! `hs-auth`'s [`Requester`] extractor is implemented as `impl FromRequestParts<AuthState> for
//! Requester` (`crates/hs-auth/src/middleware.rs`) — concrete over `AuthState`, not generic over
//! any state that can produce one via `FromRef`. That is `hs-auth`'s design, not this crate's to
//! change (ownership rules forbid editing another track's crate). The only way to use `Requester`
//! as an extractor at all is therefore to run this route on a router whose `State` is exactly
//! `AuthState`.
//!
//! [`ping_router`] does exactly that, and takes the [`crate::ping::PingService`] as an
//! [`Extension`] layer instead of `State` — the same pattern `hs-testkit`'s `FakeAppservice`
//! already uses for its log (`crates/hs-testkit/src/fake_appservice.rs`). Whoever assembles the
//! real server's router (track 12/15's job, not this crate's — see
//! `docs/status/11-appservices-and-bridges.md`) merges this fragment into a router whose own
//! `State` is `AuthState`, the same way every other crate's `hs-auth`-authenticated routes must.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::routing::post;
use axum::{Json, Router};
use hs_auth::requester::Requester;
use hs_auth::state::AuthState;
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_kv::KvBackend;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppserviceError;
use crate::ping::PingService;

#[derive(Debug, Deserialize, Default)]
struct PingRequestBody {
    #[serde(default)]
    transaction_id: Option<String>,
}

fn appservice_error_to_matrix(e: AppserviceError) -> MatrixError {
    match e {
        AppserviceError::NotFound(id) => MatrixError::custom(
            axum::http::StatusCode::FORBIDDEN,
            MatrixErrorCode::Forbidden,
            format!("no such appservice: {id}"),
        ),
        other => MatrixError::custom(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            MatrixErrorCode::Unknown,
            other.to_string(),
        ),
    }
}

#[allow(
    clippy::result_large_err,
    reason = "MatrixError is the workspace's standard Matrix-shaped error response type; boxing it would only churn every call site"
)]
async fn ping_handler<B: KvBackend>(
    State(_auth): State<AuthState>,
    Extension(service): Extension<Arc<PingService<B>>>,
    requester: Requester,
    Path(appservice_id): Path<String>,
    body: Option<Json<PingRequestBody>>,
) -> Result<Json<Value>, MatrixError> {
    // Spec: "This API cannot be invoked by users who are not identified as application
    // services. Additionally, the appservice ID in the path must be the same as the appservice
    // whose as_token is being used" — both violations are 403 M_FORBIDDEN.
    let Some(identity) = &requester.appservice else {
        return Err(MatrixError::forbidden(
            "this endpoint may only be called with an application service access token",
        ));
    };
    if identity.appservice_id != appservice_id {
        return Err(MatrixError::forbidden(
            "the appservice ID in the path must match the as_token's own appservice",
        ));
    }

    let transaction_id = body.and_then(|Json(b)| b.transaction_id);
    let outcome = service
        .ping(&appservice_id, transaction_id.as_deref())
        .await
        .map_err(appservice_error_to_matrix)?;

    match outcome {
        Ok(success) => Ok(Json(json!({ "duration_ms": success.duration_ms }))),
        Err(failure) => {
            let status = axum::http::StatusCode::from_u16(failure.http_status())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let mut err = MatrixError::custom(
                status,
                MatrixErrorCode::Other(failure.errcode().to_string()),
                failure.to_string(),
            );
            if let crate::ping::PingFailure::BadStatus { status, body } = &failure {
                err.extra
                    .insert("status".to_string(), Value::Number((*status).into()));
                err.extra
                    .insert("body".to_string(), Value::String(body.clone()));
            }
            Err(err)
        }
    }
}

/// The inbound ping route, mounted at `/_matrix/client/v1/appservice/{appserviceId}/ping`. See
/// the module docs for why its state must be composed into an `AuthState` router.
pub fn ping_router<B: KvBackend>(service: Arc<PingService<B>>) -> Router<AuthState> {
    Router::new()
        .route("/appservice/{appserviceId}/ping", post(ping_handler::<B>))
        .layer(Extension(service))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::Namespaces;
    use crate::registration::Registration;
    use crate::registry::Registry;
    use axum::body::Body;
    use axum::http::Request;
    use hs_auth::appservice::{AppserviceRecord, InMemoryAppserviceRegistry};
    use hs_kv::memory::MemoryBackend;
    use ruma::{server_name, user_id};
    use tower::ServiceExt;

    async fn mock_bridge_server(status: u16) -> String {
        use axum::routing::post as axpost;
        let app = Router::new().route(
            "/_matrix/app/v1/ping",
            axpost(move || async move { axum::http::StatusCode::from_u16(status).unwrap() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn build_app(bridge_url: &str) -> (Router, hs_auth::state::AuthState) {
        let registry =
            Arc::new(Registry::open(MemoryBackend::new(), server_name!("example.org")).unwrap());
        registry
            .add(&Registration {
                id: "irc".to_string(),
                url: Some(bridge_url.to_string()),
                as_token: "as_irc".to_string(),
                hs_token: "hs_irc".to_string(),
                sender_localpart: "ircbot".to_string(),
                rate_limited: true,
                namespaces: Namespaces::default(),
                protocols: vec![],
                receive_ephemeral: false,
                push_ephemeral_legacy: false,
                msc3202: false,
                msc4190: false,
                extra: Default::default(),
            })
            .unwrap();
        let service = Arc::new(PingService::new(
            registry,
            Arc::new(crate::ping::HttpPingTransport::new()),
        ));

        let as_registry = InMemoryAppserviceRegistry::new();
        as_registry.insert(
            "as_irc",
            AppserviceRecord {
                appservice_id: "irc".to_string(),
                sender: user_id!("@ircbot:example.org").to_owned(),
                user_namespaces: vec![],
            },
        );
        let auth_state = hs_auth::state::AuthState {
            appservices: Arc::new(as_registry),
            ..hs_auth::state::AuthState::in_memory()
        };

        let router = ping_router::<MemoryBackend>(service).with_state(auth_state.clone());
        (router, auth_state)
    }

    #[tokio::test]
    async fn successful_ping_returns_duration_ms() {
        let bridge_url = mock_bridge_server(200).await;
        let (app, _) = build_app(&bridge_url).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/appservice/irc/ping")
                    .header("authorization", "Bearer as_irc")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert!(json["duration_ms"].is_number());
    }

    #[tokio::test]
    async fn mismatched_appservice_id_in_path_is_forbidden() {
        let bridge_url = mock_bridge_server(200).await;
        let (app, _) = build_app(&bridge_url).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/appservice/someone_else/ping")
                    .header("authorization", "Bearer as_irc")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }

    #[tokio::test]
    async fn non_appservice_token_is_forbidden() {
        let bridge_url = mock_bridge_server(200).await;
        let (app, _auth_state) = build_app(&bridge_url).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/appservice/irc/ping")
                    .header("authorization", "Bearer not_an_appservice_token")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // An unrecognized token is 401 M_UNKNOWN_TOKEN (ordinary token auth failure), which is
        // also a correct rejection of this endpoint — either way, it must not succeed as a ping.
        assert!(response.status() == 401 || response.status() == 403);
    }
}
