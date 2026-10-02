//! An in-memory [`super::NotificationLogStore`], for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use ruma::{OwnedRoomId, RoomId, UserId};

use super::{NewNotification, NotificationEntry, NotificationLogStore, is_highlight};
use crate::error::StoreError;

#[derive(Default)]
struct UserLog {
    next_seq: u64,
    entries: Vec<NotificationEntry>,
    /// Per room, the newest `seq` a receipt has covered.
    read_marks: HashMap<OwnedRoomId, u64>,
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
        log.entries.push(NotificationEntry {
            seq,
            room_id: notification.room_id,
            event_id: notification.event_id,
            event: notification.event,
            actions: notification.actions,
            profile_tag: notification.profile_tag,
            ts_ms: notification.ts_ms,
            read: false,
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
        Ok(log
            .entries
            .iter()
            .rev()
            .filter(|e| e.seq < ceiling)
            .filter(|e| !only_highlight || is_highlight(&e.actions))
            .take(limit)
            .map(|e| {
                let mut entry = e.clone();
                entry.read = log
                    .read_marks
                    .get(&e.room_id)
                    .is_some_and(|mark| e.seq <= *mark);
                entry
            })
            .collect())
    }

    async fn mark_room_read(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError> {
        let mut users = self.users.write().unwrap();
        let log = users.entry(user_id.to_owned()).or_default();
        let mark = log.next_seq;
        log.read_marks.insert(room_id.to_owned(), mark);
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
    }
}
