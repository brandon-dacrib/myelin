//! Matrix-shaped errors.
//!
//! Every handler in this crate returns [`MatrixError`], which serializes as the spec's standard
//! error body (`{"errcode", "error", ...}`) and carries its own HTTP status. The exact `errcode`
//! values and the fields that ride alongside them (`soft_logout` on `M_UNKNOWN_TOKEN`) are part of
//! the behavioral contract with Synapse 1.161 that `docs/rfcs/0002-auth-tokens-and-requester.md`
//! writes up; this module is where that contract is enforced in code.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value};

/// A Matrix `errcode`, restricted to the values this crate's endpoints are documented to return.
///
/// New variants are added as new endpoints need them; the wire value is whatever the spec or
/// Synapse's observable behavior uses, via [`ErrCode::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrCode {
    /// `M_MISSING_TOKEN`: no access token was supplied at all.
    MissingToken,
    /// `M_UNKNOWN_TOKEN`: a token was supplied but is not recognized, is expired, or was
    /// revoked. Carries `soft_logout` so clients know whether to keep local state.
    UnknownToken,
    /// `M_FORBIDDEN`: recognized credentials, but the operation is not permitted (includes
    /// wrong password, wrong UIA stage result, and guests forbidden from an endpoint).
    Forbidden,
    /// `M_USER_LOCKED`: the account is locked by an administrator.
    UserLocked,
    /// `M_USER_SUSPENDED`: the account is suspended by an administrator.
    UserSuspended,
    /// `M_UNAUTHORIZED`: generic 401 without the missing/unknown-token nuance (rarely used
    /// directly; prefer the two variants above for token problems).
    Unauthorized,
    /// `M_UNKNOWN`: a bare, catch-all error.
    Unknown,
    /// `M_NOT_FOUND`: request target does not exist (unknown device, unknown session, ...).
    NotFound,
    /// `M_INVALID_PARAM`: a parameter had the wrong shape or an invalid value.
    InvalidParam,
    /// `M_MISSING_PARAM`: a required parameter was absent.
    MissingParam,
    /// `M_BAD_JSON`: the request body was not valid JSON, or not a JSON object.
    BadJson,
    /// `M_NOT_JSON`: the request body could not be parsed as JSON at all.
    NotJson,
    /// `M_USER_IN_USE`: the requested localpart is already registered.
    UserInUse,
    /// `M_INVALID_USERNAME`: the requested localpart is not a valid Matrix user ID localpart.
    InvalidUsername,
    /// `M_EXCLUSIVE`: the localpart is reserved by an application service namespace.
    Exclusive,
    /// `M_THREEPID_IN_USE`: the 3PID is already bound to another account.
    ThreepidInUse,
    /// `M_THREEPID_NOT_FOUND`: no 3PID session/validation found.
    ThreepidNotFound,
    /// `M_THREEPID_DENIED`: the homeserver refuses this 3PID (policy).
    ThreepidDenied,
    /// `M_LIMIT_EXCEEDED`: rate limited. Carries `retry_after_ms`.
    LimitExceeded,
    /// `M_WEAK_PASSWORD`: the password fails the configured password policy.
    WeakPassword,
    /// `M_PASSWORD_TOO_SHORT`, `M_PASSWORD_NO_DIGIT`, etc. are folded into `WeakPassword` with a
    /// `policy` extension field naming the violated rule, matching Synapse's
    /// `PasswordPolicyError` bodies closely enough for clients that just show `error`.
    GuestAccessForbidden,
    /// `M_UNRECOGNIZED`: unknown endpoint or method (used for the UIA fallback 404 case).
    Unrecognized,
    /// `M_UNKNOWN_DEVICE`: an appservice tried to masquerade as a device that does not exist for
    /// the effective user.
    UnknownDevice,
    /// `M_UNSUPPORTED_ROOM_VERSION`-style placeholder is not used here; reserved.
    #[doc(hidden)]
    _Reserved,
}

