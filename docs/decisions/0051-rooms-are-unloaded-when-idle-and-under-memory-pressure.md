# 0051: Rooms are unloaded after half an hour unused, and least recently used first under memory pressure

Date: 2026-10-15. Track 04 (`hs-room`'s registry), with `hs-config` and `hs-cli` wiring.

## Context

On 0.3.22 with federation on, the demo's memory tracked its resident rooms: 126 rooms and
1.05 GB at 20:14 UTC, 133 and 1.17 GB at 20:20, 73% of its 1.5 GiB limit.
`server.rooms.idle_unload_after` was unset by default, so a room stayed loaded for the life of the
process, and a server joined to rooms over federation loads one for each that sends it anything.
The memory guard (`hs-cli`'s `memory_guard`, `federation.inbound_memory_shed_fraction` 0.85) sheds
inbound federation at its line, but shedding frees no room, so once there it would stay there.
Until the same branch made loads fast, an unload was also expensive to undo: a backfilled
federated room of thousands of members took seconds to minutes to load.

## Decision

- `server.rooms.idle_unload_after` unset means **30 minutes**; `0s` keeps rooms for ever
  (`RoomsConfig::idle_unload_after()`). Thirty minutes is what Synapse keeps an unused cache
  entry for (`caches.cache_entry_ttl`). A room in use is touched by every request and by the
  session hub on every update, so it is never unloaded between two of its users' syncs. A room
  nobody here reads, which the server took in only because another server sent it an event,
  goes after half an hour. An explicit `null` gets the default as well, so a stored section
  saved before the default existed is covered.
- New `server.rooms.memory_unload_fraction`, default **0.75**, hot: every 5 s
  (`RoomRegistry::spawn_memory_pressure_unloader`), with resident memory at or over that share
  of the limit, the registry unloads a quarter of the rooms it may, least recently used first,
  until memory is back under the line. After each pass that unloaded it waits 30 s, because
  resident memory falls only as the allocator returns pages, and it should see the step before
  more rooms go. The line is ten points under the federation
  shedding line, so rooms go first. The reading is the memory guard's (cgroup `memory.max`,
  else v1 `memory.limit_in_bytes`, else the new `HS_MEMORY_LIMIT_BYTES` for a process outside
  a container).
- A room is **never** unloaded, idle or under pressure, while:
  - a request or task holds a handle to it (`RoomActorHandle::holders() > 1`), because a later
    load would make a second actor for the room;
  - its load is under way;
  - it published an update after its last use, less than 10 minutes ago
    (`PENDING_UPDATE_GRACE`). The session hub asks for the room as it takes each update, so
    this means the update has not reached its members' syncs yet;
  - under pressure only, it was used in the last minute (`MEMORY_UNLOAD_MIN_IDLE`).
- `RoomRegistry::evict_idle`, the explicit eviction tests use, still evicts a room with an update
  waiting. It no longer evicts a room somebody holds.
- Observable: `hs_room_unloads_total{reason="idle"|"memory"}`. The idle sweep logs at `info`
  and each pressure pass logs at `warn`, with resident, eligible, unloaded, in-use and
  pending-update counts.
- New `storage.cache_size` (bootstrap, embedded backend), default **64 MiB**: Fjall's block cache,
  twice Fjall's 32 MiB default.

## Consequences

A room can be unloaded and loaded again during a day. That costs a load, now well under a second
even for a backfilled room of thousands of members (`crates/hs-room/tests/load_speed.rs`). The
first read of a room after it is unloaded pays that load. A server that wants the old behaviour
sets `idle_unload_after: 0s` and `memory_unload_fraction: 0`.

Measured on the real binary (status 04, 2026-10-15):
- In the deploy image (static musl), resident memory followed unloads within seconds. Under a
  280 MiB cgroup limit, three passes took the process from 0.90 to 0.71 of it and stopped.
- On macOS, the allocator gave the pages back about 20 s after the last of the rooms went. A
  loop reading every 5 s had already unloaded everything it could.

So after a pass that unloaded, the loop waits 30 s (`MEMORY_UNLOAD_COOLDOWN`) before it reads
memory again.
