//! The production state store's durable keyspaces (`docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`),
//! their encodings, the bounded caches in front of them, and the reader/writer that make them
//! a [`crate::chain_cover::ChainReader`]/[`crate::chain_cover::ChainWriter`].
//!
//! Every keyspace is store-wide, not per room: `EventSn`s are interned store-wide
//! (`hs-tables`'s `event_sn` table), a `(type, state_key)` pair means the same thing in every
//! room, and chain IDs come from one counter so a chain position is unambiguous without a room
//! prefix. A query only ever names events of one room, so nothing is scoped by room except the
//! migration marker ([`KS_LAYOUT`]), which is per room by nature.
//!
//! | keyspace | key | value |
//! |---|---|---|
//! | [`KS_STATE_AT`] | `EventSn` (8 bytes BE) | the root of the state after the event, as `StateRepr::encode_root` writes it |
//! | [`KS_EVENT`] | `EventSn` | the [`crate::record::EventRecord`] |
//! | [`KS_EVENT_ID`] | `EventSn` | the event ID |
//! | `state_key_id_{fwd,rev,seq}` | `hs_tables::key::encode(&(type, state_key))` / 8-byte id / `next` | the `hs-tables` `state_key_id` interning table's own layout, so the two agree on disk |
//! | [`KS_CHAIN_POS`] | `EventSn` | chain (4) ‖ sequence (4) ‖ `StateKeyId` (4) |
//! | [`KS_CHAIN_EVENT`] | chain (4) ‖ sequence (4) | `EventSn` |
//! | [`KS_CHAIN_LINK`] | chain (4) ‖ sequence (4) | n × (chain (4) ‖ sequence (4)) |
//! | [`KS_CHAIN_TIP`] | chain (4) | sequence (4) |
//! | [`KS_CHAIN_SEQ`] | `next` | the chain counter (`hs_kv::KvWrite::atomic_add`) |
//! | [`KS_LAYOUT`] | room ID | [`crate::kv_store::LAYOUT_VERSION`] (4) once the room's records are complete |
//!
//! Rows are immutable once written (a tip advances, but a tip is never cached), which is what
//! makes a cache in front of them safe to fill from any committed read.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Arc;

use bytes::Bytes;
use hs_kv::{KvBackend, KvError, KvRead, KvWrite, RangeSpec};
use hs_model::ids::{EventSn, StateKeyId};
use ruma::OwnedEventId;

use crate::cache::LruCache;
use crate::chain_cover::{ChainId, ChainPosition, ChainReader, ChainWriter};
use crate::record::{self, EventRecord, RecordError};

/// See the module docs.
pub const KS_STATE_AT: &str = "state_at";
/// See the module docs.
pub const KS_EVENT: &str = "state_event";
/// See the module docs.
pub const KS_EVENT_ID: &str = "state_event_id";
/// See the module docs.
pub const KS_KEY_FWD: &str = "state_key_id_fwd";
/// See the module docs.
pub const KS_KEY_REV: &str = "state_key_id_rev";
/// See the module docs.
pub const KS_KEY_SEQ: &str = "state_key_id_seq";
/// See the module docs.
pub const KS_CHAIN_POS: &str = "state_chain_pos";
/// See the module docs.
pub const KS_CHAIN_EVENT: &str = "state_chain_event";
/// See the module docs.
pub const KS_CHAIN_LINK: &str = "state_chain_link";
/// See the module docs.
pub const KS_CHAIN_TIP: &str = "state_chain_tip";
/// See the module docs.
pub const KS_CHAIN_SEQ: &str = "state_chain_seq";
/// See the module docs.
pub const KS_LAYOUT: &str = "state_layout";

/// Every keyspace name this module opens, for a caller that wants to clear or copy them.
pub const ALL_KEYSPACES: [&str; 12] = [
    KS_STATE_AT,
    KS_EVENT,
    KS_EVENT_ID,
    KS_KEY_FWD,
    KS_KEY_REV,
    KS_KEY_SEQ,
    KS_CHAIN_POS,
    KS_CHAIN_EVENT,
    KS_CHAIN_LINK,
    KS_CHAIN_TIP,
    KS_CHAIN_SEQ,
    KS_LAYOUT,
];

const COUNTER_KEY: &[u8] = b"next";

