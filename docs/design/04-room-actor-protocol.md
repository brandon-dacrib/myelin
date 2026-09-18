# 04. The room actor protocol

Owner: track 04 (room and events). This is the day-one design document the brief
(`docs/workstreams/04-room-and-events.md`) asks for; the authoritative, always-in-sync version of
this document is the rustdoc on `crates/hs-room/src/protocol.rs` (the command set and publish
stream) and `crates/hs-room/src/actor.rs` / `crates/hs-room/src/registry.rs` (the implementation).
This file is the prose summary for readers who want the shape without opening the source; where
the two disagree, the rustdoc is correct (it ships with the code and is checked by
`cargo test -p hs-room`'s doc-tests and the crate's own `#![warn(missing_docs)]`).

## 1. The unit of consistency

One `crate::actor::RoomActor` owns exactly one room (`PLAN.md` sections 5.3, 6.2). It is the only
thing that ever writes that room's state, timeline, extremities and membership. Every mutation —
create the room, send an event, change membership, redact, and (once track 06 lands it) persist an
inbound federation event — goes through it, one at a time.

**Implementation note.** This pass implements that serialization with a
`tokio::sync::Mutex<RoomActor<B>>` inside `crate::actor::RoomActorHandle`, not a spawned mailbox
task reading a channel. Both give the same guarantee (one writer at a time); the mutex needed no
supervisor, shutdown protocol or backpressure policy to be correct, none of which matter yet
because `hs-cluster`'s ownership routing (which would let more than one caller reach the same room
actor from different places) is not wired in. See `crate::protocol`'s module docs for the full
rationale. `RoomActorHandle`'s public API does not need to change if the internal mechanism does.

## 2. The command set

| Command | Method | What it does |
|---|---|---|
| Create a room | `RoomRegistry::create_room` → `RoomActor::create_room` | Builds `m.room.create`, the creator's own join, the preset's default state (power levels, join rules, history visibility, guest access), `initial_state`, `name`/`topic`, a local alias if requested, and invites — in spec order. |
| Send an event | `RoomActorHandle::send_event` → `RoomActor::send_event` | Message or state event: builds, hashes, signs, authorizes and persists (`crate::pipeline`). |
| Membership operation | `RoomActorHandle::membership` → `RoomActor::membership_action` | join/invite/leave/kick/ban/unban/knock: precheck (`crate::membership::precheck`) then `send_event` for the resulting `m.room.member`. |
| Redact | `RoomActorHandle::redact` → `RoomActor::send_event` + `RoomActor::apply_redaction` | Sends `m.room.redaction`, then marks the target event redacted. |
| Persist inbound federation event | `Command::PersistInbound` (`crate::protocol`) | **Seam, not implemented.** Track 06's entry point once inbound `/send` transactions exist; see `docs/rfcs/0010-room-actor-state-store-seam.md` section 2 for what it needs from `hs-state` that isn't wired in yet. |

Queries (state, timeline, members, relations, aliases) are **not** part of the serialized command
set — they are plain async methods on `RoomActorHandle` (`RoomActorHandle::query`, and the
convenience wrappers `crate::routes` calls) that read the actor's hot state under the same mutex
but do not need write-ordering guarantees relative to each other. This is a deliberate scope
simplification recorded in `crate::protocol`'s module docs.

## 3. The publish stream

`crate::protocol::RoomUpdate`, broadcast on every successful mutation via
`tokio::sync::broadcast` (`RoomActorHandle::subscribe`). This is the week-8 seam
(`docs/workstreams/README.md`) tracks 05, 06, 10 and 11 consume instead of polling the store.

Fields: `room_sn`, `room_id`, `room_pos`, `event_sn`, `event_id`, `event_type`, `state_key`,
`sender`, `changed_state_keys` (empty unless a state event), `membership_deltas` (empty unless
`m.room.member`), `push_evaluation_inputs` (reserved, not yet populated — see the rustdoc for why).
Deliberately excludes the event's full content: consumers that need it already hold, or can cheaply
query, the room's hot state; duplicating full event JSON onto every broadcast message would make
the channel's fixed-size backlog cost scale with message size.

## 4. The hot-state cache and its eviction policy

- **Within one `RoomActor`**: the whole room's event history stays resident in memory for the
  actor's lifetime (`RoomActor`'s `events: HashMap<EventSn, Event>` field) — a Phase 0 scope
  decision, not the final shape; a bounded recent-timeline window with KV fallback for older events
  is the documented next step (see that field's doc comment).
- **Across rooms**: `crate::registry::RoomRegistry` is the process-wide map from room ID to
  `RoomActorHandle`, loaded on first access (`RoomActor::load`, which replays a room's full
  persisted timeline from `hs-kv`/`hs-tables`) and dropped by `RoomRegistry::evict_idle` once idle
  longer than a caller-supplied threshold. A dropped room loses nothing durable — everything a
  `RoomActor` holds is derivable from the store (`PLAN.md` section 5.3) — the next access just pays
  the reconstruction cost again. `RoomRegistry::spawn_eviction_sweeper` is an optional periodic
  driver; `evict_idle` itself is exposed directly for deterministic tests or a caller with its own
  scheduler.

## 5. The membership state machine

`crate::membership`: `Action` (join, invite, leave, kick, ban, unban, knock) plus
`TRANSITIONS`, an explicit table of which prior membership states each action is valid from,
checked by `precheck` before the real pipeline runs (for a fast, clear client error). The
authoritative rule engine remains `hs_state::auth` (state-dependent checks: power levels, join
rules including restricted/knock-restricted, third-party invites) — `precheck` is a necessary, not
sufficient, precondition, cross-checked against the real thing by
`crate::actor::tests::private_room_membership_invariant_holds_across_room_versions` (a property
test parameterized over eight room versions) and `crate::membership::tests` (a property test
asserting `precheck` never drifts from a literal scan of its own table).

## 6. What this pass deliberately does not implement

See `docs/rfcs/0010-room-actor-state-store-seam.md` for the full write-up:

1. `hs_state::StateStore`/state resolution is not wired into `RoomActor` — the room actor uses its
   own directly-maintained flat current-state map instead, which is correct only because this actor
   is presently the room's sole writer and never creates more than one forward extremity. Multiple
   forward extremities (inbound federation, out of scope per this track's instructions) need the
   general case.
2. Room version 12 (hash-based room IDs, MSC4291) is rejected outright with a clear error rather
   than attempted half-correctly.
3. Retention/purge, spaces/room summaries, room upgrades, delayed/sticky events (MSC4140/MSC4354)
   and large-room fan-out-on-read are Phase 1/2 items per the brief and are not started; see
   `crate::retention`'s module doc and `docs/status/04-room-and-events.md`.
