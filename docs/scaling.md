# What adding a replica gets you, and what it does not

Written 2026-09-26, revised 2026-09-27 for cluster-aware `/sync`. This is the plain answer to
"if I set `replicas: 3`, what do I get?", and
it is deliberately split into what the design gives, what is built today, and what has been
measured (nothing, yet). `PLAN.md` sections 5 and 7 are the design; `docs/status/03-cluster.md`
is the record of what has actually run.

## The one-paragraph answer

A replica adds capacity for **more rooms being written to at once** and **more clients
connected at once**, roughly in proportion to the number of replicas, and it adds
**availability**: a replica that dies loses only cache, and what it owned moves to a survivor.
A replica does **not** make any one room faster, does **not** add database capacity, and does
**not** make a message to ten thousand people cheaper. The database is the ceiling; replicas
spread the CPU and memory work above it.

## How the work is divided

Every room and every user session is assigned to exactly one replica by hashing its ID over
the live replicas (rendezvous hashing over virtual shards, RFC 0001). The replica that owns a
room holds its hot state in memory (current state, extremities, recent timeline, member index,
auth-chain cache) and serialises every write to it: authorisation, state resolution, persist,
push evaluation, publication to sync, federation and bridges. Any replica may receive any
request; one that concerns a room it does not own is forwarded over the internal mesh to the
owner and answered from there. There are no worker types and no path-routing table: the
partition is the hash, and the routing is the mesh.

## What a replica adds

| More of this | Why it scales with N |
|---|---|
| Rooms active at the same time | each room has one owner; N replicas hold N times the hot rooms in memory and run N times the event pipelines in parallel |
| Clients connected at the same time | a client's `/sync` is answered by whichever replica it reaches, from the shared store; the room's owner wakes that replica over the mesh when something happens, and a write made through one replica is in the client's next `/sync` on another (built and run as two processes on 2026-09-27, `docs/status/05-sync.md`). Long-polls, response building and the per-user feed work are spread across replicas |
| Inbound federation | a `/send` transaction is split by room and dispatched to each room's owner |
| Outbound federation and bridge delivery | per-destination and per-appservice queues are sharded across replicas (designed; see "today") |
| Availability | a lost replica's rooms and users are re-owned by survivors within the lease timeout; the target is under five seconds to the first successful write on the new owner |
| Rolling updates | a replica hands its shards off before it stops, so an upgrade is not an outage (readiness is withdrawn first; the handoff exists; neither has been measured under load) |

The per-event work that dominates a busy homeserver, signature verification, state resolution,
push-rule evaluation and sync fan-out, runs on the owner. Spreading owners across machines is
what spreads that work.

## What a replica does not add

| Not more of this | Why not |
|---|---|
| Throughput inside one room | one room has one owner, so its write rate is bounded by one core's pipeline. The answer to a very busy room is that the pipeline is much faster than Synapse's (the section 13 target is ten times the events per second per core), not that it is parallel |
| Database capacity | every replica writes to the same PostgreSQL primary. Read replicas help reads only. matrix.org runs Synapse on one primary, so the primary is not the first bottleneck, but it is the ceiling, and it is the only stateful thing in the cluster |
| Cheaper fan-out to a huge room | an event in a room of ten thousand members costs ten thousand recipients' worth of push and sync work, on that room's owner, however many replicas there are |
| Faster single requests | a request for a room another replica owns pays one mesh hop. Latency does not go down with N; it goes up by that hop when the client happens to hit a non-owner |
| Federation bandwidth to one peer | one destination's queue lives on one shard |
| Free reads of a room from a replica that does not own it | such a replica reads the room through a copy (`hs_user::cluster::RoomMirror`) that it loads once and then advances by reading only the store's rows past it (RFC 0018, decision 0022): a few point reads per new event, whatever the room's size (3-4 ms measured, against nearly a second to reload a 2,000-message room; status 05 session 12), in every room a replica has readers in but does not own, plus a whole reload when the owner rewrites the room (backfill, a purge). The copy also costs the room's size in memory on each such replica (at most 1,024 copies) |
| Cheaper typing, receipts and presence with more replicas | every change is sent to every live replica in the wake batch (decision 0018): typing whole, receipts and presence as a hint to reread the store. O(replicas) small messages per change, coalesced per peer, and each replica keeps a copy of every typing entry |

## The same thing in Synapse's terms

Synapse gets the first table too, but the operator does the partitioning: choosing worker types
(`synchrotron`, event persister, federation sender, ...), how many of each, and a routing table
at the reverse proxy that sends each path to the right worker, plus Redis for replication. Here
the operator's whole input is the number. The second table applies to Synapse in exactly the
same way; a single PostgreSQL primary and a single event persister per room are its ceilings
as well.

## What is built today, honestly

Cluster mode is a correctness proof for writes, not yet a working multi-replica server for
clients. As of 2026-09-26:

- **Rooms are partitioned for every `/rooms/{roomId}/...` request**, reads and writes,
  forwarded to the owner or refused with `503 M_HS_NOT_SHARD_OWNER`. Two `hs serve` processes
  on one PostgreSQL took forty concurrent sends to one room and did not fork its history
  (`docs/status/03-cluster.md`). Fencing inside the write transaction is installed too.
