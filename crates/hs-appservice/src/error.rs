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

/// `error` with every cause in its source chain, `outer: inner: innermost`, each said once.
///
/// An HTTP client's error says what it was doing and keeps why in its sources: reqwest's
/// `Display` for a request that timed out is `error sending request for url (...)`, and the
/// `operation timed out` (or `connection refused`, or `dns error: ...`) is a source or two down.
/// That top line alone was all the delivery log and an appservice's `last_error` said on the
/// demo on 2026-10-12, when five bridges stopped taking transactions and nothing said why. A
/// cause whose text is already in what has been said (hyper and reqwest sometimes repeat one)
/// is left out.
#[must_use]
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut current = error.source();
    while let Some(cause) = current {
        let text = cause.to_string();
        if !text.is_empty() && !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        current = cause.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::error_chain;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|l| l as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn every_cause_is_said_once_outermost_first() {
        let error = Layer(
            "error sending request for url (http://bridge:29318/)",
            Some(Box::new(Layer(
                "client error (SendRequest)",
                Some(Box::new(Layer(
                    "operation timed out",
                    Some(Box::new(Layer("operation timed out", None))),
                ))),
            ))),
        );
        assert_eq!(
            error_chain(&error),
            "error sending request for url (http://bridge:29318/): client error (SendRequest): operation timed out"
        );
        assert_eq!(error_chain(&Layer("alone", None)), "alone");
    }

    #[tokio::test]
    async fn a_refused_connection_says_it_was_refused() {
        // A port that was bound and let go: nothing listens there now.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let error = reqwest::Client::new()
            .put(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        let chained = error_chain(&error);
        assert!(
            chained.to_lowercase().contains("refused"),
            "{chained} (plain: {error})"
        );
    }
}
