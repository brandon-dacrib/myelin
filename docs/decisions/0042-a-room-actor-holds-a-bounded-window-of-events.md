# 0042: 2026-10-10: a room actor holds a bounded window of its events, least recently used

Status: accepted (track 04; a setting in 13's `hs-config`, metrics 12 scrapes).

## The problem

`RoomActor` held every event of its room in memory for its lifetime, and an index of every
event ID. A room's memory followed its history, and the demo, having joined
`#matrix:matrix.org` on 2026-10-10, sat at 1.2 GiB a minute after boot (`docs/status/04-room-and-events.md`,
2026-10-10).

## What was chosen

1. **The actor keeps a per-room, least-recently-used cache of event bodies**
   (`crates/hs-room/src/actor/event_cache.rs`), `server.rooms.event_cache_size` events (default
   1,000), shared capacity across rooms and hot. Everything else is read from `Tables::events`
   when a request needs it, counted as a miss. LRU, because a room's hot set is small and
   stable -- its extremities, create, power levels, join rules, the active senders'
   memberships, the last few pages -- and that is what least-recently-used keeps. Bulk reads
   (the whole state, a page of old timeline) go around the cache so a `/members` of a large
   room does not evict the working set.
2. **Reads keep returning `&Event`.** The actor pins what one operation touched in an
   append-only map (`elsa::FrozenMap<EventSn, Arc<Event>>`, cleared by `RoomActorHandle`
   before every closure), so the twenty-odd `&Event`-returning methods other crates call did
   not change. No `unsafe` in this crate for it; `elsa` is MIT/Apache-2.0.
3. **No whole-history ID index.** An event ID is resolved through the cached events, then the
   `event_sn` interning table, and accepted only if the room's state store knows the event or
   holds it as rejected. The old `event_id_index` was about 130 bytes per event.
4. **Positions come from the rows.** `room_pos_of` reads the event row's `room_pos` (kept with
   the cached copy) instead of walking the timeline; the "joined later" test of history
   visibility walks the requester's membership chain backwards from the current state instead
   of scanning every later event; `timestamp_to_event` bisects the timeline instead of reading
   every event. Each of these was O(history) in memory and would have been O(history) store
   reads.
5. **The state store is told less about messages.** `RoomActor::store_inputs` hands `hs-state`
   an empty `content` and no `auth_events` for an event that is not a state event: state
   resolution reads neither of a message, and the store keeps a record of every event it is
   given (RFC 0024).
6. **What is still O(history) is named, not hidden.** `hs-state`'s in-memory per-event records
   and the replay on load that fills them (RFC 0024, track 02); the actor's `timeline` map
   (about 30 bytes per event); `relations_by_target` (per relation) and `redactions_by_target`
   (per redaction).
7. **Operators can see it.** `hs_room_events_cached`, `hs_room_resident_rooms`,
   `hs_room_actors_alive`, `hs_room_event_cache_misses_total`,
   `hs_room_event_cache_evictions_total`, `hs_room_event_id_lookups_total`, and, new to the
   server, `process_resident_memory_bytes` and `process_virtual_memory_bytes`
   (`/proc/self/status` on Linux, `proc_pidinfo` on macOS -- the one `unsafe` call in
   `hs-room`, so the crate's `forbid(unsafe_code)` became `deny` with an allow on that
   function). An INFO line the first time a room evicts.
8. **An idle room can be unloaded.** `server.rooms.idle_unload_after` (unset by default): the
   registry's sweeper unloads rooms nobody used for that long, every minute. Off by default
   because a reload still replays history (RFC 0024) and the actor's in-memory-only state (the
   transaction-id dedup, `/forget`) is lost with it, as the status file has long recorded.

## Consequences

- A `RoomActor` is no longer `Sync` in spirit (its cache is a `RefCell`); it never was shared
  that way (one `tokio::sync::Mutex` per room), and the compiler holds the line.
- A read of an old event costs a key-value read and a parse (tens of microseconds); a page of
  100 old events a few milliseconds.
- Every path that changes a stored event row must also tell the cache (`hold`,
  `update_cached`, `set_cached_room_pos`); the code review rule is "the row first, then the
  cache", and `tests/working_set.rs` checks redaction, relations, state and positions beyond
  the window and across a reload.
