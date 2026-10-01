//! The Fjall 3 backend: the embedded, single-node production backend (`PLAN.md` section 6.5).
//! LZ4 block compression (Fjall's default with the `lz4` feature, which this crate enables),
//! key-value separation for large values, and Fjall's own optimistic serializable transactions,
//! which is what makes this backend able to pass the same conformance suite
//! ([`crate::conformance`]) as the in-memory reference.
//!
//! # Every `hs-kv` keyspace in one Fjall keyspace
//!
//! An `hs-kv` keyspace is a *logical* namespace here: all of them share a single Fjall keyspace,
//! [`SHARED_KEYSPACE`], and a key is stored there behind its keyspace's prefix, one length byte
//! and the keyspace's name (`[len][name][key]`, prefix-free because the length comes first).
//! Every read, range and write is translated to that prefix and back, so callers see exactly the
//! independently ordered namespaces the crate contract describes, and a range scan of one
//! keyspace is bounded to its prefix, in the scan and in the transaction's conflict tracking
//! alike.
//!
//! The reason is the cost of creating a Fjall keyspace. Fjall creates one under its keyspace
//! lock, one at a time: a tree directory and manifest written and fsynced, the keyspace's
//! configuration ingested into Fjall's meta keyspace as a new table, and the meta keyspace
//! compacted. That was about sixty milliseconds each on the 2026-09-27 measurement, so the
//! eighty-odd keyspaces `hs serve` opens cost a first boot about five seconds
//! (`docs/status/01-storage-engine.md`; decision 0022). With one shared keyspace a fresh store
//! creates one Fjall keyspace whatever the number of tables, and opening a logical keyspace
//! writes nothing at all. Fjall offers no batched creation and serializes creations under its
//! lock, so neither one sync for many keyspaces nor creating them in parallel was available.
//!
//! A data directory written before this layout keeps working: a name that already exists as a
//! Fjall keyspace of its own is opened as one, unprefixed, as before; only keyspaces that do not
//! exist yet go into the shared one. Nothing is migrated.

use std::collections::HashMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use fjall::{KeyspaceCreateOptions, KvSeparationOptions};

use crate::error::{Conflict, KvError};
use crate::limits::{self, TxnBudget};
use crate::traits::{KvBackend, KvRead, KvWrite, RangeItem, RangeIter, RangeSpec};
use crate::watch::{Hub, Watch};

/// The name of the one Fjall keyspace every `hs-kv` keyspace shares (see the module docs). It
/// cannot itself be used as an `hs-kv` keyspace name.
pub const SHARED_KEYSPACE: &str = "_hs_kv_shared";

/// The longest key Fjall stores (`lsm-tree` encodes a key's length as a `u16`).
const FJALL_MAX_KEY: usize = u16::MAX as usize;

/// What the keyspace lock guards: the shared Fjall keyspace once opened, and every `hs-kv`
/// keyspace handed out so far.
#[derive(Default)]
struct Keyspaces {
    shared: Option<fjall::OptimisticTxKeyspace>,
    opened: HashMap<String, FjallKeyspace>,
}

