//! An in-memory [`super::CountsStore`], for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId};

use super::{Counts, CountsStore, RoomNotificationCounts, Scope};
use crate::error::StoreError;

type Key = (OwnedUserId, OwnedRoomId, String);

/// An in-memory `(user_id, room_id, thread_key) -> Counts` map. `thread_key` is empty for the
/// main timeline, else the thread root event ID as a string (matches
/// `crate::counts::tables::TablesCountsStore`'s on-disk key shape, so the two implementations
/// stay behaviorally identical, per [`super::contract_tests`]).
#[derive(Debug, Default)]
pub struct InMemoryCountsStore {
    rows: RwLock<HashMap<Key, Counts>>,
}

impl InMemoryCountsStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn key(user_id: &UserId, room_id: &RoomId, scope: Scope<'_>) -> Key {
    (user_id.to_owned(), room_id.to_owned(), scope.key_part())
}

#[async_trait::async_trait]
impl CountsStore for InMemoryCountsStore {
    async fn get_room_counts(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<RoomNotificationCounts, StoreError> {
        let rows = self.rows.read().unwrap();
        let mut out = RoomNotificationCounts::default();
        for ((u, r, thread), counts) in rows.iter() {
            if u != user_id || r != room_id {
                continue;
            }
            if thread.is_empty() {
                out.main = *counts;
            } else {
                let root: OwnedEventId = thread.as_str().try_into().map_err(|e| {
                    StoreError::Backend(format!("stored thread key is not an event id: {e}"))
                })?;
                out.threads.insert(root, *counts);
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
        let mut rows = self.rows.write().unwrap();
        let entry = rows.entry(key(user_id, room_id, scope)).or_default();
        entry.notification_count += 1;
        if highlight {
            entry.highlight_count += 1;
        }
        Ok(())
    }

    async fn reset(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
    ) -> Result<(), StoreError> {
        self.rows
            .write()
            .unwrap()
            .remove(&key(user_id, room_id, scope));
        Ok(())
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
}
