# RFC 0021: The PostgreSQL backend's commit should flush its buffered writes in bulk

Status: **accepted and implemented** (track 01, branch `agent/postgres-bulk-flush`, 2026-10-04).
Proposed 2026-10-02 (track 05 asks track 01).

## What landed (2026-10-04)

`postgres_backend::flush_pending` groups the transaction's pending writes by table (in table
name order, so transactions that write the same tables take their row locks in the same order)
and sends, per table, one prepared multi-row upsert for the puts and one multi-key delete for the
deletes, each in chunks of at most `FLUSH_CHUNK_ROWS` (2,000) rows:

```sql
INSERT INTO <table> (k, v) SELECT u.k, u.v FROM unnest($1::bytea[], $2::bytea[]) AS u(k, v)
  ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v;
DELETE FROM <table> WHERE k = ANY($1::bytea[]);
```

Nothing else changed: `SERIALIZABLE`, `is_serialization_conflict`, the write budget, the watch
notifications and the one-isolated-trip commit are as they were, and a failed statement still
rolls the transaction back before the error is reported.

**Measured** (`crates/hs-kv/tests/postgres_bulk_flush.rs::fan_out_shaped_commit_timing`: the
shape above, a batch of 100 members, 300 puts over `hs_user.feed`, `hs_user.feed_by_room` and
`hs_user.feed_heads`, one commit, 50 rounds, PostgreSQL 17 in Docker on the owner's desktop, the
old and the new flush as two test binaries run alternately under the same load):

| flush | statements per commit | median per commit | mean per commit |
|---|---|---|---|
| before, one statement per write (load average ~4) | 300 | 77-79 ms | 78-79 ms |
| before, same, while other agents built (load 9-21) | 300 | 87-274 ms | 98-293 ms |
| after, bulk (load 9-21, interleaved with the row above) | 3 | 3.2-7.3 ms | 3.5-8.5 ms |

So a 300-put commit is some 20-25 times quicker, and a 300-member fan-out (three batches) is
about 10-20 ms of store time instead of a quarter second. The unnamed-thread-per-call cost named
below as second-order stays second-order: a flush is now one `run_isolated` trip whatever its
size.

**Observable:** each flush with writes is a `debug` line under `hs_kv::postgres` (`puts`,
`deletes`, `tables`, `statements`, `elapsed_us`); `PostgresBackend::flush_stats()` counts
flushes, writes and statements per backend; `hs_kv::metrics::register_metrics` puts
`hs_kv_postgres_flush_writes` (histogram), `hs_kv_postgres_flush_duration_seconds` (histogram)
and `hs_kv_postgres_flush_statements_total` on a registry. `hs-cli` has to call
`metrics.with_registry(hs_kv::metrics::register_metrics)` next to the other crates' registrations
in `serve.rs` for them to appear on `/metrics`; that one line is outside track 01's crates and
is left for the coordinator or track 12.

**Tests:** `postgres_bulk_flush.rs` (a commit larger than a chunk with puts and deletes to the
same table, put-then-delete and delete-then-put of one key, the exact statement count; an empty
commit; a flush failed by a check constraint rolls back and the next commit succeeds; the timing
above) and the whole `hs-kv` suite including the PostgreSQL conformance breakdown and
`postgres_tls`, all against PostgreSQL 17.


## What track 05 needs

A room update's fan-out is now one store transaction per batch of up to 100 members
(decision 0026, `hs_user::store::UserStore::apply_fan_out`): the reads of a batch are four
multi-gets, each one `SELECT ... WHERE k = ANY($1)`, and its writes are two or three puts per
member -- the feed row, the `feed_by_room` pointer when a row is added, and the user's feed head.
On PostgreSQL that leaves the writes as the whole cost: `postgres_backend::flush_pending` runs one
`INSERT ... ON CONFLICT DO UPDATE` (or one `DELETE`) per buffered write, each a server round trip
on the transaction's connection, so a 300-member update is still some 600-900 statements before
`COMMIT`. In-memory and on Fjall the same transaction is a few hundred microseconds
(`docs/status/05-sync.md`, session 14).

## Proposal

In `crates/hs-kv/src/postgres_backend.rs`, `flush_pending` groups the pending writes by table and
issues, per table, one multi-row upsert and one multi-key delete:

```sql
INSERT INTO <table> (k, v) SELECT * FROM unnest($1::bytea[], $2::bytea[])
  ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v;
DELETE FROM <table> WHERE k = ANY($1::bytea[]);
```

(`unnest` of two arrays rather than a `VALUES` list keeps the statement at two parameters
whatever the batch size; PostgreSQL's parameter limit is 65,535.) Chunking at a few thousand rows
per statement keeps a pathological transaction from building one enormous array. The ordering
within a transaction does not matter: `pending` is a map keyed by `(table, key)`, so each key is
written once, and the upsert and the delete touch disjoint keys.

Nothing else changes: the conflict model (`SERIALIZABLE`, `is_serialization_conflict`), the write
budget (`limits`), the watch notifications after commit and the one-isolated-trip rule
(`run_isolated`) all stay as they are. The conformance suite (`hs_kv::conformance`) already covers
puts, deletes and overwrites within one transaction, so it checks the change.

While there: `PgTxn::with_conn` spawns a fresh OS thread per call (`run_isolated`), including for
every single `get` and `range` inside a transaction. That is correct and the reason given in the
module docs stands, but a transaction that does many reads pays a thread spawn each; a
transaction-scoped worker thread fed through a channel would cost one spawn per transaction.
This is a second-order cost next to the per-write statements and is mentioned, not asked for.

## Expected effect

A 300-member fan-out on PostgreSQL goes from ~900 statements to about 10 (four multi-gets, three
tables' upserts, `BEGIN`, `COMMIT`), which on a local PostgreSQL is a few milliseconds instead of
a few hundred. The owner's wake latency in the brief's 303-member room (8 s before decision
0026, status 05 session 12) is then the room's own cost, not the feed's.

## What track 05 does meanwhile

Nothing is blocked. The batched fan-out already removes the per-member transactions and
snapshots (two per member) and the per-member reads; the remaining per-write statements are what
this RFC is about. `docs/status/05-sync.md` session 14 has the measurement.
