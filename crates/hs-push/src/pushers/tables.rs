//! An `hs-kv`/`hs-tables`-backed [`super::PusherStore`].

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::UserId;
use ruma::api::client::push::{Pusher, PusherIds};

use super::PusherStore;
use crate::error::StoreError;

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.pushers` | `(user_id, app_id, pushkey)` | the `Pusher`, JSON-encoded |
pub struct TablesPusherStore<B: KvBackend> {
    backend: B,
    pushers: TypedKeyspace<B::Keyspace, (String, String, String)>,
}

impl<B: KvBackend> TablesPusherStore<B> {
    /// Opens (creating if necessary) this store's keyspace.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let pushers = TypedKeyspace::new(
            backend
                .keyspace("hs_push.pushers")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        Ok(Self { backend, pushers })
    }
}

fn key(user_id: &UserId, ids: &PusherIds) -> (String, String, String) {
    (user_id.to_string(), ids.app_id.clone(), ids.pushkey.clone())
}

#[async_trait::async_trait]
impl<B: KvBackend> PusherStore for TablesPusherStore<B> {
    async fn get_pushers(&self, user_id: &UserId) -> Result<Vec<Pusher>, StoreError> {
        let snap = self.backend.snapshot();
        let prefix = (user_id.to_string(),);
        let spec = TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&prefix);
        let mut out = Vec::new();
        for item in self.pushers.range(&snap, spec) {
            let (_, bytes) = item?;
            let pusher: Pusher =
                serde_json::from_slice(&bytes).map_err(|e| StoreError::Backend(format!("decode pusher: {e}")))?;
            out.push(pusher);
        }
        Ok(out)
    }

    async fn set_pusher(&self, user_id: &UserId, pusher: Pusher) -> Result<(), StoreError> {
        let k = key(user_id, &pusher.ids);
        let value = serde_json::to_vec(&pusher).map_err(|e| StoreError::Backend(format!("encode pusher: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.pushers.put(txn, &k, &value).map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }

    async fn delete_pusher(&self, user_id: &UserId, ids: &PusherIds) -> Result<(), StoreError> {
        let k = key(user_id, ids);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.pushers.delete(txn, &k).map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use ruma::api::client::push::{PusherInit, PusherKind};
    use ruma::push::HttpPusherData;
    use ruma::user_id;

    fn sample_pusher(pushkey: &str) -> Pusher {
        PusherInit {
            ids: PusherIds::new(pushkey.to_owned(), "com.example.app".to_owned()),
            kind: PusherKind::Http(HttpPusherData::new("https://gw.example.org/notify".to_owned())),
            app_display_name: "Example".to_owned(),
            device_display_name: "Phone".to_owned(),
            profile_tag: None,
            lang: "en".to_owned(),
        }
        .into()
    }

    #[tokio::test]
    async fn round_trips_and_scopes_by_user() {
        let store = TablesPusherStore::open(MemoryBackend::new()).unwrap();
        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");

        store.set_pusher(alice, sample_pusher("key-1")).await.unwrap();
        store.set_pusher(bob, sample_pusher("key-2")).await.unwrap();

        let alice_pushers = store.get_pushers(alice).await.unwrap();
        assert_eq!(alice_pushers.len(), 1);
        assert_eq!(alice_pushers[0].ids.pushkey, "key-1");

        store
            .delete_pusher(
                alice,
                &PusherIds::new("key-1".to_owned(), "com.example.app".to_owned()),
            )
            .await
            .unwrap();
        assert!(store.get_pushers(alice).await.unwrap().is_empty());
        assert_eq!(store.get_pushers(bob).await.unwrap().len(), 1);
    }
}