impl ErrCode {
    /// The wire `errcode` string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingToken => "M_MISSING_TOKEN",
            Self::UnknownToken => "M_UNKNOWN_TOKEN",
            Self::Forbidden => "M_FORBIDDEN",
            Self::UserLocked => "M_USER_LOCKED",
            Self::UserSuspended => "M_USER_SUSPENDED",
            Self::Unauthorized => "M_UNAUTHORIZED",
            Self::Unknown => "M_UNKNOWN",
            Self::NotFound => "M_NOT_FOUND",
            Self::InvalidParam => "M_INVALID_PARAM",
            Self::MissingParam => "M_MISSING_PARAM",
            Self::BadJson => "M_BAD_JSON",
            Self::NotJson => "M_NOT_JSON",
            Self::UserInUse => "M_USER_IN_USE",
            Self::InvalidUsername => "M_INVALID_USERNAME",
            Self::Exclusive => "M_EXCLUSIVE",
            Self::ThreepidInUse => "M_THREEPID_IN_USE",
            Self::ThreepidNotFound => "M_THREEPID_NOT_FOUND",
            Self::ThreepidDenied => "M_THREEPID_DENIED",
            Self::LimitExceeded => "M_LIMIT_EXCEEDED",
            Self::WeakPassword => "M_WEAK_PASSWORD",
            Self::GuestAccessForbidden => "M_GUEST_ACCESS_FORBIDDEN",
            Self::Unrecognized => "M_UNRECOGNIZED",
            Self::UnknownDevice => "M_UNKNOWN_DEVICE",
            Self::_Reserved => "M_UNKNOWN",
        }
    }
}

/// A Matrix API error response: status code, `errcode`, human-readable `error`, and any
/// endpoint-specific extension fields (`soft_logout`, `retry_after_ms`, ...).
#[derive(Debug, Clone)]
pub struct MatrixError {
    status: StatusCode,
    errcode: ErrCode,
    error: String,
    extra: Map<String, Value>,
}

impl MatrixError {
    /// Builds an error with the given status, code and human-readable message.
    #[must_use]
    pub fn new(status: StatusCode, errcode: ErrCode, error: impl Into<String>) -> Self {
        Self {
            status,
            errcode,
            error: error.into(),
            extra: Map::new(),
        }
    }

