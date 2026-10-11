//! [`KvStateStore`]: the production [`crate::api::StateStore`], generic over one
//! [`StateRepr`] implementation, with every per-event record durable.
//!
//! This is [`crate::store::InMemoryStateStore`]'s ingestion and resolution logic, unchanged in
//! substance, made generic over the state representation so it is not duplicated across the
//! production representation (`crate::frames::FrameRepr`, the bake-off's winning candidate B --
//! `docs/decisions/0006-state-bakeoff-results.md`) and the two benchmark-only representations kept
//! under `crate::bakeoff` for that decision's own "what would change this decision" re-runs.
//! [`ProductionStateStore`] is this type instantiated with the production representation; that is
//! what tracks 04 and 06 should hold.
//!
//! # Nothing per event stays in memory
//!
//! Until 2026-10-10 this store kept, in memory and rebuilt on every open, the root of the state
//! after every event, two event-id maps, a [`crate::state_res::ResolutionEvent`] per event and
//! the chain-cover index, so a room actor had to replay its whole history into the store before
//! it could answer anything, and the store then held a record of every event for the actor's
//! lifetime (`docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`). All of it is
//! now in `crate::durable`'s keyspaces, written as each event is ingested (inside the caller's
//! transaction when it hands one in, [`KvStateStore::ingest_in`]) and read on demand through
//! bounded caches (`crate::cache`). Opening a store opens keyspaces and nothing else; what it
//! holds afterwards is bounded by [`crate::durable::CacheSizes`], not by history.
//!
//! State resolution reads only the records it touches ([`crate::state_res::EventFetch`]): the
//! conflicted events, their auth chains and the handful of state entries the auth checks ask
//! for, with the auth chains answered by the durable chain-cover index rather than a walk.
//!
//! # Migration
//!
//! A room ingested before this change has frames and nothing else, and its frames reference
//! `StateKeyId`s numbered per process, not by the interning table. Its records are built once
//! by feeding the room's events again -- the room actor's load already does exactly that -- and
//! the room is then marked ([`KvStateStore::mark_migrated`], [`LAYOUT_VERSION`]) so no later
//! open replays it; an open that finds the marker reads current state and nothing else. The
//! old frames stay in the content-addressed frames keyspace unreferenced; they are small and
//! harmless. Bumping [`LAYOUT_VERSION`] re-migrates every room on its next open.
//!
//! `InMemoryStateStore` itself is deliberately *not* rebuilt on top of this module (it stays a
//! hand-written, dependency-light reference implementation for tests -- see its own module docs).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use elsa::FrozenMap;
use hs_kv::{KvBackend, KvError, KvRead, TransactConfig, transact};
use hs_model::canonical::CanonicalJsonObject;
use hs_model::ids::{EventSn, StateKeyId};
use hs_model::room_version::{self, RoomVersionRules, StateResolutionVersion};
use ruma::state_res::utils::event_id_set::EventIdSet;
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId};
use thiserror::Error;

use crate::api::{StateDiff, StateStore};
use crate::chain_cover::{self, ChainPosition, ChainReader};
use crate::durable::{self, CacheSizes, Caches, DurableError, IdScratch, Reader, Tables, Writer};
use crate::error::StateResError;
use crate::frames::FrameRepr;
use crate::record::EventRecord;
use crate::repr::StateRepr;
use crate::state_res::{self, EventFetch, ResolutionEvent};

/// The version of the durable layout a migrated room is marked with. Bump it to have every
/// room's records rebuilt from its events on its next open (see the module docs).
pub const LAYOUT_VERSION: u32 = 1;

/// Above how many events touched during an open the store logs its open summary
/// ([`KvStateStore::log_open_summary`]).
pub const OPEN_SUMMARY_THRESHOLD: u64 = 10_000;

/// Errors from [`KvStateStore`]: the representation's own errors, plus the same ingestion-level
/// errors [`crate::store::InMemoryStateStore`]'s `StoreError` defines.
#[derive(Debug, Error)]
pub enum KvStoreError<E: std::error::Error + Send + Sync + 'static> {
    /// The representation itself failed (storage I/O, an unknown root).
    #[error(transparent)]
    Repr(E),
    /// A durable record keyspace failed, or a row did not decode.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// [`StateStore::state_at`] was called for an event this store has not ingested.
    #[error("unknown event {0}")]
    UnknownEvent(EventSn),
    /// [`StateStore::resolve`] was called with no forks.
    #[error("resolve() requires at least one fork")]
    EmptyForks,
    /// [`StateStore::resolve`] was called with a different room version than this store was
    /// created for.
    #[error("resolve() called with a different room version than this store was created for")]
    WrongRoomVersion,
    /// The room version is not one this crate's room-version table or `ruma-state-res` supports.
    #[error("unsupported room version: {0}")]
    UnsupportedRoomVersion(String),
    /// State resolution failed.
    #[error(transparent)]
    StateRes(#[from] StateResError),
}

impl<E: std::error::Error + Send + Sync + 'static> From<KvError> for KvStoreError<E> {
    fn from(e: KvError) -> Self {
        Self::Durable(DurableError::Kv(e))
    }
}

/// One event to ingest: the parameter list [`KvStateStore::add_event`] has always taken, as a
/// struct, for [`KvStateStore::ingest_in`].
#[derive(Debug, Clone)]
pub struct NewEvent<'a> {
    /// The event's short ID.
    pub event: EventSn,
    /// The event's ID.
    pub event_id: OwnedEventId,
    /// The room.
    pub room_id: OwnedRoomId,
    /// The event's `type`.
    pub event_type: &'a str,
    /// The event's `state_key`, if it is a state event.
    pub state_key: Option<&'a str>,
    /// The event's `sender`.
    pub sender: OwnedUserId,
    /// The event's `content` (a caller may hand in an empty object for a non-state event:
    /// state resolution never reads it).
    pub content: CanonicalJsonObject,
    /// The event's `depth`.
    pub depth: i64,
    /// The event's `origin_server_ts`.
    pub origin_server_ts: i64,
    /// The event's `auth_events`, as short IDs; entries this store does not know are skipped.
    pub auth_events: &'a [EventSn],
    /// The event's `prev_events`, as short IDs; entries this store does not know are skipped.
    pub prev_events: &'a [EventSn],
    /// See [`crate::auth::IncomingEvent::only_prev_event_is_room_create`].
    pub only_prev_event_is_room_create: bool,
}

