//! The PostgreSQL backend: the clustered production backend (`PLAN.md` section 6.5).
//!
//! # Client library
//!
//! [`KvBackend`] is a synchronous trait — every method is a plain `fn`, matching
//! [`crate::fjall_backend::FjallBackend`], which is itself blocking (Fjall is an embedded engine
//! with no async API). Bridging an async PostgreSQL client (`tokio-postgres`, `sqlx`) across that
//! boundary inside every trait method would mean either driving a private Tokio runtime with
//! `block_on` from inside a caller who may *already* be on a Tokio worker thread (which panics:
//! "Cannot start a runtime from within a runtime"), or making every method spawn onto a runtime
//! and block on a channel back — extra machinery to reinvent something the ecosystem already
//! ships.
//!
//! Instead this backend uses the **synchronous `postgres` crate** — the same `rust-postgres`
//! project as `tokio-postgres`, wrapping the identical wire-protocol and connection code behind a
//! blocking facade with its own hidden per-connection runtime — pooled with **`r2d2`** via
//! `r2d2_postgres`. Callers on an async runtime are expected to run `hs-kv` calls through
//! `tokio::task::spawn_blocking`, exactly as they already must for the Fjall backend; this is not
//! a new obligation this backend introduces.
//!
//! # Table shape
//!
//! One table per keyspace (the brief's recommendation, for `VACUUM` locality), named
//! `"{schema}"."kv_{keyspace}"`, each `(k bytea primary key, v bytea not null)`. All tables for one
//! [`PostgresBackend`] live in one PostgreSQL schema (`"public"` by default, or whatever
//! [`PostgresBackend::open`] is given), created with `CREATE SCHEMA IF NOT EXISTS`. Keyspace names
//! are validated by [`validate_keyspace_name`] before being interpolated into DDL/DML, since
//! PostgreSQL has no way to bind an identifier as a query parameter: either a single safe SQL
//! identifier (`^[A-Za-z_][A-Za-z0-9_]*$`) or a **dotted path** of them (e.g. `hs_auth.users`,
//! the convention several consuming crates use to group their own keyspaces — see that function's
//! docs for why the dot is safe to allow), at most 55 bytes total. There is no user-controlled
//! input in this path (keyspace names come from other crates' source code, not request data), but
//! the check is cheap insurance.
//!
//! # Ordering and range scans
//!
//! The primary key `k bytea` sorts byte-wise under PostgreSQL's default `bytea` comparison, which
//! is exactly [`crate::KvRead::range`]'s contract. A range scan compiles [`RangeSpec`] into a
//! `WHERE k >= / > / <= / < $n` clause (per bound), `ORDER BY k ASC` or `DESC` for
//! [`RangeSpec::reverse`], and a `LIMIT`. Unlike the Fjall and in-memory backends, which return a
//! lazy iterator, **this backend materializes the whole result set into memory before returning
//! the iterator** — the `postgres` crate's streaming `query_raw` API needs an async runtime, and a
//! blocking row-by-row cursor held open across the caller's iteration would extend the
//! transaction and violate this crate's own "keep transactions short" rule. See the crate-level
//! docs / status file for the fuller cost note; callers already using `RangeSpec::limit` (as the
//! contract recommends) are unaffected in practice.
//!
//! # Transactions
//!
//! [`KvBackend::begin`] opens a pooled connection and issues `BEGIN ISOLATION LEVEL SERIALIZABLE`.
//! Reads ([`KvRead::get`], `multi_get`, `range`) run immediately, inside that transaction, so
//! PostgreSQL's own SSI predicate tracking sees them and its snapshot semantics apply. **Writes do
//! not**: [`KvWrite::put`] and [`KvWrite::delete`] only record the mutation in an in-memory buffer
//! (checked by every subsequent read on the same [`PgTxn`], so a transaction still reads its own
//! writes) and touch the database for the first time inside [`KvBackend::commit`], which flushes
//! the whole buffer as a burst of `INSERT ... ON CONFLICT DO UPDATE` / `DELETE` statements
//! immediately followed by `COMMIT`.
//!
//! This is not an optimization, it is a correctness requirement, discovered by actually running
//! this crate's conformance suite against a real server rather than assuming the SQL translation
//! was equivalent. PostgreSQL's row-level write locks are held for the lifetime of the
//! transaction, not just at commit: an early implementation issued each `put` as its own
//! `INSERT ... ON CONFLICT DO UPDATE` immediately, and the conformance suite's
//! `lost_update_is_prevented` / `write_skew_is_prevented` scenarios — which open two transactions
//! and write the same key from each *before either commits*, all on one thread — deadlocked
//! forever: the second write blocked waiting for the first transaction to end, but nothing was
//! ever going to end it (the only thread that could was the one blocked). The in-memory and Fjall
//! backends never hit this because their "transactions" are pure optimistic buffers that never
//! touch shared state before commit, which is exactly what this backend now does too. See the
//! status file for the incident in more detail.
//!
//! Buffering writes shrinks but does not eliminate the window where a real row lock can be held
//! against a genuinely concurrent (different-thread, different-transaction) writer to the same
//! key: two commits racing on the same row can still make one wait on the other's row lock for a
//! moment. `BEGIN` additionally sets `lock_timeout` (see [`WRITE_LOCK_TIMEOUT`]) so that wait is
//! bounded: a write that cannot acquire its row lock within that window fails with `SQLSTATE
//! 55P03` rather than blocking indefinitely, which this backend treats exactly like a
//! serialization failure (below).
//!
//! [`KvBackend::commit`] issues the buffered writes, then `COMMIT`. PostgreSQL's SSI can report a
//! serialization failure (`SQLSTATE 40001`), a deadlock (`40P01`), or (per the above) a lock
//! timeout (`55P03`) on **any** statement in the transaction, not only on `COMMIT` — unlike the
//! in-memory and Fjall backends, which only ever detect a conflict at commit time. When that
//! happens on a `get`/`multi_get`/`range` call (a real, immediate read), this backend rolls the
//! transaction back right away (the server has already aborted it; a later `COMMIT` would just
//! fail again) and returns [`KvError::MidTransactionConflict`] instead of the operation's normal
//! result. [`crate::transact`] treats that exactly like a commit-time [`Conflict`]: it discards the
//! transaction and retries the whole closure. Code that calls [`KvBackend::begin`] /
//! [`KvBackend::commit`] directly (bypassing `transact`) must additionally check for
//! `KvError::MidTransactionConflict` from every fallible transaction method, not only for
//! `Conflict` from `commit`; see the status file for why this extension exists only for
//! PostgreSQL.
//!
//! [`KvBackend::snapshot`] opens a pooled connection and issues
//! `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`, PostgreSQL's snapshot isolation: a
//! consistent view fixed at the transaction's first statement, unaffected by later commits, which
//! matches this crate's snapshot contract. Because [`KvBackend::snapshot`] cannot itself return a
//! `Result` (it is infallible in the trait), a pool-acquisition or `BEGIN` failure is captured and
//! re-surfaced as a [`KvError::Backend`] from the *first* read call against that snapshot instead.
//!
//! # Watches
//!
//! Exactly like every other backend, watches are the shared, in-process [`crate::watch::Hub`] —
//! not PostgreSQL `LISTEN`/`NOTIFY`. This keeps the "watches are hints, never cross-process, never
//! durable" contract identical across backends (a decision already recorded for the Fjall backend
//! in `docs/status/01-storage-engine.md`); a real `LISTEN`/`NOTIFY` fan-out would only matter for
//! cross-process wake-ups, which the contract explicitly does not promise.
//!
//! # Execution model: why this backend can be opened and called from inside a Tokio runtime
//!
//! The synchronous `postgres` crate is not merely "blocking" — every method that talks to the
//! server (`Client::connect`/`r2d2`'s manager, `batch_execute`, `query`, `query_opt`, `execute`)
//! internally spins up its own hidden Tokio runtime the first time it is needed and drives it with
//! `Runtime::block_on`. Tokio detects an already-active runtime on the *calling thread* via a
//! thread-local and panics rather than nesting ("Cannot start a runtime from within a runtime").
//! Since [`hs serve`](../../hs-cli) is an async binary — it opens storage from an async fn and
//! every later request handler that touches storage runs as a Tokio task — naively calling this
//! backend's methods directly panicked on the very first call, not merely at startup: any thread
//! Tokio chose to run a handler on could already be carrying that thread-local.
//!
//! The fix is that **no method on this backend ever calls into `postgres`/`r2d2` from the thread
//! the caller invoked it on.** [`run_isolated`] spawns a brand-new, bare OS thread with
//! [`std::thread::scope`], runs the given closure there, and blocks the calling thread on its
//! result. A freshly spawned OS thread has its own thread-local storage, initialized from
//! scratch — it does not inherit whatever the parent thread was doing — so it can never be
//! "already inside a runtime" no matter what thread asked for the work, including a Tokio worker
//! thread, `hs serve`'s main thread, or a plain `#[test]`'s single thread. Every single touch
//! point that reaches `postgres`/`r2d2` (`open`, `keyspace`, `begin`, `commit`,
//! `drop_schema_for_test`, `snapshot`, every `get`/`multi_get`/`range` on [`PgTxn`] and
//! [`PgSnapshot`], and both types' `Drop` impls, which issue a real `ROLLBACK`) goes through
//! [`run_isolated`] — not just the ones that looked risky, because the panic is not "opening is
//! unsafe", it is "every call is unsafe on the wrong thread", including the rollback a `Drop` runs
//! silently in the background.
//!
//! **Why a fresh thread per call, not a persistent worker pool.** Two credible designs exist here:
//! a small pool of long-lived worker threads fed through a channel, or `tokio-postgres` driven on
//! a runtime this backend owns and manages itself. Both were considered; a thread spawned fresh
//! per call was chosen instead, because the dominant cost of every operation this backend performs
//! is already a network round trip to PostgreSQL — measured at roughly 5.3ms per uncontended
//! `transact` cycle against a local Docker container (see the status file). An OS thread spawn
//! costs on the order of tens of microseconds: three orders of magnitude smaller, i.e. noise
//! against the cost this backend already pays on every call regardless of execution strategy. A
//! persistent pool would need its own lifecycle (start it in `open`, shut it down cleanly,
//! propagate a panicked worker thread's failure back to callers instead of silently wedging the
//! pool), and `tokio-postgres` would mean this backend owning and threading through its own
//! runtime handle while still presenting a synchronous [`KvBackend`] to every other crate in the
//! workspace — either is real, ongoing complexity bought for a savings this backend's own numbers
//! say does not matter. `std::thread::scope` additionally means [`run_isolated`] can borrow
//! non-`'static` data (a `&mut postgres::Client` living on the caller's stack, a `&str` SQL
//! fragment) directly, with no channel plumbing or `Arc`-wrapping to satisfy a `'static` bound. If
//! profiling of a real deployment ever shows thread-spawn overhead mattering (it would have to
//! become comparable to a multi-millisecond network round trip first), swapping the body of
//! [`run_isolated`] for a persistent pool is a localized change: every call site already goes
//! through this one function.
//!
//! The public [`KvBackend`]/[`KvRead`]/[`KvWrite`] surface is completely unchanged by this: every
//! method is still a plain, synchronous `fn` returning once the (isolated) work is done. A caller
//! on a Tokio runtime still is, and remains, expected to run `hs-kv` calls through
//! `tokio::task::spawn_blocking` if it wants to avoid blocking its own worker thread for the
//! duration of a call — exactly as already documented for the Fjall backend — but it is no longer
//! *required* to for correctness: this backend now tolerates being called directly from inside a
//! runtime, it just costs that thread the call's latency if you do. See
//! `postgres_survives_being_opened_and_called_from_inside_a_tokio_runtime` in
//! `tests/postgres_conformance.rs` for the regression test that proves this: it opens the backend,
//! runs a real transaction, and drops it, all from inside `#[tokio::test]`'s ambient
//! multi-threaded runtime.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use bytes::Bytes;
use postgres::NoTls;
use postgres::error::SqlState;
use postgres::types::ToSql;
use r2d2::Pool;
use r2d2_postgres::PostgresConnectionManager;

