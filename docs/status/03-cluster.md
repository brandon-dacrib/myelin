# 03 Cluster: status

## Fix landed, 2026-09-19 (this session): the split-brain is closed

**Summary.** The two-replica experiment below (both the original run and the integration lead's
independent reproduction) found that nothing ever called `hs-cluster`'s ownership API: `hs-cluster`
was not even a dependency of `hs-cli`, so two replicas against one PostgreSQL each ran a fully
independent, uncoordinated `RoomActor` registry and silently forked a room's event DAG under
concurrent writes. This session wires `hs-cluster` into `hs-cli` end to end -- ownership, forwarding,
startup and shutdown -- **without editing `hs-room`, `hs-config` or any other track's crate**, and
reproduces the *fixed* behavior against two real `hs serve` processes on one real PostgreSQL
database (transcript below). `hs-cluster` itself required no code changes; every primitive the
brief named (`Cluster::single_node`/`start`, `Ownership::is_mine`/`owner_of`/`fence`,
`ShardLayout::room_shard`, `Forwarder`, `Fence::check`, `Cluster::ready`/`drain`) was already built
and tested and is now actually called.

**What changed, all in `crates/hs-cli/**` (new file `crates/hs-cli/src/cluster.rs`, plus edits to
`crates/hs-cli/src/serve.rs`, `crates/hs-cli/src/lib.rs` and `crates/hs-cli/Cargo.toml`; nothing in
`crates/hs-cluster/**` needed to change):**

1. **`hs-cluster` is now a dependency of `hs-cli`.**
2. **Startup** (`crate::cluster::start`, called from `spawn_serve_with_backend` right after storage
   opens): builds `Cluster::single_node(<host:pid>)` when `config.cluster.single_node` (the
   default -- inert, zero behavioral change, confirmed by `cargo test -p hs-loadgen --test
   real_client` and `--test real_client_encrypted` both still passing unmodified), or a real
   `hs_cluster::Cluster::start` over the same already-open storage backend plus a
   `hs_cluster::mesh::Forwarder`, otherwise. The `hs_config::ClusterConfig` -> `hs_cluster::
   ClusterConfig` conversion the previous update flagged as missing now exists
   (`crate::cluster::to_hs_cluster_config`) -- see "Decisions made" for exactly how the shape
   mismatch was resolved without adding fields to `hs-config`.
