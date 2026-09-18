# RFC 0001. Cluster ownership: shards, leases, fencing, mesh, handoff

Date: 2026-09-17. Updated: 2026-09-18 (reconciled with the landed `hs-kv`; implementation caught up
to the design, see section 18). Owner: track 03 (Cluster). Status: implemented for Phase 0 --
the reference for `hs-cluster`, for track 12's probes and rollout settings, and for the storage
hooks track 01 provides.

Consumers: 04 Room, 05 Sync, 06 Federation, 11 Appservices (ownership API and forwarding), 12 Platform (probes, leases, rollout timings, mesh certificates), 01 Storage (epoch key inside transactions, SlateDB per-shard fencing), 13 Config (the `cluster` config section), 14 Test (chaos suite).

## 0. Reconciliation with the landed `hs-kv` (2026-09-18)

This RFC was written before track 01's `hs-kv` landed, on the assumption (section 6, section 12's
draft interface) that `hs-cluster` would need its own `LeaseStore` trait plus an `EpochReader`
trait implemented later for `hs-kv`'s transaction type. `hs-kv` has now landed
(`crates/hs-kv/src/lib.rs`) and its crate-level documentation states the guarantee section 6 of
this RFC predicted almost exactly: full serializable snapshot isolation on every backend, where a
transaction that reads a key and later commits successfully is a guarantee the key did not change
out from under it, and where the crate's own docs name this exact pattern ("Track 03's lease and
epoch fencing is exactly this pattern ... No separate fencing primitive exists in this trait
because none is needed").

Consequence: **the day-one `LeaseStore` / `EpochReader` split described in the original section 12
was never implemented and is superseded.** `hs-cluster` has no store trait of its own. Every design
decision below (virtual shard count, rendezvous hashing, the registry, the fencing epoch, the
convergence loop, forwarding, backpressure, mesh auth, single-node mode, handoff) is unchanged; only
the plumbing in section 12 and the storage-hooks paragraph of section 17 are replaced with what is
actually implemented, directly against [`hs_kv::KvBackend`](../../crates/hs-kv/src/lib.rs):

- `crates/hs-cluster/src/store.rs`: `ClusterStore<B: KvBackend>` -- the replica registry, shard
  rows and shard layout, each operation an ordinary `hs_kv::transact` closure (read-modify-write).
  There is no `cas_shard` primitive distinct from "read the row, decide, write the row": `hs-kv`'s
  SSI conflict check *is* the compare-and-swap, and `transact`'s retry (re-running the closure from
  scratch on a conflict) is exactly the acquire/release state machine of section 6.
- `crates/hs-cluster/src/fence.rs`: `Fence::check` takes any `T: hs_kv::KvRead` and the shard
  keyspace handle, reads the shard row inside the caller's own transaction, and compares epochs.
  Nothing needs to be implemented per backend: `KvRead` is already implemented by every backend's
  transaction and snapshot type, so this one function works unchanged against the in-memory
  backend, Fjall, and (when they land) PostgreSQL and SlateDB.
- `crates/hs-cluster/src/ownership.rs`: `KvOwnership<B: KvBackend>` is the clustered
  `Ownership`/`Drainable` implementation, generic over the backend, running the heartbeat and
  convergence loop from section 4/5/7 against `ClusterStore<B>`. `SingleNode` is the inert
  implementation from section 12.

Everything else in this document is normative as originally written.

## 1. Motivation and scope

`PLAN.md` D2 and sections 5.1 to 5.4, 6.5, 7.2 and 7.3: every replica runs every function, and each room, user session, federation destination queue and appservice queue has exactly one owner replica at a time. Requests arriving elsewhere are forwarded. Failover is lease expiry plus a cold load; rolling updates hand ownership off before a pod stops; one replica means one owner for everything and no mesh.

This RFC fixes the parts of that design that other tracks build against: the shard space, how ownership is decided and confirmed, how a stale owner is prevented from writing (fencing), the forwarding envelope and its retry semantics, backpressure, mesh authentication, single-node mode, and the graceful handoff sequence. Everything here is implemented in `crates/hs-cluster`; the API surface is summarised in section 12.

