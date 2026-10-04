# 0028: 2026-10-04: a replica keeps its shards until its lease lapses

Status: accepted (track 03; no interface change for other tracks).

## The problem

`hs-cluster` judged its own heartbeat in two places with two different clocks. A replica
answers `is_mine` for a shard it holds while its last good heartbeat is under `lease_ttl`
old, and its peers take a shard from it only once its heartbeat row has not changed for
`lease_ttl` on their clock: the lease. But `converge`, the tick that moves a replica's
holdings toward what rendezvous hashing wants, wanted a shard only while the heartbeat was
*fresh*: under `2 * heartbeat_interval` (2 s at the defaults, 1 s in the two-replica tests).
One heartbeat that failed, or a tick that ran late by two intervals after a failed one, and
the next tick released every shard the replica held. No peer had judged it dead; nothing was
wrong with the lease. A peer took the shards on its next tick and handed them straight back
once the heartbeat recovered, and every request in between was forwarded, retried or fenced
(`421`s, `503 M_HS_NOT_SHARD_OWNER`, fenced writes in the room actors) for nothing.

Found on 2026-10-01 chasing a flaky `cluster_create_room.rs` on a machine at load 15+, where
a debug build is late by that much often enough; logged since then (`warn` with the gap);
listed as an open decision in `docs/next-steps.md`.

## What was chosen

A replica's own heartbeat gates what it *takes* and what it *keeps* differently:

- It **claims a new shard** only while its heartbeat is fresh (`self_heartbeat_fresh`, last
  good heartbeat under `2 * heartbeat_interval`). This is the same freshness it needs to judge
  a peer dead (RFC 0001 section 4): a replica whose store round trips are failing should not
  be taking anything from anybody.
- It **keeps a shard it holds** while its lease is alive (`self_lease_alive`, last good
  heartbeat under `lease_ttl`): exactly as long as `is_mine` answers for the shard, and
  strictly before any peer can have judged it dead (a peer counts the same `lease_ttl` from
  the moment it last saw the heartbeat change, which is never earlier than the heartbeat
  itself). Once the lease lapses the replica releases its shards in its next tick; `is_mine`
  already refused them at read time.

So a transient heartbeat failure costs a replica nothing while it lasts less than a lease,
and a longer one costs it exactly what it would have cost anyway. The `warn` lines at
`tick` say when a heartbeat failed, how long the gap was and whether it reached the lease.

## What was rejected

- **Keep claiming while the lease is alive too.** A replica whose store is failing it would
  then take shards from peers it has no fresh basis to judge. Taking needs freshness; keeping
  does not, because keeping takes nothing from anyone.
- **Release at some third threshold between the two.** There is no third fact to tie it to.
  The lease is the one number every replica agrees on, and `is_mine`, the peers and now
  `converge` use it alike.
- **Make `lease_ttl` the freshness bound for judging peers as well.** That would let a
  replica whose own heartbeats had failed for most of a lease declare a live peer dead on a
  stale observation; RFC 0001 chose two intervals for that deliberately and it stays.

## Consequences

- `KvOwnership::converge` takes `self_lease_alive` beside `self_heartbeat_fresh`;
  `is_mine` and `Drainable::ready` share the `self_lease_alive` predicate. No public type or
  configuration key changes. `docs/config.md` and the chart's comment on `leaseTtl` now say the
  replica itself holds on for the lease.
- `ownership::tests::a_late_heartbeat_keeps_the_shards_until_the_lease_lapses` fails on the
  old code (the first tick two intervals after the last good heartbeat gave up `room/0`), and
  `a_replica_with_a_stale_heartbeat_claims_no_new_shard` pins the other half.
- Operators see the gap as before: `hs_cluster_lease_age_seconds` (time since the replica's
  last successful heartbeat), the admin API's `last_heartbeat_at` and `heartbeat_seq` per
  replica, and the Cluster page's "Last heartbeat" column.