/// How many entries each cache keeps, per store (one store per open room).
#[derive(Debug, Clone, Copy)]
pub struct CacheSizes {
    /// `state_at` roots.
    pub state_at: usize,
    /// Event IDs by short ID.
    pub event_ids: usize,
    /// Interned keys, each direction.
    pub keys: usize,
    /// Chain positions.
    pub chain_positions: usize,
    /// Event records (`hs_state_resolution_events_cached`).
    pub records: usize,
}

impl Default for CacheSizes {
    fn default() -> Self {
        Self {
            state_at: 4_096,
            event_ids: 4_096,
            keys: 4_096,
            chain_positions: 4_096,
            records: 1_024,
        }
    }
}

/// A durable error: a backend failure, or a row that did not decode.
#[derive(Debug, thiserror::Error)]
pub enum DurableError {
    /// The `hs-kv` backend failed.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A row of `keyspace` did not decode.
    #[error("corrupt row in {keyspace}")]
    Corrupt {
        /// Which keyspace.
        keyspace: &'static str,
    },
    /// An event record did not decode.
    #[error(transparent)]
    Record(#[from] RecordError),
}

/// The keyspace handles. Cheap to clone (handles are).
#[derive(Debug, Clone)]
pub struct Tables<Ks> {
    /// [`KS_STATE_AT`].
    pub state_at: Ks,
    /// [`KS_EVENT`].
    pub event: Ks,
    /// [`KS_EVENT_ID`].
    pub event_id: Ks,
    /// [`KS_KEY_FWD`].
    pub key_fwd: Ks,
    /// [`KS_KEY_REV`].
    pub key_rev: Ks,
    /// [`KS_KEY_SEQ`].
    pub key_seq: Ks,
    /// [`KS_CHAIN_POS`].
    pub chain_pos: Ks,
    /// [`KS_CHAIN_EVENT`].
    pub chain_event: Ks,
    /// [`KS_CHAIN_LINK`].
    pub chain_link: Ks,
    /// [`KS_CHAIN_TIP`].
    pub chain_tip: Ks,
    /// [`KS_CHAIN_SEQ`].
    pub chain_seq: Ks,
    /// [`KS_LAYOUT`].
    pub layout: Ks,
}

impl<Ks: Clone> Tables<Ks> {
    /// Opens (creating if necessary) every keyspace on `backend`.
    ///
    /// # Errors
    /// Returns [`KvError`] if a keyspace could not be opened.
    pub fn open<B: KvBackend<Keyspace = Ks>>(backend: &B) -> Result<Self, KvError> {
        Ok(Self {
            state_at: backend.keyspace(KS_STATE_AT)?,
            event: backend.keyspace(KS_EVENT)?,
            event_id: backend.keyspace(KS_EVENT_ID)?,
            key_fwd: backend.keyspace(KS_KEY_FWD)?,
            key_rev: backend.keyspace(KS_KEY_REV)?,
            key_seq: backend.keyspace(KS_KEY_SEQ)?,
            chain_pos: backend.keyspace(KS_CHAIN_POS)?,
            chain_event: backend.keyspace(KS_CHAIN_EVENT)?,
            chain_link: backend.keyspace(KS_CHAIN_LINK)?,
            chain_tip: backend.keyspace(KS_CHAIN_TIP)?,
            chain_seq: backend.keyspace(KS_CHAIN_SEQ)?,
            layout: backend.keyspace(KS_LAYOUT)?,
        })
    }
}

/// The bounded caches in front of the keyspaces. `Root` is the representation's root type.
pub struct Caches<Root> {
    /// Decoded `state_at` roots.
    pub state_at: LruCache<EventSn, Root>,
    /// Event IDs.
    pub event_ids: LruCache<EventSn, OwnedEventId>,
    /// `(type, state_key)` to id.
    pub key_fwd: LruCache<(String, String), StateKeyId>,
    /// Id to `(type, state_key)`.
    pub key_rev: LruCache<StateKeyId, (String, String)>,
    /// Chain positions with the event's key.
    pub chain_pos: LruCache<EventSn, (ChainPosition, StateKeyId)>,
    /// Event records.
    pub records: LruCache<EventSn, Arc<EventRecord>>,
}

impl<Root> Caches<Root> {
    /// Empty caches of the given sizes.
    #[must_use]
    pub fn new(sizes: CacheSizes) -> Self {
        Self {
            state_at: LruCache::new(sizes.state_at),
            event_ids: LruCache::new(sizes.event_ids),
            key_fwd: LruCache::new(sizes.keys),
            key_rev: LruCache::new(sizes.keys),
            chain_pos: LruCache::new(sizes.chain_positions),
            records: LruCache::new(sizes.records),
        }
    }

