//! [`EventCache`]: the bounded working set of event bodies a [`super::RoomActor`] keeps in
//! memory, and the one place that decides what stays resident.
//!
//! Until 2026-10-10 the actor held every event of its room in a `HashMap<EventSn, Event>` for
//! its whole lifetime, so a room's memory grew with its history: the demo, having joined
//! `#matrix:matrix.org`, held about 1.2 GiB within a minute of boot. The store
//! (`crate::persist::Tables::events`) has always held every body durably; this cache is the
//! recently used ones, up to a capacity the operator sets (`server.rooms.event_cache_size`,
//! shared by every room and read on every insert, so a change applies at once). Anything not
//! in it is read from the store on demand ([`super::RoomActor::event`]), counted as a miss
//! (`hs_room_event_cache_misses_total`).
//!
//! The policy is least recently used: the hot set of a room is small and stable -- its
//! forward extremities (cited by every send), its create, power levels and join rules (cited
//! by every authorisation), the senders' memberships, and whatever the last few `/sync`s and
//! `/messages` pages touched -- and LRU keeps exactly that. Bulk reads that walk the whole
//! state or an old stretch of the timeline go around the cache
//! ([`super::RoomActor::event_uncached`]), so a `/members` of a 50,000-member room does not
//! evict the working set on its way through.
//!
//! What a cache cannot answer, the store does, and what must not be lost is written to the
//! store first: every change to an event body or its flags (a redaction, a purge, a soft-fail
//! verdict) is persisted by the code that makes it, and this cache is only told to drop or
//! replace its copy. The in-memory-only decoration of legacy redacted events
//! (`unsigned.redacted_by` for events redacted before 2026-10-01) is applied again on every
//! read from the store ([`super::RoomActor::decorate_from_store`]), so an evicted event comes
//! back exactly as it left.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hs_model::Event;
use hs_model::ids::EventSn;
use ruma::{EventId, OwnedEventId, OwnedRoomId};

/// The default `server.rooms.event_cache_size`: how many event bodies one room keeps resident.
/// A parsed event is a few kilobytes (its canonical JSON twice: as bytes and as a tree, plus
/// its header), so this is a few megabytes per room that has that much history, and nothing
/// extra for the many rooms that do not.
pub const DEFAULT_CAPACITY: usize = 1_000;

/// A shared, hot-reloadable capacity: one per [`crate::registry::RoomRegistry`], handed to
/// every actor it constructs or loads. `0` means no cache at all (every read goes to the
/// store), which is what a measurement of the store path wants and no operator does.
#[derive(Debug, Clone)]
pub struct CacheCapacity(Arc<AtomicUsize>);

impl CacheCapacity {
    /// A capacity of `n` events per room.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self(Arc::new(AtomicUsize::new(n)))
    }

    /// The capacity as it stands.
    #[must_use]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Changes the capacity. Caches over the new value shrink on their next insert.
    pub fn set(&self, n: usize) {
        self.0.store(n, Ordering::Relaxed);
    }
}

impl Default for CacheCapacity {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

struct Entry {
    event: Arc<Event>,
    /// The LRU clock value of the entry's last use; its key in `EventCache::order`.
    tick: u64,
    /// The event's timeline position as its stored row says (`PersistedEvent::room_pos`):
    /// `None` for an outlier. What `RoomActor::room_pos_of` answers from without a read.
    room_pos: Option<i64>,
}

/// One room's bounded, least-recently-used set of event bodies. See the module docs.
pub struct EventCache {
    capacity: CacheCapacity,
    room_id: OwnedRoomId,
    entries: HashMap<EventSn, Entry>,
    /// Event ID -> `EventSn` for the cached events only: the hot half of what used to be the
    /// actor's whole-history `event_id_index`. An ID not here is looked up in the interning
    /// table.
    by_id: HashMap<OwnedEventId, EventSn>,
    /// LRU clock value -> event: the oldest entry is the first key.
    order: BTreeMap<u64, EventSn>,
    clock: u64,
    /// Whether this room has evicted anything yet: the first eviction is logged once per
    /// room, so an operator can see which rooms outgrow the window.
    evicted_once: bool,
}

impl EventCache {
    /// An empty cache for `room_id` at `capacity`.
    #[must_use]
    pub fn new(room_id: OwnedRoomId, capacity: CacheCapacity) -> Self {
        crate::metrics::actor_constructed();
        Self {
            capacity,
            room_id,
            entries: HashMap::new(),
            by_id: HashMap::new(),
            order: BTreeMap::new(),
            clock: 0,
            evicted_once: false,
        }
    }

