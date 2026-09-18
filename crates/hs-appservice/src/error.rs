//! Errors from the registry, scheduler and their HTTP surfaces.

use hs_tables::TableError;

use crate::regexp::NamespacePatternError;
use crate::registration::RegistrationError;

/// Everything that can go wrong operating the appservice registry or scheduler.
#[derive(Debug, thiserror::Error)]
pub enum AppserviceError {
    /// The underlying store failed.
    #[error("store error: {0}")]
    Store(String),

    /// A stored row could not be decoded (a schema mismatch, or corruption).
    #[error("failed to decode a stored row: {0}")]
    Decode(String),

    /// No appservice with this id is registered.
    #[error("no appservice registered with id {0:?}")]
    NotFound(String),

    /// An appservice with this id already exists.
    #[error("an appservice with id {0:?} is already registered")]
    AlreadyExists(String),

    /// `as_token` or `hs_token` collides with a different, already-registered appservice.
    #[error("{token_kind} is already used by appservice {existing_id:?}")]
    TokenConflict {
        /// `"as_token"` or `"hs_token"`.
        token_kind: &'static str,
        /// The id of the appservice that already owns this token.
        existing_id: String,
    },

    /// An exclusive namespace, or a sender user id, collides with another registered appservice.
    #[error(
        "namespace conflict: {new_id:?}'s {kind} {value:?} is already exclusively claimed by {existing_id:?}"
    )]
    NamespaceConflict {
        /// The appservice being added or updated.
        new_id: String,
        /// `"sender"`, `"users"`, `"aliases"` or `"rooms"`.
        kind: &'static str,
        /// The colliding value (a user id, alias, or room id/pattern description).
        value: String,
        /// The appservice that already exclusively owns it.
        existing_id: String,
    },

    /// A registration file failed to parse.
    #[error(transparent)]
    Registration(#[from] RegistrationError),

    /// A namespace pattern in an update request failed to compile.
    #[error(transparent)]
    Namespace(#[from] NamespacePatternError),

    /// An appservice with `url: null` was asked to receive a push, which `PLAN.md` section 8.2
    /// forbids ("`url: null` registrations ... are never pushed to").
    #[error("appservice {0:?} has no url and must never be pushed to")]
    NoUrl(String),
}

impl From<hs_kv::KvError> for AppserviceError {
    fn from(e: hs_kv::KvError) -> Self {
        Self::Store(e.to_string())
    }
}

impl From<TableError> for AppserviceError {
    fn from(e: TableError) -> Self {
        Self::Store(e.to_string())
    }
}

impl From<hs_tables::IndexError> for AppserviceError {
    fn from(e: hs_tables::IndexError) -> Self {
        match e {
            hs_tables::IndexError::UniqueConflict => Self::Store(
                "unique index violated (should have been checked explicitly first)".to_string(),
            ),
            other => Self::Store(other.to_string()),
        }
    }
}