/// What a store has read and written since it was opened: the numbers behind
/// `hs_state_events_replayed_total` and the open summary, per store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Events ingested through this store.
    pub events_ingested: u64,
    /// Of those, events whose durable `state_at` row already existed (a replay).
    pub events_replayed: u64,
    /// `state_at` rows read from the store (cache misses).
    pub state_at_reads: u64,
    /// Event records read from the store (cache misses).
    pub records_read: u64,
}

/// A [`StateStore`] backed by any [`StateRepr`] implementation `R` over the `hs-kv` backend
/// `KV`. See the module docs; the concrete type production callers want is
/// [`ProductionStateStore`].
pub struct KvStateStore<R: StateRepr, KV: KvBackend> {
    room_version: RoomVersionId,
    rules: RoomVersionRules,
    repr: R,
    backend: KV,
    tables: Tables<KV::Keyspace>,
    caches: RefCell<Caches<R::Root>>,
    opened_at: Instant,
    events_ingested: Cell<u64>,
    events_replayed: Cell<u64>,
    state_at_reads: Cell<u64>,
    records_read: Cell<u64>,
}

/// The production [`StateStore`]: [`KvStateStore`] instantiated with the bake-off's winning
/// representation (`crate::frames::FrameRepr`) over a caller-chosen `hs_kv::KvBackend`. This is
/// what tracks 04 and 06 should hold one of per room -- see
/// `docs/status/02-state-and-model.md`'s "Interfaces provided" for the call patterns.
pub type ProductionStateStore<KV> = KvStateStore<FrameRepr<KV>, KV>;

/// What one ingestion computed, for the caches once it has committed.
struct Ingested<Root> {
    root: Root,
    event_id: OwnedEventId,
    position: Option<(ChainPosition, StateKeyId)>,
}

impl<R: StateRepr, KV: KvBackend> KvStateStore<R, KV> {
    /// Wraps `repr` as a `StateStore` for a room of `room_version`, opening the durable record
    /// keyspaces on `backend` (the same backend `repr` writes to) with the default cache sizes.
    ///
    /// # Errors
    /// Returns [`KvStoreError::UnsupportedRoomVersion`] if `room_version` is not in
    /// [`hs_model::room_version`]'s table, or [`KvStoreError::Durable`] if a keyspace could not
    /// be opened.
    pub fn new(
        room_version: RoomVersionId,
        repr: R,
        backend: KV,
    ) -> Result<Self, KvStoreError<R::Error>> {
        Self::with_cache_sizes(room_version, repr, backend, CacheSizes::default())
    }

    /// [`KvStateStore::new`] with explicit cache sizes.
    ///
    /// # Errors
    /// As [`KvStateStore::new`].
    pub fn with_cache_sizes(
        room_version: RoomVersionId,
        repr: R,
        backend: KV,
        sizes: CacheSizes,
    ) -> Result<Self, KvStoreError<R::Error>> {
        let started = Instant::now();
        let rules = room_version::rules_for(&room_version).ok_or_else(|| {
            KvStoreError::UnsupportedRoomVersion(room_version.as_str().to_owned())
        })?;
        let tables = Tables::open(&backend)?;
        crate::metrics::observe_open(started.elapsed());
        Ok(Self {
            room_version,
            rules,
            repr,
            backend,
            tables,
            caches: RefCell::new(Caches::new(sizes)),
            opened_at: started,
            events_ingested: Cell::new(0),
            events_replayed: Cell::new(0),
            state_at_reads: Cell::new(0),
            records_read: Cell::new(0),
        })
    }

    /// The empty state.
    #[must_use]
    pub fn empty_root(&self) -> R::Root {
        self.repr.empty_root()
    }

    /// The backend this store writes to.
    #[must_use]
    pub fn backend(&self) -> &KV {
        &self.backend
    }

    /// What this store has read and written since it was opened.
    #[must_use]
    pub fn stats(&self) -> StoreStats {
        StoreStats {
            events_ingested: self.events_ingested.get(),
            events_replayed: self.events_replayed.get(),
            state_at_reads: self.state_at_reads.get(),
            records_read: self.records_read.get(),
        }
    }

    /// How many event records this store holds in its cache right now.
    #[must_use]
    pub fn resolution_events_cached(&self) -> usize {
        self.caches.borrow().records.len()
    }

    /// The interned `(event_type, state_key)` id for `key`, allocating a fresh one if unseen.
    /// The same as [`StateStore::intern_state_key`]; kept under its old name for the corpus
    /// generators and the bake-off harness.
    ///
    /// # Errors
    /// Returns [`KvStoreError::Durable`] on a storage failure.
    pub fn intern(
        &self,
        event_type: &str,
        state_key: &str,
    ) -> Result<StateKeyId, KvStoreError<R::Error>> {
        self.intern_state_key(event_type, state_key)
    }