    /// Caches an event record, keeping `hs_state_resolution_events_cached` current.
    pub fn insert_record(&mut self, sn: EventSn, record: Arc<EventRecord>) {
        let before = self.records.len();
        self.records.insert(sn, record);
        let after = self.records.len();
        crate::metrics::adjust_resolution_events_cached(after as i64 - before as i64);
    }
}

impl<Root> Drop for Caches<Root> {
    fn drop(&mut self) {
        crate::metrics::adjust_resolution_events_cached(-(self.records.len() as i64));
    }
}

/// `hs_tables::key::encode(&(event_type, state_key))`: the interning table's name for a pair.
#[must_use]
pub fn key_name(event_type: &str, state_key: &str) -> Vec<u8> {
    hs_tables::key::encode(&(event_type.to_owned(), state_key.to_owned()))
}

fn decode_key_name(bytes: &[u8]) -> Option<(String, String)> {
    <(String, String) as hs_tables::key::TupleKey>::decode(bytes).ok()
}

fn chain_key(chain: ChainId, sequence: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&chain.0.to_be_bytes());
    out[4..].copy_from_slice(&sequence.to_be_bytes());
    out
}

fn decode_chain_key(bytes: &[u8]) -> Option<ChainPosition> {
    let arr: [u8; 8] = bytes.try_into().ok()?;
    Some(ChainPosition {
        chain: ChainId(u32::from_be_bytes([arr[0], arr[1], arr[2], arr[3]])),
        sequence: u32::from_be_bytes([arr[4], arr[5], arr[6], arr[7]]),
    })
}

fn decode_u32(bytes: &[u8]) -> Option<u32> {
    <[u8; 4]>::try_from(bytes).ok().map(u32::from_be_bytes)
}

fn decode_u64(bytes: &[u8]) -> Option<u64> {
    <[u8; 8]>::try_from(bytes).ok().map(u64::from_be_bytes)
}

fn encode_links(targets: &[ChainPosition]) -> Vec<u8> {
    let mut out = Vec::with_capacity(targets.len() * 8);
    for t in targets {
        out.extend_from_slice(&chain_key(t.chain, t.sequence));
    }
    out
}

fn decode_links(bytes: &[u8]) -> Option<Vec<ChainPosition>> {
    let (chunks, rest) = bytes.as_chunks::<8>();
    if !rest.is_empty() {
        return None;
    }
    chunks.iter().map(|c| decode_chain_key(c)).collect()
}

/// Reads over any `hs-kv` reader (a snapshot, or a transaction that must see its own writes)
/// with the caches in front. The chain-cover algorithms run on this directly.
pub struct Reader<'a, Ks, R, Root> {
    /// The keyspaces.
    pub tables: &'a Tables<Ks>,
    /// The reader.
    pub kv: &'a R,
    /// The caches.
    pub caches: &'a RefCell<Caches<Root>>,
}

impl<'a, Ks, R: KvRead<Keyspace = Ks>, Root: Copy> Reader<'a, Ks, R, Root> {
    /// A reader over `kv`.
    pub fn new(tables: &'a Tables<Ks>, kv: &'a R, caches: &'a RefCell<Caches<Root>>) -> Self {
        Self { tables, kv, caches }
    }

    /// The raw `state_at` row of `event`, if any (decoding is the representation's).
    ///
    /// # Errors
    /// Returns [`DurableError::Kv`] on a backend failure.
    pub fn state_at_bytes(&self, event: EventSn) -> Result<Option<Bytes>, DurableError> {
        Ok(self.kv.get(&self.tables.state_at, &event.to_be_bytes())?)
    }

