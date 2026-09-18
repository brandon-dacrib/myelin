# 0010. The room actor to state store seam

Status: proposed, informational — records decisions already made and shipped behind documented
workarounds. Owner: track 04 (room and events). Consumers: track 02 (state and model, for the
`StateStore` ergonomics feedback its own status file invited), track 06 (federation, the next
consumer of the seams this crate deliberately left open).

Companion artifacts: `crates/hs-room/src/pipeline.rs`, `crates/hs-room/src/actor.rs`,
`crates/hs-state/src/api.rs`, `docs/status/02-state-and-model.md`'s "Interfaces needed" (which
explicitly asks track 04 for exactly this feedback), `docs/status/04-room-and-events.md`.

## 1. Motivation

Track 04's brief (`docs/workstreams/04-room-and-events.md`) budgets its first pass around the
common case a homeserver actually spends its time on: one server, one client population,
originating its own events into rooms it already knows the current state of. Two real gaps showed
up building that pass, both with a clear "not yet, but here is exactly what closes it" answer. This
RFC records both so the next session (this track's own, or track 06's, whichever needs the general
case first) does not have to re-derive them, and so track 02 has the concrete feedback its own
status file asked for.

## 2. Gap 1: `hs_state::StateStore` has no way to look up a `StateKeyId` for a known `(event_type, state_key)` pair

### The problem

