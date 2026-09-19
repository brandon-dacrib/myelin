//! An in-process cache for the `Idempotency-Key` header (RFC 0004 section 3 / the OpenAPI
//! `IdempotencyKey` parameter, declared on every mutating `POST` in `openapi/openapi.yaml`): a
//! client that retries a mutating request with the same key gets back the exact response the
//! first attempt produced, instead of the mutation running twice.
//!
//! Scoped to this process only: a real cluster deployment would need a shared store (`hs-tables`,
//! most likely), which is out of scope for this crate (see
//! `docs/status/15-admin-api-and-modules.md` "Decisions made"). Entries expire 24 hours after they
//! are recorded, matching the OpenAPI parameter's description ("Replays return the stored response
//! for 24 hours").
//!
//! Only successful (2xx) responses are cached; a request that failed validation, hit a `503`, or
//! any other non-2xx path is never stored, so retrying after a failure always re-attempts the
//! mutation rather than replaying a stale error.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a recorded response is replayed for before a reused key is treated as fresh again.
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A previously-produced response, cached verbatim so a replay can reconstruct it exactly.
#[derive(Debug, Clone)]
pub struct StoredResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

struct Entry {
    body_hash: u64,
    response: StoredResponse,
    recorded_at: Instant,
}

/// What checking an `Idempotency-Key` against the store produced.
#[derive(Debug)]
pub enum Replay {
    /// No live entry for this key: the caller should run the mutation and, if it succeeds, call
    /// [`IdempotencyStore::record`].
    Fresh,
    /// The same key was used before with an identical request body: return this stored response
    /// verbatim rather than repeating the mutation.
    Same(StoredResponse),
    /// The same key was used before with a *different* request body
    /// (`urn:hs:problem:idempotency-key-payload-mismatch`).
    Mismatch,
}

fn hash_body(body: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

/// The key -> response cache. One instance lives on [`crate::router::AdminState`], shared by
/// every mutating handler that declares `Idempotency-Key`.
#[derive(Default)]
pub struct IdempotencyStore {
    entries: Mutex<HashMap<String, Entry>>,
}

impl IdempotencyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// `scope` should be the operation id (`"users.lock"`, ...): the same `Idempotency-Key` value
    /// reused on a *different* operation is treated as a fresh key rather than a collision, since
    /// RFC 0004 does not require a single global key namespace and per-operation scoping avoids a
    /// client's key for one action accidentally shadowing another's.
    pub fn check(&self, scope: &str, key: &str, request_body: &[u8]) -> Replay {
        let mut entries = self.entries.lock().expect("idempotency store poisoned");
        let full_key = format!("{scope}:{key}");
        let Some(entry) = entries.get(&full_key) else {
            return Replay::Fresh;
        };
        if entry.recorded_at.elapsed() > TTL {
            entries.remove(&full_key);
            return Replay::Fresh;
        }
        if entry.body_hash == hash_body(request_body) {
            Replay::Same(entry.response.clone())
        } else {
            Replay::Mismatch
        }
    }

    /// Records a successful response under `scope`/`key`, replacing any prior entry.
    pub fn record(&self, scope: &str, key: &str, request_body: &[u8], response: StoredResponse) {
        let mut entries = self.entries.lock().expect("idempotency store poisoned");
        entries.insert(
            format!("{scope}:{key}"),
            Entry {
                body_hash: hash_body(request_body),
                response,
                recorded_at: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_response(body: &[u8]) -> StoredResponse {
        StoredResponse {
            status: 200,
            content_type: "application/json".to_string(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn unknown_key_is_fresh() {
        let store = IdempotencyStore::new();
        assert!(matches!(
            store.check("users.lock", "k1", b"{}"),
            Replay::Fresh
        ));
    }

    #[test]
    fn same_body_replays_the_stored_response() {
        let store = IdempotencyStore::new();
        store.record("users.lock", "k1", b"{}", ok_response(b"{\"ok\":true}"));
        match store.check("users.lock", "k1", b"{}") {
            Replay::Same(stored) => assert_eq!(stored.body, b"{\"ok\":true}"),
            other => panic!("expected Same, got {other:?}"),
        }
    }

    #[test]
    fn different_body_under_the_same_key_is_a_mismatch() {
        let store = IdempotencyStore::new();
        store.record("users.lock", "k1", b"{}", ok_response(b"{}"));
        assert!(matches!(
            store.check("users.lock", "k1", b"{\"reason\":\"spam\"}"),
            Replay::Mismatch
        ));
    }

    #[test]
    fn the_same_key_on_a_different_operation_is_fresh() {
        let store = IdempotencyStore::new();
        store.record("users.lock", "k1", b"{}", ok_response(b"{}"));
        assert!(matches!(
            store.check("users.unlock", "k1", b"{}"),
            Replay::Fresh
        ));
    }

    #[test]
    fn an_expired_entry_is_treated_as_fresh() {
        let store = IdempotencyStore::new();
        store.entries.lock().unwrap().insert(
            "users.lock:k1".to_string(),
            Entry {
                body_hash: hash_body(b"{}"),
                response: ok_response(b"{}"),
                recorded_at: Instant::now() - Duration::from_secs(25 * 60 * 60),
            },
        );
        assert!(matches!(
            store.check("users.lock", "k1", b"{}"),
            Replay::Fresh
        ));
    }
}