    /// The event ID of `event`, if the store holds it.
    ///
    /// # Errors
    /// Returns [`DurableError`] on a backend failure or a corrupt row.
    pub fn event_id(&self, event: EventSn) -> Result<Option<OwnedEventId>, DurableError> {
        if let Some(id) = self.caches.borrow_mut().event_ids.get(&event) {
            return Ok(Some(id.clone()));
        }
        let Some(bytes) = self.kv.get(&self.tables.event_id, &event.to_be_bytes())? else {
            return Ok(None);
        };
        let id = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| OwnedEventId::try_from(s).ok())
            .ok_or(DurableError::Corrupt {
                keyspace: KS_EVENT_ID,
            })?;
        self.caches.borrow_mut().event_ids.insert(event, id.clone());
        Ok(Some(id))
    }

    /// The event IDs of `events`, in order; `None` for one the store does not hold. Misses are
    /// read in one `multi_get`.
    ///
    /// # Errors
    /// Returns [`DurableError`] on a backend failure or a corrupt row.
    pub fn event_ids(&self, events: &[EventSn]) -> Result<Vec<Option<OwnedEventId>>, DurableError> {
        let mut out: Vec<Option<OwnedEventId>> = vec![None; events.len()];
        let mut misses: Vec<usize> = Vec::new();
        {
            let mut caches = self.caches.borrow_mut();
            for (i, sn) in events.iter().enumerate() {
                match caches.event_ids.get(sn) {
                    Some(id) => out[i] = Some(id.clone()),
                    None => misses.push(i),
                }
            }
        }
        if misses.is_empty() {
            return Ok(out);
        }
        let keys: Vec<[u8; 8]> = misses.iter().map(|&i| events[i].to_be_bytes()).collect();
        let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let values = self.kv.multi_get(&self.tables.event_id, &refs)?;
        let mut caches = self.caches.borrow_mut();
        for (&i, value) in misses.iter().zip(values) {
            let Some(bytes) = value else { continue };
            let id = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| OwnedEventId::try_from(s).ok())
                .ok_or(DurableError::Corrupt {
                    keyspace: KS_EVENT_ID,
                })?;
            caches.event_ids.insert(events[i], id.clone());
            out[i] = Some(id);
        }
        Ok(out)
    }

    /// The id interned for `(event_type, state_key)`, if any. Never allocates.
    ///
    /// # Errors
    /// Returns [`DurableError`] on a backend failure or a corrupt row.
    pub fn key_id(
        &self,
        event_type: &str,
        state_key: &str,
    ) -> Result<Option<StateKeyId>, DurableError> {
        let pair = (event_type.to_owned(), state_key.to_owned());
        if let Some(id) = self.caches.borrow_mut().key_fwd.get(&pair) {
            return Ok(Some(*id));
        }
        let Some(bytes) = self
            .kv
            .get(&self.tables.key_fwd, &key_name(event_type, state_key))?
        else {
            return Ok(None);
        };
        let id = decode_u64(&bytes)
            .and_then(|v| u32::try_from(v).ok())
            .map(StateKeyId::new)
            .ok_or(DurableError::Corrupt {
                keyspace: KS_KEY_FWD,
            })?;
        let mut caches = self.caches.borrow_mut();
        caches.key_fwd.insert(pair.clone(), id);
        caches.key_rev.insert(id, pair);
        Ok(Some(id))
    }

    /// The `(event_type, state_key)` pairs of `keys`, in order; `None` for an id never interned.
    ///
    /// # Errors
    /// Returns [`DurableError`] on a backend failure or a corrupt row.
    pub fn key_pairs(
        &self,
        keys: &[StateKeyId],
    ) -> Result<Vec<Option<(String, String)>>, DurableError> {
        let mut out: Vec<Option<(String, String)>> = vec![None; keys.len()];
        let mut misses: Vec<usize> = Vec::new();
        {
            let mut caches = self.caches.borrow_mut();
            for (i, key) in keys.iter().enumerate() {
                match caches.key_rev.get(key) {
                    Some(pair) => out[i] = Some(pair.clone()),
                    None => misses.push(i),
                }
            }
        }
        if misses.is_empty() {
            return Ok(out);
        }
        let names: Vec<[u8; 8]> = misses
            .iter()
            .map(|&i| u64::from(keys[i].get()).to_be_bytes())
            .collect();
        let refs: Vec<&[u8]> = names.iter().map(|k| k.as_slice()).collect();
        let values = self.kv.multi_get(&self.tables.key_rev, &refs)?;
        let mut caches = self.caches.borrow_mut();
        for (&i, value) in misses.iter().zip(values) {
            let Some(bytes) = value else { continue };
            let pair = decode_key_name(&bytes).ok_or(DurableError::Corrupt {
                keyspace: KS_KEY_REV,
            })?;
            caches.key_rev.insert(keys[i], pair.clone());
            caches.key_fwd.insert(pair.clone(), keys[i]);
            out[i] = Some(pair);
        }
        Ok(out)
    }

    /// The record of `event`, if the store holds it.
    ///
    /// # Errors
    /// Returns [`DurableError`] on a backend failure or a corrupt row.
    pub fn record(&self, event: EventSn) -> Result<Option<Arc<EventRecord>>, DurableError> {
        if let Some(record) = self.caches.borrow_mut().records.get(&event) {
            return Ok(Some(Arc::clone(record)));
        }
        let Some(bytes) = self.kv.get(&self.tables.event, &event.to_be_bytes())? else {
            return Ok(None);
        };
        crate::metrics::count_records_read(1);
        let record = Arc::new(record::decode(&bytes)?);
        self.caches
            .borrow_mut()
            .insert_record(event, Arc::clone(&record));
        Ok(Some(record))
    }

    /// The layout version recorded for `room_id`, if any.
    ///
    /// # Errors
    /// Returns [`DurableError`] on a backend failure or a corrupt row.
    pub fn layout_version(&self, room_id: &[u8]) -> Result<Option<u32>, DurableError> {
        match self.kv.get(&self.tables.layout, room_id)? {
            None => Ok(None),
            Some(bytes) => decode_u32(&bytes).map(Some).ok_or(DurableError::Corrupt {
                keyspace: KS_LAYOUT,
            }),
        }
    }
}

