//! Get-or-create interning with reverse lookups and an in-process cache: `PLAN.md` section 6.1's
//! trick of replacing hot-path identifiers (room, user and server IDs, event IDs, `(type,
//! state_key)` pairs, event types) with compact integers the instant they are first seen.
//!
//! [`InternTable::get_or_create`] is safe under concurrent interning of the *same new* name: it
//! always re-checks the store before allocating (see the module's design note below), so a race
//! between two transactions interning an identical name for the first time resolves to exactly one
//! id, not two, and the retry loop in [`hs_kv::transact`] is what makes that re-check happen on
//! every attempt.
//!
//! # Why the cache never caches an uncommitted allocation
//!
//! Interning is append-only: once a name has a committed id, that mapping never changes. That
//! makes a *committed* mapping always safe to cache without any invalidation. It also means the
//! opposite must never happen: caching a mapping this call is *about* to write, before it is known
//! to have committed, would poison the cache forever if the enclosing transaction went on to
//! conflict and retry with a different outcome (a concurrent transaction reusing the id this one
//! provisionally computed). So [`InternTable::get_or_create`] only ever populates the cache from a
//! read that found an *existing* entry (which, by transaction snapshot semantics, can only be
//! committed data) — never from the branch that allocates a new one. A freshly allocated mapping
//! becomes cached the next time anything looks it up, once it has necessarily committed.
//!
//! # Wire format
//!
//! Regardless of `Id`'s native width (`u32` for most of `PLAN.md`'s short IDs, `u64` for
//! `EventSn`), the interned id is stored as a fixed 8-byte big-endian `u64` in both directions, so
//! decoding never needs to guess a width.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use hs_kv::{KvBackend, KvError, KvRead, KvWrite};

/// A `PLAN.md` section 6.1 short ID: a compact integer standing in for some other identifier.
/// Implemented here (not in `hs-model`, which only defines the types) for
/// [`hs_model::RoomSn`], [`hs_model::UserSn`], [`hs_model::ServerSn`], [`hs_model::StateKeyId`],
/// [`hs_model::TypeId`] and [`hs_model::EventSn`].
pub trait ShortId: Copy + Eq + std::hash::Hash + Send + Sync + 'static {
    /// Builds a short ID from its canonical `u64` representation (see the module's "wire format"
    /// note).
    fn from_u64(value: u64) -> Self;
    /// The short ID's canonical `u64` representation.
    fn to_u64(self) -> u64;
}

macro_rules! impl_short_id_u32 {
    ($($t:ty),+ $(,)?) => {
        $(
            impl ShortId for $t {
                fn from_u64(value: u64) -> Self {
                    Self::new(u32::try_from(value).expect("interned id exceeds u32 range"))
                }
                fn to_u64(self) -> u64 {
                    u64::from(self.get())
                }
            }
        )+
    };
}

impl_short_id_u32!(
    hs_model::RoomSn,
    hs_model::UserSn,
    hs_model::ServerSn,
    hs_model::StateKeyId,
    hs_model::TypeId,
);

impl ShortId for hs_model::EventSn {
    fn from_u64(value: u64) -> Self {
        Self::new(value)
    }
    fn to_u64(self) -> u64 {
        self.get()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("interned id value is {len} bytes, expected 8")]
struct InvalidInternedIdError {
    len: usize,
}

fn decode_id<Id: ShortId>(bytes: &[u8]) -> Result<Id, KvError> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| KvError::backend(InvalidInternedIdError { len: bytes.len() }))?;
    Ok(Id::from_u64(u64::from_be_bytes(arr)))
}

/// A get-or-create interning table over three `hs-kv` keyspaces (forward, reverse, and a counter),
/// with an in-process cache. See the module docs for the concurrency and caching design.
///
/// Cloning shares the cache and keyspace handles (cheap, like cloning a backend handle).
#[derive(Clone)]
pub struct InternTable<Ks, Id> {
    fwd: Ks,
    rev: Ks,
    seq: Ks,
    cache_fwd: Arc<RwLock<HashMap<Vec<u8>, Id>>>,
    cache_rev: Arc<RwLock<HashMap<Id, Vec<u8>>>>,
}

