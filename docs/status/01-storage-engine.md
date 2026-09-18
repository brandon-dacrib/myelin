# 01 Storage engine: status

Track brief: `docs/workstreams/01-storage-engine.md`. Owner crates: `hs-kv`, `hs-tables`,
`hs-search` (not started).

Last updated: 2026-09-18 (session 1, working from a clean restart after a prior attempt was
interrupted before writing any code).

## Done

This session delivered the scoped subset the lead asked for: `hs-kv` trait v0 plus contract, the
in-memory backend and conformance suite, the Fjall backend, `hs-tables`, and `hs-kv` benchmarks.
PostgreSQL backend, `hs-search`, and the store export/import tooling from the full brief are **not**
part of this delivery; see "Next".

- **`crates/hs-kv/src/lib.rs`**: the semantic contract as rustdoc — keyspaces, ordering (byte-wise;
  `hs-tables` makes it typed), snapshots vs. serializable transactions, why SSI (not "snapshot
  isolation") is the guaranteed level, why a read-only transaction never conflicts, size limits
  (`MAX_KEY_BYTES` 64 KiB, `MAX_VALUE_BYTES` 32 MiB, `MAX_TXN_MUTATIONS` 10,000,
  `MAX_TXN_BYTES` 8 MiB), retry rules (only `Conflict` retries, and only by re-running the whole
  closure), and watches as hints, never a fencing mechanism.
- **`crates/hs-kv/src/traits.rs`**: `KvBackend`, `KvRead` (get, `multi_get`, `range`), `KvWrite`
  (put, delete, `atomic_add`), `RangeSpec` (inclusive/exclusive bounds, reverse, limit,
  `RangeSpec::prefix`).
- **`crates/hs-kv/src/retry.rs`**: `transact` / `TransactConfig`, the serializable-transaction
  retry helper with jittered exponential backoff.
- **`crates/hs-kv/src/watch.rs`**: `Hub` / `Watch`, a process-local, self-pruning, best-effort
  change notifier shared by every backend (not backend-specific — see the contract on why).
- **`crates/hs-kv/src/memory.rs`**: `MemoryBackend`, a reference implementation with real SSI
  (read-set and range-read tracking, validated at commit against a global write log) — not just
  last-writer-wins. This is the backend the conformance suite was designed against first, and the
  backend other tracks should use in their own unit tests.
- **`crates/hs-kv/src/conformance.rs`**: `run_conformance_suite(make: impl Fn() -> B)`, covering
  get/put/delete, `multi_get` order, range boundaries (inclusive/exclusive/reverse/limit/past-the-
  end), snapshot repeatable-read isolation, lost update, write skew, phantom reads (both the
  "conflicts" and "does not conflict" cases), `atomic_add` under real thread contention (8 threads
  x 25 increments, exact-count assertion), watches (fires on write, times out otherwise), and
  read-only transactions never conflicting.
- **`crates/hs-kv/src/fjall_backend.rs`**: `FjallBackend` on Fjall 3.1 (`OptimisticTxDatabase`),
  one keyspace per table, LZ4 block compression (the crate's default with the `lz4` feature, which
  is on), key-value separation enabled per keyspace (`KvSeparationOptions::default()`). Passes the
  identical conformance suite (`tests/fjall_conformance.rs`), plus a reopen-after-drop durability
  test (`reopen_after_drop_preserves_committed_data`: put/delete, drop, reopen, verify).
- **`crates/hs-kv/benches/kv_bench.rs`**: Criterion micro-benchmarks for `get`, `multi_get` (100
  keys), a bounded `range` scan (100 rows) and transaction commit, generic over `KvBackend` and run
  against both `memory` and `fjall`. Compiles and runs (`cargo bench -p hs-kv`); not run to
  completion / recorded in this session (time-boxed).
- **`crates/hs-tables/src/key.rs`**: order-preserving tuple key encoding. Fixed-width big-endian
  integers (sign-bit-flipped for signed types) so numeric order holds across byte-length
  boundaries — chosen explicitly over varint, which does not preserve order without extra
  machinery (see the module doc and `key::tests::u32_sorts_numerically_across_byte_length_
  boundaries`). Strings and blobs use FoundationDB-style null-byte escaping
  (`0x00`→`0x00 0xFF`, terminator `0x00 0x00`) so they are both order-preserving and
  self-delimiting inside a tuple. `KeyEncode`/`KeyDecode` are implemented for `u8..u64`, `i16`,
  `i32`, `i64`, `String`/`str`, `Vec<u8>`/`[u8]`, and all six `hs-model` short ID types
  (`RoomSn`, `UserSn`, `ServerSn`, `StateKeyId`, `TypeId`, `EventSn`), and — critically — for tuples
  themselves, component by component, so tuples nest (a composite index key is
  `(IndexKeyTuple, PrimaryKeyTuple)`). `TupleKey` is a blanket-implemented convenience trait giving
  whole-key `.encode()` / `::decode()` (full-consumption, unlike the component methods). Property
  tests (`proptest`) prove encoded order matches natural order for arbitrary strings, `u64` and
  `i64`; deterministic tests cover the byte-length boundary, embedded NUL round-trip, prefix
  ordering, and trailing-byte rejection.
- **`crates/hs-tables/src/keyspace.rs`**: `TypedKeyspace<Ks, K>`, a thin typed wrapper over a raw
  `hs-kv` keyspace handle (get/`multi_get`/put/delete/range, `TypedKeyspace::prefix` for
  "everything under this key prefix"). Values are left as raw bytes deliberately — key encoding is
  this crate's job, value serialization is each table owner's choice.
- **`crates/hs-tables/src/index.rs`**: `IndexDef` (keyspace + uniqueness + a
  `Fn(&PrimaryKey, &[u8]) -> Option<IndexKey>` derive closure) and `maintain_index` (called with
  the row's value before and after a mutation; diffs and applies exactly the needed index writes,
  inside the caller's transaction). Storage scheme: index rows are keyed by the composite
  `(index_key, primary_key)` with an empty value, which unifies unique and non-unique indexes —
  uniqueness is enforced by a prefix scan over `index_key`'s encoding before insert. `lookup` does
  the same prefix scan for reads.
- **`crates/hs-tables/src/migrations.rs`**: `Migration` / `run_migrations`, a version table
  (`_hs_tables_schema_meta`), each migration applied inside its own retried transaction with the
  version bump in the *same* transaction as the migration's writes (atomic: never partially
  applied-but-unrecorded). Rejects out-of-order input; skips already-applied versions; unit-tested
  for ordering, application, and idempotence (re-running does not re-invoke an applied migration's
  closure).
- **`crates/hs-tables/src/interning.rs`**: `InternTable<Ks, Id>` — get-or-create, reverse lookup,
  in-process cache — plus preconfigured constructors for the six `PLAN.md` section 6.1 short IDs
  (`room_sn_table`, `user_sn_table`, `server_sn_table`, `event_sn_table`, `state_key_id_table`,
  `type_id_table`; `state_key_id` interns the caller's encoded `(type, state_key)` tuple bytes).
  The cache is only ever populated from a read that found an *existing* (therefore committed)
  entry, never from the branch that allocates a new id — this is what makes it safe under retry
  (see the module doc's "why the cache never caches an uncommitted allocation"). Tested including a
  genuine 8-thread race interning the same brand-new name concurrently, asserting all eight resolve
  to one id and the counter was not wasted.
- **`crates/hs-tables/tests/index_proptest.rs`**: property test generating random sequences (1-40
  ops, 200 cases) of insert/update/delete against a primary table with one non-unique and one
  unique index, asserting after *every* op that every index row's primary key exists with a value
  that really derives that index key (no orphans), every live row appears in both indexes under its
  current value (no missing entries), and the unique index never has two rows under the same key.
  Plus a deterministic test that a unique index actually rejects a second primary key.

## In progress

Nothing left mid-flight; the session's scoped deliverables are complete and green.

## Next

- PostgreSQL backend (one table per keyspace, `SERIALIZABLE` + retry, pipelined multi-get,
  `LISTEN`/`NOTIFY` watches) — full brief item, not requested this session.
- `hs-search` (`tantivy` per shard) — not started.
- Actually run the Criterion benchmarks to completion and record numbers / a cost model per
  backend (brief's "written cost model... that 04, 05 and 06 use to design access patterns").
- Store export/import tooling, index verifier, crash-kill9 recovery test (the full brief's Phase 0
  items beyond this session's scope).
- `hs-tables`: partial indexes are supported structurally (`derive` returning `None` skips
  indexing) but untested with a dedicated case; multi-valued indexes (one row, several index
  entries) are not supported — `IndexDef::derive` returns `Option<IK>`, not `Vec<IK>`. Add if a
  consuming track needs it.

## Blockers

None.

## Interfaces provided

- `hs-kv` trait v0 (frozen this session, matching the week-2 seam in `docs/workstreams/README.md`):
  `KvBackend`, `KvRead`, `KvWrite`, `RangeSpec`, `transact`, `Hub`/`Watch`. Two backends:
  `hs_kv::memory::MemoryBackend`, `hs_kv::fjall_backend::FjallBackend`.
- `hs-tables`: `key::{KeyEncode, KeyDecode, TupleKey}`, `keyspace::TypedKeyspace`,
  `index::{IndexDef, maintain_index, lookup}`, `migrations::{Migration, run_migrations,
  current_version}`, `interning::{InternTable, ShortId, room_sn_table, user_sn_table,
  server_sn_table, event_sn_table, state_key_id_table, type_id_table}`.
- Both crates are usable today by any other track: `hs_kv::memory::MemoryBackend::new()` needs no
  setup and is the recommended backend for other tracks' own unit tests, per
  `docs/workstreams/README.md` rule 2.

## Decisions made

- **Fixed-width integers, not varint, inside tuple keys.** One of the brief's open questions.
  Varint does not preserve order across a byte-length boundary without extra machinery FoundationDB
  itself needs (a length-prefixed scheme); fixed-width big-endian trivially does, and matches
  `hs-model`'s short IDs, which already expose `to_be_bytes`/`from_be_bytes` for this reason.
  Signed integers flip the sign bit so two's-complement negatives still sort first.
- **Serializable snapshot isolation (SSI) is the one isolation level, on every backend**, not
  "snapshot isolation" (which permits write skew) and not read-committed. This is stronger than
  PostgreSQL's own `SERIALIZABLE` default posture requires callers to opt into, but it's what Fjall's
  optimistic transactions give for free, it's what the in-memory reference was built to prove, and
  it's the simplest contract to explain and to cite from other tracks ("read it inside a
  transaction and the commit fails if it changed" — no separate fencing primitive needed, which is
  exactly what track 03 asked for). Both backends prove it via the shared conformance suite.
- **A transaction with no writes never conflicts, on every backend.** This falls directly out of
  Fjall's own optimistic-transaction implementation (an empty write set skips validation) and the
  in-memory backend was deliberately built to match it, so the contract is backend-independent
  rather than an accident of one implementation.
- **Watches are implemented once, in `hs-kv` itself, not per backend.** Fjall 3 has no built-in
  watch/subscribe API; rather than build one twice (once for Fjall, differently for a future
  PostgreSQL `LISTEN`/`NOTIFY` backend) with subtly different semantics, `hs-kv` owns a single
  process-local hint mechanism every backend's write path notifies into. This is also *why* the
  contract insists watches are hints only: a cross-backend, cross-process guarantee was never in
  scope.
- **Values are always raw bytes at both the `hs-kv` and `hs-tables` layers.** `hs-tables` encodes
  keys; it does not prescribe a value codec (JSON, canonical event bytes, protobuf...). Callers pick
  their own.
- **Index storage unifies unique and non-unique indexes** as `(index_key, primary_key) -> b""`
  rather than two different schemes. Uniqueness is enforced by a prefix scan at write time, not by
  a separate value slot.
- **`hs-kv`'s size limits are enforced, not just documented**: `MAX_KEY_BYTES` (64 KiB, Fjall's own
  limit), `MAX_VALUE_BYTES` (32 MiB), `MAX_TXN_MUTATIONS` (10,000) and `MAX_TXN_BYTES` (8 MiB) all
  return a typed error rather than being taste. These are defaults every backend shares; nothing in
  the trait stops a future backend from having a stricter effective limit, but nothing should be
  laxer without an RFC, since other tracks (04 especially, batching event persistence) will design
  against these numbers.
- **`cargo fmt` was run scoped to this track's crates (`cargo fmt -p hs-kv`, `cargo fmt -p
  hs-tables`), not `cargo fmt --all`**, to avoid reformatting other tracks' in-flight files during
  concurrent work. Worth flagging to the integration lead in case a workspace-wide format pass is
  wanted at a checkpoint.

- **`hs_kv::conformance` uses `unwrap`/`expect` freely outside `#[cfg(test)]`**, which reads as a
  quality-bar exception (`docs/decisions/0002-workspace-conventions.md`: "no `unwrap` ... outside
  tests"). It is deliberate: the module's only purpose is to be called from other crates' `#[test]`
  functions (this crate's own and, per `docs/workstreams/README.md` rule 2, every other track's), so
  it is test code in substance even though it cannot be `#[cfg(test)]`-gated without also hiding it
  from consumers. Panicking on an unexpected `Err` is the correct behavior there, identical to what
  `assert_eq!` already does throughout it.

## Shared dependencies added

None beyond what the previous attempt already added to `[workspace.dependencies]` (`fjall`,
`tokio-postgres`, `deadpool-postgres`, `tempfile`) — this session used only those, plus already-
present `bytes`, `thiserror`, `tracing`, `criterion`, `proptest`. `hs-tables` added ordinary path
dependencies on `hs-kv` and `hs-model` (not workspace-level, since they're in-tree crates).