    /// How many events are resident.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The capacity this cache shrinks to on insert.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity.get()
    }

    /// Points this cache at `capacity` and shrinks to it at once.
    pub fn set_capacity(&mut self, capacity: CacheCapacity) {
        self.capacity = capacity;
        self.shrink_to_capacity();
    }

    /// The cached event `sn`, marked as just used.
    pub fn get(&mut self, sn: EventSn) -> Option<Arc<Event>> {
        let next = self.next_tick();
        let entry = self.entries.get_mut(&sn)?;
        self.order.remove(&entry.tick);
        entry.tick = next;
        self.order.insert(next, sn);
        Some(Arc::clone(&entry.event))
    }

    /// The cached event `sn` without marking it used.
    #[must_use]
    pub fn peek(&self, sn: EventSn) -> Option<&Arc<Event>> {
        self.entries.get(&sn).map(|entry| &entry.event)
    }

    /// The stored timeline position of the cached event `sn`: `Some(None)` for a cached
    /// outlier, `None` when `sn` is not cached.
    #[must_use]
    pub fn peek_room_pos(&self, sn: EventSn) -> Option<Option<i64>> {
        self.entries.get(&sn).map(|entry| entry.room_pos)
    }

    /// The `EventSn` of a cached event by ID.
    #[must_use]
    pub fn sn_of(&self, event_id: &EventId) -> Option<EventSn> {
        self.by_id.get(event_id).copied()
    }

    /// Whether `sn` is resident.
    #[must_use]
    pub fn contains(&self, sn: EventSn) -> bool {
        self.entries.contains_key(&sn)
    }

    /// Makes `event` the resident copy of `sn` (replacing one already there), then evicts the
    /// least recently used entries until the cache is within its capacity. With a capacity of
    /// zero nothing is kept.
    pub fn insert(&mut self, sn: EventSn, event: Arc<Event>, room_pos: Option<i64>) {
        let tick = self.next_tick();
        if let Some(old) = self.entries.insert(
            sn,
            Entry {
                event: Arc::clone(&event),
                tick,
                room_pos,
            },
        ) {
            self.order.remove(&old.tick);
            self.by_id.remove(old.event.event_id());
        } else {
            crate::metrics::events_cached_delta(1);
        }
        self.by_id.insert(event.event_id().to_owned(), sn);
        self.order.insert(tick, sn);
        self.shrink_to_capacity();
    }

    /// Records that the stored row of a cached event now says `room_pos` (an outlier placed in
    /// the timeline by backfill). Nothing to do when `sn` is not cached.
    pub fn set_room_pos(&mut self, sn: EventSn, room_pos: Option<i64>) {
        if let Some(entry) = self.entries.get_mut(&sn) {
            entry.room_pos = room_pos;
        }
    }

    /// Drops `sn`'s resident copy, if any, answering it and its stored position. The next
    /// read comes from the store.
    pub fn remove(&mut self, sn: EventSn) -> Option<(Arc<Event>, Option<i64>)> {
        let entry = self.entries.remove(&sn)?;
        self.order.remove(&entry.tick);
        self.by_id.remove(entry.event.event_id());
        crate::metrics::events_cached_delta(-1);
        Some((entry.event, entry.room_pos))
    }

    /// Drops everything resident.
    pub fn clear(&mut self) {
        let dropped = self.entries.len();
        self.entries.clear();
        self.by_id.clear();
        self.order.clear();
        crate::metrics::events_cached_delta(-(dropped as i64));
    }