use crate::error::{Conflict, KvError};
use crate::limits::{self, TxnBudget};
use crate::traits::{KvBackend, KvRead, KvWrite, RangeItem, RangeIter, RangeSpec};
use crate::watch::{Hub, Watch};

type PgManager = PostgresConnectionManager<NoTls>;
type PgPool = Pool<PgManager>;
type PgConn = r2d2::PooledConnection<PgManager>;
/// A [`PgTxn`]'s buffered, not-yet-flushed writes: `(keyspace's qualified table name, key) ->
/// Some(value)` for a pending `put`, `None` for a pending `delete`. See [`PgTxn`]'s docs.
type PendingWrites = HashMap<(Arc<str>, Vec<u8>), Option<Vec<u8>>>;

/// A plain string wrapped as a [`std::error::Error`], used to re-surface a pool/connection
/// failure captured at [`KvBackend::snapshot`] time (which cannot itself return a `Result`) from
/// the first fallible read call against that snapshot.
#[derive(Debug)]
struct DeferredError(String);

impl fmt::Display for DeferredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeferredError {}

/// Validates a keyspace or schema name as a safe, unquoted SQL identifier component. Keyspace
/// names are chosen by `hs-tables`, not user data (see the crate-level contract), but this is
/// cheap insurance against ever interpolating something unexpected into DDL/DML.
/// Whether `segment` is shaped like a safe, unquoted SQL identifier on its own
/// (`^[A-Za-z_][A-Za-z0-9_]*$`) — the building block both [`validate_ident`] and
/// [`validate_keyspace_name`] check every dot-separated piece of a name against.
fn is_safe_ident_segment(segment: &str) -> bool {
    let first_ok = segment
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = segment
        .chars()
        .skip(1)
        .all(|c| c.is_ascii_alphanumeric() || c == '_');
    !segment.is_empty() && first_ok && rest_ok
}

