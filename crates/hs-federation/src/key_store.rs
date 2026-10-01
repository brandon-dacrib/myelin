//! Where [`crate::keys::RemoteKeyCache`] keeps the key responses it has accepted, so that the
//! notary (`/_matrix/key/v2/query`, `crate::transport::key_server`) and the verification of
//! inbound requests and events survive a restart.
//!
//! Until 2026-10-01 they were held in memory only: after a restart this server's notary had
//! nothing to answer for a server that was down (the spec's notary returns the last keys it holds
//! for one, expired ones included), and every remote server's keys were fetched again. Now each
//! response accepted is written under every key it lists, `(server, key id) -> response`, one row
//! per key overwritten by the next response that lists it, so the store holds at most one
//! response per key ever seen; on boot the cache is rebuilt from it (every response verified
//! again, oldest first), and a response that expired more than [`MAX_HELD_AGE_MS`] ago is
//! forgotten rather than kept for ever.

use std::collections::HashMap;
use std::sync::Mutex;

use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_tables::TableError;
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};

/// How long after it expired a held response is still kept: a year. Long enough that a notary
/// can still vouch for a server that has been gone a while; short enough that the keys of
/// servers long gone do not accumulate for ever.
pub const MAX_HELD_AGE_MS: u64 = 365 * 24 * 60 * 60 * 1000;

/// One key response held for one key it lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeldKeyResponse {
    /// The server the response is from (and about).
    pub server_name: String,
    /// The key it is held under: one of its `verify_keys`.
    pub key_id: String,
    /// The response's own `valid_until_ts`.
    pub valid_until_ts: u64,
    /// The response, exactly as the server published it, signatures and all.
    pub doc: serde_json::Value,
}

/// Durable storage for [`HeldKeyResponse`]s. Every method is best effort: a failure is logged by
/// the implementation and never fails the verification or the notary answer that caused it.
pub trait HeldKeyStore: Send + Sync {
    /// Every response held.
    fn load(&self) -> Vec<HeldKeyResponse>;
    /// Holds `response` under its `(server_name, key_id)`, replacing what was held there.
    fn hold(&self, response: &HeldKeyResponse);
    /// Forgets what is held under `(server_name, key_id)`.
    fn forget(&self, server_name: &str, key_id: &str);
}

/// An in-memory [`HeldKeyStore`], for tests.
#[derive(Default)]
pub struct InMemoryHeldKeyStore {
    held: Mutex<HashMap<(String, String), HeldKeyResponse>>,
}

impl HeldKeyStore for InMemoryHeldKeyStore {
    fn load(&self) -> Vec<HeldKeyResponse> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn hold(&self, response: &HeldKeyResponse) {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (response.server_name.clone(), response.key_id.clone()),
                response.clone(),
            );
    }

    fn forget(&self, server_name: &str, key_id: &str) {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(server_name.to_owned(), key_id.to_owned()));
    }
}

/// The `hs-kv` [`HeldKeyStore`]: keyspace `hs_federation.held_key_responses`, keyed by
/// `(server_name, key_id)`, the value the JSON of a [`HeldKeyResponse`]. In a cluster every
/// replica reads and writes the same rows, so whichever replica fetched a key, every replica's
/// next boot starts with it.
pub struct KvHeldKeyStore<B: KvBackend> {
    backend: B,
    table: TypedKeyspace<B::Keyspace, (String, String)>,
}

impl<B: KvBackend> KvHeldKeyStore<B> {
    /// Opens (creating if necessary) the `hs_federation.held_key_responses` keyspace.
    ///
    /// # Errors
    /// Returns the backend's error if the keyspace cannot be opened.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        let table = TypedKeyspace::new(backend.keyspace("hs_federation.held_key_responses")?);
        Ok(Self { backend, table })
    }
}

impl<B: KvBackend> HeldKeyStore for KvHeldKeyStore<B> {
    fn load(&self) -> Vec<HeldKeyResponse> {
        let snapshot = self.backend.snapshot();
        let mut held = Vec::new();
        for item in self.table.range(&snapshot, RangeSpec::full()) {
            match item {
                Ok((_, bytes)) => match serde_json::from_slice::<HeldKeyResponse>(&bytes) {
                    Ok(response) => held.push(response),
                    Err(error) => {
                        tracing::warn!(%error, "skipping a held key response that does not decode");
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "could not read the held key responses");
                    break;
                }
            }
        }
        held
    }

    fn hold(&self, response: &HeldKeyResponse) {
        let key = (response.server_name.clone(), response.key_id.clone());
        let Ok(bytes) = serde_json::to_vec(response) else {
            return;
        };
        if let Err(error) = transact(&self.backend, TransactConfig::default(), |txn| {
            self.table.put(txn, &key, &bytes).map_err(to_kv_err)
        }) {
            tracing::warn!(
                server = %response.server_name,
                key_id = %response.key_id,
                %error,
                "could not keep a key response; it is held in memory only"
            );
        }
    }

    fn forget(&self, server_name: &str, key_id: &str) {
        let key = (server_name.to_owned(), key_id.to_owned());
        if let Err(error) = transact(&self.backend, TransactConfig::default(), |txn| {
            self.table.delete(txn, &key).map_err(to_kv_err)
        }) {
            tracing::warn!(server = server_name, key_id, %error, "could not forget a held key response");
        }
    }
}

fn to_kv_err(e: TableError) -> hs_kv::KvError {
    match e {
        TableError::Kv(kv) => kv,
        other => hs_kv::KvError::backend(DecodeError(other.to_string())),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct DecodeError(String);
