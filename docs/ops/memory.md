# Memory: what to watch, and the federation sender

Written 2026-10-10 after the demo pod's memory crept 13 MiB an hour while idle and ran past its
limit twice in one evening (`docs/status/06-federation.md`, "2026-10-10: the leak hunt"). This
page names the metrics that were added so the next time is a dashboard, not a kubelet reading.

## Process memory

`/metrics` carries the process's own memory, read from the operating system at each scrape:

| metric                          | what it is                                                      |
|---------------------------------|-----------------------------------------------------------------|
| `process_resident_memory_bytes` | The resident set: this process's pages in physical memory now.  |
| `process_virtual_memory_bytes`  | Address space mapped. Large and uninformative on macOS.         |

Both are absent (not zero) on a platform where they cannot be read. The alert `HsMemoryNearLimit`
(`deploy/helm/hs/templates/prometheusrule.yaml`) fires on the kubelet's
`container_memory_working_set_bytes`, which counts the page cache with the heap. When it fires,
compare the two: a working set well above the resident set is file pages (Fjall's journal and
tables, media), which the kernel can drop; a resident set that itself climbs is the process.

A resident set that climbs linearly while nothing is happening is most likely write buffering in
the embedded store: Fjall keeps every version of a rewritten key in its keyspace's memtable until
that memtable reaches 64 MiB, and there is no cap on the total across keyspaces (RFC 0024). Any
writer on a timer (presence, cluster leases, positions, a retrying federation destination) fills
one. It plateaus or saw-tooths at the memtable size; it is not a leak in the usual sense, but it is
memory an operator has to budget for until RFC 0024 lands.

## The federation sender

The outbound sender keeps one worker per destination server. Its state is on `/metrics` as gauges,
read from the sender at each scrape:

| metric                                          | what it is                                                      |
|-------------------------------------------------|-----------------------------------------------------------------|
| `hs_federation_sender_destinations`             | Destinations this replica has a worker for.                     |
| `hs_federation_sender_pdus_pending`             | PDUs queued for them and not yet accepted or dropped.           |
| `hs_federation_sender_edus_queued`              | In-memory EDUs (typing, receipts, presence) waiting.            |
| `hs_federation_sender_destinations_backing_off` | Workers waiting out a retry right now.                          |
| `hs_federation_sender_state_bytes`              | An estimate of the per-destination state held in memory.        |

and its attempts as counters, rendered at zero for every label from the first scrape:

| metric                                             | what it counts                                                                        |
|----------------------------------------------------|---------------------------------------------------------------------------------------|
| `hs_federation_transactions_total{outcome}`        | Attempts at `PUT /send`: `accepted`, `rejected` (non-2xx), `unresolvable` (no DNS), `failed` (connection, TLS, timeout), `deferred` (not attempted: the destination's backoff had not ended), `dropped` (this server's own policy). |
| `hs_federation_pdus_sent_total`                    | PDUs in accepted transactions.                                                        |
| `hs_federation_key_fetch_failures_total{reason}`   | Other servers' signing keys that could not be fetched, by reason.                     |

What to expect from a large public room: it names hundreds of servers whose DNS is gone. Each is
one worker, backing off from a second to an hour (persisted, so a restart carries on where it was),
and the client refuses to ask the resolver again until that backoff ends. `unresolvable` grows
slowly and `deferred` faster; `destinations_backing_off` is close to `destinations`. The log says
so three times per dead destination in all (`federation destination did not resolve; will retry`
at `warn` on the first failure, when the wait reaches a minute, and when it reaches the ceiling),
the rest at `debug`. A `warn` rate of several a second from the sender is a regression.

To get rid of a dead destination for good: `DELETE /api/v1/federation/destinations/{server}` or
`POST /api/v1/federation/destinations/prune` (decision 0042), once the room that named it is left.

## Reproducing a creep locally

- `tools/rss-sample.sh <pid> [interval] [samples] [outfile]` samples a process's RSS with `ps`,
  and prints the slope in KiB per hour.
- `cargo test -p hs-federation --test sender_soak -- --ignored --nocapture` runs the sender for
  `SOAK_MINUTES` against `SOAK_DESTINATIONS` unresolvable servers and asserts a flat RSS;
  `SOAK_BACKEND=memory` swaps Fjall for the memory backend to tell the two apart.