    /// Ingests one event in a transaction of its own; see [`KvStateStore::ingest_in`].
    ///
    /// # Errors
    /// As [`KvStateStore::ingest_in`].
    pub fn ingest(
        &self,
        event: &NewEvent<'_>,
        explicit_state: Option<&[EventSn]>,
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        // `transact` retries on conflict and carries only `KvError`; any other error of ours is
        // parked here and returned once the closure has given up.
        let parked: RefCell<Option<KvStoreError<R::Error>>> = RefCell::new(None);
        let outcome = transact(&self.backend, TransactConfig::default(), |txn| {
            match self.ingest_locked(txn, event, explicit_state) {
                Ok(ingested) => Ok(ingested),
                Err(KvStoreError::Durable(DurableError::Kv(e))) => Err(e),
                Err(other) => {
                    *parked.borrow_mut() = Some(other);
                    Err(KvError::backend(std::io::Error::other(
                        "state store ingestion failed",
                    )))
                }
            }
        });
        let ingested = match outcome {
            Ok(ingested) => ingested,
            Err(e) => {
                return Err(parked.borrow_mut().take().unwrap_or_else(|| e.into()));
            }
        };
        self.remember(event.event, &ingested);
        Ok(ingested.root)
    }

    /// Ingests one event inside the caller's transaction `txn` and returns the root of the state
    /// after it. The event's `state_at` row, its record, its ID and its chain-cover position
    /// are written to `txn`, so they commit (or not) with whatever else the caller writes --
    /// a room actor's event row, timeline row and extremities. The state frames themselves are
    /// content-addressed and written in their own transactions as the representation always
    /// has; one left behind by a transaction that did not commit is unreferenced and harmless.
    ///
    /// With `explicit_state == None`, the state before the event is derived from its
    /// `prev_events`: the `state_at` of each one this store knows, resolved if there are
    /// several (exactly [`crate::store::InMemoryStateStore::add_event`]). With
    /// `Some(state)`, it is instead `state`, the complete list of events -- one per
    /// `(event_type, state_key)` -- that make up the room's resolved state immediately before
    /// the event, every entry of which must already have been ingested (by any ingestion
    /// method, in any order; its key is read back from what was recorded then). That is what a
    /// room bootstrapped from a federation `send_join` response needs
    /// (`docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md`): the resident's `state`
    /// *is* the state before the join, and `prev_events` (recorded, but playing no part in the
    /// state) are events this server does not hold.
    ///
    /// Ingesting an event this store already holds recomputes and overwrites its rows with the
    /// same values, and counts as a replay (`hs_state_events_replayed_total`).
    ///
    /// Nothing is cached by this method: the transaction may yet conflict and be retried. The
    /// next read of what it wrote fills the caches from the committed rows.
    ///
    /// # Errors
    /// Returns [`KvStoreError::UnknownEvent`] naming the first entry of `explicit_state` this
    /// store has not ingested (nothing is written in that case), [`KvStoreError::Repr`] if the
    /// representation fails, [`KvStoreError::Durable`] on a storage failure, or
    /// [`KvStoreError::StateRes`] if resolving several `prev_events` fails.
    pub fn ingest_in(
        &self,
        txn: &mut KV::Txn,
        event: &NewEvent<'_>,
        explicit_state: Option<&[EventSn]>,
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        Ok(self.ingest_locked(txn, event, explicit_state)?.root)
    }

    /// Ingests one event exactly as [`crate::store::InMemoryStateStore::add_event`] does, in a
    /// transaction of its own. See [`KvStateStore::ingest_in`].
    ///
    /// # Errors
    /// As [`KvStateStore::ingest_in`].
    #[allow(clippy::too_many_arguments)]
    pub fn add_event(
        &self,
        event: EventSn,
        event_id: OwnedEventId,
        room_id: OwnedRoomId,
        event_type: &str,
        state_key: Option<&str>,
        sender: OwnedUserId,
        content: CanonicalJsonObject,
        depth: i64,
        origin_server_ts: i64,
        auth_events: &[EventSn],
        prev_events: &[EventSn],
        only_prev_event_is_room_create: bool,
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        let new_event = NewEvent {
            event,
            event_id,
            room_id,
            event_type,
            state_key,
            sender,
            content,
            depth,
            origin_server_ts,
            auth_events,
            prev_events,
            only_prev_event_is_room_create,
        };
        self.ingest(&new_event, None)
    }

    /// Ingests one event with an explicit state before it, in a transaction of its own. See
    /// [`KvStateStore::ingest_in`]'s `explicit_state`.
    ///
    /// # Errors
    /// As [`KvStateStore::ingest_in`].
    #[allow(clippy::too_many_arguments)]
    pub fn add_event_with_state(
        &self,
        event: EventSn,
        event_id: OwnedEventId,
        room_id: OwnedRoomId,
        event_type: &str,
        state_key: Option<&str>,
        sender: OwnedUserId,
        content: CanonicalJsonObject,
        depth: i64,
        origin_server_ts: i64,
        auth_events: &[EventSn],
        prev_events: &[EventSn],
        only_prev_event_is_room_create: bool,
        state: &[EventSn],
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        let new_event = NewEvent {
            event,
            event_id,
            room_id,
            event_type,
            state_key,
            sender,
            content,
            depth,
            origin_server_ts,
            auth_events,
            prev_events,
            only_prev_event_is_room_create,
        };
        self.ingest(&new_event, Some(state))
    }

    /// Whether this store holds `event`: a cached or point read of its `state_at` row.
    ///
    /// # Errors
    /// Returns [`KvStoreError::Durable`] on a storage failure.
    pub fn has_event(&self, event: EventSn) -> Result<bool, KvStoreError<R::Error>> {
        if self.caches.borrow().state_at.contains(&event) {
            return Ok(true);
        }
        let snapshot = self.backend.snapshot();
        let reader = self.reader(&snapshot);
        Ok(reader.state_at_bytes(event)?.is_some())
    }

    /// The layout version `room_id`'s durable records were marked complete at, if ever.
    ///
    /// # Errors
    /// Returns [`KvStoreError::Durable`] on a storage failure.
    pub fn layout_version(&self, room_id: &RoomId) -> Result<Option<u32>, KvStoreError<R::Error>> {
        let snapshot = self.backend.snapshot();
        Ok(self.reader(&snapshot).layout_version(room_id.as_bytes())?)
    }

