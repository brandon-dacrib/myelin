# 01 Storage engine: status

> **Integration note, 2026-09-19 (integration lead): the Postgres backend cannot serve yet, and
> the conformance suite could not have told us.** Wiring it into `hs serve` and booting against a
> real PostgreSQL 17 panics immediately: `Cannot start a runtime from within a runtime`
> (`postgres-0.19.14/src/connection.rs:66`). The synchronous `postgres` client drives its own
> internal Tokio runtime with `block_on`, which panics on any thread that already has one — and
> `hs serve` is async, so this hits at open and would hit on every storage call thereafter. Every
> conformance test passes because they are plain `#[test]` functions with no ambient runtime; this
> failure only exists once something async opens the backend, which is the only way it will ever
> be used in production.
>
> `crates/hs-cli/src/storage.rs` now detects an ambient runtime and refuses with an explanatory
> error rather than panicking, and the rest of the wiring (config → DSN, the `OpenedStorage`
> variant, `spawn_serve` generic over the backend, TLS refused rather than ignored) is in place
> and tested, so the only thing between this and a working multi-node deployment is the client
> strategy. The fix is for `PostgresBackend` to own its execution: dispatch every operation onto
> its own dedicated thread(s) with no ambient runtime, or move to `tokio-postgres` driven on a
> runtime handle the backend controls. Whichever is chosen, the acceptance test is booting
> `hs serve` with `storage.backend: postgres` and registering a user — not the conformance suite.
>
> **RESOLVED, session 3 (same day): `PostgresBackend` now isolates every call onto a freshly
> spawned OS thread — see "Session 3" below.** Every operation, including `open` and both
> transaction types' `Drop`-time rollback, now runs on a thread with no ambient Tokio context, so
> the backend tolerates being opened and called from inside a runtime. **Action needed from the
> integration lead**: delete `StorageOpenError::PostgresInsideRuntime` and its guard in
> `crates/hs-cli/src/storage.rs` — they are no longer needed and now stand in the way of a real
> deployment. This track could not make that edit (`hs-cli` is out of scope for track 01).
>
> **Integration note, 2026-09-19 (integration lead, follow-up): guard removed, `hs serve` no
> longer panics, but it fails one step later** — `storage backend error: invalid keyspace name
> "hs_auth.users"`. Every consuming crate names its keyspaces with a dotted crate-scoped prefix
> (`hs_auth.users`, `hs_room.*`, `hs_e2e.*`, `hs_push.*`, ...); this backend's identifier
> validation rejected the dot, which the in-memory and Fjall backends never did. **RESOLVED, same
> day: see "Dotted keyspace names" under "Session 3" below.**
>
> **Integration note, 2026-09-19 (integration lead, second follow-up): dotted names fixed, then two more real bugs found by running two `hs serve` processes against one database for the
> first time.** (1) Starting two replicas *simultaneously* against an empty database killed
> one of them at boot (`backend error: db error`) — staggered starts never showed it, so this
> was concurrent schema/table setup, not a logic bug. (2) That error message itself carried no
> SQLSTATE, no message, nothing an operator could act on. **RESOLVED, same day: see "Concurrent
> setup and honest errors" under "Session 3" below.**


Track brief: `docs/workstreams/01-storage-engine.md`. Owner crates: `hs-kv`, `hs-tables`,
`hs-search` (not started).

