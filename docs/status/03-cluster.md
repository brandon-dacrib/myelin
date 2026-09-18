# 03 Cluster: status

Updated: 2026-09-17 (day one).

## Done
- `docs/rfcs/0001-cluster-ownership.md`: the design (shard count, registry and heartbeats, failure detection, rendezvous hashing, fencing epoch, forwarding with idempotency keys, backpressure, mesh auth, single-node mode, handoff sequence, chaos plan, metrics, interface draft).

## In progress
- `crates/hs-cluster`: `LeaseStore` trait and in-memory implementation.

## Next
- Replica registry, lease manager, rendezvous hashing, ownership API, fencing API.
- Mesh (hyper HTTP/2, rustls mTLS, shared-secret mode), forwarding envelope, retries.
- Handoff and graceful shutdown API.
- Toy replicated-log actor and in-process chaos harness; `deploy/chaos/` kind manifests (untested).
- Metrics.

## Blockers
- None. `hs-kv` (01) is not published yet; `hs-cluster` ships its own in-memory `LeaseStore` and an `EpochReader` trait that will be implemented for `hs-kv`'s transaction in this crate when it lands.

## Interfaces provided
- Draft in RFC 0001 section 13. Frozen at week 6.

## Interfaces needed
- 01: transaction type with a `get` usable inside the transaction (for `EpochReader`); an `hs-kv`-backed `LeaseStore`; SlateDB per-shard open with the epoch.
- 07: `RequesterContext` type (carried as JSON until then).
- 12: readiness wiring, cert-manager per-pod certificates, `kind` job for `deploy/chaos/`; `hs-telemetry` conventions.
- 13: the `cluster` config section (RFC 0001 section 17).

## Decisions made
- See RFC 0001 section 16: 256 room and 256 user shards by default (fixed at creation, 1024 recommended above 16 replicas); forward not redirect; mTLS in production with a shared-secret mode for tests; the epoch read inside every owner transaction is mandatory; single-node mode is a runtime flag with an inert manager; failure detection uses observer-local monotonic time gated on the observer's own heartbeat success; the owner releases and the desired owner acquires (store row is the truth).

## Shared dependencies added
- (none yet)
