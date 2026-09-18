# 03 Cluster: status

Updated: 2026-09-18.

## Done

- `docs/rfcs/0001-cluster-ownership.md`: reconciled with the landed `hs-kv` (section 0) and
  finished -- virtual shard count with rationale, rendezvous hashing, replica registry and
  heartbeats, lease TTLs and failure detection, the fencing epoch built on `hs-kv`'s transactional
  read-set validation, forwarding with idempotency keys, backpressure, mesh authentication,
  single-node mode, and the graceful handoff sequence for rolling updates. Section 13 (interfaces)
  and section 17 (what other tracks need) rewritten to match what is actually implemented; the
  `LeaseStore`/`EpochReader` split the day-one draft proposed was dropped (see below).
- `crates/hs-cluster/src/types.rs`, `hash.rs`: surviving day-one code, kept. Fixed two "pinned"
  test constants that were placeholder values never actually computed from the real
  implementation (`hash_is_pinned`, `layout_counts_and_mapping_are_stable`) -- the hashing and
  shard-mapping code itself was correct, only the hard-coded expected values in the tests were
  wrong. `gen` as a parameter name also had to be renamed (reserved keyword in edition 2024).
- `crates/hs-cluster/src/store.rs`: `ClusterStore<B: hs_kv::KvBackend>` -- replica registry
  (`heartbeat`, `list_replicas`, `remove_replica`, with the generation-guard rule from RFC section
  4), shard rows (`get_shard`, `list_shards`, `acquire_shard`, `release_shard`, all built as plain
  `hs_kv::transact` read-modify-write closures -- no separate CAS primitive needed), and the shard
  layout (`init_layout`, `get_layout`, immutable after first write). 9 tests.
- `crates/hs-cluster/src/fence.rs`: `Fence` (epoch proof) and `Fence::check`, generic over any
  `hs_kv::KvRead`, so it works against every backend's transaction/snapshot type unchanged. 3
  tests demonstrating a fence passes while unchanged, fails after another replica acquires, and is
  a no-op in single-node mode.
- `crates/hs-cluster/src/ownership.rs`: `Ownership` and `Drainable` traits; `SingleNode` (inert);
  `KvOwnership<B>` -- the heartbeat + rendezvous-convergence background task, self-suspicion,
  `owner_of`/`is_mine`/`fence`/`subscribe`/`shard_map`, and `drain` (the graceful handoff
  sequence: flip to `Draining`, release owned shards in parallel batches up to
  `handoff.parallelism`, wait for each to show a new owner up to the deadline, deregister). 4
  tests including two live replicas partitioning the shard space with no overlap and a
  single-replica drain.
- `crates/hs-cluster/src/config.rs`: `ClusterConfig`, `MeshConfig`, `HandoffConfig` with the RFC's
  documented defaults (256/256/64/64/1 shards by default via `ShardLayout`, 1s heartbeat, 3s
  lease TTL, 3 max hops, 4 max attempts, 1024 max in-flight per peer, 64 max fan-out).
- `crates/hs-cluster/src/mesh/`: the internal mesh.
  - `auth.rs`: `AuthMode`, `Authenticator` trait, `SharedSecretAuthenticator` (constant-time via
    `subtle`), `MutualTlsAuthenticator` (SAN-suffix check over what the TLS layer already
    verified). 5 tests.
  - `tls.rs`: `TlsMaterial` (loads CA/cert/key PEM via `rustls-pemfile`, builds `rustls`
    server/client configs requiring mutual auth), `fingerprint_sha256_hex`, and `extract_dns_sans`
    -- a small, purpose-built DER walker for the `subjectAltName` extension (no general X.509
    parser dependency was available in the workspace, so it locates the extension by its unique
    DER-encoded OID byte pattern rather than parsing the whole certificate structure). 3 tests
    against `rcgen`-generated certificates.
  - `envelope.rs`: `Envelope`, `Reply`, `IdempotencyKey`, `RequesterContext` (carried as
    `serde_json::Value` until 07 freezes a type, per the RFC), `ShardHandler` trait.
  - `idempotency.rs`: `IdempotencyCache`, bounded and TTL'd, per shard. 4 tests.
  - `server.rs`: `MeshServer` (HTTP/2 over `hyper`, TLS or plaintext), `MeshDeps`. Handles
    `POST /mesh/v1/forward` (auth, ownership/fencing check, idempotency cache, bounded in-flight
    semaphore, dispatch to `ShardHandler`) and `POST /mesh/v1/released` (nudges the local
    convergence loop).
  - `forwarder.rs`: `Forwarder` -- opens a fresh HTTP/2 connection per forward (connection pooling
    is *not* implemented yet, see Decisions), retries on connection failure / `421` (ownership
    refresh) / `503` (bounded backoff), enforces the hop limit and deadline, records metrics.