    /// Whether `room_id`'s durable records must be built from its events before an open can
    /// trust them: no marker, or one from an older [`LAYOUT_VERSION`]. A room actor that finds
    /// this true feeds the store the room's history once and calls
    /// [`KvStateStore::mark_migrated`]; one that finds it false reads current state only.
    ///
    /// # Errors
    /// Returns [`KvStoreError::Durable`] on a storage failure.
    pub fn needs_migration(&self, room_id: &RoomId) -> Result<bool, KvStoreError<R::Error>> {
        Ok(self.layout_version(room_id)? != Some(LAYOUT_VERSION))
    }

    /// Marks `room_id`'s durable records complete at [`LAYOUT_VERSION`], in a transaction of
    /// its own, counting `hs_state_migrations_total` and logging the room and how many events
    /// were fed at `info`. `events` is how many the caller replayed to get here (0 for a room
    /// created after the change, which a caller marks as it creates it).
    ///
    /// # Errors
    /// Returns [`KvStoreError::Durable`] on a storage failure.
    pub fn mark_migrated(
        &self,
        room_id: &RoomId,
        events: u64,
    ) -> Result<(), KvStoreError<R::Error>> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let mut writer = Writer::new(&self.tables, txn, &self.caches);
            writer
                .put_layout_version(room_id.as_bytes(), LAYOUT_VERSION)
                .map_err(durable_to_kv)
        })?;
        if events > 0 {
            crate::metrics::count_migration();
            tracing::info!(
                room_id = %room_id,
                events,
                layout_version = LAYOUT_VERSION,
                elapsed_ms = self.opened_at.elapsed().as_millis() as u64,
                "built the room's durable state records from its events; later opens read \
                 current state only"
            );
        }
        Ok(())
    }

    /// [`KvStateStore::mark_migrated`] inside the caller's transaction, with no logging or
    /// counting: for a room created after the change, marked in the transaction that creates it.
    ///
    /// # Errors
    /// Returns [`KvStoreError::Durable`] on a storage failure.
    pub fn mark_migrated_in(
        &self,
        txn: &mut KV::Txn,
        room_id: &RoomId,
    ) -> Result<(), KvStoreError<R::Error>> {
        let mut writer = Writer::new(&self.tables, txn, &self.caches);
        writer.put_layout_version(room_id.as_bytes(), LAYOUT_VERSION)?;
        Ok(())
    }

    /// Logs, at `info`, what opening this store cost -- events replayed into it versus rows
    /// read from it -- when the room is large (more than [`OPEN_SUMMARY_THRESHOLD`] events
    /// ingested or `state_at` rows read since the open). A room actor calls it at the end of
    /// its load.
    pub fn log_open_summary(&self, room_id: &RoomId) {
        let stats = self.stats();
        if stats.events_ingested + stats.state_at_reads < OPEN_SUMMARY_THRESHOLD {
            return;
        }
        tracing::info!(
            room_id = %room_id,
            replayed = stats.events_replayed,
            ingested = stats.events_ingested,
            state_rows_read = stats.state_at_reads,
            records_read = stats.records_read,
            elapsed_ms = self.opened_at.elapsed().as_millis() as u64,
            "state store open summary"
        );
    }

    fn reader<'a, Rd: KvRead<Keyspace = KV::Keyspace>>(
        &'a self,
        kv: &'a Rd,
    ) -> Reader<'a, KV::Keyspace, Rd, R::Root> {
        Reader::new(&self.tables, kv, &self.caches)
    }

    /// The root recorded for `event` through `reader`, cached if seen before.
    fn state_at_via<Rd: KvRead<Keyspace = KV::Keyspace>>(
        &self,
        reader: &Reader<'_, KV::Keyspace, Rd, R::Root>,
        event: EventSn,
    ) -> Result<Option<R::Root>, KvStoreError<R::Error>> {
        if let Some(root) = self.caches.borrow_mut().state_at.get(&event) {
            return Ok(Some(*root));
        }
        let Some(bytes) = reader.state_at_bytes(event)? else {
            return Ok(None);
        };
        self.state_at_reads.set(self.state_at_reads.get() + 1);
        let root = self.repr.decode_root(&bytes).ok_or(DurableError::Corrupt {
            keyspace: durable::KS_STATE_AT,
        })?;
        self.caches.borrow_mut().state_at.insert(event, root);
        Ok(Some(root))
    }

    /// Fills the caches with what a committed ingestion wrote.
    fn remember(&self, event: EventSn, ingested: &Ingested<R::Root>) {
        let mut caches = self.caches.borrow_mut();
        caches.state_at.insert(event, ingested.root);
        caches.event_ids.insert(event, ingested.event_id.clone());
        if let Some(position) = ingested.position {
            caches.chain_pos.insert(event, position);
        }
    }

    fn ingest_locked(
        &self,
        txn: &mut KV::Txn,
        event: &NewEvent<'_>,
        explicit_state: Option<&[EventSn]>,
    ) -> Result<Ingested<R::Root>, KvStoreError<R::Error>> {
        let mut writer = Writer::new(&self.tables, txn, &self.caches);

        // The state before the event, computed before anything is written so an unknown
        // explicit-state entry leaves the store untouched.
        let state_before = match explicit_state {
            Some(state) => {
                let mut added = BTreeMap::new();
                for &sn in state {
                    let (_, key) = writer
                        .position_of(sn)?
                        .ok_or(KvStoreError::UnknownEvent(sn))?;
                    added.insert(key, sn);
                }
                if added.is_empty() {
                    self.repr.empty_root()
                } else {
                    let diff = StateDiff {
                        added,
                        removed: std::collections::BTreeSet::new(),
                    };
                    self.repr
                        .apply(self.repr.empty_root(), &diff)
                        .map_err(KvStoreError::Repr)?
                }
            }
            None => {
                let mut prev_roots: Vec<R::Root> = Vec::with_capacity(event.prev_events.len());
                for &sn in event.prev_events {
                    if let Some(root) = self.state_at_via(&writer.reader(), sn)? {
                        prev_roots.push(root);
                    }
                }
                match prev_roots.len() {
                    0 => self.repr.empty_root(),
                    1 => prev_roots[0],
                    _ => self.resolve_with(&writer.reader(), &prev_roots)?,
                }
            }
        };

        let already = writer.reader().state_at_bytes(event.event)?.is_some();

        // The record: `auth_events`/`prev_events` narrowed to the entries this store knows, as
        // the in-memory store always recorded them.
        let known_auth: Vec<EventSn> = {
            let reader = writer.reader();
            let ids = reader.event_ids(event.auth_events)?;
            event
                .auth_events
                .iter()
                .zip(ids)
                .filter_map(|(sn, id)| id.map(|_| *sn))
                .collect()
        };
        let known_prev: Vec<EventSn> = {
            let reader = writer.reader();
            let ids = reader.event_ids(event.prev_events)?;
            event
                .prev_events
                .iter()
                .zip(ids)
                .filter_map(|(sn, id)| id.map(|_| *sn))
                .collect()
        };
        let record = EventRecord {
            event_id: event.event_id.clone(),
            room_id: event.room_id.clone(),
            event_type: event.event_type.to_owned(),
            state_key: event.state_key.unwrap_or_default().to_owned(),
            sender: event.sender.clone(),
            content: event.content.clone(),
            depth: event.depth,
            origin_server_ts: event.origin_server_ts,
            auth_events: known_auth,
            prev_events: known_prev,
            only_prev_event_is_room_create: event.only_prev_event_is_room_create,
        };
        writer.put_record(event.event, &record)?;

        // The event's own entry, if it is a state event, on top of the state before it.
        let mut position = None;
        let new_root = if let Some(state_key) = event.state_key {
            let key_id = writer.intern_key(event.event_type, state_key)?;
            let root = self
                .repr
                .apply(state_before, &StateDiff::set(key_id, event.event))
                .map_err(KvStoreError::Repr)?;
            // A replayed event keeps the position it was given; positions are never reassigned.
            match writer.position_of(event.event)? {
                Some(existing) => position = Some(existing),
                None => {
                    let placed = chain_cover::add_event(
                        &mut writer,
                        event.event,
                        key_id,
                        event.auth_events,
                    )?;
                    position = Some((placed, key_id));
                }
            }
            root
        } else {
            state_before
        };
        writer.put_state_at(event.event, &self.repr.encode_root(new_root))?;

        self.events_ingested.set(self.events_ingested.get() + 1);
        if already {
            self.events_replayed.set(self.events_replayed.get() + 1);
            crate::metrics::count_replayed(1);
        }

        Ok(Ingested {
            root: new_root,
            event_id: event.event_id.clone(),
            position,
        })
    }

    /// Resolves `forks` reading records through `reader`.
    fn resolve_with<Rd: KvRead<Keyspace = KV::Keyspace>>(
        &self,
        reader: &Reader<'_, KV::Keyspace, Rd, R::Root>,
        forks: &[R::Root],
    ) -> Result<R::Root, KvStoreError<R::Error>> {
        if forks.iter().all(|r| *r == forks[0]) {
            return Ok(forks[0]);
        }

        let scratch: IdScratch = RefCell::new(HashMap::new());
        let mut state_maps: Vec<state_res::StateMap> = Vec::with_capacity(forks.len());
        for root in forks {
            let full = self.repr.full_state(*root).map_err(KvStoreError::Repr)?;
            let keys: Vec<StateKeyId> = full.keys().copied().collect();
            let sns: Vec<EventSn> = full.values().copied().collect();
            let pairs = reader.key_pairs(&keys)?;
            let ids = reader.event_ids(&sns)?;
            let mut map = state_res::StateMap::new();
            let mut scratch = scratch.borrow_mut();
            for ((pair, id), sn) in pairs.into_iter().zip(ids).zip(sns) {
                let (Some(pair), Some(id)) = (pair, id) else {
                    continue;
                };
                scratch.insert(id.clone(), sn);
                map.insert(pair, id);
            }
            state_maps.push(map);
        }

        let fetch = KvEventFetch {
            reader,
            scratch: &scratch,
            arena: FrozenMap::new(),
            records_read: &self.records_read,
        };
        let resolved: state_res::StateMap = match self.rules.state_res {
            StateResolutionVersion::V1 => state_res::v1::resolve(&self.rules, &state_maps, &fetch)?,
            StateResolutionVersion::V2 { .. } => {
                state_res::v2::resolve(&self.room_version, &state_maps, &fetch)?
            }
        };

        let mut resolved_map = BTreeMap::new();
        for ((event_type, state_key), id) in resolved {
            let Some(&sn) = scratch.borrow().get(&id) else {
                continue;
            };
            // Every key of a resolved map came from one of the input maps, so it is interned.
            let Some(key_id) = reader.key_id(&event_type, &state_key)? else {
                continue;
            };
            resolved_map.insert(key_id, sn);
        }

        // Base the result on one fork (the first) rather than rebuilding from the empty state:
        // a representation with structural sharing (candidates B and C) only benefits from that
        // sharing if resolution's *write* is expressed as a diff against something it already
        // has, not as "every key, from scratch," which would defeat the whole point of measuring
        // structural sharing under "resolution time on forks."
        let base = forks[0];
        let base_map = self.repr.full_state(base).map_err(KvStoreError::Repr)?;
        let mut added = BTreeMap::new();
        for (key, sn) in &resolved_map {
            if base_map.get(key) != Some(sn) {
                added.insert(*key, *sn);
            }
        }
        let removed = base_map
            .keys()
            .filter(|key| !resolved_map.contains_key(key))
            .copied()
            .collect();
        let diff = StateDiff { added, removed };
        self.repr.apply(base, &diff).map_err(KvStoreError::Repr)
    }
}

