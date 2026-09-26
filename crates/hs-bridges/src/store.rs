//! The manager's tables: offerings, instances, and which of the manager's bots sit in which
//! rooms. JSON rows in `hs-kv`, like the appservice registry's. Every change to an instance goes
//! through [`BridgeStore::update_instance`], a read-modify-write in one serializable
//! transaction, so two replicas advancing the same instance cannot both win.

use hs_kv::{KvBackend, KvError, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};

use hs_admin::model::{BridgeOfferingAccess, BridgeOfferingOptions};

/// Why a store call failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store: {0}")]
    Store(String),
    #[error("decode: {0}")]
    Decode(String),
}

impl From<KvError> for StoreError {
    fn from(e: KvError) -> Self {
        Self::Store(e.to_string())
    }
}

impl From<hs_tables::TableError> for StoreError {
    fn from(e: hs_tables::TableError) -> Self {
        Self::Store(e.to_string())
    }
}

/// An offering as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfferingRow {
    pub bridge_type: String,
    pub enabled: bool,
    /// `cluster` or `elsewhere`.
    pub runtime: String,
    pub image_tag: String,
    pub access: BridgeOfferingAccess,
    pub options: BridgeOfferingOptions,
    pub created_at_ms: u64,
}

/// Where an instance is in its life (RFC 0017 section 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Requested,
    Registered,
    Deploying,
    Starting,
    Ready,
    Failed,
    Removing,
}

impl InstanceState {
    /// The word the admin API uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Registered => "registered",
            Self::Deploying => "deploying",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Removing => "removing",
        }
    }
}

/// An instance as stored. Holds its tokens: the manager renders its files from them, and its bot
/// speaks with them.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceRow {
    pub bridge_type: String,
    /// The owner's Matrix ID, or `_` for a shared type's instance.
    pub owner: String,
    pub state: InstanceState,
    pub reason: Option<String>,
    pub appservice_id: Option<String>,
    /// The Kubernetes name of its `Bridge`, Deployment and Service.
    pub deploy_name: Option<String>,
    pub as_token: Option<String>,
    pub hs_token: Option<String>,
    /// Where this server reaches it: the registration's `url`.
    pub url: Option<String>,
    /// The room its owner asked for it in, to tell them there when it is ready.
    pub front_door_room: Option<String>,
    /// The direct chat between the owner and the instance's bot, once made.
    pub dm_room: Option<String>,
    pub created_at_ms: u64,
    /// When it entered its current state.
    pub state_since_ms: u64,
    pub ready_at_ms: Option<u64>,
}

impl std::fmt::Debug for InstanceRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceRow")
            .field("bridge_type", &self.bridge_type)
            .field("owner", &self.owner)
            .field("state", &self.state)
            .field("reason", &self.reason)
            .field("appservice_id", &self.appservice_id)
            .field("deploy_name", &self.deploy_name)
            .finish_non_exhaustive()
    }
}

impl InstanceRow {
    /// A new instance, `requested` now.
    #[must_use]
    pub fn new(bridge_type: &str, owner: &str, now_ms: u64) -> Self {
        Self {
            bridge_type: bridge_type.to_owned(),
            owner: owner.to_owned(),
            state: InstanceState::Requested,
            reason: None,
            appservice_id: None,
            deploy_name: None,
            as_token: None,
            hs_token: None,
            url: None,
            front_door_room: None,
            dm_room: None,
            created_at_ms: now_ms,
            state_since_ms: now_ms,
            ready_at_ms: None,
        }
    }

    /// Moves to `state`, noting when.
    pub fn enter(&mut self, state: InstanceState, now_ms: u64) {
        if self.state != state {
            self.state = state;
            self.state_since_ms = now_ms;
        }
    }
}

/// The manager's own appservice: its tokens, minted once.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagerRow {
    pub as_token: String,
    pub hs_token: String,
}

/// The tables.
pub struct BridgeStore<B: KvBackend> {
    backend: B,
    offerings: TypedKeyspace<B::Keyspace, (String,)>,
    instances: TypedKeyspace<B::Keyspace, (String, String)>,
    /// `room_id -> bot localpart`: which of the manager's bots is in which room.
    rooms: TypedKeyspace<B::Keyspace, (String,)>,
    meta: TypedKeyspace<B::Keyspace, (String,)>,
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::Decode(e.to_string()))
}

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

