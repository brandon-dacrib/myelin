//! Per-room email throttle state: when the last notification email about a room went to an
//! address, and how long the next one must wait. Persisted so a restart does not re-mail a
//! room that was mailed a minute ago.

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::{RoomId, UserId};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// One room's throttle state for one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleState {
    /// When the last email about the room was sent, in milliseconds since the epoch.
    pub last_sent_ms: u64,
    /// How long after `last_sent_ms` the next email about the room may go.
    pub throttle_ms: u64,
}

/// Persistence for [`ThrottleState`], keyed by `(user, room, address)`.
#[async_trait::async_trait]
pub trait ThrottleStore: Send + Sync {
    /// The state for `(user_id, room_id, address)`, or `None` if no email about the room has
    /// gone to that address (or the room was read since).
    async fn get(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        address: &str,
    ) -> Result<Option<ThrottleState>, StoreError>;

    /// Records that an email about the room went to `address`.
    async fn set(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        address: &str,
        state: ThrottleState,
    ) -> Result<(), StoreError>;

    /// Forgets the room for every address of `user_id`: what reading the room does.
    async fn reset_room(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError>;
}

/// An in-memory [`ThrottleStore`], for tests.
#[derive(Debug, Default)]
pub struct InMemoryThrottleStore {
    rows: std::sync::RwLock<std::collections::BTreeMap<(String, String, String), ThrottleState>>,
}

impl InMemoryThrottleStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn key(user_id: &UserId, room_id: &RoomId, address: &str) -> (String, String, String) {
    (
        user_id.to_string(),
        room_id.to_string(),
        address.to_ascii_lowercase(),
    )
}

#[async_trait::async_trait]
impl ThrottleStore for InMemoryThrottleStore {
    async fn get(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        address: &str,
    ) -> Result<Option<ThrottleState>, StoreError> {
        Ok(self
            .rows
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key(user_id, room_id, address))
            .copied())
    }

    async fn set(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        address: &str,
        state: ThrottleState,
    ) -> Result<(), StoreError> {
        self.rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key(user_id, room_id, address), state);
        Ok(())
    }

    async fn reset_room(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError> {
        let user = user_id.to_string();
        let room = room_id.to_string();
        self.rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(u, r, _), _| !(*u == user && *r == room));
        Ok(())
    }
}

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.email_throttle` | `(user_id, room_id, address)` | the [`ThrottleState`], JSON-encoded |
pub struct TablesThrottleStore<B: KvBackend> {
    backend: B,
    rows: TypedKeyspace<B::Keyspace, (String, String, String)>,
}

impl<B: KvBackend> TablesThrottleStore<B> {
    /// Opens (creating if necessary) this store's keyspace.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let rows = TypedKeyspace::new(
            backend
                .keyspace("hs_push.email_throttle")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        Ok(Self { backend, rows })
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> ThrottleStore for TablesThrottleStore<B> {
    async fn get(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        address: &str,
    ) -> Result<Option<ThrottleState>, StoreError> {
        let snap = self.backend.snapshot();
        self.rows
            .get(&snap, &key(user_id, room_id, address))?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|e| StoreError::Backend(format!("decode throttle state: {e}")))
            })
            .transpose()
    }

    async fn set(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        address: &str,
        state: ThrottleState,
    ) -> Result<(), StoreError> {
        let k = key(user_id, room_id, address);
        let value = serde_json::to_vec(&state)
            .map_err(|e| StoreError::Backend(format!("encode throttle state: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.rows
                .put(txn, &k, &value)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }

    async fn reset_room(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError> {
        let snap = self.backend.snapshot();
        let prefix = (user_id.to_string(), room_id.to_string());
        let spec = TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&prefix);
        let mut doomed = Vec::new();
        for item in self.rows.range(&snap, spec) {
            let (k, _) = item?;
            doomed.push(k);
        }
        if doomed.is_empty() {
            return Ok(());
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            for k in &doomed {
                self.rows.delete(txn, k).map_err(hs_kv::KvError::backend)?;
            }
            Ok(())
        })
        .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    async fn behaves(store: &dyn ThrottleStore) {
        let alice = ruma::user_id!("@alice:example.org");
        let room = ruma::room_id!("!room:example.org");
        let other = ruma::room_id!("!other:example.org");
        assert_eq!(store.get(alice, room, "a@x.org").await.unwrap(), None);
        let state = ThrottleState {
            last_sent_ms: 1_000,
            throttle_ms: 600_000,
        };
        store.set(alice, room, "A@x.org", state).await.unwrap();
        store.set(alice, other, "a@x.org", state).await.unwrap();
        assert_eq!(
            store.get(alice, room, "a@x.org").await.unwrap(),
            Some(state),
            "addresses compare case-insensitively"
        );
        store.reset_room(alice, room).await.unwrap();
        assert_eq!(store.get(alice, room, "a@x.org").await.unwrap(), None);
        assert_eq!(
            store.get(alice, other, "a@x.org").await.unwrap(),
            Some(state),
            "another room is untouched"
        );
        store.reset_room(alice, other).await.unwrap();
        store.reset_room(alice, other).await.unwrap();
    }

    #[tokio::test]
    async fn memory_store_keeps_the_state() {
        behaves(&InMemoryThrottleStore::new()).await;
    }

    #[tokio::test]
    async fn tables_store_keeps_the_state() {
        behaves(&TablesThrottleStore::open(MemoryBackend::new()).unwrap()).await;
    }
}