- `crates/hs-cluster/src/cluster.rs`: `Cluster` facade -- `single_node`, `start`, `ownership`,
  `ready`, `drain` -- matching RFC section 13's documented lifecycle surface. 2 tests.
- `crates/hs-cluster/src/metrics.rs`: `ClusterMetrics` / `MetricsSnapshot` with the fields RFC
  section 15 names (ownership churn by reason, owned-shard gauge, forward latency histogram by
  route/outcome, forward retries by reason, fenced count, live replicas, lease age). 2 tests.
- `crates/hs-cluster/tests/chaos.rs`: the in-process chaos harness. A toy replicated-log actor
  (`ChaosLog`) whose every `append` is a real `hs_kv` transaction that calls `Fence::check` exactly
  as a production actor must, plus `FaultyBackend<B>` (a `KvBackend` wrapper that can be "cut" to
  simulate a partitioned/stalled store). 5 tests, all on the `tokio` paused clock for deterministic
  timing:
  - `no_two_replicas_ever_commit_the_same_shard_epoch`: 4 replicas, 40 rounds of appends across 17
    shards with a replica kill partway through; asserts every committed epoch per shard has
    exactly one writer and epochs never go backwards.
  - `failover_completes_within_configured_ttl`: kills the owner, measures how long the survivor
    takes to take over, asserts it is within the configured TTL.
  - `retried_append_is_not_duplicated`: same idempotency key appended twice, asserts exactly one
    committed entry and identical results.
  - `drain_hands_off_to_a_live_peer_before_stopping`: two replicas, drains one while the other's
    convergence loop runs concurrently, asserts a nonzero handoff count and that ownership fully
    converges onto the survivor.
  - `a_partitioned_replica_cannot_write_after_being_fenced`: cuts one replica's store connection,
    waits for takeover, asserts a write using the partitioned replica's stale fence is rejected
    and a write using the new owner's fresh fence succeeds.
