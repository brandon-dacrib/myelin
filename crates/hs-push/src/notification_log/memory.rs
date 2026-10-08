//! An in-memory [`super::NotificationLogStore`], for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use ruma::{OwnedEventId, OwnedRoomId, RoomId, UserId};

use super::{
    NewNotification, NotificationEntry, NotificationLogStore, ReadMark, entry_is_read,
    is_highlight, thread_of_event,
};
use crate::counts::ReceiptThread;
use crate::error::StoreError;

/// One logged entry, with what deciding whether it is read needs.
struct Logged {
    entry: NotificationEntry,
    pos: Option<i64>,
    thread: Option<OwnedEventId>,
}

#[derive(Default)]
struct UserLog {
    next_seq: u64,
    entries: Vec<Logged>,
    /// Per room, per receipt scope (`ReceiptThread`'s stored form), what the receipts read.
    read_marks: HashMap<OwnedRoomId, HashMap<String, ReadMark>>,
}

/// An in-memory per-user notification log.
#[derive(Default)]
pub struct InMemoryNotificationLogStore {
    users: RwLock<HashMap<ruma::OwnedUserId, UserLog>>,
}

impl InMemoryNotificationLogStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl NotificationLogStore for InMemoryNotificationLogStore {
    async fn append(
        &self,
        user_id: &UserId,
        notification: NewNotification,
    ) -> Result<u64, StoreError> {
        let mut users = self.users.write().unwrap();
        let log = users.entry(user_id.to_owned()).or_default();
        log.next_seq += 1;
        let seq = log.next_seq;
        let thread = notification
            .thread
            .or_else(|| thread_of_event(&notification.event));
        log.entries.push(Logged {
            entry: NotificationEntry {
                seq,
                room_id: notification.room_id,
                event_id: notification.event_id,
                event: notification.event,
                actions: notification.actions,
                profile_tag: notification.profile_tag,
                ts_ms: notification.ts_ms,
                read: false,
            },
            pos: notification.pos,
            thread,
        });
        Ok(seq)
    }

    async fn page(
        &self,
        user_id: &UserId,
        before: Option<u64>,
        limit: usize,
        only_highlight: bool,
    ) -> Result<Vec<NotificationEntry>, StoreError> {
        let users = self.users.read().unwrap();
        let Some(log) = users.get(user_id) else {
            return Ok(Vec::new());
        };
        let ceiling = before.unwrap_or(u64::MAX);
        let no_marks = HashMap::new();
        Ok(log
            .entries
            .iter()
            .rev()
            .filter(|l| l.entry.seq < ceiling)
            .filter(|l| !only_highlight || is_highlight(&l.entry.actions))
            .take(limit)
            .map(|l| {
                let mut entry = l.entry.clone();
                let marks = log.read_marks.get(&entry.room_id).unwrap_or(&no_marks);
                entry.read = entry_is_read(marks, entry.seq, l.pos, l.thread.as_deref());
                entry
            })
            .collect())
    }

    async fn mark_read(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        thread: &ReceiptThread,
        pos: Option<i64>,
    ) -> Result<(), StoreError> {
        let mut users = self.users.write().unwrap();
        let log = users.entry(user_id.to_owned()).or_default();
        let newest = log.next_seq;
        log.read_marks
            .entry(room_id.to_owned())
            .or_default()
            .entry(thread.mark_key())
            .or_default()
            .apply(newest, pos);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn satisfies_the_shared_notification_log_contract() {
        let store = InMemoryNotificationLogStore::new();
        crate::notification_log::contract_tests::behaves_correctly(&store).await;
        let store = InMemoryNotificationLogStore::new();
        crate::notification_log::contract_tests::receipts_mark_entries_read_per_thread(&store)
            .await;
    }
}
