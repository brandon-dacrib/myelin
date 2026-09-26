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

    /// A parameter is present and unusable: not the right type, or not a valid identifier.
    /// `400 M_INVALID_PARAM`.
    #[error("{0}")]
    InvalidParam(String),

    /// An alias being set as a room's canonical or alternative alias does not point at that
    /// room: it does not exist, or it is another room's. `400 M_BAD_ALIAS`.
    #[error("{0}")]
    BadAlias(String),

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

    /// [`crate::actor::RoomActor::forget`]: the user is still joined to the room (or the room
    /// does not exist at all -- `crate::routes::membership::post_forget` reuses this variant for
    /// that case too, since the spec documents only one error shape for this endpoint: `400
    /// M_UNKNOWN`).
    #[error("{0}")]
    StillJoined(String),

    /// The room has been blocked by a server administrator (`hs-admin`'s `rooms.set_blocked`,
    /// enforced by [`crate::actor::RoomActor::send_event_citing`]'s own precheck): new events and
    /// joins from local users are rejected while the block is in effect. Carries the
    /// administrator's reason, if one was given.
    #[error(
        "this room has been blocked by a server administrator{}",
        .0.as_deref().map(|r| format!(": {r}")).unwrap_or_default()
    )]
    RoomBlocked(Option<String>),

    /// [`crate::actor::RoomActor::persist`]'s belt-and-braces cluster-fencing check
    /// (`docs/status/03-cluster.md` item 4) found that this replica no longer holds the shard
    /// this room belongs to -- a real ownership handoff raced the routing gate that should have
    /// kept this replica from handling the request at all. The caller should retry against the
    /// current owner (`hs-cli`'s forwarding layer, not this crate, decides who that is).
    #[error("fenced: {0}")]
    Fenced(String),

    /// [`crate::actor::RoomActor::accept_remote_event`]: the event names a `prev_events` or
    /// `auth_events` entry this actor does not hold. The ordinary federation case of an event
    /// arriving before its ancestors have been backfilled -- not a hard protocol violation, and
    /// deliberately distinct from [`RoomError::Forbidden`] so a caller (track 06) can tell "go
    /// backfill these IDs and retry" apart from "this event is rejected, do not retry".
    #[error("missing {0:?}: backfill required before this event can be authorized")]
    MissingAncestors(Vec<ruma::OwnedEventId>),
    /// [`crate::remote_join::RemoteJoin`]: a join of a room hosted elsewhere could not be
    /// completed for a reason that is neither the room refusing it (`Forbidden`) nor no server
    /// knowing it (`RoomNotFound`): every server asked was unreachable, or answered with
    /// something that was not a join. `502 M_UNKNOWN`, the shape Synapse gives the same failure.
    #[error("could not join the room through federation: {0}")]
    RemoteJoinFailed(String),
    /// [`crate::backfill::Backfill`]: a room's history from before the oldest event this server
    /// holds could not be fetched: no server in the room could be reached, or none answered with
    /// a usable batch. `502 M_UNKNOWN` like [`RoomError::RemoteJoinFailed`], though
    /// `GET /messages` never surfaces it -- a page is answered from what is held and the failure
    /// is logged.
    #[error("could not fetch the room's earlier history through federation: {0}")]
    BackfillFailed(String),
}

impl RoomError {
    /// Maps to the Matrix client-server error shape.
    #[must_use]
    pub fn to_matrix_error(&self) -> MatrixError {
        match self {
            Self::RoomNotFound(_) | Self::EventNotFound(_) => {
                MatrixError::not_found(self.to_string())
            }
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
            // The spec gives an event over the 65,535-byte limit its own status and code: `413
            // M_TOO_LARGE`. It was answered as `400 M_BAD_JSON`, which tells a client its JSON is
            // malformed when the problem is that there is too much of it.
            Self::InvalidEvent(hs_model::EventError::TooLarge { .. }) => MatrixError::custom(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                MatrixErrorCode::TooLarge,
                self.to_string(),
            ),
            Self::InvalidEvent(_) | Self::BadRequest(_) => MatrixError::bad_json(self.to_string()),
            Self::InvalidParam(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::InvalidParam,
                msg.clone(),
            ),
            Self::BadAlias(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::BadAlias,
                msg.clone(),
            ),
            Self::Forbidden(msg) => MatrixError::forbidden(msg.clone()),
            Self::RoomBlocked(_) => MatrixError::forbidden(self.to_string()),
            Self::Fenced(_) => MatrixError::custom(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                MatrixErrorCode::Unknown,
                self.to_string(),
            ),
            Self::StillJoined(msg) => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::Unknown,
                msg.clone(),
            ),
            Self::InvalidPaginationToken => MatrixError::custom(
                axum::http::StatusCode::BAD_REQUEST,
                MatrixErrorCode::InvalidParam,
                "invalid pagination token",
            ),
            Self::MissingAncestors(_) => MatrixError::custom(
                axum::http::StatusCode::CONFLICT,
                MatrixErrorCode::Other("M_MISSING_PREV_EVENTS".to_owned()),
                self.to_string(),
            ),
            Self::RemoteJoinFailed(_) | Self::BackfillFailed(_) => MatrixError::custom(
                axum::http::StatusCode::BAD_GATEWAY,
                MatrixErrorCode::Unknown,
                self.to_string(),
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
