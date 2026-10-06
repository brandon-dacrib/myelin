//! An `hs-kv`/`hs-tables`-backed [`super::CountsStore`].
//!
//! | keyspace | primary key | value |
//! |---|---|---|
//! | `hs_push.unread` | `(user_id, room_id, thread_key, pos)` | one unread notification: `1` if highlighted, else `0` |
//! | `hs_push.read_marks` | `(user_id, room_id, mark_key)` | the room-local position a receipt read up to, 8 bytes big-endian |
//! | `hs_push.counts` | `(user_id, room_id, thread_key)` | legacy: counters written before notifications were kept one by one |
//!
//! `thread_key` is `""` for the room's main timeline, else the thread root's event ID;
//! `mark_key` is `""` for an unthreaded receipt, `main`, or a thread root's event ID
//! ([`super::ReceiptThread`]). Keys start `(user_id, room_id)`, so a room's counts are one
//! prefix scan and a user's badge one more.
//!
//! The legacy counters have no positions: they are counted until a receipt that covers their
//! scope (or a reset) deletes them, and nothing writes them any more.

use hs_kv::{KvBackend, KvError, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::{RoomId, UserId};

use super::{Counts, CountsStore, ReceiptThread, RoomNotificationCounts, Scope};
use crate::error::StoreError;

type UnreadKey = (String, String, String, i64);
type TripleKey = (String, String, String);

/// The store. See the module docs.
pub struct TablesCountsStore<B: KvBackend> {
    backend: B,
    unread: TypedKeyspace<B::Keyspace, UnreadKey>,
    marks: TypedKeyspace<B::Keyspace, TripleKey>,
    legacy: TypedKeyspace<B::Keyspace, TripleKey>,
}

fn decode_legacy(bytes: &[u8]) -> Result<Counts, StoreError> {
    let (Some(n), Some(h)) = (bytes.get(0..8), bytes.get(8..16)) else {
        return Err(StoreError::Backend(format!(
            "counts row has unexpected length {}",
            bytes.len()
        )));
    };
    let word = |b: &[u8]| {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        u64::from_le_bytes(arr)
    };
    Ok(Counts {
        notification_count: word(n),
        highlight_count: word(h),
    })
}

fn decode_pos(bytes: &[u8]) -> Option<i64> {
    let arr: [u8; 8] = bytes.try_into().ok()?;
    Some(i64::from_be_bytes(arr))
}

fn room_prefix(user_id: &UserId, room_id: &RoomId) -> (String, String) {
    (user_id.to_string(), room_id.to_string())
}

impl<B: KvBackend> TablesCountsStore<B> {
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
        Ok(Self {
            unread: TypedKeyspace::new(open("hs_push.unread")?),
            marks: TypedKeyspace::new(open("hs_push.read_marks")?),
            legacy: TypedKeyspace::new(open("hs_push.counts")?),
            backend,
        })
    }

    /// Deletes the unread rows and legacy counters of `user_id` in `room_id` that `covers`
    /// says go (by scope key and, for unread rows, position).
    fn delete_where(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        covers: impl Fn(&str, Option<i64>) -> bool,
    ) -> Result<(), StoreError> {
        let prefix = room_prefix(user_id, room_id);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let mut unread = Vec::new();
            for item in self.unread.range(
                txn,
                TypedKeyspace::<B::Keyspace, UnreadKey>::prefix(&prefix),
            ) {
                let (key, _) = item.map_err(KvError::backend)?;
                if covers(&key.2, Some(key.3)) {
                    unread.push(key);
                }
            }
            let mut legacy = Vec::new();
            for item in self.legacy.range(
                txn,
                TypedKeyspace::<B::Keyspace, TripleKey>::prefix(&prefix),
            ) {
                let (key, _) = item.map_err(KvError::backend)?;
                if covers(&key.2, None) {
                    legacy.push(key);
                }
            }
            for key in &unread {
                self.unread.delete(txn, key).map_err(KvError::backend)?;
            }
            for key in &legacy {
                self.legacy.delete(txn, key).map_err(KvError::backend)?;
            }
            Ok(())
        })
        .map_err(StoreError::from)
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
        let prefix = room_prefix(user_id, room_id);
        let mut out = RoomNotificationCounts::default();
        let mut add = |thread_key: &str, counts: Counts| -> Result<(), StoreError> {
            match Scope::parse_key_part(thread_key)? {
                None => out.main.add(counts),
                Some(root) => out.threads.entry(root).or_default().add(counts),
            }
            Ok(())
        };
        for item in self.unread.range(
            &snap,
            TypedKeyspace::<B::Keyspace, UnreadKey>::prefix(&prefix),
        ) {
            let ((_, _, thread_key, _), value) = item?;
            add(
                &thread_key,
                Counts {
                    notification_count: 1,
                    highlight_count: u64::from(value.first() == Some(&1)),
                },
            )?;
        }
        for item in self.legacy.range(
            &snap,
            TypedKeyspace::<B::Keyspace, TripleKey>::prefix(&prefix),
        ) {
            let ((_, _, thread_key), value) = item?;
            add(&thread_key, decode_legacy(&value)?)?;
        }
        Ok(out)
    }

    async fn record_notification(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
        highlight: bool,
        pos: i64,
    ) -> Result<(), StoreError> {
        let root = match scope {
            Scope::Main => None,
            Scope::Thread(root) => Some(root),
        };
        let mark_keys = [
            ReceiptThread::Unthreaded.mark_key(),
            ReceiptThread::of_scope(root).mark_key(),
        ];
        let key = (
            user_id.to_string(),
            room_id.to_string(),
            scope.key_part(),
            pos,
        );
        transact(&self.backend, TransactConfig::default(), |txn| {
            for mark_key in &mark_keys {
                let mark = (user_id.to_string(), room_id.to_string(), mark_key.clone());
                let read_to = self
                    .marks
                    .get(txn, &mark)
                    .map_err(KvError::backend)?
                    .and_then(|b| decode_pos(&b));
                if read_to.is_some_and(|read| pos <= read) {
                    return Ok(());
                }
            }
            self.unread
                .put(txn, &key, &[u8::from(highlight)])
                .map_err(KvError::backend)
        })
        .map_err(StoreError::from)
    }

    async fn mark_read(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        thread: &ReceiptThread,
        pos: i64,
    ) -> Result<(), StoreError> {
        let mark = (user_id.to_string(), room_id.to_string(), thread.mark_key());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let current = self
                .marks
                .get(txn, &mark)
                .map_err(KvError::backend)?
                .and_then(|b| decode_pos(&b));
            if current.is_none_or(|c| c < pos) {
                self.marks
                    .put(txn, &mark, &pos.to_be_bytes())
                    .map_err(KvError::backend)?;
            }
            Ok(())
        })
        .map_err(StoreError::from)?;
        self.delete_where(user_id, room_id, |thread_key, at| {
            at.is_none_or(|at| at <= pos)
                && Scope::parse_key_part(thread_key).is_ok_and(|root| thread.reads(root.as_deref()))
        })
    }

    async fn reset(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
    ) -> Result<(), StoreError> {
        let key = scope.key_part();
        self.delete_where(user_id, room_id, |thread_key, _| thread_key == key)
    }

    async fn reset_room(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError> {
        self.delete_where(user_id, room_id, |_, _| true)
    }

    async fn total_unread(&self, user_id: &UserId) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        let prefix = (user_id.to_string(),);
        let mut total = 0u64;
        for item in self.unread.range(
            &snap,
            TypedKeyspace::<B::Keyspace, UnreadKey>::prefix(&prefix),
        ) {
            item?;
            total += 1;
        }
        for item in self.legacy.range(
            &snap,
            TypedKeyspace::<B::Keyspace, TripleKey>::prefix(&prefix),
        ) {
            let (_, value) = item?;
            total += decode_legacy(&value)?.notification_count;
        }
        Ok(total)
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

    #[tokio::test]
    async fn reads_per_thread_up_to_each_receipt() {
        let store = TablesCountsStore::open(MemoryBackend::new()).unwrap();
        crate::counts::contract_tests::receipts_read_per_thread_up_to_their_position(&store).await;
    }

    /// Counters a previous build wrote (no positions) are counted until a receipt covering
    /// their scope, then gone.
    #[tokio::test]
    async fn legacy_counters_count_until_a_receipt_reads_their_scope() {
        let backend = MemoryBackend::new();
        let store = TablesCountsStore::open(backend.clone()).unwrap();
        let alice = ruma::user_id!("@alice:example.org");
        let room = ruma::room_id!("!r:example.org");
        let root = ruma::event_id!("$root");
        let legacy = |n: u64, h: u64| {
            let mut v = n.to_le_bytes().to_vec();
            v.extend_from_slice(&h.to_le_bytes());
            v
        };
        transact(&backend, TransactConfig::default(), |txn| {
            for (scope, value) in [("", legacy(2, 1)), (root.as_str(), legacy(3, 0))] {
                store
                    .legacy
                    .put(
                        txn,
                        &(alice.to_string(), room.to_string(), scope.to_owned()),
                        &value,
                    )
                    .map_err(KvError::backend)?;
            }
            Ok(())
        })
        .unwrap();
        let counts = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(counts.main.notification_count, 2);
        assert_eq!(counts.main.highlight_count, 1);
        assert_eq!(counts.threads[root].notification_count, 3);
        assert_eq!(store.total_unread(alice).await.unwrap(), 5);

        store
            .mark_read(alice, room, &ReceiptThread::Main, 10)
            .await
            .unwrap();
        let counts = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(counts.main, Counts::default());
        assert_eq!(counts.threads[root].notification_count, 3);
        store
            .mark_read(alice, room, &ReceiptThread::Unthreaded, 1)
            .await
            .unwrap();
        assert_eq!(store.total_unread(alice).await.unwrap(), 0);
    }
}