- `deploy/chaos/`: Kubernetes chaos manifests and scripts, **UNTESTED** (Docker is not running on
  this machine; see `deploy/chaos/README.md` for exactly what that means and what still depends on
  `hs-cli`'s not-yet-built `hs chaos-actor` subcommand). Manifests for a disposable namespace,
  single-instance PostgreSQL behind a `toxiproxy` sidecar, a headless Service for mesh discovery,
  a 4-replica StatefulSet running the (future) chaos actor in shared-secret mesh-auth mode, and
  baseline/partition `NetworkPolicy` pairs. Scripts for pod-kill, partition, slow-store and
  rolling-update scenarios plus a `checker.py` linearizability checker mirroring the in-process
  harness's invariant.
- Quality bar: `cargo fmt -p hs-cluster` clean, `cargo clippy -p hs-cluster --all-targets -- -D
  warnings` clean, `cargo test -p hs-cluster` (45 unit/integration tests) and `cargo test -p
  hs-cluster --test chaos` (5 tests) all green, run repeatedly with no observed flakiness.

## In progress

- Nothing actively in progress; the assignment above is complete for Phase 0.

## Next (not done; for whoever picks this up next)

- Mesh connection pooling: `Forwarder` opens a fresh HTTP/2 connection per forward today. The RFC
  calls for one persistent, multiplexed HTTP/2 connection per peer; this is a real gap for
  forward-latency and connection-count at scale, just not a correctness one.
- Kubernetes `Lease` membership (RFC section 4: "the store may be the data store ... or, in Phase
  1, Kubernetes `coordination.k8s.io/v1` Leases via `kube-rs`"). Only the store-based path is
  implemented; it works everywhere including single-node and is what `deploy/chaos/` exercises.
- Weighted rendezvous hashing (zone spread, capacity) and the per-shard pin row for moving a hot
  room's shard -- both explicitly Phase 1 in the RFC.
- SlateDB per-shard open taking the epoch for manifest fencing -- blocked on the SlateDB backend
  landing in `hs-kv`.
- `hs cluster status` / drain CLI commands -- blocked on `hs-cli` (track 15) existing.
- Wiring `ClusterMetrics` into `hs-telemetry`'s Prometheus registry once track 12 publishes it;
  until then `ClusterMetrics::snapshot()` is the interface.
- `deploy/chaos/` is unrun. It needs Docker, a `kind` cluster, and `hs chaos-actor` (see the
  README's "what this depends on that does not exist yet").
- Actual integration with 04's room actor / 05's user session actor / 06's federation sender
  shards / 11's appservice shards -- this is Phase 1/2 per the brief, and those tracks' actors do
  not exist yet.

## Blockers

- None for what was assigned. `deploy/chaos/` cannot be run without Docker and without track 15's
  `hs chaos-actor`, but that is documented, not blocking further track-03 work.

## Interfaces provided

- `hs_cluster::{Ownership, Drainable, ShardMap, OwnershipEvent, Readiness, DrainReport}` --
  `crates/hs-cluster/src/ownership.rs`.
- `hs_cluster::Fence` and `fence::read_epoch` -- `crates/hs-cluster/src/fence.rs`. Any crate
  storing shard data calls `fence.check(&txn, cluster_store.shard_keyspace())` as the last read
  before committing a transaction against that shard.
- `hs_cluster::store::ClusterStore<B: hs_kv::KvBackend>` -- `crates/hs-cluster/src/store.rs`.
- `hs_cluster::mesh::{Envelope, Reply, ShardHandler, Forwarder, MeshServer, MeshDeps,
  Authenticator, AuthMode, ...}` -- `crates/hs-cluster/src/mesh/`.
- `hs_cluster::Cluster` (`single_node`, `start`, `ownership`, `ready`, `drain`) --
  `crates/hs-cluster/src/cluster.rs`.
- `hs_cluster::{ClusterConfig, MeshConfig, HandoffConfig}` -- `crates/hs-cluster/src/config.rs`.
- `hs_cluster::metrics::ClusterMetrics` -- `crates/hs-cluster/src/metrics.rs`.
- All frozen as of this update; interface changes go through a new dated RFC per the workstream
  rules.

## Interfaces needed

- 01 Storage: nothing further for Fjall/PostgreSQL (built directly on `hs_kv::KvBackend`, see
  Decisions). When SlateDB lands, its per-shard open needs to accept the epoch
  `ClusterStore::acquire_shard` returns.
- 04, 05, 06, 11: their actors need to exist before shard ownership actually gates anything in
  production; until then, this crate's own chaos harness stands in as the proof of the contract
  (`fence.check` before every commit, drop state on `Lost`, persist idempotency keys durably).
- 07: `RequesterContext` as a real type; the mesh envelope carries `serde_json::Value` until then.
- 12: readiness should map to `Cluster::ready()`; `terminationGracePeriodSeconds` should be at
  least `cluster.handoff.deadline` plus a margin; cert-manager (or an equivalent) for per-pod mTLS
  certificates if mTLS mode is used in production; a `kind` job to actually run `deploy/chaos/`.
- 13: the `cluster` config section shape is `ClusterConfig`/`MeshConfig`/`HandoffConfig`
  (`crates/hs-cluster/src/config.rs`); 13 owns turning YAML into these types.
- 15: `hs chaos-actor` subcommand for `deploy/chaos/` to have anything to actually deploy.

## Decisions made

- **The `LeaseStore` / `EpochReader` split from the day-one RFC draft was dropped.** `hs-kv`
  landed with full serializable snapshot isolation on every backend and its own docs name track
  03's fencing pattern as the reason no separate fencing primitive is needed. `hs-cluster` builds
  directly on `hs_kv::KvBackend` (`ClusterStore<B>`) and `hs_kv::KvRead` (`Fence::check`, generic
  over any backend's transaction/snapshot type) instead. No code ever shipped a `LeaseStore` trait
  (the previous status file listed it as "in progress" but no file existed), so there was nothing
  to migrate beyond the RFC text itself (RFC 0001 section 0).
- 256 room and 256 user shards by default, 64 federation, 64 appservice, 1 global; fixed at
  creation; 1024 recommended above 16 replicas (RFC section 3, carried over from day one,
  unchanged).
- Forward, not redirect; mTLS in production with a shared-secret mode for tests (RFC sections 8,
  11, carried over).
- The epoch read is mandatory inside every owner transaction; it is a single hot-key read with no
  steady-state conflicts (RFC section 6, carried over, now backed by a working implementation and
  the chaos harness's `a_partitioned_replica_cannot_write_after_being_fenced` test).
- Single-node mode is a runtime flag (`Cluster::single_node`) with an inert `SingleNode` manager
  (RFC section 12, carried over).
- Failure detection is observer-local monotonic time (`tokio::time::Instant`, deliberately *not*
  `std::time::Instant` -- see the comment in `ownership.rs`) gated on the observer's own recent
  heartbeat success, exactly as RFC section 4 specifies. Using `tokio::time::Instant` throughout
  was necessary (not just a style choice) for the paused-clock tests and the chaos harness to be
  deterministic; `std::time::Instant` does not respect `tokio::time::pause`/`advance`.
- The owner releases and the desired owner acquires; the store row is the truth, the hash is the
  convergence target (RFC section 7, carried over).
- Fjall (not just tests): the `hs-kv` fence/ownership API is generic over `hs_kv::KvBackend`, so it
  will work against Fjall or PostgreSQL without any change to this crate once those backends'
  `Txn`/`Snapshot` types exist and implement `KvRead`/`KvWrite` (they already must, per `hs-kv`'s
  own contract).
- `Forwarder` does not pool connections in this Phase 0 implementation (see "Next"); this was a
  deliberate scope cut to ship a correct, simple client first rather than a persistent-connection
  pool with its own lifecycle and error-recovery surface.
- Test timing pattern: background work parked behind `tokio::task::spawn_blocking` (used for every
  store operation, since `hs_kv`'s API is synchronous) needs real executor polls to be noticed, not
  just virtual-clock advancement. Tests use a `settle()` helper that interleaves
  `tokio::time::advance` with many `tokio::task::yield_now().await` calls per round; a single
  `advance()` plus one `yield_now()` was observed to be insufficient and left tests hanging
  indefinitely on the very first `spawn_blocking` join past the first one. This is worth knowing
  for any other track writing async tests against a `spawn_blocking`-heavy API under
  `start_paused = true`.

## Shared dependencies added

- `hs-kv` added as a path dependency of `hs-cluster` (`crates/hs-cluster/Cargo.toml`), per this
  track's instructions to build directly on it. Already present in the workspace (track 01).
- `rustls-pemfile` added to `crates/hs-cluster/Cargo.toml` as `{ workspace = true }`. It was
  already present in `[workspace.dependencies]` (added by another track) but not yet referenced by
  any crate's own `Cargo.toml`; no version was added or changed.
- No new entries were added to `[workspace.dependencies]` in the root `Cargo.toml`. Everything
  else this crate needed (`rustls`, `tokio-rustls`, `hyper-util`, `rcgen`, `subtle`, `hyper`,
  `http`, `http-body-util`, `xxhash-rust`, `sha2`, `base64`, `hex`, `rand`, `async-trait`) was
  already listed there per the task's setup.

## How to verify

```sh
cargo fmt -p hs-cluster -- --check
cargo clippy -p hs-cluster --all-targets -- -D warnings
cargo test -p hs-cluster            # 40 unit tests + 5 chaos tests + 0 doctests
cargo test -p hs-cluster --test chaos   # just the chaos harness, if iterating on it alone
```

`deploy/chaos/` is not runnable in this environment (no Docker); see its README for what running
it for real requires.
