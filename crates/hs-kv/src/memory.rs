//! The in-memory backend: a reference implementation of the full [`crate::KvBackend`] contract,
//! including real serializable-snapshot-isolation conflict detection (not just "last writer
//! wins"). It is not a performance backend — reads and range scans clone data rather than
//! sharing structure — but it is the backend every other track's unit tests run against, and the
//! backend the conformance suite ([`crate::conformance`]) was written against first, so its
//! isolation behavior is the reference the Fjall and (later) PostgreSQL backends are checked
//! against.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::{Bound, RangeBounds};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::error::{Conflict, KvError};
use crate::limits::{self, TxnBudget};
use crate::traits::{KvBackend, KvPair, KvRead, KvWrite, RangeIter, RangeSpec};
use crate::watch::{Hub, Watch};

type KeyspaceData = Arc<BTreeMap<Vec<u8>, Bytes>>;
/// Per-keyspace pending writes for one transaction: `None` marks a delete.
type WriteBuffer = HashMap<String, BTreeMap<Vec<u8>, Option<Bytes>>>;
/// `(keyspace, key)` read within a transaction.
type ReadKey = (String, Vec<u8>);
/// `(keyspace, start, end)` range read within a transaction.
type ReadRange = (String, Bound<Vec<u8>>, Bound<Vec<u8>>);

#[derive(Default)]
struct State {
    commit_seq: u64,
    keyspaces: HashMap<String, KeyspaceData>,
    /// `(seq the write committed at, keyspace, key)`, oldest first. Never garbage collected: the
    /// in-memory backend is a test and reference double for short-lived processes, not a
    /// long-running store, so unbounded growth across a process lifetime is an accepted
    /// trade-off for a simple, obviously-correct conflict check. See the crate docs.
    write_log: Vec<(u64, String, Vec<u8>)>,
}

struct Inner {
    state: Mutex<State>,
    hub: Hub,
}

/// The in-memory [`KvBackend`]. Cloning shares the underlying store (it is an `Arc` handle), so
/// `backend.clone()` gives a second handle to the same data — exactly like opening a second
/// connection to the same Fjall database.
#[derive(Clone)]
pub struct MemoryBackend {
    inner: Arc<Inner>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    /// Creates a fresh, empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                hub: Hub::new(),
            }),
        }
    }
}

/// A handle to one in-memory keyspace.
#[derive(Debug, Clone)]
pub struct MemoryKeyspace {
    name: Arc<str>,
}

impl MemoryKeyspace {
    fn as_str(&self) -> &str {
        &self.name
    }
}

/// A read-only, point-in-time view of every keyspace.
pub struct MemorySnapshot {
    data: HashMap<String, KeyspaceData>,
}

/// A read-write, serializable transaction over the in-memory backend.
pub struct MemoryTxn {
    start_seq: u64,
    base: HashMap<String, KeyspaceData>,
    write_buffer: WriteBuffer,
    read_keys: std::cell::RefCell<HashSet<ReadKey>>,
    read_ranges: std::cell::RefCell<Vec<ReadRange>>,
    budget: TxnBudget,
}

fn read_from(
    base: &HashMap<String, KeyspaceData>,
    write_buffer: &WriteBuffer,
    ks: &str,
    key: &[u8],
) -> Option<Bytes> {
    if let Some(overlay) = write_buffer.get(ks)
        && let Some(entry) = overlay.get(key)
    {
        return entry.clone();
    }
    base.get(ks).and_then(|m| m.get(key).cloned())
}

fn range_from<'a>(
    base: &HashMap<String, KeyspaceData>,
    write_buffer: Option<&WriteBuffer>,
    ks: &str,
    spec: RangeSpec,
) -> RangeIter<'a> {
    let start = spec.start.map(|b| b.to_vec());
    let end = spec.end.map(|b| b.to_vec());
    let mut merged: BTreeMap<Vec<u8>, Bytes> = BTreeMap::new();
    if let Some(base_map) = base.get(ks) {
        for (k, v) in base_map.range((start.clone(), end.clone())) {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Some(write_buffer) = write_buffer
        && let Some(overlay) = write_buffer.get(ks)
    {
        for (k, opt_v) in overlay.range((start.clone(), end.clone())) {
            match opt_v {
                Some(v) => {
                    merged.insert(k.clone(), v.clone());
                }
                None => {
                    merged.remove(k);
                }
            }
        }
    }
    let mut items: Vec<KvPair> = merged
        .into_iter()
        .map(|(k, v)| (Bytes::from(k), v))
        .collect();
    if spec.reverse {
        items.reverse();
    }
    if let Some(limit) = spec.limit {
        items.truncate(limit);
    }
    Box::new(items.into_iter().map(Ok))
}

impl KvRead for MemorySnapshot {
    type Keyspace = MemoryKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        Ok(self
            .data
            .get(keyspace.as_str())
            .and_then(|m| m.get(key).cloned()))
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        range_from(&self.data, None, keyspace.as_str(), spec)
    }
}

