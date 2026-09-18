# 03. Cluster

Wave 1, starts day one. Owns everything that makes N replicas behave as one server.

**Expert profile.** Distributed systems (leases, fencing, failure detection, exactly-once handoff), Kubernetes API, Rust async networking and TLS, chaos testing.

**Mission.** Sharded ownership of rooms and users across identical replicas, a mesh for forwarding, lease-based failover with fencing, graceful handoff on rollouts, and a single-node mode in which none of it runs. See `PLAN.md` sections 4 (D2), 5.1 to 5.4, 6.5, 7.2 and 7.3.

**Owns.** `hs-cluster` (replica registry, leases, rendezvous hashing over virtual shards, ownership API, mesh RPC, forwarding, fencing epochs, background-job leasing), the graceful shutdown sequence, the chaos suite (with 14), SlateDB per-shard lifecycle with 01.

**Provides.** Week 6: ownership API (`owner_of(shard)`, `is_mine`, `forward(request)`), mesh RPC envelope with mutual TLS, lease manager hooks, job leasing. Week 10: handoff protocol for rolling updates.

**Consumes.** `hs-kv` from 01 (membership rows, epochs, confirmations), `hs-config` from 13, `kube-rs` Lease integration with 12.

**Day-one work.**
- Design document: virtual shard count (256 versus 1024), lease TTLs, failure detection, rendezvous hashing, forwarding semantics with idempotency keys, backpressure, mesh authentication.
- Fencing design: every shard has an epoch in the store; every transaction by an owner reads the epoch key inside the transaction and aborts if it changed. On serializable backends this makes stale owners unable to write, including on PostgreSQL; on SlateDB it composes with manifest fencing. This is mandatory, not optional.
- A toy replicated log actor and a chaos harness on `kind` (pod kills, partitions via network policies or a proxy, slow store injection).

**Phase 0 deliverables.** Prototype passing chaos tests: no two replicas ever write the same shard (verified by epoch checks and by a checker over recorded writes), failover under 5 s, no lost or duplicated writes for the toy actor, handoff before pod stop on rolling updates; single-node mode with the cluster code compiled out or inert; metrics for ownership churn, forward latency and lease age.

**Phase 1 and 2 deliverables.** Integration with the room and user actors (04, 05), federation sender shards (06), appservice shards (11); background jobs leased one-of-N; Kubernetes Lease membership when the API is reachable, store heartbeats otherwise; zone awareness; SlateDB shard open and close with fencing; `hs cluster status` and drain commands; multi-version rolling upgrade support for one release step.

**Definition of done.** Chaos suite in CI on `kind`; per-shard linearizability checker (with 14) reports no anomalies across the suite; failover and forwarding benchmarks tracked; the design doc is the reference for 12's probes and rollout settings.

**References.** Kubernetes Lease API; Orleans virtual actors; Kafka partition assignment and rebalancing; SlateDB manifest fencing; `refs/synapse/docs/workers.md` (the stream-writer model this replaces); `refs/palpo/crates/data/migrations/2026-03-09-000001_cluster_support/up.sql` (what a server has to move out of memory to become multi-instance).

**Open questions to settle first.** Shard count; forward versus redirect for client requests (forward, since clients sit behind one Service); mTLS versus shared-secret mesh auth; the cost of the epoch read per transaction; how single-node mode is selected (runtime flag recommended, with an inert ownership manager).

**Risks.** Store stalls masquerading as owner death (the fencing epoch makes this safe but not free); a hot room saturating its owner (owner-side batching and the option to move shards).
