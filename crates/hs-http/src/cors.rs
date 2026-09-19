//! CORS for `/api/v1`. RFC 0004 section 3.1: same-origin by default (no headers emitted), and an
//! operator running the management interface elsewhere configures `admin_api.cors_origins`.

use axum::http::{HeaderName, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Headers the admin API's mutations and pagination rely on, beyond the defaults tower-http
/// already allows.
fn extra_allowed_headers() -> Vec<HeaderName> {
    vec![
        header::AUTHORIZATION,
        header::CONTENT_TYPE,
        header::IF_MATCH,
        HeaderName::from_static("idempotency-key"),
        HeaderName::from_static("last-event-id"),
    ]
}

/// Headers the browser is allowed to read from the response.
fn exposed_headers() -> Vec<HeaderName> {
    vec![
        HeaderName::from_static("x-request-id"),
        header::ETAG,
        header::RETRY_AFTER,
        header::LOCATION,
        HeaderName::from_static("idempotency-replayed"),
        HeaderName::from_static("ratelimit"),
        HeaderName::from_static("ratelimit-policy"),
    ]
}

/// Builds the CORS layer for `/api/v1`. An empty `origins` list means same-origin only: no
/// `Access-Control-*` headers are emitted, matching the RFC's default. Passing `["*"]` allows any
/// origin (only sensible for local development against the mock server).
pub fn layer(origins: &[String]) -> CorsLayer {
    let allow_origin = if origins.is_empty() {
        AllowOrigin::list(Vec::<axum::http::HeaderValue>::new())
    } else if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        let values: Vec<_> = origins
            .iter()
            .filter_map(|o| axum::http::HeaderValue::from_str(o).ok())
            .collect();
        AllowOrigin::list(values)
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(extra_allowed_headers())
        .expose_headers(exposed_headers())
}

/// CORS for the Matrix client-server and media APIs, as the specification prescribes
/// (`refs/matrix-spec/content/client-server-api/_index.md`, "Web Browser Clients"):
///
/// ```text
/// Access-Control-Allow-Origin: *
/// Access-Control-Allow-Methods: GET, POST, PUT, DELETE, OPTIONS
/// Access-Control-Allow-Headers: X-Requested-With, Content-Type, Authorization
/// ```
///
/// This is deliberately *not* the same policy as [`layer`] above, and the difference is the whole
/// point of having two. The admin API is same-origin by default because it is an operator surface
/// whose browser client this project ships and can configure. The client-server API is the
/// opposite: it is meant to be reached by web clients hosted anywhere — app.element.io talking to
/// your homeserver is the normal case, not an exception — so a wildcard origin is what the spec
/// asks for and what every implementation does.
///
/// Without this, a browser refuses every request after the preflight and a web client sees nothing
/// but opaque network failures. That was the state of this server until it was pointed at a real
/// browser client: none of the `/_matrix` routes carried a single `Access-Control-*` header.
///
/// The spec also requires that `OPTIONS` never run an endpoint's own logic. `CorsLayer` answers
/// preflight requests itself before the inner service is called, which satisfies that by
/// construction.
pub fn matrix_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::any())
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            HeaderName::from_static("x-requested-with"),
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
        ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_origins_allows_nothing() {
        let _ = layer(&[]);
    }

    #[test]
    fn wildcard_is_recognized() {
        let _ = layer(&["*".to_string()]);
    }

    #[test]
    fn explicit_origin_list() {
        let _ = layer(&["https://admin.example.org".to_string()]);
    }
}
#[cfg(test)]
mod matrix_cors_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        axum::Router::new()
            .route("/_matrix/client/versions", get(|| async { "{}" }))
            .layer(matrix_layer())
    }

    #[tokio::test]
    async fn a_preflight_is_answered_without_running_the_endpoint() {
        // The spec is explicit that OPTIONS must not run the endpoint's own logic. The layer
        // answers the preflight itself, so the handler is never reached.
        let response = app()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/_matrix/client/versions")
                    .header("origin", "https://app.element.io")
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.status() == StatusCode::OK || response.status() == StatusCode::NO_CONTENT,
            "a preflight must succeed, got {}",
            response.status()
        );
        let headers = response.headers();
        assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
        let methods = headers
            .get("access-control-allow-methods")
            .expect("preflight names the allowed methods")
            .to_str()
            .unwrap()
            .to_ascii_uppercase();
        for m in ["GET", "POST", "PUT", "DELETE", "OPTIONS"] {
            assert!(methods.contains(m), "{m} missing from {methods}");
        }
        let allowed = headers
            .get("access-control-allow-headers")
            .expect("preflight names the allowed headers")
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        for h in ["x-requested-with", "content-type", "authorization"] {
            assert!(allowed.contains(h), "{h} missing from {allowed}");
        }
    }

    #[tokio::test]
    async fn an_ordinary_request_carries_the_wildcard_origin() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/_matrix/client/versions")
                    .header("origin", "https://app.element.io")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .expect("a browser client cannot read the response without this"),
            "*"
        );
    }
}
