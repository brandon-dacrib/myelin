//! Permissive JSON body parsing: Matrix clients are inconsistent about `Content-Type` on request
//! bodies (some omit it, some send `text/plain`), so `/_matrix/*` parses the body as JSON
//! regardless of the declared content type. `/api/v1` is strict about this instead (RFC 0004
//! section 3.1: a body with another `Content-Type` is `415`); handlers there should check
//! `Content-Type` themselves before using this extractor, or use [`StrictJson`].
//!
//! Every `/_matrix` route that takes a JSON body should take it through [`PermissiveJson`], not
//! a bare `axum::Json`. `axum::Json`'s rejections are plain text -- `415` for a missing
//! `Content-Type`, `400` for a syntax error, `422` for a shape error -- and the client-server
//! spec's "Standard error response" section wants every one of those to be a JSON object with an
//! `errcode`: `M_NOT_JSON` when the body "did not contain valid JSON", `M_BAD_JSON` when it
//! "contained valid JSON, but it was malformed in some way, e.g. missing required keys, invalid
//! values for keys". Complement's `TestRequestEncodingFails` checks the first by posting invalid
//! UTF-8 to `/register`.

use axum::RequestExt;
use axum::extract::{FromRequest, OptionalFromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::de::DeserializeOwned;

use crate::error::{MatrixError, MatrixErrorCode};
use crate::problem::Problem;

/// What went wrong reading a body, independent of which error shape reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyParseKind {
    /// The bytes are not JSON at all: a syntax error, a truncated document, invalid UTF-8.
    NotJson,
    /// The bytes are JSON, but not the JSON this route takes: a missing key, a wrong type.
    BadJson,
    /// The body is larger than the router's `DefaultBodyLimit` allows.
    TooLarge,
    /// [`StrictJson`] only: the `Content-Type` is not `application/json`.
    UnsupportedMediaType,
    /// The connection failed before the whole body arrived.
    Unreadable,
}

/// A rejection from [`PermissiveJson`] or [`StrictJson`]: enough information for either the
/// Matrix or the problem-details error shape to be built from it.
///
/// Its own [`IntoResponse`] impl defaults to the Matrix error shape, since permissive parsing is
/// primarily a Matrix-route concern; admin-API handlers that use [`StrictJson`] should convert
/// with [`BodyParseError::to_problem`] before returning, rather than relying on this default.
#[derive(Debug, Clone)]
pub struct BodyParseError {
    pub kind: BodyParseKind,
    pub status: StatusCode,
    pub message: String,
}

impl IntoResponse for BodyParseError {
    fn into_response(self) -> Response {
        self.to_matrix_error().into_response()
    }
}

impl BodyParseError {
    fn new(kind: BodyParseKind, message: impl Into<String>) -> Self {
        let status = match kind {
            BodyParseKind::NotJson | BodyParseKind::BadJson | BodyParseKind::Unreadable => {
                StatusCode::BAD_REQUEST
            }
            BodyParseKind::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            BodyParseKind::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        };
        Self {
            kind,
            status,
            message: message.into(),
        }
    }

    pub fn to_matrix_error(&self) -> MatrixError {
        match self.kind {
            BodyParseKind::NotJson | BodyParseKind::Unreadable => {
                MatrixError::not_json(self.message.clone())
            }
            // Matrix has no unsupported-media-type errcode; permissive parsing means that arm is
            // only reachable for StrictJson misuse on a Matrix route, which should not happen.
            BodyParseKind::BadJson | BodyParseKind::UnsupportedMediaType => {
                MatrixError::bad_json(self.message.clone())
            }
            BodyParseKind::TooLarge => MatrixError::custom(
                StatusCode::PAYLOAD_TOO_LARGE,
                MatrixErrorCode::TooLarge,
                self.message.clone(),
            ),
        }
    }

    pub fn to_problem(&self) -> Problem {
        match self.kind {
            BodyParseKind::UnsupportedMediaType => {
                Problem::unsupported_media_type().with_detail(self.message.clone())
            }
            BodyParseKind::TooLarge => {
                Problem::payload_too_large().with_detail(self.message.clone())
            }
            BodyParseKind::NotJson | BodyParseKind::BadJson | BodyParseKind::Unreadable => {
                Problem::validation_failed().with_detail(self.message.clone())
            }
        }
    }
}

