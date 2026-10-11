//! How a [`RoomActor`] reads and keeps event bodies now that it no longer holds them all:
//! the cache ([`super::event_cache`]), the store behind it, and the per-operation pins that
//! let `&self` reads hand out `&Event`.
//!
//! Three layers, in the order a read tries them:
//!
//! 1. **Pins** (`RoomActor::pins`): every event an operation has touched, pinned for the rest
//!    of that operation. An append-only map (`elsa::FrozenMap`) hands out `&Event` through
//!    `&self`, which is what keeps `event_by_id`, `members`, `events_around` and the rest of
//!    the read API returning references as they always have. The pins are cleared at the start
//!    of every operation on the actor ([`RoomActor::begin_operation`], called by
//!    `RoomActorHandle` under its lock), so they hold what one `/messages` page or one `/state`
//!    touched, never more.
//! 2. **The cache** (`RoomActor::cache`): the room's bounded, least-recently-used working set,
//!    `server.rooms.event_cache_size` events.
//! 3. **The store** (`Tables::events`): every event, durably. A read from here is a miss
//!    (`hs_room_event_cache_misses_total`), parsed back into an [`Event`] with the flags it was
//!    stored with, and decorated the way a load used to decorate it
//!    ([`RoomActor::decorate_from_store`]).
//!
//! An event is "held" by the room when the state store knows it or it is held as rejected
//! ([`RoomActor::known`]); a short ID the interning table answers for some other room's event
//! is not served. The whole-history `event_id_index` the actor used to carry is gone: an ID is
//! resolved through the cached events first and the interning table otherwise
//! ([`RoomActor::sn_of`], `hs_room_event_id_lookups_total`).

use std::sync::Arc;

use hs_kv::KvBackend;
use hs_model::Event;
use hs_model::event::EventFlags;
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use ruma::EventId;

use super::RoomActor;
use super::event_cache::CacheCapacity;
use super::redactions::{redacted_by, with_redaction};
use crate::error::RoomError;
use crate::persist::PersistedEvent;
use crate::pipeline::EventLookup;

impl<B: KvBackend> RoomActor<B> {
    /// Points this actor's event cache at `capacity` (the registry's shared, hot-reloadable
    /// one), shrinking it at once if it is over. Called by [`crate::registry::RoomRegistry`]
    /// right after constructing or loading an actor; an actor nobody installs one on keeps
    /// [`super::event_cache::DEFAULT_CAPACITY`].
    pub fn set_cache_capacity(&mut self, capacity: CacheCapacity) {
        self.cache.borrow_mut().set_capacity(capacity);
    }

    /// How many event bodies this actor holds in its cache right now.
    #[must_use]
    pub fn cached_events(&self) -> usize {
        self.cache.borrow().len()
    }

    /// Starts an operation on this actor: drops the pins of the previous one, so what a read
    /// touched is released when the next request begins. `RoomActorHandle` calls it under its
    /// lock before every closure it runs; code that drives an actor directly (tests, the
    /// importer) may call it between its own operations or let the pins live.
    pub fn begin_operation(&mut self) {
        self.pins.as_mut().clear();
    }

    /// How many events the current operation has pinned (see the module docs).
    #[must_use]
    pub fn pinned_events(&self) -> usize {
        self.pins.len()
    }

    /// The event `sn`, if this room holds it: from the pins, the cache, or the store, in that
    /// order, and cached when it came from the store. The reference lives as long as this
    /// borrow, pinned.
    pub(crate) fn event(&self, sn: EventSn) -> Option<&Event> {
        if let Some(event) = self.pins.get(&sn) {
            return Some(event);
        }
        let cached = self.cache.borrow_mut().get(sn);
        let event = match cached {
            Some(event) => event,
            None => {
                let (event, room_pos) = self.fetch_from_store(sn)?;
                self.cache
                    .borrow_mut()
                    .insert(sn, Arc::clone(&event), room_pos);
                event
            }
        };
        Some(self.pins.insert(sn, event))
    }

