# Cluster chaos suite (Kubernetes / `kind`)

**Status: UNTESTED.** Docker is not available in the environment these were written in, so none
of this has been run. It is written to the design in `docs/rfcs/0001-cluster-ownership.md`
section 14 and to track 03's brief (`docs/workstreams/03-cluster.md`), and it is the companion to
the in-process harness at `crates/hs-cluster/tests/chaos.rs`, which *is* tested (`cargo test -p
hs-cluster --test chaos`) and exercises the same safety properties without Kubernetes. Whoever
picks this up with Docker available should treat every manifest and script here as a first draft:
run it, fix what is wrong, and delete this paragraph.

## What this depends on that does not exist yet

- **`hs chaos-actor`**: a subcommand on the `hs` binary (owned by track 15, `hs-cli`) that runs
  the same toy replicated-log actor as `crates/hs-cluster/tests/chaos.rs`'s `ChaosLog`, but as a
  standalone server: it joins the cluster over `hs-cluster`, owns shards, exposes an HTTP endpoint
  to append records and read them back, and writes its committed-write log to a file (or stdout)
  in the format the checker script parses. It does not exist yet; `manifests/statefulset.yaml`
  references `hs serve --chaos-actor` as a placeholder invocation. Track 03 owns the `hs-cluster`
  side (this directory); track 15 owns wiring the subcommand once `hs-cli` exists.
- **A `kind` cluster** with a container registry the chaos image can be pushed to, and
  **cert-manager** (or the shared-secret mesh auth mode, which needs no certificates and is the
  easier path for this suite specifically -- see `manifests/statefulset.yaml`'s
  `HS_CLUSTER_MESH_AUTH=shared-secret` env var).
- **`toxiproxy`** for the slow-store simulation in front of PostgreSQL (`manifests/toxiproxy.yaml`).

## Layout

```
deploy/chaos/
  manifests/
    namespace.yaml              # isolated namespace, deleted between runs
    postgres.yaml                # single-instance PostgreSQL (not HA -- this is a chaos target, not production)
    toxiproxy.yaml                # sidecar in front of postgres for slow-store injection
    headless-service.yaml         # mesh peer discovery (RFC 0001 section 11)
    statefulset.yaml               # N replicas of the chaos actor
    network-policy-baseline.yaml   # default: full mesh connectivity
    network-policy-partition.yaml  # applied during the partition scenario: isolates one pod
  scripts/
    run-all.sh          # bootstrap kind, apply manifests, run every scenario, tear down
    scenario-pod-kill.sh
    scenario-partition.sh
    scenario-slow-store.sh
    scenario-rolling-update.sh
    checker.py           # the per-shard linearizability checker (RFC 0001 section 14 / 14's owner)
```

## Scenarios

Each scenario script assumes `manifests/` is already applied and the StatefulSet is `Ready`
(`kubectl -n hs-chaos rollout status statefulset/hs-chaos-actor`).

1. **Pod kill** (`scenario-pod-kill.sh`): `kubectl delete pod` on a random replica; asserts (via
   `checker.py`) that every shard it owned has a new owner within `lease_ttl + heartbeat_interval`
   plus one tick, matching the in-process test `failover_completes_within_configured_ttl`.
2. **Partition** (`scenario-partition.sh`): applies `network-policy-partition.yaml`, which drops
   all traffic to and from one pod's mesh port (8449) while leaving its readiness probe
   reachable, so it looks alive to Kubernetes but cannot heartbeat the store or be forwarded to;
   asserts the rest of the cluster takes over its shards and the partitioned pod's own writes
   (checked against its local log) stop landing once the epoch has moved, matching the in-process
   test `a_partitioned_replica_cannot_write_after_being_fenced`.
3. **Slow store** (`scenario-slow-store.sh`): uses the `toxiproxy` HTTP API to inject latency
   between PostgreSQL and every replica; asserts no second writer appears for any shard while the
   store is slow (RFC 0001's risk: "store stalls masquerading as owner death").
4. **Rolling update** (`scenario-rolling-update.sh`): patches the StatefulSet image tag and watches
   the rollout; asserts `hs_cluster_forward_retries_total` and p99 forward latency (scraped from
   `/metrics`) stay flat across the rollout, and that `checker.py` reports no anomalies, matching
   RFC 0001 section 10's rolling-update claim.

`checker.py` reads each pod's committed-write log (via `kubectl logs` or a shared volume) and
applies the same invariant `crates/hs-cluster/tests/chaos.rs` checks in-process: for every shard,
every committed epoch has exactly one writer and epochs are non-decreasing.

## Running (once Docker and `hs chaos-actor` exist)

```sh
./scripts/run-all.sh
```

Tears everything down on exit (`trap ... EXIT` in the script) regardless of pass or fail.
