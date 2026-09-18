//! The `hs-kv` trait v0: snapshot reads, transactions, range scans, multi-get, atomic add and
//! watches. See the crate-level docs for the semantic contract these traits must uphold.

use std::ops::Bound;

use bytes::Bytes;

use crate::error::KvError;

/// A key or value byte string. `hs-kv` never interprets the bytes; ordering is plain
/// lexicographic (unsigned byte-wise) comparison. `hs-tables` builds order-preserving typed keys
/// on top of that.
pub type Key = Bytes;
/// See [`Key`].
pub type Value = Bytes;

/// One row from a range scan.
pub type KvPair = (Key, Value);

/// A single range item as produced by [`KvRead::range`]: `Ok((key, value))`, or an error if the
/// backend failed partway through the scan (for example a decode error on an on-disk block).
/// Once an iterator yields an `Err`, callers should stop pulling from it; the backend does not
/// guarantee it can resume cleanly.
pub type RangeItem = Result<KvPair, KvError>;

/// A boxed iterator over a range scan, always yielding in scan order (which end it started from
/// is [`RangeSpec::reverse`]'s job, not the iterator's — there is no `.rev()` on the result).
pub type RangeIter<'a> = Box<dyn Iterator<Item = RangeItem> + Send + 'a>;

/// A bounded, orderable range over keys, as consumed by [`KvRead::range`].
///
/// Construct with [`RangeSpec::new`] plus [`RangeSpec::reverse`] / [`RangeSpec::limit`], or with
/// the `From<R: RangeBounds<Key>>` conversion for the common forward, unbounded-limit case.
#[derive(Debug, Clone)]
pub struct RangeSpec {
    /// Lower bound.
    pub start: Bound<Key>,
    /// Upper bound.
    pub end: Bound<Key>,
    /// If true, scan from `end` towards `start`. The first item yielded is then the greatest key
    /// in range, matching what `range(..).rev()` would produce, not two independent scans.
    pub reverse: bool,
    /// Stop after this many items. `None` means unbounded (callers should generally set one;
    /// see the crate-level contract on avoiding unbounded scans in production paths).
    pub limit: Option<usize>,
}

impl RangeSpec {
    /// A forward, unbounded-limit range between `start` and `end`.
    #[must_use]
    pub fn new(start: Bound<Key>, end: Bound<Key>) -> Self {
        Self {
            start,
            end,
            reverse: false,
            limit: None,
        }
    }

    /// The whole keyspace.
    #[must_use]
    pub fn full() -> Self {
        Self::new(Bound::Unbounded, Bound::Unbounded)
    }

    /// All keys with the given prefix.
    #[must_use]
    pub fn prefix(prefix: impl Into<Key>) -> Self {
        let prefix = prefix.into();
        let mut upper = prefix.to_vec();
        // Find the successor of `prefix` under byte-wise order: increment the last byte that is
        // not 0xFF, dropping any trailing 0xFF bytes. If the whole prefix is 0xFF bytes (or
        // empty), there is no finite successor and the range is open-ended above.
        let end = loop {
            match upper.pop() {
                None => break Bound::Unbounded,
                Some(0xFF) => {}
                Some(b) => {
                    upper.push(b + 1);
                    break Bound::Excluded(Bytes::from(upper));
                }
            }
        };
        Self::new(Bound::Included(prefix), end)
    }

    /// Scans from the high end down to the low end.
    #[must_use]
    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    /// Caps the number of items returned.
    #[must_use]
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
}

/// Read operations available on a snapshot ([`KvBackend::snapshot`]) and inside a write
/// transaction ([`KvBackend::Txn`]).
///
/// `Keyspace` is an opaque handle from [`KvBackend::keyspace`]; every method takes one explicitly
/// because a single transaction commonly touches several keyspaces (for example a table and its
/// secondary indexes) and must see them all at the same snapshot / commit atomically.
pub trait KvRead {
    /// The keyspace handle type this reader's backend uses.
    type Keyspace;

    /// Point read. Returns `None` if the key is absent.
    ///
    /// # Errors
    /// Returns [`KvError`] on backend I/O failure.
    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Value>, KvError>;

    /// Reads several keys, preserving the input order and length: `result[i]` answers
    /// `keys[i]`. The default implementation loops over [`KvRead::get`]; backends that can batch
    /// (a pipelined `= ANY($1)` in PostgreSQL, one skip-list walk in an LSM) should override it.
    ///
    /// # Errors
    /// Returns [`KvError`] on backend I/O failure.
    fn multi_get(
        &self,
        keyspace: &Self::Keyspace,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Value>>, KvError> {
        keys.iter().map(|k| self.get(keyspace, k)).collect()
    }

    /// Range scan. See [`RangeSpec`] for bounds, direction and limit.
    ///
    /// Ordering is always plain byte-wise order on the raw key bytes; `hs-tables`'s tuple
    /// encoding is what makes that ordering meaningful for typed keys.
    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a>;
}

/// Write operations available inside a write transaction.
pub trait KvWrite: KvRead {
    /// Inserts or overwrites a key.
    ///
    /// # Errors
    /// Returns [`KvError::KeyTooLarge`], [`KvError::ValueTooLarge`],
    /// [`KvError::TransactionTooLarge`], or a backend error.
    fn put(&mut self, keyspace: &Self::Keyspace, key: &[u8], value: &[u8]) -> Result<(), KvError>;

