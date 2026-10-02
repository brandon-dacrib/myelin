//! An `hs-kv`/`hs-tables`-backed [`super::PusherStore`].

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::api::client::push::{Pusher, PusherIds};
use ruma::{DeviceId, OwnedDeviceId, UserId};

use super::PusherStore;
use crate::error::StoreError;

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.pushers` | `(user_id, app_id, pushkey)` | the `Pusher`, JSON-encoded |
/// | `hs_push.pusher_devices` | `(user_id, app_id, pushkey)` | the device that registered it, as UTF-8; no row for a pusher with no known device |
pub struct TablesPusherStore<B: KvBackend> {
    backend: B,
    pushers: TypedKeyspace<B::Keyspace, (String, String, String)>,
    devices: TypedKeyspace<B::Keyspace, (String, String, String)>,
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
        let devices = TypedKeyspace::new(
            backend
                .keyspace("hs_push.pusher_devices")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        Ok(Self {
            backend,
            pushers,
            devices,
        })
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
            let pusher: Pusher = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Backend(format!("decode pusher: {e}")))?;
            out.push(pusher);
        }
        Ok(out)
    }

    async fn set_pusher(
        &self,
        user_id: &UserId,
        pusher: Pusher,
        device_id: Option<OwnedDeviceId>,
    ) -> Result<(), StoreError> {
        let k = key(user_id, &pusher.ids);
        let value = serde_json::to_vec(&pusher)
            .map_err(|e| StoreError::Backend(format!("encode pusher: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.pushers
                .put(txn, &k, &value)
                .map_err(hs_kv::KvError::backend)?;
            match &device_id {
                Some(device) => self
                    .devices
                    .put(txn, &k, device.as_bytes())
                    .map_err(hs_kv::KvError::backend),
                None => self
                    .devices
                    .delete(txn, &k)
                    .map_err(hs_kv::KvError::backend),
            }
        })
        .map_err(StoreError::from)
    }

    async fn delete_pusher(&self, user_id: &UserId, ids: &PusherIds) -> Result<(), StoreError> {
        let k = key(user_id, ids);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.pushers
                .delete(txn, &k)
                .map_err(hs_kv::KvError::backend)?;
            self.devices
                .delete(txn, &k)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }

    async fn delete_pushers_of_other_devices(
        &self,
        user_id: &UserId,
        kept: Option<&DeviceId>,
    ) -> Result<(), StoreError> {
        let snap = self.backend.snapshot();
        let prefix = (user_id.to_string(),);
        let spec = TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&prefix);
        let mut doomed = Vec::new();
        for item in self.devices.range(&snap, spec) {
            let (k, device) = item?;
            if kept.is_some_and(|d| d.as_bytes() == device.as_ref()) {
                continue;
            }
            doomed.push(k);
        }
        if doomed.is_empty() {
            return Ok(());
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            for k in &doomed {
                self.pushers
                    .delete(txn, k)
                    .map_err(hs_kv::KvError::backend)?;
                self.devices
                    .delete(txn, k)
                    .map_err(hs_kv::KvError::backend)?;
            }
            Ok(())
        })
        .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pushers::contract_tests::{devices_scope_deletion, sample_pusher};
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    #[tokio::test]
    async fn round_trips_and_scopes_by_user() {
        let store = TablesPusherStore::open(MemoryBackend::new()).unwrap();
        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");

        store
            .set_pusher(alice, sample_pusher("key-1"), None)
            .await
            .unwrap();
        store
            .set_pusher(bob, sample_pusher("key-2"), None)
            .await
            .unwrap();

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

    #[tokio::test]
    async fn satisfies_the_shared_pusher_store_contract() {
        let store = TablesPusherStore::open(MemoryBackend::new()).unwrap();
        devices_scope_deletion(&store).await;
    }
}
