//! The crate-wide error type.

use thiserror::Error;

/// Errors from anything in `hs-federation`.
#[derive(Debug, Error)]
pub enum FederationError {
    #[error("server discovery failed for {server_name}: {reason}")]
    Discovery { server_name: String, reason: String },

    #[error("key error: {0}")]
    Key(String),

    #[error("request signature invalid: {0}")]
    InvalidSignature(String),

    #[error("request rejected: {0}")]
    Rejected(String),

    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("resource limit exceeded: {0}")]
    LimitExceeded(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http client error: {0}")]
    Http(String),
}
