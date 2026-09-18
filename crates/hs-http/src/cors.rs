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