/// Validates a schema name as a single safe, unquoted SQL identifier
/// (`^[A-Za-z_][A-Za-z0-9_]*$`, at most 55 bytes). Unlike a keyspace name (see
/// [`validate_keyspace_name`]), a schema name is never dotted — it names exactly one PostgreSQL
/// schema and is chosen by whoever calls [`PostgresBackend::open`], not by another crate's own
/// naming convention.
fn validate_ident(name: &str) -> Result<(), KvError> {
    if name.is_empty() || name.len() > 55 || !is_safe_ident_segment(name) {
        return Err(KvError::InvalidKeyspaceName(name.to_owned()));
    }
    Ok(())
}

/// Validates a keyspace name as either a single safe identifier or a **dotted path of them**
/// (`^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$`, at most 55 bytes total), e.g.
/// `"events"` or `"hs_auth.users"`.
///
/// The dotted form exists because several consuming crates group their own keyspaces under a
/// crate-scoped prefix this way (see e.g. `crates/hs-auth/src/store/tables.rs`'s `hs_auth.users`,
/// `hs_auth.access_tokens`, and the equivalent `hs_room.*`/`hs_e2e.*`/`hs_push.*` families in
/// other crates) — a convention the in-memory and Fjall backends never rejected, since neither
/// treats a keyspace name as anything more than an opaque map/partition key. This backend does
/// have to embed the name inside a SQL identifier, but a dot is not special once the whole thing
/// is inside one pair of double quotes (`"kv_hs_auth.users"` is a single ordinary identifier to
/// PostgreSQL, not a schema-qualified reference — that syntax needs a separate quoted piece per
/// segment, which this backend never produces), so accepting the dot costs nothing in safety as
/// long as every segment on either side of it is still restricted to the same safe character set:
/// every segment is checked independently against [`is_safe_ident_segment`], which in particular
/// never allows `"` (the identifier-quote-escape character) or `.` itself as a segment character,
/// so a name can never smuggle in anything that terminates the quoted identifier early or
/// resembles a second, attacker-chosen identifier. The 55-byte overall limit (checked before
/// splitting, so it bounds the dots too) leaves room for the `kv_` table-name prefix
/// ([`KvBackend::keyspace`]) while staying under PostgreSQL's 63-byte `NAMEDATALEN` identifier
/// limit — going over that limit doesn't error, it silently truncates, which would risk two
/// different keyspace names colliding on the same underlying table, so this is a hard cap, not a
/// style preference.
fn validate_keyspace_name(name: &str) -> Result<(), KvError> {
    if name.is_empty() || name.len() > 55 || !name.split('.').all(is_safe_ident_segment) {
        return Err(KvError::InvalidKeyspaceName(name.to_owned()));
    }
    Ok(())
}

/// Runs `f` to completion on a freshly spawned, bare OS thread and blocks the calling thread
/// until it finishes, returning `f`'s result. See the module docs ("Execution model") for why
/// *every* call this backend makes into `postgres`/`r2d2` goes through this function: the
/// synchronous `postgres` crate drives a hidden Tokio runtime internally and panics if invoked on
/// a thread that already has one, and a brand-new OS thread — unlike the calling thread, which
/// might be a Tokio worker — never does.
///
/// A panic inside `f` is propagated to the caller (via [`std::panic::resume_unwind`]) rather than
/// silently swallowed, so a bug in `f` still fails the calling test/request the same way it would
/// have without this indirection.
///
/// # Panics
/// Propagates any panic from `f`.
fn run_isolated<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    match thread::scope(|scope| scope.spawn(f).join()) {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn is_serialization_conflict(err: &postgres::Error) -> bool {
    matches!(
        err.code(),
        Some(code)
            if *code == SqlState::T_R_SERIALIZATION_FAILURE
                || *code == SqlState::T_R_DEADLOCK_DETECTED
                || *code == SqlState::LOCK_NOT_AVAILABLE
    )
}

/// Wraps a `postgres::Error` so its [`fmt::Display`] carries PostgreSQL's own SQLSTATE code and
/// message — and, when present, `DETAIL`/`HINT`/schema/table/constraint — instead of collapsing to
/// `postgres::Error`'s own generic top-level text.
///
/// This exists because of a real incident, not speculatively: `postgres::Error`'s `Display` impl
/// is keyed on a coarse internal `Kind` (`Io`, `Db`, `Closed`, ...), and for `Kind::Db` — i.e.
/// *every* error PostgreSQL itself reports, which is the overwhelming majority of what this
/// backend's DDL/DML calls can fail with — that `Display` is the fixed string `"db error"`,
/// full stop. The actual SQLSTATE, message, detail and hint live one level down, in
/// `postgres::Error::as_db_error()`'s `DbError`, which nothing upstream of this wrapper ever
/// looked at: `KvError::Backend`'s own `Display` (`"backend error: {0}"`) just renders whatever
/// `Display` its boxed source already produces. An operator debugging a real deployment failure
/// (see the status file: two `hs serve` replicas racing on first-boot schema creation) saw
/// `storage backend error: backend error: db error` — completely actionable-free — because of
/// exactly this. Every call site in this module that produces a `postgres::Error` now converts it
/// with [`pg_error`], which uses this wrapper, instead of the crate-wide, generic
/// [`KvError::backend`].
#[derive(Debug)]
struct PgErrorDetail(postgres::Error);

impl fmt::Display for PgErrorDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_db_error() {
            Some(db_err) => {
                // `DbError`'s own `Display` already renders `"{severity}: {message}"` plus
                // `DETAIL`/`HINT` when present, but — bizarrely, given the type's whole purpose —
                // never the SQLSTATE code itself, so it's prepended here.
                write!(f, "[{}] {db_err}", db_err.code().code())?;
                if let Some(schema) = db_err.schema() {
                    write!(f, " (schema: {schema})")?;
                }
                if let Some(table) = db_err.table() {
                    write!(f, " (table: {table})")?;
                }
                if let Some(constraint) = db_err.constraint() {
                    write!(f, " (constraint: {constraint})")?;
                }
                Ok(())
            }
            // Not a server-reported error (e.g. a connection, TLS, or DSN-parse failure) — there
            // is no `DbError` to unpack, but the top-level `Display` is at least not the
            // uninformative `"db error"` text for these `Kind`s, so it's used as-is; `source()`
            // (if any) is still reachable through this wrapper's own `Error::source`, for anything
            // that walks the chain (e.g. `anyhow`, `tracing_error`, or a future caller of this
            // crate).
            None => write!(f, "{}", self.0),
        }
    }
}

