# 0012. `RoomRegistry` needs a global "every room's updates" fan-in hook

Status: proposed, 2026-09-18. Author: track 05 (sync). Owner of the change: track 04 (room and
events), or the integration lead. Affects: `crates/hs-room/src/registry.rs`.

## The problem

`crate::hub::SessionHub` (track 05, `crates/hs-user/src/hub.rs`) turns `hs-room`'s
[`hs_room::protocol::RoomUpdate`] publish stream into every affected user's durable feed
(`PLAN.md` section 6.6). It does this by calling `RoomActorHandle::subscribe()` on a room it
already has a handle for (`SessionHub::watch_room`) and forwarding every update into
`SessionHub::process_room_update`, which reads the room's current member list and appends a feed
entry (or updates a membership record) for every affected user -- including a user who has never
synced, or even logged in, before: this is how an invite reaches its target.

That last case is the problem. `SessionHub::watch_room` needs to have already been called for a
room *before* the event that first makes it relevant to a given user (the room's creation, at the
latest, so the very first invite anyone ever receives into it is caught). But nothing in this
workspace, as of this session, tells `hs-user` "a room was just created" or "a room was just
loaded" -- `hs_room::registry::RoomRegistry::create_room` and `RoomRegistry::get_or_load` hand
back a `RoomActorHandle` to whichever caller asked for one (in practice, `hs-room`'s own route
handlers: `crates/hs-room/src/routes/create_room.rs`, `crates/hs-room/src/routes/membership.rs`,
and so on), and that caller has no reason to know `hs-user` exists, let alone to call
`SessionHub::watch_room` on its behalf. `hs-cli` (track 12), which does depend on both crates and
mounts both routers, never sees the handle either: the handle is constructed and consumed entirely
inside `hs-room`'s own handler functions.

Track 05's own operating instructions forbid editing `hs-room`'s crate to add this directly
(`.claude/agents/hs-05-sync.md`: "Never edit another track's crate"), so this RFC is the request
that rule points at, plus a working description of what track 05 built against in the meantime
(`crates/hs-user/src/room_source.rs`'s `RoomSource` trait) and how test coverage was obtained
without the real hook (`crates/hs-user/tests/sync_scenario.rs`'s "discovery-gap workaround," which
calls the same operation this RFC asks `RoomRegistry` to do automatically).

## The proposed change

Add to `hs_room::registry::RoomRegistry<B>`:

```rust
/// A stream of every RoomUpdate published by any room this registry has ever loaded or created,
/// starting from the moment this subscription was taken out.
pub fn subscribe_global(&self) -> tokio::sync::broadcast::Receiver<crate::protocol::RoomUpdate>
```

Implementation sketch: the registry already has exactly one place where a `RoomActorHandle`
starts existing under its management -- `RoomRegistry::insert` (called by both `create_room`'s
success path and `get_or_load`'s cold-load path). Add one `tokio::sync::broadcast::Sender<RoomUpdate>`
field to `RoomRegistry`, and inside `insert`, spawn one small forwarding task per newly-inserted
room (`handle.subscribe()` -> forward every item into the registry's own global sender), guarded
so it is only spawned once per room (not once per `get_or_load` call for an already-resident
room -- `insert`'s existing "entry already present" branch is already the right place: only spawn
when the entry did not exist before). `subscribe_global` just calls `.subscribe()` on that shared
sender.

This is purely additive: no existing method's signature or behavior changes, no existing caller is
affected, and it does not require `RoomRegistry` to know anything about `hs-user`.

## What track 05 does today, without this

- `crates/hs-user/src/room_source.rs`: `RoomSource<B>` is a one-method trait
  (`get_or_load`) that `Arc<RoomRegistry<B>>` already satisfies today, so `hs-user` does not need
  this RFC to depend on `hs-room`'s registry for the operations it *does* have.
- `crates/hs-user/src/hub.rs`: `SessionHub::watch_room(handle)` is the method this RFC's
  `subscribe_global` forwarder would call once per room, automatically, in production. Until then,
  whatever assembles the merged router (today: `crates/hs-user/tests/sync_scenario.rs`'s `setup`;
  in production: `hs-cli`, see `docs/status/05-sync.md`'s "For track 12") must call it explicitly,
  once, right after a room is created or first loaded.
- This is a real, documented functional gap in anything short of that manual wiring: a room this
  process has never been told to watch is invisible to every `hs-user` feature (feeds, the public
  room directory in `crate::store::UserStore::list_public_rooms`, everything). It is not a
  correctness bug in what *is* implemented (once a room is watched, `crate::hub`'s tests and
  `crate::sync`'s tests -- including the "a token from before a message still returns that
  message" case -- pass against the real `hs-room` crate), only a coverage gap pending this hook
  or the equivalent `hs-cli` wiring.

## Alternative considered: `hs-user` polls for new rooms

Rejected. Nothing in `hs-room`'s public API enumerates "every room that exists" (only aliases and
already-known room ids can be looked up), and adding such an enumeration would be a second,
independent ask of track 04's crate that duplicates the intent of this one, plus polling
contradicts this track's own instruction: "Use the room actor's existing publish stream rather
than polling rooms."