fn durable_to_kv(e: DurableError) -> KvError {
    match e {
        DurableError::Kv(kv) => kv,
        other => KvError::backend(std::io::Error::other(other.to_string())),
    }
}

/// The [`EventFetch`] over the durable records for one resolution: records are read on first
/// use, kept in an append-only arena for the resolution's lifetime (so the resolvers can hold
/// references), and the auth chains come from the chain-cover index.
struct KvEventFetch<'a, Ks, Rd, Root> {
    reader: &'a Reader<'a, Ks, Rd, Root>,
    scratch: &'a IdScratch,
    arena: FrozenMap<OwnedEventId, Box<ResolutionEvent>>,
    records_read: &'a Cell<u64>,
}

impl<Ks, Rd: KvRead<Keyspace = Ks>, Root: Copy> KvEventFetch<'_, Ks, Rd, Root> {
    fn load(&self, id: &EventId) -> Result<Option<&ResolutionEvent>, DurableError> {
        let sn = match self.scratch.borrow().get(id) {
            Some(sn) => *sn,
            None => return Ok(None),
        };
        let cached_before = self.reader.caches.borrow().records.contains(&sn);
        let Some(record) = self.reader.record(sn)? else {
            return Ok(None);
        };
        if !cached_before {
            self.records_read.set(self.records_read.get() + 1);
        }
        let auth_ids = self.ids_of(&record.auth_events)?;
        let prev_ids = self.ids_of(&record.prev_events)?;
        let event = ResolutionEvent {
            event_id: record.event_id.clone(),
            room_id: record.room_id.clone(),
            event_type: record.event_type.clone(),
            state_key: record.state_key.clone(),
            sender: record.sender.clone(),
            content: record.content.clone(),
            depth: record.depth,
            origin_server_ts: record.origin_server_ts,
            auth_events: auth_ids,
            prev_events: prev_ids,
            only_prev_event_is_room_create: record.only_prev_event_is_room_create,
        };
        Ok(Some(self.arena.insert(id.to_owned(), Box::new(event))))
    }

    /// The IDs of `sns`, each remembered in the scratch map; an unknown one is left out.
    fn ids_of(&self, sns: &[EventSn]) -> Result<Vec<OwnedEventId>, DurableError> {
        let ids = self.reader.event_ids(sns)?;
        let mut scratch = self.scratch.borrow_mut();
        Ok(sns
            .iter()
            .zip(ids)
            .filter_map(|(sn, id)| {
                let id = id?;
                scratch.insert(id.clone(), *sn);
                Some(id)
            })
            .collect())
    }
}

