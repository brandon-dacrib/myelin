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
//! are validated as safe SQL identifiers (`^[A-Za-z_][A-Za-z0-9_]*$`, at most 55 bytes) before
//! being interpolated into DDL/DML, since PostgreSQL has no way to bind an identifier as a query
//! parameter; there is no user-controlled input in this path (keyspace names come from
//! `hs-tables`), but the check is cheap insurance.
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

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

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
fn validate_ident(name: &str) -> Result<(), KvError> {
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = name
        .chars()
        .skip(1)
        .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if name.is_empty() || name.len() > 55 || !first_ok || !rest_ok {
        return Err(KvError::InvalidKeyspaceName(name.to_owned()));
    }
    Ok(())
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
    pool: PgPool,
    schema: String,
    hub: Hub,
    tables: Mutex<HashMap<String, Arc<str>>>,
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
        let config: postgres::Config = dsn.parse().map_err(KvError::backend)?;
        let manager = PgManager::new(config, NoTls);
        let pool = Pool::builder()
            .max_size(16)
            // Fail fast rather than r2d2's 30-second default: a caller (including a reachability
            // check like the one `postgres_conformance.rs` uses to decide whether to skip) should
            // not have to wait half a minute to learn there is no server. Production callers that
            // want resilience against a slow-starting database retry `open`/individual operations
            // at a higher level (readiness probes, `hs serve`'s own startup retry), not by waiting
            // longer here.
            .connection_timeout(std::time::Duration::from_secs(3))
            .build(manager)
            .map_err(KvError::backend)?;
        {
            let mut conn = pool.get().map_err(KvError::backend)?;
            conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
                .map_err(KvError::backend)?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                pool,
                schema: schema.to_owned(),
                hub: Hub::new(),
                tables: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Drops this backend's whole schema, including every keyspace's table. Only ever used by
    /// tests to clean up after themselves; production callers have no reason to call this.
    ///
    /// # Errors
    /// Returns [`KvError::Backend`] on a connection or query failure.
    pub fn drop_schema_for_test(&self) -> Result<(), KvError> {
        let mut conn = self.inner.pool.get().map_err(KvError::backend)?;
        conn.batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE",
            self.inner.schema
        ))
        .map_err(KvError::backend)
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
            let _ = conn.batch_execute("ROLLBACK");
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
    fn with_conn<T>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T, postgres::Error>,
    ) -> Result<T, KvError> {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let conn = guard.as_mut().expect(
            "PgTxn used after commit (hs-kv only calls with_conn before commit consumes the txn)",
        );
        match f(conn) {
            Ok(value) => Ok(value),
            Err(e) if is_serialization_conflict(&e) => {
                let _ = conn.batch_execute("ROLLBACK");
                *guard = None;
                Err(KvError::MidTransactionConflict)
            }
            Err(e) => Err(KvError::backend(e)),
        }
    }
}

impl Drop for PgTxn {
    fn drop(&mut self) {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mut conn) = guard.take() {
            let _ = conn.batch_execute("ROLLBACK");
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
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let conn = match &mut *state {
            SnapState::Ready(conn) => conn,
            SnapState::Failed(msg) => return Err(KvError::backend(DeferredError(msg.clone()))),
        };
        let sql = format!("SELECT v FROM {} WHERE k = $1", keyspace.qualified);
        let row = conn.query_opt(&sql, &[&key]).map_err(KvError::backend)?;
        Ok(row.map(|r| {
            let v: Vec<u8> = r.get(0);
            Bytes::from(v)
        }))
    }

    fn multi_get(
        &self,
        keyspace: &Self::Keyspace,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Bytes>>, KvError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let conn = match &mut *state {
            SnapState::Ready(conn) => conn,
            SnapState::Failed(msg) => return Err(KvError::backend(DeferredError(msg.clone()))),
        };
        multi_get_impl(conn, &keyspace.qualified, keys).map_err(KvError::backend)
    }

    fn range<'a>(&'a self, keyspace: &Self::Keyspace, spec: RangeSpec) -> RangeIter<'a> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let result = match &mut *state {
            SnapState::Ready(conn) => {
                run_range(conn, &keyspace.qualified, &spec).map_err(KvError::backend)
            }
            SnapState::Failed(msg) => Err(KvError::backend(DeferredError(msg.clone()))),
        };
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
        validate_ident(name)?;
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
        let mut conn = self.inner.pool.get().map_err(KvError::backend)?;
        conn.batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {qualified} (k bytea PRIMARY KEY, v bytea NOT NULL)"
        ))
        .map_err(KvError::backend)?;
        cache.insert(name.to_owned(), qualified.clone());
        Ok(PgKeyspace {
            name: Arc::from(name),
            qualified,
        })
    }

    fn snapshot(&self) -> Self::Snapshot {
        let state = match self.inner.pool.get() {
            Ok(mut conn) => {
                match conn.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY") {
                    Ok(()) => SnapState::Ready(Box::new(conn)),
                    Err(e) => SnapState::Failed(e.to_string()),
                }
            }
            Err(e) => SnapState::Failed(e.to_string()),
        };
        PgSnapshot {
            state: Mutex::new(state),
        }
    }

    fn begin(&self) -> Result<Self::Txn, KvError> {
        let mut conn = self.inner.pool.get().map_err(KvError::backend)?;
        conn.batch_execute(&format!(
            "BEGIN ISOLATION LEVEL SERIALIZABLE; SET LOCAL lock_timeout = '{WRITE_LOCK_TIMEOUT}'"
        ))
        .map_err(KvError::backend)?;
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
        let outcome =
            flush_pending(&mut conn, &txn.pending).and_then(|()| conn.batch_execute("COMMIT"));
        match outcome {
            Ok(()) => {
                for (ks, key) in &txn.write_keys {
                    self.inner.hub.notify(ks, key);
                }
                Ok(Ok(()))
            }
            Err(e) => {
                let _ = conn.batch_execute("ROLLBACK");
                if is_serialization_conflict(&e) {
                    Ok(Err(Conflict))
                } else {
                    Err(KvError::backend(e))
                }
            }
        }
    }

    fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> Watch {
        self.inner.hub.watch(&keyspace.name, key)
    }
}
