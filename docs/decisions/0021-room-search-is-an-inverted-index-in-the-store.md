# 0021: Room-event search is an inverted index in the server's own store (2026-10-01)

Status: accepted (track 04). Closes the `docs/next-steps.md` gap "`/search` unimplemented".

## Context

`POST /_matrix/client/v3/search` (category `room_events`) is Element's search box. It needs a
cross-room full-text index, which the room-actor model had no place for: an actor knows one
room, and a search spans every room the requester is in. `PLAN.md` and track 01's brief name
`tantivy` "per shard" for this, but nothing in the workspace depends on `tantivy` (`hs-search`
was never created; `hs-tables` has no search code). Synapse uses PostgreSQL's full-text search
or SQLite's FTS, with each query word matched as a prefix and every word required.

A `tantivy` index would be a second storage engine: its own directory on each replica (a pod
volume in the chart), its own commit point that does not move with the store's transactions, a
large dependency tree on a machine and CI the project keeps lean, and in a cluster one index per
replica covering only what that replica saw.

## Decision

- **An inverted index in one keyspace of the server's store, `room_search`.** Postings keyed
  `word, field, room, position` with the event's timestamp and word count as the value; a record
  of each event's postings (so a redaction can take them out); a cursor per room; a document
  count. Every backend has it, it is as durable as the events, and on PostgreSQL every replica
  reads one shared index. A query word matches every indexed word it is a prefix of (one range
  scan); every word must match. Words are runs of letters and digits, lower-cased.
- **Fields: Synapse's three.** `content.body` of `m.room.message`, `content.name` of
  `m.room.name`, `content.topic` of `m.room.topic`.
- **Fed from a cursor, with the room stream as a doorbell** (as appservice delivery,
  `hs_appservice::pump`). A page of events, its postings and the room's cursor are one
  transaction, so a restart neither replays nor misses anything. At start (the first start
  indexes everything held this way), after a lagged stream and every 30 s the indexer compares
  each room's head with its cursor. In a cluster each replica indexes the rooms whose shard it
  owns; the sweep picks a room up after a handoff, and an event two replicas both index is
  written once.
- **Read-your-writes.** A search first brings each room it will read, and this replica owns, up
  to its head, so that a message sent a moment ago is found (Complement searches right after
  sending).
- **The index proposes, the room decides.** Every hit is read back from its room: it must still
  be there, still say what was searched for (a redaction or purge the index has not seen yet
  never shows), pass the request's filter, and be visible to the requester under the room's
  history visibility at the event (`RoomActor::event_visible_to`, what `/messages` and `/context`
  use). Only rooms the requester is currently joined to are searched. On a replica that does not
  own a room, the room is read by a fresh load from the store, not left resident.
- **Rank** is `(1 + ln tf) * ln(1 + N / df)` summed over the query words; `recent` is
  `origin_server_ts`, newest first. `next_batch` names the last result's place in the order, so
  a page is stable while new events arrive. A full page always carries `next_batch` (Synapse
  does, and Complement expects it).

## Consequences

- Backfilled history (negative positions, a rejoin's gap) is not indexed: the cursor moves
  forward only.
- A query word matching more than 50,000 postings reads only the first 50,000 (by word, then
  room); `count` is then a lower bound, and it is logged.
- Scripts written without spaces are one word per run, found by its start only; there is no
  stemming. A tokenizer change needs a reindex (drop the keyspace's rows; the indexer rebuilds
  from the cursors' absence).
- Metrics: `hs_room_search_indexed_events_total`, `hs_room_search_index_documents`,
  `hs_room_search_rooms_behind`, `hs_room_search_index_delay_seconds`,
  `hs_room_search_duration_seconds`; `info` lines when a catch-up starts and ends.
