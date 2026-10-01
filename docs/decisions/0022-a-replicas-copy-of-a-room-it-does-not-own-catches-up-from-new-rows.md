# 0022: A replica's copy of a room it does not own catches up from new rows (2026-10-01)

Status: accepted (tracks 05 and 04). Implements `docs/rfcs/0018-room-actor-catch-up.md` and
closes the `docs/next-steps.md` gap "A non-owner replica reloads a whole room per event to answer
`/sync`".

## Context

In a cluster every room has one owner, but any replica answers any user's `/sync`. A replica
reads a room it does not own through `hs_user::cluster::RoomMirror`, a read-only `RoomActor`
built from the shared store. Until now the mirror rebuilt that actor whole
(`RoomActor::load`) whenever the store's timeline head had moved past it: every event in a room
cost every other replica with a reader in it a parse of the whole room, about 25 ms for a small
room and unbounded for a large one. RFC 0018 asked `hs-room` for an incremental catch-up and
left open how a copy learns that something other than an append happened.

## Decision

- **`RoomActor::catch_up` reads the timeline rows past the actor's own head** and absorbs them
  in position order exactly as `load`'s timeline loop does (the event's body and flags, its
  state fed to the state store, the relations index), then re-reads the forward extremities.
  Membership and history-visibility changes therefore apply in order, each event seeing the
  state a fresh load would give it. All rows are checked before any is absorbed, so a verdict
  of "reload" leaves the copy untouched.
- **A per-room rewrite counter tells a copy that something other than an append happened.**
  New keyspace `room_rewrites`, `(RoomSn,) -> i64`, bumped with `atomic_add` inside the same
  transaction as every write that changes rows a copy may already have read: outliers added
  (a federation join), history placed below the head (backfill, a timeline gap filled), a gap
  closed, a purge, pruned forward extremities. A room deletion removes the row with the rest.
  `load` records the counter from its own snapshot; `catch_up` compares it first and answers
  `Reload(Rewritten)` when it moved. These writes are rare (administration and federation
  history), so a reload on each is cheap in aggregate, and nobody has to enumerate what changed.
- **The other reload triggers** are a position that is not the one after the copy's head
  (`PositionGap`), a new event with an explicit state or a gap row (a rejoin through another
  server; `ExplicitState`), a row whose event record is missing or already held
  (`UnexpectedRow`), a deleted room (`Gone`), and, in the mirror, the store's head being behind
  the copy (`regressed`) or a catch-up error (`error`). Each reload is an `info` (or `warn`)
  line with the reason and counts in `hs_user_mirror_full_reloads_total{reason}`.
- **Redactions are applied, not reloaded for.** The redaction is a timeline event; the owner
  rewrites its target's row with the `redacted` flag in a second transaction a moment later
  (`RoomActor::apply_redaction`). The copy re-reads the target's row when it absorbs the
  redaction and, if the flag is not there yet, keeps the target pending and re-checks it on the
  next catch-ups (the mirror catches up while anything is pending even if the head has not
  moved), giving up after 16 checks -- a redaction received over federation is never flagged on
  the owner either, and the copy then matches the owner.
- **The wake is the trigger.** `RoomWake::room_pos` already carries the owner's head after the
  update (a batch keeps the highest per room), so no field was added: the receiving hub calls
  `RoomMirror::prefetch(room, room_pos)` for each room in a batch, each as a task of its own,
  and waits for them at most 250 ms before it wakes the users the batch names; a copy already at
  or past that position reads nothing (`hs_user_mirror_wakes_covered_total`). The bound matters:
  the first version awaited the catch-ups inline, and a catch-up that turned into a whole
  reload of a big room held the peer's mesh request past its two-second deadline, which
  cancelled the request, the reload and the wakes with it. A room with no copy here is not
  loaded by a wake. The durable-head check on every read stays: the store is the truth, the
  wake a doorbell.
- **Bounds.** Copies are dropped after ten minutes unread (unchanged), and a mirror holds at most
  1,024 (`DEFAULT_MAX_MIRRORED_ROOMS`; the least recently read is dropped first). A copy holds
  the whole room in memory, as the owner's resident actor does.
- **A copy is advanced in place.** A reader holding the handle sees the room move on between
  two reads, as a reader of the owner's actor always has; before, a reload made a new actor
  and an old handle kept the old room.
- **An escape hatch.** `HS_SYNC_MIRROR_FULL_RELOAD=1` in a replica's environment turns catch-up
  off (every new event reloads the room whole, as before) with a `warn` at startup. It is also
  the baseline of the measurement in `crates/hs-cli/tests/cluster_mirror.rs`.

## Consequences

- A non-owner's work per event is the event's, not the room's: 3.4 ms against 989 ms per event
  in a room of 2,000 messages and 53 members, and 4.2 ms against 45.7 ms in a small one (release
  build, three real replicas; `docs/status/05-sync.md`, session 12).
- Every new code path that rewrites a room's existing rows, or places rows below its head, must
  call `actor::catch_up::bump_rewrites` in its transaction. A path that forgets leaves other
  replicas' copies stale until they are evicted or the next rewrite; the module docs of
  `hs_room::actor::catch_up` say so. The tests there exercise the purge; backfill, outliers, gap
  closes and extremity pruning bump the same way without a test of their own.
- `RoomRegistry::read_room` (search, on a non-owner) still loads a room whole per call; it is a
  per-request read, not per event, and could use the same mirror later.