`StateStore::get(root, key: StateKeyId)` (`crates/hs-state/src/api.rs`) answers "what event set
this key" -- but every `StateKeyId` in the trait's vocabulary is produced by
[`hs_state::store::InMemoryStateStore::add_event`]'s own internal interning
(`Inner::intern_key`, private to that module), assigned in first-seen order as events are ingested.
There is no method on `StateStore` (or exposed elsewhere in `hs-state`) that answers "give me the
`StateKeyId` for `("m.room.member", "@alice:example.org")`" independent of having already seen the
event that set it through that exact store instance. A caller that wants to look up a *specific*
key it already knows the string form of (which is every auth check: `m.room.create`,
`m.room.power_levels`, a specific user's `m.room.member`) has no way to get there except by already
holding an `EventSn` for an event that set it -- circular for the general case.

`hs-tables::interning::state_key_id_table` exists and would be the natural global answer (intern
`(event_type, state_key)` once, store-wide, the same way `room_sn`/`event_sn` are interned), but
`InMemoryStateStore`'s internal interning is a separate, per-store-instance, private table -- using
`hs-tables`' global table would not produce IDs `StateStore::get` recognizes, because
`InMemoryStateStore` never consults it.

### This track's workaround

`crate::pipeline::CurrentState` (`crates/hs-room/src/pipeline.rs`) does not use
`hs_state::StateStore` for the room actor's current-state queries at all. Instead,
`crate::actor::RoomActor` maintains its own flat `(event_type, state_key) -> EventSn` map
(`current_state`), updated directly at persist time (`RoomActor::persist`) from the event it just
built and knows every field of -- no interning, no reverse lookup, no `StateStore` involved.
`CurrentState` implements `hs_state::state_fetch::StateFetch` directly over that map plus the
actor's in-memory event cache, which is exactly the seam `hs-state`'s own docs anticipate
(`crates/hs-state/src/state_fetch.rs`: "the production room actor ... is expected to implement
`StateFetch` itself"). This sidesteps the `StateKeyId` gap entirely for the operations this pass
needs (build an event, decide who is allowed to send it).

### Why this workaround is only correct for the single-writer, no-fork case

The flat map is correct because, and only because, this room actor is presently the room's sole
writer and always sets a new local event's `prev_events` to exactly its own current forward
extremity, which stays a single event after every persist (`crate::actor::RoomActor`'s doc comment
on its `forward_extremity` field). A new state event simply overwrites its own key in the flat map;
no reconciliation between forks is ever needed, because no fork is ever created. `hs_state::StateStore`
(state resolution proper, via `resolve()`) is *not* wired into `RoomActor` in this pass at all --
not because it doesn't work, but because nothing in this pass's scope (no inbound federation
ingestion, no concurrent writers) ever produces more than one forward extremity to resolve between.

### What track 06 (or a future multi-writer pass) needs

Inbound federation events can arrive citing `prev_events` that fork the room's history, which is
exactly when `state_res` and a real `StateStore::resolve()` call are required, and exactly when the
flat-map shortcut above stops being correct (resolving two forks can change the winner for a key
neither side directly caused this actor to overwrite). Closing gap 1 for that case needs one of:

1. **Preferred**: add `StateStore::intern_state_key(&self, event_type: &str, state_key: &str) -> Result<StateKeyId, Self::Error>` to the frozen trait (`crates/hs-state/src/api.rs`), implemented by `InMemoryStateStore` as a thin wrapper around its existing private `intern_key`/`Inner::key_of` (already does exactly this internally; it would only need to stop being private). This keeps every `StateKeyId` a single store instance ever hands out consistent with each other, which the current private-method design already guarantees internally -- it only needs to be reachable from outside the module.
2. Alternatively, back `StateKeyId` allocation with `hs-tables::interning::state_key_id_table` globally and have `InMemoryStateStore` (and any KV-backed successor from the section 6.3 bake-off) consult it instead of a private local table. This is a bigger change (moves ID allocation out of `hs-state` into a shared, cross-crate-visible table) but would also make `StateKeyId`s comparable *across* rooms and stores, which nothing currently needs but which the chain-cover index's own design (`PLAN.md` section 6.4) may eventually want.

Either way, this is track 02's call (interface owner); this RFC only asks for it, with the concrete
call site (`crate::pipeline::CurrentState`) that would switch to it once available.

## 3. Gap 2: room version 12's hash-based room IDs (MSC4291) are not implemented

### The problem

Room version 12 (`RoomIdFormat::V2HashBased`, `crates/hs-model/src/room_version.rs`) makes a
room's ID the reference hash of its own `m.room.create` event -- the room ID is not known until
after that event is built, hashed and signed, but the room ID is itself one of the fields most of
the rest of the event-construction pipeline needs first (every subsequent event's `room_id` field,
every `hs-tables` key this crate scopes by `RoomSn`, `hs_state::auth::IncomingEvent::room_id` for
every auth check). `crate::pipeline::build_and_authorize`
(`crates/hs-room/src/pipeline.rs`) and `crate::actor::RoomActor` (`crates/hs-room/src/actor.rs`)
both take `room_id: &RoomId` as a plain, already-known parameter throughout; making it
"unknown until the create event exists" is a real two-phase construction change (build the
room-id-less create event first, derive the ID, *then* start the ordinary pipeline for every event
after it, including intern-ing the now-known room ID), not a parameter reshuffle.

### This track's workaround

`RoomActor::create` (`crates/hs-room/src/actor.rs`) checks
`rules.room_id_format != RoomIdFormat::V1Opaque` up front and returns
`RoomError::UnsupportedRoomVersion` with a message naming the gap, rather than attempting a
half-correct construction. `RoomActor::create_room`'s default room version is `"11"` (the newest
version this pipeline fully supports), so a caller that does not explicitly ask for room version 12
never hits this path.

### What closing it needs

A `RoomActor::create_v2` (or a branch inside the existing `create`) that: (1) builds the
`m.room.create` event's JSON without a `room_id` field and without going through
`pipeline::build_and_authorize`'s current room-id-requires-a-`RoomId`-up-front shape (a reduced
variant of that function, or a `room_id: Option<&RoomId>` parameter threaded through it and
`IncomingEvent`); (2) hashes and signs it; (3) derives `OwnedRoomId` via
`ruma::RoomId::new_v2(&reference_hash_string)`; (4) *then* interns that room ID and proceeds with
the ordinary pipeline for every event after it, including the `m.room.create` event's own
`event_id`, which for room version 12 uses `hash::derive_event_id` off the same reference hash
already computed. Not attempted in this pass; scoped out explicitly rather than shipped
half-correct, per this track's quality bar.

## 4. What is not a gap: the actor's mutex-serialized implementation

Recorded here only to head off the question, since sections 2 and 3 might otherwise read as "the
whole actor is provisional": `crate::actor::RoomActorHandle`'s `tokio::sync::Mutex`-based
serialization (instead of a spawned mailbox task) is a deliberate, documented implementation choice
(`crate::protocol`'s module docs, "Today's implementation"), not a gap waiting on another track's
interface. It has no dependency on anything outside this crate and can change internally without
moving `RoomActorHandle`'s public API.
