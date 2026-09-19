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


Track brief: `docs/workstreams/01-storage-engine.md`. Owner crates: `hs-kv`, `hs-tables`,
`hs-search` (not started).

Last updated: 2026-09-19 (session 2: the PostgreSQL `KvBackend`, closing the
`docs/next-steps.md` gap "PostgreSQL and SlateDB backends absent — only the embedded backend can
actually open"). SlateDB is **still absent** — out of scope for this session by explicit
instruction; see "Next".

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