impl<Ks, R: KvRead<Keyspace = Ks>, Root: Copy> ChainReader for Reader<'_, Ks, R, Root> {
    type Error = DurableError;

    fn position_of(
        &self,
        event: EventSn,
    ) -> Result<Option<(ChainPosition, StateKeyId)>, DurableError> {
        if let Some(entry) = self.caches.borrow_mut().chain_pos.get(&event) {
            return Ok(Some(*entry));
        }
        let Some(bytes) = self.kv.get(&self.tables.chain_pos, &event.to_be_bytes())? else {
            return Ok(None);
        };
        let entry = (bytes.len() == 12)
            .then(|| {
                let position = decode_chain_key(&bytes[..8])?;
                let key = decode_u32(&bytes[8..])?;
                Some((position, StateKeyId::new(key)))
            })
            .flatten()
            .ok_or(DurableError::Corrupt {
                keyspace: KS_CHAIN_POS,
            })?;
        self.caches.borrow_mut().chain_pos.insert(event, entry);
        Ok(Some(entry))
    }

    fn tip_of(&self, chain: ChainId) -> Result<u32, DurableError> {
        match self
            .kv
            .get(&self.tables.chain_tip, &chain.0.to_be_bytes())?
        {
            None => Ok(0),
            Some(bytes) => decode_u32(&bytes).ok_or(DurableError::Corrupt {
                keyspace: KS_CHAIN_TIP,
            }),
        }
    }

    fn links_at(&self, at: ChainPosition) -> Result<Vec<ChainPosition>, DurableError> {
        match self
            .kv
            .get(&self.tables.chain_link, &chain_key(at.chain, at.sequence))?
        {
            None => Ok(Vec::new()),
            Some(bytes) => decode_links(&bytes).ok_or(DurableError::Corrupt {
                keyspace: KS_CHAIN_LINK,
            }),
        }
    }

    fn links_up_to(
        &self,
        chain: ChainId,
        sequence: u32,
    ) -> Result<Vec<ChainPosition>, DurableError> {
        let spec = RangeSpec::new(
            Bound::Included(Bytes::copy_from_slice(&chain_key(chain, 0))),
            Bound::Included(Bytes::copy_from_slice(&chain_key(chain, sequence))),
        );
        let mut out = Vec::new();
        for item in self.kv.range(&self.tables.chain_link, spec) {
            let (_, value) = item?;
            out.extend(decode_links(&value).ok_or(DurableError::Corrupt {
                keyspace: KS_CHAIN_LINK,
            })?);
        }
        Ok(out)
    }

    fn events_between(
        &self,
        chain: ChainId,
        from: u32,
        to: u32,
    ) -> Result<Vec<EventSn>, DurableError> {
        if from > to {
            return Ok(Vec::new());
        }
        let spec = RangeSpec::new(
            Bound::Included(Bytes::copy_from_slice(&chain_key(chain, from))),
            Bound::Included(Bytes::copy_from_slice(&chain_key(chain, to))),
        );
        let mut out = Vec::new();
        for item in self.kv.range(&self.tables.chain_event, spec) {
            let (_, value) = item?;
            let sn = decode_u64(&value).ok_or(DurableError::Corrupt {
                keyspace: KS_CHAIN_EVENT,
            })?;
            out.push(EventSn::new(sn));
        }
        Ok(out)
    }
}

