//! Per-destination retry/backoff state, persisted so a restart resumes rather than immediately
//! retrying every previously-failing destination (`docs/status/06-federation.md` item 7).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::TableError;
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// One destination's retry/backoff bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DestinationState {
    pub failure_count: u32,
    /// Milliseconds since the epoch; `None` means "never attempted" (treated the same as "ready
    /// now" by [`DestinationState::ready_at`]).
    pub retry_at_ms: Option<u64>,
    pub last_success_ms: Option<u64>,
}

impl DestinationState {
    /// Whether a request to this destination should be attempted right now.
    #[must_use]
    pub fn is_ready(&self, now_ms: u64) -> bool {
        self.retry_at_ms.is_none_or(|retry_at| now_ms >= retry_at)
    }

    /// Applies a failed attempt: increments the failure count and computes the next
    /// `retry_at_ms` via exponential backoff (base 1s, doubling, capped at `max_backoff_ms`) with
    /// full jitter (uniformly random in `[0, computed_delay]`) to avoid every caller retrying a
    /// recovering destination in lockstep.
    #[must_use]
    pub fn on_failure(
        mut self,
        now: u64,
        max_backoff_ms: u64,
        jitter: impl Fn(u64) -> u64,
    ) -> Self {
        self.failure_count = self.failure_count.saturating_add(1);
        let base_delay_ms = 1000u64.saturating_mul(1u64 << self.failure_count.min(20));
        let capped = base_delay_ms.min(max_backoff_ms);
        let delay = jitter(capped);
        self.retry_at_ms = Some(now.saturating_add(delay));
        self
    }

    /// Applies a successful attempt: resets the failure count/backoff and records the success
    /// time.
    #[must_use]
    pub fn on_success(mut self, now: u64) -> Self {
        self.failure_count = 0;
        self.retry_at_ms = None;
        self.last_success_ms = Some(now);
        self
    }
}

/// Per-destination retry/backoff state, keyed by destination server name.
#[async_trait]
pub trait DestinationStore: Send + Sync {
    async fn get(&self, destination: &str) -> DestinationState;
    async fn record_failure(&self, destination: &str, max_backoff_ms: u64);
    async fn record_success(&self, destination: &str);
}

/// An in-memory [`DestinationStore`], for tests and for a deployment that accepts losing backoff
/// state across restarts (not the default — see [`KvDestinationStore`]).
#[derive(Default)]
pub struct InMemoryDestinationStore {
    state: Mutex<HashMap<String, DestinationState>>,
}

impl InMemoryDestinationStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DestinationStore for InMemoryDestinationStore {
    async fn get(&self, destination: &str) -> DestinationState {
        self.state
            .lock()
            .unwrap()
            .get(destination)
            .copied()
            .unwrap_or_default()
    }

    async fn record_failure(&self, destination: &str, max_backoff_ms: u64) {
        let mut map = self.state.lock().unwrap();
        let entry = map.entry(destination.to_string()).or_default();
        *entry = entry.on_failure(now_ms(), max_backoff_ms, |cap| {
            if cap == 0 {
                0
            } else {
                rand::random::<u64>() % (cap + 1)
            }
        });
    }

    async fn record_success(&self, destination: &str) {
        let mut map = self.state.lock().unwrap();
        let entry = map.entry(destination.to_string()).or_default();
        *entry = entry.on_success(now_ms());
    }
}

/// A `hs-kv`/`hs-tables`-backed [`DestinationStore`]: a `destinations` typed keyspace keyed by
/// destination server name, so backoff state survives a restart (the plan's explicit
/// requirement).
pub struct KvDestinationStore<B: KvBackend> {
    backend: B,
    table: TypedKeyspace<B::Keyspace, (String,)>,
}

impl<B: KvBackend> KvDestinationStore<B> {
    /// Opens (creating if necessary) the `hs_federation.destinations` keyspace on `backend`.
    ///
    /// # Errors
    /// Returns the backend's error if the keyspace cannot be opened.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        let table = TypedKeyspace::new(backend.keyspace("hs_federation.destinations")?);
        Ok(Self { backend, table })
    }
}