impl<Ks, Rd: KvRead<Keyspace = Ks>, Root: Copy> EventFetch for KvEventFetch<'_, Ks, Rd, Root> {
    fn fetch(&self, id: &EventId) -> Option<&ResolutionEvent> {
        if let Some(event) = self.arena.get(id) {
            return Some(event);
        }
        match self.load(id) {
            Ok(event) => event,
            Err(error) => {
                // `EventFetch` has no error channel (a missing event and a failed read look the
                // same to a resolver, which then reports a missing event); the cause is logged.
                tracing::warn!(event_id = %id, %error, "could not read an event record for state resolution");
                None
            }
        }
    }

    fn auth_chain(&self, ids: &[OwnedEventId]) -> EventIdSet<OwnedEventId> {
        let sns: Vec<EventSn> = {
            let scratch = self.scratch.borrow();
            ids.iter()
                .filter_map(|id| scratch.get(id).copied())
                .collect()
        };
        // The union of the auth chains of `sns` is their auth difference against nothing.
        let chain = match chain_cover::auth_chain_difference(self.reader, &[sns, Vec::new()]) {
            Ok(chain) => chain,
            Err(error) => {
                tracing::warn!(%error, "could not read the chain-cover index for state resolution");
                return EventIdSet::new();
            }
        };
        match self.ids_of(&chain) {
            Ok(ids) => ids.into_iter().collect(),
            Err(error) => {
                tracing::warn!(%error, "could not read event ids for an auth chain");
                EventIdSet::new()
            }
        }
    }
}

impl<KV: KvBackend> KvStateStore<FrameRepr<KV>, KV> {
    /// Opens the production state store for a room of `room_version` over `backend`: the
    /// convenience constructor tracks 04 and 06 should use instead of building a `FrameRepr` and
    /// wrapping it by hand. Opens keyspaces and nothing else; see the module docs.
    ///
    /// # Errors
    /// Returns [`KvStoreError::UnsupportedRoomVersion`] if `room_version` is not in
    /// [`hs_model::room_version`]'s table, [`KvStoreError::Repr`] if `backend` could not open
    /// the frames keyspace, or [`KvStoreError::Durable`] if a record keyspace could not be opened.
    pub fn open(
        room_version: RoomVersionId,
        backend: KV,
    ) -> Result<Self, KvStoreError<crate::frames::Error>> {
        let repr = FrameRepr::new(backend.clone()).map_err(KvStoreError::Repr)?;
        Self::new(room_version, repr, backend)
    }
}

impl<R: StateRepr, KV: KvBackend> StateStore for KvStateStore<R, KV> {
    type Root = R::Root;
    type Error = KvStoreError<R::Error>;

    fn intern_state_key(
        &self,
        event_type: &str,
        state_key: &str,
    ) -> Result<StateKeyId, Self::Error> {
        {
            let snapshot = self.backend.snapshot();
            if let Some(id) = self.reader(&snapshot).key_id(event_type, state_key)? {
                return Ok(id);
            }
        }
        let id = transact(&self.backend, TransactConfig::default(), |txn| {
            let mut writer = Writer::new(&self.tables, txn, &self.caches);
            writer
                .intern_key(event_type, state_key)
                .map_err(durable_to_kv)
        })?;
        let mut caches = self.caches.borrow_mut();
        let pair = (event_type.to_owned(), state_key.to_owned());
        caches.key_fwd.insert(pair.clone(), id);
        caches.key_rev.insert(id, pair);
        Ok(id)
    }

