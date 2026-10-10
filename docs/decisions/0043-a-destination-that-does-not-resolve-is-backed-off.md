# 0043: a destination that does not resolve is backed off like one that does not answer

- Date: 2026-10-10
- Track: 06 (federation)
- Affects: every caller of `hs_federation::client::FederationClient` (the sender, joins, key
  fetches, backfill, remote media) and what the admin API's destination list shows

## Decision

`FederationClient::send` and `FederationClient::get_media` record a `ClientError::Discovery`
(the destination resolved to no address, or discovery failed outright) in the destination store
exactly as they record a `ClientError::Request`: the destination's failure count grows and
`retry_at` moves out, doubling from a second to `federation.max_retry_backoff` (an hour) with
full jitter, and until then every call to that destination is refused with
`ClientError::Backoff` without touching the resolver.

The sender counts such an attempt as `unresolvable` in `hs_federation_transactions_total`, logs
it at `warn` once per backoff level (the first failure, the first wait of a minute or more, the
first wait at the ceiling) and at `debug` otherwise, and keeps its own persisted, doubling
per-destination backoff as before.

## Why

Before, a `Discovery` error recorded nothing. The sender retried a server whose DNS was gone on
its own schedule from one second (1 s, 2 s, 4 s, ...), each attempt asking the resolver again and
each one a `warn`; every other caller (a join verifying a `send_join` answer, a key fetch) asked
the same dead server as often as its own loop allowed. On 2026-10-10 the demo, in a room naming
hundreds of dead servers, logged 2,790 such warnings in 400 seconds and wrote a retry-state row
per attempt into Fjall, whose memtable kept every version (RFC 0024).

Synapse treats a DNS failure as a `RequestSendFailed` and backs the destination off the same way.

## Consequences

- A destination that does not resolve appears among the failing destinations in the admin API
  (`failing_since`, `retry_at`) and can be reset there, like any other.
- A transient resolver hiccup costs one request a short, jittered wait (one to two seconds the
  first time), which is the same cost a connection reset already had.
- Tests that resolve a destination to nothing and expect a second request to reach the resolver
  must reset the destination store in between, or use a destination store that does not back off.

## Verification

`cargo test -p hs-federation sender::tests::an_unresolvable_destination_backs_off_like_any_other_failure`
and `sender::tests::a_failing_destination_warns_once_per_backoff_level`.