impl KvRead for MemoryTxn {
    type Keyspace = MemoryKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        self.read_keys
            .borrow_mut()
            .insert((keyspace.as_str().to_owned(), key.to_vec()));
        Ok(read_from(
            &self.base,
            &self.write_buffer,
            keyspace.as_str(),
            key,
        ))
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        self.read_ranges.borrow_mut().push((
            keyspace.as_str().to_owned(),
            spec.start.clone().map(|b| b.to_vec()),
            spec.end.clone().map(|b| b.to_vec()),
        ));
        range_from(
            &self.base,
            Some(&self.write_buffer),
            keyspace.as_str(),
            spec,
        )
    }
}

impl KvWrite for MemoryTxn {
    fn put(&mut self, keyspace: &Self::Keyspace, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        limits::check_key(key)?;
        limits::check_value(value)?;
        self.budget.record(key.len() + value.len())?;
        self.write_buffer
            .entry(keyspace.as_str().to_owned())
            .or_default()
            .insert(key.to_vec(), Some(Bytes::copy_from_slice(value)));
        Ok(())
    }

    fn delete(&mut self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<(), KvError> {
        limits::check_key(key)?;
        self.budget.record(key.len())?;
        self.write_buffer
            .entry(keyspace.as_str().to_owned())
            .or_default()
            .insert(key.to_vec(), None);
        Ok(())
    }
}

impl KvBackend for MemoryBackend {
    type Keyspace = MemoryKeyspace;
    type Snapshot = MemorySnapshot;
    type Txn = MemoryTxn;

    fn keyspace(&self, name: &str) -> Result<Self::Keyspace, KvError> {
        if name.is_empty() {
            return Err(KvError::InvalidKeyspaceName(name.to_owned()));
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .keyspaces
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(BTreeMap::new()));
        Ok(MemoryKeyspace {
            name: Arc::from(name),
        })
    }

    fn snapshot(&self) -> Self::Snapshot {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        MemorySnapshot {
            data: state.keyspaces.clone(),
        }
    }

    fn begin(&self) -> Result<Self::Txn, KvError> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(MemoryTxn {
            start_seq: state.commit_seq,
            base: state.keyspaces.clone(),
            write_buffer: HashMap::new(),
            read_keys: std::cell::RefCell::new(HashSet::new()),
            read_ranges: std::cell::RefCell::new(Vec::new()),
            budget: TxnBudget::default(),
        })
    }

    fn commit(&self, txn: Self::Txn) -> Result<Result<(), Conflict>, KvError> {
        if txn.write_buffer.values().all(BTreeMap::is_empty) {
            // Read-only: nothing to validate or apply, matches the Fjall backend's behavior.
            return Ok(Ok(()));
        }

        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let read_keys = txn.read_keys.borrow();
        let read_ranges = txn.read_ranges.borrow();
        for (seq, wks, wkey) in &state.write_log {
            if *seq <= txn.start_seq {
                continue;
            }
            if read_keys.contains(&(wks.clone(), wkey.clone())) {
                return Ok(Err(Conflict));
            }
            for (rks, start, end) in read_ranges.iter() {
                if rks == wks && (start.clone(), end.clone()).contains(wkey) {
                    return Ok(Err(Conflict));
                }
            }
        }
        drop(read_keys);
        drop(read_ranges);

        state.commit_seq += 1;
        let seq = state.commit_seq;
        let mut notifications = Vec::new();
        for (ks, ops) in &txn.write_buffer {
            if ops.is_empty() {
                continue;
            }
            let mut next = (**state
                .keyspaces
                .entry(ks.clone())
                .or_insert_with(|| Arc::new(BTreeMap::new())))
            .clone();
            for (key, value) in ops {
                match value {
                    Some(v) => {
                        next.insert(key.clone(), v.clone());
                    }
                    None => {
                        next.remove(key);
                    }
                }
                state.write_log.push((seq, ks.clone(), key.clone()));
                notifications.push((ks.clone(), key.clone()));
            }
            state.keyspaces.insert(ks.clone(), Arc::new(next));
        }
        drop(state);

        for (ks, key) in notifications {
            self.inner.hub.notify(&ks, &key);
        }

        Ok(Ok(()))
    }

    fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Watch {
        self.inner.hub.watch(keyspace.as_str(), key)
    }
}