    fn state_at(&self, event: EventSn) -> Result<R::Root, Self::Error> {
        if let Some(root) = self.caches.borrow_mut().state_at.get(&event) {
            return Ok(*root);
        }
        let snapshot = self.backend.snapshot();
        self.state_at_via(&self.reader(&snapshot), event)?
            .ok_or(KvStoreError::UnknownEvent(event))
    }

    fn get(&self, root: R::Root, key: StateKeyId) -> Result<Option<EventSn>, Self::Error> {
        self.repr.get(root, key).map_err(KvStoreError::Repr)
    }

    fn diff(&self, from: R::Root, to: R::Root) -> Result<StateDiff, Self::Error> {
        self.repr.diff(from, to).map_err(KvStoreError::Repr)
    }

    fn apply(&self, root: R::Root, changes: &StateDiff) -> Result<R::Root, Self::Error> {
        self.repr.apply(root, changes).map_err(KvStoreError::Repr)
    }

    fn resolve(
        &self,
        room_version: &RoomVersionId,
        forks: &[R::Root],
    ) -> Result<R::Root, Self::Error> {
        if forks.is_empty() {
            return Err(KvStoreError::EmptyForks);
        }
        if *room_version != self.room_version {
            return Err(KvStoreError::WrongRoomVersion);
        }
        let snapshot = self.backend.snapshot();
        self.resolve_with(&self.reader(&snapshot), forks)
    }

    fn chain_position(&self, event: EventSn) -> Result<Option<ChainPosition>, Self::Error> {
        let snapshot = self.backend.snapshot();
        Ok(self.reader(&snapshot).position_of(event)?.map(|(p, _)| p))
    }

    fn auth_chain_contains(
        &self,
        event: EventSn,
        ancestor: EventSn,
    ) -> Result<Option<bool>, Self::Error> {
        let snapshot = self.backend.snapshot();
        Ok(chain_cover::contains(
            &self.reader(&snapshot),
            event,
            ancestor,
        )?)
    }

    fn auth_chain_difference(&self, sets: &[Vec<EventSn>]) -> Result<Vec<EventSn>, Self::Error> {
        let snapshot = self.backend.snapshot();
        Ok(chain_cover::auth_chain_difference(
            &self.reader(&snapshot),
            sets,
        )?)
    }
}