struct Inner {
    db: fjall::OptimisticTxDatabase,
    hub: Hub,
    keyspaces: Mutex<Keyspaces>,
    fresh: bool,
    created: AtomicUsize,
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
        let path = path.as_ref();
        // Fjall writes its version marker when it creates a database, and recovers one whenever
        // the marker is there (`fjall::Database::create_or_recover`).
        let fresh = !path
            .join("version")
            .try_exists()
            .map_err(KvError::backend)?;
        let db = fjall::OptimisticTxDatabase::builder(path)
            .open()
            .map_err(KvError::from)?;
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                hub: Hub::new(),
                keyspaces: Mutex::new(Keyspaces::default()),
                fresh,
                created: AtomicUsize::new(0),
            }),
        })
    }

    /// Whether [`FjallBackend::open`] created the database rather than recovering one: `true`
    /// on the very first boot over a data directory.
    #[must_use]
    pub fn created_fresh(&self) -> bool {
        self.inner.fresh
    }

    /// How many Fjall keyspaces this handle has created: the expensive, fsynced step of a first
    /// boot (see the module docs). At most one, the shared keyspace, on a fresh store; zero on
    /// every later open.
    #[must_use]
    pub fn fjall_keyspaces_created(&self) -> usize {
        self.inner.created.load(Ordering::Relaxed)
    }

    /// How many distinct `hs-kv` keyspaces have been opened through this handle.
    #[must_use]
    pub fn keyspaces_opened(&self) -> usize {
        self.lock_keyspaces().opened.len()
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

    fn lock_keyspaces(&self) -> std::sync::MutexGuard<'_, Keyspaces> {
        self.inner
            .keyspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Opens a Fjall keyspace by name, counting it if this call created it.
    fn fjall_keyspace(&self, name: &str) -> Result<fjall::OptimisticTxKeyspace, KvError> {
        let existed = self.inner.db.keyspace_exists(name);
        let keyspace = self
            .inner
            .db
            .keyspace(name, || {
                KeyspaceCreateOptions::default()
                    .with_kv_separation(Some(KvSeparationOptions::default()))
            })
            .map_err(KvError::from)?;
        if !existed {
            self.inner.created.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(keyspace = name, "created a Fjall keyspace");
        }
        Ok(keyspace)
    }
}

impl From<fjall::Error> for KvError {
    fn from(err: fjall::Error) -> Self {
        KvError::backend(err)
    }
}

/// A handle to one `hs-kv` keyspace (one table): a prefix in the shared Fjall keyspace, or a
/// Fjall keyspace of its own in a data directory written before the shared layout.
#[derive(Clone)]
pub struct FjallKeyspace {
    name: Arc<str>,
    inner: fjall::OptimisticTxKeyspace,
    /// `[len][name]` in the shared keyspace; empty for a keyspace of its own.
    prefix: Bytes,
}

impl std::fmt::Debug for FjallKeyspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FjallKeyspace")
            .field("name", &self.name)
            .field("shared", &!self.prefix.is_empty())
            .finish()
    }
}

impl FjallKeyspace {
    /// The key as Fjall stores it, or `None` when the prefixed key would be longer than Fjall
    /// can store (so it cannot exist).
    fn stored_key(&self, key: &[u8]) -> Option<Vec<u8>> {
        if self.prefix.len() + key.len() > FJALL_MAX_KEY {
            return None;
        }
        let mut stored = Vec::with_capacity(self.prefix.len() + key.len());
        stored.extend_from_slice(&self.prefix);
        stored.extend_from_slice(key);
        Some(stored)
    }

    /// A key written through this keyspace, checked against both `hs-kv`'s limit and what Fjall
    /// can store once prefixed.
    fn writable_key(&self, key: &[u8]) -> Result<Vec<u8>, KvError> {
        limits::check_key(key)?;
        self.stored_key(key).ok_or(KvError::KeyTooLarge {
            len: key.len(),
            limit: FJALL_MAX_KEY - self.prefix.len(),
        })
    }

    /// A range over this keyspace's keys as a range over the stored keys: bounded to the prefix
    /// at whichever end the caller left open.
    fn stored_range(&self, start: Bound<Bytes>, end: Bound<Bytes>) -> (Bound<Bytes>, Bound<Bytes>) {
        if self.prefix.is_empty() {
            return (start, end);
        }
        let prefixed = |key: &Bytes| {
            let mut stored = Vec::with_capacity(self.prefix.len() + key.len());
            stored.extend_from_slice(&self.prefix);
            stored.extend_from_slice(key);
            Bytes::from(stored)
        };
        let start = match start {
            Bound::Included(k) => Bound::Included(prefixed(&k)),
            Bound::Excluded(k) => Bound::Excluded(prefixed(&k)),
            Bound::Unbounded => Bound::Included(self.prefix.clone()),
        };
        let end = match end {
            Bound::Included(k) => Bound::Included(prefixed(&k)),
            Bound::Excluded(k) => Bound::Excluded(prefixed(&k)),
            Bound::Unbounded => RangeSpec::prefix(self.prefix.clone()).end,
        };
        (start, end)
    }
}

