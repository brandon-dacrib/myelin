# 0045: 2026-10-12: the state store keeps a state root per event over its delta-chained frames, durably, and a room loads without replaying its history

Status: accepted (tracks 02 and 04; RFC 0025, branch `agent/state-load`).

## The problem

`hs_state::kv_store::KvStateStore` kept, in memory only, the root of the state after every
event, two event-id maps, a record of every event for state resolution and the chain-cover
index; only the state frames were durable. So `RoomActor::load` replayed every event of a room
into the store before the room could answer anything, and the store held a record of every
event for the actor's lifetime: boot time and memory both followed a room's history (the
1.2 GiB the demo held after joining `#matrix:matrix.org` on 2026-10-10; decision 0044 bounded
the actor's own share, RFC 0025 named the store's).

Two ways to make the state at each event durable were on the table:

- **Synapse's state groups**: a group per distinct state, events mapped to groups, each group a
  delta against a previous group, with a background compressor to keep the delta chains short.
- **A root per event over the frames we already have**: the state representation the bake-off
  chose (`crate::frames::FrameRepr`, status 02) is already a content-addressed chain of
  deltas -- a root is a frame, a frame is a delta against its parent with a periodic full base
  -- so what was missing was only the `EventSn -> root` row, not a new representation.

## What was chosen

1. **A root per event, over the frames.** The `state_at` keyspace maps each event's short id to
   the 16-byte root of the state after it. Two events with the same state share a root (the
   frames are content-addressed), which is what a state group buys, without a second structure
   or a compressor: the frames' own base interval bounds the chain a read walks. State groups
   would have meant a second representation beside the frames, a migration of every room's
   frames into it, and the compressor Synapse operators run by hand.
2. **Everything else per event is durable too and read on demand** (`crates/hs-state/src/durable.rs`):
   a compact record per event (`state_event`), its id (`state_event_id`), the
   `(type, state_key)` interning in `hs-tables`' layout (`state_key_id_*`), the chain-cover
   index (`state_chain_*`), and a per-room marker (`state_layout`). In memory only bounded
   least-recently-used caches. State resolution fetches the records it touches.
3. **The room actor feeds the store inside its own transaction** (`RoomActor::persist_with`,
   `KvStateStore::ingest_in`): an event's row, timeline position, extremities and state records
   commit together or not at all. A room's first timeline event marks its records complete
   (`mark_migrated_in`) in the same transaction.
4. **A load reads; it replays once.** `RoomActor::load` asks `needs_migration`: a room created
   or migrated since this change is loaded from its rows and the store's records, feeding the
   store only an event it has no record of (expected never; counted in
   `hs_room_state_records_repaired_total` and logged at `warn`); a room from before is replayed
   once, as every load did until now, then marked (`mark_migrated`, `hs_state_migrations_total`,
   an `info` line). Ordinary timeline rows are not even decoded into events on a load: the
   actor's bookkeeping (position, soft-failed and purged flags, relations, the create) comes
   from the row's fields.
5. **"This actor holds it" is the room's own rows, not the store.** The store is now shared by
   every actor and replica on the backend, so it knows every room's events and a read copy's
   owner's newer ones. `RoomActor::known` is: rejected or purged here, resident, or a stored row
   of this room that this actor has taken in (an outlier, or a timeline event at the position
   its row names), with a bounded cache of yes verdicts. No per-room set of every event held
   came back.
6. **Re-ingestion is idempotent.** Ingesting an event already recorded writes the same rows
   (chain positions are never reassigned; frames are content-addressed). A frame two writers
   create at once is written once: the frame write is a retried transaction that skips a frame
   already there (PostgreSQL's serializable isolation can refuse the second writer; until this
   change that was an error). Tested with two writers migrating one room at once, on the
   in-memory backend and on PostgreSQL.

## Consequences

- Measured (`cargo test -p hs-room --release --test durable_state measure -- --ignored
  --nocapture`, 50,000 messages on Fjall, this desktop): a load of a room whose records are
  complete 0.16-0.20 s, feeding the store nothing; replaying over the records (what every load
  did until this change) 3.0-3.5 s; the one-time migration into an empty store 2.8-3.2 s. Track
  02's own figure for the store alone: 0.00 s and +0 MiB to open, against 0.60 s and +55 MiB.
- An existing deployment pays one replay per room, on that room's first load after the upgrade,
  then never again. Old frames stay in the content-addressed keyspace, unreferenced.
- A read copy catching up with its owner's rows (decision 0022) feeds the store nothing: the
  owner already did, in the same transaction as the row.
- Still O(history) on a load: one read of each timeline row (its fields, not the event) and the
  actor's in-memory `timeline` map; one read of each outlier row's fields. Still fed to the
  store just after their own commit, and repaired by the next load if a crash falls between:
  outliers (`persist_outliers`) and placed history (`place_history`).
- `LAYOUT_VERSION` lets a future change to the records ask every room for one more replay.
