//! Permissive JSON body parsing: Matrix clients are inconsistent about `Content-Type` on request
//! bodies (some omit it, some send `text/plain`), so `/_matrix/*` parses the body as JSON
//! regardless of the declared content type. `/api/v1` is strict about this instead (RFC 0004
//! section 3.1: a body with another `Content-Type` is `415`); handlers there should check
//! `Content-Type` themselves before using this extractor, or use [`StrictJson`].

use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use crate::error::MatrixError;
use crate::problem::Problem;

/// A rejection from [`PermissiveJson`] or [`StrictJson`]: enough information for either the
/// Matrix or the problem-details error shape to be built from it.
///
/// Its own [`IntoResponse`] impl defaults to the Matrix error shape, since permissive parsing is
/// primarily a Matrix-route concern; admin-API handlers that use [`StrictJson`] should convert
/// with [`BodyParseError::to_problem`] before returning, rather than relying on this default.
#[derive(Debug, Clone)]
pub struct BodyParseError {
    pub status: StatusCode,
    pub message: String,
}

impl IntoResponse for BodyParseError {
    fn into_response(self) -> Response {
        self.to_matrix_error().into_response()
    }
}

impl BodyParseError {
    pub fn to_matrix_error(&self) -> MatrixError {
        if self.status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            // Matrix has no unsupported-media-type errcode; permissive parsing means this path
            // is only reachable for StrictJson misuse on a Matrix route, which should not happen.
            MatrixError::bad_json(self.message.clone())
        } else {
            MatrixError::not_json(self.message.clone())
        }
    }

    pub fn to_problem(&self) -> Problem {
        if self.status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            Problem::unsupported_media_type().with_detail(self.message.clone())
        } else {
            Problem::validation_failed().with_detail(self.message.clone())
        }
    }
}

/// Parses the request body as JSON no matter what `Content-Type` (or lack of one) the client
/// sent. Used for `/_matrix/*` and `/_synapse/*`.
#[derive(Debug)]
pub struct PermissiveJson<T>(pub T);

impl<S, T> FromRequest<S> for PermissiveJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = BodyParseError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
            .await
            .map_err(|e| BodyParseError {
                status: StatusCode::BAD_REQUEST,
                message: format!("could not read request body: {e}"),
            })?;
        let _ = state;
        if bytes.is_empty() {
            // An empty body is treated as `{}`, matching Synapse's leniency for bodyless POSTs.
            return serde_json::from_slice(b"{}")
                .map(PermissiveJson)
                .map_err(|e| BodyParseError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("invalid JSON: {e}"),
                });
        }
        serde_json::from_slice(&bytes)
            .map(PermissiveJson)
            .map_err(|e| BodyParseError {
                status: StatusCode::BAD_REQUEST,
                message: format!("invalid JSON: {e}"),
            })
    }
}

/// Parses the request body as JSON, and only as JSON: any `Content-Type` other than
/// `application/json` (or a missing one) is `415`. Used for `/api/v1` (RFC 0004 section 3.1).
#[derive(Debug)]
pub struct StrictJson<T>(pub T);

impl<S, T> FromRequest<S> for StrictJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = BodyParseError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let is_json = content_type
            .split(';')
            .next()
            .map(|s| s.trim())
            .map(|s| s.eq_ignore_ascii_case("application/json"))
            .unwrap_or(false);
        if !is_json {
            return Err(BodyParseError {
                status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                message: format!("expected application/json, got {content_type:?}"),
            });
        }
        let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
            .await
            .map_err(|e| BodyParseError {
                status: StatusCode::BAD_REQUEST,
                message: format!("could not read request body: {e}"),
            })?;
        let _ = state;
        serde_json::from_slice(&bytes)
            .map(StrictJson)
            .map_err(|e| BodyParseError {
                status: StatusCode::BAD_REQUEST,
                message: format!("invalid JSON: {e}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Ping {
        ok: bool,
    }

    #[tokio::test]
    async fn permissive_json_ignores_content_type() {
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "text/plain")
            .body(Body::from(r#"{"ok":true}"#))
            .unwrap();
        let PermissiveJson(ping) = PermissiveJson::<Ping>::from_request(req, &())
            .await
            .unwrap();
        assert_eq!(ping, Ping { ok: true });
    }

    #[tokio::test]
    async fn permissive_json_treats_empty_body_as_object() {
        #[derive(Debug, Deserialize, PartialEq, Default)]
        struct Empty {}
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let PermissiveJson(e) = PermissiveJson::<Empty>::from_request(req, &())
            .await
            .unwrap();
        assert_eq!(e, Empty::default());
    }

    #[tokio::test]
    async fn strict_json_rejects_non_json_content_type() {
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "text/plain")
            .body(Body::from(r#"{"ok":true}"#))
            .unwrap();
        let err = StrictJson::<Ping>::from_request(req, &())
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn strict_json_accepts_application_json() {
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json; charset=utf-8")
            .body(Body::from(r#"{"ok":false}"#))
            .unwrap();
        let StrictJson(ping) = StrictJson::<Ping>::from_request(req, &()).await.unwrap();
        assert_eq!(ping, Ping { ok: false });
    }
}
