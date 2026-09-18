# 03 Cluster: status

Updated: 2026-09-18 (integration-review follow-up: strengthened the chaos suite's headline test,
verified by mutation testing; added mesh connection pooling and its own tests; assessed and
deferred Kubernetes `Lease` membership).

## Integration review follow-up (this update)

An integration pass mutation-tested `Fence::check` by making it unconditionally return `Ok` (i.e.
disabling fencing entirely) and ran the chaos suite. Only `a_partitioned_replica_cannot_write_after_being_fenced`
failed; `no_two_replicas_ever_commit_the_same_shard_epoch` -- the test whose name claims to guard
that exact property -- passed, because its replica kill was a graceful removal from the harness's
own bookkeeping: nothing ever attempted a write with a stale fence after ownership moved on, so
the fence was never adversarially exercised.

Fixed: `no_two_replicas_ever_commit_the_same_shard_epoch` now captures the doomed replica's fence
for every shard it owns *before* killing it, and keeps retrying writes with that stale, pre-kill
fence (as a would-be caller retrying against a last-known fence would) on every round for the rest
of the test, gated on the store actually showing a new epoch (so it never asserts before a real
new owner exists). Every such attempt must fail. Re-running the same mutation afterward:

- **Before the fix**: 4 of 5 chaos tests passed under the mutation (only the partition test
  failed) -- the headline test gave false confidence.
- **After the fix**: 2 of 5 chaos tests fail under the mutation
  (`no_two_replicas_ever_commit_the_same_shard_epoch` and
  `a_partitioned_replica_cannot_write_after_being_fenced`), plus a third failure in the lib suite
  (`fence::tests::fence_fails_after_another_replica_acquires`) -- 3 tests total across the crate
  now catch a disabled fence, exceeding the "at least two" bar. The mutation was applied, verified
  to produce these failures, then reverted; `cargo test -p hs-cluster` is green again post-revert.

Also strengthened `a_partitioned_replica_cannot_write_after_being_fenced` to cover the second
requested shape ("a stalled store on one replica while the others make progress"): the surviving
replica now makes five further successful commits while the partitioned one stays cut, and the
partitioned replica's stale fence is re-checked as still-rejected after that progress, not just at
the moment of takeover.

Also implemented mesh connection pooling (previously listed under "Next") and added end-to-end
tests for it that exercise the real mesh transport over a socket for the first time in this
crate's test suite. Kubernetes `Lease` membership was assessed and deliberately left out as more
than a contained change; see Decisions below for why.

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
  - `forwarder.rs`: `Forwarder` -- pools one persistent, multiplexed HTTP/2 connection per peer
    address (`hyper`'s `SendRequest` handles are `Clone`, safe to share concurrently across
    forwards), evicting and redialing inline when a pooled connection turns out to be dead before
    surfacing a failure to its own retry loop, which separately retries on connection failure /
    `421` (ownership refresh) / `503` (bounded backoff) and enforces the hop limit and deadline.
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
    waits for takeover, asserts a write using the partitioned replica's stale fence is rejected,
    asserts the new owner keeps successfully committing (five further writes) while the
    partitioned replica stays cut, and asserts the stale fence is still rejected after that.
  - `no_two_replicas_ever_commit_the_same_shard_epoch` additionally captures the killed replica's
    fence for every shard it owned and keeps retrying writes with that stale fence for the rest of
    the test once the store shows a new epoch for the shard, asserting every such attempt is
    rejected -- see "Integration review follow-up" above for why this was necessary.
- `crates/hs-cluster/tests/mesh_pool.rs`: end-to-end tests of `Forwarder` against a real socket (a
  minimal raw HTTP/2 peer, not `MeshServer`, so what is measured is unambiguously `Forwarder`'s own
  behavior). 2 tests: `forwarder_reuses_one_connection_across_many_forwards` (5 forwards to the
  same peer accept exactly 1 TCP connection) and
  `forwarder_redials_after_the_pooled_connection_is_gone` (the peer silently drops the first
  connection; the next forward still succeeds, having dialed exactly one fresh connection). This
  is also the first test in the crate to exercise the mesh transport over a real socket at all --
  every other test calls `Ownership`/`ChaosLog` directly.
