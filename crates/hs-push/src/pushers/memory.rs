//! An in-memory [`super::PusherStore`], for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use ruma::UserId;
use ruma::api::client::push::{Pusher, PusherIds};

use super::PusherStore;
use crate::error::StoreError;

type Key = (ruma::OwnedUserId, String, String);

fn key(user_id: &UserId, ids: &PusherIds) -> Key {
    (user_id.to_owned(), ids.app_id.clone(), ids.pushkey.clone())
}

/// An in-memory `(user_id, app_id, pushkey) -> Pusher` map.
#[derive(Default)]
pub struct InMemoryPusherStore {
    rows: RwLock<HashMap<Key, Pusher>>,
}

impl InMemoryPusherStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl PusherStore for InMemoryPusherStore {
    async fn get_pushers(&self, user_id: &UserId) -> Result<Vec<Pusher>, StoreError> {
        Ok(self
            .rows
            .read()
            .unwrap()
            .iter()
            .filter(|((u, _, _), _)| u == user_id)
            .map(|(_, pusher)| pusher.clone())
            .collect())
    }

    async fn set_pusher(&self, user_id: &UserId, pusher: Pusher) -> Result<(), StoreError> {
        let k = key(user_id, &pusher.ids);
        self.rows.write().unwrap().insert(k, pusher);
        Ok(())
    }

    async fn delete_pusher(&self, user_id: &UserId, ids: &PusherIds) -> Result<(), StoreError> {
        self.rows.write().unwrap().remove(&key(user_id, ids));
        Ok(())
    }
}
