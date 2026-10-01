# 0020: A new room's id is placed on a shard the replica building it owns (2026-09-30)

Status: accepted (track 04, with a one-line diagnostic change in `hs-cli`'s shard gate). Closes
the `docs/next-steps.md` gap "A v12 room's id cannot be pre-assigned"; completes RFC 0019.

## Context

RFC 0019 shard-gates `POST /createRoom` by minting the room's id in the gate: the replica that
owns the id's shard builds the room. That works for room versions 1-11, whose ids are opaque.
From room version 12 (MSC4291) the id is the create event's reference hash, so the gate's id is
ignored and the room is built on whichever replica the gate chose, under an id that hashes to
that replica's shards only by chance. Every later request for the room went to the true owner
(which loaded it from the shared store), so the room worked, but its first actor and its
creation burst lived on a non-owner, unfenced.

The RFC named the remedy for hash-derived ids (rebuild the create event until its id lands on a
local shard). The alternative was to forward the built create event to the owner of its hash's
shard: deterministic, but the owner would need a new mesh call that accepts a pre-built,
pre-signed create event and the rest of the request, and the forward can itself land on a
handoff.

## Decision

- **Retry, as RFC 0019 says.** `RoomActor::create_placed` builds the create event, derives the
  id, and while `ownership.is_mine(layout.room_shard(id))` is false rebuilds it with
  `origin_server_ts` one millisecond earlier (earlier, so the create never looks newer than the
  creator's join stamped `now`). Each attempt is a hash and a signature, no I/O; the expected
  count is about the number of replicas.
- **Bounded, and refused when the bound runs out.** At most `MAX_ID_ATTEMPTS_PER_SHARD` (16)
  times the room shard count (4,096 at the default 256 shards, under a second). A replica that
  owns even one shard fails to place an id within the bound with probability below `e^-16`; one
  that owns no room shard always fails. Running out is `RoomError::Fenced`, `503`, which the
  client retries, rather than building the room somewhere it does not belong.
- **Opaque ids the handler mints itself are placed the same way.** A room created with no
  pre-assigned id (an admin-created room, a server-notices room, an appservice's room through a
  path the gate does not see) mints `!random:server` until one hashes to an owned shard. An id
  the caller chose (the gate's pre-assigned id, an upgrade's replacement room) is used as given.
- **The creation burst is fenced.** The registry's fence is installed on the new actor before
  its create event is persisted; before this, a created room's fence was installed only after
  `RoomActor::create_room` had written the create event, the creator's join and the preset's
  state.
- **Observable.** `hs_room_create_room_id_attempts` (histogram) on `/metrics`; a `debug` line
  when more than one attempt was needed and a `warn` when the bound ran out. `hs-cli`'s gate no
  longer warns about a room whose id differs from the pre-assigned one when the id hashes to a
  shard the replica owns (it logs that at `debug`): that is the version-12 case working.

## Consequences

- In single-node mode nothing changes: every shard is the replica's, the first id is taken.
- A version-12 room's create event is up to a few milliseconds older than its creation; nothing
  reads that difference.
- Upgrading a room *to* version 12 is not covered: `/rooms/{roomId}/upgrade` names the
  replacement room's id in the tombstone before the room exists, which a hash-derived id cannot
  satisfy. That is a separate gap.

## Amendment (2026-10-01): the id must also be new

Sytest found two version-12 `createRoom` calls by one user with the same body in the same
millisecond answered with one room: the two create events were identical, so their hash was,
and nothing asked whether a room already had the id (status 04 session 17).

- **The create event's write claims the id.** It reads `Tables::room_meta` for the room inside
  its own serializable transaction and refuses an id a room already has
  (`RoomError::RoomAlreadyExists`, nothing written). Two concurrent creates of one id cannot
  both pass: one conflicts, re-runs, and finds the row. The check is against the shared store,
  so a room another replica built counts.
- **A taken id is one more attempt** of the loop above, under the same bound and in the same
  histogram. With no fencing installed (this crate's tests; `hs serve` always installs it) the
  bound is `MAX_ID_ATTEMPTS_PER_SHARD` itself.
- **After a taken id the create event goes back a random 1-1,024 ms**, not one: every identical
  create walks the same path from the same `now_ms`, so a fixed step makes each create of a
  burst try every earlier one's id in turn. A placement miss still steps one millisecond. A
  minted opaque id is minted again; a chosen one is refused (`M_ROOM_IN_USE`), never swapped.
- **Observable.** `hs_room_create_room_id_taken_total` and an `info` line per taken id.

The "few milliseconds older" consequence above becomes "up to about a second per collision".