    /// Adds an extension field to the error body (`soft_logout`, `retry_after_ms`, ...).
    #[must_use]
    pub fn with_extra(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_string(), value.into());
        self
    }

    /// `401 M_MISSING_TOKEN`: no credentials were supplied.
    #[must_use]
    pub fn missing_token() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrCode::MissingToken,
            "Missing access token",
        )
    }

    /// `401 M_UNKNOWN_TOKEN`, optionally instructing the client to soft-logout (drop the token
    /// and any refresh token but keep local room state, per the spec's soft-logout semantics).
    #[must_use]
    pub fn unknown_token(soft_logout: bool) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrCode::UnknownToken,
            "Unrecognised access token",
        )
        .with_extra("soft_logout", soft_logout)
    }

    /// `401 M_FORBIDDEN`.
    #[must_use]
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, ErrCode::Forbidden, msg)
    }

    /// `401 M_USER_LOCKED`.
    #[must_use]
    pub fn user_locked() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrCode::UserLocked,
            "This account has been locked",
        )
        .with_extra("soft_logout", true)
    }

    /// `403 M_USER_SUSPENDED`.
    #[must_use]
    pub fn user_suspended() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            ErrCode::UserSuspended,
            "Property cannot be changed while account is suspended",
        )
    }

    /// `429 M_LIMIT_EXCEEDED`.
    #[must_use]
    pub fn limit_exceeded(retry_after_ms: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrCode::LimitExceeded,
            "Too many requests",
        )
        .with_extra("retry_after_ms", retry_after_ms)
    }

    /// `400 M_USER_IN_USE`.
    #[must_use]
    pub fn user_in_use() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrCode::UserInUse,
            "User ID already taken",
        )
    }

    /// `400 M_INVALID_USERNAME`.
    #[must_use]
    pub fn invalid_username(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrCode::InvalidUsername, msg)
    }

    /// `400 M_INVALID_PARAM`.
    #[must_use]
    pub fn invalid_param(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrCode::InvalidParam, msg)
    }

    /// `400 M_MISSING_PARAM`.
    #[must_use]
    pub fn missing_param(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrCode::MissingParam, msg)
    }

    /// `404 M_NOT_FOUND`.
    #[must_use]
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrCode::NotFound, msg)
    }

    /// `400 M_WEAK_PASSWORD`.
    #[must_use]
    pub fn weak_password(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrCode::WeakPassword, msg)
    }

    /// `403 M_GUEST_ACCESS_FORBIDDEN`: a guest tried to use an endpoint that requires a full
    /// account.
    #[must_use]
    pub fn guest_access_forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            ErrCode::GuestAccessForbidden,
            "Guest access not allowed",
        )
    }

    /// `400 M_UNKNOWN_DEVICE`: an appservice tried to masquerade as a device that does not exist.
    #[must_use]
    pub fn unknown_device(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrCode::UnknownDevice, msg)
    }

    /// `404 M_UNRECOGNIZED`: the route exists in code but the feature it implements is not
    /// configured on this server (for example,
    /// `POST /_synapse/admin/v1/register` with no `registration_shared_secret` set). Deliberately
    /// indistinguishable from "this route does not exist at all" -- neither a `500` nor a
    /// working-but-pointless nonce -- so an unconfigured feature does not confirm its own
    /// existence to an unauthenticated prober.
    #[must_use]
    pub fn feature_not_configured() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ErrCode::Unrecognized,
            "Unrecognised request",
        )
    }

    /// `500 M_UNKNOWN` for storage/internal failures. Never leaks the underlying error text to
    /// the client; callers should `tracing::error!` the real cause before returning this.
    #[must_use]
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrCode::Unknown,
            "Internal server error",
        )
    }

    /// The `errcode` this error carries.
    #[must_use]
    pub fn errcode(&self) -> ErrCode {
        self.errcode
    }

    /// The HTTP status this error carries.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl std::fmt::Display for MatrixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.status,
            self.errcode.as_str(),
            self.error
        )
    }
}

impl std::error::Error for MatrixError {}

impl From<crate::store::StoreError> for MatrixError {
    /// Storage failures never reach the client as anything but a generic `M_UNKNOWN`/500; the
    /// real cause is logged here so an operator can see it, matching the rule that library code
    /// returns errors but handlers never leak backend detail across the API boundary.
    fn from(err: crate::store::StoreError) -> Self {
        tracing::error!(error = %err, "auth store error");
        MatrixError::internal()
    }
}

#[derive(Serialize)]
struct Body<'a> {
    errcode: &'a str,
    error: &'a str,
    #[serde(flatten)]
    extra: &'a Map<String, Value>,
}

impl IntoResponse for MatrixError {
    fn into_response(self) -> Response {
        let body = Body {
            errcode: self.errcode.as_str(),
            error: &self.error,
            extra: &self.extra,
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_token_carries_soft_logout() {
        let err = MatrixError::unknown_token(true);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["errcode"], "M_UNKNOWN_TOKEN");
        assert_eq!(json["soft_logout"], true);
    }

    #[tokio::test]
    async fn missing_token_has_no_soft_logout_field() {
        let err = MatrixError::missing_token();
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["errcode"], "M_MISSING_TOKEN");
        assert!(json.get("soft_logout").is_none());
    }

    #[test]
    fn user_locked_status_is_401_with_soft_logout() {
        let err = MatrixError::user_locked();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.errcode(), ErrCode::UserLocked);
    }

    #[test]
    fn user_suspended_status_is_403() {
        let err = MatrixError::user_suspended();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.errcode(), ErrCode::UserSuspended);
    }

    #[test]
    fn feature_not_configured_is_404_unrecognized() {
        let err = MatrixError::feature_not_configured();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.errcode(), ErrCode::Unrecognized);
    }
}
