# 0018: Typing, receipts and presence ride the wake batch (2026-09-30)

Status: accepted (track 05). Extends the `user.wake` mesh message of `docs/status/05-sync.md`
session 7.

## Context

Since session 7 a room's owner tells every other live replica, after each update it has fed,
which users to wake (`hs_user::cluster::WakeBatch` over `POST /mesh/v1/peer`, route
`user.wake`). That covers everything on the registry stream. Typing, receipts and presence are
not on it: each replica's `TypingRegistry`, `ReceiptRegistry` and `PresenceRegistry` is its
own memory, and a two-pod cluster showed a user on one pod nothing ephemeral from a user on the
other (`docs/next-steps.md`, "Known gaps"). Receipts and presence are durable since
2026-09-27, but a replica loads a room's receipts, or a user's presence, from the store once
and then serves its copy.

Three ways were open: a new mesh message for ephemeral data; a per-room pull from the owner on
every `/sync`; or the existing wake batch. A pull is a mesh round trip per room per `/sync`
and the owner building part of every other replica's response, which is what RFC 0018's
"alternative considered" rejects for rooms. A new message is a second pump, a second route and
a second thing to keep ordered with the wakes for no gain: the batch already goes to every live
peer, is coalesced per peer, and arrives before the peer's long-polls are answered.

## Decision

- `WakeBatch` gains `ephemeral: Vec<EphemeralUpdate>` (`#[serde(default)]`, so a batch from a
  replica without the field still parses). Any replica that changes typing, receipt or presence
  state -- a client's `PUT .../typing`, a receipt, `PUT /presence`, a `/sync` that changes
  presence, an EDU from another server, the restamp on a join -- hands the change to
  `SessionCluster::publish_ephemeral`, and it rides the next batch to every other live replica.
  Not only the room's owner: typing and receipts are room requests and so always land on the
  owner, but presence and inbound EDUs land wherever they arrive.
- **Typing travels whole** (`room_id`, `user_id`, `typing`, `timeout_ms`). It is in no store
  and never will be; the receiver puts it in its own registry with the timeout, and expires it
  on its own clock. A receiver never re-publishes what it was sent, so there is no loop.
- **Receipts and presence travel as a hint to reread** (`room_id` + stamp; `user_id` + stamp).
  The data is already in the shared store, written by the replica that took it. The receiver
  forgets its cached copy (`ReceiptRegistry::forget`, `PresenceRegistry::forget`) so its next
  read is from the store, raises its stamp counter past the writer's stamp, and wakes the
  long-polls concerned. Rereading rather than re-sending keeps one source of truth and one
  code path for a restart and for a peer.
- Best effort, like the wake: a lost hint leaves a peer serving its cache until the next one;
  a lost typing update leaves a peer not knowing until the client's next `PUT` (real clients
  repeat it every few seconds while the user types). There is no read-your-writes wait for
  ephemeral data across replicas: `settle_before_read` waits on registry-stream numbers, and
  these are not on it. In practice the hint is on the peer within a millisecond, before any
  `/sync` there can be built.
- `hs_cluster_ephemeral_updates_total{kind, direction}` counts what each replica sent (once per
  peer reached) and received, by `typing`, `receipt`, `presence`.

## Consequences

- One message type between replicas, as before. The mesh (`hs-cluster`) is unchanged.
- Stamps (`hs_user::stamp`) are `max(previous + 1, unix micros)` and are now compared across
  replicas: a client's token holds the largest stamp it has seen from any replica. The hints
  carry the writer's stamp and the receiver observes it, so the counters converge as traffic
  flows; a replica whose clock is behind another's by more than the gap between two changes
  could still stamp something below a client's token. Replicas are expected to run NTP; this
  is the same assumption the restart-safety of stamps already made.
- Per-replica typing timeouts start when the update is applied there, a mesh hop after the
  owner's. A replica that starts after the typing began has no copy until the next `PUT`.