- `deploy/chaos/`: Kubernetes chaos manifests and scripts, **UNTESTED** (Docker is not running on
  this machine; see `deploy/chaos/README.md` for exactly what that means and what still depends on
  `hs-cli`'s not-yet-built `hs chaos-actor` subcommand). Manifests for a disposable namespace,
  single-instance PostgreSQL behind a `toxiproxy` sidecar, a headless Service for mesh discovery,
  a 4-replica StatefulSet running the (future) chaos actor in shared-secret mesh-auth mode, and
  baseline/partition `NetworkPolicy` pairs. Scripts for pod-kill, partition, slow-store and
  rolling-update scenarios plus a `checker.py` linearizability checker mirroring the in-process
  harness's invariant.
- Quality bar: `cargo fmt -p hs-cluster` clean, `cargo clippy -p hs-cluster --all-targets -- -D
  warnings` clean, `cargo test -p hs-cluster` (47 tests: 40 lib + 5 chaos + 2 mesh-pool
  integration, 0 doctests) all green, run repeatedly (chaos and mesh-pool suites specifically, 5x
  each) with no observed flakiness.

## In progress

- Nothing actively in progress; the assignment above is complete for Phase 0.

## Next (not done; for whoever picks this up next)

- Kubernetes `Lease` membership (RFC section 4: "the store may be the data store ... or, in Phase
  1, Kubernetes `coordination.k8s.io/v1` Leases via `kube-rs`"). Only the store-based path is
  implemented; it works everywhere including single-node and is what `deploy/chaos/` exercises.
  Assessed during this update and deliberately deferred rather than attempted as a "cheap fix":
  see Decisions below for the specific reasons (new heavy dependency, a pluggable-membership-source
  abstraction, and zero ability to test it in this environment).
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
- `Forwarder` now pools one persistent HTTP/2 connection per peer (`std::sync::Mutex<HashMap<String,
  SendRequest<...>>>`, evict-and-redial-once-inline on a dead pooled connection). The initial
  Phase 0 cut of shipping a per-forward-fresh-connection client first was reasonable to get
  something correct out the door, but the pooled version is not meaningfully more complex and
  removes a real gap (a fresh TCP + TLS + HTTP/2 handshake per forward is expensive at scale), so
  it was worth doing in the same pass rather than leaving it for later.
- Kubernetes `Lease` membership was assessed and deliberately **not** attempted in this pass, even
  though asked to fix it "if cheap": it requires a new, heavy dependency (`kube-rs` plus
  `k8s-openapi`, neither in the workspace today), a new pluggable-membership-source abstraction
  (RFC section 4 is explicit that only membership can move to Leases -- shard rows must stay in the
  data store, so this isn't a drop-in swap of the existing registry, it's a second code path
  alongside it), and -- unlike everything else in this pass -- there is no way to test it at all in
  this environment (no Kubernetes API reachable, Docker not running), so it would ship as far
  less-verified code than the rest of this crate. That combination made it not a contained change
  by this track's own quality bar (RFC 0001 section 2's "day-one" scope, and the workspace
  convention of tests for everything claimed to work), so it is left for whoever has a cluster to
  test against.
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
cargo test -p hs-cluster                # 40 lib tests + 5 chaos tests + 2 mesh-pool tests + 0 doctests
cargo test -p hs-cluster --test chaos      # just the chaos harness, if iterating on it alone
cargo test -p hs-cluster --test mesh_pool  # just the connection-pooling tests
```

To re-verify the chaos suite's fencing coverage by mutation (as this update's integration review
did): in `crates/hs-cluster/src/fence.rs`, make `Fence::check` `{ return Ok(()); }` unconditionally,
run `cargo test -p hs-cluster`, confirm `fence::tests::fence_fails_after_another_replica_acquires`
and the chaos tests `no_two_replicas_ever_commit_the_same_shard_epoch` and
`a_partitioned_replica_cannot_write_after_being_fenced` fail (3 tests), then revert and confirm
everything is green again.

`deploy/chaos/` is not runnable in this environment (no Docker); see its README for what running
it for real requires.
