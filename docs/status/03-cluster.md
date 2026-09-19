# 03 Cluster: status

> **Integration note, 2026-09-19 (integration lead): reproduced independently, with one
> correction.** The silent DAG fork in answer 3 is real and reproduces exactly: two replicas on
> one PostgreSQL, five concurrent sends through each, every request `200` — then `/messages` on
> replica A returns only A's five messages and on replica B only B's five, permanently, with no
> error ever surfaced to any client. Room *state* created on A is visible on B immediately (B
> served the room's `m.room.name` correctly), which makes the fork harder to notice, not easier:
> a client sees a working room that silently drops half its traffic.
>
> **The correction is to answer 1.** Two replicas do *not* always start cleanly. Started
> simultaneously against an empty database, one of them dies at boot:
>
> ```
> NOTICE: schema "public" already exists, skipping
> hs serve: storage backend error: backend error: db error
> ```
>
> Started staggered — A first, then B once the schema exists — B starts fine, which is presumably
> how the experiment below was run. So there is a **cold-start schema race**: concurrent
> `CREATE`-style setup against a fresh database is not idempotent under concurrency, and the loser
> exits with an error that says nothing useful ("db error" — no SQLSTATE, no statement, no table
> name). Two separate defects to fix: the race itself (create the schema in a single transaction
> that tolerates a concurrent creator, or take an advisory lock around setup), and the diagnostics,
> since an operator rolling out two pods at once would see only "db error" and have nothing to act
> on. Neither is a `hs-cluster` defect — both live in `hs-kv`'s postgres backend.


Updated: 2026-09-19 (two-replica experiment: `hs serve` run twice against one real PostgreSQL
database for the first time in this project's life. Result: both processes start cleanly and
share user/auth/room-state data perfectly, but concurrent writes to the same room silently fork
the room's event DAG into two permanently divergent branches, one per replica, with zero errors
returned to any client. No `hs-cluster` code defect was found or fixed; the crate's own machinery
was never invoked, because nothing calls it. Full transcript and root-cause below.)

## Two-replica experiment (2026-09-19)

**Goal.** Find out what actually happens when two `hs serve` processes run against one PostgreSQL
database, rather than reasoning about it. This is the first time any second real process has
existed in this project; `hs-cluster`'s ownership/lease/fencing/mesh machinery has so far only run
inside its own in-process chaos harness against simulated peers.

**Setup, exact commands:**

```sh
docker run --rm -d --name hs-cluster-pg -e POSTGRES_PASSWORD=hspg -p 5435:5432 postgres:17
# waited for: docker exec hs-cluster-pg pg_isready -U postgres   (ready after 4s)
cargo build -p hs-cli --bin hs

cd /tmp/hs-cluster-exp
hs generate-config --server-name cluster.example.org -o a.yaml
# edited: storage.backend: postgres (host 127.0.0.1, port 5435, database postgres, user postgres,
#   password hspg, tls false); listeners[0].bind_addresses: ['127.0.0.1'], port: 18030;
#   auth.enable_registration: true, auth.registration_shared_secret: cluster-secret;
#   server.signing_key_path: ./signing-keys-dir (a directory, not a file -- see finding 0 below)
cp a.yaml b.yaml   # only the listener port differs: 18031
hs generate-signing-key -o signing-keys-dir/cluster.example.org.signing.key   # shared by both

hs serve -c a.yaml > a.log 2>&1 &
hs serve -c b.yaml > b.log 2>&1 &
```

**Finding 0 (config gotcha, not a cluster bug, worth recording anyway).** `server.signing_key_path`
must be a *directory* (Synapse's layout, one key file scanned non-recursively); pointing it at a
plain file the way `hs generate-signing-key -o <file>` naturally suggests silently falls back to
"no ed25519 signing key found; generating an ephemeral one for this process only" -- each replica
would then sign events with a *different* key, undetected, unless you read the log. Not a
multi-replica-specific bug (a single misconfigured replica has the exact same problem), but it is
the kind of thing that is easy to get wrong precisely when standing up a second replica for the
first time, since a fresh single-node deployment never previously had reason to persist and share
a signing key across processes. Fixed in the experiment by pointing both configs at the same
directory.

### 1. Do both processes even start against one database?

**Yes, cleanly. No lock, no schema race, no keyspace collision.** Both replicas ran the exact same
migration/`CREATE TABLE IF NOT EXISTS`-style startup path concurrently; each of the ~55 `NOTICE:
relation "..." already exists, skipping` lines in `b.log` is the second replica finding the first
replica's schema already there and treating it as a no-op, not an error:

```
[...] INFO postgres::config: NOTICE: schema "public" already exists, skipping
[...] INFO postgres::config: NOTICE: relation "kv_hs_auth.users" already exists, skipping
[... 53 more identical NOTICE lines ...]
[...] INFO hs_cli::serve: no .well-known documents are published (...)
[...] INFO hs_cli::cli: listening addr=127.0.0.1:18030
```

Both processes reached `listening` within milliseconds of each other and stayed up. There is
**no cluster-awareness whatsoever** at this point: `hs_config::ClusterConfig` is parsed (the
generated config has a `cluster:` section, `single_node: true` by default) but `grep cluster
crates/hs-cli/src/serve.rs` returns nothing -- `hs-cluster` is not a dependency of `hs-cli` at all
today. Each replica is a fully independent, un-coordinated single-node server that happens to
point at the same Postgres database. That is the actual starting condition for everything below.

### 2. Register on A, login on B, create a room on A -- does B see it?

**Yes, immediately, in every case tried.**

```
$ hs register -u alice -p hunter2pass -k cluster-secret -v http://127.0.0.1:18030
@alice:cluster.example.org
access_token: syt_YWxpY2U_QoJpzztdgKyTXOvgnGFP_1O4ixA

$ curl -X POST http://127.0.0.1:18031/_matrix/client/v3/login -d '{"type":"m.login.password",
  "identifier":{"type":"m.id.user","user":"alice"},"password":"hunter2pass"}'
{"user_id":"@alice:cluster.example.org","access_token":"syt_...","device_id":"4TXjCPgIQv"}
```

Login on B succeeded on the first try with no delay. `createRoom` on A
(`!2F9ujP5FINRZKb6WWR:cluster.example.org`), then a fresh `/sync` on B, showed the room fully
joined with complete state (`m.room.create`, membership, power levels, join rules, history
visibility) on the very next request. This is expected, not surprising: auth/user data and room
state reads are plain point reads/writes against the shared Postgres tables with no per-replica
cache in front of them, so both replicas trivially see the same committed rows. This part of "two
replicas, one database" works with **zero cluster-awareness code**, which is exactly why the next
question is the one that matters.

### 3. Send messages to the same room through both replicas at once

**This is where it breaks, and it breaks silently: the room's event DAG forks into two permanently
divergent branches, one per replica, and every single write reports success.**

Fired 20 concurrent `PUT /rooms/{room}/send/m.room.message/{txn}` requests through A and 20 through
B at the same time, same room, same user (`alice`, logged in separately on each replica):

```
for i in $(seq 1 20); do
  curl -X PUT ".../18030/_matrix/client/v3/rooms/$ENC/send/m.room.message/txnA-$i" ... &
  curl -X PUT ".../18031/_matrix/client/v3/rooms/$ENC/send/m.room.message/txnB-$i" ... &
done; wait
```

**All 40 requests returned `200` with a distinct, well-formed `event_id`. Zero errors, zero
retries visible to the client.** `select count(*) from kv_room_events` afterward: 47 rows (7 room
state events + 40 messages) -- every event really was written to Postgres, none lost or
overwritten at the row level.

But `GET /messages?dir=b&limit=100` on A returns exactly the 7 state events plus **A's own 20
messages** (`from A #1`..`#20`) -- none of B's. The identical call on B returns the 7 state events
plus **B's own 20 messages** -- none of A's:

```
A's /messages: 27 events, 20 messages, bodies = {from A #1 .. from A #20}
B's /messages: 27 events, 20 messages, bodies = {from B #1 .. from B #20}
```

A fresh `/sync` on each replica confirms this is not a `/messages`-pagination artifact: A's
timeline tail is entirely `from A #*` events, B's is entirely `from B #*` events. **Two clients
talking to the same room through different replicas now see two different, non-overlapping
message histories, permanently, with no error ever surfaced.** This is worse than a lost write or
a visible conflict -- it is a silent, self-consistent split-brain per replica.

**Root cause, found by reading the code the experiment pointed at (`crates/hs-room/src/actor.rs`,
read-only -- this file belongs to track 04, not edited):**

- `RoomActor::send_event` (line 570) computes `prev_events` from `self.forward_extremities_vec()`
  -- **the actor's own in-memory field**, not a fresh read of a shared "current extremities" row.
- `RoomActor::persist` (line 884) opens a `transact(&self.backend, ..., |txn| { ... })` block that
  deletes `old_extremities` (again, read from `self.forward_extremities`, in memory) and inserts
  the new event as the sole forward extremity. **The transaction never reads any shared row to
  validate that `self.forward_extremities` is still current** -- there is nothing in this
  transaction a concurrent writer's commit could conflict with, because the write set never
  includes anything the other replica's write set also touches in a way `hs-kv`'s SSI would catch
  (each replica deletes *its own* believed-old extremity and inserts *its own* new one; A deleting
  `create-event` and inserting `A#1` does not conflict with B deleting `create-event` and inserting
  `B#1` under snapshot isolation, since after both commit the row for `create-event` is deleted by
  both harmlessly and two different new rows exist -- exactly the fork observed).
- `self.forward_extremities` and `self.events` (used for `is_first`) are populated once when the
  `RoomActor` is constructed and mutated only by that same process's own successful persists. A
  `RoomActor` living inside replica A's process has **no mechanism at all** to learn that replica
  B's in-process `RoomActor` for the same room just moved the extremity out from under it. Every
  subsequent event A sends cites A's last-known (increasingly stale, from the room's true
  perspective) extremity, and vice versa for B -- hence two clean, internally-consistent, mutually
  invisible chains.

This is precisely the failure mode the brief's fencing design exists to prevent ("Two processes
both believing they own a room is precisely what the lease and fencing machinery exists to
prevent"), but fencing alone would not have been sufficient here even if `RoomActor::persist`
called `Fence::check`: fencing stops a *stale* owner from committing after ownership has moved, by
aborting its transaction. It does not, by itself, stop *two current, un-coordinated* actors from
each successfully writing non-conflicting deltas that are individually valid but jointly wrong.
The actual fix has to be architectural, and it is the one `hs-cluster` was built for: **only the
shard owner may ever construct a `RoomActor` for a room it owns; every other replica must forward
the request over the mesh to the owner instead of handling it locally.** With that in place, fencing
is still required as the belt-and-braces check inside the owner's own transaction (a network
partition can make a replica believe it is still the owner after it no longer is), but the primary
defense is "there is only ever one live `RoomActor` for a given room, host on the shard owner." See
"Wiring the integration lead must add" below for exactly what that requires.