impl std::error::Error for PgErrorDetail {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Converts a `postgres::Error` into a [`KvError::Backend`] that preserves PostgreSQL's own
/// SQLSTATE code and message (see [`PgErrorDetail`]). Used at every call site in this module that
/// handles a `postgres::Error` directly; call sites that only see an `r2d2::Error` (a pool/connect
/// failure) cannot use this; see the module docs ("Concurrent setup") for why.
fn pg_error(e: postgres::Error) -> KvError {
    KvError::backend(PgErrorDetail(e))
}

/// A simple, deterministic 64-bit hash (FNV-1a), used only to turn a schema/table name into a
/// `pg_advisory_xact_lock` key (see [`create_if_not_exists_race_free`]). Not a cryptographic hash
/// and not meant to be one — it only needs to give two processes racing to create the *same*
/// schema or table the same lock key. `std::collections::hash_map::DefaultHasher` was deliberately
/// not used here: its algorithm is explicitly documented as unspecified and may change between
/// Rust releases or even between runs (`RandomState`'s seed), and two `hs serve` replicas racing
/// at boot are not guaranteed to be the same build in every deployment shape (a rolling upgrade
/// mid-flight, for one) — this hash must be stable across processes and time for the lock to work
/// at all.
fn advisory_lock_key(name: &str) -> i64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // `pg_advisory_xact_lock` takes a signed `bigint`; reinterpreting the bits (not truncating)
    // keeps the full 64-bit hash space as the lock-key space.
    hash as i64
}

/// Runs a `CREATE ... IF NOT EXISTS` DDL statement (`create_sql`) in a way that is safe against
/// another connection doing the exact same thing at the exact same time — the normal "N replicas
/// of `hs serve` roll out simultaneously against an empty database" Kubernetes case, and a real
/// incident: starting two replicas *simultaneously* against a fresh database killed one of them at
/// boot with a bare `db error` (see [`PgErrorDetail`] for why that message itself was uninformative,
/// separately fixed), because `CREATE SCHEMA IF NOT EXISTS` is well known **not** to be atomic in
/// PostgreSQL — the existence check and the creation are two separate steps, and two sessions
/// racing can both observe "does not exist" and both attempt the `CREATE`, with the loser getting a
/// real `duplicate_schema`/`duplicate_table` error instead of the silent no-op its name implies.
/// Starting the replicas staggered (so the schema already exists by the time the second one opens)
/// never showed the bug, which is exactly why it is a concurrency bug and not a logic bug.
///
/// Two layers of defense, both real, not redundant for show:
///
/// 1. **`pg_advisory_xact_lock(lock_key)`** serializes every caller trying to create the *same*
///    named object: the lock is held only for this one transaction and releases automatically at
///    `COMMIT`/`ROLLBACK`, so a second caller blocks until the first has either created the object
///    or given up, then proceeds with an accurate, no-longer-racing view of whether it exists. This
///    is the layer that actually prevents the race in the overwhelming majority of cases, and the
///    reason two replicas serialize at boot instead of both hitting the database at once.
/// 2. **The loser's SQLSTATE (`duplicate_object_code`) is still caught and treated as success**,
///    not propagated, in case the lock is ever bypassed (a differently-versioned instance not
///    taking this lock during a rolling upgrade, an operator running raw DDL by hand, or simply
///    because relying on "the lock always works" to justify skipping this check is exactly the kind
///    of assumption the brief warned against). Belt and suspenders, on purpose: `IF NOT EXISTS`
///    being non-atomic is documented PostgreSQL behavior, not a hypothetical this backend gets to
///    assume away just because it also takes a lock.
fn create_if_not_exists_race_free(
    conn: &mut postgres::Client,
    lock_key: i64,
    create_sql: &str,
    duplicate_object_code: SqlState,
) -> Result<(), postgres::Error> {
    conn.batch_execute("BEGIN")?;
    let outcome = conn
        .execute("SELECT pg_advisory_xact_lock($1)", &[&lock_key])
        .and_then(|_rows| conn.batch_execute(create_sql));
    match outcome {
        Ok(()) => conn.batch_execute("COMMIT"),
        // Lost the race despite the lock (or the lock was bypassed): the object exists now, which
        // is exactly the end state `IF NOT EXISTS` was asked to guarantee — not an error. Checked
        // empirically, not assumed: with the lock removed, forcing this exact race (see the status
        // file's "verification" section) showed PostgreSQL raises `23505 unique_violation` against
        // the system catalog's own unique index (`pg_namespace_nspname_index` /
        // `pg_class_relname_nsp_index`), **not** the seemingly-obvious `duplicate_schema`/
        // `duplicate_table` (`42P06`/`42P07`) — those only fire for the ordinary, non-concurrent
        // "you asked to create something that was already committed before your transaction
        // began" case. A real race loses at the physical index-insert step, which is a
        // `UNIQUE_VIOLATION`, so both codes are accepted here; `duplicate_object_code` is kept as
        // an explicit parameter (not folded into a constant) so each call site still states which
        // semantic "already exists" error it expects in the non-racing case.
        Err(e)
            if e.code() == Some(&duplicate_object_code)
                || e.code() == Some(&SqlState::UNIQUE_VIOLATION) =>
        {
            // The transaction is already aborted server-side (PostgreSQL aborts the whole
            // transaction on any statement error), so roll it back explicitly rather than relying
            // on `COMMIT`'s implicit-rollback-in-an-aborted-transaction behavior.
            conn.batch_execute("ROLLBACK")
        }
        Err(e) => {
            let _ = conn.batch_execute("ROLLBACK");
            Err(e)
        }
    }
}

