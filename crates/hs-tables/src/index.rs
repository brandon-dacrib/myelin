//! Declarative composite secondary indexes, maintained inside the same transaction as the primary
//! row they index — the antidote to the Conduit lineage's chronic hand-maintained-index bugs that
//! `PLAN.md` section 4 (D1) calls out by name.
//!
//! An [`IndexDef`] names an index keyspace, whether it is unique, and how to derive an index key
//! from a primary key and a row's raw value. [`maintain_index`] is called once per index, inside
//! the same write transaction as the primary-row mutation, with the row's value *before* and
//! *after* the mutation (`None` for "row did not exist" / "row is being deleted"); it computes the
//! diff and applies exactly the index writes needed — no stale entries left behind on an update
//! that changes the indexed field, no missing entries on insert, no orphans on delete. The property
//! tests in `tests/index_proptest.rs` are the executable proof, generated over random sequences of
//! insert, update and delete.
//!
//! # Storage scheme
//!
//! An index keyspace stores rows keyed by the composite tuple `(index_key, primary_key)` with an
//! empty value; there is no separate "value" to keep in sync. This works identically for unique
//! and non-unique indexes: non-unique lookups are a prefix scan over `index_key`'s encoding that
//! yields every matching `primary_key`; unique lookups are the same scan, expected to yield at
//! most one entry, and [`maintain_index`] itself performs that scan before inserting to reject a
//! second primary key claiming an index value already taken by a different one.
//!
//! # Example
//!
//! ```
//! use bytes::Bytes;
//! use hs_kv::memory::MemoryBackend;
//! use hs_kv::{transact, KvBackend, KvWrite, TransactConfig};
//! use hs_tables::index::{maintain_index, IndexDef};
//!
//! let backend = MemoryBackend::new();
//! let rooms = backend.keyspace("rooms").unwrap();
//! let by_alias = backend.keyspace("rooms_by_alias").unwrap();
//!
//! // Index on a room's canonical alias, taken from the first line of its (toy) value encoding.
//! let alias_index: IndexDef<_, (u32,), (String,)> = IndexDef::new(by_alias, true, |_pk, value| {
//!     std::str::from_utf8(value).ok().map(|s| (s.to_owned(),))
//! });
//!
//! transact(&backend, TransactConfig::default(), |txn| {
//!     let pk = (1u32,);
//!     txn.put(&rooms, &hs_tables::key::encode(&pk), b"#general")?;
//!     maintain_index(txn, &alias_index, &pk, None, Some(b"#general")).unwrap();
//!     Ok(())
//! })
//! .unwrap();
//! ```

use hs_kv::{KvError, KvWrite, RangeSpec};

use crate::key::{KeyCodecError, TupleKey};

/// An error from index maintenance.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The `hs-kv` backend reported an error.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A stored composite index key did not decode as `(IK, K)`.
    #[error(transparent)]
    KeyCodec(#[from] KeyCodecError),
    /// A unique index's derived key is already owned by a different primary key.
    #[error("unique index violated: this index key is already used by a different row")]
    UniqueConflict,
}

type DeriveFn<K, IK> = Box<dyn Fn(&K, &[u8]) -> Option<IK> + Send + Sync>;

/// A declarative secondary index: an index keyspace, a uniqueness flag, and how to derive an
/// index key `IK` from a primary key `K` and a row's raw value. See the module docs.
pub struct IndexDef<Ks, K, IK> {
    keyspace: Ks,
    unique: bool,
    derive: DeriveFn<K, IK>,
}

impl<Ks, K, IK> IndexDef<Ks, K, IK>
where
    K: TupleKey,
    IK: TupleKey,
{
    /// Declares an index. `derive` returns `None` when a row should not appear in the index at
    /// all (a partial index over rows that have some optional field set, for example).
    pub fn new(
        keyspace: Ks,
        unique: bool,
        derive: impl Fn(&K, &[u8]) -> Option<IK> + Send + Sync + 'static,
    ) -> Self {
        Self {
            keyspace,
            unique,
            derive: Box::new(derive),
        }
    }

    /// The index's own raw keyspace handle.
    pub fn keyspace(&self) -> &Ks {
        &self.keyspace
    }

    /// Whether this index rejects a second primary key mapping to the same index key.
    #[must_use]
    pub fn is_unique(&self) -> bool {
        self.unique
    }
}

/// Brings `index` up to date with a primary row's mutation, inside `txn`.
///
/// Pass `old_value: None` for an insert (the row did not exist before), `new_value: None` for a
/// delete (the row no longer exists), and both `Some` for an update. Passing the row's full raw
/// value both times (even when most fields did not change) is intentional: `maintain_index` only
/// needs whatever `IndexDef`'s `derive` closure reads out of it.
///
/// # Errors
/// Returns [`IndexError::UniqueConflict`] if `index` is unique and the derived key from
/// `new_value` already belongs to a different primary key. Returns [`IndexError::Kv`] or
/// [`IndexError::KeyCodec`] on backend or decode failure.
pub fn maintain_index<Ks, K, IK, W>(
    txn: &mut W,
    index: &IndexDef<Ks, K, IK>,
    pk: &K,
    old_value: Option<&[u8]>,
    new_value: Option<&[u8]>,
) -> Result<(), IndexError>
where
    K: TupleKey + Clone + PartialEq,
    IK: TupleKey + Clone + PartialEq,
    W: KvWrite<Keyspace = Ks>,
{
    let old_key = old_value.and_then(|v| (index.derive)(pk, v));
    let new_key = new_value.and_then(|v| (index.derive)(pk, v));

    if old_key == new_key {
        // Nothing indexable changed (including the common case of neither value producing an
        // index entry at all): no index write needed.
        return Ok(());
    }

    if let Some(old_key) = &old_key {
        let composite = (old_key.clone(), pk.clone()).encode();
        txn.delete(&index.keyspace, &composite)?;
    }

    if let Some(new_key) = &new_key {
        if index.unique {
            let scan = RangeSpec::prefix(new_key.encode());
            for item in txn.range(&index.keyspace, scan) {
                let (raw_key, _value) = item?;
                let (_index_key, existing_pk): (IK, K) = TupleKey::decode(&raw_key)?;
                if existing_pk != *pk {
                    return Err(IndexError::UniqueConflict);
                }
            }
        }
        let composite = (new_key.clone(), pk.clone()).encode();
        txn.put(&index.keyspace, &composite, &[])?;
    }

    Ok(())
}

/// Looks up every primary key currently mapped to `index_key` in `index` (for a unique index,
/// there is at most one).
///
/// # Errors
/// Returns [`IndexError::Kv`] or [`IndexError::KeyCodec`] on backend or decode failure.
pub fn lookup<Ks, K, IK, R>(
    txn: &R,
    index: &IndexDef<Ks, K, IK>,
    index_key: &IK,
) -> Result<Vec<K>, IndexError>
where
    K: TupleKey,
    IK: TupleKey,
    R: hs_kv::KvRead<Keyspace = Ks>,
{
    let scan = RangeSpec::prefix(index_key.encode());
    let mut out = Vec::new();
    for item in txn.range(&index.keyspace, scan) {
        let (raw_key, _value) = item?;
        let (_index_key, pk): (IK, K) = TupleKey::decode(&raw_key)?;
        out.push(pk);
    }
    Ok(out)
}
