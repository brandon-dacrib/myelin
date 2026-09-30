# 0017: A forward waits out a shard handoff (2026-09-28)

Status: accepted (track 03). Amends RFC 0001 section 8's retry policy.

## Context

RFC 0001 section 8 said: retry a forward on a refused connection, on `421` and on `503`, with
backoff 10, 50, 200 ms, at most 4 attempts within the request's deadline. The first run of two
pods on a real cluster (`docs/status/03-cluster.md`, 2026-09-28) measured what that means for
a client:

- a graceful `kubectl delete pod` of one replica left a ~0.4 s window in which the other
  forwarded to it and got `421`;
- the replica coming back took ~1.6 s from being asked to acquiring its shards;
- during a rolling update some shards had no owner at all for ~1 s, and a forward with no
  owner failed on its first lookup.

Four attempts 10 ms apart are over in ~50 ms, so every request in those windows reached the
client as `503 M_HS_NOT_SHARD_OWNER`: 7 of 240 sends in a failover, 322 failures over three
windows of a rolling update.

## Decision

- `421`, `503` without `Retry-After`, a refused connection and "no owner known" are all retried
  with a backoff that doubles from `retry_base_backoff` (10 ms) and caps at 250 ms.
- A forward stops when the attempt budget is spent or when the next attempt could not start
  before the request's deadline, and then returns the last refusal as it is.
- `MeshConfig::max_attempts` defaults to 40, which with the cap is about nine seconds inside the
  default 10 s deadline. The deadline, not the count, is the real bound.
- At the edge, a request whose shard moved away while it ran here (so the room's fence refused
  its write with a `503`) is sent on to the new owner.

## Consequences

A request that lands mid-handoff waits up to the handoff's length (seconds) instead of failing
in milliseconds. A client sees a slower request, not an error. A peer that is really gone is
retried until the deadline, which is the lease TTL's order anyway (a dead owner's shards move
when its lease expires). `hs_cluster_forward_retries_total{reason}` and
`hs_cluster_forward_latency_seconds` show how often and how long.
