//! Helpers to construct and assert the spec's standard error shape
//! (`{"errcode": "M_...", "error": "...", ...}`) without every scenario re-deriving the JSON
//! pointer path by hand.

use http::StatusCode;
use serde_json::Value;

/// An expected Matrix API error: HTTP status plus `errcode`. Build with [`MatrixErrorExpectation::new`]
/// and refine with [`MatrixErrorExpectation::with_extra`] for endpoint-specific fields
/// (`soft_logout`, `retry_after_ms`, ...).
#[derive(Debug, Clone)]
pub struct MatrixErrorExpectation {
    status: StatusCode,
    errcode: &'static str,
    extra: Vec<(&'static str, Value)>,
}

impl MatrixErrorExpectation {
    /// Expect `status` with the given `errcode` (for example `"M_FORBIDDEN"`).
    #[must_use]
    pub fn new(status: StatusCode, errcode: &'static str) -> Self {
        Self {
            status,
            errcode,
            extra: Vec::new(),
        }
    }

    /// Also require `body[key] == value`.
    #[must_use]
    pub fn with_extra(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.extra.push((key, value.into()));
        self
    }

    /// Checks `actual_status` and `body` against this expectation, panicking with a diagnostic
    /// message (not a bare `assert_eq!` on the whole body, which is unreadable for large error
    /// payloads) if either mismatches.
    pub fn check(&self, actual_status: StatusCode, body: &Value) {
        assert_eq!(
            actual_status, self.status,
            "expected HTTP status {}, got {} (body: {body})",
            self.status, actual_status
        );
        let actual_errcode = body.get("errcode").and_then(Value::as_str);
        assert_eq!(
            actual_errcode,
            Some(self.errcode),
            "expected errcode {:?}, got {:?} (body: {body})",
            self.errcode,
            actual_errcode
        );
        assert!(
            body.get("error").and_then(Value::as_str).is_some(),
            "Matrix error body is missing a human-readable \"error\" field: {body}"
        );
        for (key, expected) in &self.extra {
            let actual = body.get(*key);
            assert_eq!(
                actual,
                Some(expected),
                "expected body[{key:?}] == {expected}, got {actual:?} (body: {body})"
            );
        }
    }
}

/// Shorthand for the common case: build and immediately check a [`MatrixErrorExpectation`] with
/// no extension fields.
pub fn assert_matrix_error(
    actual_status: StatusCode,
    body: &Value,
    status: StatusCode,
    errcode: &'static str,
) {
    MatrixErrorExpectation::new(status, errcode).check(actual_status, body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_a_well_formed_error_body() {
        let body = json!({"errcode": "M_FORBIDDEN", "error": "nope"});
        assert_matrix_error(
            StatusCode::FORBIDDEN,
            &body,
            StatusCode::FORBIDDEN,
            "M_FORBIDDEN",
        );
    }

    #[test]
    fn checks_extension_fields() {
        let body = json!({"errcode": "M_UNKNOWN_TOKEN", "error": "bad token", "soft_logout": true});
        MatrixErrorExpectation::new(StatusCode::UNAUTHORIZED, "M_UNKNOWN_TOKEN")
            .with_extra("soft_logout", true)
            .check(StatusCode::UNAUTHORIZED, &body);
    }

    #[test]
    #[should_panic(expected = "expected errcode")]
    fn panics_on_wrong_errcode() {
        let body = json!({"errcode": "M_FORBIDDEN", "error": "nope"});
        assert_matrix_error(
            StatusCode::FORBIDDEN,
            &body,
            StatusCode::FORBIDDEN,
            "M_UNKNOWN_TOKEN",
        );
    }

    #[test]
    #[should_panic(expected = "missing a human-readable")]
    fn panics_when_error_message_is_missing() {
        let body = json!({"errcode": "M_FORBIDDEN"});
        assert_matrix_error(
            StatusCode::FORBIDDEN,
            &body,
            StatusCode::FORBIDDEN,
            "M_FORBIDDEN",
        );
    }
}