fn kv<E: std::error::Error + Send + Sync + 'static>(e: E) -> KvError {
    KvError::backend(e)
}

impl<B: KvBackend> BridgeStore<B> {
    /// Opens (creating) the keyspaces.
    ///
    /// # Errors
    /// [`StoreError::Store`] if one cannot be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        Ok(Self {
            offerings: TypedKeyspace::new(backend.keyspace("hs_bridges.offerings")?),
            instances: TypedKeyspace::new(backend.keyspace("hs_bridges.instances")?),
            rooms: TypedKeyspace::new(backend.keyspace("hs_bridges.rooms")?),
            meta: TypedKeyspace::new(backend.keyspace("hs_bridges.meta")?),
            backend,
        })
    }

    /// # Errors
    /// On a store failure.
    pub fn offering(&self, bridge_type: &str) -> Result<Option<OfferingRow>, StoreError> {
        let snap = self.backend.snapshot();
        self.offerings
            .get(&snap, &(bridge_type.to_owned(),))?
            .map(|b| decode(&b))
            .transpose()
    }

    /// # Errors
    /// On a store failure.
    pub fn offerings(&self) -> Result<Vec<OfferingRow>, StoreError> {
        let snap = self.backend.snapshot();
        self.offerings
            .range(&snap, RangeSpec::full())
            .map(|item| decode(&item?.1))
            .collect()
    }

    /// # Errors
    /// On a store failure.
    pub fn put_offering(&self, row: &OfferingRow) -> Result<(), StoreError> {
        let key = (row.bridge_type.clone(),);
        let value = encode(row);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.offerings.put(txn, &key, &value).map_err(kv)
        })?;
        Ok(())
    }

    /// # Errors
    /// On a store failure.
    pub fn delete_offering(&self, bridge_type: &str) -> Result<(), StoreError> {
        let key = (bridge_type.to_owned(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.offerings.delete(txn, &key).map_err(kv)
        })?;
        Ok(())
    }

    /// # Errors
    /// On a store failure.
    pub fn instance(
        &self,
        bridge_type: &str,
        owner: &str,
    ) -> Result<Option<InstanceRow>, StoreError> {
        let snap = self.backend.snapshot();
        self.instances
            .get(&snap, &(bridge_type.to_owned(), owner.to_owned()))?
            .map(|b| decode(&b))
            .transpose()
    }

    /// Every instance, or every instance of one type.
    ///
    /// # Errors
    /// On a store failure.
    pub fn instances(&self, bridge_type: Option<&str>) -> Result<Vec<InstanceRow>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = match bridge_type {
            Some(t) => TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(t.to_owned(),)),
            None => RangeSpec::full(),
        };
        self.instances
            .range(&snap, spec)
            .map(|item| decode(&item?.1))
            .collect()
    }

    /// Inserts `row` unless an instance with its key exists; returns the one that is there
    /// afterwards and whether it was this call that made it.
    ///
    /// # Errors
    /// On a store failure.
    pub fn insert_instance(&self, row: &InstanceRow) -> Result<(InstanceRow, bool), StoreError> {
        let key = (row.bridge_type.clone(), row.owner.clone());
        let value = encode(row);
        let existing = transact(&self.backend, TransactConfig::default(), |txn| {
            if let Some(bytes) = self.instances.get(txn, &key).map_err(kv)? {
                return Ok(Some(bytes));
            }
            self.instances.put(txn, &key, &value).map_err(kv)?;
            Ok(None)
        })?;
        match existing {
            Some(bytes) => Ok((decode(&bytes)?, false)),
            None => Ok((row.clone(), true)),
        }
    }

    /// Reads the instance, applies `f`, and writes the result back, in one transaction.
    /// `f` returning `None` changes nothing; returning `Some(None)`... is not a thing: to delete,
    /// use [`BridgeStore::delete_instance`]. Returns the row as written, or `None` if there was
    /// no such instance or `f` declined.
    ///
    /// # Errors
    /// On a store failure.
    pub fn update_instance(
        &self,
        bridge_type: &str,
        owner: &str,
        mut f: impl FnMut(&mut InstanceRow) -> bool,
    ) -> Result<Option<InstanceRow>, StoreError> {
        let key = (bridge_type.to_owned(), owner.to_owned());
        let out = transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.instances.get(txn, &key).map_err(kv)? else {
                return Ok(None);
            };
            let mut row: InstanceRow = decode(&bytes).map_err(kv)?;
            if !f(&mut row) {
                return Ok(None);
            }
            self.instances.put(txn, &key, &encode(&row)).map_err(kv)?;
            Ok(Some(row))
        })?;
        Ok(out)
    }

    /// # Errors
    /// On a store failure.
    pub fn delete_instance(&self, bridge_type: &str, owner: &str) -> Result<(), StoreError> {
        let key = (bridge_type.to_owned(), owner.to_owned());
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.instances.delete(txn, &key).map_err(kv)
        })?;
        Ok(())
    }

    /// Records that bot `localpart` is in `room_id`.
    ///
    /// # Errors
    /// On a store failure.
    pub fn put_room(&self, room_id: &str, localpart: &str) -> Result<(), StoreError> {
        let key = (room_id.to_owned(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.rooms.put(txn, &key, localpart.as_bytes()).map_err(kv)
        })?;
        Ok(())
    }

    /// Which of the manager's bots is in `room_id`.
    ///
    /// # Errors
    /// On a store failure.
    pub fn room(&self, room_id: &str) -> Result<Option<String>, StoreError> {
        let snap = self.backend.snapshot();
        Ok(self
            .rooms
            .get(&snap, &(room_id.to_owned(),))?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// The manager's tokens, minted by `mint` the first time and kept.
    ///
    /// # Errors
    /// On a store failure.
    pub fn manager(&self, mint: impl Fn() -> ManagerRow) -> Result<ManagerRow, StoreError> {
        let key = ("manager".to_owned(),);
        let bytes = transact(&self.backend, TransactConfig::default(), |txn| {
            if let Some(bytes) = self.meta.get(txn, &key).map_err(kv)? {
                return Ok(bytes.to_vec());
            }
            let value = encode(&mint());
            self.meta.put(txn, &key, &value).map_err(kv)?;
            Ok(value)
        })?;
        decode(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    #[test]
    fn instances_insert_once_and_update_in_place() {
        let store = BridgeStore::open(MemoryBackend::new()).unwrap();
        let row = InstanceRow::new("mautrix-whatsapp", "@a:x", 1);
        let (_, created) = store.insert_instance(&row).unwrap();
        assert!(created);
        let (again, created) = store
            .insert_instance(&InstanceRow::new("mautrix-whatsapp", "@a:x", 2))
            .unwrap();
        assert!(!created);
        assert_eq!(again.created_at_ms, 1);
        store
            .insert_instance(&InstanceRow::new("mautrix-signal", "@a:x", 3))
            .unwrap();
        assert_eq!(store.instances(Some("mautrix-whatsapp")).unwrap().len(), 1);
        assert_eq!(store.instances(None).unwrap().len(), 2);

        let updated = store
            .update_instance("mautrix-whatsapp", "@a:x", |r| {
                r.enter(InstanceState::Registered, 5);
                true
            })
            .unwrap()
            .unwrap();
        assert_eq!(updated.state, InstanceState::Registered);
        assert_eq!(updated.state_since_ms, 5);
        assert!(
            store
                .update_instance("mautrix-whatsapp", "@a:x", |_| false)
                .unwrap()
                .is_none()
        );
        store.delete_instance("mautrix-whatsapp", "@a:x").unwrap();
        assert!(
            store
                .instance("mautrix-whatsapp", "@a:x")
                .unwrap()
                .is_none()
        );

        let m1 = store
            .manager(|| ManagerRow {
                as_token: "a".into(),
                hs_token: "b".into(),
            })
            .unwrap();
        let m2 = store
            .manager(|| ManagerRow {
                as_token: "c".into(),
                hs_token: "d".into(),
            })
            .unwrap();
        assert_eq!(m1.as_token, m2.as_token);
    }
}
