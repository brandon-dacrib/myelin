//! RFC 9457 problem details for `/api/v1`.
//!
//! See `docs/rfcs/0004-admin-api.md` section 3.5 for the closed catalog this module implements.
//! The Matrix routes never use this shape; see `crate::error` for those.

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// One field-level validation failure, referenced from [`Problem::errors`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationError {
    /// A JSON Pointer into the request body, or `param:<name>` for a query parameter.
    pub pointer: String,
    pub detail: String,
}

impl ValidationError {
    pub fn new(pointer: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            pointer: pointer.into(),
            detail: detail.into(),
        }
    }
}

/// An RFC 9457 problem details object, plus the extension members RFC 0004 section 3.5 defines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Problem {
    pub r#type: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errcode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ValidationError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_scope: Option<String>,
    /// Headers to add to the HTTP response (`WWW-Authenticate`, `Retry-After`, `Allow`), beyond
    /// `Content-Type`. Not part of the JSON body; skipped by serde via a custom impl below is
    /// unnecessary since this field is never serialized (see `#[serde(skip)]`).
    #[serde(skip)]
    pub extra_headers: Vec<(HeaderName, HeaderValue)>,
}

/// One entry of the closed catalog (RFC 0004 section 3.5): `(type slug, title, status)`.
macro_rules! problem_kind {
    ($name:ident, $slug:literal, $title:literal, $status:expr) => {
        pub fn $name() -> Problem {
            Problem::new($slug, $title, $status)
        }
    };
}

impl Problem {
    pub fn new(type_slug: &str, title: &str, status: StatusCode) -> Self {
        Self {
            r#type: format!("urn:hs:problem:{type_slug}"),
            title: title.to_string(),
            status: status.as_u16(),
            detail: None,
            instance: None,
            request_id: None,
            errcode: None,
            errors: Vec::new(),
            retry_after_ms: None,
            required_scope: None,
            extra_headers: Vec::new(),
        }
    }

    problem_kind!(
        validation_failed,
        "validation-failed",
        "Validation failed",
        StatusCode::BAD_REQUEST
    );
    problem_kind!(
        invalid_cursor,
        "invalid-cursor",
        "Invalid cursor",
        StatusCode::BAD_REQUEST
    );
    problem_kind!(
        unauthenticated,
        "unauthenticated",
        "Unauthenticated",
        StatusCode::UNAUTHORIZED
    );
    problem_kind!(
        insufficient_scope,
        "insufficient-scope",
        "Insufficient scope",
        StatusCode::FORBIDDEN
    );
    problem_kind!(forbidden, "forbidden", "Forbidden", StatusCode::FORBIDDEN);
    problem_kind!(not_found, "not-found", "Not found", StatusCode::NOT_FOUND);
    problem_kind!(
        method_not_allowed,
        "method-not-allowed",
        "Method not allowed",
        StatusCode::METHOD_NOT_ALLOWED
    );
    problem_kind!(conflict, "conflict", "Conflict", StatusCode::CONFLICT);
    problem_kind!(
        idempotency_key_in_flight,
        "idempotency-key-in-flight",
        "Idempotency key in flight",
        StatusCode::CONFLICT
    );
    problem_kind!(
        precondition_failed,
        "precondition-failed",
        "Precondition failed",
        StatusCode::PRECONDITION_FAILED
    );
    problem_kind!(
        payload_too_large,
        "payload-too-large",
        "Payload too large",
        StatusCode::PAYLOAD_TOO_LARGE
    );
    problem_kind!(
        unsupported_media_type,
        "unsupported-media-type",
        "Unsupported media type",
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    problem_kind!(
        idempotency_key_payload_mismatch,
        "idempotency-key-payload-mismatch",
        "Idempotency key reused with a different request",
        StatusCode::UNPROCESSABLE_ENTITY
    );
    problem_kind!(
        unprocessable,
        "unprocessable",
        "Unprocessable",
        StatusCode::UNPROCESSABLE_ENTITY
    );
    problem_kind!(
        rate_limited,
        "rate-limited",
        "Rate limited",
        StatusCode::TOO_MANY_REQUESTS
    );
    problem_kind!(
        internal,
        "internal",
        "Internal error",
        StatusCode::INTERNAL_SERVER_ERROR
    );
    problem_kind!(
        not_implemented,
        "not-implemented",
        "Not implemented",
        StatusCode::NOT_IMPLEMENTED
    );
    problem_kind!(
        unavailable,
        "unavailable",
        "Unavailable",
        StatusCode::SERVICE_UNAVAILABLE
    );

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn with_errcode(mut self, errcode: impl Into<String>) -> Self {
        self.errcode = Some(errcode.into());
        self
    }

    pub fn with_errors(mut self, errors: Vec<ValidationError>) -> Self {
        self.errors = errors;
        self
    }

    /// Sets `retry_after_ms` and the matching `Retry-After` header (seconds, rounded up).
    pub fn with_retry_after_ms(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        let seconds = ms.div_ceil(1000);
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            self.extra_headers.push((header::RETRY_AFTER, value));
        }
        self
    }

    pub fn with_required_scope(mut self, scope: impl Into<String>) -> Self {
        let scope = scope.into();
        self.required_scope = Some(scope);
        self
    }

    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.extra_headers.push((name, value));
        self
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let headers = self.extra_headers.clone();
        let body = serde_json::to_vec(&self).unwrap_or_default();
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/problem+json")
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        let response_headers: &mut HeaderMap = response.headers_mut();
        for (name, value) in headers {
            response_headers.insert(name, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_uses_urn_prefix() {
        let p = Problem::not_found().with_detail("no such user");
        assert_eq!(p.r#type, "urn:hs:problem:not-found");
        assert_eq!(p.status, 404);
    }

    #[test]
    fn rate_limited_sets_header_and_body() {
        let p = Problem::rate_limited().with_retry_after_ms(1500);
        assert_eq!(p.retry_after_ms, Some(1500));
        assert!(
            p.extra_headers
                .iter()
                .any(|(n, v)| n == header::RETRY_AFTER && v == "2")
        );
    }

    #[test]
    fn serializes_without_null_fields() {
        let p = Problem::validation_failed();
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("detail").is_none());
        assert!(json.get("errors").is_none());
    }
}