/// How long a write inside a [`PgTxn`] will wait on a row another open (uncommitted) transaction
/// is holding before giving up. See the module docs: without this, `put`/`delete` would use
/// PostgreSQL's normal row-lock **wait**, which blocks until the other transaction ends — the
/// opposite of the optimistic, never-block-on-a-write contract every other backend upholds, and
/// enough to hang a caller that (like this crate's own conformance suite, and any real code
/// following the same pattern) opens two transactions, writes the same key from each, and only
/// commits afterwards, all on one thread with no other thread ever available to end the first
/// transaction. A blocked write instead fails fast with `SQLSTATE 55P03` (`lock_not_available`),
/// which this backend treats exactly like a serialization failure: roll back, report
/// `KvError::MidTransactionConflict`, let `transact` retry.
const WRITE_LOCK_TIMEOUT: &str = "200ms";

struct Inner {
    // `Option`, not a plain `PgPool` field — see `impl Drop for Inner` immediately below for why:
    // dropping an `r2d2::Pool` drops every pooled `postgres::Client`, whose own `Drop` impl makes
    // a real blocking call, so it must happen inside `run_isolated` like every other call in this
    // module, not as an ordinary field drop on whatever thread `Inner` happens to be dropped on.
    // `Option::take` extracts it safely (this crate forbids `unsafe`, so `ManuallyDrop::take`,
    // the usual tool for this, is not available). Always `Some` from construction until
    // `Inner::drop` runs; every other method may assume that and `.expect()` accordingly, exactly
    // like `PgTxn::with_conn`'s existing "used after commit" invariant below.
    pool: Option<PgPool>,
    schema: String,
    hub: Hub,
    tables: Mutex<HashMap<String, Arc<str>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // `r2d2::Pool` is a cheap `Arc`-backed handle, but dropping the *last* one tears down
        // every idle pooled connection, and `postgres::Client::drop` issues a real, blocking
        // `postgres` call of its own (`close_inner` -> `block_on`) — the exact same "no ambient
        // runtime" hazard as every explicit call in this module (see the module docs, "Execution
        // model"), just triggered implicitly by a destructor instead of a method call. This is
        // not hypothetical: dropping a `PostgresBackend` from inside a `#[tokio::test]`'s runtime
        // panicked here before this fix (see
        // `postgres_survives_being_opened_and_called_from_inside_a_tokio_runtime` in
        // `tests/postgres_conformance.rs`, which drops a backend at the end of its round trip).
        if let Some(pool) = self.pool.take() {
            run_isolated(move || drop(pool));
        }
    }
}

/// The PostgreSQL [`KvBackend`]. Cloning shares the connection pool.
#[derive(Clone)]
pub struct PostgresBackend {
    inner: Arc<Inner>,
}

impl PostgresBackend {
    /// Opens a connection pool to `dsn` (a `postgres://` connection string) and ensures `schema`
    /// exists, creating it if necessary. All of this backend's tables live in that schema.
    ///
    /// `dsn` is parsed by the `postgres` crate itself, so it accepts the same syntax `psql`
    /// does (`postgres://user:password@host:port/database?options`).
    ///
    /// # Errors
    /// Returns [`KvError::InvalidKeyspaceName`] if `schema` is not a safe identifier, or
    /// [`KvError::Backend`] if the DSN is invalid, no connection could be established, or the
    /// schema could not be created.
    pub fn open(dsn: &str, schema: &str) -> Result<Self, KvError> {
        validate_ident(schema)?;
        let config: postgres::Config = dsn.parse().map_err(pg_error)?;
        // See the module docs ("Execution model"): building the pool and taking its first
        // connection both may dial PostgreSQL, which the synchronous `postgres` crate does by
        // driving a hidden Tokio runtime — never safe to do on the caller's own thread, since
        // `open` itself may be called from inside an async fn (as `hs serve` does).
        run_isolated(move || {
            let manager = PgManager::new(config, NoTls);
            let pool = Pool::builder()
                .max_size(16)
                // Fail fast rather than r2d2's 30-second default: a caller (including a
                // reachability check like the one `postgres_conformance.rs` uses to decide
                // whether to skip) should not have to wait half a minute to learn there is no
                // server. Production callers that want resilience against a slow-starting
                // database retry `open`/individual operations at a higher level (readiness
                // probes, `hs serve`'s own startup retry), not by waiting longer here.
                .connection_timeout(std::time::Duration::from_secs(3))
                .build(manager)
                .map_err(KvError::backend)?;
            {
                let mut conn = pool.get().map_err(KvError::backend)?;
                // See `create_if_not_exists_race_free`'s docs ("Concurrent setup"): two `hs serve`
                // replicas booting simultaneously against a fresh database both racing on this
                // exact statement is a real, seen-in-practice failure, not a hypothetical.
                create_if_not_exists_race_free(
                    &mut conn,
                    advisory_lock_key(&format!("hs_kv_schema:{schema}")),
                    &format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""),
                    SqlState::DUPLICATE_SCHEMA,
                )
                .map_err(pg_error)?;
            }
            Ok(Self {
                inner: Arc::new(Inner {
                    pool: Some(pool),
                    schema: schema.to_owned(),
                    hub: Hub::new(),
                    tables: Mutex::new(HashMap::new()),
                }),
            })
        })
    }

    /// Drops this backend's whole schema, including every keyspace's table. Only ever used by
    /// tests to clean up after themselves; production callers have no reason to call this.
    ///
    /// # Errors
    /// Returns [`KvError::Backend`] on a connection or query failure.
    pub fn drop_schema_for_test(&self) -> Result<(), KvError> {
        let pool = self
            .inner
            .pool
            .as_ref()
            .expect("PostgresBackend used after Inner was dropped, which cannot happen: Inner is owned by an Arc this handle holds a strong reference to");
        let schema = &self.inner.schema;
        run_isolated(move || {
            let mut conn = pool.get().map_err(KvError::backend)?;
            conn.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
                .map_err(pg_error)
        })
    }
}

/// A handle to one PostgreSQL table (one keyspace).
#[derive(Clone)]
pub struct PgKeyspace {
    /// The unquoted keyspace name, for watch hub bookkeeping.
    name: Arc<str>,
    /// The fully quoted, schema-qualified table identifier, ready to interpolate into SQL text.
    qualified: Arc<str>,
}

impl fmt::Debug for PgKeyspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgKeyspace")
            .field("name", &self.name)
            .finish()
    }
}

enum SnapState {
    Ready(Box<PgConn>),
    Failed(String),
}

/// A read-only, repeatable-read view of every keyspace, backed by a PostgreSQL
/// `REPEATABLE READ READ ONLY` transaction.
pub struct PgSnapshot {
    state: Mutex<SnapState>,
}

impl Drop for PgSnapshot {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let SnapState::Ready(conn) = &mut *state {
            // See `PgTxn`'s `Drop` impl and the module docs: a real `postgres` call from a `Drop`
            // impl must be isolated exactly like any other call.
            run_isolated(move || {
                let _ = conn.batch_execute("ROLLBACK");
            });
        }
    }
}