## 2. Terms

- **Replica**: one `hs serve` process. Identified by a `ReplicaId` (string; the pod name on Kubernetes, `HOSTNAME` otherwise) and a `Generation` (`u64`, strictly increasing per replica id across restarts).
- **Shard**: the unit of ownership. `ShardId { kind, index }`. Kinds: `Room`, `User`, `Federation` (outbound destination queues), `Appservice` (outbound transaction queues), `Global` (exactly one shard; singletons and background jobs). Rooms and users map to a shard by a stable hash of their identifier.
- **Epoch**: a `u64` per shard, stored in the data store, incremented every time ownership of the shard is acquired. The fencing token.
- **Owner**: the replica named in the shard's store row, holding the current epoch.
- **Desired owner**: the replica that rendezvous hashing selects for a shard over the current live membership. The desired owner and the owner differ transiently; the system converges.
- **Lease**: a replica's registry row with a heartbeat. A replica whose heartbeat has not advanced for `lease_ttl` is dead as far as ownership is concerned.

## 3. Virtual shard count

Decision: **256 room shards and 256 user shards by default, 64 federation shards, 64 appservice shards, 1 global shard. Configurable at cluster creation, recorded in the store, immutable afterwards** (`cluster.shards.rooms`, `cluster.shards.users`, and so on). A replica that boots with a layout that differs from the recorded one refuses to start.

Why 256 and not 1024:

- With SlateDB each shard is its own database (memtable, WAL, manifest, compaction). A replica owning 1024/N shards carries that overhead 1024/N times; at three replicas that is 341 open databases each. 256 keeps it at 85, and 8 at 32 replicas. With PostgreSQL the shards are only an ownership concept and the count costs nothing, but the layout has to be the same number for every backend so that a store export and import between backends does not re-shard.
- The shard count bounds the useful replica count. Rendezvous hashing assigns each shard independently, so a replica owns Binomial(S, 1/N) shards with relative spread about sqrt(N/S): 12% at 4 replicas, 25% at 16, 50% at 64 for S = 256. Room load is itself skewed by a few very large rooms, which dominates the shard-count imbalance below about 16 replicas. Above that, 1024 halves the spread. The recommendation for deployments planning more than 16 replicas is to create the cluster with 1024 room and user shards. The Phase 1 zone- and weight-aware hashing corrects the remainder.
- Failover granularity: a dying replica's shards are spread over all survivors, one shard at a time; the cold load is per room and lazy, so shard size does not affect time-to-first-write.
- Ownership churn on scale-out moves 1/N of the shards regardless of S; S only sets the granularity (1/256 of the keyspace per move).

Mapping: `shard = ShardId { kind: Room, index: xxh3_64(room_id) mod rooms }`, likewise for users (`user_id`), federation (`destination server name`) and appservices (`appservice id`). xxh3 with seed 0 is the hash; it is stable across versions and architectures, which rendezvous hashing and the shard mapping both require (`std::hash::DefaultHasher` is documented as unstable and must not be used for either).

## 4. Replica registry and heartbeats

Every replica has a row `cluster/replica/<id>`:

```
ReplicaRecord {
  id: ReplicaId,
  generation: u64,          // max(unix_ms at start, previous generation + 1)
  mesh_addr: String,        // host:port for the mesh listener
  zone: Option<String>,     // topology.kubernetes.io/zone when known
  version: String,          // binary version, for rolling-upgrade decisions
  state: Joining | Active | Draining | Left,
  heartbeat_seq: u64,       // incremented on every heartbeat
  heartbeat_unix_ms: u64,   // wall clock, a hint for operators only
}
```

Heartbeat: every `heartbeat_interval` (default 1 s) the replica rewrites its row with `heartbeat_seq + 1` and reads the whole registry. Reads and writes go through the `LeaseStore` trait (section 12); the store may be the data store (heartbeat rows, works everywhere) or, in Phase 1, Kubernetes `coordination.k8s.io/v1` Leases via `kube-rs` when the API is reachable. Only membership can move to Kubernetes Leases: the shard rows of section 6 must live in the data store because they are read inside data transactions.