    fn next_tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn shrink_to_capacity(&mut self) {
        let capacity = self.capacity.get();
        let mut evicted = 0u64;
        while self.entries.len() > capacity {
            let Some((&tick, &sn)) = self.order.iter().next() else {
                break;
            };
            self.order.remove(&tick);
            if let Some(entry) = self.entries.remove(&sn) {
                self.by_id.remove(entry.event.event_id());
            }
            evicted += 1;
        }
        if evicted > 0 {
            crate::metrics::events_cached_delta(-(evicted as i64));
            crate::metrics::count_event_cache_evictions(evicted);
            if !self.evicted_once {
                self.evicted_once = true;
                tracing::info!(
                    room_id = %self.room_id,
                    capacity,
                    "the room has more events than its event cache holds; older events are \
                     read from the store from now on (server.rooms.event_cache_size)"
                );
            }
        }
    }
}

impl Drop for EventCache {
    fn drop(&mut self) {
        crate::metrics::events_cached_delta(-(self.entries.len() as i64));
        crate::metrics::actor_dropped();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_model::canonical::to_canonical_object;
    use ruma::RoomVersionId;

    fn event(n: u64) -> Arc<Event> {
        let json = serde_json::json!({
            "type": "m.room.message",
            "sender": "@a:hs1",
            "room_id": "!r:hs1",
            "origin_server_ts": n,
            "depth": n,
            "content": {"body": format!("event {n}")},
            "prev_events": [],
            "auth_events": [],
        });
        // The canonical form is what `Event::parse` hashes an ID from; a distinct timestamp
        // makes each event distinct.
        let canonical = to_canonical_object(&json, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(
            &hs_model::canonical::CanonicalJsonValue::Object(canonical).to_canonical_bytes(),
        )
        .unwrap();
        Arc::new(Event::parse(&value, RoomVersionId::V11).unwrap())
    }

    fn cache(capacity: usize) -> EventCache {
        EventCache::new(
            OwnedRoomId::try_from("!r:hs1").unwrap(),
            CacheCapacity::new(capacity),
        )
    }

    #[test]
    fn keeps_the_most_recently_used_entries() {
        let mut cache = cache(2);
        let (a, b, c) = (event(1), event(2), event(3));
        cache.insert(EventSn::new(1), a.clone(), None);
        cache.insert(EventSn::new(2), b.clone(), None);
        // Touch `a` so that `b` is the least recently used.
        assert!(cache.get(EventSn::new(1)).is_some());
        cache.insert(EventSn::new(3), c.clone(), None);
        assert_eq!(cache.len(), 2);
        assert!(cache.contains(EventSn::new(1)));
        assert!(!cache.contains(EventSn::new(2)));
        assert!(cache.contains(EventSn::new(3)));
        assert_eq!(cache.sn_of(a.event_id()), Some(EventSn::new(1)));
        assert_eq!(cache.sn_of(b.event_id()), None);
    }

    #[test]
    fn a_shared_capacity_change_applies_on_the_next_insert() {
        let capacity = CacheCapacity::new(10);
        let mut cache = EventCache::new(OwnedRoomId::try_from("!r:hs1").unwrap(), capacity.clone());
        for n in 1..=10 {
            cache.insert(EventSn::new(n), event(n), None);
        }
        assert_eq!(cache.len(), 10);
        capacity.set(3);
        cache.insert(EventSn::new(11), event(11), None);
        assert_eq!(cache.len(), 3);
        assert!(cache.contains(EventSn::new(11)));
        assert!(cache.contains(EventSn::new(10)));
        assert!(cache.contains(EventSn::new(9)));
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let mut cache = cache(0);
        cache.insert(EventSn::new(1), event(1), None);
        assert!(cache.is_empty());
        assert!(cache.get(EventSn::new(1)).is_none());
    }

    #[test]
    fn replacing_an_entry_keeps_one_copy_and_one_id() {
        let mut cache = cache(5);
        cache.insert(EventSn::new(1), event(1), None);
        cache.insert(EventSn::new(1), event(1), None);
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.remove(EventSn::new(1)).map(|(e, _)| e.header().depth),
            Some(1)
        );
        assert!(cache.is_empty());
        assert_eq!(cache.sn_of(event(1).event_id()), None);
    }
}