Last updated: 2026-09-19 (session 3: `PostgresBackend` execution-model fix, plus a follow-up
dotted-keyspace-name fix found by actually booting `hs serve` — see "Session 3:
`PostgresBackend` can now be called from inside a Tokio runtime" below). Session 2 delivered the
PostgreSQL `KvBackend` itself, closing the `docs/next-steps.md` gap "PostgreSQL and SlateDB
backends absent — only the embedded backend can actually open". SlateDB is **still absent** — out
of scope for this session by explicit instruction; see "Next".

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

## Session 2: PostgreSQL `KvBackend`

**Scope note:** the lead asked for PostgreSQL only. **SlateDB stays absent** — not started, not
attempted, no partial work; both backends were explicitly not to be started at once.

### Done

- **`crates/hs-kv/src/postgres_backend.rs`**: `PostgresBackend`, a full `KvBackend` implementation
  against real PostgreSQL. Client library: the **synchronous `postgres` crate** (the same
  `rust-postgres` project as `tokio-postgres`, wrapping the identical protocol code behind a
  blocking facade with a hidden per-connection runtime), pooled with **`r2d2`** via
  `r2d2_postgres`. Chosen over driving `tokio-postgres`/`deadpool-postgres` (already in
  `[workspace.dependencies]` from a previous session, still unused by this backend) directly
  because `KvBackend` is a synchronous trait, matching the embedded Fjall backend, which is also
  blocking; bridging an async client into every trait method would mean either `block_on`-ing from
  inside a caller that might already be on a Tokio worker thread (a documented panic:
  "Cannot start a runtime from within a runtime") or building a spawn-and-channel-back shim. The
  sync `postgres` crate needs none of that. Callers on an async runtime are expected to run
  `hs-kv` calls through `tokio::task::spawn_blocking`, exactly as they already must for the Fjall
  backend.
  - **Table shape**: one table per keyspace (the brief's own recommendation, for `VACUUM`
    locality), `"{schema}"."kv_{keyspace}"`, each `(k bytea primary key, v bytea not null)`. All of
    one `PostgresBackend`'s tables live in one PostgreSQL schema (`CREATE SCHEMA IF NOT EXISTS`),
    default `"public"` in production, a fresh randomly-named schema per test run.
  - **Ordering / range scans**: `bytea`'s default comparison is byte-wise, matching the trait's
    ordering contract exactly. `RangeSpec` compiles to `WHERE k >= / > / <= / < $n`, `ORDER BY k
    ASC`/`DESC`, `LIMIT`. Range scans **materialize the whole result eagerly** rather than
    streaming (the crate's streaming API needs async; see "What is slower" below).
  - **Transactions**: `SERIALIZABLE` for read-write, `REPEATABLE READ READ ONLY` for snapshots
    (PostgreSQL's own snapshot isolation, a correct match for the trait's "fixed at the instant
    taken, unaffected by later commits" snapshot contract). **Writes are buffered client-side and
    flushed only at commit** — see the incident below for why this is load-bearing, not an
    optimization.
  - **Watches**: the same in-process `hs_kv::watch::Hub` every backend uses, not `LISTEN`/`NOTIFY`
    (a decision already recorded last session for Fjall; reaffirmed here rather than building a
    second, differently-shaped mechanism for one backend). Still process-local, still hints only.
  - **New `KvError::MidTransactionConflict`** (`error.rs`) and `transact()` retry-loop support
    (`retry.rs`): PostgreSQL's SSI can report a serialization failure (`40001`), deadlock
    (`40P01`), or lock-timeout (`55P03`, see below) on **any** statement, not only `COMMIT`, unlike
    the in-memory/Fjall backends. This is a purely additive change to the `KvError` enum (checked:
    no crate in the workspace exhaustively matches on `KvError` without a wildcard arm, so this
    cannot have broken anyone) and to `transact()`'s retry loop (which now also retries on this
    variant, identically to a commit `Conflict`). No existing backend can produce the new variant.
  - **`WRITE_LOCK_TIMEOUT` (200ms)**: `SET LOCAL lock_timeout` on every read-write transaction, so
    a write that would otherwise block on another open transaction's row lock fails fast
    (`55P03`, mapped to `MidTransactionConflict`) instead of blocking indefinitely. Necessary, not
    cosmetic — see the incident below.
- **The incident that shaped the design** (worth recording in full, because it is the reason the
  implementation looks like it does, not a cosmetic choice): the first working version issued each
  `put`/`delete` as its own `INSERT ... ON CONFLICT DO UPDATE` / `DELETE`, immediately, inside the
  open transaction — the naive, obvious SQL translation. Run against a real `postgres:17` container
  for the first time, `cargo test -p hs-kv --test postgres_conformance` **hung forever**. Diagnosis
  (via `sample` on the stuck process plus `pg_stat_activity`) found the shared conformance suite's
  `lost_update_is_prevented` scenario is the cause: it opens two transactions, has both read the
  same key, then writes it from *both* before committing *either*, on one thread. PostgreSQL's row
  lock for the second write genuinely blocks — server-side, correctly — waiting for the first
  transaction to end, but nothing else was ever going to end it: the only thread that could call
  `commit()` was the one parked inside the second `put()`. This is not a bug in that test (the
  in-memory and Fjall backends pass it trivially, because their "transactions" never touch shared
  state before commit) and it is not fixable by tuning: **any** backend that takes a real row lock
  at write time, not commit time, will deadlock on this exact program shape. The fix was to change
  the architecture, not patch around it: `put`/`delete` now only buffer the mutation in an
  in-process map (checked by that transaction's own subsequent reads, so it still reads its own
  writes) and the buffer is flushed as a single burst of statements immediately before `COMMIT`.
  This matches what "optimistic transaction" already means for the in-memory and Fjall backends
  and is the reason `lost_update_is_prevented` and `write_skew_is_prevented` now pass. `git blame`
  will show both versions were written and tested in this session; this is not a hypothetical risk,
  it happened, cost real debugging time, and is recorded here so nobody reintroduces it.
- **`crates/hs-kv/src/conformance.rs`**: the eleven scenario functions inside
  `run_conformance_suite` are now individually `pub fn` (previously private), purely additively —
  `run_conformance_suite` itself, and every existing call site (`memory_conformance.rs`,
  `fjall_conformance.rs`), is untouched. This lets a backend with a known, documented divergence
  from one scenario report an honest per-scenario breakdown instead of an all-or-nothing result
  that stops at the first failure — exactly the PostgreSQL backend's situation, below.
- **`crates/hs-kv/tests/postgres_conformance.rs`**: the PostgreSQL backend's test suite.
  - `postgres_backend_conformance_breakdown`: runs all eleven scenarios individually (each against
    its own fresh, randomly-named schema), reports which passed/failed, and asserts that the *only*
    failures are the two documented divergences below (loudly fails on any other regression, and
    loudly fails if either documented divergence unexpectedly starts passing, as a prompt to update
    this file).
  - `reopen_against_the_same_schema_preserves_committed_data`: a durability check analogous to
    Fjall's `reopen_after_drop_preserves_committed_data` — commit data, drop every in-process handle
    including the connection pool, open a *new* `PostgresBackend` against the *same* schema, read
    it back. (For a real server this is a weaker test than Fjall's kill/reopen — the data was never
    at risk of not surviving a clean process exit; it is closer to proving the schema/table
    plumbing round-trips correctly across a fresh pool than proving crash durability, which is
    PostgreSQL's own WAL's job, not this crate's.)
  - Gating: `reachable_dsn()` tries to open a probe backend against `HS_KV_TEST_POSTGRES_DSN`
    (default `postgres://postgres:hskvtest@localhost:5433/postgres`) with a 3-second connection
    timeout (see below) and prints a clear `SKIP:` message plus the exact `docker run` command to
    stderr, then returns cleanly, if it cannot connect — both tests return immediately, green, in
    that case. Confirmed both directions: `cargo test -p hs-kv --test postgres_conformance` is
    green in ~3s with no database running, and green in ~4s with the database running.
  - `PostgresBackend::open`'s connection pool now uses a 3-second `connection_timeout`, down from
    `r2d2`'s 30-second default — discovered while building this gate: the "no database" skip path
    took 30 seconds before this change, which is a bad experience for every `cargo test` run on a
    machine without Docker. This is a real behavior change for production too (a caller that can't
    reach PostgreSQL at all now fails fast rather than hanging half a minute); resilience against a
    slow-starting database is a higher-level concern (readiness probes, `hs serve`'s own startup
    retry), not this pool's job.

### PostgreSQL conformance run (exact commands and honest results)

```sh
docker run --rm -d --name hs-kv-pg-test -e POSTGRES_PASSWORD=hskvtest -p 5433:5432 postgres:17
# wait for it to accept connections (`docker exec hs-kv-pg-test pg_isready -U postgres`), then:
cargo test -p hs-kv --test postgres_conformance -- --nocapture
docker rm -f hs-kv-pg-test
```

(`HS_KV_TEST_POSTGRES_DSN` overrides the DSN if you're not using port 5433 / that password.)

**Result, run repeatedly (3+ times) for stability: `reopen_against_the_same_schema_preserves_committed_data`
passes every time; `postgres_backend_conformance_breakdown` passes every time, reporting 9/11
scenarios passed outright and exactly 2 documented, understood divergences, never anything else:**

1. **`phantom_insert_inside_a_scanned_range_conflicts` — fails every time, for a structural
   reason, not a bug.** This scenario asserts a guarantee *stronger* than true serializability:
   that any write into a key range a transaction scanned conflicts, full stop. The in-memory and
   Fjall backends provide exactly this (by construction: they conservatively treat any touch of a
   scanned range as a conflict). Real PostgreSQL SSI implements textbook academic serializability:
   a conflict requires an actual rw-antidependency *cycle* among concurrent transactions. The
   scenario's schedule (reader scans a range and separately writes an unrelated key; writer inserts
   into that range and commits; reader then commits) has exactly one dependency edge, not a cycle —
   it is genuinely, provably serializable (equivalent to running reader-then-writer), and
   PostgreSQL correctly does not abort either transaction. This over-conservative guarantee is
   almost certainly not achievable by *any* correct implementation sharing one PostgreSQL database
   across multiple processes/replicas without much heavier machinery (predicate locks are
   PostgreSQL's own mechanism for exactly this, and they already decline to flag this case) — it is
   fundamentally a single-process-only guarantee the in-memory/Fjall backends get for free from
   being one process with one global lock. **Track 03 or anyone else relying on the crate's phantom
   guarantee for a range scan (not a point read) on the PostgreSQL backend should re-read this
   before assuming it holds.** Point-read fencing (read one key's value/epoch, write, commit —
   track 03's actual lease/fencing pattern) is unaffected: that is a two-way rw/wr edge that *does*
   form a genuine cycle when it matters, which PostgreSQL's SSI catches correctly (proven by
   `lost_update_is_prevented` and `write_skew_is_prevented`, both passing).
2. **`atomic_add_under_contention` — fails intermittently (roughly half of runs, 1-5 of 200 total
   increments), not a bug, a latency/tuning fact.** This scenario hammers one row from 8 threads
   with no mercy. Measured single-threaded, uncontended `transact` round-trip latency against this
   container: **~5.3ms per commit** (20 sequential commits, 106.7ms total; measured over
   Docker Desktop's port-forwarded loopback on this machine, so a bare-metal or same-pod production
   deployment should do better, not worse). `hs_kv::transact`'s default `TransactConfig` (10
   attempts, 100ms max backoff) has enormous headroom against the in-memory/Fjall backends, whose
   attempts cost microseconds; against a real network round trip, 10 attempts covers a much smaller
   wall-clock window, and under this scenario's deliberately worst-case single-row contention, that
   window is occasionally not enough. **No data is ever lost, corrupted, or double-applied** — the
   operation cleanly returns `KvError::RetriesExhausted` rather than doing anything incorrect.
   Widening `WRITE_LOCK_TIMEOUT` from 200ms to 2s (tested) did **not** fix it, which rules out
   lock-wait timeouts as the cause and confirms it is genuine SSI serialization-failure contention,
   not a tunable knob in this backend — the fix, if a caller needs one, is a larger `TransactConfig`
   for a known-hot key, exactly as that type's own docs already invite ("a background job doing
   bulk work may want a larger `max_attempts`"). **Recommendation for consumers of this backend**:
   any code that increments a single hot counter under real concurrent write load (a room's
   `event_sn`, say) should pass a `TransactConfig` with a larger `max_attempts` and/or
   `max_backoff` than the default when targeting the PostgreSQL backend specifically.

Both divergences are asserted *by name* in `postgres_backend_conformance_breakdown` (one as
"must always fail", one as "may flakily fail, never counted as unexpected") — the test fails
loudly if any *other* scenario ever fails, or if the first divergence ever stops failing.

### What is slower or different (performance note)

- **Every operation is a network round trip** (or several): `begin` = 1 (`BEGIN...; SET
  LOCAL...`), each real read = 1, `commit` = (number of distinct keys written) + 1. Measured
  ~5.3ms per single-key, uncontended `transact` cycle against a local Docker container — compare
  to the in-memory backend's sub-microsecond operations. This is the dominant cost of this backend
  and is inherent to using a real, possibly-remote database rather than an embedded one; it is not
  a benchmark this session ran to completion in a controlled, reproducible way (Criterion), just an
  honest wall-clock measurement, flagged as such rather than presented as more precise than it is.
- **`flush_pending` issues one SQL statement per distinct key written**, not a single batched
  multi-row upsert. Fine within the crate's own `MAX_TXN_MUTATIONS` (10,000) limit for correctness,
  but a real cost for large transactions; a `VALUES (...),(...),(...) ON CONFLICT` or `UNNEST`-based
  bulk upsert would cut this to one or two round trips regardless of key count and is the obvious
  next optimization if a consuming track's write pattern needs it (04's batched event persistence,
  most likely).
- **Range scans materialize the entire result set into memory** before returning (see the module
  docs for why: the crate's streaming query API needs async, and a blocking cursor held open across
  the caller's iteration would extend the transaction, violating this crate's own "keep
  transactions short" rule). Fine for the bounded, `RangeSpec::limit`-ed scans the contract already
  recommends; a caller doing a genuinely large unbounded scan on this backend will hold much more
  in memory at once than the same scan against Fjall or the in-memory backend.
- **Writes never touch the database until commit** (see the incident above) — this is a
  correctness fix, but it also means a transaction with many writes does zero work against
  PostgreSQL until the very end, then a burst; contrast with Fjall, which writes into its own
  local write-set immediately too (so behaviorally similar), versus a hypothetical "eager" SQL
  backend that would spread real I/O across the transaction's lifetime instead of bursting it at
  the end.
- **PostgreSQL's SSI can abort *any* statement, not only commit** (see `KvError::MidTransactionConflict`)
  — a real behavioral surface the in-memory and Fjall backends do not have, requiring the addition
  documented above. Any code calling `begin()`/`commit()` directly instead of `transact()` on this
  backend must handle it.
- **No TLS support yet.** `PostgresBackend::open` connects with `NoTls` unconditionally; there is
  no code path for `hs_config::PostgresStorageConfig::tls`. See "Wiring the integration lead must
  add" below — this needs to be surfaced as an explicit error, not silently ignored, until TLS
  support is added to this backend.

### Wiring the integration lead must add

`crates/hs-cli/src/storage.rs` (not edited — the integration lead's file this session) needs:

1. Add a `Postgres` variant to `OpenedStorage`:
   ```rust
   pub enum OpenedStorage {
       Embedded(hs_kv::fjall_backend::FjallBackend),
       Postgres(hs_kv::postgres_backend::PostgresBackend),
   }
   ```
   (and the matching arm in its `Debug` impl).
2. In `open_storage`, replace the `StorageConfig::Postgres(_) => Err(BackendNotImplemented {backend: "postgres"})`
   arm with something like:
   ```rust
   hs_config::StorageConfig::Postgres(pg) => open_postgres(pg).map(OpenedStorage::Postgres),
   ```
   with `open_postgres` building a DSN from the config fields that already exist on
   `hs_config::storage::PostgresStorageConfig` — `host`, `port`, `database`, `user`, `password`
   (a `SecretString`; already resolved from `password_file` by the time validation/loading is
   done, per `StorageConfig::resolve_secrets`; read it with `pg.password.as_str()`) — e.g.
   `format!("postgres://{user}:{password}@{host}:{port}/{database}")` (URL-encode user/password/
   database if they can contain reserved characters; this track's backend does not do that
   encoding for you, `postgres::Config`'s parser expects a valid DSN).
3. **`pg.tls` has no effect on `hs_kv::postgres_backend::PostgresBackend::open`, which is
   `NoTls`-only.** If `pg.tls` is `true`, `open_postgres` should return a clear
   `StorageOpenError` (a new variant, e.g. `TlsNotImplemented`) rather than silently connecting
   without TLS despite the operator asking for it. Wiring TLS into this backend (swapping
   `postgres::NoTls` for `postgres_native_tls` or `postgres_rustls`, wiring `PgManager`'s type
   parameter accordingly) is future work on this track, not blocking this wiring.
4. **`PostgresStorageConfig` has no `schema` field.** `PostgresBackend::open(dsn, schema)` takes a
   PostgreSQL schema name to scope its tables under; every field needed for the DSN already exists
   (see point 2), but there is nothing to pass as `schema` beyond a hardcoded default. Recommend
   `open_postgres` passes `"public"` for now (PostgreSQL's own default schema, and correct for a
   single homeserver instance owning its whole database) — a `schema` config field is only needed
   if a future requirement wants several `hs` instances or environments sharing one physical
   PostgreSQL database, and can be added to `PostgresStorageConfig` by track 12/13 later without
   any change to this crate (`PostgresBackend::open` already takes it as a parameter).
5. `pool_size` (already on `PostgresStorageConfig`, default 10) is **not yet wired**:
   `PostgresBackend::open` hardcodes `r2d2::Pool::builder().max_size(16)`. A trivial follow-up: add
   a `max_size` parameter (or a small `PostgresOptions` struct) to `PostgresBackend::open` so
   `open_postgres` can pass `pg.pool_size` through. Not done this session because it touches the
   public constructor signature and the brief scoped this session to "PostgreSQL only, get it
   correct and tested" rather than every config knob; flagging it explicitly rather than silently
   ignoring `pool_size`.

## Session 3: `PostgresBackend` can now be called from inside a Tokio runtime

**Scope**: fix exactly the bug in the integration note at the top of this file — `hs-cli` and
every other crate were explicitly out of bounds this session; only `crates/hs-kv` and this status
file were touched.

### The bug, precisely

The synchronous `postgres` crate is not "blocking" in the ordinary sense: every method that
actually talks to the server (`Client::connect` — reached via `r2d2`'s manager whenever the pool
dials a new connection —, `batch_execute`, `query`, `query_opt`, `execute`, and even
`Client::drop` itself) spins up a hidden Tokio runtime on first use and drives it with
`Runtime::block_on`. Tokio detects an already-active runtime on the *calling thread* via a
thread-local and panics ("Cannot start a runtime from within a runtime") rather than nesting. Since
`hs serve` opens storage from an async fn and every later request handler that touches storage runs
as a Tokio task, this was not an "opening" problem to special-case — it was every single call, on
whatever thread Tokio happened to schedule that task on, and additionally on **`Drop`**, since both
`PgTxn` and `PgSnapshot` issue a real `ROLLBACK` when dropped, and dropping the last
`PostgresBackend` handle drops the whole `r2d2::Pool`, which drops every pooled `postgres::Client`,
whose own `Drop` impl also makes a real blocking call. All four of these — explicit calls, the two
`Drop` impls, and pool teardown — had to be fixed; fixing only the obviously-named methods left the
last one as a live panic that a naive "wrap `open` in `spawn_blocking`" fix would have missed
entirely (confirmed: it reproduced on `PostgresBackend` drop specifically, not on any explicit
method call, the first time the regression test below was run).

### The fix: `run_isolated`, a fresh bare OS thread per call

Considered both designs the assignment offered:

1. **Chosen**: keep the synchronous `postgres` client, but ensure no method ever calls into it on
   the thread the caller invoked it on. `run_isolated` (`postgres_backend.rs`) spawns a **brand
   new OS thread via `std::thread::scope`** for every single touch point that reaches
   `postgres`/`r2d2`, runs the work there, and blocks the calling thread on the join. A freshly
   spawned OS thread has its own thread-local storage from scratch — it cannot be "already inside a
   runtime" regardless of what thread asked for the work, including a Tokio worker thread, `hs
   serve`'s main thread, or a plain `#[test]`'s single thread.
2. **Rejected**: driving `tokio-postgres` on a runtime this backend owns. Would work, but means
   this backend managing its own runtime's lifecycle while still presenting a synchronous
   `KvBackend` to the rest of the workspace — real, ongoing complexity, for the same outcome.

**Why a thread spawned fresh per call, not a persistent worker pool (the other credible design
within option 1).** The dominant cost of every operation this backend performs is already a
network round trip — measured last session at ~5.3ms per uncontended `transact` cycle against a
local Docker container. An OS thread spawn costs tens of microseconds: three orders of magnitude
smaller, i.e. noise against a cost this backend pays regardless of execution strategy. A persistent
pool would need its own lifecycle (started in `open`, shut down cleanly, a panicked worker's
failure propagated back to callers instead of quietly wedging the pool) for a savings this
backend's own numbers say doesn't matter. `std::thread::scope` also means `run_isolated` can borrow
non-`'static` data (a `&mut postgres::Client` living on the caller's stack) directly, with no
channel plumbing or `Arc`-wrapping needed to satisfy a `'static` bound that a persistent pool fed
through a channel would require. Full rationale in `postgres_backend`'s module docs ("Execution
model"), including how to swap to a persistent pool later if this ever needs revisiting (localized:
every call site already goes through `run_isolated`).

Every touch point now goes through it: `PostgresBackend::open`, `drop_schema_for_test`,
`keyspace`, `snapshot`, `begin`, `commit`; `PgTxn::with_conn` (which also moved the
conflict-triggered `ROLLBACK` inside the same isolated call, since it's a `postgres` call too);
`PgSnapshot`'s new `with_conn` helper (`get`/`multi_get`/`range` now share it, replacing three
near-identical `match &mut *state` blocks); and both types' `Drop` impls.

**The pool-teardown case needed one more change.** `Inner::pool` changed from a plain `PgPool`
field to `Option<PgPool>` so `Inner`'s own new `Drop` impl can `.take()` it and drop that value
inside `run_isolated`, instead of letting Rust's ordinary field-drop order tear down the pool (and
every idle pooled `postgres::Client`) on whatever thread drops the last `PostgresBackend` handle.
`hs-kv` has `#![forbid(unsafe_code)]`, so `ManuallyDrop::take` (the usual tool for this) was not
available; `Option::take` is the safe equivalent and was chosen over it for exactly that reason.
Every other method that reads `self.inner.pool` now does
`.as_ref().expect("PostgresBackend used after Inner was dropped, which cannot happen: ...")` —
consistent with the existing "used after commit" invariant style already in this file
(`PgTxn::with_conn`) — since the field is `Some` for the entire lifetime of every live
`PostgresBackend` handle and only becomes `None` inside `Inner::drop` itself.

**Public API impact: none.** `KvBackend`/`KvRead`/`KvWrite` are unchanged — every method is still a
plain, synchronous `fn`. A caller on a Tokio runtime is still free to (and, to avoid blocking one of
its own worker threads for a call's duration, may still want to) run `hs-kv` calls through
`tokio::task::spawn_blocking`, but it is no longer *required* to for correctness.

### The regression test the conformance suite could not have provided

`crates/hs-kv/tests/postgres_conformance.rs` gained two tests sharing one body
(`round_trip_body`: open, `transact` a `put`, read it back through a `snapshot`, drop everything —
every step a real blocking call):

- `postgres_round_trip_from_a_plain_test_with_no_ambient_runtime` (plain `#[test]`) — the control
  case, always passed, kept so both contexts are pinned down side by side.
- `postgres_survives_being_opened_and_called_from_inside_a_tokio_runtime`
  (`#[tokio::test(flavor = "multi_thread", worker_threads = 4)]`) — the regression case. Calls the
  backend directly, no `spawn_blocking`, from a genuine Tokio worker thread — the exact shape `hs
  serve` uses. **This test is what actually found the pool-teardown case above**, and is recorded
  as the real debugging sequence in case it recurs: after isolating every explicit `postgres`/`r2d2`
  call site (`with_conn`, `open`, `begin`, `commit`, etc.) but before also isolating pool teardown,
  this test still reliably panicked with the same "Cannot start a runtime from within a runtime" —
  not from any explicit call, but from inside `reachable_dsn`'s probe backend being dropped at the
  end of the function, which tears down the connection pool and, with it, every pooled
  `postgres::Client`. Isolating the explicit call sites alone was not sufficient; the `Inner::drop`
  fix above was needed too, and this test is what proved it.

Exact output, confirming both directions and the full crate:

```
$ cargo test -p hs-kv --test postgres_conformance -- --nocapture --test-threads=1
running 4 tests
test postgres_backend_conformance_breakdown ... ok
test postgres_round_trip_from_a_plain_test_with_no_ambient_runtime ... ok
test postgres_survives_being_opened_and_called_from_inside_a_tokio_runtime ... ok
test reopen_against_the_same_schema_preserves_committed_data ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in ~5.3s
```

Repeated 3 times for stability (identical result each time), and `cargo test -p hs-kv` (no
`HS_KV_TEST_POSTGRES_DSN` set, no database running) confirmed still green in ~3s via the existing
skip path — this fix did not touch, and does not affect, the no-database developer experience.

### Conformance result: unchanged, 9/11, same two documented divergences

Re-ran `cargo test -p hs-kv --test postgres_conformance -- --nocapture --test-threads=1` against a
real `postgres:17` container per the exact commands already documented under "PostgreSQL
conformance run" above. **Identical result to session 2, byte-for-byte down to which two scenarios
diverge**: 9/11 passed,
`phantom_insert_inside_a_scanned_range_conflicts` fails deterministically (same structural reason,
unaffected by an execution-model change), `atomic_add_under_contention` fails intermittently (same
latency/tuning fact — its retry-budget math depends on wall-clock round-trip time, which this
change did not alter; if anything, a bare OS thread spawn is negligible next to the ~5.3ms network
round trip, so no measurable latency shift is expected or was observed). This session's change is
purely about *which thread* issues each call, never *what* is sent to PostgreSQL or *when* within a
transaction, so this result is exactly what should be expected, not a coincidence.

### Whether the server can now serve on Postgres

**Not fully verified end-to-end, because that requires `hs serve` + a config file + a live
register call, and this session could not edit `crates/hs-cli`** (owned by the integration lead;
its temporary guard, `StorageOpenError::PostgresInsideRuntime`, currently still refuses to open
Postgres inside a runtime on purpose, per the brief, so `hs serve` would still refuse today even
though the underlying bug is fixed). What *is* verified, as the closest available proxy: a test in
this track's own crate (`postgres_survives_being_opened_and_called_from_inside_a_tokio_runtime`)
reproduces the exact shape `hs serve` uses — opening `PostgresBackend` and driving a full
transact/read/drop cycle directly from inside a multi-threaded Tokio runtime, with no
`spawn_blocking` — and it passes. **Action needed from the integration lead**: delete
`StorageOpenError::PostgresInsideRuntime` and the guard that returns it in
`crates/hs-cli/src/storage.rs`, then run the actual acceptance check (build `hs`, config with
`storage.backend: postgres`, `hs serve`, `POST /_matrix/client/v3/register` against a real
PostgreSQL) — that is the only remaining gap between this fix and a verified working deployment,
and it is entirely inside a file this track is not allowed to touch.

**Update, same day: the integration lead did exactly that, and it found a second, real bug.** With
the guard removed, `hs serve` no longer panicked, but failed one step later: `storage backend
error: invalid keyspace name "hs_auth.users"`. See "Dotted keyspace names" immediately below.

### Dotted keyspace names: the execution-model fix's own regression test found the panic, but not this

Every consuming crate names its keyspaces with a crate-scoped, dotted prefix — `hs_auth.users`,
`hs_auth.access_tokens`, `hs_room.events`, `hs_e2e.device_keys`, `hs_push.rules`, and so on (see
`crates/hs-auth/src/store/tables.rs` and the equivalent tables modules in other crates). The
in-memory and Fjall backends never rejected this, since neither treats a keyspace name as more
than an opaque map/partition key. `PostgresBackend`'s identifier validation (`validate_ident`,
`^[A-Za-z_][A-Za-z0-9_]*$`, no dot) did — the moment `hs serve` opened its first real store, it
failed with `invalid keyspace name "hs_auth.users"`. This was invisible to every test this track
had written, including the brand-new inside-a-runtime regression test above, because every one of
them (the shared conformance suite's `"t"`, this file's own `"durable"`) uses a bare, undotted
name. Booting the actual binary is what found it, exactly as the brief anticipated when it said
the acceptance check is `hs serve` + register, not the conformance suite.

**Fix, in `crates/hs-kv/src/postgres_backend.rs` only:**

- Split the old single `validate_ident` into two functions sharing a new `is_safe_ident_segment`
  helper (the original per-character check, unchanged): `validate_ident` (schema names — a schema
  is chosen by whoever calls `PostgresBackend::open`, never dotted) stays exactly as strict as
  before; a new `validate_keyspace_name` (used only by `KvBackend::keyspace`) accepts either a
  single safe identifier or a **dotted path** of them — every `.`-separated segment individually
  checked against the same `^[A-Za-z_][A-Za-z0-9_]*$` rule, so `hs_auth.users` passes but
  `hs_auth."users`, `hs_auth.'users`, `hs_auth.us;ers`, a leading/trailing/doubled dot, or anything
  else outside that character set is still rejected exactly as before.
- **Why this is safe against injection, not just permissive**: the qualified table name is one
  double-quoted PostgreSQL identifier (`"kv_hs_auth.users"`), and a `.` inside a *single* quoted
  identifier is just an ordinary character to PostgreSQL, not a schema-qualifier — that syntax
  needs its own quotes per segment, which this backend never emits. The character actually worth
  guarding against is `"` (the identifier-quote-escape character), which `is_safe_ident_segment`
  never allowed, dot or no dot. Restricting every segment independently, rather than only checking
  the joined string doesn't contain `"`, also means a name can never sneak in something that reads
  as two identifiers or a schema reference by construction, not just by absence of the one
  dangerous character.
- **Length limit unchanged and still correct**: the existing 55-byte overall cap (checked before
  splitting, so it bounds the dots too) already left headroom for the `kv_` table-name prefix under
  PostgreSQL's 63-byte `NAMEDATALEN` identifier limit; real dotted names (`hs_auth.access_tokens`
  is 22 bytes) are nowhere near it. Going over 63 bytes doesn't error in PostgreSQL, it silently
  truncates — a real hazard (two different long keyspace names could collide on the same
  underlying table), which is why this stayed a hard cap rather than being loosened.

**Tests added:**

- Four new pure, offline unit tests in `postgres_backend.rs` itself
  (`#[cfg(test)] mod tests`, no database needed): dotted names from the real families above are
  accepted; empty/leading-dot/trailing-dot/double-dot/space/quote/semicolon/backslash/
  digit-first names are all still rejected; the 55-byte limit is enforced with dots in the mix;
  schema names stay undotted-only. These are exactly the tests that would have caught this before
  it ever reached `hs serve` — nothing before this exercised `validate_ident`/
  `validate_keyspace_name` in isolation.
- `postgres_keyspace_name_with_a_dotted_crate_prefix_round_trips` in
  `tests/postgres_conformance.rs`: the same open → `transact` a put → drop (exercising pool
  teardown too) → reopen against the same schema → read-back shape as
  `reopen_against_the_same_schema_preserves_committed_data`, but against keyspace name
  `"hs_auth.users"` specifically, proving the round-trip requirement the integration lead asked
  for: reopening finds the same data under the same dotted name.

**Verification**: `cargo fmt -p hs-kv`, `cargo clippy -p hs-kv --all-targets -- -D warnings` both
clean. `cargo test -p hs-kv --lib` (9 tests, including the 4 new validator tests) green with no
database. Full `cargo test -p hs-kv` against a real `postgres:17` container: 5/5 in
`postgres_conformance.rs` (the new dotted-name test alongside the four from before), unchanged 9/11
conformance breakdown with the same two documented divergences, plus fjall/memory/unit/doctests all
green. `cargo test -p hs-kv --test postgres_conformance` with no database running: still green in
~3s via the existing skip path.

### Concurrent setup and honest errors: two more bugs, found only by running two real servers

The integration lead booted two `hs serve` processes against one PostgreSQL database for the first
time (the normal Kubernetes "N replicas roll out at once" case) and hit two more real bugs, both in
this crate, neither a cluster problem:

**Bug 1: cold-start schema/table creation is not safe under concurrency.** Two replicas started
*simultaneously* against an empty database — one died at boot with `storage backend error: backend
error: db error`. Staggered starts (A first, then B once the schema existed) always worked, which is
exactly what makes this a concurrency bug: `CREATE SCHEMA IF NOT EXISTS` (and `CREATE TABLE IF NOT
EXISTS`, the same pattern in `KvBackend::keyspace`) is well known **not** to be atomic in
PostgreSQL — the existence check and the creation are two separate steps, so two sessions can both
observe "does not exist" and both attempt the `CREATE`.

Fix, entirely in `crates/hs-kv/src/postgres_backend.rs`: a new `create_if_not_exists_race_free`
wraps every `CREATE ... IF NOT EXISTS` this backend runs (schema creation in `open`, table creation
in `keyspace`) in a transaction that first takes `pg_advisory_xact_lock(lock_key)`, where `lock_key`
is a deterministic FNV-1a hash (`advisory_lock_key`) of the schema/table's own name — **not**
`std::collections::hash_map::DefaultHasher`, whose algorithm is explicitly unspecified and may
differ between processes, which two racing replicas cannot risk. The lock serializes every caller
racing to create the *same* named object and releases automatically at `COMMIT`/`ROLLBACK` (a
`_xact_` lock, not a session lock — no manual unlock to get wrong). This alone removes the race in
the overwhelming majority of cases.

**Verified empirically, not assumed, that a fallback is still needed and what it must actually
catch**: temporarily reverted the lock (kept the honest-error fix below) and re-ran the new race
test (below) — it failed within a handful of iterations, and the real error was
`23505 unique_violation` on `pg_namespace`'s own unique index (`pg_namespace_nspname_index`), i.e.
**not** the semantically-obvious `duplicate_schema`/`duplicate_table` (`42P06`/`42P07`) — those only
fire for the ordinary, non-concurrent "already committed before your transaction began" case; a
genuine race loses at the physical index-insert step, which PostgreSQL reports as a unique
violation. `create_if_not_exists_race_free` now treats *either* code as "someone else already
created it, not an error" — belt and suspenders in case the lock is ever bypassed (a
differently-versioned instance mid-rolling-upgrade, an operator running raw DDL by hand), kept
specifically because "IF NOT EXISTS is not atomic" is documented PostgreSQL behavior, not a
hypothetical this backend gets to assume away just because it also takes a lock. This is exactly
the kind of thing the brief warned about ("handle that error explicitly rather than assuming it
away"), and skipping the empirical check would have shipped a fallback that silently didn't work.

**Bug 2: `KvError`'s error messages threw away everything PostgreSQL actually said.** The `db error`
text above is not a placeholder — it is `postgres::Error`'s *entire* top-level `Display` output for
`Kind::Db`, i.e. every server-reported error this backend can hit. The real SQLSTATE, message,
detail, and hint live one level down, in `postgres::Error::as_db_error()`'s `DbError`, which nothing
upstream ever read: `KvError::Backend`'s own `Display` (`"backend error: {0}"`) just renders
whatever `Display` its boxed source produces, and `postgres::Error`'s `Display` for a `Kind::Db`
error is the fixed string `"db error"`, full stop — an operator got nothing to act on.

Fix: a new `PgErrorDetail` wrapper (`postgres_backend.rs`) whose `Display` unpacks
`as_db_error()` and renders `[SQLSTATE] severity: message` plus `DETAIL`/`HINT` (from `DbError`'s
own `Display`, which — also worth knowing — never includes the code itself) and, when present,
schema/table/constraint names; falls back to the plain top-level `Display` for non-`Db` errors
(connection/TLS/DSN-parse failures), which are already reasonably specific. A new `pg_error(e:
postgres::Error) -> KvError` helper wraps a `postgres::Error` in `PgErrorDetail` before handing it
to `KvError::backend`, and now every call site in this module that produces a `postgres::Error`
directly uses it — audited the whole file (the brief specifically asked to check for other
discarding paths): `dsn.parse()`, both `CREATE ... IF NOT EXISTS` sites (via
`create_if_not_exists_race_free`), `DROP SCHEMA`, `BEGIN` (both the snapshot and read-write paths —
`snapshot()`'s failure path previously used a bare `e.to_string()`, same bug, now fixed too),
`PgTxn::with_conn`, `PgSnapshot::with_conn`, and `commit()`. **One path is still, unavoidably,
lossy**: `pool.get()`/`Pool::builder().build()` return `r2d2::Error`, and `r2d2` itself discards the
structured `postgres::Error` into a plain `String` (via its own internal `.to_string()`) *before*
this crate ever sees the failure — there is nothing left to recover by the time we have an
`r2d2::Error`. In practice this only affects pool/connect-time failures (unreachable server, auth
failure, timeout), whose `postgres::Error::Kind` messages are already specific enough on their own
(`"authentication error"`, `"timeout waiting for server"`, etc.) — it's the `Kind::Db` case
(`"db error"`) that was actually uninformative, and that case never goes through `r2d2::Error`.
Documented in `PgErrorDetail`'s module docs rather than silently left as-is.

The exact error text an operator now sees for the schema race (captured from the test below, before
the lock was restored — see "verified empirically" above):

```
backend error: [23505] ERROR: duplicate key value violates unique constraint "pg_namespace_nspname_index"
DETAIL: Key (nspname)=(hs_kv_test_18466_3_race3) already exists. (schema: pg_catalog) (table: pg_namespace) (constraint: pg_namespace_nspname_index)
```

**Test added**: `postgres_two_replicas_opening_the_same_fresh_schema_simultaneously_both_succeed` in
`tests/postgres_conformance.rs`. Two real threads, a `std::sync::Barrier` to bring them as close to
simultaneous as `std::thread` allows, racing to `PostgresBackend::open` the *same* brand-new schema
name, then racing again to `.keyspace("hs_auth.users")` on top of that (the second place this exact
bug hits) — both threads must succeed both times. Runs 20 iterations internally (a race test that
passes once proves little, since the actual overlap window is narrow and not fully controllable
from a test).

**Run count, as asked**: with the fix in place, ran the dedicated race test **11 times** end to end
against a real `postgres:17` container (once as part of a full `cargo test -p hs-kv --test
postgres_conformance` run, then 5 more standalone runs before finding the `23505` gap above, then 6
more standalone runs after fixing it) — every run's 20 internal iterations passed, so **220 total
paired-open races plus 220 total paired-keyspace races, all green** on the fixed code. Separately,
with the advisory lock temporarily removed (fix-verification step, not a shipped state — reverted
immediately after), the same test reliably failed (within single-digit iterations each attempt),
confirming the test actually exercises the race rather than passing vacuously.

**Verification**: `cargo fmt -p hs-kv`, `cargo clippy -p hs-kv --all-targets -- -D warnings` both
clean. Full `cargo test -p hs-kv` against a real `postgres:17` container: all green, 6/6 in
`postgres_conformance.rs` now (the new race test alongside the five from before), conformance
breakdown still unchanged at 9/11 with the same two documented divergences. `cargo test -p hs-kv
--test postgres_conformance` with no database running: still green in ~3s via the existing skip
path.

## Next

- SlateDB backend — **explicitly out of scope this session**, per instruction; still not started
  at all. Do not start both PostgreSQL and SlateDB in one sitting.
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

- `hs-kv` trait v0 (frozen session 1, matching the week-2 seam in `docs/workstreams/README.md`):
  `KvBackend`, `KvRead`, `KvWrite`, `RangeSpec`, `transact`, `Hub`/`Watch` — signatures unchanged
  this session. Three backends now: `hs_kv::memory::MemoryBackend`,
  `hs_kv::fjall_backend::FjallBackend`, `hs_kv::postgres_backend::PostgresBackend` (new this
  session; `PostgresBackend::open(dsn, schema)`).
- **New, additive-only this session**: `hs_kv::KvError::MidTransactionConflict` (a new enum
  variant — see "Decisions made" for why this is safe) and `hs_kv::conformance`'s eleven scenario
  functions are now individually `pub` (were private), alongside the unchanged
  `run_conformance_suite`.
- `hs-tables`: `key::{KeyEncode, KeyDecode, TupleKey}`, `keyspace::TypedKeyspace`,
  `index::{IndexDef, maintain_index, lookup}`, `migrations::{Migration, run_migrations,
  current_version}`, `interning::{InternTable, ShortId, room_sn_table, user_sn_table,
  server_sn_table, event_sn_table, state_key_id_table, type_id_table}`.
- Both crates are usable today by any other track: `hs_kv::memory::MemoryBackend::new()` needs no
  setup and is the recommended backend for other tracks' own unit tests, per
  `docs/workstreams/README.md` rule 2.
- **Session 3, behavioral only, no signature change**: `hs_kv::postgres_backend::PostgresBackend`
  now tolerates being opened and called from any thread, including one with an ambient Tokio
  runtime (a Tokio worker thread, `hs serve`'s async main, etc.) — see "Session 3" above. No
  `KvBackend`/`KvRead`/`KvWrite` signature changed. Consumers no longer need `spawn_blocking` for
  correctness against this backend (only, optionally, to avoid blocking a Tokio worker thread for a
  call's latency).
- **Session 3, behavioral only, no signature change**: `PostgresBackend::open` and
  `KvBackend::keyspace` are now safe to call from two (or more) processes/replicas at the exact
  same moment against the same fresh schema/table — see "Concurrent setup and honest errors" above.
  `KvError::Backend`'s message text for a PostgreSQL-originated error now includes the SQLSTATE
  code and the database's own message (and detail/hint/schema/table/constraint, when PostgreSQL
  supplies them) instead of collapsing to the uninformative `"db error"` — a text-content change to
  an existing error variant's `Display`, not a new variant or a signature change, so nothing that
  matches on `KvError` structurally is affected, only anything that was asserting on the literal
  error string (nothing in this workspace was, checked).

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

- **(Session 2) `postgres` (sync) + `r2d2`/`r2d2_postgres` over `tokio-postgres`/`deadpool-postgres`
  for the PostgreSQL backend.** `KvBackend` is a synchronous trait; the sync `postgres` crate wraps
  the identical `rust-postgres` wire-protocol code as `tokio-postgres` behind a blocking facade
  with its own hidden per-connection runtime, avoiding a `block_on`-from-inside-a-runtime hazard or
  a spawn/channel shim in every trait method. Full rationale in `postgres_backend`'s module docs.
- **(Session 2) `KvError::MidTransactionConflict` added as a new enum variant, and
  `transact()`'s retry loop extended to treat it like a commit-time `Conflict`.** Checked before
  adding: no crate in the workspace exhaustively matches on `KvError` without a wildcard arm, so
  this is a safe, additive change to a type other tracks already consume. Needed because
  PostgreSQL's SSI can report a serialization failure, deadlock, or lock timeout on any statement,
  not only `COMMIT`, unlike the in-memory/Fjall backends (which only ever detect a conflict at
  commit). Neither existing backend can produce this variant, so their behavior is unchanged.
- **(Session 2) Writes are buffered client-side inside a PostgreSQL transaction and flushed only
  at `commit()`, not applied immediately on `put`/`delete`.** This is the single most important
  decision this session made, and it was forced by a real deadlock, not chosen speculatively — see
  the incident write-up above. It is also what makes the PostgreSQL backend's write behavior match
  the in-memory/Fjall backends' "optimistic transaction" semantics rather than diverging from them.
- **(Session 2) `hs_kv::conformance`'s eleven scenario functions made individually `pub`,
  `run_conformance_suite` itself unchanged.** Purely additive (existing call sites untouched); done
  so the PostgreSQL backend's test suite can run every scenario and report a full breakdown instead
  of stopping at the first failure, which was necessary to honestly characterize the two documented
  divergences (see "PostgreSQL conformance run" above) instead of hiding everything behind one
  bulk pass/fail.
- **(Session 2) The phantom-range guarantee in the crate-level contract doc (`lib.rs`: "including a
  write of a *new* key that falls inside a previously scanned range... causes the commit to report
  Conflict") is not fully portable to PostgreSQL, and this was not "fixed" by weakening the
  in-memory/Fjall backends or by strengthening PostgreSQL's guarantee to match.** See "PostgreSQL
  conformance run" above for the full reasoning: it is a guarantee stronger than true
  serializability, achievable single-process (memory, Fjall) but not, as far as this session could
  determine, achievable across multiple processes sharing one real PostgreSQL database without
  much heavier machinery than this session's scope justified. Left as a documented, tested,
  named divergence rather than silently papered over. Other tracks relying on this exact guarantee
  for a *range* scan (not a point read) on the PostgreSQL backend should read that section before
  assuming it holds; point-read fencing (track 03's actual pattern) is unaffected.
- **`hs_kv::conformance` uses `unwrap`/`expect` freely outside `#[cfg(test)]`**, which reads as a
  quality-bar exception (`docs/decisions/0002-workspace-conventions.md`: "no `unwrap` ... outside
  tests"). It is deliberate: the module's only purpose is to be called from other crates' `#[test]`
  functions (this crate's own and, per `docs/workstreams/README.md` rule 2, every other track's), so
  it is test code in substance even though it cannot be `#[cfg(test)]`-gated without also hiding it
  from consumers. Panicking on an unexpected `Err` is the correct behavior there, identical to what
  `assert_eq!` already does throughout it.

## Shared dependencies added

Session 1: `fjall`, `tokio-postgres`, `deadpool-postgres`, `tempfile` (plus already-present
`bytes`, `thiserror`, `tracing`, `criterion`, `proptest`). `hs-tables` added ordinary path
dependencies on `hs-kv` and `hs-model` (not workspace-level, since they're in-tree crates).

Session 2 (PostgreSQL backend): added to `[workspace.dependencies]` in the root `Cargo.toml`
(all missing) and to `crates/hs-kv/Cargo.toml`:
- `postgres = "0.19"` — the synchronous PostgreSQL client actually used by `PostgresBackend` (see
  "Session 2" above for why the sync crate was chosen over the already-present
  `tokio-postgres`/`deadpool-postgres`, which remain unused by this backend and were not removed —
  another track may still want them, or a future async consumer of this crate might).
- `r2d2 = "0.8"` and `r2d2_postgres = "0.18"` — the synchronous connection pool matching a
  synchronous client, used in place of `deadpool-postgres` (an async pool) for the same reason.

Session 3 (execution-model fix): `tokio` added to `crates/hs-kv/[dev-dependencies]` only, so
`tests/postgres_conformance.rs` can hold an `#[tokio::test]` regression test to an ambient runtime.
Already present in `[workspace.dependencies]` (with the `full` feature) — not new to the workspace,
and not a dependency of `hs-kv`'s own library code, only its test target.