Failure detection is by **observed change on the observer's monotonic clock**, never by comparing wall clocks: each replica remembers, per peer, the last `heartbeat_seq` it saw and the local `Instant` at which it saw it change. A peer is **dead** when

1. no change has been observed for `lease_ttl` (default 3 s, must be at least 2 × `heartbeat_interval`), and
2. the observer's own last heartbeat write succeeded within the last `heartbeat_interval`.

Condition 2 is what makes a store stall not look like everyone else dying: a replica that cannot reach the store is not entitled to judge anyone. When the store recovers, every replica heartbeats first and only then re-evaluates its peers, so a short stall produces no churn. This is the same observation rule client-go's leader election uses (`observedTime` against the local clock), and the same Kubernetes Lease semantics 12 will map onto.

A replica in state `Left` is removed from the registry by itself on graceful exit; a dead replica's row is garbage-collected by any live replica after `10 × lease_ttl` (the row is harmless in between: it is dead for hashing purposes). A row whose `generation` is lower than another row for the same id is ignored.

**Self-suspicion.** An owner whose own heartbeats have failed for `lease_ttl` stops serving its shards (reads included) until a heartbeat succeeds and it has re-read its shard rows. This is not needed for write safety (section 6 covers that) but it is what keeps reads from memory linearizable: without it a partitioned owner could serve stale room state while the new owner accepts writes. The margin is the observer's confirmation delay (condition 2 plus the read latency), which exceeds zero on every observer; monotonic clock rate drift is the residual risk, as in every lease system.

Timings (defaults; `cluster.heartbeat_interval`, `cluster.lease_ttl`): interval 1 s, TTL 3 s. Worst-case detection is TTL + one interval = 4 s, the takeover write is milliseconds, and the cold load is per room and lazy, so the plan's "under 5 s to first successful write on the new owner" holds when 04's cold load of one room's hot state stays under about half a second. Rolling updates never pay this: section 10.

## 5. Rendezvous hashing over live replicas

`desired_owner(shard) = argmax over live replicas r of xxh3_64(shard bytes || r.id bytes)`. Live means `state ∈ {Joining, Active}` and not dead. `Draining` replicas are excluded, which is what moves shards off a pod that is shutting down. Ties are broken by replica id ordering (they are practically impossible with a 64-bit hash but the function must be total).

Properties relied upon: every replica with the same membership view computes the same assignment with no coordination; adding or removing one replica moves only the shards that map to it (1/N of the keyspace), which is minimal disruption; no ring metadata to store. Recomputing the full map is 256 × N hashes on each membership change and is cached; `owner_of` is a lookup.

Phase 1 adds weights (zone spread, capacity) by multiplying the hash into a weighted score (the "weighted rendezvous" transform), and a per-shard pin row `cluster/pin/<shard>` that overrides the hash for an operator- or load-driven move of a hot room's shard (the risk in the brief).

## 6. Shard rows and the fencing epoch

Every shard has a row `cluster/shard/<kind>/<index>`:

```
ShardRecord {
  epoch: u64,                          // fencing token; increases on every acquire
  owner: Option<(ReplicaId, Generation)>,  // None = released
}
```

**Ownership is changed only by a compare-and-swap on this row**, which every backend supports (a serializable transaction that reads the row and writes it; on Fjall and FoundationDB natively, on PostgreSQL under `SERIALIZABLE`, on SlateDB within the global shard's writer). Because it is a single-key CAS it is linearizable per shard, so two replicas can never both believe they own a shard at the same epoch. This is the "confirmed through the store" step of `PLAN.md` 7.3.

Acquire (by replica R, which must currently be the desired owner): read the row; require `owner == None` or `owner` dead by section 4's rule (this is R's judgement and not a store-side check, which is why the epoch exists); write `{ epoch: epoch + 1, owner: (R, R.generation) }`. Release (by the owner): read; require `owner == (R, gen)`; write `{ epoch, owner: None }`. Both fail with `Conflict` if the row changed, and the ownership loop re-reads and re-decides.