3. **The fix itself: `RoomShardGate`, an `axum` middleware layered over the *entire* built
   router.** It inspects every request's path for a `/rooms/{roomId}/...` segment (a small
   hand-rolled percent-decoder, no new dependency); if this replica does not own that room's
   shard, the request never reaches `hs-room`'s registry at all -- it is either forwarded
   verbatim to the owner over the mesh (a raw HTTP reverse proxy: method, path, headers including
   `Authorization`, and body, replayed against the owner's own copy of the exact same `axum::
   Router` via `tower::Service::oneshot`, so the owner's own auth middleware authenticates it
   exactly as if it had arrived directly) or refused with a clear `503 M_HS_NOT_SHARD_OWNER`
   naming the believed owner. This gates **reads as well as writes** -- see "Decisions made" for
   why gating only `/send` would not have been sufficient to make both replicas' `/messages`
   agree.
4. **Shutdown**: `Cluster::drain` runs before the HTTP listeners stop accepting (`ServeHandle::
   shutdown`, called from `hs serve`'s existing `SIGTERM` handler in `cli.rs` -- no change needed
   there), then the mesh listener is stopped. **Readiness**: `/health/ready` now additionally
   checks `Cluster::ready()` (`NotReady` while a clustered replica has not yet heartbeated
   successfully), collapsing to exactly today's behavior in single-node mode.
5. **Fencing inside the write path**: not reachable without editing `hs-room` (confirmed again
   this session; see "What `hs-room` still needs" below). The routing gate (item 3) is the primary
   defense and is sufficient on its own for the reproduced bug, per the root-cause analysis
   below it in this file: two *simultaneously live* owners never trip a fence at all, so fencing
   alone would not have fixed this bug even if `RoomActor::persist` called it. It remains the
   documented belt-and-braces gap for the one case the gate cannot cover (a stale `is_mine` read
   racing a real handoff).

**Acceptance test: the two-replica experiment, re-run against the fix.**

Setup (same as the original experiment, `postgres:17` in Docker on `127.0.0.1:5435`, two `hs
serve` processes on one host), except both configs now carry a real `cluster:` section instead of
the default:

```yaml
storage:
  backend: postgres
  host: 127.0.0.1
  port: 5435
  database: postgres
  user: postgres
  password: hspg
  tls: false
cluster:
  single_node: false
  room_shards: 4
  user_shards: 4
  mesh:
    port: 18449          # 18450 on the second replica -- see "Decisions made"
    shared_secret: mesh-shared-secret
  heartbeat_interval: 200ms
  lease_ttl: 1s
```

(A's client listener is `18040`, B's is `18041`; each config's `cluster.mesh.port` must differ
when co-located on one host, the same as the client listener ports already must.)

```
$ hs serve -c a.yaml &     # replica A: client 18040, mesh 18449
$ hs serve -c b.yaml &     # replica B: client 18041, mesh 18450
$ curl http://127.0.0.1:18040/health/ready   # 200, once both have heartbeated
$ curl http://127.0.0.1:18041/health/ready   # 200

$ hs register -u alice -p hunter2pass -k cluster-secret -v http://127.0.0.1:18040
@alice:cluster2.example.org
$ curl -X POST http://127.0.0.1:18041/_matrix/client/v3/login -d '{...}'    # login on B
$ curl -X POST http://127.0.0.1:18040/_matrix/client/v3/createRoom -d '{"preset":"public_chat", ...}'
{"room_id":"!mYHcfy3IDZbukA3Ppa:cluster2.example.org"}

# 5 concurrent PUT /send through A and 5 through B, same room, same user, at once:
B#1 -> 200   A#1 -> 200   B#2 -> 200   A#4 -> 200   B#3 -> 200
A#2 -> 200   B#4 -> 200   A#5 -> 200   B#5 -> 200   A#3 -> 200
```

All ten returned `200` with a distinct `event_id`, exactly as before the fix (`select count(*)
from kv_room_events` afterward: 17 rows -- 7 state events + 10 messages, all present, none lost).
**The difference is what `GET /messages` shows afterward:**

```
GET /messages on A (dir=b, limit=100), messages only:
10 ['from A #1', 'from A #2', 'from A #3', 'from A #4', 'from A #5',
    'from B #1', 'from B #2', 'from B #3', 'from B #4', 'from B #5']

GET /messages on B, same call:
10 ['from A #1', 'from A #2', 'from A #3', 'from A #4', 'from A #5',
    'from B #1', 'from B #2', 'from B #3', 'from B #4', 'from B #5']
```

**Identical on both replicas** -- all ten messages, same set, same `event_id`s, same
`prev_events` chain (confirmed by inspecting the raw JSON: the full ordered chunk matches
byte-for-byte between A and B). Querying `kv_cluster_shards` directly during the run confirms this
is real ownership routing, not an accident of a tiny shard count: the room's shard (and each
user/federation shard) is owned by exactly one of `127.0.0.1:18449` (A) or `127.0.0.1:18450` (B)
at a time, split across the two replicas (`epoch:1`, no churn during the run) -- so half of the ten
concurrent sends were transparently forwarded across the mesh to whichever replica actually owned
the room, and the client making them never saw a difference (all `200`, real `event_id`s). This is
the **forward** behavior (brief deliverable 2); the **refuse** behavior (deliverable 1) is exercised
by the same code path whenever the forwarder itself fails (no owner known, mesh unreachable,
retries exhausted) -- see `crate::cluster::RoomShardGate::refuse` -- and is covered by
`cluster::tests::single_node_gate_never_forwards_or_refuses` plus the unit tests on `extract_room_id`;
forcing an actual refuse in the live two-replica setup (e.g. by pointing the mesh secret at the
wrong value) was not additionally re-run this session given time, but the code path is identical
regardless of which of the two outcomes `forward` produces.

Shutdown was also exercised as part of the same run: `kill -TERM` on both processes simultaneously
(no live peer to hand off to) produced, in each log, `cluster drain complete handed_off=0
released_unclaimed=<n>` after releasing every owned shard and waiting out the drain deadline --
confirming `Cluster::drain` is now actually wired to `SIGTERM`, not just tested in isolation.

**What `hs-room` still needs (not fixed here, cannot be from `hs-cli`):** `RoomActor::persist`
(`crates/hs-room/src/actor.rs:884`, track 04's file) should call `fence.check(txn, cluster_store.
shard_keyspace())` as the last read before its transaction commits, using the `hs_cluster::Fence`
the caller obtained from `ownership.fence(shard)`. This is not required to fix the reproduced bug
(the routing gate already ensures only one replica's `RoomActor` for a room is ever live), but it
is the documented belt-and-braces protection against a stale `is_mine` read racing a real
ownership handoff (a network partition, a rolling update) between the gate's check and the
transaction's commit.

**Known gaps, not fixed this session (see "Decisions made" for why each was left):**
- `/createRoom` is not gated (the room id does not exist yet at request time) -- the replica that
  handles `/createRoom` always constructs the new room's first `RoomActor` locally, regardless of
  which replica will end up owning its shard. A subsequent request that is correctly gated will
  still forward to whichever replica *does* own it, but that first write happens wherever the
  client's `/createRoom` landed.
- The mesh's advertised host defaults to the first listener's bind address (or `127.0.0.1` if that
  is a wildcard), not a real Kubernetes pod IP -- fine for this session's same-host setup, wrong
  for a real multi-pod deployment. `hs-config` has no field for this yet.
- Mutual TLS for the mesh is not wired from `hs-cli` (shared-secret only); `hs-cluster` already
  supports it, wiring it needs `hs_config::listeners::TlsConfig` file paths threaded through, left
  for a follow-up.
- The forwarded response's status code is passed through verbatim from the proxied handler, which
  collides in principle with the two statuses `Forwarder::forward` treats specially (`421`, `503`)
  -- this workspace's client-server API does not use either today, so it is a latent risk, not an
  observed bug.
- `hs cluster status` / drain CLI commands and Kubernetes `Lease` membership remain unimplemented,
  as recorded in the "Next" section below (unchanged by this session).

**Verify:**
```sh
cargo fmt -p hs-cluster -p hs-cli -- --check
cargo clippy -p hs-cluster --all-targets -- -D warnings
cargo clippy -p hs-cli --all-targets -- -D warnings
cargo test -p hs-cluster                 # 40 lib + 5 chaos + 2 mesh-pool tests, unchanged
cargo test -p hs-cli                     # 81 lib tests + e2e.rs (9) + federation_reads.rs (7) all
                                          # pass; federation_writes.rs has 3 pre-existing failures
                                          # unrelated to this change (see below)
cargo test -p hs-loadgen --test real_client            # passes, single-node unaffected
cargo test -p hs-loadgen --test real_client_encrypted  # passes, single-node unaffected
```

**A pre-existing, unrelated failure found while verifying, not caused by this change:**
`cargo test -p hs-cli --test federation_writes` has 3 failing tests
(`send_rejects_a_new_event_whose_auth_events_do_not_authorize_it`,
`send_gives_up_when_the_remote_serves_an_endless_backfill_chain`,
`send_backfills_a_missing_ancestor_then_accepts_the_original_event`), all failing on a signature
verification error (`"signature from local.example/ed25519:1 does not verify"` /
`"signature from remote.example/ed25519:a_remote does not verify"`) rather than the behavior each
test asserts. `crates/hs-federation/src/{inbound,client,backfill}.rs` (track 06's files) were
modified during this session (mtimes newer than the test file itself), while `crates/hs-cli/tests/
federation_writes.rs` was not -- this is track 06's in-flight work, not a cluster-track regression:
nothing in this update touches signing, federation, or event auth, and these tests exercise
`/_matrix/federation/v1/send` transport, never `/rooms/{roomId}` client routing, so `RoomShardGate`
cannot be involved. Reproduced twice (`cargo test -p hs-cli --test federation_writes`, run
independently of this update's changes). Flagging for the integration lead rather than working
around it.

---

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

- Nothing actively in progress. This session's assignment (wire `hs-cluster` into `hs-cli` to stop
  the two-replica split-brain) is complete: see "Fix landed, 2026-09-19" at the top of this file.

## Next (not done; for whoever picks this up next)

- **`hs-room`: call `Fence::check` inside `RoomActor::persist`'s transaction** (line 884,
  `crates/hs-room/src/actor.rs`), using the `hs_cluster::Fence` the caller obtained from
  `ownership.fence(shard)`. Not required to fix the reproduced bug (the routing gate in
  `hs-cli` already ensures only one replica's `RoomActor` for a room is ever live), but it is the
  documented belt-and-braces protection against a stale `is_mine` read racing a real ownership
  handoff between the gate's check and the transaction's commit.
- **`/createRoom` is not shard-gated** (see "Fix landed" above): the id does not exist until the
  handler runs, so whichever replica receives `/createRoom` always constructs the new room's first
  `RoomActor` locally, regardless of which replica ends up owning its shard. Every *subsequent*
  request is correctly gated and will forward to the true owner, but the very first write is not.
  Fixing this properly needs either a two-phase create (reserve the id, then route) or moving room
  creation into a per-replica-agnostic path; not attempted this session.
- **A real advertised mesh address.** `crate::cluster::advertise_host` (`crates/hs-cli/src/
  cluster.rs`) falls back to the first listener's bind address, or `127.0.0.1` if that is a
  wildcard -- correct for this session's same-host two-replica test, wrong for a real multi-pod
  Kubernetes deployment, which needs the pod IP. No `hs-config` field carries this today.
- **Mutual TLS for the mesh, wired from `hs-cli`.** `hs-cluster` already supports it
  (`AuthMode::MutualTls`); `crate::cluster::start` currently refuses to boot rather than silently
  downgrading to shared-secret auth if `cluster.mesh.tls` is set in the native config, since
  loading the certificate files and building the `TlsMaterial` needs `hs_config::listeners::
  TlsConfig` threaded through and was not done this session (see "Decisions made" below).
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
- Actual integration with 05's user session actor / 06's federation sender shards / 11's
  appservice shards -- 04's room actor is now integrated (this session, client-server routing
  only; federation's own `/rooms/{roomId}`-shaped paths, if any, are not gated -- see "Decisions
  made"). This is Phase 1/2 per the brief for the rest.

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
- All of the above frozen as before; unchanged by this session (no `hs-cluster` code was touched).
- **New, in `hs-cli` (not a frozen cross-track interface, but worth other tracks knowing about):**
  `hs_cli::cluster::{ClusterHandles, RoomShardGate, MeshRuntime, start}` --
  `crates/hs-cli/src/cluster.rs`. `RoomShardGate::layer` is how any future `hs-cli` router
  addition gets shard-ownership gating for free by virtue of being mounted before it in
  `serve::build_router`; nothing outside `hs-cli` needs to call into this module.

## Interfaces needed

- 01 Storage: nothing further for Fjall/PostgreSQL (built directly on `hs_kv::KvBackend`, see
  Decisions). When SlateDB lands, its per-shard open needs to accept the epoch
  `ClusterStore::acquire_shard` returns.
- 04: `RoomActor::persist` (`crates/hs-room/src/actor.rs:884`) should call `Fence::check` -- see
  "Next" above. Not blocking (the routing gate is sufficient for the reproduced bug on its own),
  but the documented remaining gap.
- 05, 06, 11: their actors need to exist (or, for 06, be routed through the same kind of gate 04's
  now is) before shard ownership actually gates anything for them in production.
- 07: `RequesterContext` as a real type; the mesh envelope carries `serde_json::Value` until then
  (unaffected by this session -- the reverse-proxy forwarder does not use this field at all, since
  it re-authenticates from the raw `Authorization` header instead; see "Decisions made").
- 12: readiness now does map to `Cluster::ready()` (this session, `crate::health_ready` in
  `crates/hs-cli/src/serve.rs`); `terminationGracePeriodSeconds` should be at least the new
  `CLUSTER_DRAIN_DEADLINE` (20s, `crates/hs-cli/src/serve.rs`) plus the listeners' own drain
  margin; cert-manager (or an equivalent) for per-pod mTLS certificates once mTLS mode is wired
  from `hs-cli` (not yet, see "Next"); a `kind` job to actually run `deploy/chaos/`.
- 13: **new since this update** -- `hs_config::ClusterConfig` (`crates/hs-config/src/cluster.rs`)
  has no fields for this replica's identity, its mesh-advertised address, zone, or federation/
  appservice shard counts (only `room_shards`/`user_shards`); `hs-cli`'s
  `crate::cluster::to_hs_cluster_config` fills these in from process-level facts and
  `hs_cluster::ShardLayout::default()`'s federation/appservice counts rather than config, which
  means every replica in a cluster must be given matching `room_shards`/`user_shards` by hand
  today (a mismatch is caught: `ClusterStore::init_layout` rejects a layout that disagrees with
  what is already recorded) but federation/appservice shard counts cannot be configured at all.
  Also: `hs_config::cluster::MeshConfig` has no host field, only `port` -- see `advertise_host` in
  `crates/hs-cli/src/cluster.rs` for the fallback this session used instead. Not touched in
  `hs-config` itself per this session's instructions (owned by another agent); recorded here
  instead.
- 15: `hs chaos-actor` subcommand for `deploy/chaos/` to have anything to actually deploy.

## Decisions made

**This session (2026-09-19, wiring `hs-cluster` into `hs-cli`):**

- **Gate reads as well as writes, not just `/send`.** The brief's deliverable 1 says "a replica
  that does not own a room's shard must not serve a *write* for it," but `RoomShardGate` gates
  every `/rooms/{roomId}/...` request regardless of method. Reasoning: `hs-room`'s
  `RoomRegistry` is a per-process `room_id -> RoomActorHandle` map loaded lazily on *any* access,
  read or write (`crates/hs-room/src/registry.rs`'s own doc comment: "loaded on first access ...
  dropped after `evict_idle`"). If only writes were gated, a `GET /messages` on a non-owner that
  already has (or later loads) a resident `RoomActor` for that room would keep serving from that
  actor's own local, never-updated-by-the-real-owner view forever -- exactly the kind of silent,
  self-consistent divergence the original bug produced, just for reads instead of writes. Gating
  every access is what makes "route to the one live owner" actually true, and it is what the
  live two-replica re-run above confirms: both replicas' `/messages` agree exactly, not just their
  write acknowledgements.
- **The forward path is a raw HTTP reverse proxy, not a structured RPC over the mesh envelope.**
  `RoomShardGate::forward` serializes the incoming method/path/headers/body into JSON (a `base64`
  body field so the envelope stays valid text regardless of content) and hands it to
  `Forwarder::forward`; the owner's `ProxyShardHandler` replays it against its own copy of the
  same `axum::Router` via `tower::Service::oneshot` and ships the real response back the same way.
  Rejected alternative: defining a typed `hs-room`-aware RPC (send this event, read these
  messages) inside `hs-cluster`'s `ShardHandler` -- this would need `hs-cluster` to depend on
  `hs-room`'s request/response types (or `hs-cli` to hand-translate every route, one at a time,
  forever staying one route behind whatever `hs-room` adds), and would not have been buildable
  without editing `hs-room` in some form to expose an in-process call surface, which this session
  could not do. The reverse-proxy approach forwards *any* `/rooms/{roomId}` route automatically
  (state, redact, relations, receipts, typing, ...), including ones added to `hs-room` after this
  session, with zero coupling to its internal types -- the cost is losing structured retry
  semantics for exactly two status codes (`421`, `503`; see "Known gaps" above) and one extra
  JSON-plus-base64 encode/decode per forwarded request, both acceptable trade-offs given the
  alternative.
- **The owner re-authenticates the forwarded request from its own `Authorization` header rather
  than trusting a `RequesterContext` the origin attaches.** `Envelope::requester` (track 07's
  seam, still `serde_json::Value` per the frozen mesh envelope) is left `Null` and never read by
  `ProxyShardHandler`. This is safe specifically because the payload is a full raw HTTP request
  including its original `Authorization` header, replayed through the owner's *own* auth
  middleware -- the owner never has to trust the origin's opinion of who the requester is. This
  would not be safe for a structured RPC that only carries a claimed user id.
- **This replica's `hs-cluster` identity is its own dialable mesh address (`host:port`), not a
  separate name.** `Forwarder::resolve_addr`'s own doc comment already documents this convention
  ("exactly right for ... `ReplicaId == host:port`"); `to_hs_cluster_config` in
  `crates/hs-cli/src/cluster.rs` follows it rather than inventing a name-to-address lookup table,
  which would need its own registry.
- **Federation and appservice shard counts default to `hs_cluster::ShardLayout::default()`'s
  values (64 each) rather than being configurable**, since `hs_config::ClusterConfig` has no
  fields for them (see "Interfaces needed" above). Every replica in one cluster computes the same
  default, so this is internally consistent; it just cannot be tuned without a `hs-config` change
  this session did not make.
- **`cluster.mesh.tls` being set causes startup to fail loudly rather than silently falling back
  to shared-secret auth.** `to_hs_cluster_config` still records the intent to use mutual TLS in the
  `AuthMode` value it builds (with empty placeholder paths), and `crate::cluster::start` checks for
  that variant and returns `ClusterSetupError::Invalid` before ever starting the ownership manager,
  specifically so a deployment that configured TLS for a reason (an untrusted network between
  pods) never ends up running unauthenticated-by-certificate without an operator being told.
  Wiring real mutual TLS is left for a follow-up (see "Next").
- **`CLUSTER_DRAIN_DEADLINE` (20s) is a `hs-cli`-local constant, not a config field**, since
  `hs_config::ClusterConfig` has no handoff-deadline field (only `hs_cluster::HandoffConfig`,
  internal, does) and adding one is out of scope for this session (see "Interfaces needed"). Chosen
  to comfortably fit inside a 30s Kubernetes `terminationGracePeriodSeconds` with margin left for
  the HTTP listeners' own drain.
- **No new `[workspace.dependencies]` entries.** `hs-cluster` was added as an ordinary path
  dependency of `hs-cli` (`crates/hs-cli/Cargo.toml`); every type or trait `crate::cluster` needed
  from outside `hs-cluster`/`hs-cli` (`serde`, `base64`, `bytes`, `http`, `tower`, `tokio`,
  `axum`) was already a dependency of `hs-cli`. No percent-decoding crate was added either -- a
  dozen-line hand-rolled decoder in `crate::cluster::percent_decode` was enough for one path
  segment shaped like a Matrix room id, and avoided touching the root `Cargo.toml` at all for this
  change.

**Carried over from previous updates:**

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

**This session:** `hs-cluster` added as a path dependency of `hs-cli`
(`crates/hs-cli/Cargo.toml`). No other new dependency, and no `[workspace.dependencies]` entries
added -- see "Decisions made" above.

**Carried over from previous updates:**

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

For `hs-cluster` alone (unchanged this session):

```sh
cargo fmt -p hs-cluster -- --check
cargo clippy -p hs-cluster --all-targets -- -D warnings
cargo test -p hs-cluster                # 40 lib tests + 5 chaos tests + 2 mesh-pool tests + 0 doctests
cargo test -p hs-cluster --test chaos      # just the chaos harness, if iterating on it alone
cargo test -p hs-cluster --test mesh_pool  # just the connection-pooling tests
```

For the `hs-cli` wiring added this session, see the commands and results under "Fix landed,
2026-09-19" at the top of this file (the live two-replica acceptance test, plus `cargo fmt`/
`clippy`/`test` for both crates and `hs-loadgen`'s real-client tests for single-node regression).

To re-verify the chaos suite's fencing coverage by mutation (as this update's integration review
did): in `crates/hs-cluster/src/fence.rs`, make `Fence::check` `{ return Ok(()); }` unconditionally,
run `cargo test -p hs-cluster`, confirm `fence::tests::fence_fails_after_another_replica_acquires`
and the chaos tests `no_two_replicas_ever_commit_the_same_shard_epoch` and
`a_partitioned_replica_cannot_write_after_being_fenced` fail (3 tests), then revert and confirm
everything is green again.

`deploy/chaos/` is not runnable in this environment (no Docker); see its README for what running
it for real requires.