/// Reads the whole body, under whatever `DefaultBodyLimit` the router carries (axum's own default
/// of 2 MiB when none is set) -- the same limit `axum::Json` applies, so switching a route from
/// one to the other does not quietly make it accept an unbounded body.
async fn read_limited(req: Request) -> Result<Bytes, BodyParseError> {
    axum::body::to_bytes(req.into_limited_body(), usize::MAX)
        .await
        .map_err(|e| {
            if is_length_limit(&e) {
                BodyParseError::new(BodyParseKind::TooLarge, "request body is too large")
            } else {
                BodyParseError::new(
                    BodyParseKind::Unreadable,
                    format!("could not read request body: {e}"),
                )
            }
        })
}

fn is_length_limit(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(e) = current {
        if e.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        current = e.source();
    }
    false
}

/// `serde_json` already draws the spec's line: `Data` is "valid JSON, wrong shape", and every
/// other category means the bytes never were a JSON document.
///
/// UTF-8 is checked over the whole body first, because `serde_json` does not: it validates the
/// strings it *deserializes* and skips the ones it ignores, so `{"test":"a\x81"}` read into a
/// struct with no `test` field is either accepted outright or reported as a missing field,
/// depending on the struct. RFC 8259 section 8.1 requires UTF-8 of the whole text, not of the
/// parts a given route happens to read. Having checked, `from_str` does not check again.
fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, BodyParseError> {
    let text = std::str::from_utf8(bytes).map_err(|e| {
        BodyParseError::new(
            BodyParseKind::NotJson,
            format!("invalid JSON: request body is not valid UTF-8: {e}"),
        )
    })?;
    serde_json::from_str(text).map_err(|e| {
        let kind = match e.classify() {
            serde_json::error::Category::Data => BodyParseKind::BadJson,
            serde_json::error::Category::Syntax
            | serde_json::error::Category::Eof
            | serde_json::error::Category::Io => BodyParseKind::NotJson,
        };
        BodyParseError::new(kind, format!("invalid JSON: {e}"))
    })
}

/// Parses the request body as JSON no matter what `Content-Type` (or lack of one) the client
/// sent. Used for `/_matrix/*` and `/_synapse/*`.
///
/// An empty body is read as `{}`. `Option<PermissiveJson<T>>` is `None` for an empty body
/// instead, for the routes that need to tell "sent nothing" from "sent an empty object".
#[derive(Debug)]
pub struct PermissiveJson<T>(pub T);

impl<S, T> FromRequest<S> for PermissiveJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = BodyParseError;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = read_limited(req).await?;
        if bytes.is_empty() {
            // An empty body is treated as `{}`, matching Synapse's leniency for bodyless POSTs.
            return parse(b"{}").map(PermissiveJson);
        }
        parse(&bytes).map(PermissiveJson)
    }
}

