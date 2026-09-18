//! [`MediaError`]: every failure mode this crate produces, and its mapping onto
//! `hs_http::MatrixError`.
//!
//! Handlers convert with `?` (`MediaError: From<...>` for the lower-level errors) and axum's
//! `IntoResponse` (implemented by delegating to the `MatrixError` mapping) turns the final error
//! into the wire response, so a handler body never has to hand-construct a Matrix error shape.

use axum::response::{IntoResponse, Response};
use hs_http::{MatrixError, MatrixErrorCode};

/// Every failure this crate's repository and HTTP layers can produce.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// No media exists under this `(server_name, media_id)`.
    #[error("media not found")]
    NotFound,

    /// The media exists but has been quarantined by an administrator; ordinary callers must be
    /// told exactly what Synapse tells them (`M_NOT_FOUND`, not a distinguishing code — quarantine
    /// state is not leaked to non-admins).
    #[error("media is quarantined")]
    Quarantined,

    /// An async upload (`POST .../create`) exists but its content has not been `PUT` yet, and its
    /// reservation has not expired.
    #[error("upload not yet completed")]
    NotYetUploaded,

    /// An async upload's reservation expired before the content was `PUT`.
    #[error("upload expired")]
    UploadExpired,

    /// The request body exceeded the configured or capability-advertised size limit.
    #[error("upload exceeds the maximum allowed size of {limit} bytes")]
    TooLarge {
        /// The limit that was exceeded, in bytes.
        limit: u64,
    },

    /// A per-user or per-server upload quota (see [`crate::policy`]) rejected the request.
    #[error("upload quota exceeded: {reason}")]
    QuotaExceeded {
        /// A human-readable reason, safe to echo to the client.
        reason: String,
    },

    /// The caller supplied a `Range` header this crate cannot satisfy (`416`).
    #[error("range not satisfiable")]
    RangeNotSatisfiable,

    /// The uploaded bytes could not be decoded as any supported image format, or decoding
    /// exceeded a safety limit (dimensions, allocation size). See [`crate::sniff`].
    #[error("could not decode image: {0}")]
    DecodeFailed(String),

    /// The requested thumbnail size or method is not one this server generates and dynamic
    /// thumbnailing is disabled or the size is outside the configured bounds.
    #[error("unsupported thumbnail size or method")]
    UnsupportedThumbnail,

    /// A malformed `multipart/mixed` federation media response (see [`crate::multipart`]).
    #[error("malformed multipart body: {0}")]
    MalformedMultipart(String),

    /// The configured object-store backend could not be constructed (bad config) or an operation
    /// against it failed (I/O, network).
    #[error("object store error: {0}")]
    Store(String),

    /// The metadata table (`hs-tables`/`hs-kv`) failed or a stored row would not decode.
    #[error("metadata store error: {0}")]
    Metadata(String),

    /// A caller-supplied value (media ID shape, filename, content type) was invalid.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Authentication failed. Carries the original `hs_auth::error::MatrixError` (`hs-auth`
    /// predates `hs-http` and has its own, differently-shaped error type — see
    /// `crate::state::MediaRequester`'s doc) so the real status and errcode reach the client
    /// rather than collapsing every auth failure into a generic `400`.
    #[error("authentication failed: {0}")]
    Auth(hs_auth::error::MatrixError),

    /// Content scanning (`crate::scanning`) rejected this upload outright (`block` mode's bad
    /// verdict, or a per-reason `unscannable`/`oversize` policy configured to `block`). Per RFC
    /// 0008 section 5's `block` mode, the content is never persisted when this is returned —
    /// `crate::repository::MediaRepository` returns this *before* calling `object_store.put`.
    #[error("upload rejected by content scanning: {0}")]
    RejectedByScanner(String),
}

impl From<object_store::Error> for MediaError {
    fn from(e: object_store::Error) -> Self {
        match e {
            object_store::Error::NotFound { .. } => MediaError::NotFound,
            other => MediaError::Store(other.to_string()),
        }
    }
}