### 4. Kill A mid-write -- does B carry on? Manual recovery needed?

**B carries on immediately and completely, no manual recovery of any kind.**

```
# fired 30 sequential writes through A in the background, killed -9 partway through
$ kill -9 <A's pid>
{"event_id":"..."}   # 4 succeeded before the kill
{"event_id":"..."}
{"event_id":"..."}
{"event_id":"..."}
curl: (52) Empty reply from server   # the in-flight request when A died

$ curl http://127.0.0.1:18030/_matrix/client/versions
curl: (7) Failed to connect to 127.0.0.1 port 18030: Couldn't connect to server

$ curl -X PUT http://127.0.0.1:18031/.../send/m.room.message/afterkill-1 -d '...'
{"event_id":"$XVqtc2LK2YfAxZioLFT6voJi1WzoIctsE8DNyOgxZK4"}   # B, unaffected
```

Checked PostgreSQL immediately after the kill for anything A might have left dangling
(`select pid, state, query from pg_stat_activity`, `select count(*) from pg_locks`): every
remaining backend was `idle` (not `idle in transaction`), the terminal `query` on the ones that had
run one was `COMMIT` or `ROLLBACK`, and there were no abandoned locks. `r2d2`'s pooled connections
on A's side were simply closed by the OS when the process died, and PostgreSQL's own crash-safety
(a `SIGKILL`led client's uncommitted work is never durable) meant there was nothing to clean up.
**This part requires no cluster machinery at all** -- it is a property of every client of a
transactional database, not something `hs-cluster` earns credit for or needs to fix. The one
caveat: this says nothing about whether *B* now has to do anything about whatever shard(s) A used
to "own" (informally, since ownership is not wired in) -- with real ownership wired in, this is
exactly the case the lease TTL and failover chaos test
(`failover_completes_within_configured_ttl`) already covers, just never exercised against a real
second process until now.

### Summary of the four answers

| # | Question | Answer |
|---|---|---|
| 1 | Both processes start against one DB? | Yes, cleanly. No lock/schema-race/keyspace collision. Zero cluster-awareness is invoked either way -- `hs-cluster` is not wired into `hs-cli` at all. |
| 2 | Register on A, login/see room on B? | Yes, immediately, for both auth data and room state reads. Plain shared-Postgres point reads/writes need no cluster code. |
| 3 | Concurrent messages to the same room via both replicas? | **Silent, permanent fork of the room's event DAG.** Every write reports success; no error, no duplicate, no lost row at the storage level -- but each replica's in-memory `RoomActor` diverges from the other's immediately and never reconciles. Two clients see two different message histories in the same room, forever. |
| 4 | Kill A mid-write -- does B carry on? | Yes, instantly, no manual recovery. PostgreSQL's own transactional guarantees handle this without any cluster involvement. |

**No `hs-cluster` code was changed as a result of this experiment.** The crate's own machinery
(`Ownership`, `Fence`, `ClusterStore`, the mesh) was not exercised at all, because nothing calls it
today -- `hs-cli`, `hs-room` and `hs-user` all still behave exactly as a single-node server would,
regardless of what `cluster.single_node` is set to in config. The chaos suite's own claim ("no two
replicas ever write the same shard") remains true *of the toy actor the chaos suite itself uses*,
which does call `Fence::check` on every append; it says nothing about `hs-room`'s real actor, which
calls it zero times. This is not a regression in this session -- it is the expected, previously
undemonstrated state of integration, now demonstrated for the first time with two real processes
instead of reasoned about.

## Wiring the integration lead must add

None of this is inside `crates/hs-cluster`; all of it is `hs-cli` (`serve.rs`, `storage.rs`) plus
one hook into `hs-room`'s (track 04's) room actor. Concretely, in order:

1. **At `hs serve` startup, after storage opens and before the listener binds:** construct a
   `hs_cluster::Cluster`. If `config.cluster.single_node` is `true`, call
   `Cluster::single_node(ReplicaId::new(<derive an id, e.g. hostname:pid or a configured value>))`
   -- this is inert and changes nothing (matches today's behavior exactly, so this step alone is
   safe to land first with no functional change). If `false`, build an
   `hs_cluster::config::ClusterConfig` from `hs_config::ClusterConfig`
   (`crates/hs-config/src/cluster.rs`) and call
   `hs_cluster::Cluster::start(cluster_config, backend.clone()).await`, where `backend` is the same
   `hs_kv::KvBackend` `storage.rs` already opened. **Note the config shapes do not line up
   one-to-one today** and there is no existing conversion function in either crate: `hs_config::
   ClusterConfig` has `room_shards`/`user_shards` but no `federation`/`appservice` shard counts
   (`hs_cluster::types::ShardLayout` needs all four), and has no replica identity, mesh advertise
   address, zone, or handoff settings (all process-level facts, not YAML). Deliberately not built
   in this pass on the `hs-cluster` side: it would mean guessing at `hs_config::MeshConfig`'s TLS
   field shape (`crates/hs-config/src/listeners.rs::TlsConfig`) without being able to compile
   against it from this crate (adding `hs-config` as a dependency of `hs-cluster` is possible per
   this track's ownership rules, but building and testing that conversion needs a compile against
   the real, currently-in-flux `hs-config` types a wiring PR can verify directly). Whoever writes
   this should feel free to add `hs-config` as a path dependency of `hs-cluster` and put the
   conversion in `hs_cluster::config` if that ends up the more natural home than `hs-cli`.