/// The same fork-and-merge scenario `crate::store::InMemoryStateStore`'s tests exercise, run
/// through `KvStateStore` for every bake-off candidate, over `hs_kv::memory::MemoryBackend`. This
/// is the correctness gate the bake-off's numbers depend on: a candidate that is fast but resolves
/// forks incorrectly would invalidate every other measurement, so this runs the full
/// ingest-then-resolve path (not just `StateRepr::get`/`diff`/`apply` in isolation, which each
/// candidate's own module already covers) before any candidate is trusted with real corpus data.
#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use hs_model::canonical::to_canonical_object;
    use ruma::{EventId, RoomId, UserId};
    use serde_json::json;

    use super::*;
    use crate::bakeoff::{PersistentMapRepr, SnapshotDeltaRepr};
    use crate::frames::FrameRepr;

    fn obj(v: serde_json::Value) -> CanonicalJsonObject {
        to_canonical_object(&v, true).unwrap()
    }

    /// Builds `create -> join -> power_levels -> (topic "a" | topic "b") -> merge` and asserts
    /// the merge event's resolved state carries the higher-depth topic, exactly like
    /// `crate::store::InMemoryStateStore`'s `fork_and_merge_resolves_through_the_trait`.
    fn fork_and_merge_resolves<R: StateRepr, KV: KvBackend>(store: KvStateStore<R, KV>) {
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = UserId::parse("@c:hs1").unwrap();

        store
            .add_event(
                EventSn::new(1),
                EventId::parse("$1:hs1").unwrap(),
                room_id.clone(),
                "m.room.create",
                Some(""),
                creator.clone(),
                obj(json!({"creator": creator.as_str()})),
                1,
                1,
                &[],
                &[],
                false,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(2),
                EventId::parse("$2:hs1").unwrap(),
                room_id.clone(),
                "m.room.member",
                Some(creator.as_str()),
                creator.clone(),
                obj(json!({"membership": "join"})),
                2,
                2,
                &[EventSn::new(1)],
                &[EventSn::new(1)],
                true,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(3),
                EventId::parse("$3:hs1").unwrap(),
                room_id.clone(),
                "m.room.power_levels",
                Some(""),
                creator.clone(),
                obj(json!({
                    "users": {creator.as_str(): 100},
                    "ban": 50, "kick": 50, "redact": 50, "invite": 0,
                    "users_default": 0, "events_default": 0, "state_default": 50,
                })),
                3,
                3,
                &[EventSn::new(1), EventSn::new(2)],
                &[EventSn::new(2)],
                false,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(4),
                EventId::parse("$4a:hs1").unwrap(),
                room_id.clone(),
                "m.room.topic",
                Some(""),
                creator.clone(),
                obj(json!({"topic": "a"})),
                4,
                4,
                &[EventSn::new(3), EventSn::new(2)],
                &[EventSn::new(3)],
                false,
            )
            .unwrap();

        store
            .add_event(
                EventSn::new(5),
                EventId::parse("$4b:hs1").unwrap(),
                room_id.clone(),
                "m.room.topic",
                Some(""),
                creator.clone(),
                obj(json!({"topic": "b"})),
                5,
                5,
                &[EventSn::new(3), EventSn::new(2)],
                &[EventSn::new(3)],
                false,
            )
            .unwrap();

        let merged_root = store
            .add_event(
                EventSn::new(6),
                EventId::parse("$6:hs1").unwrap(),
                room_id,
                "m.room.message",
                None,
                creator,
                obj(json!({"body": "merged"})),
                6,
                6,
                &[EventSn::new(3), EventSn::new(2)],
                &[EventSn::new(4), EventSn::new(5)],
                false,
            )
            .unwrap();

        let topic_key = store.intern("m.room.topic", "").unwrap();
        let winner = store.get(merged_root, topic_key).unwrap();
        assert_eq!(
            winner,
            Some(EventSn::new(5)),
            "the higher-depth candidate (branch B) must win the topic conflict"
        );

        assert!(store.chain_position(EventSn::new(3)).unwrap().is_some());
        assert_eq!(
            store
                .auth_chain_contains(EventSn::new(4), EventSn::new(1))
                .unwrap(),
            Some(true)
        );
    }

    #[test]
    fn candidate_a_snapshot_delta() {
        let backend = MemoryBackend::default();
        let repr = SnapshotDeltaRepr::new(backend.clone()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr, backend).unwrap();
        fork_and_merge_resolves(store);
    }

    #[test]
    fn candidate_b_frames() {
        let backend = MemoryBackend::default();
        let repr = FrameRepr::new(backend.clone()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr, backend).unwrap();
        fork_and_merge_resolves(store);
    }

    #[test]
    fn candidate_c_persistent_map() {
        let backend = MemoryBackend::default();
        let repr = PersistentMapRepr::new(backend.clone()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr, backend).unwrap();
        fork_and_merge_resolves(store);
    }

    /// `add_event_with_state`: the shape a room bootstrapped from a `send_join` response has.
    /// Three snapshot events are ingested as outliers -- with no `prev_events` at all, so their
    /// own `state_at` is deliberately meaningless -- and a join whose prev events the store has
    /// never seen is then ingested with the snapshot as its explicit state. Its `state_at` must
    /// be exactly the snapshot plus itself, its chain-cover position must reach the snapshot's
    /// ancestors, and naming an unknown snapshot entry must be an error that records nothing.
    #[test]
    fn add_event_with_state_seeds_the_state_from_an_explicit_snapshot() {
        let backend = MemoryBackend::default();
        let repr = FrameRepr::new(backend.clone()).unwrap();
        let store = KvStateStore::new(RoomVersionId::V11, repr, backend).unwrap();
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = UserId::parse("@c:hs1").unwrap();
        let joiner = UserId::parse("@j:hs2").unwrap();

        // The snapshot, as outliers: create, the creator's join, power levels. No prev events.
        store
            .add_event(
                EventSn::new(1),
                EventId::parse("$1:hs1").unwrap(),
                room_id.clone(),
                "m.room.create",
                Some(""),
                creator.clone(),
                obj(json!({"creator": creator.as_str()})),
                1,
                1,
                &[],
                &[],
                false,
            )
            .unwrap();
        store
            .add_event(
                EventSn::new(2),
                EventId::parse("$2:hs1").unwrap(),
                room_id.clone(),
                "m.room.member",
                Some(creator.as_str()),
                creator.clone(),
                obj(json!({"membership": "join"})),
                2,
                2,
                &[EventSn::new(1)],
                &[],
                true,
            )
            .unwrap();
        store
            .add_event(
                EventSn::new(3),
                EventId::parse("$3:hs1").unwrap(),
                room_id.clone(),
                "m.room.power_levels",
                Some(""),
                creator.clone(),
                obj(json!({"users": {creator.as_str(): 100}})),
                3,
                3,
                &[EventSn::new(1), EventSn::new(2)],
                &[],
                false,
            )
            .unwrap();

        // An unknown snapshot entry is refused before anything is recorded.
        let err = store
            .add_event_with_state(
                EventSn::new(4),
                EventId::parse("$4:hs2").unwrap(),
                room_id.clone(),
                "m.room.member",
                Some(joiner.as_str()),
                joiner.clone(),
                obj(json!({"membership": "join"})),
                50,
                50,
                &[EventSn::new(1), EventSn::new(3)],
                &[EventSn::new(999)],
                false,
                &[
                    EventSn::new(1),
                    EventSn::new(2),
                    EventSn::new(3),
                    EventSn::new(42),
                ],
            )
            .unwrap_err();
        assert!(matches!(err, KvStoreError::UnknownEvent(sn) if sn == EventSn::new(42)));
        assert!(matches!(
            store.state_at(EventSn::new(4)),
            Err(KvStoreError::UnknownEvent(_))
        ));

        // The join, with the snapshot as its explicit state; its prev event ($999) is unknown.
        let root = store
            .add_event_with_state(
                EventSn::new(4),
                EventId::parse("$4:hs2").unwrap(),
                room_id,
                "m.room.member",
                Some(joiner.as_str()),
                joiner.clone(),
                obj(json!({"membership": "join"})),
                50,
                50,
                &[EventSn::new(1), EventSn::new(3)],
                &[EventSn::new(999)],
                false,
                &[EventSn::new(1), EventSn::new(2), EventSn::new(3)],
            )
            .unwrap();
        assert_eq!(store.state_at(EventSn::new(4)).unwrap(), root);

        let full = store.diff(store.empty_root(), root).unwrap();
        let mut sns: Vec<EventSn> = full.added.values().copied().collect();
        sns.sort();
        assert_eq!(
            sns,
            vec![
                EventSn::new(1),
                EventSn::new(2),
                EventSn::new(3),
                EventSn::new(4)
            ],
            "state_at(join) must be exactly the snapshot plus the join itself"
        );
        assert_eq!(
            store
                .get(
                    root,
                    store.intern("m.room.member", joiner.as_str()).unwrap()
                )
                .unwrap(),
            Some(EventSn::new(4))
        );
        assert_eq!(
            store
                .auth_chain_contains(EventSn::new(4), EventSn::new(1))
                .unwrap(),
            Some(true),
            "the join's chain-cover position must reach the create event through power levels"
        );
    }
}