impl PgSnapshot {
    /// Runs `f` against the live connection on an isolated thread (see the module docs), or
    /// re-surfaces a pool/connection failure captured at [`KvBackend::snapshot`] time.
    fn with_conn<T: Send>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T, postgres::Error> + Send,
    ) -> Result<T, KvError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *state {
            SnapState::Ready(conn) => run_isolated(move || f(conn)).map_err(pg_error),
            SnapState::Failed(msg) => Err(KvError::backend(DeferredError(msg.clone()))),
        }
    }
}

/// A read-write, serializable transaction, backed by a PostgreSQL `SERIALIZABLE` transaction.
///
/// The connection lives behind a `Mutex` (not simple ownership) purely so [`KvRead::get`] /
/// [`KvRead::range`] — which take `&self`, since a transaction is also readable through a shared
/// reference in this trait — can still reach the underlying `postgres::Client`'s `&mut self`
/// query methods. `pending` and `write_keys` need no such trick: they are only ever written by
/// [`KvWrite::put`] / [`KvWrite::delete`] (`&mut self`) and only ever read by this module's own
/// `&self` read methods, which is an ordinary shared/exclusive split, not concurrent access.
pub struct PgTxn {
    conn: Mutex<Option<PgConn>>,
    /// Buffered, not-yet-flushed writes: `(keyspace's qualified table name, key) -> Some(value)`
    /// for a pending `put`, `None` for a pending `delete`. See the module docs on why writes are
    /// buffered instead of applied immediately. Read by `get`/`multi_get`/`range` before they
    /// fall through to the database, so a transaction sees its own writes.
    pending: PendingWrites,
    write_keys: Vec<(String, Vec<u8>)>,
    budget: TxnBudget,
}

impl PgTxn {
    /// Runs `f` against the live connection, translating a PostgreSQL serialization failure,
    /// deadlock, or lock timeout into [`KvError::MidTransactionConflict`] and rolling the
    /// (already server-side-aborted) transaction back immediately — see the module docs. Any
    /// other error becomes [`KvError::Backend`].
    fn with_conn<T: Send>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T, postgres::Error> + Send,
    ) -> Result<T, KvError> {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let conn = guard.as_mut().expect(
            "PgTxn used after commit (hs-kv only calls with_conn before commit consumes the txn)",
        );
        // Both the query itself and, on a serialization failure, the rollback that follows it
        // are real `postgres` calls and must happen on the same isolated thread — see the module
        // docs ("Execution model"). `f(conn)` reborrows `conn` (an ordinary implicit `&mut`
        // reborrow), so it is still usable afterward for the conditional rollback.
        match run_isolated(move || {
            let result = f(conn);
            if let Err(e) = &result
                && is_serialization_conflict(e)
            {
                let _ = conn.batch_execute("ROLLBACK");
            }
            result
        }) {
            Ok(value) => Ok(value),
            Err(e) if is_serialization_conflict(&e) => {
                *guard = None;
                Err(KvError::MidTransactionConflict)
            }
            Err(e) => Err(pg_error(e)),
        }
    }
}

impl Drop for PgTxn {
    fn drop(&mut self) {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mut conn) = guard.take() {
            // A real `postgres` call, made from a `Drop` impl that may run on any thread
            // (including a Tokio worker, if a caller drops a `PgTxn` from inside async code) —
            // must go through `run_isolated` exactly like every other call. See the module docs.
            run_isolated(move || {
                let _ = conn.batch_execute("ROLLBACK");
            });
        }
    }
}

fn range_sql(qualified: &str, spec: &RangeSpec) -> (String, Vec<Vec<u8>>) {
    use std::ops::Bound;

    let mut sql = format!("SELECT k, v FROM {qualified} WHERE 1 = 1");
    let mut params: Vec<Vec<u8>> = Vec::new();

    match &spec.start {
        Bound::Included(k) => {
            params.push(k.to_vec());
            sql.push_str(&format!(" AND k >= ${}", params.len()));
        }
        Bound::Excluded(k) => {
            params.push(k.to_vec());
            sql.push_str(&format!(" AND k > ${}", params.len()));
        }
        Bound::Unbounded => {}
    }
    match &spec.end {
        Bound::Included(k) => {
            params.push(k.to_vec());
            sql.push_str(&format!(" AND k <= ${}", params.len()));
        }
        Bound::Excluded(k) => {
            params.push(k.to_vec());
            sql.push_str(&format!(" AND k < ${}", params.len()));
        }
        Bound::Unbounded => {}
    }
    sql.push_str(if spec.reverse {
        " ORDER BY k DESC"
    } else {
        " ORDER BY k ASC"
    });
    if let Some(limit) = spec.limit {
        // `limit` is a plain `usize` formatted directly, never user SQL text, so this is not an
        // injection vector.
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    (sql, params)
}

fn run_range(
    conn: &mut postgres::Client,
    qualified: &str,
    spec: &RangeSpec,
) -> Result<Vec<(Bytes, Bytes)>, postgres::Error> {
    let (sql, params) = range_sql(qualified, spec);
    let param_refs: Vec<&(dyn ToSql + Sync)> =
        params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let rows = conn.query(&sql, &param_refs)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let k: Vec<u8> = row.get(0);
            let v: Vec<u8> = row.get(1);
            (Bytes::from(k), Bytes::from(v))
        })
        .collect())
}

fn multi_get_impl(
    conn: &mut postgres::Client,
    qualified: &str,
    keys: &[&[u8]],
) -> Result<Vec<Option<Bytes>>, postgres::Error> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let owned: Vec<Vec<u8>> = keys.iter().map(|k| k.to_vec()).collect();
    let sql = format!("SELECT k, v FROM {qualified} WHERE k = ANY($1)");
    let rows = conn.query(&sql, &[&owned])?;
    let mut found: HashMap<Vec<u8>, Bytes> = HashMap::with_capacity(rows.len());
    for row in rows {
        let k: Vec<u8> = row.get(0);
        let v: Vec<u8> = row.get(1);
        found.insert(k, Bytes::from(v));
    }
    Ok(keys.iter().map(|k| found.get(*k).cloned()).collect())
}

fn range_result_to_iter<'a>(result: Result<Vec<(Bytes, Bytes)>, KvError>) -> RangeIter<'a> {
    match result {
        Ok(rows) => Box::new(rows.into_iter().map(Ok)) as RangeIter<'a>,
        Err(e) => Box::new(std::iter::once(Err(e) as RangeItem)) as RangeIter<'a>,
    }
}