/// Writes inside one transaction, reading through it (so a batch of events in one transaction
/// sees the rows the batch already wrote) with the caches in front for immutable rows.
pub struct Writer<'a, Ks, W, Root> {
    /// The keyspaces.
    pub tables: &'a Tables<Ks>,
    /// The transaction.
    pub txn: &'a mut W,
    /// The caches.
    pub caches: &'a RefCell<Caches<Root>>,
}

impl<'a, Ks, W: KvWrite<Keyspace = Ks>, Root: Copy> Writer<'a, Ks, W, Root> {
    /// A writer over `txn`.
    pub fn new(tables: &'a Tables<Ks>, txn: &'a mut W, caches: &'a RefCell<Caches<Root>>) -> Self {
        Self {
            tables,
            txn,
            caches,
        }
    }

    /// This writer as a [`Reader`] over the same transaction.
    pub fn reader(&self) -> Reader<'_, Ks, W, Root> {
        Reader::new(self.tables, &*self.txn, self.caches)
    }

    /// Get-or-create interning of `(event_type, state_key)`, exactly as
    /// `hs_tables::interning::InternTable::get_or_create` does it (same keyspaces, same wire
    /// format, same counter). A freshly allocated id is deliberately not cached: this write
    /// has not committed yet.
    ///
    /// # Errors
    /// Returns [`DurableError`] on a backend failure or a corrupt row.
    pub fn intern_key(
        &mut self,
        event_type: &str,
        state_key: &str,
    ) -> Result<StateKeyId, DurableError> {
        if let Some(id) = self.reader().key_id(event_type, state_key)? {
            return Ok(id);
        }
        let next = self.txn.atomic_add(&self.tables.key_seq, COUNTER_KEY, 1)?;
        let id = u64::try_from(next)
            .ok()
            .and_then(|v| u32::try_from(v).ok())
            .map(StateKeyId::new)
            .ok_or(DurableError::Corrupt {
                keyspace: KS_KEY_SEQ,
            })?;
        let encoded = u64::from(id.get()).to_be_bytes();
        let name = key_name(event_type, state_key);
        self.txn.put(&self.tables.key_fwd, &name, &encoded)?;
        self.txn.put(&self.tables.key_rev, &encoded, &name)?;
        Ok(id)
    }

    /// Writes `event`'s `state_at` row.
    ///
    /// # Errors
    /// Returns [`DurableError::Kv`] on a backend failure.
    pub fn put_state_at(&mut self, event: EventSn, root: &[u8]) -> Result<(), DurableError> {
        Ok(self
            .txn
            .put(&self.tables.state_at, &event.to_be_bytes(), root)?)
    }

    /// Writes `event`'s ID and record rows.
    ///
    /// # Errors
    /// Returns [`DurableError::Kv`] on a backend failure.
    pub fn put_record(&mut self, event: EventSn, record: &EventRecord) -> Result<(), DurableError> {
        self.txn.put(
            &self.tables.event_id,
            &event.to_be_bytes(),
            record.event_id.as_bytes(),
        )?;
        self.txn.put(
            &self.tables.event,
            &event.to_be_bytes(),
            &record::encode(record),
        )?;
        Ok(())
    }

    /// Writes `room_id`'s layout marker.
    ///
    /// # Errors
    /// Returns [`DurableError::Kv`] on a backend failure.
    pub fn put_layout_version(&mut self, room_id: &[u8], version: u32) -> Result<(), DurableError> {
        Ok(self
            .txn
            .put(&self.tables.layout, room_id, &version.to_be_bytes())?)
    }
}

