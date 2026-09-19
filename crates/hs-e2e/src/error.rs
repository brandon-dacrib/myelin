//! [`E2eError`]: this crate's handler error type, mapped to `hs-http`'s Matrix error shape
//! (`hs_http::error::MatrixError`) at the edge via [`axum::response::IntoResponse`].

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};

use crate::store::StoreError;

/// Errors a handler in this crate can return.
#[derive(Debug, thiserror::Error)]
pub enum E2eError {
    /// The request body was not valid JSON, or not a JSON object.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// A referenced record (a backup version, a device, ...) does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The caller is not allowed to perform this operation.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The caller supplied the wrong backup version (`M_WRONG_ROOM_KEYS_VERSION`): a write to
    /// `/room_keys/keys` named a version that is not the current one.
    #[error("wrong backup version: expected {current}, got {given}")]
    WrongBackupVersion {
        /// The version the caller supplied.
        given: String,
        /// The version that is actually current.
        current: String,
    },
    /// A storage backend failure.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl E2eError {
    /// Builds an [`E2eError::WrongBackupVersion`].
    #[must_use]
    pub fn wrong_backup_version(given: impl Into<String>, current: impl Into<String>) -> Self {
        Self::WrongBackupVersion {
            given: given.into(),
            current: current.into(),
        }
    }
}

impl IntoResponse for E2eError {
    fn into_response(self) -> Response {
        let matrix_error = match &self {
            // `M_BAD_JSON` (not `M_INVALID_PARAM`) is the spec's errcode for "the request body
            // does not have the shape this endpoint requires" -- every `BadRequest` in this crate
            // is exactly that (a missing/misshapen field, not an otherwise-valid parameter with
            // an invalid value). Complement's malformed-shape tests
            // (`TestKeysQueryWithDeviceIDAsObjectFails`, `upload_keys_test.go`'s "Rejects invalid
            // device keys") assert this exact errcode, not just the 400 status.
            Self::BadRequest(msg) => MatrixError::custom(
                StatusCode::BAD_REQUEST,
                MatrixErrorCode::BadJson,
                msg.clone(),
            ),
            Self::NotFound(msg) => MatrixError::not_found(msg.clone()),
            Self::Forbidden(msg) => MatrixError::forbidden(msg.clone()),
            Self::WrongBackupVersion { given, current } => MatrixError::custom(
                StatusCode::FORBIDDEN,
                MatrixErrorCode::WrongRoomKeysVersion,
                format!(
                    "Wrong backup version: this session is on {given}, the current version is {current}"
                ),
            ),
            Self::Store(StoreError::NotFound(msg)) => MatrixError::not_found(msg.clone()),
            Self::Store(StoreError::Conflict(msg)) => {
                MatrixError::custom(StatusCode::CONFLICT, MatrixErrorCode::Unknown, msg.clone())
            }
            Self::Store(StoreError::Backend(msg)) => {
                tracing::error!(error = %msg, "hs-e2e storage backend error");
                MatrixError::custom(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    MatrixErrorCode::Unknown,
                    "Internal storage error",
                )
            }
        };
        matrix_error.into_response()
    }
}
