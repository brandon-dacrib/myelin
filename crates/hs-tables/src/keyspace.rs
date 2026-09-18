//! [`TypedKeyspace`]: an `hs-kv` keyspace handle paired with a [`TupleKey`] shape, so callers
//! read and write typed keys instead of raw bytes.

use std::marker::PhantomData;

use bytes::Bytes;
use hs_kv::{KvError, KvRead, KvWrite, RangeSpec};

use crate::key::{KeyCodecError, TupleKey};

/// An error from the typed layer: either the underlying `hs-kv` backend failed, or a stored key
/// could not be decoded back into `K` (a schema mismatch, or data written by a different table).
#[derive(Debug, thiserror::Error)]
pub enum TableError {
    /// The `hs-kv` backend reported an error.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A stored key did not decode as this table's [`TupleKey`] shape.
    #[error("key did not decode as the table's key shape: {0}")]
    KeyCodec(#[from] KeyCodecError),
}

/// A keyspace typed by its key shape `K`. `Ks` is whichever backend's opaque keyspace handle type
/// (`hs_kv::memory::MemoryKeyspace`, `hs_kv::fjall_backend::FjallKeyspace`, ...); `TypedKeyspace`
/// is generic over it so the same table code runs unchanged against either backend.
///
/// Values are left as raw `hs_kv::Value` (`Bytes`): `hs-tables` encodes keys, not values — value
/// serialization (JSON, a `hs-model` event's canonical bytes, ...) is each table owner's choice.
#[derive(Debug, Clone)]
pub struct TypedKeyspace<Ks, K> {
    keyspace: Ks,
    _key: PhantomData<fn() -> K>,
}

impl<Ks: Clone, K: TupleKey> TypedKeyspace<Ks, K> {
    /// Wraps an already-opened `hs-kv` keyspace handle with a key shape.
    pub fn new(keyspace: Ks) -> Self {
        Self {
            keyspace,
            _key: PhantomData,
        }
    }

    /// The underlying raw keyspace handle, for interop with code that has not adopted typed keys
    /// yet (or for [`index::maintain_index`](crate::index::maintain_index), which writes its own
    /// composite keys directly).
    pub fn raw(&self) -> &Ks {
        &self.keyspace
    }

    /// Reads the value at `key`.
    ///
    /// # Errors
    /// Returns [`TableError::Kv`] on backend failure.
    pub fn get<R: KvRead<Keyspace = Ks>>(
        &self,
        txn: &R,
        key: &K,
    ) -> Result<Option<Bytes>, TableError> {
        Ok(txn.get(&self.keyspace, &key.encode())?)
    }

    /// Reads several keys, preserving order and length (see [`hs_kv::KvRead::multi_get`]).
    ///
    /// # Errors
    /// Returns [`TableError::Kv`] on backend failure.
    pub fn multi_get<R: KvRead<Keyspace = Ks>>(
        &self,
        txn: &R,
        keys: &[K],
    ) -> Result<Vec<Option<Bytes>>, TableError> {
        let encoded: Vec<Vec<u8>> = keys.iter().map(TupleKey::encode).collect();
        let refs: Vec<&[u8]> = encoded.iter().map(Vec::as_slice).collect();
        Ok(txn.multi_get(&self.keyspace, &refs)?)
    }

    /// Writes `key` to `value`.
    ///
    /// # Errors
    /// Returns [`TableError::Kv`] on backend failure (including the `hs-kv` size and transaction
    /// limits).
    pub fn put<W: KvWrite<Keyspace = Ks>>(
        &self,
        txn: &mut W,
        key: &K,
        value: &[u8],
    ) -> Result<(), TableError> {
        Ok(txn.put(&self.keyspace, &key.encode(), value)?)
    }

    /// Deletes `key`. Deleting an absent key is not an error.
    ///
    /// # Errors
    /// Returns [`TableError::Kv`] on backend failure.
    pub fn delete<W: KvWrite<Keyspace = Ks>>(
        &self,
        txn: &mut W,
        key: &K,
    ) -> Result<(), TableError> {
        Ok(txn.delete(&self.keyspace, &key.encode())?)
    }

    /// Scans a raw byte range (see [`hs_kv::RangeSpec`]; [`TypedKeyspace::prefix`] builds one from
    /// a partial key), decoding each key as `K` and each value as raw bytes.
    pub fn range<'a, R: KvRead<Keyspace = Ks>>(
        &self,
        txn: &'a R,
        spec: RangeSpec,
    ) -> impl Iterator<Item = Result<(K, Bytes), TableError>> + 'a
    where
        K: 'a,
    {
        txn.range(&self.keyspace, spec).map(|item| {
            let (k, v) = item?;
            let key = K::decode(&k)?;
            Ok((key, v))
        })
    }

    /// Builds a [`RangeSpec`] matching every full key `K` whose leading components equal
    /// `prefix`'s encoding. `P` is typically a shorter tuple than `K` (for example `(RoomSn,)` as
    /// a prefix of `(RoomSn, EventSn)`) — this is what a "every row for this room" scan is.
    pub fn prefix<P: TupleKey>(prefix: &P) -> RangeSpec {
        RangeSpec::prefix(Bytes::from(prefix.encode()))
    }
}
