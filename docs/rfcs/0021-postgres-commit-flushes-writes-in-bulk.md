# RFC 0021: The PostgreSQL backend's commit should flush its buffered writes in bulk

Status: proposed (track 05 asks track 01). Date: 2026-10-02.

## What track 05 needs

A room update's fan-out is now one store transaction per batch of up to 100 members
(decision 0025, `hs_user::store::UserStore::apply_fan_out`): the reads of a batch are four
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
0025, status 05 session 12) is then the room's own cost, not the feed's.

## What track 05 does meanwhile

Nothing is blocked. The batched fan-out already removes the per-member transactions and
snapshots (two per member) and the per-member reads; the remaining per-write statements are what
this RFC is about. `docs/status/05-sync.md` session 14 has the measurement.