**Fencing rule (mandatory).** Every transaction that an owner runs against a shard's data reads that shard's row inside the transaction and aborts with `Fenced` if `epoch` is not the epoch the owner holds. On serializable backends this makes a stale owner unable to commit: its transaction has the epoch key in its read set, the new owner's acquire wrote that key, and the two cannot both commit in an order that lets the stale write land after the takeover. If they serialize with the stale write first, the write is durable before the new owner's acquire and the new owner's cold load sees it, which is correct. PostgreSQL SSI implements exactly this (the SIREAD lock on the epoch row detects the rw-conflict); Fjall's optimistic transactions validate the read set at commit; FoundationDB validates read conflict ranges. On SlateDB the rule composes with manifest fencing: opening the shard's database as the new writer fences the old process at the storage layer as well, and the epoch is what the new owner records in its manifest open.

Cost: one extra small-key read per transaction (a hot row in the buffer cache: tens of microseconds on PostgreSQL, a memtable hit on the embedded stores). The row is only *written* on ownership change, so in steady state the read-set entry never conflicts with anything; SIREAD locks on the same row from many transactions do not conflict with each other. The key is per shard, never global, so ownership changes of one shard never abort transactions on another. The read must be inside the transaction; a cached check outside it is not fencing.

The API is `Fence { shard, epoch }` (handed to actors when the shard is acquired) and `Fence::check(&mut txn)`, where the transaction type implements `EpochReader` (section 12); `hs-kv`'s transaction will implement it in `hs-cluster` once 01's trait lands (trait local to `hs-cluster`, so no cross-crate edit).

Reads from the owner's memory are protected by self-suspicion (section 4) rather than by the epoch. Reads from the store that do not touch hot state (profiles, media, keys) run on any replica and are not fenced; they are snapshot reads.

## 7. Ownership convergence

Each replica runs one ownership loop, triggered by membership change, by a mesh nudge, and on every heartbeat tick. For each shard it compares the desired owner with the row:

| Row | Desired == me | Action |
|---|---|---|
| owner == me | yes | nothing |
| owner == me | no | quiesce the actor, flush, release, nudge the desired owner |
| owner == None | yes | acquire; emit `Acquired { shard, fence }` |
| owner == other, alive | yes | wait; the other's loop releases (both compute the same function once views converge) |
| owner == other, dead | yes | acquire; emit `Acquired`; the old owner is fenced by the epoch bump |
| owner == other | no | nothing; cache the owner for forwarding |

Ownership is therefore sticky: the row is the truth, the hash is what both sides converge on. A momentary difference of membership views (one replica has not yet seen a new peer) cannot cause two acquires, only a delay. The loop also emits `Released { shard }` and, when a transaction reports `Fenced`, `Lost { shard, epoch }`, at which point the actor must drop its in-memory state.

Readiness (`/health/ready`, consumed by 12): the replica has joined the mesh, has completed a heartbeat, and its ownership loop has converged: every shard it is the desired owner of it owns, and it owns no shard it is not the desired owner of, evaluated over a membership view no older than one interval. During a rollout this becomes true on a new pod only after it has taken its share, which is what stops the rollout from moving faster than ownership.

The `ShardMap` (owner per shard) is kept on every replica from the row scan at each tick and from best-effort `ShardMapDelta` announcements over the mesh on every acquire and release, so forwarders learn of moves in milliseconds without waiting for the next scan. The `NotOwner` reply of section 8 carries the owner hint as a third source.

## 8. Forwarding

Forward, not redirect: clients sit behind one Service and would not be able to follow a redirect to a peer. Any replica may receive a request for any shard; if `is_mine(shard)` is false the edge forwards the request to `owner_of(shard)` over the mesh and relays the reply.

Envelope (HTTP/2 `POST /mesh/v1/forward`, metadata in headers, payload as the body so the owner never re-serialises it):