/// Whether `key` falls inside `spec`'s bounds. Used to overlay [`PgTxn::pending`] writes onto a
/// database range scan, since a buffered insert or update that has not been flushed yet would
/// otherwise be invisible to the transaction's own `range` calls.
fn key_in_range(key: &[u8], spec: &RangeSpec) -> bool {
    use std::ops::Bound;

    let after_start = match &spec.start {
        Bound::Included(s) => key >= s.as_ref(),
        Bound::Excluded(s) => key > s.as_ref(),
        Bound::Unbounded => true,
    };
    let before_end = match &spec.end {
        Bound::Included(e) => key <= e.as_ref(),
        Bound::Excluded(e) => key < e.as_ref(),
        Bound::Unbounded => true,
    };
    after_start && before_end
}

impl KvRead for PgSnapshot {
    type Keyspace = PgKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        let qualified = keyspace.qualified.clone();
        self.with_conn(move |conn| {
            let sql = format!("SELECT v FROM {qualified} WHERE k = $1");
            conn.query_opt(&sql, &[&key])
        })
        .map(|row| {
            row.map(|r| {
                let v: Vec<u8> = r.get(0);
                Bytes::from(v)
            })
        })
    }

    fn multi_get(
        &self,
        keyspace: &Self::Keyspace,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Bytes>>, KvError> {
        self.with_conn(|conn| multi_get_impl(conn, &keyspace.qualified, keys))
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        let result = self.with_conn(|conn| run_range(conn, &keyspace.qualified, &spec));
        range_result_to_iter(result)
    }
}

impl KvRead for PgTxn {
    type Keyspace = PgKeyspace;

    fn get(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        if let Some(pending) = self
            .pending
            .get(&(keyspace.qualified.clone(), key.to_vec()))
        {
            return Ok(pending.clone().map(Bytes::from));
        }
        let qualified = keyspace.qualified.clone();
        self.with_conn(move |conn| {
            let sql = format!("SELECT v FROM {qualified} WHERE k = $1");
            conn.query_opt(&sql, &[&key])
        })
        .map(|row| {
            row.map(|r| {
                let v: Vec<u8> = r.get(0);
                Bytes::from(v)
            })
        })
    }

