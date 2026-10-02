//! Per-room cursors: how far into each room's timeline the push pipeline has evaluated, so an
//! event announced twice (the room stream re-announces a room's newest event whenever the room
//! is loaded, and after every restart) is evaluated once.

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::RoomId;

use crate::error::StoreError;

/// Persistence for the pipeline's per-room position.
#[async_trait::async_trait]
pub trait CursorStore: Send + Sync {
    /// The last `room_pos` evaluated for `room_id`, or `None` if the pipeline has never seen the
    /// room.
    async fn get(&self, room_id: &RoomId) -> Result<Option<i64>, StoreError>;

    /// Records that everything up to `room_pos` has been evaluated.
    async fn set(&self, room_id: &RoomId, room_pos: i64) -> Result<(), StoreError>;
}

/// An in-memory [`CursorStore`], for tests.
#[derive(Debug, Default)]
pub struct InMemoryCursorStore {
    rows: std::sync::RwLock<std::collections::HashMap<ruma::OwnedRoomId, i64>>,
}

impl InMemoryCursorStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl CursorStore for InMemoryCursorStore {
    async fn get(&self, room_id: &RoomId) -> Result<Option<i64>, StoreError> {
        Ok(self.rows.read().unwrap().get(room_id).copied())
    }

    async fn set(&self, room_id: &RoomId, room_pos: i64) -> Result<(), StoreError> {
        self.rows
            .write()
            .unwrap()
            .insert(room_id.to_owned(), room_pos);
        Ok(())
    }
}

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.room_cursor` | `(room_id,)` | the last evaluated `room_pos`, as 8 little-endian bytes |
pub struct TablesCursorStore<B: KvBackend> {
    backend: B,
    cursors: TypedKeyspace<B::Keyspace, (String,)>,
}

impl<B: KvBackend> TablesCursorStore<B> {
    /// Opens (creating if necessary) this store's keyspace.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let cursors = TypedKeyspace::new(
            backend
                .keyspace("hs_push.room_cursor")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        Ok(Self { backend, cursors })
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> CursorStore for TablesCursorStore<B> {
    async fn get(&self, room_id: &RoomId) -> Result<Option<i64>, StoreError> {
        let snap = self.backend.snapshot();
        Ok(self
            .cursors
            .get(&snap, &(room_id.to_string(),))?
            .map(|b| i64::from_le_bytes(b.as_ref().try_into().unwrap_or_default())))
    }

    async fn set(&self, room_id: &RoomId, room_pos: i64) -> Result<(), StoreError> {
        let key = (room_id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.cursors
                .put(txn, &key, &room_pos.to_le_bytes())
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    async fn behaves(store: &dyn CursorStore) {
        let room = ruma::room_id!("!room:example.org");
        assert_eq!(store.get(room).await.unwrap(), None);
        store.set(room, 7).await.unwrap();
        assert_eq!(store.get(room).await.unwrap(), Some(7));
        store.set(room, 9).await.unwrap();
        assert_eq!(store.get(room).await.unwrap(), Some(9));
    }

    #[tokio::test]
    async fn memory_store_keeps_the_position() {
        behaves(&InMemoryCursorStore::new()).await;
    }

    #[tokio::test]
    async fn tables_store_keeps_the_position() {
        behaves(&TablesCursorStore::open(MemoryBackend::new()).unwrap()).await;
    }
}
