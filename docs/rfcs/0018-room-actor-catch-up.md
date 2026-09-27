# 0018. A non-owner's room snapshot needs an incremental catch-up, not a reload

Status: proposed, 2026-09-27. Author: track 05 (sync). Owner of the change: track 04 (room and
events). Affects: `crates/hs-room/src/actor.rs`, `crates/hs-room/src/registry.rs`.

## The problem

Since 2026-09-27 `/sync` is cluster-aware (`docs/status/05-sync.md`, session 7). A replica
answers `/sync` for any user, and reads a room it does not own through
`hs_user::cluster::RoomMirror`: a read-only `RoomActor::load` snapshot per room, kept as long as
the store's durable timeline head (the newest `room_timeline` key under the room) is not past
the snapshot's, and **reloaded whole** when it is. That is correct -- `RoomActor::load` is the
one function that turns the store into an actor, and it is what the owner itself would do after
a restart -- and it is the right thing to trust, because the store is the source of truth and
the mesh wake is only a doorbell. It is also O(room size) per new event in every room that a
replica has sessions reading but does not own: a room with ten thousand events costs ten
thousand event parses on replica B every time somebody says something in it on replica A. In a
two-replica cluster that is half of every user's rooms.

`hs-user` cannot fix this on its own. Everything an actor knows lives in private fields of
`RoomActor` (`events`, `timeline`, the state store's roots, the extremities, the relations
index), and the only public constructors are `create_room`, `load`, `create_from_remote_join`
and the federation paths. There is no "here are rows past your head, take them" entry point.

## The proposed change

Add to `hs_room::actor::RoomActor<B>`:

```rust
/// Advances this actor to the store's current head by reading only the `room_timeline` rows
/// past its own: the events they name, their `state_snapshots` rows (fed to the state store
/// the way `load` feeds them), the extremities, the relations index. A no-op when the store
/// is not ahead. Returns how many events were applied.
///
/// For an actor that is *not* the room's owner: it never persists, it only reads. The owner
/// never needs this (it wrote the rows), and calling it on the owner is harmless.
pub fn catch_up(&mut self) -> Result<usize, RoomError>;
```

and to `RoomActorHandle<B>` the `async` wrapper the other mutating methods have. The timeline
key is `(RoomSn, room_pos)` and every row past the actor's head is exactly the set of events it
lacks; `load` already contains the per-event decode and state-feed steps, so `catch_up` is that
loop with a start position, factored out of `load`. Redactions and the `flags` byte come with
the `PersistedEvent`, as they do today.

`hs_user::cluster::RoomMirror::get_or_load` would then call `catch_up` under its per-room
entry instead of dropping the snapshot and loading again. The durable-head check it makes
first (two point reads) stays: it is what tells it whether to bother.

## What track 05 does today, without this

- `crates/hs-user/src/cluster.rs`, `RoomMirror`: reload whole, as described. The cost is
  documented on the type and in `docs/scaling.md`'s "what a replica does not add" table.
- The mirror is evicted after ten minutes idle, so a room nobody on this replica reads costs
  nothing.

## Alternative considered: forward `/sync`'s room reads to the owner over the mesh

Rejected for now. A `/sync` touches every room with news, so it would turn one client request
into a fan-out of mesh round trips, the owner would build every other replica's responses as
well as its own, and the whole point of a replica -- spreading the response-building work --
would be lost. `PLAN.md` 5.4 says the user session serves from "memory plus targeted store
reads"; the mirror is that, and `catch_up` makes the reads targeted.