    /// Deletes a key. Deleting an absent key is not an error.
    ///
    /// # Errors
    /// Returns [`KvError::TransactionTooLarge`] or a backend error.
    fn delete(&mut self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<(), KvError>;

    /// Atomically reads an 8-byte big-endian `i64` counter at `key` (treating an absent key as
    /// `0`), adds `delta`, writes the result back, and returns the new value.
    ///
    /// This is equivalent to, and implemented in terms of, a read of `key` (added to the
    /// transaction's read set, so a concurrent writer of the same key causes a conflict) followed
    /// by a write — it is not a special lock-free primitive, it is serializability doing the
    /// work. Concurrent `atomic_add` calls on the same key from different transactions serialize:
    /// exactly one wins per round, the rest see [`Conflict`](crate::Conflict) and must retry
    /// (`hs_kv::transact` does this automatically).
    ///
    /// # Errors
    /// Returns [`KvError::Backend`] if the existing value is not a valid 8-byte counter, or
    /// [`KvError::TransactionTooLarge`].
    fn atomic_add(
        &mut self,
        keyspace: &Self::Keyspace,
        key: &[u8],
        delta: i64,
    ) -> Result<i64, KvError> {
        let current = match self.get(keyspace, key)? {
            None => 0i64,
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_ref()
                    .try_into()
                    .map_err(|_| KvError::backend(InvalidCounterError { len: bytes.len() }))?;
                i64::from_be_bytes(arr)
            }
        };
        let next = current.wrapping_add(delta);
        self.put(keyspace, key, &next.to_be_bytes())?;
        Ok(next)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("counter value is {len} bytes, expected 8")]
struct InvalidCounterError {
    len: usize,
}

/// A handle to an opaque backend keyspace (one Fjall keyspace, one PostgreSQL table, one
/// in-memory `BTreeMap`). Keyspace handles are cheap to clone and carry no borrow on the backend.
pub trait KeyspaceHandle: Clone + Send + Sync + std::fmt::Debug + 'static {}
impl<T: Clone + Send + Sync + std::fmt::Debug + 'static> KeyspaceHandle for T {}

/// The ordered, transactional key-value abstraction every backend implements.
///
/// A `KvBackend` is a cheap-to-clone handle (an `Arc` internally); cloning it does not open a new
/// connection or duplicate cached state. See the crate-level docs for the full semantic contract.
pub trait KvBackend: Clone + Send + Sync + 'static {
    /// Opaque keyspace handle.
    type Keyspace: KeyspaceHandle;
    /// A read-only, point-in-time snapshot.
    type Snapshot: KvRead<Keyspace = Self::Keyspace> + Send + Sync;
    /// A read-write, serializable transaction.
    type Txn: KvWrite<Keyspace = Self::Keyspace> + Send;

    /// Opens (creating if necessary) the named keyspace. Keyspace names are stable identifiers
    /// chosen by `hs-tables`, not user data; backends may restrict their charset and length (see
    /// the crate-level contract).
    ///
    /// # Errors
    /// Returns [`KvError::InvalidKeyspaceName`] or a backend error.
    fn keyspace(&self, name: &str) -> Result<Self::Keyspace, KvError>;

    /// Opens a read-only snapshot: a consistent, point-in-time view of every keyspace, established
    /// at the moment this call returns and unaffected by writes committed afterwards.
    fn snapshot(&self) -> Self::Snapshot;

    /// Begins a new read-write transaction. The transaction sees a snapshot taken at this call
    /// (repeatable read) and additionally validates, at commit time, that nothing it read has
    /// changed since (serializable — see the crate-level contract).
    ///
    /// # Errors
    /// Returns a backend error if a transaction could not be started (for example, a connection
    /// pool is exhausted).
    fn begin(&self) -> Result<Self::Txn, KvError>;

    /// Commits a transaction. `Ok(Ok(()))` means it committed; `Ok(Err(Conflict))` means it lost a
    /// serializability race and every mutation in it was discarded — the caller must rebuild and
    /// retry the transaction from scratch (do not just re-call `commit`). Prefer
    /// [`crate::transact`], which does this for you.
    ///
    /// # Errors
    /// Returns a backend error (as opposed to a conflict) if the commit could not be attempted at
    /// all (for example, an I/O failure writing the journal).
    fn commit(&self, txn: Self::Txn) -> Result<Result<(), crate::Conflict>, KvError>;

    /// Registers interest in a key, for best-effort wake-ups. See the crate-level contract:
    /// watches are hints, never a substitute for reading the key inside a transaction.
    fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> crate::watch::Watch;
}
