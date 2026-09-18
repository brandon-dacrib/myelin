//! An in-memory [`super::NotificationLogStore`], for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use ruma::push::Action;
use ruma::{EventId, RoomId, UserId};

use super::{NotificationEntry, NotificationLogStore, is_highlight};
use crate::error::StoreError;

#[derive(Default)]
struct UserLog {
    next_seq: u64,
    entries: Vec<NotificationEntry>,
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
        room_id: &RoomId,
        event_id: &EventId,
        actions: Vec<Action>,
        ts_ms: u64,
    ) -> Result<u64, StoreError> {
        let mut users = self.users.write().unwrap();
        let log = users.entry(user_id.to_owned()).or_default();
        log.next_seq += 1;
        let seq = log.next_seq;
        log.entries.push(NotificationEntry {
            seq,
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            actions,
            ts_ms,
            read: false,
        });
        Ok(seq)
    }

    async fn page(
        &self,
        user_id: &UserId,
        after: Option<u64>,
        limit: usize,
        only_highlight: bool,
    ) -> Result<Vec<NotificationEntry>, StoreError> {
        let users = self.users.read().unwrap();
        let Some(log) = users.get(user_id) else {
            return Ok(Vec::new());
        };
        let floor = after.unwrap_or(0);
        Ok(log
            .entries
            .iter()
            .filter(|e| e.seq > floor)
            .filter(|e| !only_highlight || is_highlight(&e.actions))
            .take(limit)
            .cloned()
            .collect())
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
