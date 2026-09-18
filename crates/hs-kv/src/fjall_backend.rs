//! The Fjall 3 backend: the embedded, single-node production backend (`PLAN.md` section 6.5).
//! One Fjall keyspace per table, LZ4 block compression (Fjall's default with the `lz4` feature,
//! which this crate enables), key-value separation for large values, and Fjall's own optimistic
//! serializable transactions, which is what makes this backend able to pass the same conformance
//! suite ([`crate::conformance`]) as the in-memory reference.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use fjall::{KeyspaceCreateOptions, KvSeparationOptions, Readable as _};

use crate::error::{Conflict, KvError};
use crate::limits::{self, TxnBudget};
use crate::traits::{KvBackend, KvRead, KvWrite, RangeItem, RangeIter, RangeSpec};
use crate::watch::{Hub, Watch};

struct Inner {
    db: fjall::OptimisticTxDatabase,
    hub: Hub,
    keyspaces: Mutex<HashMap<String, fjall::OptimisticTxKeyspace>>,
}

/// The Fjall [`KvBackend`]. Cloning shares the open database handle.
#[derive(Clone)]
pub struct FjallBackend {
    inner: Arc<Inner>,
}

impl FjallBackend {
    /// Opens (creating if necessary) a Fjall database rooted at `path`.
    ///
    /// # Errors
    /// Returns [`KvError::Backend`] if Fjall could not open or create the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, KvError> {
        let db = fjall::OptimisticTxDatabase::builder(path)
            .open()
            .map_err(KvError::from)?;
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                hub: Hub::new(),
                keyspaces: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Flushes the active journal so data written so far is durable. `hs-kv` transactions are
    /// crash-safe without this (Fjall's journal makes every commit durable by default), but a
    /// caller with an explicit durability checkpoint — before acknowledging a federation
    /// transaction, say — can force it.
    ///
    /// # Errors
    /// Returns [`KvError::Backend`] on I/O failure.
    pub fn persist_all(&self) -> Result<(), KvError> {
        self.inner
            .db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(KvError::from)
    }
}

impl From<fjall::Error> for KvError {
    fn from(err: fjall::Error) -> Self {
        KvError::backend(err)
    }
}

/// A handle to one Fjall keyspace (one table).
#[derive(Clone)]
pub struct FjallKeyspace {
    name: Arc<str>,
    inner: fjall::OptimisticTxKeyspace,
}

impl std::fmt::Debug for FjallKeyspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FjallKeyspace")
            .field("name", &self.name)
            .finish()
    }
}

/// A read-only, point-in-time view of every keyspace, backed by a Fjall MVCC snapshot.
pub struct FjallSnapshot {
    inner: fjall::Snapshot,
}

/// A read-write, serializable transaction, backed by Fjall's optimistic write transaction.
pub struct FjallTxn {
    inner: fjall::OptimisticWriteTx,
    write_keys: Vec<(String, Vec<u8>)>,
    budget: TxnBudget,
}

fn guard_to_item(guard: fjall::Guard) -> RangeItem {
    guard
        .into_inner()
        .map(|(k, v)| (Bytes::from(k), Bytes::from(v)))
        .map_err(KvError::from)
}

fn fjall_range<'a, R: fjall::Readable>(
    source: &'a R,
    ks: &FjallKeyspace,
    spec: RangeSpec,
) -> RangeIter<'a> {
    let iter = source.range(&ks.inner, (spec.start, spec.end));
    let mut mapped: RangeIter<'a> = if spec.reverse {
        Box::new(iter.rev().map(guard_to_item))
    } else {
        Box::new(iter.map(guard_to_item))
    };
    if let Some(limit) = spec.limit {
        mapped = Box::new(mapped.take(limit));
    }
    mapped
}

impl KvRead for FjallSnapshot {
    type Keyspace = FjallKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        Ok(self
            .inner
            .get(&keyspace.inner, key)
            .map_err(KvError::from)?
            .map(Bytes::from))
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        fjall_range(&self.inner, keyspace, spec)
    }
}

impl KvRead for FjallTxn {
    type Keyspace = FjallKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        Ok(self
            .inner
            .get(&keyspace.inner, key)
            .map_err(KvError::from)?
            .map(Bytes::from))
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        fjall_range(&self.inner, keyspace, spec)
    }
}

impl KvWrite for FjallTxn {
    fn put(&mut self, keyspace: &Self::Keyspace, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        limits::check_key(key)?;
        limits::check_value(value)?;
        self.budget.record(key.len() + value.len())?;
        self.inner
            .insert(&keyspace.inner, key.to_vec(), value.to_vec());
        self.write_keys
            .push((keyspace.name.to_string(), key.to_vec()));
        Ok(())
    }

    fn delete(&mut self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<(), KvError> {
        limits::check_key(key)?;
        self.budget.record(key.len())?;
        self.inner.remove(&keyspace.inner, key.to_vec());
        self.write_keys
            .push((keyspace.name.to_string(), key.to_vec()));
        Ok(())
    }
}

impl KvBackend for FjallBackend {
    type Keyspace = FjallKeyspace;
    type Snapshot = FjallSnapshot;
    type Txn = FjallTxn;

    fn keyspace(&self, name: &str) -> Result<Self::Keyspace, KvError> {
        if name.is_empty() {
            return Err(KvError::InvalidKeyspaceName(name.to_owned()));
        }
        let mut cache = self
            .inner
            .keyspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(inner) = cache.get(name) {
            return Ok(FjallKeyspace {
                name: Arc::from(name),
                inner: inner.clone(),
            });
        }
        let inner = self
            .inner
            .db
            .keyspace(name, || {
                KeyspaceCreateOptions::default()
                    .with_kv_separation(Some(KvSeparationOptions::default()))
            })
            .map_err(KvError::from)?;
        cache.insert(name.to_owned(), inner.clone());
        Ok(FjallKeyspace {
            name: Arc::from(name),
            inner,
        })
    }

    fn snapshot(&self) -> Self::Snapshot {
        FjallSnapshot {
            inner: self.inner.db.read_tx(),
        }
    }

    fn begin(&self) -> Result<Self::Txn, KvError> {
        let inner = self.inner.db.write_tx().map_err(KvError::from)?;
        Ok(FjallTxn {
            inner,
            write_keys: Vec::new(),
            budget: TxnBudget::default(),
        })
    }

    fn commit(&self, txn: Self::Txn) -> Result<Result<(), Conflict>, KvError> {
        match txn.inner.commit().map_err(KvError::from)? {
            Ok(()) => {
                for (ks, key) in &txn.write_keys {
                    self.inner.hub.notify(ks, key);
                }
                Ok(Ok(()))
            }
            Err(fjall::Conflict) => Ok(Err(Conflict)),
        }
    }

    fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Watch {
        self.inner.hub.watch(&keyspace.name, key)
    }
}
