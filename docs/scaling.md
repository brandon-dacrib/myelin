# What adding a replica gets you, and what it does not

Written 2026-09-26. This is the plain answer to "if I set `replicas: 3`, what do I get?", and
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
| Clients connected at the same time | open `/sync` long-polls and per-user change trackers are memory and fan-out work, spread across owners |
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
- **`/sync` is not cluster-aware.** A client's `/sync` is served by whichever replica it
  reaches, from that replica's own feeds, and the session hub watches only its own replica's
  room stream (`hub.watch_all(rooms.subscribe_global())` in `crates/hs-cli/src/serve.rs`). So a
  long-poll on replica B for a room replica A owns is not woken by A's events. The design's
  user-session owner, woken by room owners over the mesh (`PLAN.md` section 5.4), is not built.
  This is the reason the "clients connected" row above is design, not fact.
- **`/createRoom` is shard-gated in `hs-cli`, and `hs-room` does not yet honour it** (RFC 0018,
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
2. The user session owner: `/sync` routed to, or woken by, the right replica, so a client can
   connect to any pod and see every room. Until then, clients must all reach the same replica,
   which makes N replicas an availability feature and not a capacity one.
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