| Field | Meaning |
|---|---|
| `shard` | target shard |
| `route` | which handler on the owner (`room.send`, `user.sync`, ...); the cluster does not interpret it |
| `idempotency_key` | 128-bit random, generated once per client request at the edge, reused on every retry |
| `requester` | the authenticated requester context from 07's middleware (user, device, appservice assertion, admin flag), serialised as JSON; the owner trusts it because the mesh is authenticated (section 9) |
| `deadline` | remaining budget in milliseconds; decremented per hop; work whose deadline passed is dropped, not started |
| `origin`, `origin_generation`, `hops` | the sending replica and hop count; `hops > max_hops` (default 3) fails the request rather than looping |
| `traceparent` | W3C trace context, so a forwarded request is one trace |
| body | opaque payload |

Replies: `200` with the handler's payload and status; `421 Misdirected Request` with an `owner hint` when the receiving replica does not own the shard (it may be draining, or ownership moved); `503` with `Retry-After` when the owner is at capacity or shutting down; `401` on authentication failure.

Retry policy at the edge: on connection failure, on `421` (refresh ownership from the hint, then retry to the new owner), and on `503` (bounded backoff 10, 50, 200 ms). Never on application errors (`4xx`/`5xx` from the handler are relayed as they are). At most `max_attempts` (4) within the request deadline. Retries reuse the idempotency key.

Idempotency: the owner keeps a per-shard in-memory cache `idempotency_key → reply` (bounded, TTL 60 s) so a retry after a lost reply returns the original reply without re-executing. That cache does not survive failover, so **an actor whose effect is not naturally idempotent persists the key in the same transaction as the effect** (for example `room/<shard>/idem/<key> → event_id` next to the event); the new owner then answers the retry from the store. The toy actor in `hs-cluster` demonstrates this and the chaos harness checks it. The cluster provides the key and the cache; the actor owns the durable rule.

Forwarded writes and the in-memory idempotency cache are per shard so that a shard move takes its cache semantics with it (the new owner starts empty and relies on the durable rule).

## 9. Backpressure

- Per peer: one HTTP/2 connection (multiplexed), a bounded number of in-flight forwards (`cluster.mesh.max_in_flight_per_peer`, default 1024) enforced by a semaphore at the sender; when it is full the edge fails fast with `503` rather than queueing, so client-visible latency stays bounded and the caller (or the client) backs off.
- Per shard at the owner: the actor mailbox is bounded (owned by 04/05); when the dispatcher cannot enqueue within a short wait it replies `503` with `Retry-After`. The cluster never buffers requests on behalf of an actor.
- Deadlines propagate in the envelope and are enforced on both sides; expired work is not started.
- Bytes: HTTP/2 flow control, with the connection window sized for the mesh (`cluster.mesh.window_bytes`).
- Fan-out: a replica that has to contact many owners (an appservice or federation `/send` split by room) caps concurrency at `cluster.mesh.max_fan_out` (default 64) per request.

## 10. Graceful handoff and rolling updates

Sequence on SIGTERM (the library exposes it as `Cluster::drain(deadline)`; `hs-cli` wires the signal):

1. Set my registry state to `Draining` and heartbeat immediately. Every peer's next hash excludes me, so each of my shards now has a different desired owner.
2. Stop accepting new client connections (the HTTP listeners, owned by `hs-http`, are told to close their acceptors; in-flight requests continue). Forwarded requests for shards I still own continue to be served until each shard is released.
3. For each owned shard, in parallel up to `cluster.handoff.parallelism`: tell the actor to quiesce (finish in-flight work, stop taking new commands: new commands get `NotOwner` so the edge retries), flush, release the row, then nudge the desired owner over the mesh (`POST /mesh/v1/released`). The desired owner acquires on the nudge, and the edge that retried lands on it. Long-polls held by user sessions are answered with their current position so clients reconnect through the Service to another replica.
4. Wait until every shard shows a new owner in the row scan, or until `deadline − safety margin`. Shards still unclaimed at the deadline remain released (`owner == None`); a live peer acquires them on its next tick, so nothing is lost, only latency.
5. Set state `Left`, delete my registry row, stop the heartbeat, close the mesh listener, exit.

Budget: `terminationGracePeriodSeconds` is the deadline; the chart sets 30 s and the drain aims to finish in under 10 s for a replica with hundreds of shards, because a release is one CAS and a nudge. `preStop` is not needed for correctness but 12 may use it to delay SIGTERM until the endpoint is removed from the Service, which avoids a burst of forwards to a pod that is going away.

