# 0012. Draining a replica is a request in the shared store, and it outlives the replica

Status: accepted, 2026-09-28. Author: track 15 (the Cluster admin area).
Applies to tracks 03 (`hs-cluster`), 12 (operator, chart), 15 (admin API) and 16 (`web/`).

## Context

`cluster.replicas.drain` and `cluster.replicas.undrain` were declared in the admin API from the
start and answered 501. `hs-cluster` could drain only the process it runs in, and only on the
way out: `Cluster::drain` (what `SIGTERM` calls) releases every shard, deregisters the replica
and stops its heartbeat loop. There was no way to drain a peer, and no way back short of a
restart. An earlier attempt (branch `worktree-agent-ae592ed29bb65b973`, superseded) answered
`503` for a peer ("ask it directly") and for any undrain ("restart it"), which is honest but
leaves an operator without the thing a drain is for: taking a replica out of service without
stopping it, and putting it back.

## Decision

- **A drain is a row, not a message.** `ClusterStore::request_drain(id, DrainRequest)` writes
  `drain/<replica id>` in the `cluster_replicas` keyspace: who asked, when, and the admin task
  following it. Whichever replica receives the admin request writes it; no mesh call is needed,
  and the store is the one thing every replica already reads every heartbeat.
- **The named replica honours it itself.** At each heartbeat `KvOwnership` reads its own drain
  request. While one is in force it heartbeats as `Draining` (so the others stop hashing shards
  onto it) and its convergence releases every shard it holds, which the others then acquire:
  RFC 0001 section 10's handoff without the shutdown. It stays ready and keeps serving,
  forwarding each request to the shard's owner.
- **Undrain deletes the row.** At its next heartbeat the replica is `Active` again, the others
  release the shards rendezvous hashing now gives it, and it takes them.
- **It outlives the replica.** Because the request is in the store, a drained replica that
  restarts comes back drained (its first heartbeat already reads the request), and a drained
  replica that is stopped stays listed by the admin API, `drained`, until it is undrained. This
  is the Kubernetes cordon, not a one-shot action; an operator draining a replica before
  replacing its node gets the replacement drained too, and undrains it when ready.
- **Refused when nothing would take the shards.** `409` when no *other* replica is active and
  undrained, which includes a single node. After writing a request the admin side looks again
  and withdraws it if a concurrent drain left nothing serving.
- **Followed by a task.** The drain answers `200` with the replica (`draining`, as the frozen
  contract says) and starts `cluster.replicas.drain` in the task registry, which reports shards
  handed off as progress and succeeds when the replica owns none; its id is on the replica
  (`drain_task_id`). Undrain cancels it. The task times out after 15 minutes without failing the
  drain itself.
- **Observed.** Audited as `cluster.replicas.drain` / `cluster.replicas.undrain`, published as
  `cluster.replica_draining` / `cluster.replica_undrained`, logged on the requesting replica and
  on the drained one, and counted in `hs_cluster_admin_drains_total{event}` and
  `hs_cluster_admin_drain_duration_seconds`.

## Consequences

- `ReplicaState::Draining` in a heartbeat row now means either "shutting down" or "drained by an
  administrator"; the admin API tells them apart by the drain request.
- The operator (track 12) can drain a pod before evicting it by calling the admin API instead of
  relying on `SIGTERM`'s twenty seconds, and must undrain a replacement explicitly if it drained
  the old one's id (pod names are stable in a StatefulSet, so the replacement inherits the
  request by design).
- Found on the way and fixed in `hs-cluster`: a heartbeat tick already in flight when
  `Cluster::drain` deregistered a replica could write its row back, so a replica restarted within
  the lease was refused as a live duplicate of itself. The heartbeat loop and the deregistration
  now exclude each other (`KvOwnership::tick_lock`).
