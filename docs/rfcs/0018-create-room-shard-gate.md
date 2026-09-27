# RFC 0018: `/createRoom` is shard-gated by a pre-assigned room id

Status: implemented on the `hs-cli`/`hs-cluster` side (2026-09-27, track 03); needs one line in
`hs-room` (track 04) to be complete. Author: track 03 (cluster).

## Problem

Every `/rooms/{roomId}/...` request carries its room id in the path, so `hs-cli`'s
`RoomShardGate` (`crates/hs-cli/src/cluster.rs`) hashes it to a shard and forwards the request
to the shard's owner before any local `RoomActor` is built. `POST /createRoom` cannot be gated
that way: the id is minted inside `hs-room`'s handler (`RoomActor::create_room`,
`RoomId::new_v1`). So whichever replica receives `/createRoom` builds the new room's first actor
locally, whether or not it owns the shard the id hashes to. Every later request for the room is
routed to the true owner, and the write fence installed in `RoomActor::persist` stops the
non-owner's leftover actor from committing anything -- but the first actor was still built in
the wrong place, and its bootstrap events were written without the owner's serialisation.
`docs/next-steps.md` lists this under known gaps ("`/createRoom` not shard-gated").

## Design

Move the choice of id in front of the gate.

1. The gate intercepts `POST .../createRoom`. In clustered mode it mints an id itself,
   `!random:server_name` (`ruma::RoomId::new_v1`, the same shape the handler would produce),
   and hashes it with `ShardLayout::room_shard`.
2. If this replica owns that shard, the gate inserts the id as a request extension,
   `hs_cluster::PreassignedRoomId`, and lets the request through to the handler.
3. Otherwise it forwards the request to the owner over the mesh, exactly as it forwards any
   room request, with the id in the mesh header `x-hs-preassigned-room-id`
   (`hs_cluster::PREASSIGNED_ROOM_ID_HEADER`). On the owner, the mesh handler
   (`ProxyShardHandler`) marks the replayed request with the `hs_cluster::ViaMesh` extension
   before it re-enters the router; the owner's gate sees `ViaMesh`, re-checks that it owns the
   id's shard *now* (refusing with `503 M_HS_NOT_SHARD_OWNER` if ownership has moved, which the
   sender's forwarder retries), and turns the header into the same `PreassignedRoomId`
   extension.
4. The handler creates the room under the pre-assigned id.

Two invariants make the seam safe:

- **A client can never choose a room id.** The gate strips `x-hs-preassigned-room-id` from
  every request that did not arrive over the mesh, before anything reads it. Request extensions
  cannot be set from a client socket, so a handler that finds `PreassignedRoomId` can trust it
  unconditionally. This is why the handler must read the *extension*, never the header.
- **The owner re-checks ownership** when it receives a forwarded id, so a stale ownership view
  on the sender can never make a non-owner build the actor.

In single-node mode nothing is pre-minted: the gate strips the header and passes the request
through; the handler mints its own id as today.

`hs-cluster` owns the three items (`create_room.rs`) so that `hs-room` can read the extension
without depending on the binary crate; `hs-room` already depends on `hs-cluster` for fencing.

## What `hs-room` needs to do

In `crates/hs-room/src/routes/create_room.rs`, `post_create_room` takes one more extractor and
passes it into the request it already builds:

```rust
pub async fn post_create_room<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    RoomRequester(requester): RoomRequester,
    preassigned: Option<axum::Extension<hs_cluster::PreassignedRoomId>>,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    // ...
    let request = CreateRoomRequest {
        // ...
        room_id: preassigned
            .map(|axum::Extension(p)| ruma::RoomId::parse(p.as_str()).map(|r| r.to_owned()))
            .transpose()
            .map_err(|e| RoomError::Internal(format!("pre-assigned room id: {e}")))?,
        ..Default::default()
    };
```

`CreateRoomRequest::room_id` already exists (it is how `/rooms/{roomId}/upgrade` names the
replacement room ahead of creating it), so nothing below the route changes.

Until this lands, the gate still mints, forwards and re-checks, and the handler ignores the
extension: the room is created on the replica the gate chose, under an id the handler chose,
which hashes to that replica's shard only by chance. The gate detects this from the response
(the returned `room_id` differs from the pre-assigned one) and logs a warning naming this RFC;
the room is usable either way. The two-process run in `docs/status/03-cluster.md` (2026-09-27)
shows exactly that warning.

## Room versions with hash-derived ids

Room version 12 derives the room id from the create event's reference hash, so no id can be
chosen ahead of the handler. For those rooms the handler's side of the fix is to *retry*: build
the create event, derive the id, and if `ownership.is_mine(layout.room_shard(&id))` is false,
rebuild with a fresh `origin_server_ts` until it is (expected `N` attempts for `N` replicas,
each a hash, no I/O). `hs-room` has the `RoomFencing` hook with `ownership` and `layout` on the
registry already. This RFC does not implement that; today a v12 `/createRoom` behaves as the
paragraph above describes.

## Tests

`crates/hs-cli/src/cluster.rs`, `mod tests`, with scripted ownership and a stand-in handler that
answers the way `hs-room` will once it honours the extension:

- `on_the_owner_create_room_runs_locally_under_a_preassigned_id`: the owner creates under the
  gate's id; a client-supplied header is stripped.
- `on_a_non_owner_create_room_is_forwarded_to_the_owner_which_creates_it`: replica A owns
  nothing and forwards to B's real mesh listener; B's handler sees `ViaMesh` and the extension;
  A's handler never runs; the client gets the id B created under.
- `a_forwarded_create_room_for_a_shard_not_owned_here_is_refused`: a forwarded id for a shard
  this replica does not own is answered `503 M_HS_NOT_SHARD_OWNER`.
- `in_single_node_mode_create_room_passes_through_untouched`.
