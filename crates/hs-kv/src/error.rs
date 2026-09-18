//! Error types for `hs-kv`.

use std::fmt;

/// An error from a backend or from the `hs-kv` layer itself.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// The key exceeds [`crate::MAX_KEY_BYTES`].
    #[error("key of {len} bytes exceeds the {limit}-byte limit")]
    KeyTooLarge {
        /// The offending key's length.
        len: usize,
        /// The configured limit.
        limit: usize,
    },

    /// The value exceeds [`crate::MAX_VALUE_BYTES`].
    #[error("value of {len} bytes exceeds the {limit}-byte limit")]
    ValueTooLarge {
        /// The offending value's length.
        len: usize,
        /// The configured limit.
        limit: usize,
    },

    /// The transaction accumulated more mutations, or more mutation bytes, than
    /// [`crate::MAX_TXN_MUTATIONS`] / [`crate::MAX_TXN_BYTES`] allow. Transactions must be kept
    /// short: split the work into multiple transactions instead of growing this one.
    #[error("transaction too large: {reason}")]
    TransactionTooLarge {
        /// Human-readable reason (which limit was hit).
        reason: String,
    },

    /// The named keyspace does not exist and the backend was asked not to create it.
    #[error("keyspace {0:?} does not exist")]
    NoSuchKeyspace(String),

    /// A keyspace name was invalid (empty, or too long for the backend).
    #[error("invalid keyspace name {0:?}")]
    InvalidKeyspaceName(String),

    /// The backend's storage engine reported an I/O or encoding error.
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// A closure passed to [`crate::transact`] asked for the transaction to be aborted, without
    /// retrying, carrying an application-level error.
    #[error("transaction aborted: {0}")]
    Aborted(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// [`crate::transact`] gave up after exhausting its retry budget because every attempt
    /// conflicted with a concurrent transaction.
    #[error(
        "transaction retry budget ({attempts} attempts) exhausted, last conflict was on commit"
    )]
    RetriesExhausted {
        /// How many attempts were made.
        attempts: u32,
    },
}

impl KvError {
    /// Wraps an arbitrary backend error.
    pub fn backend(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend(Box::new(err))
    }
}

/// The outcome of asking a write transaction to commit: either it succeeded, or it lost a
/// serializability race with a concurrent transaction and must be retried from scratch.
///
/// This is distinct from [`KvError`]: a conflict is an expected, routine outcome of optimistic
/// concurrency control, not a failure of the store. [`crate::transact`] handles it by retrying;
/// callers using [`crate::KvBackend::begin`] directly must check for it themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict;

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "transaction conflict: a concurrent transaction committed first"
        )
    }
}

impl std::error::Error for Conflict {}