Rolling update with `maxSurge: 1, maxUnavailable: 0`: the new pod joins (`Joining` → `Active`), takes its rendezvous share from everyone (a fraction 1/(N+1) of shards, each by owner-initiated release as in section 7), reports ready, and only then does Kubernetes terminate an old pod, which drains as above. p99 stays flat because no lease ever expires; ownership moves are owner-initiated with a quiesce-and-flush, so the new owner's cold load is the only cost, and it is per room and lazy. Mixed versions for one release step are supported by keeping the registry, shard-row and envelope formats backward compatible for one step (versioned by the `version` field; a newer replica never sends an envelope field an older one cannot ignore).

## 11. Mesh authentication

The mesh is HTTP/2 over `hyper` 1.x. Authentication is pluggable behind `Authenticator` and two implementations ship:

- **Mutual TLS** (`rustls`, the production mode): the server requires a client certificate chained to the cluster CA (`cluster.mesh.tls.ca_file`) and presents its own (`cert_file`, `key_file`; cert-manager issues them per pod, with the headless Service name as SAN). The client verifies the server the same way. Peer identity is the certificate's SHA-256 fingerprint, and, when configured, the SAN must match `cluster.mesh.tls.peer_san_suffix`. Rotation: files are re-read on SIGHUP (13's reloadable-section mechanism) and new connections use the new material; existing connections are closed when the old certificate expires.
- **Shared secret** (tests, development, and single-process integration tests): `Authorization: Bearer <secret>` compared in constant time. Not for production over an untrusted network, and the config marks it so.

Authorization is coarse: any authenticated peer may call any mesh route. All replicas run the same binary under the same operator, so a compromised replica is out of the threat model; the mesh exists to keep everyone else out. The requester context in the envelope is trusted because the transport is authenticated; the client-facing edge is the only place that authenticates end users.

Discovery is the registry (`mesh_addr` per replica); 12's headless Service supplies DNS for the mTLS SANs, not the peer list.

## 12. Single-node mode

Selected at runtime by `hs serve --single-node` (or `cluster.mode: single`), not at compile time, so the same binary and image serve both cases. In single-node mode `Cluster::single_node()` returns an ownership manager that is inert: `owner_of` is always me, `is_mine` is always true, `Fence::check` is a no-op (there is no other writer; the embedded store is single-process), there is no registry, no heartbeat task and no mesh listener, and `forward` is never reached (an attempt returns `NotClustered`). Actors are written once against the same API. A cargo feature to compile the mesh dependencies (`hyper`, `rustls`) out of the ARM binary is a later size optimisation; it does not change behaviour.

## 13. Interfaces `hs-cluster` provides (frozen at week 6; as implemented -- see section 0)