impl From<hs_tables::TableError> for MediaError {
    fn from(e: hs_tables::TableError) -> Self {
        MediaError::Metadata(e.to_string())
    }
}

impl From<hs_kv::KvError> for MediaError {
    fn from(e: hs_kv::KvError) -> Self {
        MediaError::Metadata(e.to_string())
    }
}

impl MediaError {
    /// The Matrix error this maps to. Kept separate from `IntoResponse` so callers that need the
    /// status code or errcode without building a full response (logging, tests) can use it too.
    #[must_use]
    pub fn to_matrix_error(&self) -> MatrixError {
        match self {
            MediaError::NotFound | MediaError::Quarantined => {
                MatrixError::not_found("Media not found")
            }
            MediaError::NotYetUploaded => MatrixError::custom(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                MatrixErrorCode::Other("M_NOT_YET_UPLOADED".to_string()),
                "The content has not yet been uploaded",
            ),
            MediaError::UploadExpired => MatrixError::not_found("The upload reservation expired"),
            MediaError::TooLarge { .. } => MatrixError::custom(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                MatrixErrorCode::TooLarge,
                self.to_string(),
            ),
            MediaError::QuotaExceeded { reason } => MatrixError::custom(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                MatrixErrorCode::TooLarge,
                reason.clone(),
            ),
            MediaError::RangeNotSatisfiable => MatrixError::custom(
                axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
                MatrixErrorCode::Unknown,
                "Range not satisfiable",
            ),
            MediaError::DecodeFailed(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::Unknown,
                format!("Could not decode image: {msg}"),
            ),
            MediaError::UnsupportedThumbnail => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::Unknown,
                "Unsupported thumbnail size or method",
            ),
            MediaError::MalformedMultipart(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_GATEWAY,
                MatrixErrorCode::Unknown,
                format!("Malformed remote response: {msg}"),
            ),
            MediaError::Store(msg) | MediaError::Metadata(msg) => MatrixError::custom(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                MatrixErrorCode::Unknown,
                msg.clone(),
            ),
            MediaError::InvalidInput(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::InvalidParam,
                msg.clone(),
            ),
            MediaError::Auth(e) => MatrixError::custom(
                e.status(),
                MatrixErrorCode::Other(e.errcode().as_str().to_string()),
                e.to_string(),
            ),
            MediaError::RejectedByScanner(reason) => MatrixError::custom(
                axum::http::StatusCode::FORBIDDEN,
                MatrixErrorCode::Forbidden,
                format!("upload rejected by content scanning: {reason}"),
            ),
        }
    }
}

impl IntoResponse for MediaError {
    fn into_response(self) -> Response {
        self.to_matrix_error().into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_maps_to_404() {
        let e = MediaError::NotFound;
        let me = e.to_matrix_error();
        assert_eq!(me.status, axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn quarantined_media_looks_identical_to_not_found() {
        // Deliberate: quarantine state must not be observable by a non-admin caller.
        let not_found = MediaError::NotFound.to_matrix_error();
        let quarantined = MediaError::Quarantined.to_matrix_error();
        assert_eq!(not_found.status, quarantined.status);
        assert_eq!(not_found.errcode, quarantined.errcode);
    }

    #[test]
    fn too_large_is_413_with_m_too_large() {
        let e = MediaError::TooLarge { limit: 100 };
        let me = e.to_matrix_error();
        assert_eq!(me.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(me.errcode.as_str(), "M_TOO_LARGE");
    }

    #[test]
    fn range_not_satisfiable_is_416() {
        let me = MediaError::RangeNotSatisfiable.to_matrix_error();
        assert_eq!(me.status, axum::http::StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[test]
    fn rejected_by_scanner_is_403_with_m_forbidden() {
        let e = MediaError::RejectedByScanner("infected: Eicar-Test-Signature".into());
        let me = e.to_matrix_error();
        assert_eq!(me.status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(me.errcode.as_str(), "M_FORBIDDEN");
    }
}
