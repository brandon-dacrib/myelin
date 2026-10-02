//! An `hs-kv`/`hs-tables`-backed [`super::NotificationLogStore`].

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::push::Action;
use ruma::{OwnedEventId, OwnedRoomId, RoomId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{NewNotification, NotificationEntry, NotificationLogStore};
use crate::error::StoreError;

#[derive(Serialize, Deserialize)]
struct Row {
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    #[serde(default)]
    event: Value,
    actions: Vec<Action>,
    #[serde(default)]
    profile_tag: Option<String>,
    ts_ms: u64,
}

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.notification_log` | `(user_id, seq)` | JSON-encoded [`Row`] |
/// | `hs_push.notification_log_seq` | `(user_id,)` | the last `seq` assigned, as 8 little-endian bytes |
/// | `hs_push.notification_read_marks` | `(user_id, room_id)` | the newest `seq` a receipt for the room covered, as 8 little-endian bytes |
///
/// `seq` sorts numerically because `hs-tables`' `u64` key encoding is order-preserving (the same
/// property `hs_room`'s `room_pos` timeline index relies on), so a reverse scan from `(user_id,
/// before)` yields entries newest-first with no secondary sort needed.
pub struct TablesNotificationLogStore<B: KvBackend> {
    backend: B,
    log: TypedKeyspace<B::Keyspace, (String, u64)>,
    seq: TypedKeyspace<B::Keyspace, (String,)>,
    read_marks: TypedKeyspace<B::Keyspace, (String, String)>,
}

fn u64_of(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap_or_default())
}

impl<B: KvBackend> TablesNotificationLogStore<B> {
    /// Opens (creating if necessary) this store's keyspaces.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if a keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let open = |name: &str| {
            backend
                .keyspace(name)
                .map_err(|e| StoreError::Backend(e.to_string()))
        };
        let log = TypedKeyspace::new(open("hs_push.notification_log")?);
        let seq = TypedKeyspace::new(open("hs_push.notification_log_seq")?);
        let read_marks = TypedKeyspace::new(open("hs_push.notification_read_marks")?);
        Ok(Self {
            backend,
            log,
            seq,
            read_marks,
        })
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> NotificationLogStore for TablesNotificationLogStore<B> {
    async fn append(
        &self,
        user_id: &UserId,
        notification: NewNotification,
    ) -> Result<u64, StoreError> {
        let row = Row {
            room_id: notification.room_id,
            event_id: notification.event_id,
            event: notification.event,
            actions: notification.actions,
            profile_tag: notification.profile_tag,
            ts_ms: notification.ts_ms,
        };
        let value = serde_json::to_vec(&row)
            .map_err(|e| StoreError::Backend(format!("encode notification: {e}")))?;
        let seq_key = (user_id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let current = self
                .seq
                .get(txn, &seq_key)
                .map_err(hs_kv::KvError::backend)?
                .map(|b| u64_of(b.as_ref()))
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
        before: Option<u64>,
        limit: usize,
        only_highlight: bool,
    ) -> Result<Vec<NotificationEntry>, StoreError> {
        let snap = self.backend.snapshot();
        let user_key = user_id.to_string();
        let ceiling = before.unwrap_or(u64::MAX);
        let spec = hs_kv::RangeSpec::new(
            std::ops::Bound::Included(bytes::Bytes::from(hs_tables::key::encode(&(
                user_key.clone(),
                0u64,
            )))),
            std::ops::Bound::Excluded(bytes::Bytes::from(hs_tables::key::encode(&(
                user_key.clone(),
                ceiling,
            )))),
        )
        .reverse();
        let mut marks: std::collections::HashMap<OwnedRoomId, u64> =
            std::collections::HashMap::new();
        let mut out = Vec::new();
        for item in self.log.range(&snap, spec) {
            let ((_, seq), bytes) = item?;
            let row: Row = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Backend(format!("decode notification: {e}")))?;
            if only_highlight && !row.actions.iter().any(Action::is_highlight) {
                continue;
            }
            let mark = match marks.get(&row.room_id) {
                Some(m) => *m,
                None => {
                    let m = self
                        .read_marks
                        .get(&snap, &(user_key.clone(), row.room_id.to_string()))?
                        .map(|b| u64_of(b.as_ref()))
                        .unwrap_or(0);
                    marks.insert(row.room_id.clone(), m);
                    m
                }
            };
            out.push(NotificationEntry {
                seq,
                read: seq <= mark,
                room_id: row.room_id,
                event_id: row.event_id,
                event: row.event,
                actions: row.actions,
                profile_tag: row.profile_tag,
                ts_ms: row.ts_ms,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn mark_room_read(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError> {
        let seq_key = (user_id.to_string(),);
        let mark_key = (user_id.to_string(), room_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let current = self
                .seq
                .get(txn, &seq_key)
                .map_err(hs_kv::KvError::backend)?
                .map(|b| u64_of(b.as_ref()))
                .unwrap_or(0);
            self.read_marks
                .put(txn, &mark_key, &current.to_le_bytes())
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
    async fn satisfies_the_shared_notification_log_contract() {
        let store = TablesNotificationLogStore::open(MemoryBackend::new()).unwrap();
        crate::notification_log::contract_tests::behaves_correctly(&store).await;
    }
}