2. **Hold the `Cluster` (or at least `Arc<dyn hs_cluster::Ownership>`) somewhere every request
   handler can reach it** (probably the same shared app state `storage.rs`'s backend already lives
   in).
3. **Before a request handler constructs or looks up a `RoomActor` for a room** (the call sites
   above, `crates/hs-room/src/actor.rs`, are the ones that matter, but the gate belongs at the
   `hs-cli` routing layer, not inside `hs-room`): compute
   `shard = cluster_config.layout.room_shard(&room_id)`, then check
   `ownership.is_mine(shard)`. If false: **do not construct or touch a local `RoomActor` for that
   room at all** -- forward the request over `hs_cluster::mesh::Forwarder` to
   `ownership.owner_of(shard)` instead (the mesh server/forwarder/envelope/idempotency-key
   machinery for this already exists and is tested; only the call site is missing). This is the
   change that actually prevents the fork in answer 3 -- it is what makes "only one replica ever
   holds a `RoomActor` for a given room" true.
4. **Inside `RoomActor::persist`'s existing `transact(...)` closure** (track 04's file,
   `crates/hs-room/src/actor.rs:884`, cited here only so the hook is easy to find, not to prescribe
   04's internals): call `fence.check(txn, cluster_store.shard_keyspace())` as the *last* read
   before the closure returns `Ok`, where `fence` is the `hs_cluster::Fence` the caller obtained
   from `ownership.fence(shard)` at the top of the request (per the interface already documented
   under "Interfaces provided" below). This is the belt-and-braces check for the case step 3's
   routing gate raced with a real ownership handoff (a network partition, a rolling update) between
   the moment `is_mine` was checked and the moment the transaction commits. Fencing alone (without
   step 3) does **not** fix answer 3's fork, since two simultaneously-current owners never trip a
   fence at all -- both steps are required together, not either in isolation.
5. **Readiness and shutdown**: point track 12's `/health/ready` at `Cluster::ready()` and call
   `Cluster::drain(deadline)` on `SIGTERM` before the listener stops accepting, per RFC 0001 section
   10 and this crate's existing `Drainable` trait -- not exercised by this experiment (no rolling
   update was attempted), but it is the same shape of gap: the trait exists and is tested in
   isolation, nothing in `hs-cli` calls it yet.

None of steps 1-5 require a new `hs-cluster` capability; every method named above
(`Cluster::single_node`, `Cluster::start`, `Ownership::is_mine`, `Ownership::owner_of`,
`Ownership::fence`, `ShardLayout::room_shard`, `Forwarder`, `Fence::check`, `Cluster::ready`,
`Cluster::drain`) is implemented, tested and frozen today (see "Interfaces provided" below). The
gap this experiment found is entirely that nothing in `hs-cli` or `hs-room` calls any of them yet.

## Integration review follow-up (2026-09-18)

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