- **`/sync` is cluster-aware, as two processes on one PostgreSQL** (2026-09-27,
  `docs/status/05-sync.md`). A client's `/sync` is answered by whichever replica it reaches;
  only a room's owner writes feeds, and after each update it sends every other live replica a
  wake over the mesh (`POST /mesh/v1/peer`, `hs_cli::sync_cluster`), so a long-poll on
  replica B for a room replica A owns returns as soon as A has fed the update (the mesh hop
  measured under a millisecond; what remains is the owner's feed writes and the reader's
  response build). Before reading, a `/sync` asks every peer what it has published and waits,
  within the same 500 ms budget the single-replica read-your-writes wait has, to have received
  that peer's wakes up to it: 160 of 160 sends through one replica were in the very next
  `timeout=0` sync on the other. A replica reads a room it does not own through a snapshot
  checked against the store's head on every access and caught up from the rows past it
  (RFC 0018, since 2026-10-01). What is *not* built: the user-session *owner* of `PLAN.md` 5.4
  (no `/sync` is forwarded; there is no per-user shard in use). Typing, receipts
  and presence cross replicas since 2026-09-30 (decision 0018; two real processes on
  PostgreSQL, `crates/hs-cli/tests/cluster_ephemeral.rs`). Two pods have still not done this: the
  run was two processes on one host over a plain (non-TLS) mesh. Release build, same host: a
  cross-replica long-poll returns about 150 ms after the write is acknowledged, of which the
  mesh is under a millisecond.
- **Per-replica settings are seeded into the shared configuration store.** The bootstrap file
  seeds the database once, and the database outranks the file afterwards, so two replicas
  seeding one database leave the loser's `listeners` and `cluster.mesh.port` in force for both
  on the next restart (seen: replica A restarted as B and failed to bind). Until the config
  store excludes per-replica sections in cluster mode, `hs config unset /listeners/listeners`
  and `unset /cluster/mesh/port` once after the first start make each replica's own file win.
- **`/createRoom` is shard-gated in `hs-cli`, and `hs-room` does not yet honour it** (RFC 0019,
  2026-09-27): the gate mints the new room's id, hashes it and forwards the request to the
  shard's owner over the mesh, but `hs-room`'s handler still mints its own id, so the room's
  first actor is built on the replica the gate chose under an id that hashes to that replica's
  shard only by chance. One line in `hs-room` closes that; every later request is routed to
  the true owner either way.
- **Outbound federation and bridge delivery are not shard-gated on a real cluster**: the
  outbound sender is in memory and would run on every replica; the appservice pump moves with
  its shard in a unit test with scripted ownership only.
- **Two pods have never talked**, but the pieces are in place (2026-09-27): a replica advertises
  `cluster.mesh.advertise_address` to its peers, which the chart sets per pod to its stable DNS
  name under the headless Service; the mesh runs mutual TLS from `cluster.mesh.tls` (a private
  CA, mounted from a `kubernetes.io/tls` Secret); a third process with a certificate from
  another CA is refused. Verified as three processes on one host and one PostgreSQL
  (`docs/status/03-cluster.md`); `deploy/helm/hs/values-two-replica-experiment.yaml` is the
  values file for the same thing on a real cluster, not yet run. One same-host caveat found on
  the way: the configuration's database layer outranks the file (RFC 0016), so two processes
  from one database cannot differ in `listeners`; every pod in Kubernetes has the same
  listeners, so the chart is unaffected.
- **Nothing is measured.** Every number in `PLAN.md` section 13 is a target. No loadgen run
  has compared one replica to two.

## What would turn the first table into facts

In order, each a transcript in `docs/status/`:

1. Two pods in `mode=cluster` on the verification cluster, which already runs CloudNativePG:
   the pod's IP advertised to the mesh, a room created on one and written through both, from
   Element.
2. ~~The user session owner: `/sync` routed to, or woken by, the right replica, so a client can
   connect to any pod and see every room.~~ Done as "woken by", as two processes on one host
   (2026-09-27); a client may reach any replica. A non-owner's room reads are incremental since
   2026-10-01 (RFC 0018). Left: the same on two pods, and the owner's per-member fan-out writes
   (`docs/next-steps.md`, known gaps), which the three-replica measurement found to dominate.
3. `hs-loadgen` against one replica, then two, then three, on the same PostgreSQL: connected
   users and active rooms at a fixed sync p99. The slope of that line is the number this
   document is really about, and it does not exist yet.
4. A rolling restart under that load, counting failed requests; the target is zero.

## Sizing rule of thumb, until there are numbers

Start with one replica and the embedded store; it is the fastest configuration for one
machine and the one this project tests most. Move to cluster mode when you need availability
(a node can go away) or when one machine's CPU is saturated across many rooms. Do not move to
cluster mode to make one huge room faster; it will not. Size PostgreSQL first, because it is
the ceiling for everything.