impl<Ks, W: KvWrite<Keyspace = Ks>, Root: Copy> ChainReader for Writer<'_, Ks, W, Root> {
    type Error = DurableError;

    fn position_of(
        &self,
        event: EventSn,
    ) -> Result<Option<(ChainPosition, StateKeyId)>, DurableError> {
        self.reader().position_of(event)
    }

    fn tip_of(&self, chain: ChainId) -> Result<u32, DurableError> {
        self.reader().tip_of(chain)
    }

    fn links_at(&self, at: ChainPosition) -> Result<Vec<ChainPosition>, DurableError> {
        self.reader().links_at(at)
    }

    fn links_up_to(
        &self,
        chain: ChainId,
        sequence: u32,
    ) -> Result<Vec<ChainPosition>, DurableError> {
        self.reader().links_up_to(chain, sequence)
    }

    fn events_between(
        &self,
        chain: ChainId,
        from: u32,
        to: u32,
    ) -> Result<Vec<EventSn>, DurableError> {
        self.reader().events_between(chain, from, to)
    }
}

impl<Ks, W: KvWrite<Keyspace = Ks>, Root: Copy> ChainWriter for Writer<'_, Ks, W, Root> {
    fn allocate_chain(&mut self) -> Result<ChainId, DurableError> {
        let next = self
            .txn
            .atomic_add(&self.tables.chain_seq, COUNTER_KEY, 1)?;
        u64::try_from(next)
            .ok()
            .and_then(|v| u32::try_from(v).ok())
            .map(ChainId)
            .ok_or(DurableError::Corrupt {
                keyspace: KS_CHAIN_SEQ,
            })
    }

    fn record_position(
        &mut self,
        event: EventSn,
        position: ChainPosition,
        key: StateKeyId,
    ) -> Result<(), DurableError> {
        let mut value = [0u8; 12];
        value[..8].copy_from_slice(&chain_key(position.chain, position.sequence));
        value[8..].copy_from_slice(&key.get().to_be_bytes());
        self.txn
            .put(&self.tables.chain_pos, &event.to_be_bytes(), &value)?;
        self.txn.put(
            &self.tables.chain_event,
            &chain_key(position.chain, position.sequence),
            &event.to_be_bytes(),
        )?;
        Ok(())
    }

    fn record_tip(&mut self, chain: ChainId, sequence: u32) -> Result<(), DurableError> {
        Ok(self.txn.put(
            &self.tables.chain_tip,
            &chain.0.to_be_bytes(),
            &sequence.to_be_bytes(),
        )?)
    }

    fn record_links(
        &mut self,
        at: ChainPosition,
        targets: Vec<ChainPosition>,
    ) -> Result<(), DurableError> {
        Ok(self.txn.put(
            &self.tables.chain_link,
            &chain_key(at.chain, at.sequence),
            &encode_links(&targets),
        )?)
    }
}

