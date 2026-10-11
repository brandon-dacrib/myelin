# 0025. A room load that does not replay its history: the state store's per-event records must be durable

Status: **implemented in `hs-state`** (branch `agent/state-load`, 2026-10-10; decision 0045 is
left to write: a root per event over the frames' delta chains, not Synapse-style state groups).
The room actor's half (its load reading only current state, and feeding the store inside its own
transaction) is track 04's; the seams are in place. Originally proposed 2026-10-10 by track 04
on branch `agent/room-memory`. Owner of the change: track 02 (state and model). Affects:
`crates/hs-state/src/kv_store.rs` (`KvStateStore`), `crates/hs-state/src/durable.rs` (new),
then `crates/hs-room/src/actor.rs` (`RoomActor::load`).

## What landed (track 02, 2026-10-10, `agent/state-load`)

Each numbered need below, as `crates/hs-state` now answers it:

1. **`state_at` durable**: the `state_at` keyspace, `EventSn` to the frame root (16 bytes),
   written by every ingestion and read through a bounded cache (`KvStateStore::state_at`).
2. **Interning durable**: the `state_key_id_{fwd,rev,seq}` keyspaces in `hs-tables`'s own
   layout, so `hs_tables::interning::state_key_id_table` reads the same rows; a bounded cache
   each way instead of `InternTable`'s unbounded one. Rooms whose frames were written by the
   per-process numbering are rebuilt once: `KvStateStore::needs_migration(room_id)` says so,
   the actor's existing replay is the rebuild, `mark_migrated` ends it, `LAYOUT_VERSION` lets a
   future change ask for it again; `mark_migrated_in(txn)` marks a room as it is created.
3. **Records by short id, on demand**: the `state_event` keyspace holds a compact record per
   event (`crate::record`); state resolution reads only the records it touches through
   `state_res::EventFetch` (conflicted events, their auth chains, the state entries auth checks
   ask for), kept for the resolution in an arena and across resolutions in an LRU of 1,024
   (`hs_state_resolution_events_cached`).
4. **The chain-cover index durable**: `state_chain_{pos,event,link,tip,seq}`, the same
   algorithm as the in-memory `ChainCoverIndex` behind `chain_cover::{ChainReader,
   ChainWriter}`, read lazily; it also answers the resolvers' auth chains.
5. **Writes inside the caller's transaction**: `KvStateStore::ingest_in(txn, &NewEvent,
   explicit_state)`; the frames stay content-addressed in their own idempotent writes.
6. **`has_event(EventSn)`**: a cached or point read.

Metrics `hs_state_open_seconds`, `hs_state_events_replayed_total`,
`hs_state_resolution_events_cached`, `hs_state_migrations_total`,
`hs_state_event_records_read_total` (`hs_state::metrics::register_metrics`, for `hs-cli` to
call); `KvStateStore::log_open_summary(room_id)` logs replayed-versus-read counts for a room of
more than 10,000 events. Measured (`crates/hs-state/tests/open_memory.rs`, release, 50,000
events on Fjall): a replay of the room into the store 0.60 s and +55 MiB; an open that reads
current state 0.00 s and +0 MiB, zero records read, zero events ingested.

## The original proposal

## The problem

On 2026-10-10 the demo joined `#matrix:matrix.org` and held about 1.2 GiB within a minute of
boot, before any traffic. Track 04 has since bounded what the room actor itself keeps
(`crates/hs-room/src/actor/event_cache.rs`, decision 0044): the actor no longer holds every
event body of a room, nor an index of every event ID. What it cannot bound from its side is the
state store it feeds.

`hs_state::kv_store::KvStateStore` keeps, in a `RefCell<CommonInner>` that is **in memory
only** and starts empty on every `ProductionStateStore::open`:

| field | per event | what it is for |
|---|---|---|
| `state_at: BTreeMap<EventSn, Root>` | 40 B | `StateStore::state_at` -- the root of the state after each event |
| `event_id_of`, `sn_of_event_id` | ~2 x 110 B | translating between short ids and event ids during `resolve` |
| `events: EventStore` (a `ResolutionEvent` per event: id, room id, type, state key, sender, **content**, auth and prev event ids) | 0.5 to several KB | `state_res::v1`/`v2::resolve` |
| `chain_index: ChainCoverIndex` | position per event | `auth_chain_difference`, `chain_position` |
| `key_of`, `key_strings` | per `(type, state_key)` | interning `StateKeyId`s, which the durable frames reference |

