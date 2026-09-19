//! An `hs-kv`/`hs-tables`-backed [`super::CountsStore`].

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::{OwnedEventId, RoomId, UserId};

use super::{Counts, CountsStore, RoomNotificationCounts, Scope};
use crate::error::StoreError;

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.counts` | `(user_id, room_id, thread_key)` — `thread_key` is `""` for the room's main timeline, else a thread root event ID | `Counts`, as two little-endian `u64`s |
///
/// Grouping the key `(user_id, room_id, thread_key)` in that order (rather than, say,
/// `(room_id, user_id, ...)`) is what makes [`TablesCountsStore::get_room_counts`] a single
/// prefix scan on `(user_id, room_id)` — `hs-tables`' order-preserving tuple encoding guarantees
/// every row for one user's one room sorts contiguously.
pub struct TablesCountsStore<B: KvBackend> {
    backend: B,
    counts: TypedKeyspace<B::Keyspace, (String, String, String)>,
}

fn encode(counts: Counts) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&counts.notification_count.to_le_bytes());
    buf.extend_from_slice(&counts.highlight_count.to_le_bytes());
    buf
}

fn decode(bytes: &[u8]) -> Result<Counts, StoreError> {
    if bytes.len() != 16 {
        return Err(StoreError::Backend(format!(
            "counts row has unexpected length {}",
            bytes.len()
        )));
    }
    let notification_count = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let highlight_count = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    Ok(Counts {
        notification_count,
        highlight_count,
    })
}

impl<B: KvBackend> TablesCountsStore<B> {
    /// Opens (creating if necessary) this store's keyspace.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let counts = TypedKeyspace::new(
            backend
                .keyspace("hs_push.counts")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        Ok(Self { backend, counts })
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> CountsStore for TablesCountsStore<B> {
    async fn get_room_counts(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<RoomNotificationCounts, StoreError> {
        let snap = self.backend.snapshot();
        let prefix = (user_id.to_string(), room_id.to_string());
        let spec = TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&prefix);
        let mut out = RoomNotificationCounts::default();
        for item in self.counts.range(&snap, spec) {
            let ((_, _, thread_key), bytes) = item?;
            let counts = decode(&bytes)?;
            if thread_key.is_empty() {
                out.main = counts;
            } else {
                let root: OwnedEventId = thread_key.as_str().try_into().map_err(|e| {
                    StoreError::Backend(format!("stored thread key is not an event id: {e}"))
                })?;
                out.threads.insert(root, counts);
            }
        }
        Ok(out)
    }

    async fn record_notification(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
        highlight: bool,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), room_id.to_string(), scope.key_part());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let current = self
                .counts
                .get(txn, &key)
                .map_err(hs_kv::KvError::backend)?
                .map(|b| decode(&b))
                .transpose()
                .map_err(hs_kv::KvError::backend)?
                .unwrap_or_default();
            let updated = Counts {
                notification_count: current.notification_count + 1,
                highlight_count: current.highlight_count + u64::from(highlight),
            };
            self.counts
                .put(txn, &key, &encode(updated))
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }

    async fn reset(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), room_id.to_string(), scope.key_part());
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.counts
                .delete(txn, &key)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    #[tokio::test]
    async fn satisfies_the_shared_counts_store_contract() {
        let store = TablesCountsStore::open(MemoryBackend::new()).unwrap();
        crate::counts::contract_tests::behaves_correctly(&store).await;
    }
}