/// A scratch map from event ID to short ID for one resolution: every ID the store hands to a
/// resolver came from one of its own rows, so the way back never needs a durable reverse index.
pub type IdScratch = RefCell<HashMap<OwnedEventId, EventSn>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_cover::{self, ChainCoverIndex};
    use hs_kv::memory::MemoryBackend;
    use hs_kv::{TransactConfig, transact};
    use proptest::prelude::*;

    fn sn(n: u64) -> EventSn {
        EventSn::new(n)
    }

    /// Generates a random small DAG, as `chain_cover::property_tests` does.
    fn dag_strategy(n: usize) -> impl Strategy<Value = (Vec<Vec<usize>>, Vec<u32>)> {
        let keys = prop::collection::vec(0u32..3, n);
        let auth: Vec<_> = (0..n)
            .map(|i| {
                if i == 0 {
                    Just(Vec::<usize>::new()).boxed()
                } else {
                    prop::collection::vec(0..i, 0..=2usize.min(i))
                        .prop_map(|mut v| {
                            v.sort_unstable();
                            v.dedup();
                            v
                        })
                        .boxed()
                }
            })
            .collect();
        (auth, keys)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// The durable index answers exactly what the in-memory index answers for the same
        /// DAG: positions may be numbered differently (chain ids come from a counter), but
        /// `contains` and `auth_chain_difference` agree on every pair.
        #[test]
        fn durable_index_agrees_with_in_memory_index((auth, keys) in dag_strategy(12)) {
            let backend = MemoryBackend::new();
            let tables = Tables::open(&backend).unwrap();
            let caches: RefCell<Caches<[u8; 16]>> = RefCell::new(Caches::new(CacheSizes::default()));
            let mut memory = ChainCoverIndex::new();
            for (i, a) in auth.iter().enumerate() {
                let auth_sn: Vec<EventSn> = a.iter().map(|&j| sn(j as u64)).collect();
                memory.add_event(sn(i as u64), StateKeyId::new(keys[i]), &auth_sn);
                transact(&backend, TransactConfig::default(), |txn| {
                    let mut writer = Writer::new(&tables, txn, &caches);
                    chain_cover::add_event(&mut writer, sn(i as u64), StateKeyId::new(keys[i]), &auth_sn)
                        .map_err(|e| KvError::backend(std::io::Error::other(e.to_string())))?;
                    Ok(())
                }).unwrap();
            }
            let snapshot = backend.snapshot();
            let reader = Reader::new(&tables, &snapshot, &caches);
            for i in 0..auth.len() {
                for j in 0..auth.len() {
                    let expected = memory.contains(sn(i as u64), sn(j as u64));
                    let actual = chain_cover::contains(&reader, sn(i as u64), sn(j as u64)).unwrap();
                    prop_assert_eq!(actual, expected, "contains({}, {})", i, j);
                }
            }
            let n = auth.len();
            if n >= 2 {
                let sets = vec![vec![sn((n - 1) as u64)], vec![sn((n - 2) as u64)]];
                let mut expected = memory.auth_chain_difference(&sets);
                let mut actual = chain_cover::auth_chain_difference(&reader, &sets).unwrap();
                expected.sort();
                actual.sort();
                prop_assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn interning_is_stable_across_reopen_and_matches_hs_tables() {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let caches: RefCell<Caches<[u8; 16]>> = RefCell::new(Caches::new(CacheSizes::default()));
        let first = transact(&backend, TransactConfig::default(), |txn| {
            let mut w = Writer::new(&tables, txn, &caches);
            let a = w.intern_key("m.room.member", "@a:hs1").unwrap();
            let b = w.intern_key("m.room.topic", "").unwrap();
            let a_again = w.intern_key("m.room.member", "@a:hs1").unwrap();
            assert_eq!(a, a_again);
            assert_ne!(a, b);
            Ok((a, b))
        })
        .unwrap();

        // A second set of handles with empty caches: the same ids come back from the rows.
        let tables2 = Tables::open(&backend).unwrap();
        let caches2: RefCell<Caches<[u8; 16]>> = RefCell::new(Caches::new(CacheSizes::default()));
        let snapshot = backend.snapshot();
        let reader = Reader::new(&tables2, &snapshot, &caches2);
        assert_eq!(
            reader.key_id("m.room.member", "@a:hs1").unwrap(),
            Some(first.0)
        );
        assert_eq!(reader.key_id("m.room.topic", "").unwrap(), Some(first.1));
        assert_eq!(reader.key_id("m.room.name", "").unwrap(), None);
        assert_eq!(
            reader.key_pairs(&[first.1, first.0]).unwrap(),
            vec![
                Some(("m.room.topic".to_owned(), String::new())),
                Some(("m.room.member".to_owned(), "@a:hs1".to_owned()))
            ]
        );

        // `hs-tables`'s own table reads the same rows.
        let table = hs_tables::interning::state_key_id_table(&backend).unwrap();
        assert_eq!(
            table
                .lookup(&snapshot, &key_name("m.room.member", "@a:hs1"))
                .unwrap(),
            Some(first.0)
        );
        let via_table = transact(&backend, TransactConfig::default(), |txn| {
            table.get_or_create(txn, &key_name("m.room.name", ""))
        })
        .unwrap();
        let snapshot = backend.snapshot();
        let reader = Reader::new(&tables2, &snapshot, &caches2);
        assert_eq!(reader.key_id("m.room.name", "").unwrap(), Some(via_table));
    }
}