    /// [`RoomActor::event`] for a bulk read -- the whole state, a stretch of the timeline --
    /// that must not push the room's working set out of the cache on its way through: a miss
    /// is pinned for this operation and not cached.
    pub(crate) fn event_uncached(&self, sn: EventSn) -> Option<&Event> {
        if let Some(event) = self.pins.get(&sn) {
            return Some(event);
        }
        let cached = self.cache.borrow().peek(sn).cloned();
        let event = match cached {
            Some(event) => event,
            None => self.fetch_from_store(sn)?.0,
        };
        Some(self.pins.insert(sn, event))
    }

    /// Whether this room holds the event `sn`: fed to its state store, or held as rejected.
    /// What used to be "is in `events`".
    pub(crate) fn known(&self, sn: EventSn) -> bool {
        self.rejected.contains(&sn) || self.store.state_at(sn).is_ok()
    }

    /// The short ID of `event_id`, if this room holds the event: from a cached event's ID, or
    /// the interning table checked against [`RoomActor::known`].
    pub(crate) fn sn_of(&self, event_id: &EventId) -> Option<EventSn> {
        let cached = self.cache.borrow().sn_of(event_id);
        if let Some(sn) = cached {
            return Some(sn);
        }
        crate::metrics::count_event_id_lookup();
        let snapshot = self.backend.snapshot();
        let sn = match self.tables.event_sn.lookup(&snapshot, event_id.as_bytes()) {
            Ok(sn) => sn?,
            Err(error) => {
                tracing::warn!(room_id = %self.room_id, %event_id, %error, "could not look up an event id");
                return None;
            }
        };
        self.known(sn).then_some(sn)
    }

    /// Makes `event` the room's resident copy of `sn`: what every path that just persisted an
    /// event calls, so the event the next read wants is already here.
    pub(crate) fn hold(&mut self, sn: EventSn, event: Event, room_pos: Option<i64>) {
        self.pins.as_mut().remove(&sn);
        self.cache
            .borrow_mut()
            .insert(sn, Arc::new(event), room_pos);
    }

    /// The timeline position of the event `sn`, if it is in the timeline: what its stored row
    /// says (`PersistedEvent::room_pos`, kept with the cached copy), checked against the
    /// timeline, so a purged event (its row keeps its old position) answers `None` as it did
    /// when this walked the whole timeline. `None` for an outlier and for an event not held.
    pub(crate) fn room_pos_of(&self, sn: EventSn) -> Option<i64> {
        // Bound on its own line: a `match` scrutinee's borrow would live through the arms,
        // and the miss arm borrows the cache again to fill it.
        let peeked = self.cache.borrow().peek_room_pos(sn);
        let stored = match peeked {
            Some(stored) => stored,
            None => self.stored_room_pos(sn)?,
        };
        stored.filter(|pos| self.timeline.get(pos) == Some(&sn))
    }

    /// `sn`'s row's timeline position, reading the row (and caching the event) when it is not
    /// cached. `None` when the room does not hold `sn`.
    fn stored_room_pos(&self, sn: EventSn) -> Option<Option<i64>> {
        let (event, room_pos) = self.fetch_from_store(sn)?;
        self.cache.borrow_mut().insert(sn, event, room_pos);
        Some(room_pos)
    }

    /// Fills the cache with the newest timeline events, as many as it holds, so that a room
    /// just loaded answers its first `/sync` and its next send without a miss. Called at the
    /// end of [`RoomActor::load`], and by a caller that widened the window after it
    /// ([`RoomActor::set_cache_capacity`]) and wants it full; the pins it leaves are cleared.
    pub fn warm_cache(&mut self) {
        let capacity = self.cache.borrow().capacity();
        let newest: Vec<EventSn> = self
            .timeline
            .values()
            .rev()
            .take(capacity)
            .copied()
            .collect();
        // Oldest first, so that the newest end up most recently used.
        for sn in newest.into_iter().rev() {
            self.event(sn);
        }
        self.begin_operation();
    }

