# 0035: Another server's requests for a room go to the room's owner (2026-10-08)

Status: accepted (tracks 03 and 06). Extends the forwarding of client requests (RFC 0001
section 8, `hs-cli`'s shard gate) and decision 0017's mid-handoff behaviour to federation.

## Context

In a cluster every room has one owner, and the shard gate (`hs-cli`'s `RoomShardGate`) forwards
a client's request for a room to it over the mesh. Federation requests were not gated: they ran
on whichever replica the remote server's connection reached (a Kubernetes Service spreads them
over every replica, three by default in the chart's `cluster` mode). The room's fence then
refused the write on every replica but the owner, so a remote server's `send_join` failed with
`501 M_HS_INBOUND_INGESTION_UNSUPPORTED` ("fenced: ...") two times in three, and `/send`'s
PDUs were refused one by one with the same error. Found by `fed-cluster` (2026-10-08), whose
two-replica device-list test had to send its join to the owner by hand.

## Decision

- **A federation request for one room is forwarded whole.** The gate matches
  `/_matrix/federation/{version}/{endpoint}/{roomId}/...` for the membership handshakes
  (`make_join`, `send_join` v1/v2, `make_leave`, `send_leave` v1/v2, `make_knock`,
  `send_knock`), `invite` v1/v2 and `exchange_third_party_invite`, and for the room reads a
  remote server walks history with (`state`, `state_ids`, `backfill`, `get_missing_events`,
  `event_auth`, `timestamp_to_event`, `hierarchy`, `extremities`), since a non-owner's copy of a
  room can answer them stale. It forwards them exactly as client requests, before the `X-Matrix`
  layer: method, URI, body and `Authorization` arrive unchanged, so the owner verifies the
  signature again. A non-owner never runs these handlers.
- **`/send` is handled where it lands, and each write goes to its room's owner.** A transaction
  carries PDUs of any number of rooms under one signature, so it cannot be split and forwarded.
  The receiving replica verifies it and each PDU as before; its `RoomWriteSink` is
  `hs-cli`'s `ClusterWriteSink`, which hands each write (`accept_verified_event`,
  `unknown_events`, `accept_auth_outliers`, `accept_prev_event_with_state`) for a room another
  replica owns to that replica over the mesh, route `federation.sink`, where it is applied
  through the owner's own sink. The mesh is authenticated, so the owner trusts the receiving
  replica's verification, as it trusts a forwarded request's routing.
- **Mid-handoff, as decision 0017.** The forwarder waits out an ownerless shard, `421` and
  `503`. A write fenced on a replica because its shard moved while the write ran stored nothing
  and is made again at the new owner, by the gate (a `503` from a handler, now what a fenced
  federation write answers: `M_HS_NOT_SHARD_OWNER`, from `JoinError::NotOwner` and
  `InviteError::NotOwner`) or by `ClusterWriteSink`. An owner whose own write is fenced answers
  `503`, which the forwarder retries against the shard's next owner. Only when no owner took a
  `/send` write within the forward deadline is the transaction answered `503` and not
  remembered (`TransactionError::NotOwner`), so the sending server sends it again.
- **Counted with the client's forwards.** `hs_cluster_forward_latency_seconds` gains a `kind`
  label: `client`, `federation` (a forwarded federation request), `federation_pdu` (one `/send`
  write) and `peer`. Each forward is logged at debug level.

## Consequences

A remote server's join, knock, leave, invite or backfill against a clustered server works
whichever replica it reaches, at the cost of one mesh hop on the replicas that do not own the
room. The `kind` label is new on an existing series: a query that sums by `route` or `outcome`
is unchanged. `/send`'s room reads (room version, server ACL, forward extremities for a gap) are
still answered from the receiving replica's copy of the room; they decide nothing the owner does
not decide again.
