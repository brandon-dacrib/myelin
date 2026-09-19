//! An `hs-kv`/`hs-tables`-backed [`super::NotificationLogStore`].

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::push::Action;
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, UserId};
use serde::{Deserialize, Serialize};

use super::{NotificationEntry, NotificationLogStore};
use crate::error::StoreError;

#[derive(Serialize, Deserialize)]
struct Row {
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    actions: Vec<Action>,
    ts_ms: u64,
    read: bool,
}

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.notification_log` | `(user_id, seq)` | JSON-encoded [`Row`] |
/// | `hs_push.notification_log_seq` | `(user_id,)` | the next `seq` to assign, as 8 little-endian bytes |
///
/// `seq` sorts numerically because `hs-tables`' `u64` key encoding is order-preserving (the same
/// property `hs_room`'s `room_pos` timeline index relies on), so a range scan from `(user_id,
/// after+1)` to `(user_id, u64::MAX)` yields entries oldest-first with no secondary sort needed.
pub struct TablesNotificationLogStore<B: KvBackend> {
    backend: B,
    log: TypedKeyspace<B::Keyspace, (String, u64)>,
    seq: TypedKeyspace<B::Keyspace, (String,)>,
}

impl<B: KvBackend> TablesNotificationLogStore<B> {
    /// Opens (creating if necessary) this store's keyspaces.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if a keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let log = TypedKeyspace::new(
            backend
                .keyspace("hs_push.notification_log")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        let seq = TypedKeyspace::new(
            backend
                .keyspace("hs_push.notification_log_seq")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        Ok(Self { backend, log, seq })
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> NotificationLogStore for TablesNotificationLogStore<B> {
    async fn append(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event_id: &EventId,
        actions: Vec<Action>,
        ts_ms: u64,
    ) -> Result<u64, StoreError> {
        let row = Row {
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            actions,
            ts_ms,
            read: false,
        };
        let value = serde_json::to_vec(&row)
            .map_err(|e| StoreError::Backend(format!("encode notification: {e}")))?;
        let seq_key = (user_id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let current = self
                .seq
                .get(txn, &seq_key)
                .map_err(hs_kv::KvError::backend)?
                .map(|b| u64::from_le_bytes(b.as_ref().try_into().unwrap_or_default()))
                .unwrap_or(0);
            let next = current + 1;
            self.seq
                .put(txn, &seq_key, &next.to_le_bytes())
                .map_err(hs_kv::KvError::backend)?;
            self.log
                .put(txn, &(user_id.to_string(), next), &value)
                .map_err(hs_kv::KvError::backend)?;
            Ok(next)
        })
        .map_err(StoreError::from)
    }

    async fn page(
        &self,
        user_id: &UserId,
        after: Option<u64>,
        limit: usize,
        only_highlight: bool,
    ) -> Result<Vec<NotificationEntry>, StoreError> {
        // A prefix scan over every entry for this user, filtered and capped in memory. Simple and
        // correct; not push-down-optimal for a user with a very long unread history (it re-walks
        // from the start of their log every page rather than seeking directly to `after`) --
        // acceptable for now, recorded in `docs/status/10-push.md` as a follow-up if a real
        // deployment's `/notifications` usage shows it matters.
        let snap = self.backend.snapshot();
        let floor = after.unwrap_or(0);
        let prefix = (user_id.to_string(),);
        let spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&prefix);
        let mut out = Vec::new();
        for item in self.log.range(&snap, spec) {
            let ((_, seq), bytes) = item?;
            if seq <= floor {
                continue;
            }
            let row: Row = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Backend(format!("decode notification: {e}")))?;
            if only_highlight && !row.actions.iter().any(Action::is_highlight) {
                continue;
            }
            out.push(NotificationEntry {
                seq,
                room_id: row.room_id,
                event_id: row.event_id,
                actions: row.actions,
                ts_ms: row.ts_ms,
                read: row.read,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    #[tokio::test]
    async fn satisfies_the_shared_notification_log_contract() {
        let store = TablesNotificationLogStore::open(MemoryBackend::new()).unwrap();
        crate::notification_log::contract_tests::behaves_correctly(&store).await;
    }
}