    fn multi_get(
        &self,
        keyspace: &Self::Keyspace,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Bytes>>, KvError> {
        // Resolve whatever this transaction has already buffered locally; only the remainder
        // needs a round trip, and reordering back to the caller's order at the end.
        let mut results: Vec<Option<Option<Bytes>>> = vec![None; keys.len()];
        let mut unresolved_keys: Vec<&[u8]> = Vec::new();
        let mut unresolved_positions: Vec<usize> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            match self
                .pending
                .get(&(keyspace.qualified.clone(), key.to_vec()))
            {
                Some(pending) => results[i] = Some(pending.clone().map(Bytes::from)),
                None => {
                    unresolved_keys.push(key);
                    unresolved_positions.push(i);
                }
            }
        }
        if !unresolved_keys.is_empty() {
            let fetched =
                self.with_conn(|conn| multi_get_impl(conn, &keyspace.qualified, &unresolved_keys))?;
            for (pos, value) in unresolved_positions.into_iter().zip(fetched) {
                results[pos] = Some(value);
            }
        }
        Ok(results
            .into_iter()
            .map(|r| r.expect("every index was resolved from pending or the database"))
            .collect())
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        let mut rows = match self.with_conn(|conn| run_range(conn, &keyspace.qualified, &spec)) {
            Ok(rows) => rows,
            Err(e) => return Box::new(std::iter::once(Err(e))),
        };
        // Overlay this transaction's own not-yet-flushed writes: drop any database row a pending
        // write shadows, then add pending inserts/updates that land inside the scanned range
        // (tombstones need no action beyond the drop, since a fresh insert can't already be in
        // `rows` unless the database also independently has that key from another committed
        // writer, which the drop already handles).
        rows.retain(|(k, _)| {
            !self
                .pending
                .contains_key(&(keyspace.qualified.clone(), k.to_vec()))
        });
        for ((table, key), value) in &self.pending {
            if let Some(v) = value
                && *table == keyspace.qualified
                && key_in_range(key, &spec)
            {
                rows.push((Bytes::from(key.clone()), Bytes::from(v.clone())));
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        if spec.reverse {
            rows.reverse();
        }
        if let Some(limit) = spec.limit {
            rows.truncate(limit);
        }
        Box::new(rows.into_iter().map(Ok))
    }
}

impl KvWrite for PgTxn {
    fn put(&mut self, keyspace: &Self::Keyspace, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        limits::check_key(key)?;
        limits::check_value(value)?;
        self.budget.record(key.len() + value.len())?;
        self.pending.insert(
            (keyspace.qualified.clone(), key.to_vec()),
            Some(value.to_vec()),
        );
        self.write_keys
            .push((keyspace.name.to_string(), key.to_vec()));
        Ok(())
    }

    fn delete(&mut self, keyspace: &Self::Keyspace, key: &[u8]) -> Result<(), KvError> {
        limits::check_key(key)?;
        self.budget.record(key.len())?;
        self.pending
            .insert((keyspace.qualified.clone(), key.to_vec()), None);
        self.write_keys
            .push((keyspace.name.to_string(), key.to_vec()));
        Ok(())
    }
}

/// Applies every buffered write in `pending` against `conn`, in one go, immediately before
/// `COMMIT` — see the module docs on why writes are deferred this far rather than applied as each
/// `put`/`delete` call happens.
fn flush_pending(
    conn: &mut postgres::Client,
    pending: &PendingWrites,
) -> Result<(), postgres::Error> {
    for ((qualified, key), value) in pending {
        match value {
            Some(v) => {
                let sql = format!(
                    "INSERT INTO {qualified} (k, v) VALUES ($1, $2) \
                     ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v"
                );
                conn.execute(&sql, &[key, v])?;
            }
            None => {
                let sql = format!("DELETE FROM {qualified} WHERE k = $1");
                conn.execute(&sql, &[key])?;
            }
        }
    }
    Ok(())
}

impl KvBackend for PostgresBackend {
    type Keyspace = PgKeyspace;
    type Snapshot = PgSnapshot;
    type Txn = PgTxn;

    fn keyspace(&self, name: &str) -> Result<Self::Keyspace, KvError> {
        validate_keyspace_name(name)?;
        let mut cache = self
            .inner
            .tables
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(qualified) = cache.get(name) {
            return Ok(PgKeyspace {
                name: Arc::from(name),
                qualified: qualified.clone(),
            });
        }
        let qualified: Arc<str> = Arc::from(format!("\"{}\".\"kv_{name}\"", self.inner.schema));
        let pool = self
            .inner
            .pool
            .as_ref()
            .expect("PostgresBackend used after Inner was dropped, which cannot happen: Inner is owned by an Arc this handle holds a strong reference to");
        let q = qualified.clone();
        run_isolated(move || {
            let mut conn = pool.get().map_err(KvError::backend)?;
            // See `create_if_not_exists_race_free`'s docs ("Concurrent setup"): the same
            // simultaneous-replicas race that hits schema creation in `open` can just as easily
            // hit table creation here, the first time two replicas both open the same keyspace.
            create_if_not_exists_race_free(
                &mut conn,
                advisory_lock_key(&format!("hs_kv_table:{q}")),
                &format!("CREATE TABLE IF NOT EXISTS {q} (k bytea PRIMARY KEY, v bytea NOT NULL)"),
                SqlState::DUPLICATE_TABLE,
            )
            .map_err(pg_error)
        })?;
        cache.insert(name.to_owned(), qualified.clone());
        Ok(PgKeyspace {
            name: Arc::from(name),
            qualified,
        })
    }

    fn snapshot(&self) -> Self::Snapshot {
        let pool = self
            .inner
            .pool
            .as_ref()
            .expect("PostgresBackend used after Inner was dropped, which cannot happen: Inner is owned by an Arc this handle holds a strong reference to");
        let state = run_isolated(move || match pool.get() {
            Ok(mut conn) => {
                match conn.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY") {
                    Ok(()) => SnapState::Ready(Box::new(conn)),
                    // `PgErrorDetail`, not a bare `e.to_string()`: a real `postgres::Error` here
                    // (not the `r2d2::Error` below) would otherwise collapse to the uninformative
                    // `"db error"` for any server-reported failure — see `PgErrorDetail`'s docs.
                    Err(e) => SnapState::Failed(PgErrorDetail(e).to_string()),
                }
            }
            // `r2d2::Error`, not `postgres::Error`: no SQLSTATE/message to recover here, since
            // `r2d2` itself already discarded the structured connect error into a plain `String`
            // before this closure ever saw it (see the module docs, "Concurrent setup" /
            // `PgErrorDetail`'s docs) — `.to_string()` is already the most detail available.
            Err(e) => SnapState::Failed(e.to_string()),
        });
        PgSnapshot {
            state: Mutex::new(state),
        }
    }

    fn begin(&self) -> Result<Self::Txn, KvError> {
        let pool = self
            .inner
            .pool
            .as_ref()
            .expect("PostgresBackend used after Inner was dropped, which cannot happen: Inner is owned by an Arc this handle holds a strong reference to");
        let conn = run_isolated(move || {
            let mut conn = pool.get().map_err(KvError::backend)?;
            conn.batch_execute(&format!(
                "BEGIN ISOLATION LEVEL SERIALIZABLE; SET LOCAL lock_timeout = '{WRITE_LOCK_TIMEOUT}'"
            ))
            .map_err(pg_error)?;
            Ok::<_, KvError>(conn)
        })?;
        Ok(PgTxn {
            conn: Mutex::new(Some(conn)),
            pending: HashMap::new(),
            write_keys: Vec::new(),
            budget: TxnBudget::default(),
        })
    }

    fn commit(&self, txn: Self::Txn) -> Result<Result<(), Conflict>, KvError> {
        let mut guard = txn.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(mut conn) = guard.take() else {
            // A read earlier in this transaction already hit a serialization conflict
            // (`PgTxn::with_conn` rolled back and cleared the slot then), and the caller
            // committed anyway instead of propagating that error. Nothing left to commit; report
            // it the same way that earlier read did.
            drop(guard);
            return Ok(Err(Conflict));
        };
        drop(guard);
        // Flushing the buffered writes, committing, and (on failure) rolling back are all real
        // `postgres` calls — one isolated-thread trip covers the whole sequence.
        let pending = &txn.pending;
        let outcome = run_isolated(move || {
            let result =
                flush_pending(&mut conn, pending).and_then(|()| conn.batch_execute("COMMIT"));
            if result.is_err() {
                let _ = conn.batch_execute("ROLLBACK");
            }
            result
        });
        match outcome {
            Ok(()) => {
                for (ks, key) in &txn.write_keys {
                    self.inner.hub.notify(ks, key);
                }
                Ok(Ok(()))
            }
            Err(e) => {
                if is_serialization_conflict(&e) {
                    Ok(Err(Conflict))
                } else {
                    Err(pg_error(e))
                }
            }
        }
    }

    fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Watch {
        self.inner.hub.watch(&keyspace.name, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are pure, offline unit tests of the identifier validators — no database needed,
    // unlike the rest of this backend's test coverage (`tests/postgres_conformance.rs`), which is
    // exactly why the dotted-name regression this covers went unnoticed: nothing exercised
    // `validate_keyspace_name` in isolation before, and the one integration test that opens a
    // keyspace always used a bare, undotted name.

    #[test]
    fn keyspace_name_accepts_a_dotted_prefix_like_other_crates_use() {
        // Real names other crates already use (see crates/hs-auth/src/store/tables.rs and
        // friends): a single crate-scoped prefix, a dot, then the table name.
        for name in [
            "hs_auth.users",
            "hs_auth.access_tokens",
            "hs_room.events",
            "hs_e2e.device_keys",
            "hs_push.rules",
            "plain_no_dot_at_all",
            "a.b.c",
        ] {
            assert!(
                validate_keyspace_name(name).is_ok(),
                "{name:?} should be a valid keyspace name"
            );
        }
    }

    #[test]
    fn keyspace_name_rejects_anything_that_could_escape_the_quoted_identifier() {
        for name in [
            "",
            ".",
            ".leading_dot",
            "trailing_dot.",
            "double..dot",
            "has space",
            "has\"quote",
            "has'quote",
            "has;semicolon",
            "has\\backslash",
            "1starts_with_digit",
            ".starts_with_dot_segment.ok",
        ] {
            assert!(
                validate_keyspace_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn keyspace_name_enforces_the_overall_length_limit_even_with_dots() {
        let short_segments = "a.".repeat(30); // well over 55 bytes once joined
        assert!(validate_keyspace_name(&short_segments).is_err());
        let exactly_at_limit = "a".repeat(55);
        assert!(validate_keyspace_name(&exactly_at_limit).is_ok());
        let one_over = "a".repeat(56);
        assert!(validate_keyspace_name(&one_over).is_err());
    }

    #[test]
    fn schema_name_stays_undotted_only() {
        assert!(validate_ident("public").is_ok());
        assert!(validate_ident("hs_kv_test_123").is_ok());
        assert!(
            validate_ident("hs_auth.users").is_err(),
            "a schema name is never dotted, unlike a keyspace name"
        );
    }
}
