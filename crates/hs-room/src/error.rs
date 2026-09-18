//! [`RoomError`]: every error this crate's room actor and persistence pipeline can produce, plus
//! its mapping onto [`hs_http::error::MatrixError`] for the HTTP layer (`crate::routes`).

use hs_http::error::{MatrixError, MatrixErrorCode};

/// Errors from the room actor: building, authorizing and persisting events, and the queries
/// built on top of them.
#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    /// The room does not exist (no `m.room.create` has ever been persisted for it).
    #[error("room {0} not found")]
    RoomNotFound(String),

    /// The event does not exist, or is not visible to the caller.
    #[error("event {0} not found")]
    EventNotFound(String),

    /// The requested room version is not one this server supports.
    #[error("unsupported room version {0:?}")]
    UnsupportedRoomVersion(String),

    /// The room already exists (a duplicate `m.room.create`).
    #[error("room {0} already exists")]
    RoomAlreadyExists(String),

    /// The event's JSON, hashes or size failed [`hs_model::event::Event::parse`] or the pipeline's
    /// own size/shape checks.
    #[error("invalid event: {0}")]
    InvalidEvent(#[from] hs_model::EventError),

    /// Authorization rejected the event (`hs-state`'s `check_auth_events_selection` /
    /// `check_event_auth`, or this crate's own membership precheck).
    #[error("event rejected: {0}")]
    Forbidden(String),

    /// The request is well-formed but violates a spec precondition that is not, strictly, an
    /// authorization rule (missing required content field, malformed `m.relates_to`, and so on).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// A signing or hashing operation failed.
    #[error(transparent)]
    Signing(#[from] hs_model::SigningError),

    /// A redaction operation failed.
    #[error(transparent)]
    Redaction(#[from] hs_model::RedactionError),

    /// State resolution or another `hs-state` operation failed.
    #[error("state error: {0}")]
    State(String),

    /// The underlying store reported an error.
    #[error(transparent)]
    Store(#[from] hs_kv::KvError),

    /// A stored key or value could not be decoded.
    #[error(transparent)]
    TableCodec(#[from] hs_tables::key::KeyCodecError),

    /// A typed-keyspace operation failed (either a store error or a key codec error).
    #[error(transparent)]
    Table(#[from] hs_tables::keyspace::TableError),

    /// The pagination token supplied by the client was not one this server issued.
    #[error("invalid pagination token")]
    InvalidPaginationToken,

    /// A room-version or event-shape invariant this crate itself maintains was violated
    /// (should not happen; kept distinct from [`RoomError::InvalidEvent`] so a bug here is not
    /// misreported as a client error upstream).
    #[error("internal room actor invariant violated: {0}")]
    Internal(String),
}

impl RoomError {
    /// Maps to the Matrix client-server error shape.
    #[must_use]
    pub fn to_matrix_error(&self) -> MatrixError {
        match self {
            Self::RoomNotFound(_) | Self::EventNotFound(_) => MatrixError::not_found(self.to_string()),
            Self::UnsupportedRoomVersion(v) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::UnsupportedRoomVersion,
                format!("unsupported room version {v:?}"),
            ),
            Self::RoomAlreadyExists(_) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::RoomInUse,
                self.to_string(),
            ),
            Self::InvalidEvent(_) | Self::BadRequest(_) => {
                MatrixError::bad_json(self.to_string())
            }
            Self::Forbidden(msg) => MatrixError::forbidden(msg.clone()),
            Self::InvalidPaginationToken => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::InvalidParam,
                "invalid pagination token",
            ),
            Self::Signing(_)
            | Self::Redaction(_)
            | Self::State(_)
            | Self::Store(_)
            | Self::TableCodec(_)
            | Self::Table(_)
            | Self::Internal(_) => MatrixError::custom(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                MatrixErrorCode::Unknown,
                self.to_string(),
            ),
        }
    }
}

impl axum::response::IntoResponse for RoomError {
    fn into_response(self) -> axum::response::Response {
        self.to_matrix_error().into_response()
    }
}

impl From<hs_state::error::AuthError> for RoomError {
    fn from(e: hs_state::error::AuthError) -> Self {
        Self::Forbidden(e.to_string())
    }
}