#[async_trait]
impl<B: KvBackend> DestinationStore for KvDestinationStore<B> {
    async fn get(&self, destination: &str) -> DestinationState {
        let snapshot = self.backend.snapshot();
        let key = (destination.to_string(),);
        match self.table.get(&snapshot, &key) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            _ => DestinationState::default(),
        }
    }

    async fn record_failure(&self, destination: &str, max_backoff_ms: u64) {
        let key = (destination.to_string(),);
        let _ = transact(&self.backend, TransactConfig::default(), |txn| {
            let current: DestinationState = self
                .table
                .get(txn, &key)
                .map_err(to_kv_err)?
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let updated = current.on_failure(now_ms(), max_backoff_ms, |cap| {
                if cap == 0 {
                    0
                } else {
                    rand::random::<u64>() % (cap + 1)
                }
            });
            let bytes = serde_json::to_vec(&updated)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            self.table.put(txn, &key, &bytes).map_err(to_kv_err)
        });
    }

    async fn record_success(&self, destination: &str) {
        let key = (destination.to_string(),);
        let _ = transact(&self.backend, TransactConfig::default(), |txn| {
            let current: DestinationState = self
                .table
                .get(txn, &key)
                .map_err(to_kv_err)?
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let updated = current.on_success(now_ms());
            let bytes = serde_json::to_vec(&updated)
                .map_err(|e| hs_kv::KvError::backend(DecodeError(e.to_string())))?;
            self.table.put(txn, &key, &bytes).map_err(to_kv_err)
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    #[test]
    fn is_ready_when_never_attempted() {
        assert!(DestinationState::default().is_ready(now_ms()));
    }

    #[test]
    fn failure_sets_a_future_retry_time_and_increments_count() {
        let s = DestinationState::default().on_failure(1000, 60_000, |cap| cap);
        assert_eq!(s.failure_count, 1);
        assert!(s.retry_at_ms.unwrap() > 1000);
    }

    #[test]
    fn backoff_is_capped() {
        let mut s = DestinationState::default();
        for _ in 0..30 {
            s = s.on_failure(0, 5000, |cap| cap);
        }
        assert_eq!(s.retry_at_ms, Some(5000));
    }

    #[test]
    fn success_resets_failure_state() {
        let s = DestinationState::default()
            .on_failure(1000, 60_000, |cap| cap)
            .on_success(2000);
        assert_eq!(s.failure_count, 0);
        assert_eq!(s.retry_at_ms, None);
        assert_eq!(s.last_success_ms, Some(2000));
    }

    #[tokio::test]
    async fn in_memory_store_round_trips() {
        let store = InMemoryDestinationStore::new();
        assert!(store.get("a.example.org").await.is_ready(now_ms()));
        store.record_failure("a.example.org", 60_000).await;
        let state = store.get("a.example.org").await;
        assert_eq!(state.failure_count, 1);
        store.record_success("a.example.org").await;
        assert_eq!(store.get("a.example.org").await.failure_count, 0);
    }

    #[tokio::test]
    async fn kv_store_persists_across_separate_handles_on_the_same_backend() {
        // "Persisted so a restart resumes": simulate a restart by opening a second store handle
        // over the *same* backend instance (a `MemoryBackend` clone shares the same underlying
        // data, matching how a real `FjallBackend` would reopen the same on-disk keyspace).
        let backend = MemoryBackend::new();
        let store1 = KvDestinationStore::open(backend.clone()).unwrap();
        store1.record_failure("b.example.org", 60_000).await;
        store1.record_failure("b.example.org", 60_000).await;

        let store2 = KvDestinationStore::open(backend).unwrap();
        let state = store2.get("b.example.org").await;
        assert_eq!(state.failure_count, 2);
    }

    #[tokio::test]
    async fn kv_store_unknown_destination_is_ready() {
        let backend = MemoryBackend::new();
        let store = KvDestinationStore::open(backend).unwrap();
        assert!(store.get("never-seen.example.org").await.is_ready(now_ms()));
    }
}
