//! Error types for this crate.

use thiserror::Error;

/// Errors from canonical JSON encoding and validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CanonicalJsonError {
    /// A number is a float that is not an integral value (strict mode only).
    #[error("float value {0} is not allowed in canonical JSON")]
    Float(String),
    /// An integer lies outside `[-(2^53)+1, 2^53-1]` (strict mode only).
    #[error("integer {0} is outside the canonical JSON range")]
    IntegerOutOfRange(String),
    /// A number is not finite.
    #[error("non-finite number")]
    NonFinite,
}

/// Errors from the redaction algorithm.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RedactionError {
    /// The event has no `type` field, or it is not a string.
    #[error("event has no string `type` field")]
    MissingType,
    /// The event's `content` is present but is not an object.
    #[error("event `content` is not an object")]
    ContentNotObject,
}

/// Errors from parsing `m.room.power_levels` content.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PowerLevelsError {
    /// `content` is not a JSON object.
    #[error("power levels content is not an object")]
    NotObject,
    /// A field that must be an integer (or, in lenient versions, an integer string or float)
    /// holds something else.
    #[error("power level field `{field}` is not an integer: {value}")]
    NotInteger {
        /// Dotted path of the offending field.
        field: String,
        /// The offending value, rendered as JSON.
        value: String,
    },
    /// A map field (`users`, `events`, `notifications`) is not an object.
    #[error("power level field `{0}` is not an object")]
    NotMap(String),
    /// A key of `users` is not a valid user ID.
    #[error("power level `users` key `{0}` is not a valid user ID")]
    InvalidUserId(String),
}

/// Errors from parsing an event out of JSON.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EventError {
    /// The value is not a JSON object.
    #[error("event is not a JSON object")]
    NotObject,
    /// A required field is missing.
    #[error("event is missing required field `{0}`")]
    MissingField(&'static str),
    /// A field has the wrong JSON type.
    #[error("event field `{0}` has the wrong type")]
    WrongType(&'static str),
    /// An identifier failed to parse.
    #[error("event field `{field}` is not a valid identifier: {reason}")]
    InvalidId {
        /// Which field.
        field: &'static str,
        /// The parse error from Ruma.
        reason: String,
    },
    /// A field exceeds its size limit.
    #[error("event field `{field}` exceeds {limit} bytes")]
    TooLarge {
        /// Which field.
        field: &'static str,
        /// The limit in bytes.
        limit: usize,
    },
    /// The room version's format rules forbid or require a field.
    #[error("event violates the room version's event format: {0}")]
    Format(String),
    /// Canonical JSON encoding failed.
    #[error(transparent)]
    Canonical(#[from] CanonicalJsonError),
    /// Redaction failed while computing a hash.
    #[error(transparent)]
    Redaction(#[from] RedactionError),
    /// The room version is not known to this server.
    #[error("unknown room version `{0}`")]
    UnknownRoomVersion(String),
}