```rust
// Identity (crates/hs-cluster/src/types.rs)
pub struct ReplicaId(String);           pub struct Generation(u64);
pub enum ShardKind { Room, User, Federation, Appservice, Global }
pub struct ShardId { kind: ShardKind, index: u32 }
pub struct ShardLayout { rooms: u32, users: u32, federation: u32, appservice: u32 }
impl ShardLayout { fn room_shard(&self, room_id: &str) -> ShardId; fn user_shard(&self, user_id: &str) -> ShardId; ... }

// Ownership (object-safe; the same trait for single-node and clustered) -- src/ownership.rs
pub trait Ownership: Send + Sync {
    fn me(&self) -> &ReplicaId;
    fn owner_of(&self, shard: ShardId) -> Option<ReplicaId>;   // row owner, else desired owner
    fn is_mine(&self, shard: ShardId) -> bool;                  // owned and lease fresh
    fn fence(&self, shard: ShardId) -> Option<Fence>;           // Some iff is_mine
    fn subscribe(&self) -> broadcast::Receiver<OwnershipEvent>; // Acquired, Released, Lost, MembershipChanged
    fn shard_map(&self) -> watch::Receiver<Arc<ShardMap>>;
}
// Lifecycle is a separate trait so actor code (which only ever needs the read side above) cannot
// accidentally drain the cluster it runs in:
#[async_trait] pub trait Drainable: Send + Sync {
    fn ready(&self) -> Readiness;
    async fn drain(&self, deadline: Duration) -> DrainReport;
}

// Fencing -- src/fence.rs. Built directly on `hs_kv::KvRead`, not a crate-local `EpochReader`
// trait: any backend's transaction or snapshot type already implements `KvRead`, so this one
// function works unchanged against every backend (section 0).
pub struct Fence { shard: ShardId, epoch: Option<Epoch> }       // None in single-node mode
impl Fence {
    pub fn check<K, T: hs_kv::KvRead<Keyspace = K>>(&self, txn: &T, keyspace: &K) -> Result<(), FenceError>;
}

// Store -- src/store.rs. Not a trait: one generic struct over `hs_kv::KvBackend`, so 01 never has
// to implement anything for this crate to work (section 0 explains why the originally planned
// `LeaseStore` trait was dropped).
pub struct ClusterStore<B: hs_kv::KvBackend> { /* ... */ }
impl<B: hs_kv::KvBackend> ClusterStore<B> {
    pub fn open(backend: B) -> Result<Self, ClusterError>;
    pub fn shard_keyspace(&self) -> &B::Keyspace; // for actors calling `Fence::check` in their own txns
    pub fn init_layout(&self, wanted: ShardLayout) -> Result<ShardLayout, ClusterError>;
    pub fn heartbeat(&self, rec: &ReplicaRecord) -> Result<(), ClusterError>;
    pub fn list_replicas(&self) -> Result<Vec<ReplicaRecord>, ClusterError>;
    pub fn remove_replica(&self, id: &ReplicaId, generation: Generation) -> Result<(), ClusterError>;
    pub fn get_shard(&self, shard: ShardId) -> Result<ShardRecord, ClusterError>;
    pub fn list_shards(&self) -> Result<Vec<(ShardId, ShardRecord)>, ClusterError>;
    pub fn acquire_shard(&self, shard: ShardId, me: &ReplicaId, gen: Generation, owner_is_dead: impl Fn(&ReplicaId) -> bool) -> Result<Option<ShardRecord>, ClusterError>;
    pub fn release_shard(&self, shard: ShardId, me: &ReplicaId, gen: Generation) -> Result<(), ClusterError>;
}

// Mesh -- src/mesh/
pub struct Envelope { shard, route, idempotency_key, requester, deadline, origin, origin_generation, hops, traceparent, payload }
pub struct Reply { status: u16, payload: Bytes }
#[async_trait] pub trait ShardHandler: Send + Sync { async fn handle(&self, env: Envelope, fence: Fence) -> Reply; }
pub trait Authenticator: Send + Sync { fn authenticate(&self, headers: &HeaderMap, tls: Option<&TlsPeerInfo>) -> Result<PeerIdentity, AuthError>; }
pub struct SharedSecretAuthenticator { /* constant-time Bearer comparison */ }
pub struct MutualTlsAuthenticator { /* SAN-suffix check; chain-to-CA is verified by rustls beneath it */ }
impl Forwarder { pub async fn forward(&self, env: Envelope) -> Result<Reply, ForwardError>; }
impl MeshServer { pub fn new(listen_addr, tls: Option<&TlsMaterial>) -> Result<Self, TlsError>; pub async fn serve(self, deps: Arc<MeshDeps>, shutdown: watch::Receiver<bool>) -> io::Result<()>; }

// Lifecycle facade (hs-cli wires SIGTERM → drain) -- src/cluster.rs
impl Cluster {
    pub fn single_node(me: ReplicaId) -> Self;
    pub async fn start<B: KvBackend>(cfg: ClusterConfig, backend: B) -> Result<(Self, Arc<KvOwnership<B>>), ClusterError>;
    pub fn ownership(&self) -> &Arc<dyn Ownership>;
    pub fn ready(&self) -> Readiness;
    pub async fn drain(&self, deadline: Duration) -> DrainReport;
}
```

## 14. Chaos and verification

