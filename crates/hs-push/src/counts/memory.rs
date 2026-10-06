//! An in-memory [`super::CountsStore`], for tests.

use std::collections::{BTreeMap, HashMap};
use std::sync::{PoisonError, RwLock};

use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};

use super::{Counts, CountsStore, ReceiptThread, RoomNotificationCounts, Scope};
use crate::error::StoreError;

/// `(user_id, room_id, thread_key, pos)`: one unread notification. `thread_key` is empty for the
/// main timeline, else the thread root's event ID (the same key shape as
/// `crate::counts::tables::TablesCountsStore`, so the two behave identically, per
/// [`super::contract_tests`]).
type UnreadKey = (OwnedUserId, OwnedRoomId, String, i64);

/// `(user_id, room_id, mark_key)`: the position a receipt read up to.
type MarkKey = (OwnedUserId, OwnedRoomId, String);

#[derive(Debug, Default)]
struct Rows {
    /// Unread notifications, the value whether it was highlighted.
    unread: BTreeMap<UnreadKey, bool>,
    /// Read positions.
    marks: HashMap<MarkKey, i64>,
}

/// The in-memory store. See the module docs.
#[derive(Debug, Default)]
pub struct InMemoryCountsStore {
    rows: RwLock<Rows>,
}

impl InMemoryCountsStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Rows> {
        self.rows.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Rows> {
        self.rows.write().unwrap_or_else(PoisonError::into_inner)
    }
}

fn mark_key(user_id: &UserId, room_id: &RoomId, thread: &ReceiptThread) -> MarkKey {
    (user_id.to_owned(), room_id.to_owned(), thread.mark_key())
}

#[async_trait::async_trait]
impl CountsStore for InMemoryCountsStore {
    async fn get_room_counts(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<RoomNotificationCounts, StoreError> {
        let rows = self.read();
        let mut out = RoomNotificationCounts::default();
        for ((u, r, thread, _), highlight) in &rows.unread {
            if u != user_id || r != room_id {
                continue;
            }
            let one = Counts {
                notification_count: 1,
                highlight_count: u64::from(*highlight),
            };
            match Scope::parse_key_part(thread)? {
                None => out.main.add(one),
                Some(root) => out.threads.entry(root).or_default().add(one),
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
        pos: i64,
    ) -> Result<(), StoreError> {
        let mut rows = self.write();
        let root = match scope {
            Scope::Main => None,
            Scope::Thread(root) => Some(root),
        };
        let read_to = [ReceiptThread::Unthreaded, ReceiptThread::of_scope(root)]
            .iter()
            .filter_map(|t| rows.marks.get(&mark_key(user_id, room_id, t)).copied())
            .max();
        if read_to.is_some_and(|read| pos <= read) {
            return Ok(());
        }
        rows.unread.insert(
            (
                user_id.to_owned(),
                room_id.to_owned(),
                scope.key_part(),
                pos,
            ),
            highlight,
        );
        Ok(())
    }

    async fn mark_read(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        thread: &ReceiptThread,
        pos: i64,
    ) -> Result<(), StoreError> {
        let mut rows = self.write();
        let mark = rows
            .marks
            .entry(mark_key(user_id, room_id, thread))
            .or_insert(pos);
        *mark = (*mark).max(pos);
        let mut keep = Ok(());
        rows.unread.retain(|(u, r, scope, at), _| {
            if u != user_id || r != room_id || *at > pos {
                return true;
            }
            match Scope::parse_key_part(scope) {
                Ok(root) => !thread.reads(root.as_deref()),
                Err(e) => {
                    keep = Err(e);
                    true
                }
            }
        });
        keep
    }

    async fn reset(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
    ) -> Result<(), StoreError> {
        let key = scope.key_part();
        self.write()
            .unread
            .retain(|(u, r, s, _), _| !(u == user_id && r == room_id && *s == key));
        Ok(())
    }

    async fn reset_room(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError> {
        self.write()
            .unread
            .retain(|(u, r, _, _), _| !(u == user_id && r == room_id));
        Ok(())
    }

    async fn total_unread(&self, user_id: &UserId) -> Result<u64, StoreError> {
        Ok(self
            .read()
            .unread
            .keys()
            .filter(|(u, _, _, _)| u == user_id)
            .count() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn satisfies_the_shared_counts_store_contract() {
        let store = InMemoryCountsStore::new();
        crate::counts::contract_tests::behaves_correctly(&store).await;
    }

    #[tokio::test]
    async fn reads_per_thread_up_to_each_receipt() {
        let store = InMemoryCountsStore::new();
        crate::counts::contract_tests::receipts_read_per_thread_up_to_their_position(&store).await;
    }
}
