//! `hs-kv`: an ordered, transactional key-value abstraction with pluggable backends.
//!
//! Owned by track 01 (`docs/workstreams/01-storage-engine.md`). This is the most-consumed
//! interface in the project (`docs/workstreams/README.md`'s seam table): every other track stores
//! its data through [`KvBackend`] and the typed layer built on it in `hs-tables`, without knowing
//! which backend is underneath.
//!
//! # The semantic contract
//!
//! This section is normative. A backend that cannot uphold it must not implement [`KvBackend`];
//! callers are entitled to rely on every guarantee below regardless of which backend is
//! configured.
//!
//! ## Keyspaces
//!
//! A keyspace ([`KvBackend::keyspace`]) is an independently ordered namespace: one Fjall
//! partition, one PostgreSQL table, one in-memory `BTreeMap`. Keyspace names are chosen by
//! `hs-tables`, not derived from user data; they are ASCII, non-empty, and backends may cap their
//! length (Fjall: 255 bytes). Keys are ordered only *within* a keyspace; there is no cross-keyspace
//! ordering guarantee, so multi-table iteration is the caller's job.
//!
//! ## Ordering
//!
//! Keys order by plain byte-wise (unsigned lexicographic) comparison of their raw bytes.
//! `hs-kv` never interprets key contents. Making that byte order line up with a typed tuple's
//! natural order (integers numeric, strings lexicographic) is `hs-tables`'s job, not this crate's:
//! it encodes tuples so that byte order equals tuple order, fixed-width big-endian for integers
//! (a varint scheme without a length prefix does not preserve order across byte-length boundaries,
//! which is why `hs-tables` does not use one) and a null-escaped terminator for strings and blobs.
//!
//! ## Reads: snapshots and transactions
//!
//! [`KvBackend::snapshot`] returns a read-only, repeatable-read view fixed at the instant it is
//! taken: every read against it, no matter how much later, sees exactly that instant, unaffected
//! by concurrent or later writes. It is not serializable with anything (there is nothing for a
//! read-only view to conflict with) and never blocks a writer or is blocked by one.
//!
//! [`KvBackend::begin`] returns a write transaction that also starts from a repeatable-read
//! snapshot, but additionally validates at commit time ([`KvBackend::commit`]) that nothing it
//! read has been written by a transaction that committed in the meantime. Every backend implements
//! this as full **serializable snapshot isolation (SSI)**: point reads, multi-gets and range scans
//! all extend the transaction's read set, and a concurrent write to a key in that set — including a
//! write of a *new* key that falls inside a previously scanned range, i.e. a phantom — causes the
//! commit to report [`Conflict`] instead of applying. This is deliberately the strongest level, not
//! "read committed" or "snapshot isolation" as PostgreSQL calls it (which permits write skew): the
//! conformance suite's write-skew, lost-update and phantom-read cases are the executable version of
//! this paragraph, and every backend must pass them identically.
//!
//! A transaction that never calls [`KvWrite::put`], [`KvWrite::delete`] or
//! [`KvWrite::atomic_add`] has nothing to protect and always commits successfully, however stale
//! its reads are by the time it commits — there is no such thing as a read-only conflict. Only a
//! transaction with at least one write can lose an SSI race.
//!
//! A transaction that reads a key and later commits successfully is therefore a guarantee that the
//! key did not change out from under it — this is how a caller fences on a value. Track 03's lease
//! and epoch fencing is exactly this pattern: read the epoch inside the transaction that renews or
//! acts on a lease, and let a concurrent epoch bump abort the commit. No separate fencing primitive
//! exists in this trait because none is needed.
//!
//! [`Conflict`] is not a [`KvError`]: it is the routine, expected outcome of optimistic concurrency
//! control, returned as `Ok(Err(Conflict))` from [`KvBackend::commit`] so it cannot be accidentally
//! swallowed by `?` on the outer `Result`. [`transact`] is the retry helper: give it a closure that
//! rebuilds its writes from scratch on every attempt (never assume state left over from a
//! conflicted attempt), and it retries with jittered backoff up to a bounded number of times,
//! surfacing [`KvError::RetriesExhausted`] if every attempt conflicted.
//!
//! ## Keep transactions short
//!
//! A backend is entitled to hold resources (MVCC garbage, a PostgreSQL snapshot's XID horizon, a
//! Fjall snapshot nonce) for as long as a transaction or a snapshot is open. A long-lived
//! transaction is not just slow, it prevents the backend from reclaiming space and, on PostgreSQL,
//! can stall `VACUUM` cluster-wide. Do not hold a transaction across network I/O, `await` points
//! that are not the store itself, or anything measured in more than single-digit milliseconds. If a
//! caller needs a stable read across a longer operation, it should use [`KvBackend::snapshot`], not
//! an open write transaction.
//!
//! ## Size limits
//!
//! - [`MAX_KEY_BYTES`] (64 KiB): the hard limit on an encoded key, inherited from Fjall's own
//!   limit so the same key always fits every backend. Keys should be far smaller in practice —
//!   `hs-tables`'s interned short IDs exist specifically to keep hot-path keys under a few dozen
//!   bytes.
//! - [`MAX_VALUE_BYTES`] (32 MiB): a documented, enforced ceiling per value. Values larger than a
//!   few KiB (event JSON, media metadata blobs) should use a backend's key-value separation
//!   feature (Fjall: `KvSeparationOptions`) rather than approach this limit.
//! - [`MAX_TXN_MUTATIONS`] / [`MAX_TXN_BYTES`]: a transaction accumulating more than 10,000
//!   mutated keys or 8 MiB of mutation payload returns [`KvError::TransactionTooLarge`] rather than
//!   growing without bound. Callers that need to write more than this in one logical operation
//!   (a room import, a bulk migration) must batch it into multiple transactions themselves; there
//!   is no cross-transaction atomicity primitive in this trait, by design — the room actor (track
//!   04) is the layer that owns "apply this event and its state change" as a unit small enough to
//!   fit in one transaction.
//!
//! An empty key is always rejected; an empty value is always allowed (it is a valid zero-length
//! value, distinct from the key being absent).
//!
//! ## Retry rules
//!
//! Only [`Conflict`] is retryable, and only by re-running the whole closure — a transaction that
//! conflicts has had every one of its writes discarded, so partial progress can never be resumed.
//! [`transact`] enforces this shape (`FnMut` is called fresh every attempt) so the easy-to-get-wrong
//! pattern of half-applying a retried transaction is not expressible. Backend errors
//! ([`KvError::Backend`]) are not retried automatically: they represent the store itself failing
//! (I/O, a lost connection), and blindly retrying those can turn a transient backend outage into a
//! retry storm. Callers that want backend-error retries build that policy on top of [`transact`]
//! explicitly (with its own backoff), not inside it.
//!
//! ## Watches are hints, not guarantees
//!
//! [`KvBackend::watch`] registers best-effort interest in a key and returns a [`watch::Watch`] that
//! a caller can block on. It answers only "this key was written since I last checked" — never what
//! changed, never how many times, never with a guaranteed delivery, and never across a process
//! restart (the in-memory and Fjall backends implement it as pure in-process state; a distributed
//! backend may additionally miss notifications during a leader failover). A caller that needs
//! correctness — not just responsiveness — from "did this change" must read the key inside a
//! transaction, as above; a watch exists only to avoid polling while waiting to re-check.
//!
//! # The trait
//!
//! [`KvBackend`] ties it together: [`KvBackend::keyspace`] to name a namespace,
//! [`KvBackend::snapshot`] for read-only views, [`KvBackend::begin`] and [`KvBackend::commit`] (or
//! [`transact`]) for read-write transactions, [`KvBackend::watch`] for hints. [`KvRead`] (get,
//! `multi_get`, `range`) is implemented by both snapshots and transactions; [`KvWrite`] (put,
//! delete, `atomic_add`) only by transactions.
//!
//! # Backends
//!
//! - [`memory::MemoryBackend`]: the reference implementation (this crate). Full SSI, no
//!   persistence. Used by the conformance suite and by every other track's unit tests.
//! - [`fjall_backend::FjallBackend`]: the embedded, single-node production backend (Fjall 3,
//!   optimistic serializable transactions, one keyspace per table, LZ4 compression, optional
//!   key-value separation for large values).
//!
//! A PostgreSQL backend (the cluster default) and a SlateDB backend (the diskless-cluster option)
//! are described in `PLAN.md` section 6.5 and are future work on this crate; see
//! `docs/status/01-storage-engine.md` for what has actually landed.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod limits;
mod retry;
mod traits;
pub mod watch;

pub mod conformance;
pub mod fjall_backend;
pub mod memory;

pub use error::{Conflict, KvError};
pub use retry::{TransactConfig, transact};
pub use traits::{
    KeyspaceHandle, KvBackend, KvPair, KvRead, KvWrite, RangeItem, RangeIter, RangeSpec,
};
pub use watch::{Hub as WatchHub, Watch, WatchOutcome};

/// The hard limit on an encoded key, in bytes. See the crate-level contract.
pub const MAX_KEY_BYTES: usize = 65_536;
/// The enforced ceiling on a value, in bytes. See the crate-level contract.
pub const MAX_VALUE_BYTES: usize = 32 * 1024 * 1024;
/// The maximum number of mutated keys a single transaction may accumulate before it must commit
/// or split. See the crate-level contract.
pub const MAX_TXN_MUTATIONS: usize = 10_000;
/// The maximum total mutation payload, in bytes, a single transaction may accumulate. See the
/// crate-level contract.
pub const MAX_TXN_BYTES: usize = 8 * 1024 * 1024;
