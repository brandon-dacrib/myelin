//! [`UserError`]: every error this crate's session actor, store and routes can produce, plus its
//! mapping onto [`hs_http::error::MatrixError`] for the HTTP layer (`crate::routes`). Follows the
//! same shape `hs_room::error::RoomError` uses for the same problem.

use hs_http::error::{MatrixError, MatrixErrorCode};

use crate::token::TokenError;

/// Errors from the user session store, hub and `/sync` route handlers.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    /// The presented `since` token was not a token this server issued, or not shaped like one.
    #[error(transparent)]
    InvalidToken(#[from] TokenError),

    /// A `filter` query parameter (or `POST /user/{userId}/filter` body) was not valid JSON, or
    /// did not match the filter shape this crate understands.
    #[error("invalid filter: {0}")]
    InvalidFilter(String),

    /// A `filter_id` referred to a filter this user never uploaded.
    #[error("unknown filter id {0:?}")]
    UnknownFilterId(String),

    /// The requested resource (account data of a given type, and so on) does not exist.
    #[error("{0}")]
    NotFound(String),

    /// The room this request named does not exist, or this crate could not load its actor.
    #[error(transparent)]
    Room(#[from] hs_room::RoomError),

    /// The underlying store reported an error.
    #[error(transparent)]
    Store(#[from] hs_kv::KvError),

    /// A stored key or value could not be decoded.
    #[error(transparent)]
    TableCodec(#[from] hs_tables::key::KeyCodecError),

    /// A typed-keyspace operation failed (either a store error or a key codec error).
    #[error(transparent)]
    Table(#[from] hs_tables::keyspace::TableError),

    /// A stored JSON value failed to (de)serialize -- should not happen for data this crate wrote
    /// itself; kept distinct from `Internal` so a genuine decode bug (corrupt data, a schema
    /// change without a migration) is easy to grep for.
    #[error("decode/encode failure: {0}")]
    Codec(String),

    /// An id this crate received (from a route path, a stored record, or an `hs-room` query) was
    /// not a valid Matrix identifier.
    #[error("invalid identifier: {0}")]
    InvalidId(String),

    /// A request for one user's own data (account data, filters) named a different `userId` in
    /// its path.
    #[error("{0}")]
    NotSelf(String),

    /// An internal invariant this crate itself is responsible for maintaining was violated
    /// (should not happen; kept distinct from a client-caused error so a bug here is not
    /// misreported as a bad request).
    #[error("internal user session invariant violated: {0}")]
    Internal(String),

    /// `hs-e2e`'s store reported an error while `/sync` was populating `to_device`,
    /// `device_lists`, `device_one_time_keys_count` or `device_unused_fallback_key_types`
    /// (`docs/rfcs/0013-e2ee-sync-extensions.md`).
    #[error("e2e store error: {0}")]
    E2e(#[from] hs_e2e::store::StoreError),
}

/// Maps the store's own error enum onto this crate's, variant for variant: a storage failure
/// stays a storage failure rather than collapsing into `Internal`, so the distinction the store
/// draws survives into whatever reads `UserError`.
impl From<crate::store::StoreError> for UserError {
    fn from(e: crate::store::StoreError) -> Self {
        use crate::store::StoreError as S;
        match e {
            S::Kv(e) => Self::Store(e),
            S::KeyCodec(e) => Self::TableCodec(e),
            S::Table(e) => Self::Table(e),
            S::Codec(msg) => Self::Codec(msg),
        }
    }
}

impl UserError {
    /// Maps to the Matrix client-server error shape.
    #[must_use]
    pub fn to_matrix_error(&self) -> MatrixError {
        match self {
            Self::InvalidToken(e) => e.to_matrix_error(),
            Self::InvalidFilter(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::BadJson,
                msg.clone(),
            ),
            Self::UnknownFilterId(id) => MatrixError::not_found(format!("unknown filter {id:?}")),
            Self::NotFound(msg) => MatrixError::not_found(msg.clone()),
            Self::Room(e) => e.to_matrix_error(),
            Self::InvalidId(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::InvalidParam,
                msg.clone(),
            ),
            Self::NotSelf(msg) => MatrixError::forbidden(msg.clone()),
            Self::Store(_)
            | Self::TableCodec(_)
            | Self::Table(_)
            | Self::Codec(_)
            | Self::Internal(_)
            | Self::E2e(_) => MatrixError::custom(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                MatrixErrorCode::Unknown,
                self.to_string(),
            ),
        }
    }
}

impl axum::response::IntoResponse for UserError {
    fn into_response(self) -> axum::response::Response {
        self.to_matrix_error().into_response()
    }
}