    /// Records that the stored row of `sn` now carries `room_pos`: what placing a held outlier
    /// in the timeline (`history`) tells the cache after rewriting the row.
    pub(crate) fn set_cached_room_pos(&mut self, sn: EventSn, room_pos: Option<i64>) {
        self.cache.borrow_mut().set_room_pos(sn, room_pos);
    }

    /// Changes the resident copy of `sn` in place, if there is one, after the stored row was
    /// changed the same way (a flag set). An event not resident needs nothing: its next read
    /// brings the row as it now is.
    pub(crate) fn update_cached(&mut self, sn: EventSn, change: impl FnOnce(&mut Event)) {
        self.pins.as_mut().remove(&sn);
        let mut cache = self.cache.borrow_mut();
        if let Some((mut event, room_pos)) = cache.remove(sn) {
            change(Arc::make_mut(&mut event));
            cache.insert(sn, event, room_pos);
        }
    }

    /// Reads the stored row of `sn` back into an [`Event`] with the flags it was stored with,
    /// and the row's timeline position. `Ok(None)` when there is no row.
    ///
    /// # Errors
    /// [`RoomError::Store`] on a storage failure, [`RoomError::Internal`] on a corrupt row.
    pub(crate) fn read_stored_event(
        &self,
        sn: EventSn,
    ) -> Result<Option<(Event, Option<i64>)>, RoomError> {
        let snapshot = self.backend.snapshot();
        let Some(bytes) = self.tables.events.get(&snapshot, &(sn,))? else {
            return Ok(None);
        };
        let persisted: PersistedEvent =
            serde_json::from_slice(&bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
        let mut event = Event::parse(&persisted.json, self.room_version.clone())?;
        *event.flags_mut() = EventFlags::from_byte(persisted.flags);
        Ok(Some((event, persisted.room_pos)))
    }

    fn fetch_from_store(&self, sn: EventSn) -> Option<(Arc<Event>, Option<i64>)> {
        if !self.known(sn) {
            return None;
        }
        crate::metrics::count_event_cache_miss();
        match self.read_stored_event(sn) {
            Ok(Some((event, room_pos))) => {
                Some((Arc::new(self.decorate_from_store(event)), room_pos))
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    room_id = %self.room_id,
                    event_sn = %sn,
                    %error,
                    "could not read an event the room holds back from the store"
                );
                None
            }
        }
    }

    /// What a load used to do to every event once (`name_redactions_on_load`, until
    /// 2026-10-10), done to each event as it is read from the store instead, so an evicted
    /// event comes back the same: an event redacted before redactions were kept in it
    /// (2026-10-01) is given the first redaction held for it, so it renders `redacted_because`
    /// like any other. Nothing is written: a read may run on a replica that does not own the
    /// room.
    fn decorate_from_store(&self, event: Event) -> Event {
        if !event.header().flags.is_redacted() || redacted_by(&event).is_some() {
            return event;
        }
        let Some(redaction_id) = self
            .redactions_by_target
            .get(event.event_id())
            .and_then(|redactions| redactions.first())
        else {
            return event;
        };
        let Some(redaction_sn) = self.sn_of(redaction_id) else {
            return event;
        };
        // Read raw, not through `event`: the redaction's own decoration must not recurse into
        // this one (a redaction redacted by the event it redacts).
        let Ok(Some((redaction, _))) = self.read_stored_event(redaction_sn) else {
            return event;
        };
        match with_redaction(&event, &redaction) {
            Ok((mut named, _)) => {
                *named.flags_mut() = event.header().flags;
                named
            }
            Err(_) => event,
        }
    }
}

impl<B: KvBackend> EventLookup for RoomActor<B> {
    fn event(&self, sn: EventSn) -> Option<&Event> {
        RoomActor::event(self, sn)
    }

    fn event_with_id(&self, event_id: &EventId) -> Option<&Event> {
        let sn = self.sn_of(event_id)?;
        RoomActor::event(self, sn)
    }
}