Only the frames (`FrameRepr`, the roots and their contents) are in the key-value store. So
`RoomActor::load` must replay **every** event of the room into the store (`feed_store`) before
the room can answer anything: a load costs a read and a parse per event of history, and the
store then holds a record of every event for the actor's lifetime. For a room the size of
`#matrix:matrix.org` that is the whole of the memory left, and the whole of the boot time.

The actor now hands the store an empty `content` and no `auth_events` for events that are not
state events (`RoomActor::store_inputs`; state resolution reads neither of a message), which
shrinks a message's record to its ids and strings, but the record is still there, and still
O(history).

## What track 04 needs

A `KvStateStore` whose per-event records are durable and read on demand, so that opening a
room's store finds what the last process left and `RoomActor::load` can stop replaying:

1. **`state_at` durable.** `(EventSn) -> Root` in a keyspace, written by `add_event` /
   `add_event_with_state` (ideally inside the caller's transaction: see 5), read by
   `state_at`. `Root` is `RootB([u8; 16])`: 16 bytes a row.
2. **The `(type, state_key)` interning durable.** The frames reference `StateKeyId`s that are
   today assigned in order of first sight per process; a reload reproduces them only because
   the actor replays the room in the same order. An `hs-tables` interning table (as `room_sn`
   and `event_sn` have) makes them stable without a replay. Rooms whose frames were written by
   the per-process numbering need a one-time re-key or rebuild on the first load after the
   change: the actor can do that replay once if the store tells it to (`needs_rebuild()`).
3. **`ResolutionEvent`s by short id, durable, loaded on demand.** `resolve` needs the records
   of the conflicted state events and their auth chains, not of every event. Written by
   `add_event`; read by `resolve` as it walks (`EventStore` behind a trait that can fetch), or
   handed in by the caller for the events it names. The actor can supply bodies: it has
   `Tables::events`, and its `EventLookup` already answers `sender`/`content` by short id for
   authorisation (`hs_state::state_fetch::EventBody`).
4. **The chain-cover index durable**, or rebuilt lazily from the durable auth-event ids of the
   events a query names. `RoomActor::state_at_event` is the one caller of
   `auth_chain_difference` (for `/state_ids` and backfill's state), always on state events.
5. **A way to write inside the actor's transaction.** `RoomActor::persist` writes the event
   row, the timeline row and the extremities in one `transact`; the store's frames are written
   outside it today, which a crash can tear. A `StateStore` method that takes the caller's
   `KvWrite` (or hands back the puts to make) closes that at the same time.
6. **`has_event(EventSn) -> bool`** cheap and durable. The actor uses `state_at(sn).is_ok()` as
   "this room holds the event" (`RoomActor::known`), which with 1 becomes a point read.

With these, `RoomActor::load` reads the room's metadata, forward extremities, gaps,
redaction index and the newest `server.rooms.event_cache_size` events, and nothing else:
boot memory and boot time independent of history, which is the second half of the
2026-10-10 brief ("loading a room into its actor must not read every event; only state,
extremities, membership and the recent window").

## What stays with track 04 afterwards

- `RoomActor::load` stops feeding the timeline and outliers to the store (keeps the
  `rejected` set, built from the outliers index, and the gaps).
- `relations_by_target` and `redactions_by_target` (both small, both O(relations) and
  O(redactions), not O(history)) can move to prefix scans of `Tables::relations` and a new
  redactions keyspace if a room ever makes them matter.
- `timeline: BTreeMap<i64, EventSn>` (about 30 B per event; 30 MB for a million) is the one
  index the actor would still build on load; it is already a keyspace (`Tables::timeline`),
  and the reads that use it (`range` around a position, `next_back`) map onto range scans.

## Not proposed

Reimplementing the state store inside `hs-room` over `FrameRepr`, `state_res::v2::resolve`
and `ChainCoverIndex` (all public). It would reach the same place tonight, duplicate track 02's
store, and leave two numberings of `StateKeyId` on every existing deployment. The in-memory
records are track 02's design, and the durable version belongs there.
