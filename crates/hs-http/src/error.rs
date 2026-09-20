//! The Matrix client-server error shape: `{errcode, error, ...}`.
//!
//! Used by `/_matrix/*` and `/_synapse/*` routes, which never emit RFC 9457 problem details
//! (see `crate::problem` for `/api/v1`). See the Matrix spec's "Standard error response" and
//! RFC 0004 section 3.5 ("The Matrix routes ... keep the Matrix error shape and never emit
//! problem details; `hs-http` owns both mappings.").

use std::fmt;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// A Matrix `errcode`. Open enum: the spec adds codes over time, so an [`Other`](MatrixErrorCode::Other)
/// variant carries anything this crate does not yet name explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MatrixErrorCode {
    Forbidden,
    UnknownToken,
    MissingToken,
    BadJson,
    NotJson,
    NotFound,
    LimitExceeded,
    Unknown,
    Unrecognized,
    Unauthorized,
    UserInUse,
    InvalidUsername,
    RoomInUse,
    InvalidParam,
    MissingParam,
    TooLarge,
    Exclusive,
    WeakPassword,
    UserDeactivated,
    ResourceLimitExceeded,
    ThreepidInUse,
    ThreepidNotFound,
    ThreepidDenied,
    ServerNotTrusted,
    UnsupportedRoomVersion,
    IncompatibleRoomVersion,
    BadState,
    GuestAccessForbidden,
    CaptchaNeeded,
    CaptchaInvalid,
    InvalidSignature,
    WrongRoomKeysVersion,
    /// Anything not named above, verbatim (for example a namespaced `M_HS_*` extension code).
    Other(String),
}

impl MatrixErrorCode {
    /// The wire form, e.g. `M_NOT_FOUND`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Forbidden => "M_FORBIDDEN",
            Self::UnknownToken => "M_UNKNOWN_TOKEN",
            Self::MissingToken => "M_MISSING_TOKEN",
            Self::BadJson => "M_BAD_JSON",
            Self::NotJson => "M_NOT_JSON",
            Self::NotFound => "M_NOT_FOUND",
            Self::LimitExceeded => "M_LIMIT_EXCEEDED",
            Self::Unknown => "M_UNKNOWN",
            Self::Unrecognized => "M_UNRECOGNIZED",
            Self::Unauthorized => "M_UNAUTHORIZED",
            Self::UserInUse => "M_USER_IN_USE",
            Self::InvalidUsername => "M_INVALID_USERNAME",
            Self::RoomInUse => "M_ROOM_IN_USE",
            Self::InvalidParam => "M_INVALID_PARAM",
            Self::MissingParam => "M_MISSING_PARAM",
            Self::TooLarge => "M_TOO_LARGE",
            Self::Exclusive => "M_EXCLUSIVE",
            Self::WeakPassword => "M_WEAK_PASSWORD",
            Self::UserDeactivated => "M_USER_DEACTIVATED",
            Self::ResourceLimitExceeded => "M_RESOURCE_LIMIT_EXCEEDED",
            Self::ThreepidInUse => "M_THREEPID_IN_USE",
            Self::ThreepidNotFound => "M_THREEPID_NOT_FOUND",
            Self::ThreepidDenied => "M_THREEPID_DENIED",
            Self::ServerNotTrusted => "M_SERVER_NOT_TRUSTED",
            Self::UnsupportedRoomVersion => "M_UNSUPPORTED_ROOM_VERSION",
            Self::IncompatibleRoomVersion => "M_INCOMPATIBLE_ROOM_VERSION",
            Self::BadState => "M_BAD_STATE",
            Self::GuestAccessForbidden => "M_GUEST_ACCESS_FORBIDDEN",
            Self::CaptchaNeeded => "M_CAPTCHA_NEEDED",
            Self::CaptchaInvalid => "M_CAPTCHA_INVALID",
            Self::InvalidSignature => "M_INVALID_SIGNATURE",
            Self::WrongRoomKeysVersion => "M_WRONG_ROOM_KEYS_VERSION",
            Self::Other(s) => s,
        }
    }
}