impl<S, T> OptionalFromRequest<S> for PermissiveJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = BodyParseError;

    async fn from_request(req: Request, _state: &S) -> Result<Option<Self>, Self::Rejection> {
        let bytes = read_limited(req).await?;
        if bytes.is_empty() {
            return Ok(None);
        }
        parse(&bytes).map(PermissiveJson).map(Some)
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

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
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
            return Err(BodyParseError::new(
                BodyParseKind::UnsupportedMediaType,
                format!("expected application/json, got {content_type:?}"),
            ));
        }
        let bytes = read_limited(req).await?;
        parse(&bytes).map(StrictJson)
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

    // Both extractor traits name their method `from_request`, so with both in scope a bare
    // `PermissiveJson::<T>::from_request` is ambiguous. Handlers never hit this: axum picks the
    // trait from the parameter's type.
    async fn required<T: DeserializeOwned>(
        req: HttpRequest<Body>,
    ) -> Result<PermissiveJson<T>, BodyParseError> {
        <PermissiveJson<T> as FromRequest<()>>::from_request(req, &()).await
    }

    async fn optional<T: DeserializeOwned>(
        req: HttpRequest<Body>,
    ) -> Result<Option<PermissiveJson<T>>, BodyParseError> {
        <PermissiveJson<T> as OptionalFromRequest<()>>::from_request(req, &()).await
    }

    #[tokio::test]
    async fn permissive_json_ignores_content_type() {
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "text/plain")
            .body(Body::from(r#"{"ok":true}"#))
            .unwrap();
        let PermissiveJson(ping) = required::<Ping>(req).await.unwrap();
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
        let PermissiveJson(e) = required::<Empty>(req).await.unwrap();
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

    /// The exact bytes Complement's `TestRequestEncodingFails` posts to `/register`: a JSON
    /// string holding a lone `0x81`, which is not valid UTF-8.
    const INVALID_UTF8: &[u8] = b"{ \"test\":\"a\x81\" }";

    async fn response_json(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn invalid_utf8_is_400_m_not_json() {
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(INVALID_UTF8))
            .unwrap();
        let err = required::<serde_json::Value>(req).await.unwrap_err();
        assert_eq!(err.kind, BodyParseKind::NotJson);
        let (status, body) = response_json(err.into_response()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["errcode"], "M_NOT_JSON");
    }

    /// `serde_json` skips an ignored string without validating it, so without the up-front UTF-8
    /// check this body is *accepted* by a struct that has every field it requires.
    #[tokio::test]
    async fn invalid_utf8_in_a_field_the_route_ignores_is_still_m_not_json() {
        let mut body = br#"{"ok":true,"ignored":"a"#.to_vec();
        body.push(0x81);
        body.extend_from_slice(br#""}"#);
        assert!(
            serde_json::from_slice::<Ping>(&body).is_ok(),
            "serde_json alone now rejects this; the up-front check may be redundant"
        );
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .body(Body::from(body))
            .unwrap();
        let err = required::<Ping>(req).await.unwrap_err();
        assert_eq!(err.kind, BodyParseKind::NotJson);
    }

    #[tokio::test]
    async fn valid_json_of_the_wrong_shape_is_m_bad_json_not_m_not_json() {
        for body in [r#"{"ok":"yes"}"#, r#"{}"#, r#"[]"#] {
            let req = HttpRequest::builder()
                .method("POST")
                .uri("/")
                .body(Body::from(body))
                .unwrap();
            let err = required::<Ping>(req).await.unwrap_err();
            assert_eq!(err.kind, BodyParseKind::BadJson, "{body}");
            let (status, json) = response_json(err.into_response()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(json["errcode"], "M_BAD_JSON", "{body}");
        }
    }

    #[tokio::test]
    async fn a_truncated_document_is_m_not_json() {
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .body(Body::from(r#"{"ok":tr"#))
            .unwrap();
        let err = required::<Ping>(req).await.unwrap_err();
        assert_eq!(err.kind, BodyParseKind::NotJson);
    }

    #[tokio::test]
    async fn optional_body_is_none_when_empty_and_still_rejects_garbage() {
        let empty = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let parsed = optional::<Ping>(empty).await.unwrap();
        assert!(parsed.is_none());

        let present = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .body(Body::from(r#"{"ok":true}"#))
            .unwrap();
        let parsed = optional::<Ping>(present).await.unwrap();
        assert_eq!(parsed.unwrap().0, Ping { ok: true });

        let garbage = HttpRequest::builder()
            .method("POST")
            .uri("/")
            .body(Body::from(INVALID_UTF8))
            .unwrap();
        let err = optional::<Ping>(garbage).await.unwrap_err();
        assert_eq!(err.kind, BodyParseKind::NotJson);
    }

    /// Through a real router, because the limit lives in a request extension the router sets:
    /// calling the extractor directly would test nothing. The second request proves the limit is
    /// what rejected the first, not the route.
    #[tokio::test]
    async fn the_routers_body_limit_applies_and_answers_413_m_too_large() {
        use axum::extract::DefaultBodyLimit;
        use axum::routing::post;
        use tower::ServiceExt;

        async fn echo(
            PermissiveJson(v): PermissiveJson<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            axum::Json(v)
        }
        let app = axum::Router::new()
            .route("/", post(echo))
            .layer(DefaultBodyLimit::max(64));

        let big = format!(r#"{{"pad":"{}"}}"#, "x".repeat(200));
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/")
                    .body(Body::from(big))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["errcode"], "M_TOO_LARGE");

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/")
                    .body(Body::from(r#"{"ok":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