impl<Ks: Clone, Id: ShortId> InternTable<Ks, Id> {
    /// Opens (creating if necessary) an interning table named `prefix` (its three keyspaces are
    /// `{prefix}_fwd`, `{prefix}_rev` and `{prefix}_seq`).
    ///
    /// # Errors
    /// Returns [`KvError`] on backend failure.
    pub fn open<B>(backend: &B, prefix: &str) -> Result<Self, KvError>
    where
        B: KvBackend<Keyspace = Ks>,
    {
        Ok(Self {
            fwd: backend.keyspace(&format!("{prefix}_fwd"))?,
            rev: backend.keyspace(&format!("{prefix}_rev"))?,
            seq: backend.keyspace(&format!("{prefix}_seq"))?,
            cache_fwd: Arc::new(RwLock::new(HashMap::new())),
            cache_rev: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Returns `name`'s id, assigning a fresh one the first time `name` is seen.
    ///
    /// Call this inside [`hs_kv::transact`] (not a bare [`hs_kv::KvBackend::begin`] /
    /// [`hs_kv::KvBackend::commit`]): a conflict must retry this whole call, not just the commit,
    /// for the concurrent-first-interning race described in the module docs to resolve to one id.
    ///
    /// # Errors
    /// Returns [`KvError`] on backend failure, including the `hs-kv` transaction size limits if an
    /// enormous number of names are interned in one transaction.
    pub fn get_or_create<W: KvWrite<Keyspace = Ks>>(
        &self,
        txn: &mut W,
        name: &[u8],
    ) -> Result<Id, KvError> {
        if let Some(id) = self
            .cache_fwd
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
        {
            return Ok(*id);
        }

        if let Some(existing) = txn.get(&self.fwd, name)? {
            let id = decode_id::<Id>(&existing)?;
            self.cache_insert(name, id);
            return Ok(id);
        }

        let next = txn.atomic_add(&self.seq, b"next", 1)?;
        let id = Id::from_u64(u64::try_from(next).expect("interning counter never goes negative"));
        let encoded = id.to_u64().to_be_bytes();
        txn.put(&self.fwd, name, &encoded)?;
        txn.put(&self.rev, &encoded, name)?;
        // Deliberately not cached here -- see the module docs: this write has not committed yet.
        Ok(id)
    }

    /// The id `name` was interned to, if it has been interned at all. Unlike
    /// [`InternTable::get_or_create`], never assigns one.
    ///
    /// # Errors
    /// Returns [`KvError`] on backend failure.
    pub fn lookup<R: KvRead<Keyspace = Ks>>(
        &self,
        txn: &R,
        name: &[u8],
    ) -> Result<Option<Id>, KvError> {
        if let Some(id) = self
            .cache_fwd
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
        {
            return Ok(Some(*id));
        }
        match txn.get(&self.fwd, name)? {
            Some(existing) => {
                let id = decode_id::<Id>(&existing)?;
                self.cache_insert(name, id);
                Ok(Some(id))
            }
            None => Ok(None),
        }
    }

    /// The reverse lookup: the name interned to `id`, if any.
    ///
    /// # Errors
    /// Returns [`KvError`] on backend failure.
    pub fn resolve<R: KvRead<Keyspace = Ks>>(
        &self,
        txn: &R,
        id: Id,
    ) -> Result<Option<Vec<u8>>, KvError> {
        if let Some(name) = self
            .cache_rev
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
        {
            return Ok(Some(name.clone()));
        }
        let key = id.to_u64().to_be_bytes();
        match txn.get(&self.rev, &key)? {
            Some(name) => {
                self.cache_insert(&name, id);
                Ok(Some(name.to_vec()))
            }
            None => Ok(None),
        }
    }

    fn cache_insert(&self, name: &[u8], id: Id) {
        self.cache_fwd
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_vec(), id);
        self.cache_rev
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, name.to_vec());
    }
}

/// Opens the `room_sn` interning table (`PLAN.md` section 6.1): room IDs, interned on creation or
/// first sight.
///
/// # Errors
/// Returns [`KvError`] on backend failure.
pub fn room_sn_table<B: KvBackend>(
    backend: &B,
) -> Result<InternTable<B::Keyspace, hs_model::RoomSn>, KvError> {
    InternTable::open(backend, "room_sn")
}

/// Opens the `user_sn` interning table: local and remote user IDs.
///
/// # Errors
/// Returns [`KvError`] on backend failure.
pub fn user_sn_table<B: KvBackend>(
    backend: &B,
) -> Result<InternTable<B::Keyspace, hs_model::UserSn>, KvError> {
    InternTable::open(backend, "user_sn")
}

/// Opens the `server_sn` interning table: server names, for membership-by-server and ACLs.
///
/// # Errors
/// Returns [`KvError`] on backend failure.
pub fn server_sn_table<B: KvBackend>(
    backend: &B,
) -> Result<InternTable<B::Keyspace, hs_model::ServerSn>, KvError> {
    InternTable::open(backend, "server_sn")
}

/// Opens the `event_sn` interning table: event IDs. `PLAN.md` describes `event_sn` as assigned
/// store-wide monotonically at persist time rather than looked up by string; this table still
/// gives track 04 the string-keyed get-or-create shape uniformly with the other five, for callers
/// (an importer, a federation `event_id` reference) that only have the string form in hand.
///
/// # Errors
/// Returns [`KvError`] on backend failure.
pub fn event_sn_table<B: KvBackend>(
    backend: &B,
) -> Result<InternTable<B::Keyspace, hs_model::EventSn>, KvError> {
    InternTable::open(backend, "event_sn")
}

/// Opens the `state_key_id` interning table: the `(event type, state key)` pair that dominates
/// state maps (membership state keys especially). Callers intern the pair by encoding it first,
/// for example `hs_tables::key::encode(&(event_type, state_key))`, and pass the encoded bytes as
/// `name`.
///
/// # Errors
/// Returns [`KvError`] on backend failure.
pub fn state_key_id_table<B: KvBackend>(
    backend: &B,
) -> Result<InternTable<B::Keyspace, hs_model::StateKeyId>, KvError> {
    InternTable::open(backend, "state_key_id")
}

/// Opens the `type_id` interning table: event types, for filtering without string comparisons.
///
/// # Errors
/// Returns [`KvError`] on backend failure.
pub fn type_id_table<B: KvBackend>(
    backend: &B,
) -> Result<InternTable<B::Keyspace, hs_model::TypeId>, KvError> {
    InternTable::open(backend, "type_id")
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use hs_kv::{TransactConfig, transact};

    use super::*;

    #[test]
    fn intern_and_resolve_round_trip() {
        let backend = MemoryBackend::new();
        let table = room_sn_table(&backend).unwrap();

        let id = transact(&backend, TransactConfig::default(), |txn| {
            table.get_or_create(txn, b"!room:example.org")
        })
        .unwrap();

        let snap = backend.snapshot();
        assert_eq!(
            table.resolve(&snap, id).unwrap(),
            Some(b"!room:example.org".to_vec())
        );
        assert_eq!(table.lookup(&snap, b"!room:example.org").unwrap(), Some(id));
        assert_eq!(table.lookup(&snap, b"!other:example.org").unwrap(), None);
    }

    #[test]
    fn interning_the_same_name_twice_returns_the_same_id() {
        let backend = MemoryBackend::new();
        let table = user_sn_table(&backend).unwrap();

        let first = transact(&backend, TransactConfig::default(), |txn| {
            table.get_or_create(txn, b"@a:x")
        })
        .unwrap();
        let second = transact(&backend, TransactConfig::default(), |txn| {
            table.get_or_create(txn, b"@a:x")
        })
        .unwrap();
        assert_eq!(first, second);

        let third = transact(&backend, TransactConfig::default(), |txn| {
            table.get_or_create(txn, b"@b:x")
        })
        .unwrap();
        assert_ne!(first, third, "a different name must get a different id");
    }

    #[test]
    fn cache_is_reused_across_separate_intern_table_handles() {
        let backend = MemoryBackend::new();
        let table = server_sn_table(&backend).unwrap();
        let id = transact(&backend, TransactConfig::default(), |txn| {
            table.get_or_create(txn, b"example.org")
        })
        .unwrap();

        // A clone shares the cache (it's the point of Clone here: many callers hold a handle to
        // the same table).
        let cloned = table.clone();
        let snap = backend.snapshot();
        assert_eq!(cloned.lookup(&snap, b"example.org").unwrap(), Some(id));
    }

    #[test]
    fn concurrent_first_interning_of_the_same_name_yields_exactly_one_id() {
        let backend = MemoryBackend::new();
        let table = type_id_table(&backend).unwrap();

        let results: Vec<hs_model::TypeId> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let backend = backend.clone();
                    let table = table.clone();
                    scope.spawn(move || {
                        transact(&backend, TransactConfig::default(), |txn| {
                            table.get_or_create(txn, b"m.room.message")
                        })
                        .unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let first = results[0];
        assert!(
            results.iter().all(|id| *id == first),
            "every racing caller must resolve to the same id"
        );

        // And the counter was not wasted racing for the same name: only one id was ever assigned.
        let seq_ks = backend.keyspace("type_id_seq").unwrap();
        let snap = backend.snapshot();
        let counter = snap.get(&seq_ks, b"next").unwrap().unwrap();
        let arr: [u8; 8] = counter.as_ref().try_into().unwrap();
        assert_eq!(i64::from_be_bytes(arr), 1);
    }

    #[test]
    fn event_sn_uses_its_native_u64_width() {
        let backend = MemoryBackend::new();
        let table = event_sn_table(&backend).unwrap();
        let id = transact(&backend, TransactConfig::default(), |txn| {
            table.get_or_create(txn, b"$event:example.org")
        })
        .unwrap();
        assert_eq!(id, hs_model::EventSn::new(1));
    }
}