/// The prefix of the keyspace `name` in the shared Fjall keyspace.
fn shared_prefix(name: &str) -> Result<Bytes, KvError> {
    let len =
        u8::try_from(name.len()).map_err(|_| KvError::InvalidKeyspaceName(name.to_owned()))?;
    let mut prefix = Vec::with_capacity(1 + name.len());
    prefix.push(len);
    prefix.extend_from_slice(name.as_bytes());
    Ok(Bytes::from(prefix))
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

fn fjall_get<R: fjall::Readable>(
    source: &R,
    ks: &FjallKeyspace,
    key: &[u8],
) -> Result<Option<Bytes>, KvError> {
    let Some(stored) = ks.stored_key(key) else {
        return Ok(None);
    };
    Ok(source
        .get(&ks.inner, stored)
        .map_err(KvError::from)?
        .map(Bytes::from))
}

fn fjall_range<'a, R: fjall::Readable>(
    source: &'a R,
    ks: &FjallKeyspace,
    spec: RangeSpec,
) -> RangeIter<'a> {
    let strip = ks.prefix.len();
    let item = move |guard: fjall::Guard| -> RangeItem {
        guard
            .into_inner()
            .map(|(k, v)| (Bytes::from(k).slice(strip..), Bytes::from(v)))
            .map_err(KvError::from)
    };
    let iter = source.range(&ks.inner, ks.stored_range(spec.start, spec.end));
    let mut mapped: RangeIter<'a> = if spec.reverse {
        Box::new(iter.rev().map(item))
    } else {
        Box::new(iter.map(item))
    };
    if let Some(limit) = spec.limit {
        mapped = Box::new(mapped.take(limit));
    }
    mapped
}

impl KvRead for FjallSnapshot {
    type Keyspace = FjallKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        fjall_get(&self.inner, keyspace, key)
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        fjall_range(&self.inner, keyspace, spec)
    }
}

impl KvRead for FjallTxn {
    type Keyspace = FjallKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        fjall_get(&self.inner, keyspace, key)
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        fjall_range(&self.inner, keyspace, spec)
    }
}

impl KvWrite for FjallTxn {
    fn put(&mut self, keyspace: &Self::Keyspace, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        let stored = keyspace.writable_key(key)?;
        limits::check_value(value)?;
        self.budget.record(key.len() + value.len())?;
        self.inner.insert(&keyspace.inner, stored, value.to_vec());
        self.write_keys
            .push((keyspace.name.to_string(), key.to_vec()));
        Ok(())
    }

    fn delete(&mut self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<(), KvError> {
        let stored = keyspace.writable_key(key)?;
        self.budget.record(key.len())?;
        self.inner.remove(&keyspace.inner, stored);
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
        if name.is_empty() || name == SHARED_KEYSPACE {
            return Err(KvError::InvalidKeyspaceName(name.to_owned()));
        }
        let mut keyspaces = self.lock_keyspaces();
        if let Some(handle) = keyspaces.opened.get(name) {
            return Ok(handle.clone());
        }
        let handle = if self.inner.db.keyspace_exists(name) {
            // Written before the shared layout: keep reading and writing it where it is.
            FjallKeyspace {
                name: Arc::from(name),
                inner: self.fjall_keyspace(name)?,
                prefix: Bytes::new(),
            }
        } else {
            let prefix = shared_prefix(name)?;
            let shared = match &keyspaces.shared {
                Some(shared) => shared.clone(),
                None => {
                    let shared = self.fjall_keyspace(SHARED_KEYSPACE)?;
                    keyspaces.shared = Some(shared.clone());
                    shared
                }
            };
            FjallKeyspace {
                name: Arc::from(name),
                inner: shared,
                prefix,
            }
        };
        keyspaces.opened.insert(name.to_owned(), handle.clone());
        Ok(handle)
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
