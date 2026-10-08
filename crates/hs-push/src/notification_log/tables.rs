//! An `hs-kv`/`hs-tables`-backed [`super::NotificationLogStore`].

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::push::Action;
use ruma::{OwnedEventId, OwnedRoomId, RoomId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::collections::HashMap;

use super::{
    NewNotification, NotificationEntry, NotificationLogStore, ReadMark, entry_is_read,
    thread_of_event,
};
use crate::counts::ReceiptThread;
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
    /// The event's room position; absent on rows logged before positions were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pos: Option<i64>,
    /// The thread the event is in; absent on rows logged before threads were kept (read off
    /// `event` then) and for the main timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread: Option<OwnedEventId>,
}

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.notification_log` | `(user_id, seq)` | JSON-encoded [`Row`] |
/// | `hs_push.notification_log_seq` | `(user_id,)` | the last `seq` assigned, as 8 little-endian bytes |
/// | `hs_push.notification_read_scopes` | `(user_id, room_id, mark_key)` | a [`ReadMark`], JSON: what the receipts for one scope read (`mark_key`: `""` unthreaded, `main`, or a thread root) |
/// | `hs_push.notification_read_marks` | `(user_id, room_id)` | legacy, read only: the newest `seq` any receipt for the room covered before marks were kept per scope, as 8 little-endian bytes; taken as the unthreaded mark's `whole_seq` |
///
/// `seq` sorts numerically because `hs-tables`' `u64` key encoding is order-preserving (the same
/// property `hs_room`'s `room_pos` timeline index relies on), so a reverse scan from `(user_id,
/// before)` yields entries newest-first with no secondary sort needed.
pub struct TablesNotificationLogStore<B: KvBackend> {
    backend: B,
    log: TypedKeyspace<B::Keyspace, (String, u64)>,
    seq: TypedKeyspace<B::Keyspace, (String,)>,
    read_marks: TypedKeyspace<B::Keyspace, (String, String)>,
    read_scopes: TypedKeyspace<B::Keyspace, (String, String, String)>,
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
        let read_scopes = TypedKeyspace::new(open("hs_push.notification_read_scopes")?);
        Ok(Self {
            backend,
            log,
            seq,
            read_marks,
            read_scopes,
        })
    }

    /// `room_id`'s marks for `user_id`, by scope, the legacy whole-room mark folded in.
    fn room_marks(
        &self,
        snap: &impl hs_kv::KvRead<Keyspace = B::Keyspace>,
        user_id: &str,
        room_id: &str,
    ) -> Result<HashMap<String, ReadMark>, StoreError> {
        let prefix = (user_id.to_owned(), room_id.to_owned());
        let mut marks = HashMap::new();
        for item in self.read_scopes.range(
            snap,
            TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&prefix),
        ) {
            let ((_, _, mark_key), bytes) = item?;
            let mark: ReadMark = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Backend(format!("decode read mark: {e}")))?;
            marks.insert(mark_key, mark);
        }
        if let Some(legacy) = self.read_marks.get(snap, &prefix)? {
            let unthreaded: &mut ReadMark = marks
                .entry(ReceiptThread::Unthreaded.mark_key())
                .or_default();
            unthreaded.whole_seq = unthreaded.whole_seq.max(u64_of(legacy.as_ref()));
        }
        Ok(marks)
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
            pos: notification.pos,
            thread: notification.thread,
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
        let mut marks: HashMap<OwnedRoomId, HashMap<String, ReadMark>> = HashMap::new();
        let mut out = Vec::new();
        for item in self.log.range(&snap, spec) {
            let ((_, seq), bytes) = item?;
            let row: Row = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Backend(format!("decode notification: {e}")))?;
            if only_highlight && !row.actions.iter().any(Action::is_highlight) {
                continue;
            }
            if !marks.contains_key(&row.room_id) {
                let room_marks = self.room_marks(&snap, &user_key, row.room_id.as_str())?;
                marks.insert(row.room_id.clone(), room_marks);
            }
            let thread = row.thread.clone().or_else(|| thread_of_event(&row.event));
            let read = marks
                .get(&row.room_id)
                .is_some_and(|m| entry_is_read(m, seq, row.pos, thread.as_deref()));
            out.push(NotificationEntry {
                seq,
                read,
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

    async fn mark_read(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        thread: &ReceiptThread,
        pos: Option<i64>,
    ) -> Result<(), StoreError> {
        let seq_key = (user_id.to_string(),);
        let mark_key = (user_id.to_string(), room_id.to_string(), thread.mark_key());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let newest = self
                .seq
                .get(txn, &seq_key)
                .map_err(hs_kv::KvError::backend)?
                .map(|b| u64_of(b.as_ref()))
                .unwrap_or(0);
            let mut mark: ReadMark = self
                .read_scopes
                .get(txn, &mark_key)
                .map_err(hs_kv::KvError::backend)?
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            mark.apply(newest, pos);
            let bytes = serde_json::to_vec(&mark).map_err(hs_kv::KvError::backend)?;
            self.read_scopes
                .put(txn, &mark_key, &bytes)
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
        let store = TablesNotificationLogStore::open(MemoryBackend::new()).unwrap();
        crate::notification_log::contract_tests::receipts_mark_entries_read_per_thread(&store)
            .await;
    }

    /// A room read before marks were kept per scope (the old whole-room row) still reads as
    /// read up to where that receipt was, and later entries do not.
    #[tokio::test]
    async fn a_whole_room_mark_from_an_older_build_still_reads() {
        let backend = MemoryBackend::new();
        let store = TablesNotificationLogStore::open(backend.clone()).unwrap();
        let alice = ruma::user_id!("@alice:example.org");
        let room = ruma::room_id!("!room:example.org");
        let entry = |id: &str| NewNotification {
            room_id: room.to_owned(),
            event_id: ruma::EventId::parse(id).unwrap(),
            event: serde_json::json!({"event_id": id}),
            actions: vec![Action::Notify],
            profile_tag: None,
            ts_ms: 0,
            pos: None,
            thread: None,
        };
        let first = store
            .append(alice, entry("$one:example.org"))
            .await
            .unwrap();
        // What `mark_room_read` wrote before this build.
        transact(&backend, TransactConfig::default(), |txn| {
            store
                .read_marks
                .put(
                    txn,
                    &(alice.to_string(), room.to_string()),
                    &first.to_le_bytes(),
                )
                .map_err(hs_kv::KvError::backend)
        })
        .unwrap();
        store
            .append(alice, entry("$two:example.org"))
            .await
            .unwrap();
        let page = store.page(alice, None, 10, false).await.unwrap();
        let read: Vec<(String, bool)> = page
            .iter()
            .map(|e| (e.event_id.to_string(), e.read))
            .collect();
        assert_eq!(
            read,
            [
                ("$two:example.org".to_owned(), false),
                ("$one:example.org".to_owned(), true)
            ]
        );
    }
}