In-process harness (`crates/hs-cluster/tests/chaos.rs`): N replicas in one process, each a real `KvOwnership<MemoryBackend>` sharing one `hs_kv::memory::MemoryBackend`, with fault injection (per-replica "stop heartbeating" to simulate death, and the tokio paused clock for deterministic timings), and a toy replicated-log actor (`ChaosLog`) whose every commit records `(shard, epoch, writer, seq)` through a real `hs_kv` transaction that calls `Fence::check` before committing, exactly as a production actor must. Checks: for each shard the sequence of committed writes has non-decreasing epochs and each epoch has exactly one writer (no two replicas ever write the same shard -- the epoch makes a stale owner's commit conflict and abort, per section 6); every append acknowledged to a client is present exactly once and every append not acknowledged is present at most once (no lost or duplicated writes under retries with idempotency keys, persisted in the same transaction as the effect per section 8); after a replica death the shard has a new owner within `lease_ttl + heartbeat_interval` plus one tick; on `drain` every shard is released before the replica stops and the drained replica writes nothing afterwards.

Kubernetes harness (`deploy/chaos/`, `kind`): the same toy actor as an `hs chaos-actor` subcommand, pod kills, partitions by NetworkPolicy, slow store via a `toxiproxy` sidecar in front of PostgreSQL, rolling updates, and the recorded-write checker. Written now, untested until Docker is available; runs in CI with 12's `kind` skeleton.

## 15. Metrics

`hs_cluster_ownership_changes_total{kind, reason=acquire|release|lost}`, `hs_cluster_owned_shards{kind}`, `hs_cluster_forward_latency_seconds{route, outcome}`, `hs_cluster_forward_retries_total{reason}`, `hs_cluster_lease_age_seconds` (time since my last successful heartbeat), `hs_cluster_peer_lease_age_seconds{peer}`, `hs_cluster_live_replicas`, `hs_cluster_fenced_total{kind}`. Exposed through `hs-telemetry`'s registry when 12 publishes it; until then `hs-cluster` keeps them in a `ClusterMetrics` snapshot that any exporter can read.

## 16. Open questions settled here

| Question | Decision |
|---|---|
| Shard count | 256 rooms, 256 users by default, fixed at creation; 1024 recommended above 16 replicas (section 3). |
| Forward or redirect | Forward (section 8). |
| mTLS or shared secret | Both behind a trait; mTLS in production, shared secret for tests and development (section 11). |
| Cost of the epoch read | One hot-key read per transaction, no steady-state conflicts; mandatory (section 6). |
| Single-node selection | Runtime flag with an inert manager; compile-out is a later size optimisation (section 12). |
| Failure detection clock | Observer-local monotonic time plus own-heartbeat gating; no wall-clock comparison (section 4). |
| Who initiates a move | The owner releases, the desired owner acquires; rows are truth, the hash is the target (section 7). |

## 17. What other tracks need to do

- **01 Storage**: nothing further needed for Fjall/PostgreSQL -- `hs-cluster` already builds on `hs_kv::KvBackend` directly and needs no crate-specific hook (section 0). The one open item is the SlateDB backend, when it lands: per-shard open must take the epoch `ClusterStore::acquire_shard` returns so it composes with SlateDB's own manifest fencing, per section 6's last paragraph.
- **04, 05, 06, 11**: actors receive `Fence` on `OwnershipEvent::Acquired`, call `fence.check(&txn, cluster_store.shard_keyspace())` as the last read before every commit against the shard's data, drop state on `Lost`, quiesce on release, and persist idempotency keys for non-idempotent effects in the same transaction as the effect.
- **07**: define `RequesterContext` as a serialisable type; until then the envelope carries it as JSON.
- **12**: readiness maps to `Cluster::ready()`; `terminationGracePeriodSeconds` ≥ `cluster.handoff.deadline` + margin; cert-manager issues per-pod certificates with the headless Service SAN; the `kind` chaos job runs `deploy/chaos/`.
- **13**: the `cluster` config section (`mode`, `shards`, `heartbeat_interval`, `lease_ttl`, `mesh.listen`, `mesh.advertise`, `mesh.auth`, `mesh.tls.*`, `handoff.*`).
- **14**: the chaos suite and the per-shard linearizability checker consume the recorded-write log format from section 14.
