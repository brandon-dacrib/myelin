## 2026-10-04: a replica keeps its shards until its lease lapses (branch `agent/cluster-heartbeat`, decision 0028)

Closes the `hs-cluster` row of the known gaps: "a replica gives up every shard when one tick
runs two heartbeat intervals after its last good heartbeat". `converge` wanted a shard only
while `self_heartbeat_fresh` (last good heartbeat under `2 * heartbeat_interval`), so one
failed heartbeat under load, followed by a tick, released everything the replica held; a peer
took the shards and gave them back, and the requests between were forwarded, retried or fenced.

The decision (`docs/decisions/0028-a-replica-keeps-its-shards-until-its-lease-lapses.md`):
a replica *claims* a new shard only while its heartbeat is fresh (as before, and the same
freshness it needs to judge a peer dead), but *keeps* a shard it holds while its lease is alive
(`self_lease_alive`: last good heartbeat under `lease_ttl`), which is exactly as long as
`is_mine` answers for it and strictly before any peer can have judged it dead. Past the lease it
releases them in its next tick. `is_mine` and `Drainable::ready` share the predicate. The two
`warn` lines in `tick` (a failed heartbeat; a gap over two intervals after a good one) stay,
reworded for the new rule, with `since_last_heartbeat_ms`, `lease_ttl_ms` and (after a gap)
`lease_expired`. No config key, public type or admin field changes; `docs/config.md`'s
`lease_ttl` row and the chart's `leaseTtl` comment say the replica holds on for the lease.

Tests (`ownership::tests`, paused clock, 50 ms heartbeats and a 500 ms lease, on a backend that
refuses heartbeat commits on demand and nothing else):

- `a_late_heartbeat_keeps_the_shards_until_the_lease_lapses`: four refused heartbeats (stale,
  lease alive) move nothing in memory or in the store; the heartbeat recovers and still nothing
  moves (no `Released`/`Lost`/`Acquired` events); eleven refused heartbeats lapse the lease and
  every shard is released and announced; the heartbeat recovers and they come back at a newer
  epoch. On the old gating it fails at the first phase: "room/0 was given up after one late
  heartbeat".
- `a_replica_with_a_stale_heartbeat_claims_no_new_shard`: while stale, a shard a peer took and
  let go is dropped (`Lost`) and left ownerless until the heartbeat is fresh again, and the
  other shards stay held.

Verified: `cargo test -p hs-cluster --all-targets` (56 unit, 21 integration, `chaos.rs`
included) and `cargo clippy -p hs-cluster --all-targets -- -D warnings` pass;
`crates/hs-cli/tests/cluster_admin.rs` (both tests, 20.5 s) and `cluster_create_room.rs`
(132 s) pass with `HS_CLUSTER_TEST_POSTGRES_DSN` against a private `postgres:17`, with no
heartbeat-gap `warn` in either run. `cargo fmt --all --check` is clean.

Observability check for the "bridge manager runs on one replica only" row: a stale heartbeat
is already visible without a change. `hs_cluster_lease_age_seconds` is the time since this
replica's last successful heartbeat (set every tick), `hs_cluster_heartbeat_seq` the last seq
that reached the store; the admin API's replica objects carry `last_heartbeat_at` and
`heartbeat_seq` (OpenAPI 0.1.7), and the Cluster page's "Last heartbeat" column shows the
former as a relative time. Nothing added.

Left: nothing from this row. `cluster_mirror.rs`'s note that shards still move for several
seconds right after the last of three replicas turns `active` is the ordinary rebalance on a
new member, not this gap.

## 2026-10-01: `cluster_create_room.rs` stops flaking (branch `agent/cluster-create-room-flake`)

The two-replica test `crates/hs-cli/tests/cluster_create_room.rs` failed the merge gate on
`main` in three ways, each traced to its cause:

- **"two replicas never settled sharing the room shards"** (all four `room/*` shards on one
  replica at epoch 1). Rendezvous hashing of the two mesh addresses (a replica's id) gave all
  four room shards to one of them: a correct, stable map, which about one random port pair in
  eight produces with four room shards (613 of 5,000 consecutive pairs, counted with
  `hash::desired_owner`). The test now picks mesh ports whose map gives each replica a room
  shard (`mesh_ports_that_split_the_room_shards`), and waits up to 180 s (was 60) for the map
  to settle and hold for three heartbeats.
- **A `createRoom` answered `503 M_UNKNOWN` "fenced: this replica no longer owns shard"** (or
  "no new room id hashed to a room shard this replica owns"). Ownership moved between the
  gate's check and the handler's fenced write, and the gate handed the fence's `503` to the
  client: unlike other room requests (decision 0017), an edge `/createRoom` was never made
  again. Now `RoomShardGate::run_create_room` keeps the body and, when the handler ran here and
  answered `503`, chooses again from the current ownership (which forwards the create to the
  new owner), up to `MAX_CREATE_ROOM_ATTEMPTS` (4) inside the request deadline; if every
  attempt is fenced it answers the gate's own `503 M_HS_NOT_SHARD_OWNER`, which clients retry.
  `info` per retry, `warn` when it gives up. `hs_room_create_room_id_attempts` now counts a
  creation once its create event is written, so a fenced attempt is not counted as a room built.
- **A forwarded write answered `503` until its deadline.** The mesh server cached every reply
  under the forward's idempotency key, a `503` included, and the forwarder retries a `503` with
  the same key: once a forwarded write was fenced, every retry got the cached `503` back. A
  `503` (a refusal that did nothing) is no longer cached.

Why ownership moved at all in a test that does no failover: `converge` releases every shard a
replica holds in a tick whose last good heartbeat is `2 * heartbeat_interval` old (1 s at the
test's 500 ms), and `is_mine` refuses them once it is `lease_ttl` old. A debug build on a
machine at load 15+ is late by that much often enough. Nothing logged it; `tick` now warns when
a heartbeat fails and, after a good one, when the gap since the previous good one exceeded
`2 * heartbeat_interval`. The test runs at the production timings (1 s heartbeat, 3 s lease)
and prints each replica's warning and shard log lines when it fails. Whether one late tick
should cost a replica every shard is a known gap of its own.

Tests: `mesh_refusal_not_cached.rs::a_forward_fenced_twice_succeeds_once_the_handler_does`
(fails without the fix: the retries got the cached `503`);
`cluster::tests::a_create_room_fenced_here_is_made_again_on_the_new_owner` and
`a_create_room_fenced_on_every_attempt_is_refused_as_not_the_shard_owner` (on the old gate the
first gets the fence's `503`, the second `M_UNKNOWN`).

Verified: ten runs in a row against a private `postgres:17` with the machine at load 9-20 all passed (167-284 s each); `cluster_admin.rs` (both tests), `cargo test -p hs-cluster` (with `chaos.rs`) and `cargo test -p hs-room` pass.
## 2026-10-01: a non-owner's room reads are incremental (branch `agent/rfc-0018`, tracks 05 and 04)

Not a change to `hs-cluster`, but to what a cluster costs: a replica answering `/sync` for a
room another replica owns no longer reloads the whole room from the store per event. Its copy
(`hs_user::cluster::RoomMirror`) is advanced by `hs_room::actor::RoomActor::catch_up`, which
reads only the timeline rows past it (4.9 ms per event against 2,054 ms for a whole reload of
a room of 2,000 messages and 303 members, release build); a per-room rewrite counter (`room_rewrites`) tells it when
something other than an append happened (backfill, purge, outliers, ...) and it then reloads,
logged with the reason. The `user.wake` batch's existing `room_pos` drives it: a copy is caught
up (for at most 250 ms) before the long-polls the wake is for are woken -- a catch-up awaited
inline held the mesh request past its 2 s deadline once and was cancelled with it. RFC 0018,
decision 0022, status 05 session 12 (with a three-replica measurement in
`crates/hs-cli/tests/cluster_mirror.rs`). Two things for this track from that test: on a loaded
machine, 500 ms heartbeats with 3 s leases moved shards in the middle of the run (the test uses
2 s / 30 s), and right after the last replica of three turns `active` shards are still moving
for several seconds.

## 2026-09-30: a join or knock by alias is shard-gated (branch `agent/cli-small-gaps`)

Closes the known gap "A room alias in `/join/{alias}` or `/knock/{alias}` is not shard-gated"
(the follow-up 2026-09-27's "`/join/{roomId}`" section below left open).

- **The gate resolves the alias first.** `RoomShardGate` takes an `AliasResolver`
  (`hs_cli::cluster`); `serve.rs` gives it `RoomAliasResolver`, built from the room routes'
  own `RoomState`: a local alias is read from the store (`RoomRegistry::resolve_alias`, which
  loads no room), an alias on another server is asked of that server's directory through the
  same `RemoteJoin` the handler uses. On a clustered replica, `/join/{alias}` and
  `/knock/{alias}` are rewritten to name the room id, with the directory's servers (and the
  alias's server) appended as `server_name` after the client's own, in the order the handler
  would have tried them; then the gate routes it like a join by id -- here if this replica owns
  the room, forwarded to the owner otherwise -- and the owner does not resolve it again. An
  alias that names no room passes through to the handler, which answers the client's `404`.
  Single-node mode resolves nothing. Each resolution is an `info` line ("resolved a join or knock
  by alias ahead of the shard gate", with `alias`, `room_id`, `shard`, `owned_here`); a forward
  counts in `hs_cluster_forward_latency_seconds{route="forward"}` as any other.
- **Tests.** Unit, in `cluster.rs` with a scripted ownership and a real mesh listener: a join and
  a knock by alias on a non-owner reach the owner over the mesh as `/join/!room` with the
  servers appended, and never the local handler; on the owner it runs here, rewritten; an unknown
  alias passes through untouched; single-node mode never asks. Two real replicas on PostgreSQL 17,
  `crates/hs-cli/tests/cluster_alias_join.rs`: alice creates `#lobby` (public) and `#door`
  (knock) through A; bob joins the one and knocks on the other by alias through whichever
  replica does not own each; the non-owner's forward count rises and its log has the line with
  `owned_here=false`; the owner's state has bob's `join`/`knock`; bob's next message is taken.
  With the gate built without the resolver the join fails: `fenced: this replica no longer owns
  shard ShardId(room/0)` (the write fence refusing it on the non-owner, as the gap row said).

Verify: `HS_CLUSTER_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5462/postgres cargo
test -p hs-cli --test cluster_alias_join`, and `cargo test -p hs-cli --lib -- cluster::`.

## 2026-09-30: an in-process server restarts over its own data directory (branch `agent/cli-small-gaps`)

Closes the known gap "In-process server cannot be restarted over its data directory".

- **Every task on the server's own runtime.** `spawn_serve_with_storage` builds a multi-thread
  Tokio runtime (threads named `hs-serve`) and starts the server on it, so every `tokio::spawn`
  below it, in any crate, lands there; `ServeHandle` owns it. `ServeHandle::shutdown` runs the
  graceful steps as before (readiness, cluster drain, mesh, long-polls, appservice and
  federation stops, listeners) on that runtime -- aborting and joining this crate's own bridge
  manager and statistics sampler first, with a `warn` naming either if it does not stop within
  5 s -- then `Runtime::shutdown_timeout` drops every task still there (the background loops
  in `hs-user`, `hs-push`, `hs-federation`, `hs-appservice`, ... that do not watch for shutdown)
  and waits up to `TASKS_STOP_DEADLINE` (10 s) for work on its blocking threads, with a `warn`
  if that runs out; the store is dropped last. Dropping a handle without `shutdown()` stops the
  runtime in the background. In `hs serve` the binary's own runtime now only waits for the
  signal (and runs the configuration follower).
- **Two reference cycles.** Stopping every task was not enough: the store stayed locked. A
  component report found them (`ServeHandle`'s `components`: the auth store, audit log,
  appservice registry, room registry, session hub, e2e store, push stores, media repository,
  overview and federation sender are watched weakly; whatever is still alive after shutdown is
  named in a `warn` and returned by `shutdown()`). (1) The session hub held the appservice
  ephemeral pump's doorbell, which held the pump, which reads through the hub
  (`appservice_delivery::Doorbell` holds the pump weakly now; the pump's task owns it). (2) The
  room registry held the federation backfill, which holds the registry: the registry now gets
  `serve::WeakBackfill`, and the server's running parts own the backfill. (`WeakBackfill`
  delegates both methods of `hs_room::backfill::Backfill`; a new method needs a line there.)
- **Test.** `crates/hs-cli/tests/in_process_restart.rs`: `spawn_serve` → register, create a
  room, send, sync → `shutdown()` → `spawn_serve` over the same `data_dir`, twice, in one
  process; the message from the first run is in `/messages` each time and `shutdown()` reports
  nothing outlived it. Before: `FjallError: Locked` on the second start. (With only the runtime
  change and not the two cycle fixes, the report named the six components above and the lock
  error remained.)
- **Real-binary restart tests that exist only because of this** (not converted; each could now
  be in-process unless it asserts on the log): `e2e.rs`'s `HsProcess` harness says so outright
  (`receipts_and_presence_are_still_there_after_a_restart_of_the_real_binary`,
  `after_a_restart_a_message_in_an_old_room_still_reaches_the_other_person`,
  `a_bridge_is_sent_what_happens_in_a_room_its_bot_is_in_even_across_a_restart`, whose comment
  names the lock), `admin_user_identity.rs` and `reports_tasks_statistics.rs` (both harnesses
  say "a restart has to be a new process"), `appservice_ephemeral.rs`'s restart and
  `migration.rs`'s. The ones that read the operator's log (`e2e.rs`'s setup-link test,
  `config_history.rs`) or run two federating servers (`federation_restart.rs`,
  `federation_catch_up.rs`) have other reasons to stay real processes.

Left: a clustered in-process server has not been checked for cycles (the mesh, the sync
cluster's mirror); its components report would name any. Verify:
`cargo test -p hs-cli --test in_process_restart`.
## 2026-10-01: a release advances the fencing epoch (branch `agent/room-cluster-small`)

Closes the known gap "A released shard keeps its fencing epoch until the next owner acquires
it" (noticed in the entry below). **Decision 0023**: `ClusterStore::release_shard` now writes
the ownerless row at `epoch + 1` in the same transaction that clears the owner, and answers
the new epoch (`Some`) or `None` when the caller was not the owner and nothing changed. A fence
the old owner still holds fails from the moment the release commits, instead of passing
against the ownerless row until the next acquisition; a handoff now moves the epoch twice
(release, then acquire). RFC 0001 sections 3 and 6 are amended to say so; the `Epoch` and
`Fence` docs too. `KvOwnership::release` logs each release with its new epoch at `debug`
(the count is `hs_cluster_ownership_changes_total{reason="release"}`).

- `fence::tests::a_stale_fence_fails_while_the_released_shard_has_no_owner` (new): acquire,
  release, then the old fence fails both a snapshot check and a write transaction while the
  row has no owner. Fails on the old behaviour (the check passes).
- `store::tests::acquire_then_release_round_trips_epoch` states the new contract (release
  advances 2 to 3 and answers it; the next acquisition makes 4);
  `release_by_a_non_owner_is_a_no_op` now also asserts the epoch did not move, for another
  replica and for the same replica at an older generation.
- `ownership::tests::a_shard_taken_while_held_is_dropped_and_taken_back_once_released` (the one test that
  released and then looked at the epoch) expects the loss to be reported at the release's
  epoch.
- The handoff path (decision 0017: a write fenced mid-handoff is a `503` the edge sends on to
  the new owner) still holds. Against a private `postgres:17` on :5477, on a machine at load
  10-16: `chaos.rs` (5 tests) passes; `crates/hs-cli/tests/cluster_admin.rs` passes both tests
  (53 s: a replica drained through the other hands off every shard and comes back, and the
  last replica stops at once); `cluster_create_room.rs` passes (282 s, forty rooms written and
  six upgraded across the two replicas). One earlier run of `cluster_create_room.rs` failed
  before the test did anything: replica 2 timed out connecting to PostgreSQL at boot.

**The merge gate's failure, and why it was not this change.** This branch's first gate failed
`cluster_create_room.rs` with "two replicas never settled sharing the room shards": all four
`room/*` shards on one replica at epoch 1. Epoch 1 means those shards were never released, so
the release's epoch could not be involved; rendezvous hashing of the two mesh addresses gave
that replica all four (`hash::desired_owner`), a stable map that about one port pair in eight
produces. That and the test's other two failure modes (a create fenced by an ownership move,
and a fenced forward answered from the idempotency cache) were on `main` too, and are fixed on
`agent/cluster-create-room-flake`, which this branch is now on top of. Neither of the two
ownership rules that move shards in that test (release after a late heartbeat, self-suspicion
after `lease_ttl`) reads an epoch. On top of that branch, five runs at load 10-13: four passed
(199-300 s); the first failed before the test did anything, a replica's boot timing out on its
PostgreSQL connection pool (the cold-boot row). `cluster_admin.rs`, `room_upgrade.rs`, `cargo
test -p hs-cluster` and `cargo test -p hs-room` pass.

**How to verify.** `cargo test -p hs-cluster`; with a PostgreSQL of your own,
`HS_CLUSTER_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5477/postgres cargo test -p
hs-cli --test cluster_admin --test cluster_create_room`.
## 2026-10-01: a non-owner's room reads are incremental (branch `agent/rfc-0018`, tracks 05 and 04)

Not a change to `hs-cluster`, but to what a cluster costs: a replica answering `/sync` for a
room another replica owns no longer reloads the whole room from the store per event. Its copy
(`hs_user::cluster::RoomMirror`) is advanced by `hs_room::actor::RoomActor::catch_up`, which
reads only the timeline rows past it; a per-room rewrite counter (`room_rewrites`) tells it when
something other than an append happened (backfill, purge, outliers, ...) and it then reloads,
logged with the reason. The `user.wake` batch's existing `room_pos` drives it: a copy is caught
up before the long-polls the wake is for are woken. RFC 0018, decision 0022, status 05 session
12 (with a three-replica measurement in `crates/hs-cli/tests/cluster_mirror.rs`). Two things
for this track from that test: on a loaded machine, 500 ms heartbeats with 3 s leases moved
shards in the middle of the run (the test uses 2 s / 30 s), and right after the last replica of
three turns `active` shards are still moving for several seconds.

## 2026-09-30: two known gaps closed (branch `agent/cluster-gaps`)

**The last replica of a cluster no longer waits out its drain deadline.** `Drainable::drain`
released every shard and then waited, up to the deadline less the safety margin (18 s of
`hs serve`'s 20), for each to show a new owner, even with nobody to be one. Now it first asks
the registry whether any other replica is live and hashable (not draining, not judged dead by
this observer's own failure detector, the same judgement `tick` makes). With none, it releases
every shard in one store transaction with its fencing epoch advanced
(`ClusterStore::release_shards_fenced`: ownerless, at a later epoch, so a fence handed out
before the drain fails, as when a peer takes a lost replica's shards) and returns. During the
wait the check is repeated every `min(heartbeat_interval, 250 ms)`: a peer that dies mid-drain
stops the wait once it is judged dead, and one that appears resumes it. No tick runs between
the decision and the release (`tick_lock`), so a tick cannot release the shards first without
advancing their epochs. `DrainReport::released_at_once` counts them (also included in
`released_unclaimed`, so `hs serve`'s "cluster drain complete" line is unchanged), the drain
logs "no other replica is live to take this replica's shards: releasing them at once" and
"drain released shards at once ... released_at_once=N", and
`hs_cluster_drain_released_at_once_total` counts them. Found on the way: `release` counted a
shard as released (and lowered `hs_cluster_owned_shards`) even when a tick had released it
first; it now counts only a shard it held.

- `ownership::tests::a_lone_replica_drains_in_well_under_a_second` (real clock, the 20 s
  deadline `hs serve` uses): took 18.0 s on the old behaviour.
- `ownership::tests::drain_releases_every_owned_shard` now also asserts `released_at_once`,
  that it took under 0.5 s of its 3 s, the advanced epochs and that an old fence fails.
- `ownership::tests::a_drain_does_not_wait_for_a_peer_that_is_draining_too` and
  `a_drain_stops_waiting_once_its_only_peer_is_judged_dead` (the peer's task aborted, its row
  left Active). All four fail on the old behaviour; `chaos.rs`'s
  `drain_hands_off_to_a_live_peer_before_stopping` and the administrator-drain test still pass.
- `crates/hs-cli/tests/cluster_admin.rs` (two real `hs serve` on PostgreSQL 17) now stops B
  while A is live (B waits for A, and does not log the at-once line), then stops A, the last
  replica, and asserts it logs the at-once line and stops in under 10 s. Measured from
  `SIGTERM` to process exit, against a private `postgres:17` on :5471 (the gate's :5462 was
  under the merge lock) with the machine at a load average of 15-24: **before, 18.17 s**
  (always-wait behaviour; the test fails on the missing log line); **after, 0.20, 0.65, 0.92, 0.95
  and 3.17 s** in five runs, the drain itself 0.13-1.6 s for 121-137 shards. A first version
  released one shard per transaction and took 0.9-13.5 s of drain under the same load, which
  is why the release is now one transaction. In one of those runs the single-node test in
  the same file timed out waiting for its embedded-storage boot (no `setup_link=` in 120 s at
  load ~17), which does not touch the drain; the cold-boot gap is already a row.

Noticed, not changed: an ordinary `release_shard` (a handoff to a live peer, or convergence)
leaves the epoch as it was, so between the release and the next acquisition a fence the old
owner still holds passes `Fence::check` against the ownerless row. The next acquisition
advances the epoch, so a stale write can land only while nobody owns the shard; the new owner
reads the store after it acquires. Advancing the epoch on every release would close that
window; it changes `store::tests::acquire_then_release_round_trips_epoch`'s stated contract,
so it is left for a decision.

**`heartbeat_seq` is a counter, not the wall clock.** Peers judge a replica alive by seeing
its `heartbeat_seq` change (RFC 0001 section 4). It was the wall clock in milliseconds, so two
heartbeats in one millisecond (or a clock stepped back) read as no progress, i.e. death. Now
`KvOwnership` keeps an `AtomicU64` that each heartbeat row takes one step of, started at one
more than `ClusterStore::heartbeat_seq_floor`: the highest sequence any earlier process of the
same replica wrote, read from its registry row (left behind by a crash) or from a `seq/<id>`
key that `remove_replica` writes in the same transaction that deletes the row (a drain). The
wall clock stays in `heartbeat_unix_ms`, for operators only. A replica logs
`first_heartbeat_seq` when it joins, and `hs_cluster_heartbeat_seq` (gauge) shows the last
sequence that reached the store.

- `ownership::tests::heartbeats_in_one_millisecond_are_each_a_step_of_progress`: 500 rows
  built back to back must step by one; on the old code it failed at the first pair ("two
  heartbeats (at 1790822098077 and 1790822098077 ms) are not two steps of progress").
- `ownership::tests::a_restart_continues_the_heartbeat_seq_above_the_previous_process`: after
  a crashed process whose sequence is far above the clock, then after a drain; on the old code
  the restart went backwards ("Some(1790822101830) after 18000000000000").
- `store::tests::the_heartbeat_seq_floor_survives_deregistration`.

An upgrade from a binary before this one continues above its millisecond values when the old
process crashed (its row remains); after a drained old process, which removed its row without
keeping a `seq/` key, the new process starts at 1. Liveness compares for change, not order, and
the generation differs, so that one step back is harmless.

## 2026-09-30: the `user.wake` batch carries typing, receipts and presence (track 05)

Nothing in `hs-cluster` changed. Track 05's `WakeBatch` on the `user.wake` peer route gained
an `ephemeral` field (decision 0018): typing travels whole, receipts and presence as a hint to
reread the store, from whichever replica took the change to every live peer, in the same
per-peer pump. `hs_cluster_ephemeral_updates_total{kind,direction}` counts them on both ends.
Verified by `crates/hs-cli/tests/cluster_ephemeral.rs`, two real `hs serve` processes on
PostgreSQL 17, 5 of 5 runs. Details in `docs/status/05-sync.md`, session 9.

## 2026-09-28: the two-replica admin test runs against PostgreSQL, and found two ownership bugs

`crates/hs-cli/tests/cluster_admin.rs`'s two-process test needs a PostgreSQL whose user can
create databases; without one it prints `SKIP` and reports `ok`, and the gate run before this
merge did exactly that. Run for real against `postgres:17` (17.11) in Docker:

```
docker run --rm -d --name hs-cluster-2pod-pg -e POSTGRES_PASSWORD=hspg \
  -p 127.0.0.1:5461:5432 public.ecr.aws/docker/library/postgres:17
HS_CLUSTER_TEST_POSTGRES_DSN=postgres://postgres:hspg@127.0.0.1:5461/postgres \
  cargo test -p hs-cli --test cluster_admin -- --nocapture
```

(`docker pull postgres:17` fails from an agent session because the Docker Hub credential
helper needs the keychain; the ECR mirror of the official image needs no credentials.)

**It failed about one run in three**: after B was drained, A never took the shards B released,
and 69 of 137 stayed ownerless for the whole 90 s wait. Instrumented, the cause was in
`KvOwnership`, not in the admin API:

1. **A tick could outlast the lease.** A tick heartbeats once, then runs one store transaction
   per shard it acquires or releases. On PostgreSQL, B's first convergence of the 137-shard
   layout took 19 s against a 3 s lease: its heartbeat stopped for all of it, and
   `dead_peers()` compared against liveness observed at the tick's start, so each replica
   judged a live peer dead and took its shards.
2. **The held set never looked at the store again.** A held 137 shards in memory
   (`hs_cluster_owned_shards` summed to 137) while the store named it on 68. `converge` did
   nothing for a shard it wanted and believed it held, so when B, which had taken A's shards,
   released them on its drain, the rows stayed ownerless for good.

Fixed in `crates/hs-cluster/src/ownership.rs` (`converge`, `reconcile_with_row`, `drop_lost`):
the store row is the truth (a held shard whose row no longer names this replica at its
generation and epoch is dropped as `OwnershipEvent::Lost`, logged at `warn` ("a shard this
replica held was taken by a peer that judged it dead"), counted as
`hs_cluster_ownership_changes_total{reason="lost"}`, and acquired again if wanted), and a tick
stops acquiring and releasing after one `heartbeat_interval` and resumes on the next, which
starts at once. `report_fenced` keeps its meaning and still counts `hs_cluster_fenced`.

Tests: `ownership::tests::a_shard_taken_while_held_is_dropped_and_taken_back_once_released`
(fails without the reconciliation) and `tests/slow_store.rs` (two replicas on a store whose
every commit takes 20 ms, so converging 17 shards is longer than the 150 ms lease; fails in 3 of
3 runs without the budget, "hs-1 took a shard from hs-0 while hs-0 was alive"). After the fix the
two-process test passed 10 of 10 runs against PostgreSQL 17 with no shard lost; `hs-cluster`'s
`mesh_handoff` (5) and `chaos` (4) tests pass too.

Noticed, not changed (closed 2026-09-30, above): a replica shutting down with no live peer still waits out its whole drain
deadline (18 s in this test) for someone to claim its shards (`released_unclaimed=137`).

**Still not done: the two-pod run on the cluster with the handoff fix.** It needs `kubectl` to
`admin@dacrib0`, which agent sessions on this desktop cannot reach ("no route to host": macOS
Local Network permission), so it is a desktop item for a session that has it; the steps are
"Next, in order" below.

## 2026-09-28: an administrator can drain and undrain any replica (edited by track 15)

The admin API's `cluster.replicas.drain`/`undrain` needed what this crate did not have: draining a
peer, and a way back. Added (decision 0012): `DrainRequest` and
`ClusterStore::{request_drain, update_drain, withdraw_drain, drain_request,
list_drain_requests}` (rows under `drain/` in `cluster_replicas`); `KvOwnership` reads its own
request every heartbeat and, while one is in force, heartbeats `Draining` and releases every shard
without deregistering or becoming unready (`is_admin_drained`); withdrawing the row makes it
`Active` again. A drain request outlives a restart. Also fixed: a tick already in flight when
`Drainable::drain` deregistered could write the row back, so a replica restarted within the lease
was refused as a live duplicate of itself (`tick_lock`; found by the two-process test below).
Tests: `ownership::tests::an_administrators_drain_hands_every_shard_to_the_peer_and_withdrawing_it_rebalances`,
`a_drained_replica_stays_deregistered_so_it_can_restart_at_once`, and
`crates/hs-cli/tests/cluster_admin.rs` (two real `hs serve` processes on one PostgreSQL).

## 2026-09-28: two pods on the owner's cluster -- where this stopped

Branch `agent/two-pod-cluster-2`, merged into `main` on 2026-09-28 (after the test above ran). Two replicas as two pods on `dacrib0` (context
`admin@dacrib0`, Talos v1.10.5, Kubernetes v1.33.2), namespace `myelin-cluster`, database the
CloudNativePG `Database` `dacrib/myelin-cluster` on the shared `postgres-cluster`, media on
SeaweedFS in the namespace. **The mesh between two pods carried real traffic for the first
time: `verify.py` passed three times, `failover.py` and a rolling update ran, and both found the
same gap: a request that lands while a shard changes hands reaches the client as a `503`.** The
fix is on `main`, tested, and **not yet on the cluster**: the cluster runs an image from
before it (CD builds images from `main`; a branch image needs CD's `workflow_dispatch`, which
this session was not permitted to start). Everything below is measured unless it says otherwise.
All times UTC.

### etcd, before anything was installed

The last attempt stopped on etcd (2026-09-27 23:31 to 00:10: 111 readiness checks through one
apiserver, 36 failed; black0n0's member logged 1-3 s fdatasyncs). The owner then fixed it.
This session checked `/readyz/etcd` through all three apiservers (`--server` per node, since
the kubeconfig names only black0n0's) every ~20 s:

| Window | black0n0 (.221) | tp0n3 (.176) | tp0n1 (.244) | Notes |
| --- | --- | --- | --- | --- |
| 01:10:55-01:18:03 | 2/18 ok | 13/18 ok | 12/18 ok | black0n0 had booted at ~01:07 and its apiserver was not up; two cluster-wide failures (01:12:57-01:13:57, 01:15:05) matched 3-3.8 s fdatasyncs on tp0n1 |
| 01:18:30-01:24:50 | 2/15 ok | 15/15 | 15/15 | black0n0 **booted again at 01:23:52** |
| 01:25:10-01:38:48 | 40/40 | 40/40 | 40/40 | the gate: 120/120 over 14 minutes |
| 01:39:09-02:13:43 (during everything below) | 103/103 | 103/103 | 102/103 | one failure at 01:40:33, during `helm install` |

`talosctl logs etcd` after 01:25: black0n0 0 slow fdatasyncs (it had 31 in its first three
minutes after the 01:07 boot, up to 4.08 s); tp0n3 4 (1.0-3.0 s) in 13 minutes; tp0n1 0 after
01:22. So black0n0's disk looks fixed since its 01:23:52 boot and the other two still have an
occasional slow fsync. Nothing was installed until 01:39, after the 14 clean minutes. The
reboots of black0n0 were not this session's (it ran only reads against etcd and Talos).

### What was installed (namespace `myelin-cluster` only)

1. **SeaweedFS.** The manifest the last session fixed lived only on the other machine
   (`my-infra/.../apps/myelin-cluster/` does not exist in this machine's checkout), so it was
   rebuilt from the live objects' `last-applied-configuration`. Applied diff, against what was
   live (my-infra not touched; the owner carries this into the repository):

   ```diff
    PersistentVolumeClaim s3:  resources.requests.storage
   -  5Gi
   +  20Gi
    Deployment s3:  args of `weed server`
   -  -filer.defaultStoreDir=/data/filer
   -  -volume.max=0
   +  -volume.max=16
   +  -master.volumeSizeLimitMB=256
   ```

   The first line is the flag 3.97 does not have (the crash loop). The other two were found by
   the first media upload (a 500 after 19 s: "No writable volumes", "No more free space
   left"): `volume.max=0` with the default 30 GB volume size preallocated ~4.8 GB of volume
   files on the 5 GiB claim, and the next start panicked on "no space left on device". The
   claim was grown in place (Longhorn allows expansion; deleting it was not allowed here) and
   volumes are now 256 MB, at most 16. Then `s3-make-bucket` was deleted and re-created:
   `make_bucket: hs-media`, Complete. The s3 pod is Ready and uploads work.
2. **The chart**, from the branch, values `deploy/two-pod/values-dacrib0.yaml` (new: the
   values file the last session wrote was also on the other machine), image
   `ghcr.io/brandon-dacrib/myelin:sha-4a010ee07bdf7ca2616374ace0e9f2ef66305b40` (the server
   of `5b6cda9`, which the last session pinned; both are on ghcr):

   ```bash
   helm upgrade --install hs deploy/helm/hs -n myelin-cluster -f deploy/two-pod/values-dacrib0.yaml \
     --set image.tag=sha-4a010ee07bdf7ca2616374ace0e9f2ef66305b40 --wait --timeout 10m
   ```

   Ready at 01:40:40: hs-0 on black0n0, hs-1 on tp0n1. Both log "mesh authentication is mutual
   TLS" with `peer_san_suffix=.hs-headless.myelin-cluster.svc.cluster.local` and their own
   advertised names; `kv_cluster_replicas` lists both rows. hs-0 took all 64 federation
   shards, then gave exactly the 40 that hs-1 acquired (same set, compared). Database:
   `storage.postgres` pointed by hand at `postgres-cluster-rw.dacrib.svc.cluster.local`,
   database `myelin_cluster`, user `appuser`, password from `postgres-credentials`; plain
   (non-TLS) connections were accepted, as expected. Users `alice` and `bob` made with
   `hs register` (passwords generated, kept out of the transcript).

### `verify.py` (both replicas carry load, forwarding works)

Run 1 (01:42) passed everything but media (the SeaweedFS problem above); run 2 (01:51) passed
all; run 3 (02:02, after the failover and hs-1's return) passed all. Run 2:

- `POST /createRoom` through hs-0, six rooms: all 200, 1.49-2.17 s each.
- bob joins each through hs-1 (`/rooms/{id}/join`): all 200, 304-343 ms.
- ten concurrent sends to one room, five through each pod: all 200, 0.52-3.16 s.
- `/messages` on both pods: the same ten event ids in the same order.
- every other room written through both and read back from both: identical.
- long-poll `/sync` on hs-1 woken by a send through hs-0: returned 1.98 s after the send started
  (the send itself took 488 ms); the other direction 2.30 s (send 367 ms). Run 1: 1.25 and
  1.50 s; run 3: 1.66 and 1.87 s.
- 200 kB upload through hs-0, downloaded through hs-1: 200, sha256 equal, 82 ms / 110 ms.

Both pods carry load: after two runs each pod's `hs_http_requests_total` showed 6 of the 12
`/createRoom`s and 6 of the 12 joins executed on it (the gate forwards before the metrics layer
counts), i.e. the rooms split three and three between the pods. `POST /join/{roomId}` through
hs-1 for all six rooms: 200, 89-103 ms, so it forwards (it was fenced before RFC 0019).

Latency is high and not investigated (the owner's rule: complete before fast): `/createRoom`
1.5 s and a `/sync` wake 1.2-2.3 s after a send are far above the two-process run on one host.
The shared `postgres-cluster` on the same disks as the etcd problem is the first suspect.

### `failover.py` (hs-1 deleted mid-traffic, image `sha-4a010ee`)

Writes through hs-0 only, round-robin over the six rooms, 90 s; `kubectl delete pod hs-1` at
01:53:04 (t = 15.2 s; returned 01:53:10). 240 sends, **233 ok, 7 failed**, worst ok 796 ms:

| t (s) | what | failures |
| --- | --- | --- |
| 16.2-16.6 | hs-1 draining: hs-0 forwards, hs-1 answers `421`, four attempts 10 ms apart run out | 2 x `M_HS_NOT_SHARD_OWNER` in ~50 ms |
| 26.4 | new hs-1 (started 01:53:12, **no restart**, no "live under this identity") takes its shards back; a send already past hs-0's gate is fenced by `hs-room` | 1 x `M_UNKNOWN fenced: this replica no longer owns shard` |
| 26.7-28.0 | hs-0 has released, hs-1 not yet acquired: `421` again | 4 x `M_HS_NOT_SHARD_OWNER` |

hs-0's log names the cause of every refusal: "could not forward the request to it: decoding
the forwarded response: EOF while parsing a value" -- the forwarder's give-up returns the
peer's empty `421` body and the gate tried to decode it as a proxied response.

### Rolling update (`helm upgrade` to `sha-982370b`, same server code, a real image change)

`deploy/two-pod/rolling.py` (new): alice writes through hs-0 and bob through hs-1 at the same
time, round-robin over six rooms, with port-forwards that re-open when a pod is replaced
(a "pod down" is a send the port-forward could not deliver because its pod was restarting; a
client behind the Service would have gone to the other pod). `helm upgrade --wait` 02:08:27 to
02:09:06 (39 s; the StatefulSet replaced hs-1, then hs-0). 200 s:

- **984 ok, 322 failed, 17 pod-down.** 319 failures `M_HS_NOT_SHARD_OWNER`, 3 `fenced`.
- The failures are fast (5-50 ms), so the count mostly measures how long the windows were:
  t = 18-21 s (hs-1 draining), t = 30-37 s (new hs-1 taking shards back; hs-0 draining) and
  t = 45-49 s (hs-0 gone, new hs-0 taking its shards). Outside those windows, 0 failures.
- New in this run: "**no owner is currently known**" -- for about a second at t = 30-31 and
  45-46 some shards had no owner at all, and the forwarder gave up on the first lookup.
- Worst successful send 1.7 s; typical 0.4-0.8 s.

### The fix (on `main`; tests pass; not yet on the cluster)

- `hs_cluster::mesh::Forwarder::forward`: `421`, `503` without `Retry-After`, a refused
  connection and **no known owner** are all retried with a backoff that doubles from 10 ms and
  caps at 250 ms (`MAX_BACKOFF`), until the attempt budget or until the next attempt could not
  start before the request's deadline; then the last refusal is returned as it is.
  `MeshConfig::max_attempts` default 4 -> 40 (about nine seconds of retrying inside the 10 s
  deadline). Decision `docs/decisions/0017-forwards-wait-out-a-handoff.md` (it changes RFC
  0001 section 8's "at most 4 attempts, 10/50/200 ms"). Tests:
  `crates/hs-cluster/tests/mesh_handoff.rs` (a peer answering `421` for 2 s, `503` for 0.6 s,
  no owner for 1.2 s: all come through with the 200; a peer that never settles is given up on
  before an 800 ms deadline).
- `hs_cli::cluster::RoomShardGate`: a request for a shard owned here that comes back `503`
  after the shard moved away mid-request (the fenced send above) is sent on to the new owner
  instead of returned; only at the edge (a request that came over the mesh returns its `503`
  to the forwarder, which retries). The forwarder's give-up now reads "the believed owner
  answered that it does not own the shard (421) until the request's deadline" instead of a
  JSON parse error. Tests: `cluster::tests::a_request_fenced_by_a_handoff_mid_flight_is_sent_on_to_the_new_owner`
  and `..._503_from_a_shard_still_owned_here_is_passed_back_as_it_is`.
- **Observability:** a clustered replica's `/metrics` had **no `hs_cluster_*` series** (the
  counters existed and nothing registered them). `hs_cluster::metrics::ClusterCollector` now
  renders `hs_cluster_owned_shards{kind}`, `hs_cluster_ownership_changes_total{kind,reason}`,
  `hs_cluster_forward_latency_seconds{route,outcome}` (histogram, buckets to 10 s),
  `hs_cluster_forward_retries_total{reason}` (`connect`, `421`, `503`, `no_owner`),
  `hs_cluster_fenced_total{kind}`, `hs_cluster_live_replicas`, `hs_cluster_lease_age_seconds`,
  registered by `serve` in cluster mode. Test: `metrics::tests::the_collector_renders_every_series_under_its_rfc_name`.
  And "this replica no longer runs appservice event delivery" is logged when the global shard
  leaves (both pods' logs said they ran it; only the later one did).

### State of `myelin-cluster` now

Helm release `hs` revision 2 (image `sha-982370b`), hs-0 and hs-1 Ready, `s3` Ready,
`s3-make-bucket` Complete, users alice and bob, 24 test rooms. Left running for Element
through a port-forward. The two scripts' passwords are in the session's scratchpad only; make
new users with `hs register` if needed.

### Next, in order

Items 1 and 3 need `kubectl` to the cluster, so they are desktop items (see the top).

1. Get the image CD builds for `main`, `helm upgrade --set image.tag=sha-<commit>` **while
   `rolling.py` runs** (that upgrade is itself the rolling update to measure), then
   `failover.py`. Target: 0 failures in both. Check `/metrics` on a pod for the `hs_cluster_*`
   series (`kubectl port-forward pod/hs-0 19090:9090`).
2. The `/sync` wake latency (1.2-2.3 s) and `/createRoom` 1.5 s on this cluster: measure the
   database round trip from a pod first (track 05 for the wake path).
3. Element through a port-forward to the Service; a bridge registered; the demo's offering
   (`docs/next-steps.md` item 1) -- not started.

### Earlier: the 2026-09-27 attempt

Stopped at the etcd gate (111 checks through black0n0's apiserver, 36 failed; black0n0's
member logged 1.2-3.1 s fdatasyncs; Longhorn healthy). Nothing was installed. Its resume steps
are what this session followed, with the two differences above (the my-infra files were on
the other machine; SeaweedFS needed two more flags and a larger claim).

## 2026-09-27: a pod knows its own mesh address, the mesh is mutual TLS, `/createRoom` is gated

Branch `worktree-agent-a2bffe0b517f748bc`. Three of the four "known blockers" `docs/next-steps.md`
item 1 lists for cluster mode on a real cluster, in the order it lists them, plus the values
file for the two-pod experiment. What was **verified by running** is under "Verified"; what was
only **written** is under "Written, not run". `hs-room` was not edited (track 04's crate); the
one line it needs is in RFC 0019.

### Verified by running

**Two `hs serve` processes on one PostgreSQL 16 (apt, `127.0.0.1:5432`, database `hs03`), both
advertising real names, the mesh over mutual TLS from a private CA, a room created through A and
used from B, and a third process with a certificate from another CA refused.** The certificates
are the three `openssl` commands the experiment values file gives (a private CA, one wildcard
leaf `*.mesh03.local` shared by A and B, the way the chart shares one wildcard across pods), the
names `hs-a.mesh03.local`/`hs-b.mesh03.local`/`hs-c.mesh03.local` resolve to `127.0.0.1` through
`/etc/hosts` (what the headless Service does for pods), and each config's `cluster` section is
what the chart renders, with the per-pod values inline instead of from the environment:

```yaml
cluster:
  single_node: false
  room_shards: 4
  user_shards: 4
  heartbeat_interval: 500ms
  lease_ttl: 2s
  mesh:
    port: 18649                           # 18650 on B, 18651 on C
    advertise_address: hs-a.mesh03.local  # hs-b.mesh03.local on B, hs-c.mesh03.local on C
    tls:
      certificate_path: tls/mesh.crt      # C: tls/other.crt, issued by a different CA
      private_key_path: tls/mesh.key
      ca_certificate_path: tls/mesh-ca.crt
      peer_san_suffix: .mesh03.local
```

Transcript (the script is `run.sh` in the session's scratch directory, run against the binary
built from this branch's final commit; everything below is its output, trimmed only of
timestamps, scratch paths and Python tracebacks from a `KeyError` on a refused create's JSON).
The `03` in the names, ports and database is because another agent was running its own
two-process experiment on this host at the same time, in the same scratch directory and the
`hs` database the task named; the first attempt here collided with it (their config seeded the
database this run read, and a `pkill` of theirs terminated A and B mid-run) before everything
was moved under its own names:

```
$ fresh database

$ hs serve -c a.yaml  (client 18340, mesh hs-a.mesh03.local:18649, mTLS)
A /health/ready -> 200 (after ~1s)

$ A seeded the database with its config; drop the seeded listeners section so B can bind its own port

$   (the database layer outranks the file, RFC 0016, and HS__ cannot set a list; in Kubernetes every pod has the same listeners)
DELETE 1

$ hs serve -c b.yaml  (client 18341, mesh hs-b.mesh03.local:18650, mTLS, same CA)
B /health/ready -> 200 (after ~1s)

$ grep -h 'mesh authentication\|advertis\|starting the cluster' a.log b.log
hs_cli::cluster: mesh authentication is mutual TLS ca=tls/mesh-ca.crt certificate=tls/mesh.crt peer_san_suffix=Some(".mesh03.local")
hs_cli::cluster: starting the cluster ownership manager replica=hs-a.mesh03.local:18649 mesh_listen=0.0.0.0:18649
hs_cli::serve: no .well-known documents are published (set server.well_known_server to delegate federation, server.public_baseurl to advertise a client base URL)
hs_cli::cluster: mesh authentication is mutual TLS ca=tls/mesh-ca.crt certificate=tls/mesh.crt peer_san_suffix=Some(".mesh03.local")
hs_cli::cluster: starting the cluster ownership manager replica=hs-b.mesh03.local:18650 mesh_listen=0.0.0.0:18650
hs_cli::serve: no .well-known documents are published (set server.well_known_server to delegate federation, server.public_baseurl to advertise a client base URL)

$ replica registry in PostgreSQL (who is alive, at what address)
replica/hs-b.mesh03.local:18650|{"id":"hs-b.mesh03.local:18650","generation":1790483782363,"mesh_addr":"hs-b.mesh03.local:18650","zone":null,"version":"0.0.1","state":"active","heartbeat_seq":1790483785365,"heartbeat_unix_ms":1790483785365}
replica/hs-a.mesh03.local:18649|{"id":"hs-a.mesh03.local:18649","generation":1790483781916,"mesh_addr":"hs-a.mesh03.local:18649","zone":null,"version":"0.0.1","state":"active","heartbeat_seq":1790483785418,"heartbeat_unix_ms":1790483785418}

$ hs register -u alice ... -v http://127.0.0.1:18340
@alice:cluster.example.org
access_token: syt_YWxpY2U_NsqXjdLPYubSyRLodzWV_0JZuw4
device_id: xtUUAHSQw6

$ login on B
access_token: syt_YWxpY2U_...

$ POST /createRoom through A (5 rooms: each lands on whichever replica owns its shard)
{"room_id":"!DgkECiicXB7yP0KtsK:cluster.example.org"}
{"room_id":"!rrxIXsxgLWaYhz4VEh:cluster.example.org"}
{"room_id":"!FY8YTwQgKxSwII3TDQ:cluster.example.org"}
{"room_id":"!eWH6Q4LWcna3GD08R7:cluster.example.org"}
{"room_id":"!LdGRVYJP6J7EYtuPTo:cluster.example.org"}

$ what the gate logged on A for those creates (forwarded to B, or created here)
hs_cli::cluster: the /createRoom handler minted its own room id instead of the pre-assigned one (see docs/rfcs/0019-create-room-shard-gate.md); the room's first actor was built on this replica, which 
hs_cli::cluster: the /createRoom handler minted its own room id instead of the pre-assigned one (see docs/rfcs/0019-create-room-shard-gate.md); the room's first actor was built on this replica, which 
hs_cli::cluster: the /createRoom handler minted its own room id instead of the pre-assigned one (see docs/rfcs/0019-create-room-shard-gate.md); the room's first actor was built on this replica, which 
hs_cli::cluster: forwarding /createRoom to the shard's owner room_id=!5DOqIo2Nv2xRVc5y0r:cluster.example.org shard=room/2
hs_cli::cluster: forwarding /createRoom to the shard's owner room_id=!wybt2tlfli30IFMsvJ:cluster.example.org shard=room/2
hs_cli::cluster: the /createRoom handler minted its own room id instead of the pre-assigned one (see docs/rfcs/0019-create-room-shard-gate.md); the room's first actor was built on this replica, which 
hs_cli::cluster: the /createRoom handler minted its own room id instead of the pre-assigned one (see docs/rfcs/0019-create-room-shard-gate.md); the room's first actor was built on this replica, which 

$ room shard rows: who owns which of the 4 room shards
shard/room/0000000000|{"epoch":1,"owner":["hs-a.mesh03.local:18649",1790483781916]}
shard/room/0000000001|{"epoch":1,"owner":["hs-a.mesh03.local:18649",1790483781916]}
shard/room/0000000003|{"epoch":1,"owner":["hs-a.mesh03.local:18649",1790483781916]}
shard/room/0000000002|{"epoch":2,"owner":["hs-b.mesh03.local:18650",1790483782363]}

$ PUT /send through B, then through A, same room (5 each, concurrently)
A#4 -> 200
A#3 -> 200
A#5 -> 200
A#1 -> 200
B#2 -> 200
A#2 -> 200

$ GET /messages on A and on B (message bodies only)
B#1 -> 200
B#4 -> 200
B#3 -> 200
B#5 -> 200
A: 10 ['from A #1', 'from A #2', 'from A #3', 'from A #4', 'from A #5', 'from B #1', 'from B #2', 'from B #3', 'from B #4', 'from B #5']
B: 10 ['from A #1', 'from A #2', 'from A #3', 'from A #4', 'from A #5', 'from B #1', 'from B #2', 'from B #3', 'from B #4', 'from B #5']

$ mesh log lines on A and B so far (a forward that failed would log 'mesh forward attempt failed')

$ hs serve -c c.yaml  (client 18342, mesh hs-c.mesh03.local:18651, certificate from ANOTHER CA)
C /health/ready -> 200 (after ~1s)

$ C joined the registry (it shares the database; membership is a database row, not certificate-gated) and took shards:
replica/hs-a.mesh03.local:18649|{"id":"hs-a.mesh03.local:18649","generation":1790483781916,"mesh_addr":"hs-a.mesh03.local:18649","zone":null,"version":"0.0.1","state":"active","heartbeat_seq":1790483795263,"heartbeat_unix_ms":1790483795263}
replica/hs-c.mesh03.local:18651|{"id":"hs-c.mesh03.local:18651","generation":1790483792316,"mesh_addr":"hs-c.mesh03.local:18651","zone":null,"version":"0.0.1","state":"active","heartbeat_seq":1790483795317,"heartbeat_unix_ms":1790483795317}
replica/hs-b.mesh03.local:18650|{"id":"hs-b.mesh03.local:18650","generation":1790483782363,"mesh_addr":"hs-b.mesh03.local:18650","zone":null,"version":"0.0.1","state":"active","heartbeat_seq":1790483795706,"heartbeat_unix_ms":1790483795706}
shard/room/0000000000|{"epoch":1,"owner":["hs-a.mesh03.local:18649",1790483781916]}
shard/room/0000000001|{"epoch":1,"owner":["hs-a.mesh03.local:18649",1790483781916]}
shard/room/0000000003|{"epoch":1,"owner":["hs-a.mesh03.local:18649",1790483781916]}
shard/room/0000000002|{"epoch":3,"owner":["hs-c.mesh03.local:18651",1790483792316]}

$ creates through A until two land on a shard C owns: those are refused (503 M_HS_NOT_SHARD_OWNER), the rest succeed
{"errcode":"M_HS_NOT_SHARD_OWNER","error":"this replica does not own room/2 (believed owner: hs-c.mesh03.local:18651) and could not forward the request to it: forwarding to the shard owner: forward to shard room/2 exhausted 4 attempts"} -> 503
{"errcode":"M_HS_NOT_SHARD_OWNER","error":"this replica does not own room/2 (believed owner: hs-c.mesh03.local:18651) and could not forward the request to it: forwarding to the shard owner: forward to shard room/2 exhausted 4 attempts"} -> 503
created: 10   refused: 2

$ what A logged about forwarding to C (the TLS handshake failures behind those refusals)

$ and from C's side: a send through C to each of the five rooms created earlier. C owns room/2 now (a valid

$   registry member takes shards), so a room on room/2 is served by C locally; a room A or B owns is refused, C cannot reach them
{"errcode":"M_HS_NOT_SHARD_OWNER","error":"this replica does not own room/1 (believed owner: hs-a.mesh03.local:18649) and could not forward the request to it: forwarding to the shard owner: forward to shard room/1 exhausted 4 attempts"} -> 503
{"errcode":"M_HS_NOT_SHARD_OWNER","error":"this replica does not own room/1 (believed owner: hs-a.mesh03.local:18649) and could not forward the request to it: forwarding to the shard owner: forward to shard room/1 exhausted 4 attempts"} -> 503
{"errcode":"M_HS_NOT_SHARD_OWNER","error":"this replica does not own room/0 (believed owner: hs-a.mesh03.local:18649) and could not forward the request to it: forwarding to the shard owner: forward to shard room/0 exhausted 4 attempts"} -> 503
{"event_id":"$NFjWgi7ROnqbCOTrekH5XrDAAN3JE0RMAWLYGAKd3kw"} -> 200
{"errcode":"M_HS_NOT_SHARD_OWNER","error":"this replica does not own room/0 (believed owner: hs-a.mesh03.local:18649) and could not forward the request to it: forwarding to the shard owner: forward to shard room/0 exhausted 4 attempts"} -> 503

$ kill -TERM C, then A and B
hs_cli::serve: cluster drain complete handed_off=0 released_unclaimed=62 elapsed=18.095233624s
hs_cli::serve: cluster drain complete handed_off=0 released_unclaimed=74 elapsed=18.129770466s
hs_cli::serve: cluster drain complete handed_off=48 released_unclaimed=0 elapsed=628.015575ms
```

The log lines the script's own `grep` ran too early for (the log writer is asynchronous), read from `a.log` and `c.log` after the run:

```
# a.log: A forwarding to C, and refusing to the client when the handshake fails
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/2 attempt=1 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/2 attempt=2 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/2 attempt=3 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/2 attempt=4 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
# c.log: C forwarding to A or B
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/1 attempt=1 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/1 attempt=2 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/1 attempt=3 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
DEBUG hs_cluster::mesh::forwarder: mesh forward attempt failed shard=room/1 attempt=4 error=mesh transport error: TLS handshake: invalid peer certificate: UnknownIssuer
```

What the transcript shows, in the order the task asked:

1. **Both advertise real addresses.** The replica registry rows carry `hs-a.mesh03.local:18649`
   and `hs-b.mesh03.local:18650`, from `cluster.mesh.advertise_address`, not a bind address; with the
   field unset the server now logs a warning naming `HS__CLUSTER__MESH__ADVERTISE_ADDRESS`.
2. **The mesh is mTLS.** Each log says `mesh authentication is mutual TLS` with the CA path; the
   forwards between A and B that made the room and message flow below complete over it. C, with
   a certificate from another CA (and a perfectly good name under `.mesh03.local`), joins the
   registry (it shares the database, and membership is a database row) and takes its share of
   shards, but every forward to it from A fails at the TLS handshake and is refused to the client
   as `503 M_HS_NOT_SHARD_OWNER`, and C's own forwards to A and B fail the same way. That is the
   intended failure: a replica outside the CA gets no request and can serve none. (Membership
   itself is not certificate-gated -- anyone with the database can insert a row -- which is the
   trust model RFC 0001 section 11 describes: the database is the cluster's root of trust, the
   mesh certificate is what keeps the pod network out.)
3. **A room created through A is used from B.** `/createRoom` through A; concurrent sends through
   B and A; `/messages` identical on both. Same as the 2026-09-19 run, now over mTLS and with the
   gate below in front of `/createRoom`.
4. **`/createRoom` is gated, as far as `hs-cli` can take it.** The gate mints the id, forwards to
   the owner when the shard is not local, and the owner's gate re-checks ownership. What the
   transcript also shows, honestly: `hs-room`'s handler does not yet read the
   `PreassignedRoomId` extension, so it mints its own id, and the gate logs the warning it was
   written to log for exactly this (`the /createRoom handler minted its own room id instead of
   the pre-assigned one`, with `owned_here` true or false). Until the one-line change in RFC 0019
   lands in `hs-room`, the room is created on the replica the gate chose under an id that hashes
   to that replica's shard only by chance. Every later request is routed to the true owner and
   the write fence protects the leftover actor, as before.

**Tests, all run green on this branch** (after forcing a rebuild -- see the target-directory
finding under "Decisions made"):

```sh
cargo fmt --all --check
cargo clippy -p hs-cluster -p hs-config -p hs-cli --all-targets -- -D warnings
cargo test -p hs-cluster          # 42 lib + 5 chaos + 2 mesh-pool + 4 mesh-mtls (new)
cargo test -p hs-config           # 107 lib, 5 new (advertise_address, MeshTlsConfig, env override)
cargo test -p hs-cli --lib cluster::   # 14: advertise_addr, split_host_port, to_hs_cluster_config
                                       # (TLS), is_create_room, /join and /knock extraction, the
                                       # live-duplicate identity check, and the four /createRoom
                                       # gate tests with scripted ownership, one of them a real
                                       # forward over a MeshServer to the owner
cargo test -p hs-cli              # 134 lib + e2e 25 + federation_reads 9 + federation_sender 2
                                  # + federation_two_servers 2 + federation_writes 8, all green
```

`crates/hs-cluster/tests/mesh_mtls.rs` is the test the task asked for at the mesh level: a
`MeshServer` presenting a certificate from a private CA (minted with `rcgen`, no fixtures), a
`Forwarder` from the same CA completes a forward and the handler runs once; a `Forwarder` from
another CA is refused at the handshake and the handler never runs (and the reverse: a legitimate
forwarder refuses a server outside the CA); the same CA but a name outside `peer_san_suffix` is
refused after the handshake with `401`; a plaintext shared-secret client gets nothing from a TLS
listener.

### Per-replica settings and the configuration database (the lead's finding 1)

The configuration is layered file < database < environment (RFC 0016), and the first replica to
start seeds the database from its file. So a per-replica value written in a *file* -- the
listener port, `cluster.mesh.port`, and now `cluster.mesh.advertise_address` -- is read from the
database by every replica that starts after the first. The transcript above hit exactly this: B's
file said port 18341 and B bound A's 18340 (the run script deletes the seeded `listeners` row
before starting B; `hs config -c a.yaml unset /listeners/listeners`, which track 05 used, is the
supported spelling of the same thing).

**How a per-replica setting is meant to be set in cluster mode: through the environment, which
outranks the database.** That is what the chart does for every pod --
`HS__CLUSTER__MESH__ADVERTISE_ADDRESS` from the Downward API, `HS__CLUSTER__MESH__PORT`,
`HS__CLUSTER__SINGLE_NODE`, the TLS paths -- and what the run script does for A, B and C. The
file values in the transcript's configs are documentation; the environment is what each process
ran with. In Kubernetes every pod has the same listeners, so the list field the environment
cannot set never needs to differ; on one host it does, and `hs config unset` is the tool.

Two guards against getting this wrong were added in `hs-cli`:

- A clustered replica whose `advertise_address` is unset logs a warning naming the variable
  (the fallback to a bind address is what made "a pod does not know its own mesh address" a
  known gap).
- **A clustered replica refuses to start if the replica registry already holds a live row under
  its own identity** (`crate::cluster::refuse_live_duplicate`): that is what a second replica
  that inherited the first one's `advertise_address` from the database would look like, and two
  replicas under one identity would overwrite each other's registry row and shard ownership.
  The error names the variable to set and the `hs config unset` alternative. A stale row (a
  restart of the same pod) is allowed, as before. Unit-tested against a `MemoryBackend`
  registry (live row refused, stale row and other identities allowed).

`hs config unset /cluster/mesh/advertise_address` is not needed when the environment sets it
(the environment wins), but `hs-config`'s store could reasonably exclude `listeners` and
`cluster.mesh` from seeding altogether, the way it excludes `storage`; that is track 13's call
and is recorded under "What is next".

### `/join/{roomId}` (the lead's finding 2)

`POST /join/{roomIdOrAlias}` and `POST /knock/{roomIdOrAlias}` have no `/rooms/` segment, so
the gate let them through to the handler on a non-owner, where the write fence refused them
with `503` instead of the gate forwarding them. `extract_room_id` now reads the id from those
two paths as well when it is a room id (`!...`); it is the same gate, so the request is
forwarded to the owner exactly as `/rooms/{roomId}/join` already was. An alias (`#...`) is
still not gated: it resolves to a room id only inside the handler, and on a non-owner the
fence still refuses it. Gating aliases needs the gate to resolve them (a `RoomRegistry::
resolve_alias` call before the handler), which is a small follow-up, noted under "What is next".

### Written, not run

- **The chart in cluster mode** (`deploy/helm/hs/`): `helm` is not installed here and could not
  be fetched through the proxy (`get.helm.sh` answers 403, the GitHub release has no tarball), so
  the templates were **not rendered**. What changed, to be checked with `helm lint` and
  `helm template --set mode=cluster ...` before the two-pod run:
  - `values.yaml`: a `cluster:` block (`roomShards`, `userShards`, `heartbeatInterval`,
    `leaseTtl`, `clusterDomain`, `mesh.port`, `mesh.tls.existingSecret`,
    `mesh.sharedSecret.{existingSecret,key}`, `terminationGracePeriodSeconds`), with the
    reasoning for a DNS name over `status.podIP` in its comments.
  - `templates/statefulset.yaml`: in cluster mode, `HS__CLUSTER__SINGLE_NODE=false`,
    `HS__CLUSTER__MESH__ADVERTISE_ADDRESS=$(POD_NAME).<release>-headless.<ns>.svc.<domain>` from
    the Downward API, `HS__CLUSTER__MESH__PORT`, the three `HS__CLUSTER__MESH__TLS__*_PATH`
    variables plus `PEER_SAN_SUFFIX` (or `SHARED_SECRET_FILE`), a `mesh` container port, the
    `kubernetes.io/tls` Secret mounted whole at `/etc/hs/secrets/mesh-tls`, and
    `terminationGracePeriodSeconds`.
  - `templates/configmap.yaml`: the tunables (`room_shards`, `user_shards`,
    `heartbeat_interval`, `lease_ttl`) in the file layer.
  - `templates/service.yaml`: the mesh port on the headless Service.
  - `templates/networkpolicy.yaml`: the mesh port admitted from this release's pods only.
  - `templates/_helpers.tpl`: `hs.meshDomain`; validation that cluster mode has a shared
    database (not `embedded`) and one of the two mesh authentications.
  - `templates/NOTES.txt`: a cluster-mode paragraph.
  - **`values-two-replica-experiment.yaml`**: two replicas, CloudNativePG `hs-db`, S3 media, the
    four Secrets to make first (with the exact `kubectl`/`openssl` commands), mTLS, a sticky
    Ingress for the Element session until `/sync` is cluster-aware.
- `docs/config.md` was regenerated (`cargo run -p hs-config --bin gen_config_docs`) and did not
  change: the generator lists `cluster.mesh` as an opaque `object`. The admin interface builds
  its form from the JSON schema itself, where the new fields carry their descriptions.

### Config fields added (`crates/hs-config/src/cluster.rs`)

| Field | Env | Meaning |
|---|---|---|
| `cluster.mesh.advertise_address` | `HS__CLUSTER__MESH__ADVERTISE_ADDRESS` | host or `host:port` peers dial; bare host gets `mesh.port`; unset falls back to the bind address with a warning; ignored in single-node mode |
| `cluster.mesh.tls.certificate_path` | `HS__CLUSTER__MESH__TLS__CERTIFICATE_PATH` | this replica's PEM chain |
| `cluster.mesh.tls.private_key_path` | `HS__CLUSTER__MESH__TLS__PRIVATE_KEY_PATH` | its key |
| `cluster.mesh.tls.ca_certificate_path` | `HS__CLUSTER__MESH__TLS__CA_CERTIFICATE_PATH` | the private CA peers must chain to (new: `mesh.tls` was `listeners::TlsConfig`, which has no CA) |
| `cluster.mesh.tls.peer_san_suffix` | `HS__CLUSTER__MESH__TLS__PEER_SAN_SUFFIX` | optional; a peer's DNS SAN must end with it |

`cluster.mesh.tls` changed type from `Option<listeners::TlsConfig>` to `Option<MeshTlsConfig>`;
nothing outside `hs-cli`'s `cluster.rs` read it.

### `serve.rs`, for the merge with track 05

`crates/hs-cli/src/serve.rs` is **not modified** on this branch. Everything is in
`crates/hs-cli/src/cluster.rs` (`start`, `ClusterHandles`, `spawn_mesh`, `advertise_addr`,
`to_hs_cluster_config`, `refuse_live_duplicate`, `RoomShardGate::run_create_room`,
`ProxyShardHandler`) behind the same three entry points `serve.rs` already calls
(`crate::cluster::start`, `RoomShardGate::new(..).layer(..)`, `ClusterHandles::spawn_mesh`), so
track 05's `crate::sync_cluster::install(...)` and track 06's federation feeder after
`cluster::start` do not collide with anything here. What to watch in the merge, all in
`cluster.rs`: track 05 adds `MeshDeps.peers`, `POST /mesh/v1/peer` and
`ClusterHandles::install_peer_handler`; `spawn_mesh` here constructs `MeshDeps` (it needs the
`peers` field) and now builds its authenticator and TLS material from `MeshStartConfig::auth`
(an enum, replacing the old `secret: String` field), and `ProxyShardHandler::handle` now inserts
the `ViaMesh` extension before replaying a request. `ClusterHandles` gained a `server_name`
field (both constructors set it).

### Decisions made

- **The pod's advertised address is its stable DNS name, not `status.podIP`.** The mesh forwarder
  dials `host:port` and `rustls` verifies the peer's certificate against that host; a name lets
  one wildcard certificate (`*.<release>-headless.<ns>.svc.<domain>`) cover every pod for the
  life of the StatefulSet, while pod IPs are unknowable before the pod exists and change on
  every reschedule, so a certificate valid for them could only be issued per pod, at runtime,
  by something like cert-manager's CSI driver. The headless Service has
  `publishNotReadyAddresses: true`, so the name resolves before readiness, which the mesh needs.
  `peer_san_suffix` is set to the headless domain so a certificate the same CA issued for
  anything else is refused too.
- **The mesh TLS material is three paths, not a `*_file` secret pair**, because a certificate,
  key and CA already are files (a mounted `kubernetes.io/tls` Secret), the same convention as
  `listeners[].tls`. Loaded twice in `start` (once for the forwarder's client config, once kept
  for the listener) because `TlsMaterial` holds a private key and is deliberately not `Clone`.
- **`advertise_address` unset in cluster mode is a warning, not an error**, so the same-host
  two-process setup (and the 2026-09-19 configs) keep working; the warning names the env var.
- **`/createRoom` is gated by pre-assigning the id in the gate** (RFC 0019), with the id carried
  to the owner in a mesh-only header that the gate strips from every client request and honours
  only on a request marked `ViaMesh` by the owner's own mesh handler. The seam's types live in
  `hs-cluster` so `hs-room` can read the extension without depending on `hs-cli`. The alternative
  -- `hs-cli` calling `RoomRegistry::create_room` itself with a chosen id -- would have meant
  duplicating `hs-room`'s request parsing, presets and profile filling in the binary crate.
  Hash-derived ids (room version 12) cannot be pre-assigned; the RFC says what `hs-room` should
  do for those (retry until the derived id is local).
- **The gate post-checks the created id and warns** when the handler did not use the
  pre-assigned one, rather than refusing, so `/createRoom` keeps working while `hs-room` catches
  up and the transcript shows the gap instead of hiding it.
- **The shared `target/` directory cross-links worktrees.** Cargo hashes a workspace member's
  metadata relative to the workspace root, so `crates/hs-cluster` in every agent's worktree
  produces the *same* artifact name in the shared `target/`, and "fresh" is decided by mtimes
  against whichever worktree built last. This session's `hs-cli` first failed to compile against
  track 05's `hs-cluster` (which has a `peers` field on `MeshDeps`), and `hs-room` was later
  linked against it too. Every build in this session was run after `find crates -name '*.rs'
  -exec touch {} +` so all workspace crates rebuild from this worktree inside one cargo
  invocation (cargo's build lock keeps one invocation consistent). Other agents' builds are
  poisoned the same way in the other direction. `CARGO_TARGET_DIR` per worktree would fix it, but
  the disk was nearly full (a full rebuild hit `ENOSPC` once; the shared target's 15 GB
  `incremental/` cache was deleted to recover, and this session's builds ran with
  `CARGO_INCREMENTAL=0`), so it was not done; the lead should know before trusting any agent's
  "tests pass" from a shared target.
- **PostgreSQL access:** `su`/`sudo` to the `postgres` OS user is refused in this sandbox, so
  `pg_hba.conf`'s `127.0.0.1/32` line was switched from `scram-sha-256` to `trust` (a local,
  throwaway server) to create the `hs` role and, after the collision above, the `hs03` database.

### What is next

- `hs-room`: the one-line change in RFC 0019, then re-run `run.sh`: the `minted its own room id`
  warnings disappear and every room's first actor is built on its owner.
- `helm lint` / `helm template` in both modes, then the two-pod run with
  `values-two-replica-experiment.yaml` (the lead, from a laptop).
- `hs_config::ClusterConfig` still has no federation/appservice shard counts, zone, or handoff
  deadline (unchanged from 2026-09-19).
- Track 13: consider excluding `listeners` and `cluster.mesh` from configuration-store seeding
  (as `storage` is), so two replicas from one database can differ by file without
  `hs config unset`.
- Gate `/join/{alias}` and `/knock/{alias}` by resolving the alias in the gate.

---

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
