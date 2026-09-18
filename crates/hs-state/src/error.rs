//! Error types for this crate.

use thiserror::Error;

/// An event failed the authorization rules, or the inputs needed to check it were malformed.
///
/// This is not a Rust-level error in the usual sense: a rejection is an expected, common outcome
/// of authorizing an event (most events sent by a misbehaving or lagging server *should* be
/// rejected). Callers match on this the way they would match on a boolean, not the way they would
/// handle an I/O failure; it carries a message because operators and client developers debugging
/// "why was my event rejected" need a reason, matching the spec's own convention of describing
/// each rule's rejection condition in prose.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct AuthError(pub String);

impl AuthError {
    /// Builds a rejection with the given message.
    pub fn reject(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// The result of an authorization check: `Ok(())` if the event is allowed, `Err(AuthError)` with
/// the reason otherwise.
pub type AuthResult = Result<(), AuthError>;

/// Errors from state resolution.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateResError {
    /// A state map given to resolution contained more than one event for the same
    /// `(type, state_key)` pair, which is not a valid state map.
    #[error("duplicate entry for ({0:?}, {1:?}) in a single state map")]
    DuplicateStateKey(String, String),
    /// An event referenced by ID was not found in the event store the resolver was given.
    #[error("event {0} not found")]
    MissingEvent(String),
    /// The events being resolved are not all from the same room version, or the room version is
    /// unsupported by the requested algorithm.
    #[error("{0}")]
    UnsupportedInput(String),
    /// An underlying auth check failed while resolving (used to build the iterative auth chain in
    /// v2/v2.1).
    #[error(transparent)]
    Auth(#[from] AuthError),
}