impl fmt::Display for MatrixErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for MatrixErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// A Matrix-shaped error response: `{"errcode": "...", "error": "...", ...}`.
///
/// Construct with one of the named constructors (which pick the right status code and errcode
/// together, per the spec) or [`MatrixError::custom`] for anything else.
#[derive(Debug, Clone)]
pub struct MatrixError {
    pub status: StatusCode,
    pub errcode: MatrixErrorCode,
    pub error: String,
    pub retry_after_ms: Option<u64>,
    pub soft_logout: bool,
    /// Extra top-level members beyond the spec's own (rare; e.g. `M_LIMIT_EXCEEDED` carries no
    /// others today, but a future errcode might).
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl MatrixError {
    pub fn custom(status: StatusCode, errcode: MatrixErrorCode, error: impl Into<String>) -> Self {
        Self {
            status,
            errcode,
            error: error.into(),
            retry_after_ms: None,
            soft_logout: false,
            extra: serde_json::Map::new(),
        }
    }

    /// `404`, unrecognized endpoint. The Matrix spec uses `M_UNRECOGNIZED` (not `M_NOT_FOUND`,
    /// which is for content that is absent within a recognized endpoint) for "this path does not
    /// exist on this server".
    pub fn unrecognized() -> Self {
        Self::custom(
            StatusCode::NOT_FOUND,
            MatrixErrorCode::Unrecognized,
            "Unrecognized request",
        )
    }

    /// `405`, a recognized path called with a method it does not support.
    pub fn method_not_allowed(allowed: &[axum::http::Method]) -> Self {
        let mut e = Self::custom(
            StatusCode::METHOD_NOT_ALLOWED,
            MatrixErrorCode::Unrecognized,
            "Unrecognized request method",
        );
        // An empty set means the caller does not know which methods the path accepts (the generic
        // router fallback is in exactly that position): say nothing rather than advertise an
        // empty `Allow`, which a client would read as "no method works here".
        if !allowed.is_empty() {
            let list = allowed
                .iter()
                .map(|m| m.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            e.extra
                .insert("allow".into(), serde_json::Value::String(list));
        }
        e
    }

    /// `429`, over the rate limit. `retry_after_ms` becomes both the JSON member and the
    /// `Retry-After` header (rounded up to whole seconds, per RFC 9110).
    pub fn rate_limited(retry_after_ms: u64) -> Self {
        let mut e = Self::custom(
            StatusCode::TOO_MANY_REQUESTS,
            MatrixErrorCode::LimitExceeded,
            "Too many requests",
        );
        e.retry_after_ms = Some(retry_after_ms);
        e
    }

    /// `401`, the caller's session was soft-logged-out (its token is recognized but no longer
    /// valid for anything but logout): `errcode: M_UNKNOWN_TOKEN`, `soft_logout: true`.
    pub fn soft_logout(message: impl Into<String>) -> Self {
        let mut e = Self::custom(
            StatusCode::UNAUTHORIZED,
            MatrixErrorCode::UnknownToken,
            message,
        );
        e.soft_logout = true;
        e
    }

    pub fn missing_token() -> Self {
        Self::custom(
            StatusCode::UNAUTHORIZED,
            MatrixErrorCode::MissingToken,
            "Missing access token",
        )
    }

    pub fn unknown_token(message: impl Into<String>) -> Self {
        Self::custom(
            StatusCode::UNAUTHORIZED,
            MatrixErrorCode::UnknownToken,
            message,
        )
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::custom(StatusCode::FORBIDDEN, MatrixErrorCode::Forbidden, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::custom(StatusCode::NOT_FOUND, MatrixErrorCode::NotFound, message)
    }

    pub fn not_json(message: impl Into<String>) -> Self {
        Self::custom(StatusCode::BAD_REQUEST, MatrixErrorCode::NotJson, message)
    }

    pub fn bad_json(message: impl Into<String>) -> Self {
        Self::custom(StatusCode::BAD_REQUEST, MatrixErrorCode::BadJson, message)
    }

    pub fn missing_param(name: &str) -> Self {
        Self::custom(
            StatusCode::BAD_REQUEST,
            MatrixErrorCode::MissingParam,
            format!("Missing parameter: {name}"),
        )
    }

    fn to_json(&self) -> serde_json::Value {
        let mut map = self.extra.clone();
        map.insert(
            "errcode".into(),
            serde_json::Value::String(self.errcode.as_str().to_string()),
        );
        map.insert(
            "error".into(),
            serde_json::Value::String(self.error.clone()),
        );
        if let Some(ms) = self.retry_after_ms {
            map.insert("retry_after_ms".into(), serde_json::Value::from(ms));
        }
        if self.soft_logout {
            map.insert("soft_logout".into(), serde_json::Value::Bool(true));
        }
        serde_json::Value::Object(map)
    }
}

impl fmt::Display for MatrixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.status, self.errcode, self.error)
    }
}

impl std::error::Error for MatrixError {}

impl IntoResponse for MatrixError {
    fn into_response(self) -> Response {
        let status = self.status;
        let retry_after_ms = self.retry_after_ms;
        let body = self.to_json();
        let mut response = (status, axum::Json(body)).into_response();
        if let Some(ms) = retry_after_ms {
            let seconds = ms.div_ceil(1000);
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrecognized_is_404() {
        let e = MatrixError::unrecognized();
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        assert_eq!(e.errcode.as_str(), "M_UNRECOGNIZED");
    }

    #[test]
    fn rate_limited_carries_retry_after() {
        let e = MatrixError::rate_limited(2500);
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        let json = e.to_json();
        assert_eq!(json["errcode"], "M_LIMIT_EXCEEDED");
        assert_eq!(json["retry_after_ms"], 2500);
    }

    #[test]
    fn soft_logout_json_shape() {
        let e = MatrixError::soft_logout("session revoked");
        let json = e.to_json();
        assert_eq!(json["errcode"], "M_UNKNOWN_TOKEN");
        assert_eq!(json["soft_logout"], true);
    }

    #[tokio::test]
    async fn rate_limited_sets_retry_after_header() {
        let e = MatrixError::rate_limited(1500);
        let response = e.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "2");
    }
}
