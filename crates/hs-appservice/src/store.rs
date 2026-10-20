//! The `hs-tables`/`hs-kv` storage layer: the registry row, health row, and per-appservice
//! transaction queue, plus the keyspace handles that own them.
//!
//! Generic over `B: hs_kv::KvBackend`, following `hs-media`'s pattern
//! (`crates/hs-media/src/metadata.rs`): every method here is synchronous (`hs-kv` transactions may
//! not `.await`), called directly from async handlers/scheduler tasks — cheap against
//! `MemoryBackend` in every test in this crate, and fine against a production `FjallBackend` until
//! profiling says otherwise (no different code path is available yet; see that file's docs for the
//! `spawn_blocking` escape hatch this crate would reach for first).

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};
use hs_tables::index::{IndexDef, lookup, maintain_index};
use hs_tables::key::TupleKey;
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::AppserviceError;
use crate::namespace::NamespacesSpec;
use crate::registration::Registration;

/// One registry row: a registration plus registry-only bookkeeping (pause state, timestamps).
/// The serializable twin of [`crate::registration::Registration`] — namespaces are stored
/// uncompiled ([`NamespacesSpec`]) since a compiled pattern does not (de)serialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppserviceRow {
    /// Primary key.
    pub id: String,
    /// `None` (`url: null`) marks a double-puppeting registration; see [`crate::scheduler`].
    pub url: Option<String>,
    /// Authenticates the appservice's own client-server calls.
    pub as_token: String,
    /// Authenticates the homeserver's calls to the appservice.
    pub hs_token: String,
    /// The appservice's bot user localpart.
    pub sender_localpart: String,
    /// Whether ordinary rate limits apply.
    pub rate_limited: bool,
    /// Namespace declarations, uncompiled.
    pub namespaces: NamespacesSpec,
    /// Third-party protocol IDs.
    pub protocols: Vec<String>,
    /// MSC2409 ephemeral event delivery, stable spelling.
    pub receive_ephemeral: bool,
    /// MSC2409 ephemeral event delivery, legacy spelling. See
    /// [`crate::registration::Registration::push_ephemeral_legacy`].
    pub push_ephemeral_legacy: bool,
    /// MSC3202 device fields and device masquerading.
    pub msc3202: bool,
    /// MSC4190 device management without login.
    pub msc4190: bool,
    /// Unrecognized registration fields, preserved for export.
    #[serde(default)]
    pub extra: Map<String, Value>,
    /// True if delivery is paused (`hs appservice pause`): the scheduler still enqueues, but
    /// never dequeues, so nothing is lost, only held back.
    #[serde(default)]
    pub paused: bool,
    /// When this row was first registered.
    pub created_at_ms: u64,
    /// When this row was last modified (pause/resume/update/token rotation all bump this).
    pub updated_at_ms: u64,
}

impl AppserviceRow {
    /// Builds a fresh row from a parsed [`Registration`], stamping both timestamps to `now_ms`.
    #[must_use]
    pub fn from_registration(reg: &Registration, now_ms: u64) -> Self {
        Self {
            id: reg.id.clone(),
            url: reg.url.clone(),
            as_token: reg.as_token.clone(),
            hs_token: reg.hs_token.clone(),
            sender_localpart: reg.sender_localpart.clone(),
            rate_limited: reg.rate_limited,
            namespaces: NamespacesSpec::from(&reg.namespaces),
            protocols: reg.protocols.clone(),
            receive_ephemeral: reg.receive_ephemeral,
            push_ephemeral_legacy: reg.push_ephemeral_legacy,
            msc3202: reg.msc3202,
            msc4190: reg.msc4190,
            extra: reg.extra.clone(),
            paused: false,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }

    /// Renders this row back into a [`Registration`], recompiling its namespace patterns.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Namespace`] if a stored pattern somehow no longer compiles
    /// (only possible if the store was edited out of band; patterns are validated on every write
    /// through this crate).
    pub fn to_registration(&self) -> Result<Registration, AppserviceError> {
        Ok(Registration {
            id: self.id.clone(),
            url: self.url.clone(),
            as_token: self.as_token.clone(),
            hs_token: self.hs_token.clone(),
            sender_localpart: self.sender_localpart.clone(),
            rate_limited: self.rate_limited,
            namespaces: self.namespaces.compile()?,
            protocols: self.protocols.clone(),
            receive_ephemeral: self.receive_ephemeral,
            push_ephemeral_legacy: self.push_ephemeral_legacy,
            msc3202: self.msc3202,
            msc4190: self.msc4190,
            extra: self.extra.clone(),
        })
    }
}

/// Per-appservice health and delivery bookkeeping, answering the admin API's
/// `GET /appservices/{id}/health` (`crates/hs-admin/openapi/openapi.yaml`'s `AppServiceHealth`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthRow {
    /// When the last `/ping` (either direction) was attempted.
    pub last_ping_at_ms: Option<u64>,
    /// Whether that last ping succeeded.
    pub last_ping_success: Option<bool>,
    /// When a transaction was last delivered successfully.
    pub last_success_at_ms: Option<u64>,
    /// The queue sequence number of the last successfully delivered transaction — the scheduler's
    /// "resume from here" cursor after a restart.
    pub last_success_seq: Option<u64>,
    /// Consecutive delivery failures since the last success. Reset to `0` on success. Compared
    /// against `hs-config`'s `AppservicesConfig::tracking_failure_threshold` to decide `degraded`
    /// vs `down`.
    pub consecutive_failures: u32,
    /// The most recent delivery error, if any.
    pub last_error: Option<String>,
    /// When the last delivery attempt failed. A ping that succeeds after it says the
    /// appservice answers again, and its health is judged by that ping.
    #[serde(default)]
    pub last_failure_at_ms: Option<u64>,
    /// The last `m.room_key.withheld` delivered to one of this appservice's users (a client
    /// refused to share a room's keys with the bridge, so the bridge could not read a message),
    /// recorded by the scheduler as it delivers. `None` until one has been.
    #[serde(default)]
    pub last_key_withheld: Option<KeyWithheldRow>,
}

/// One `m.room_key.withheld` as delivered to an appservice, the admin API's `KeyWithheld`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyWithheldRow {
    /// When it was delivered.
    pub at_ms: u64,
    /// The client's user, who withheld.
    pub sender: String,
    /// The event's `code` (`m.unverified`, `m.blacklisted`, `m.unauthorised`, `m.unavailable`,
    /// `m.no_olm`).
    pub code: String,
    /// The event's `reason`, when it carried one.
    pub reason: Option<String>,
    /// The event's `room_id`, when it carried one.
    pub room_id: Option<String>,
    /// The appservice user it was addressed to.
    pub to_user_id: String,
    /// The device it was addressed to.
    pub to_device_id: String,
}

/// One transaction's delivery status, as stored in the per-appservice queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueStatus {
    /// Waiting to be sent (or retried).
    Pending,
    /// Delivered and acknowledged; kept briefly for backlog-age reporting, then reaped.
    Delivered,
    /// Exceeded the retry budget; held for operator replay
    /// (`POST /appservices/{id}/replay`).
    DeadLettered,
}

/// One row of the per-appservice transaction queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTransaction {
    /// The queue sequence number (also the transaction id sent on the wire, so retries are
    /// idempotent — the spec requires the homeserver reuse the same `txnId` on retry).
    pub seq: u64,
    /// The transaction body, pre-built with both stable and legacy key spellings
    /// (`crate::transaction::Transaction`), stored as JSON exactly as it will be sent.
    pub body: Value,
    /// When this entry was enqueued.
    pub enqueued_at_ms: u64,
    /// Delivery attempts made so far.
    pub attempts: u32,
    /// Not retried before this time (backoff).
    pub next_attempt_at_ms: u64,
    /// Current status.
    pub status: QueueStatus,
    /// The most recent delivery error, if any.
    pub last_error: Option<String>,
    /// The transaction id this entry was last sent under, set when an attempt fails. Every
    /// entry of a failed batch carries the batch's id, and the retry sends exactly those entries
    /// under it again, whatever has been queued since: the spec's idempotency contract, which a
    /// bridge that took the first attempt and answered too late relies on. `None` until the
    /// first failure, and again after a replay (a replay is a new composition, so a new id).
    #[serde(default)]
    pub txn_id: Option<u64>,
    /// When the first failed attempt since the entry was queued (or replayed) was made. A
    /// transient failure (unreachable, timed out, a 5xx, 408 or 429) dead-letters an entry only
    /// once it has been failing for `SchedulerConfig::give_up_after_ms`, not after a count of
    /// attempts: a bridge restarting or catching up is not one that refuses its transactions.
    #[serde(default)]
    pub first_failed_at_ms: Option<u64>,
}

/// [`AppserviceStore::queue_summary`]: one appservice's queue in three numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueSummary {
    /// Entries waiting to be sent, those backing off after a failure included.
    pub pending: u64,
    /// Entries that spent their retry budget and wait for an operator to replay or discard
    /// them.
    pub dead_lettered: u64,
    /// When the oldest waiting entry was queued; `None` when nothing waits.
    pub oldest_pending_at_ms: Option<u64>,
}

/// The [`AppserviceStore`] `pump_meta` row whose presence means the pump has started before.
const PUMP_STARTED: &str = "started";

/// The [`AppserviceStore`] `pump_meta` row whose presence means every queue entry has its
/// [`QueueHead`] (written by [`AppserviceStore::open`] the first time it finds it missing).
const QUEUE_HEADS_BUILT: &str = "queue_heads";

/// How many heads [`AppserviceStore::open`] writes per transaction when it builds them.
const QUEUE_HEADS_PER_TXN: usize = 512;

/// How many heads one range read takes when a queue is walked a piece at a time: the PostgreSQL
/// backend answers a range with every row at once, so an unbounded read of a queue of 40,000
/// is 40,000 rows in one reply.
const QUEUE_HEADS_PER_READ: usize = 512;

/// How many entries [`AppserviceStore::requeue_dead_lettered`] rewrites per transaction: each
/// rewrite carries its body, and a transaction is limited to [`hs_kv::MAX_TXN_BYTES`].
const REQUEUES_PER_TXN: usize = 64;

/// [`AppserviceStore::backlog_page`]: one page of an appservice's backlog, oldest first.
#[derive(Debug, Clone, Default)]
pub struct QueuePage {
    /// The page's entries, with their bodies, in sequence order.
    pub entries: Vec<QueuedTransaction>,
    /// The last sequence number on this page when there is more backlog after it: what the next
    /// page starts after. `None` on the last page.
    pub next_after: Option<u64>,
}

/// A queue entry's bookkeeping without its body: what the scheduler picks the next batch by
/// and what the queue gauges count. Kept beside each [`QueuedTransaction`] (`txn_queue_head`),
/// written in the same transaction, so that neither has to read every body. On 2026-10-13 the
/// demo's iMessage queue held ~36,000 transactions while its bridge was off; every retry and
/// every scrape read them all, and the server stopped answering its liveness probe.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct QueueHead {
    enqueued_at_ms: u64,
    next_attempt_at_ms: u64,
    status: QueueStatus,
}

impl QueueHead {
    fn of(entry: &QueuedTransaction) -> Self {
        Self {
            enqueued_at_ms: entry.enqueued_at_ms,
            next_attempt_at_ms: entry.next_attempt_at_ms,
            status: entry.status,
        }
    }
}

/// The keyspace handles and typed accessors for one appservice registry, wired to a `B: KvBackend`.
pub struct AppserviceStore<B: KvBackend> {
    backend: B,
    registry: TypedKeyspace<B::Keyspace, (String,)>,
    by_as_token: IndexDef<B::Keyspace, (String,), (String,)>,
    by_hs_token: IndexDef<B::Keyspace, (String,), (String,)>,
    health: TypedKeyspace<B::Keyspace, (String,)>,
    txn_queue: TypedKeyspace<B::Keyspace, (String, u64)>,
    /// `(appservice id, seq) -> QueueHead`: each `txn_queue` entry's status and times, without
    /// its body.
    txn_queue_head: TypedKeyspace<B::Keyspace, (String, u64)>,
    txn_seq: TypedKeyspace<B::Keyspace, (String,)>,
    /// `room_id -> position`: how far into each room's timeline [`crate::pump`] has read. See
    /// [`AppserviceStore::enqueue_for_room`].
    room_cursor: TypedKeyspace<B::Keyspace, (String,)>,
    /// Facts about the pump as a whole, by name. One today: [`PUMP_STARTED`].
    pump_meta: TypedKeyspace<B::Keyspace, (String,)>,
    /// `(appservice id, stream name) -> position`: how far into each of the server's ephemeral
    /// streams (receipts, presence, to-device, device lists) each appservice has been sent.
    /// See [`AppserviceStore::enqueue_ephemeral`] and [`crate::ephemeral`].
    ephemeral_pos: TypedKeyspace<B::Keyspace, (String, String)>,
    /// `(appservice id, network id, room id) -> ()`: the rooms an appservice published in its
    /// own room directory for one of its networks (`PUT
    /// /_matrix/client/v3/directory/list/appservice/{networkId}/{roomId}`). See
    /// [`AppserviceStore::set_network_room`].
    network_rooms: TypedKeyspace<B::Keyspace, (String, String, String)>,
    /// `(appservice id) -> seq`: no entry of that appservice's queue below `seq` is waiting to
    /// be sent, so [`AppserviceStore::pending_batch`] starts there instead of at the front of
    /// the queue. Raised by `pending_batch` past the dead-lettered and delivered entries it
    /// walked; lowered, in the same transaction, by whatever makes an older entry wait again
    /// ([`AppserviceStore::requeue_dead_lettered`], [`AppserviceStore::put_queue_entry`]).
    queue_floor: TypedKeyspace<B::Keyspace, (String,)>,
    /// How many queue bodies the paged reads ([`AppserviceStore::backlog_page`],
    /// [`AppserviceStore::pending_batch`], [`AppserviceStore::requeue_dead_lettered`]) and how
    /// many heads they have fetched: see [`AppserviceStore::queue_reads`].
    bodies_read: AtomicU64,
    heads_read: AtomicU64,
    /// What [`AppserviceStore::queue_summary`] counted below each queue's floor, so the next
    /// summary reads only what is new there ([`QueueFront`]).
    fronts: Mutex<HashMap<String, QueueFront>>,
    /// Moved on by whatever can take an entry from below a queue's floor without lowering the
    /// floor ([`AppserviceStore::purge_queue`], [`AppserviceStore::requeue_dead_lettered`]):
    /// every remembered [`QueueFront`] is counted again after it.
    front_generation: AtomicU64,
}

/// What [`AppserviceStore::queue_summary`] remembers of one queue's front: below `floor`
/// nothing waits (the floor's own invariant), so what is there are the dead-lettered entries,
/// `dead_lettered` of them, and the delivered ones, deleted -- which an LSM store keeps as
/// tombstones for a while, and which the gauges walked at every scrape, one to six seconds a
/// time on the demo on 2026-10-14 (the iMessage queue had about 31,000 behind its floor).
#[derive(Debug, Clone, Copy)]
struct QueueFront {
    floor: u64,
    dead_lettered: u64,
    /// When the front was last counted from the queue's first entry; it is counted so again
    /// after [`FRONT_RECOUNT`].
    counted: Instant,
    generation: u64,
}

/// How long a remembered [`QueueFront`] is topped up before it is counted from the front again:
/// a bound on how long a count gone wrong behind the store's back (another replica's purge)
/// lasts.
const FRONT_RECOUNT: Duration = Duration::from_secs(600);

/// [`AppserviceStore::queue_reads`]: how much of the queue the paged reads have read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueReads {
    /// Queue entries read with their bodies.
    pub bodies: u64,
    /// Head rows read.
    pub heads: u64,
}

/// What [`AppserviceStore::enqueue_ephemeral`] queues for one appservice: a transaction body,
/// or none when nothing was for it, and the stream positions it has now been sent up to.
#[derive(Debug, Clone)]
pub struct EphemeralDelivery {
    /// The appservice.
    pub appservice_id: String,
    /// The wire body to queue, if there is anything to send.
    pub body: Option<Value>,
    /// `(stream name, position)`: written whether or not there is a body, so that a scanned
    /// range with nothing in it for this appservice is not scanned again.
    pub positions: Vec<(String, u64)>,
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, AppserviceError> {
    serde_json::from_slice(bytes).map_err(|e| AppserviceError::Decode(e.to_string()))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, AppserviceError> {
    serde_json::to_vec(value).map_err(|e| AppserviceError::Decode(e.to_string()))
}

fn row_as_token(_pk: &(String,), value: &[u8]) -> Option<(String,)> {
    decode::<AppserviceRow>(value).ok().map(|r| (r.as_token,))
}

fn row_hs_token(_pk: &(String,), value: &[u8]) -> Option<(String,)> {
    decode::<AppserviceRow>(value).ok().map(|r| (r.hs_token,))
}

impl<B: KvBackend> AppserviceStore<B> {
    /// Opens (creating if necessary) every keyspace this store needs.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] if a keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, AppserviceError> {
        let registry = TypedKeyspace::new(backend.keyspace("hs_appservice.registry")?);
        let by_as_token = IndexDef::new(
            backend.keyspace("hs_appservice.by_as_token")?,
            true,
            row_as_token,
        );
        let by_hs_token = IndexDef::new(
            backend.keyspace("hs_appservice.by_hs_token")?,
            true,
            row_hs_token,
        );
        let health = TypedKeyspace::new(backend.keyspace("hs_appservice.health")?);
        let txn_queue = TypedKeyspace::new(backend.keyspace("hs_appservice.txn_queue")?);
        let txn_queue_head = TypedKeyspace::new(backend.keyspace("hs_appservice.txn_queue_head")?);
        let txn_seq = TypedKeyspace::new(backend.keyspace("hs_appservice.txn_seq")?);
        let room_cursor = TypedKeyspace::new(backend.keyspace("hs_appservice.room_cursor")?);
        let pump_meta = TypedKeyspace::new(backend.keyspace("hs_appservice.pump_meta")?);
        let ephemeral_pos = TypedKeyspace::new(backend.keyspace("hs_appservice.ephemeral_pos")?);
        let network_rooms = TypedKeyspace::new(backend.keyspace("hs_appservice.network_rooms")?);
        let queue_floor = TypedKeyspace::new(backend.keyspace("hs_appservice.queue_floor")?);
        let store = Self {
            backend,
            registry,
            by_as_token,
            by_hs_token,
            health,
            txn_queue,
            txn_queue_head,
            txn_seq,
            room_cursor,
            pump_meta,
            ephemeral_pos,
            network_rooms,
            queue_floor,
            bodies_read: AtomicU64::new(0),
            heads_read: AtomicU64::new(0),
            fronts: Mutex::new(HashMap::new()),
            front_generation: AtomicU64::new(0),
        };
        store.build_queue_heads()?;
        store.forget_queue_floors()?;
        Ok(store)
    }

    /// Forgets every queue floor, so that the first [`AppserviceStore::pending_batch`] for each
    /// appservice after a start reads its queue from the front once. A floor is only a hint, and
    /// a release without floors (a downgrade, then an upgrade again) can have made an older
    /// entry wait again without lowering one.
    fn forget_queue_floors(&self) -> Result<(), AppserviceError> {
        let floors: Vec<(String,)> = {
            let snap = self.backend.snapshot();
            self.queue_floor
                .range(&snap, RangeSpec::full())
                .map(|item| Ok(item?.0))
                .collect::<Result<_, AppserviceError>>()?
        };
        if floors.is_empty() {
            return Ok(());
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            for key in &floors {
                self.queue_floor.delete(txn, key).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Gives every queue entry written before heads existed its [`QueueHead`], once: a store
    /// whose `pump_meta` says so is left alone. Each head is written only if its entry is still
    /// there, in the transaction that checks, so an entry another replica delivers meanwhile
    /// leaves nothing behind.
    fn build_queue_heads(&self) -> Result<(), AppserviceError> {
        let snap = self.backend.snapshot();
        if self
            .pump_meta
            .get(&snap, &(QUEUE_HEADS_BUILT.to_string(),))?
            .is_some()
        {
            return Ok(());
        }
        let mut heads: Vec<((String, u64), Vec<u8>)> = Vec::new();
        for item in self.txn_queue.range(&snap, RangeSpec::full()) {
            let (key, value) = item?;
            let entry: QueueHead = decode(&value)?;
            heads.push((key, encode(&entry)?));
        }
        drop(snap);
        let total = heads.len();
        for chunk in heads.chunks(QUEUE_HEADS_PER_TXN) {
            transact(&self.backend, TransactConfig::default(), |txn| {
                for (key, head) in chunk {
                    if self.txn_queue.get(txn, key).map_err(to_kv)?.is_some() {
                        self.txn_queue_head.put(txn, key, head).map_err(to_kv)?;
                    }
                }
                Ok(())
            })
            .map_err(|e| AppserviceError::Store(e.to_string()))?;
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.pump_meta
                .put(txn, &(QUEUE_HEADS_BUILT.to_string(),), b"1")
                .map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))?;
        if total > 0 {
            tracing::info!(
                entries = total,
                "gave each queued appservice transaction its head row, once"
            );
        }
        Ok(())
    }

    /// The underlying backend, for callers (the scheduler) that need their own transactions
    /// spanning both the queue and other tables.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    // ---- registry rows ----

    /// Reads one row by id.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] or [`AppserviceError::Decode`] on failure.
    pub fn get(&self, id: &str) -> Result<Option<AppserviceRow>, AppserviceError> {
        let snap = self.backend.snapshot();
        match self.registry.get(&snap, &(id.to_string(),))? {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Looks up a row by its `as_token`, via the unique index — this is the projection
    /// `hs-auth`'s [`crate::auth_registry`] adapter uses for every request.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] or [`AppserviceError::Decode`] on failure.
    pub fn get_by_as_token(
        &self,
        as_token: &str,
    ) -> Result<Option<AppserviceRow>, AppserviceError> {
        let snap = self.backend.snapshot();
        let pks = lookup(&snap, &self.by_as_token, &(as_token.to_string(),))?;
        let Some(pk) = pks.into_iter().next() else {
            return Ok(None);
        };
        match self.registry.get(&snap, &pk)? {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Every registered row, in id order.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] or [`AppserviceError::Decode`] on failure.
    pub fn list(&self) -> Result<Vec<AppserviceRow>, AppserviceError> {
        let snap = self.backend.snapshot();
        self.registry
            .range(&snap, RangeSpec::full())
            .map(|item| {
                let (_k, v) = item?;
                decode(&v)
            })
            .collect()
    }

    /// Inserts a brand new row, maintaining the token indexes in the same transaction.
    ///
    /// # Errors
    /// Returns [`AppserviceError::AlreadyExists`] if `row.id` is taken, or
    /// [`AppserviceError::TokenConflict`] if `as_token`/`hs_token` collide with a different row
    /// (checked explicitly before the index write, so the error names which token and which
    /// existing id, which `hs_tables`'s bare `UniqueConflict` cannot).
    pub fn insert(&self, row: &AppserviceRow) -> Result<(), AppserviceError> {
        self.check_token_conflicts(row, None)?;
        let key = (row.id.clone(),);
        let value = encode(row)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            if self.registry.get(txn, &key).map_err(to_kv)?.is_some() {
                return Err(hs_kv::KvError::backend(RowExists(row.id.clone())));
            }
            self.registry.put(txn, &key, &value).map_err(to_kv)?;
            maintain_index(txn, &self.by_as_token, &key, None, Some(&value)).map_err(to_kv)?;
            maintain_index(txn, &self.by_hs_token, &key, None, Some(&value)).map_err(to_kv)?;
            Ok(())
        })
        .map_err(|e| match e {
            hs_kv::KvError::Backend(inner) if inner.downcast_ref::<RowExists>().is_some() => {
                AppserviceError::AlreadyExists(row.id.clone())
            }
            other => AppserviceError::Store(other.to_string()),
        })
    }

    /// Overwrites an existing row (update, pause, resume, token rotation all funnel through
    /// this), maintaining the token indexes.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if no row with `row.id` exists, or
    /// [`AppserviceError::TokenConflict`] if the new tokens collide with a *different* row.
    pub fn replace(&self, row: &AppserviceRow) -> Result<(), AppserviceError> {
        self.check_token_conflicts(row, Some(&row.id))?;
        let key = (row.id.clone(),);
        let value = encode(row)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(old) = self.registry.get(txn, &key).map_err(to_kv)? else {
                return Err(hs_kv::KvError::backend(RowMissing(row.id.clone())));
            };
            self.registry.put(txn, &key, &value).map_err(to_kv)?;
            maintain_index(txn, &self.by_as_token, &key, Some(&old), Some(&value))
                .map_err(to_kv)?;
            maintain_index(txn, &self.by_hs_token, &key, Some(&old), Some(&value))
                .map_err(to_kv)?;
            Ok(())
        })
        .map_err(|e| match e {
            hs_kv::KvError::Backend(inner) if inner.downcast_ref::<RowMissing>().is_some() => {
                AppserviceError::NotFound(row.id.clone())
            }
            other => AppserviceError::Store(other.to_string()),
        })
    }

    /// Removes a row and its index entries. Does not touch the transaction queue or health row —
    /// callers that want those gone too call [`AppserviceStore::purge_queue`] and
    /// [`AppserviceStore::delete_health`] explicitly, since keeping a removed appservice's
    /// backlog around briefly (for audit) is a reasonable choice a caller may want to make.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if no such row exists.
    pub fn remove(&self, id: &str) -> Result<(), AppserviceError> {
        let key = (id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(old) = self.registry.get(txn, &key).map_err(to_kv)? else {
                return Err(hs_kv::KvError::backend(RowMissing(id.to_string())));
            };
            self.registry.delete(txn, &key).map_err(to_kv)?;
            maintain_index(txn, &self.by_as_token, &key, Some(&old), None).map_err(to_kv)?;
            maintain_index(txn, &self.by_hs_token, &key, Some(&old), None).map_err(to_kv)?;
            Ok(())
        })
        .map_err(|e| match e {
            hs_kv::KvError::Backend(inner) if inner.downcast_ref::<RowMissing>().is_some() => {
                AppserviceError::NotFound(id.to_string())
            }
            other => AppserviceError::Store(other.to_string()),
        })
    }

    fn check_token_conflicts(
        &self,
        row: &AppserviceRow,
        self_id: Option<&str>,
    ) -> Result<(), AppserviceError> {
        match self.token_conflicts(row, self_id)?.into_iter().next() {
            Some((token_kind, existing_id)) => Err(AppserviceError::TokenConflict {
                token_kind,
                existing_id,
            }),
            None => Ok(()),
        }
    }

    /// Every token of `row` that a *different* row (not `self_id`) already holds, as
    /// `("as_token" | "hs_token", that row's id)`: what [`Self::insert`] and [`Self::replace`]
    /// refuse for, listed in full for a dry run.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on backend failure.
    pub(crate) fn token_conflicts(
        &self,
        row: &AppserviceRow,
        self_id: Option<&str>,
    ) -> Result<Vec<(&'static str, String)>, AppserviceError> {
        let snap = self.backend.snapshot();
        let mut conflicts = Vec::new();
        for (existing_pk, token_kind) in [
            (
                lookup(&snap, &self.by_as_token, &(row.as_token.clone(),))?,
                "as_token",
            ),
            (
                lookup(&snap, &self.by_hs_token, &(row.hs_token.clone(),))?,
                "hs_token",
            ),
        ] {
            if let Some((existing_id,)) = existing_pk.into_iter().next()
                && Some(existing_id.as_str()) != self_id
            {
                conflicts.push((token_kind, existing_id));
            }
        }
        Ok(conflicts)
    }

    // ---- health ----

    /// Reads an appservice's health row, defaulting to an all-`None`/zero row if none has been
    /// recorded yet (never pinged, never delivered).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] or [`AppserviceError::Decode`] on failure.
    pub fn health(&self, id: &str) -> Result<HealthRow, AppserviceError> {
        let snap = self.backend.snapshot();
        match self.health.get(&snap, &(id.to_string(),))? {
            Some(bytes) => decode(&bytes),
            None => Ok(HealthRow::default()),
        }
    }

    /// Overwrites an appservice's health row.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn put_health(&self, id: &str, health: &HealthRow) -> Result<(), AppserviceError> {
        let value = encode(health)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.health
                .put(txn, &(id.to_string(),), &value)
                .map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Removes an appservice's health row (called on [`AppserviceStore::remove`] by callers that
    /// want a clean slate).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn delete_health(&self, id: &str) -> Result<(), AppserviceError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.health.delete(txn, &(id.to_string(),)).map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    // ---- transaction queue ----

    /// Allocates the next queue sequence number for `id` (monotonic per appservice, starting at 1)
    /// and enqueues `body` as a new [`QueuedTransaction`] in `Pending` status, ready to send
    /// immediately (`next_attempt_at_ms` = `now_ms`). Both steps happen in one transaction, so a
    /// crash between them cannot allocate a sequence number that is never enqueued or vice versa.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn enqueue(&self, id: &str, body: Value, now_ms: u64) -> Result<u64, AppserviceError> {
        transact(&self.backend, TransactConfig::default(), move |txn| {
            self.enqueue_in(txn, id, body.clone(), now_ms)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// [`AppserviceStore::enqueue`]'s two steps, inside a transaction the caller owns.
    fn enqueue_in<W: KvWrite<Keyspace = B::Keyspace>>(
        &self,
        txn: &mut W,
        id: &str,
        body: Value,
        now_ms: u64,
    ) -> Result<u64, hs_kv::KvError> {
        let seq_key = (id.to_string(),);
        let next = txn
            .atomic_add(self.txn_seq.raw(), &hs_tables::key::encode(&seq_key), 1)
            .map_err(to_kv)?;
        #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
        let seq = next as u64;
        let entry = QueuedTransaction {
            seq,
            body,
            enqueued_at_ms: now_ms,
            attempts: 0,
            next_attempt_at_ms: now_ms,
            status: QueueStatus::Pending,
            last_error: None,
            txn_id: None,
            first_failed_at_ms: None,
        };
        let value = serde_json::to_vec(&entry)
            .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
        let head = serde_json::to_vec(&QueueHead::of(&entry))
            .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
        let key = (id.to_string(), seq);
        self.txn_queue.put(txn, &key, &value).map_err(to_kv)?;
        self.txn_queue_head.put(txn, &key, &head).map_err(to_kv)?;
        Ok(seq)
    }

    // ---- the pump's place in each room ----

    /// How far into `room_id`'s timeline the pump has read: the room-local position of the last
    /// event it has dealt with. `None` if it has never read this room.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn room_cursor(&self, room_id: &str) -> Result<Option<i64>, AppserviceError> {
        let snap = self.backend.snapshot();
        match self.room_cursor.get(&snap, &(room_id.to_string(),))? {
            Some(bytes) => decode(&bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Queues one transaction body for each of `deliveries` (`(appservice id, body)`) and moves
    /// `room_id`'s cursor to `cursor`, in one transaction.
    ///
    /// One transaction is the point. The cursor says "everything up to here has been queued for
    /// whoever wanted it"; written separately, a crash between the two either queues an event
    /// twice (a bridge relays the same message twice) or never queues it (a bridge silently
    /// misses one), and there is no telling which from the outside.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn enqueue_for_room(
        &self,
        room_id: &str,
        cursor: i64,
        deliveries: &[(String, Value)],
        now_ms: u64,
    ) -> Result<(), AppserviceError> {
        let cursor_value = encode(&cursor)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (id, body) in deliveries {
                self.enqueue_in(txn, id, body.clone(), now_ms)?;
            }
            self.room_cursor
                .put(txn, &(room_id.to_string(),), &cursor_value)
                .map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    // ---- the ephemeral pump's place in each stream ----

    /// How far into the server's `stream` (`receipts`, `presence`, `to_device`,
    /// `device_lists`) appservice `id` has been sent: the position of the last entry dealt
    /// with. `None` if it has never been sent any of it.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn ephemeral_pos(&self, id: &str, stream: &str) -> Result<Option<u64>, AppserviceError> {
        let snap = self.backend.snapshot();
        match self
            .ephemeral_pos
            .get(&snap, &(id.to_string(), stream.to_string()))?
        {
            Some(bytes) => decode(&bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Queues each delivery's body (where there is one) and records each delivery's stream
    /// positions, in one transaction: as with [`AppserviceStore::enqueue_for_room`], the
    /// position says "everything up to here has been queued for this appservice", and written
    /// separately a crash between the two would send a receipt twice or never.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn enqueue_ephemeral(
        &self,
        deliveries: &[EphemeralDelivery],
        now_ms: u64,
    ) -> Result<(), AppserviceError> {
        type Encoded = (String, Option<Value>, Vec<(String, Vec<u8>)>);
        let encoded: Vec<Encoded> = deliveries
            .iter()
            .map(|d| {
                let positions = d
                    .positions
                    .iter()
                    .map(|(stream, pos)| Ok((stream.clone(), encode(pos)?)))
                    .collect::<Result<Vec<_>, AppserviceError>>()?;
                Ok((d.appservice_id.clone(), d.body.clone(), positions))
            })
            .collect::<Result<_, AppserviceError>>()?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (id, body, positions) in &encoded {
                if let Some(body) = body {
                    self.enqueue_in(txn, id, body.clone(), now_ms)?;
                }
                for (stream, pos) in positions {
                    self.ephemeral_pos
                        .put(txn, &(id.clone(), stream.clone()), pos)
                        .map_err(to_kv)?;
                }
            }
            Ok(())
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Whether the pump has ever started against this store. See
    /// [`AppserviceStore::start_pump_at`].
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn pump_has_started(&self) -> Result<bool, AppserviceError> {
        let snap = self.backend.snapshot();
        Ok(self
            .pump_meta
            .get(&snap, &(PUMP_STARTED.to_string(),))?
            .is_some())
    }

    /// The pump's first start against this store: records where every room that already exists
    /// stands (`heads`, `(room id, position)`), and that this has been done.
    ///
    /// A server that has been running has history, and an appservice registered today did not
    /// ask for all of it. So the first start reads nothing, and remembers the present; from then
    /// on a room with no cursor is a room created since, whose every event is news.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn start_pump_at(&self, heads: &[(String, i64)]) -> Result<(), AppserviceError> {
        let encoded: Vec<(String, Vec<u8>)> = heads
            .iter()
            .map(|(room_id, head)| Ok((room_id.clone(), encode(head)?)))
            .collect::<Result<_, AppserviceError>>()?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (room_id, head) in &encoded {
                self.room_cursor
                    .put(txn, &(room_id.clone(),), head)
                    .map_err(to_kv)?;
            }
            self.pump_meta
                .put(txn, &(PUMP_STARTED.to_string(),), b"1")
                .map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Every queue entry for `id`, in sequence (delivery) order.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn queue_for(&self, id: &str) -> Result<Vec<QueuedTransaction>, AppserviceError> {
        let snap = self.backend.snapshot();
        let prefix = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(id.to_string(),));
        self.txn_queue
            .range(&snap, prefix)
            .map(|item| {
                let (_k, v) = item?;
                decode(&v)
            })
            .collect()
    }

    /// How many of `id`'s queue entries are waiting to be sent and how many are dead-lettered,
    /// and when the oldest waiting one was queued: the queue gauges
    /// ([`crate::metrics::QueueCollector`]) and the admin API's `AppService.queue`. Reads each
    /// entry's status without building its body.
    ///
    /// Reads from the queue's floor (`queue_floor`), as [`AppserviceStore::pending_batch`]
    /// does: nothing below it waits, and its dead-lettered entries are counted once and
    /// remembered ([`QueueFront`]), each later summary counting only those the floor has
    /// passed since. Before 2026-10-14 every summary walked the whole queue from its first
    /// entry, the delivered ones' tombstones included.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn queue_summary(&self, id: &str) -> Result<QueueSummary, AppserviceError> {
        let snap = self.backend.snapshot();
        let floor: Option<u64> = self
            .queue_floor
            .get(&snap, &(id.to_string(),))?
            .map(|v| decode(&v))
            .transpose()?;
        let mut summary = QueueSummary::default();
        let start = match floor {
            Some(floor) => {
                summary.dead_lettered = self.dead_lettered_below(&snap, id, floor)?;
                floor.checked_sub(1)
            }
            None => {
                self.fronts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(id);
                None
            }
        };
        let mut spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(id.to_string(),));
        if let Some(seq) = start {
            spec.start = Bound::Excluded((id.to_string(), seq).encode().into());
        }
        for item in self.txn_queue_head.range(&snap, spec) {
            let (_k, v) = item?;
            let head: QueueHead = decode(&v)?;
            match head.status {
                QueueStatus::Pending => {
                    summary.pending += 1;
                    summary.oldest_pending_at_ms = Some(
                        summary
                            .oldest_pending_at_ms
                            .map_or(head.enqueued_at_ms, |at| at.min(head.enqueued_at_ms)),
                    );
                }
                QueueStatus::DeadLettered => summary.dead_lettered += 1,
                QueueStatus::Delivered => {}
            }
        }
        Ok(summary)
    }

    /// How many of `id`'s entries below `floor` are dead-lettered: what is remembered of the
    /// queue's front ([`QueueFront`]), topped up with the entries the floor has passed since,
    /// or counted from the queue's first entry when nothing usable is remembered.
    fn dead_lettered_below<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        read: &R,
        id: &str,
        floor: u64,
    ) -> Result<u64, AppserviceError> {
        let generation = self.front_generation.load(Ordering::Acquire);
        let remembered = self
            .fronts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .copied()
            .filter(|front| {
                front.generation == generation
                    && front.floor <= floor
                    && front.counted.elapsed() < FRONT_RECOUNT
            });
        if let Some(front) = remembered
            && front.floor == floor
        {
            return Ok(front.dead_lettered);
        }
        let (mut cursor, mut dead_lettered, counted) = match remembered {
            Some(front) => (
                front.floor.checked_sub(1),
                front.dead_lettered,
                front.counted,
            ),
            None => (None, 0, Instant::now()),
        };
        'front: loop {
            let chunk = self.heads_after(read, id, cursor, QUEUE_HEADS_PER_READ)?;
            let Some((last, _)) = chunk.last() else {
                break;
            };
            cursor = Some(*last);
            let full = chunk.len() == QUEUE_HEADS_PER_READ;
            for (seq, head) in chunk {
                if seq >= floor {
                    break 'front;
                }
                if head.status == QueueStatus::DeadLettered {
                    dead_lettered += 1;
                }
            }
            if !full {
                break;
            }
        }
        self.fronts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.to_owned(),
                QueueFront {
                    floor,
                    dead_lettered,
                    counted,
                    generation,
                },
            );
        Ok(dead_lettered)
    }

    /// The first `max` entries of `id`'s queue still waiting to be sent, in sequence order, with
    /// their bodies: what [`crate::scheduler::Scheduler::drain`] sends next. Reads the heads
    /// from the queue's floor (`queue_floor`) up to the `max`th waiting entry,
    /// [`QUEUE_HEADS_PER_READ`] at a time, and the bodies of those alone, then raises the floor
    /// to the first waiting entry. So the dead-lettered entries at the front of a long queue,
    /// and the delivered ones deleted behind it (which an LSM store keeps as tombstones for a
    /// while), are walked once, not on every drain: on 2026-10-13 the demo's iMessage queue had
    /// 3,441 dead-lettered entries in front of ~31,000 waiting ones.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn pending_batch(
        &self,
        id: &str,
        max: usize,
    ) -> Result<Vec<QueuedTransaction>, AppserviceError> {
        let floor_key = (id.to_string(),);
        let (batch, floor, new_floor) = {
            let snap = self.backend.snapshot();
            let floor: Option<u64> = self
                .queue_floor
                .get(&snap, &floor_key)?
                .map(|v| decode(&v))
                .transpose()?;
            let after = floor.and_then(|f| f.checked_sub(1));
            let mut first_pending = None;
            let mut last_seen = None;
            let mut found = Vec::new();
            let mut cursor = after;
            'scan: loop {
                let chunk = self.heads_after(&snap, id, cursor, QUEUE_HEADS_PER_READ)?;
                let Some((last, _)) = chunk.last() else {
                    break;
                };
                cursor = Some(*last);
                let full = chunk.len() == QUEUE_HEADS_PER_READ;
                for (seq, head) in chunk {
                    if found.len() >= max.max(1) {
                        break 'scan;
                    }
                    last_seen = Some(seq);
                    if head.status == QueueStatus::Pending {
                        first_pending.get_or_insert(seq);
                        found.push(seq);
                    }
                }
                if !full {
                    break;
                }
            }
            if max == 0 {
                found.clear();
            }
            let batch = self.bodies(&snap, id, &found)?;
            // Nothing below the first waiting entry waits; with none, nothing up to the last
            // entry seen does (a later entry has a larger sequence number).
            let new_floor = first_pending.or_else(|| last_seen.map(|s| s.saturating_add(1)));
            (batch, floor, new_floor)
        };
        if let Some(new_floor) = new_floor
            && floor.is_none_or(|f| new_floor > f)
        {
            self.raise_queue_floor(id, floor, new_floor)?;
        }
        Ok(batch)
    }

    /// Sets `id`'s queue floor to `to` if it is still `from`: an entry made to wait again since
    /// `from` was read lowered it, and that lower floor stands.
    fn raise_queue_floor(
        &self,
        id: &str,
        from: Option<u64>,
        to: u64,
    ) -> Result<(), AppserviceError> {
        let key = (id.to_string(),);
        let value = encode(&to)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let current: Option<u64> = match self.queue_floor.get(txn, &key).map_err(to_kv)? {
                Some(v) => Some(
                    serde_json::from_slice(&v)
                        .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?,
                ),
                None => None,
            };
            if current == from {
                self.queue_floor.put(txn, &key, &value).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Within `txn`, lowers `id`'s queue floor to `seq` if it is above it: entry `seq` waits
    /// again.
    fn lower_queue_floor_in<T: KvWrite<Keyspace = B::Keyspace>>(
        &self,
        txn: &mut T,
        id: &str,
        seq: u64,
    ) -> Result<(), hs_kv::KvError> {
        let key = (id.to_string(),);
        let Some(current) = self.queue_floor.get(txn, &key).map_err(to_kv)? else {
            return Ok(());
        };
        let current: u64 = serde_json::from_slice(&current)
            .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
        if seq < current {
            let value = serde_json::to_vec(&seq)
                .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
            self.queue_floor.put(txn, &key, &value).map_err(to_kv)?;
        }
        Ok(())
    }

    /// One page of `id`'s backlog -- its waiting and dead-lettered entries, not the delivered
    /// ones -- in sequence order, starting after sequence number `after` (from the front with
    /// `None`), at most `limit` (at least one) long. Walks the heads and reads the bodies of the
    /// page's entries alone: the admin API's backlog list, which the interface polls every
    /// fifteen seconds while a bridge's page is open, read every body of the queue each time.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn backlog_page(
        &self,
        id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<QueuePage, AppserviceError> {
        let limit = limit.max(1);
        let snap = self.backend.snapshot();
        // One more than the page, to know whether there is a next one.
        let heads = self.find_heads(&snap, id, after, limit.saturating_add(1), |head| {
            head.status != QueueStatus::Delivered
        })?;
        let more = heads.len() > limit;
        let seqs: Vec<u64> = heads.iter().take(limit).map(|(seq, _)| *seq).collect();
        let entries = self.bodies(&snap, id, &seqs)?;
        Ok(QueuePage {
            next_after: if more { seqs.last().copied() } else { None },
            entries,
        })
    }

    /// Puts `id`'s dead-lettered entries that `matches(seq, enqueued_at_ms)` back in the queue
    /// for immediate retry: status pending, next attempt at `now_ms`, error cleared, and a
    /// fresh retry budget (attempts, first failure and transaction id reset). Before 2026-10-15
    /// the attempt count was kept, so a replayed entry had one attempt left: on the demo a
    /// replay of Discord's 260 dead letters sent each batch once, the bridge took longer than
    /// the request timeout, and every batch was dead-lettered again within 31 seconds while
    /// the bridge went on processing them. Walks the heads [`QUEUE_HEADS_PER_READ`] at a time and rewrites the matching
    /// entries [`REQUEUES_PER_TXN`] per transaction, so a replay of tens of thousands neither
    /// holds them in memory nor writes them in one transaction. Returns how many it put back;
    /// an entry delivered or put back by someone else meanwhile is left alone.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure; entries
    /// already put back stay put back.
    pub fn requeue_dead_lettered(
        &self,
        id: &str,
        matches: impl Fn(u64, u64) -> bool,
        now_ms: u64,
    ) -> Result<usize, AppserviceError> {
        // Entries leave the dead letters below the floor: what the summaries remember of the
        // queues' fronts is counted again.
        self.front_generation.fetch_add(1, Ordering::AcqRel);
        let mut requeued = 0usize;
        let mut after = None;
        loop {
            let chunk = {
                let snap = self.backend.snapshot();
                self.heads_after(&snap, id, after, QUEUE_HEADS_PER_READ)?
            };
            let Some((last, _)) = chunk.last() else {
                break;
            };
            after = Some(*last);
            let keys: Vec<(String, u64)> = chunk
                .iter()
                .filter(|(seq, head)| {
                    head.status == QueueStatus::DeadLettered && matches(*seq, head.enqueued_at_ms)
                })
                .map(|(seq, _)| (id.to_string(), *seq))
                .collect();
            for keys in keys.chunks(REQUEUES_PER_TXN) {
                requeued += self.requeue_in_one_transaction(id, keys, now_ms)?;
            }
            if chunk.len() < QUEUE_HEADS_PER_READ {
                break;
            }
        }
        Ok(requeued)
    }

    /// Deletes `id`'s dead-lettered entries that `matches(seq, enqueued_at_ms)`: an operator
    /// has looked at what the bridge was never sent and decided it should not be. Entries still
    /// waiting are never touched. Walks the heads [`QUEUE_HEADS_PER_READ`] at a time and
    /// deletes one chunk per transaction, re-checking each entry's status inside it, so an
    /// entry replayed meanwhile is left alone. Returns how many it deleted.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure; entries
    /// already deleted stay deleted.
    pub fn discard_dead_lettered(
        &self,
        id: &str,
        matches: impl Fn(u64, u64) -> bool,
    ) -> Result<usize, AppserviceError> {
        // Entries leave the dead letters below the floor: what the summaries remember of the
        // queues' fronts is counted again.
        self.front_generation.fetch_add(1, Ordering::AcqRel);
        let mut discarded = 0usize;
        let mut after = None;
        loop {
            let chunk = {
                let snap = self.backend.snapshot();
                self.heads_after(&snap, id, after, QUEUE_HEADS_PER_READ)?
            };
            let Some((last, _)) = chunk.last() else {
                break;
            };
            after = Some(*last);
            let keys: Vec<(String, u64)> = chunk
                .iter()
                .filter(|(seq, head)| {
                    head.status == QueueStatus::DeadLettered && matches(*seq, head.enqueued_at_ms)
                })
                .map(|(seq, _)| (id.to_string(), *seq))
                .collect();
            if !keys.is_empty() {
                discarded += transact(&self.backend, TransactConfig::default(), |txn| {
                    let mut deleted = 0usize;
                    for key in &keys {
                        let Some(head) = self.txn_queue_head.get(txn, key).map_err(to_kv)? else {
                            continue;
                        };
                        let head: QueueHead = serde_json::from_slice(&head)
                            .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
                        if head.status != QueueStatus::DeadLettered {
                            continue;
                        }
                        self.txn_queue.delete(txn, key).map_err(to_kv)?;
                        self.txn_queue_head.delete(txn, key).map_err(to_kv)?;
                        deleted += 1;
                    }
                    Ok(deleted)
                })
                .map_err(|e| AppserviceError::Store(e.to_string()))?;
            }
            if chunk.len() < QUEUE_HEADS_PER_READ {
                break;
            }
        }
        // Counted again after the deletes too: a summary taken between the generation's first
        // bump and the last delete may have remembered entries that are gone now.
        self.front_generation.fetch_add(1, Ordering::AcqRel);
        Ok(discarded)
    }

    /// [`AppserviceStore::requeue_dead_lettered`]'s writes for `keys`, in one transaction.
    fn requeue_in_one_transaction(
        &self,
        id: &str,
        keys: &[(String, u64)],
        now_ms: u64,
    ) -> Result<usize, AppserviceError> {
        let requeued = transact(&self.backend, TransactConfig::default(), |txn| {
            let values = self.txn_queue.multi_get(txn, keys).map_err(to_kv)?;
            let mut requeued = 0usize;
            let mut lowest = None;
            for (key, value) in keys.iter().zip(values) {
                let Some(value) = value else { continue };
                let mut entry: QueuedTransaction = serde_json::from_slice(&value)
                    .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
                if entry.status != QueueStatus::DeadLettered {
                    continue;
                }
                entry.status = QueueStatus::Pending;
                entry.next_attempt_at_ms = now_ms;
                entry.last_error = None;
                entry.attempts = 0;
                entry.first_failed_at_ms = None;
                entry.txn_id = None;
                let body = serde_json::to_vec(&entry)
                    .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
                let head = serde_json::to_vec(&QueueHead::of(&entry))
                    .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
                self.txn_queue.put(txn, key, &body).map_err(to_kv)?;
                self.txn_queue_head.put(txn, key, &head).map_err(to_kv)?;
                // `keys` are in sequence order: the first put back is the lowest.
                lowest.get_or_insert(entry.seq);
                requeued += 1;
            }
            if let Some(lowest) = lowest {
                self.lower_queue_floor_in(txn, id, lowest)?;
            }
            Ok(requeued)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))?;
        self.count_reads(keys.len(), 0);
        Ok(requeued)
    }

    /// How much of the queues this store's paged reads ([`AppserviceStore::pending_batch`],
    /// [`AppserviceStore::backlog_page`], [`AppserviceStore::requeue_dead_lettered`]) have read
    /// since it was opened: a diagnostic, and how the tests show that a drain or a page of the
    /// backlog reads its own entries and not the whole queue. [`AppserviceStore::queue_for`] and
    /// [`AppserviceStore::queue_summary`] are not counted.
    #[must_use]
    pub fn queue_reads(&self) -> QueueReads {
        QueueReads {
            bodies: self.bodies_read.load(Ordering::Relaxed),
            heads: self.heads_read.load(Ordering::Relaxed),
        }
    }

    fn count_reads(&self, bodies: usize, heads: usize) {
        let n = |x: usize| u64::try_from(x).unwrap_or(u64::MAX);
        self.bodies_read.fetch_add(n(bodies), Ordering::Relaxed);
        self.heads_read.fetch_add(n(heads), Ordering::Relaxed);
    }

    /// Up to `n` of `id`'s heads after sequence number `after` (from the front with `None`), in
    /// sequence order: one bounded range read.
    fn heads_after<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        read: &R,
        id: &str,
        after: Option<u64>,
        n: usize,
    ) -> Result<Vec<(u64, QueueHead)>, AppserviceError> {
        let mut spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(id.to_string(),));
        if let Some(seq) = after {
            spec.start = Bound::Excluded((id.to_string(), seq).encode().into());
        }
        spec.limit = Some(n);
        let heads = self
            .txn_queue_head
            .range(read, spec)
            .map(|item| {
                let ((_, seq), value) = item?;
                Ok((seq, decode::<QueueHead>(&value)?))
            })
            .collect::<Result<Vec<_>, AppserviceError>>()?;
        self.count_reads(0, heads.len());
        Ok(heads)
    }

    /// The first `want` of `id`'s heads after `after` that `keep`, in sequence order, read
    /// [`QUEUE_HEADS_PER_READ`] at a time until there are `want` or the queue ends.
    fn find_heads<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        read: &R,
        id: &str,
        mut after: Option<u64>,
        want: usize,
        keep: impl Fn(&QueueHead) -> bool,
    ) -> Result<Vec<(u64, QueueHead)>, AppserviceError> {
        let mut found = Vec::new();
        while found.len() < want {
            let chunk = self.heads_after(read, id, after, QUEUE_HEADS_PER_READ)?;
            let Some((last, _)) = chunk.last() else {
                break;
            };
            after = Some(*last);
            let full = chunk.len() == QUEUE_HEADS_PER_READ;
            found.extend(
                chunk
                    .into_iter()
                    .filter(|(_, head)| keep(head))
                    .take(want - found.len()),
            );
            if !full {
                break;
            }
        }
        Ok(found)
    }

    /// The bodies of `id`'s entries `seqs`, in one read, in the order given; an entry gone since
    /// its head was read is skipped.
    fn bodies<R: KvRead<Keyspace = B::Keyspace>>(
        &self,
        read: &R,
        id: &str,
        seqs: &[u64],
    ) -> Result<Vec<QueuedTransaction>, AppserviceError> {
        if seqs.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<(String, u64)> = seqs.iter().map(|seq| (id.to_string(), *seq)).collect();
        let values = self.txn_queue.multi_get(read, &keys)?;
        self.count_reads(keys.len(), 0);
        values
            .into_iter()
            .flatten()
            .map(|bytes| decode::<QueuedTransaction>(&bytes))
            .collect()
    }

    /// Overwrites one queue entry (used by the scheduler to record an attempt's outcome).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn put_queue_entry(
        &self,
        id: &str,
        entry: &QueuedTransaction,
    ) -> Result<(), AppserviceError> {
        self.put_queue_entries(id, std::slice::from_ref(entry))
    }

    /// Overwrites several queue entries in one transaction: a failed batch's attempts. One
    /// made to wait lowers the queue's floor if it is below it.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn put_queue_entries(
        &self,
        id: &str,
        entries: &[QueuedTransaction],
    ) -> Result<(), AppserviceError> {
        let rows = entries
            .iter()
            .map(|entry| {
                Ok((
                    (id.to_string(), entry.seq),
                    encode(entry)?,
                    encode(&QueueHead::of(entry))?,
                ))
            })
            .collect::<Result<Vec<_>, AppserviceError>>()?;
        let lowest_pending = entries
            .iter()
            .filter(|e| e.status == QueueStatus::Pending)
            .map(|e| e.seq)
            .min();
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (key, value, head) in &rows {
                self.txn_queue.put(txn, key, value).map_err(to_kv)?;
                self.txn_queue_head.put(txn, key, head).map_err(to_kv)?;
            }
            if let Some(seq) = lowest_pending {
                self.lower_queue_floor_in(txn, id, seq)?;
            }
            Ok(())
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Deletes a delivered queue entry once it has aged out of the backlog view.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn delete_queue_entry(&self, id: &str, seq: u64) -> Result<(), AppserviceError> {
        self.delete_queue_entries(id, &[seq])
    }

    /// Deletes several queue entries in one transaction: a delivered batch.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn delete_queue_entries(&self, id: &str, seqs: &[u64]) -> Result<(), AppserviceError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            for seq in seqs {
                let key = (id.to_string(), *seq);
                self.txn_queue.delete(txn, &key).map_err(to_kv)?;
                self.txn_queue_head.delete(txn, &key).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Deletes every queue entry for `id` (used when an appservice is removed and the caller
    /// wants no residue). By heads, [`QUEUE_HEADS_PER_READ`] per transaction: never every body,
    /// and not one transaction per entry (a bridge removed while it was off can have tens of
    /// thousands queued).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn purge_queue(&self, id: &str) -> Result<(), AppserviceError> {
        // Entries leave from below the floor too: what the summaries remember is counted again.
        self.front_generation.fetch_add(1, Ordering::AcqRel);
        loop {
            let chunk = {
                let snap = self.backend.snapshot();
                self.heads_after(&snap, id, None, QUEUE_HEADS_PER_READ)?
            };
            if chunk.is_empty() {
                break;
            }
            let seqs: Vec<u64> = chunk.iter().map(|(seq, _)| *seq).collect();
            self.delete_queue_entries(id, &seqs)?;
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.queue_floor
                .delete(txn, &(id.to_string(),))
                .map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    // ---- appservice room directories ----

    /// Publishes (`published`) or unpublishes `room_id` in appservice `id`'s room directory for
    /// `network_id`. Idempotent both ways.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn set_network_room(
        &self,
        id: &str,
        network_id: &str,
        room_id: &str,
        published: bool,
    ) -> Result<(), AppserviceError> {
        let key = (id.to_string(), network_id.to_string(), room_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            if published {
                self.network_rooms.put(txn, &key, b"").map_err(to_kv)
            } else {
                self.network_rooms.delete(txn, &key).map_err(to_kv)
            }
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// `(appservice id, network id, room id)` for every room published in an appservice's room
    /// directory: only `(id, network_id)`'s with `Some`, every network's with `None`.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn network_rooms(
        &self,
        network: Option<(&str, &str)>,
    ) -> Result<Vec<(String, String, String)>, AppserviceError> {
        let snap = self.backend.snapshot();
        let spec = match network {
            Some((id, network_id)) => {
                TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(
                    id.to_string(),
                    network_id.to_string(),
                ))
            }
            None => RangeSpec::full(),
        };
        self.network_rooms
            .range(&snap, spec)
            .map(|item| Ok(item?.0))
            .collect()
    }

    /// Removes everything appservice `id` published in its room directories (when it is
    /// removed).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn purge_network_rooms(&self, id: &str) -> Result<(), AppserviceError> {
        let snap = self.backend.snapshot();
        let prefix =
            TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(id.to_string(),));
        let keys: Vec<(String, String, String)> = self
            .network_rooms
            .range(&snap, prefix)
            .map(|item| item.map(|(k, _)| k))
            .collect::<Result<_, _>>()?;
        drop(snap);
        transact(&self.backend, TransactConfig::default(), |txn| {
            for key in &keys {
                self.network_rooms.delete(txn, key).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("row {0:?} already exists")]
struct RowExists(String);

#[derive(Debug, thiserror::Error)]
#[error("row {0:?} does not exist")]
struct RowMissing(String);

#[derive(Debug, thiserror::Error)]
#[error("failed to encode value: {0}")]
struct EncodeFail(String);

fn to_kv<E: std::error::Error + Send + Sync + 'static>(e: E) -> hs_kv::KvError {
    hs_kv::KvError::backend(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn store() -> AppserviceStore<MemoryBackend> {
        AppserviceStore::open(MemoryBackend::new()).unwrap()
    }

    fn row(id: &str, as_token: &str, hs_token: &str) -> AppserviceRow {
        AppserviceRow {
            id: id.to_string(),
            url: Some("http://localhost:1234".to_string()),
            as_token: as_token.to_string(),
            hs_token: hs_token.to_string(),
            sender_localpart: format!("{id}bot"),
            rate_limited: true,
            namespaces: NamespacesSpec::default(),
            protocols: vec![],
            receive_ephemeral: false,
            push_ephemeral_legacy: false,
            msc3202: false,
            msc4190: false,
            extra: Map::new(),
            paused: false,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn insert_get_and_list_round_trip() {
        let s = store();
        s.insert(&row("a", "as1", "hs1")).unwrap();
        assert_eq!(s.get("a").unwrap().unwrap().id, "a");
        assert_eq!(s.list().unwrap().len(), 1);
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let s = store();
        s.insert(&row("a", "as1", "hs1")).unwrap();
        let err = s.insert(&row("a", "as2", "hs2")).unwrap_err();
        assert!(matches!(err, AppserviceError::AlreadyExists(id) if id == "a"));
    }

    #[test]
    fn duplicate_as_token_is_rejected() {
        let s = store();
        s.insert(&row("a", "as1", "hs1")).unwrap();
        let err = s.insert(&row("b", "as1", "hs2")).unwrap_err();
        assert!(matches!(
            err,
            AppserviceError::TokenConflict {
                token_kind: "as_token",
                ..
            }
        ));
    }

    #[test]
    fn lookup_by_as_token_finds_the_row() {
        let s = store();
        s.insert(&row("a", "as1", "hs1")).unwrap();
        let found = s.get_by_as_token("as1").unwrap().unwrap();
        assert_eq!(found.id, "a");
        assert!(s.get_by_as_token("nope").unwrap().is_none());
    }

    #[test]
    fn replace_updates_index_when_token_changes() {
        let s = store();
        s.insert(&row("a", "as1", "hs1")).unwrap();
        let mut updated = row("a", "as1-new", "hs1");
        s.replace(&updated).unwrap();
        assert!(s.get_by_as_token("as1").unwrap().is_none());
        assert!(s.get_by_as_token("as1-new").unwrap().is_some());
        // Old token is free to be reused by a different appservice now.
        updated.id = "b".to_string();
        updated.as_token = "as1".to_string();
        updated.hs_token = "hs2".to_string();
        s.insert(&updated).unwrap();
    }

    #[test]
    fn remove_frees_the_id_and_tokens() {
        let s = store();
        s.insert(&row("a", "as1", "hs1")).unwrap();
        s.remove("a").unwrap();
        assert!(s.get("a").unwrap().is_none());
        assert!(s.get_by_as_token("as1").unwrap().is_none());
        s.insert(&row("a", "as1", "hs1")).unwrap();
    }

    #[test]
    fn remove_missing_is_not_found() {
        let s = store();
        assert!(matches!(
            s.remove("nope").unwrap_err(),
            AppserviceError::NotFound(id) if id == "nope"
        ));
    }

    #[test]
    fn enqueue_assigns_increasing_sequence_numbers_per_appservice() {
        let s = store();
        let a1 = s.enqueue("a", serde_json::json!({"n": 1}), 100).unwrap();
        let a2 = s.enqueue("a", serde_json::json!({"n": 2}), 200).unwrap();
        let b1 = s.enqueue("b", serde_json::json!({"n": 1}), 300).unwrap();
        assert_eq!((a1, a2, b1), (1, 2, 1));
        assert_eq!(s.queue_for("a").unwrap().len(), 2);
        assert_eq!(s.queue_for("b").unwrap().len(), 1);
    }

    #[test]
    fn ephemeral_positions_are_written_with_the_bodies_they_account_for() {
        let s = store();
        assert_eq!(s.ephemeral_pos("a", "receipts").unwrap(), None);
        s.enqueue_ephemeral(
            &[
                EphemeralDelivery {
                    appservice_id: "a".to_string(),
                    body: Some(serde_json::json!({"ephemeral": [1]})),
                    positions: vec![("receipts".to_string(), 7), ("presence".to_string(), 3)],
                },
                EphemeralDelivery {
                    appservice_id: "b".to_string(),
                    body: None,
                    positions: vec![("receipts".to_string(), 7)],
                },
            ],
            100,
        )
        .unwrap();
        assert_eq!(s.ephemeral_pos("a", "receipts").unwrap(), Some(7));
        assert_eq!(s.ephemeral_pos("a", "presence").unwrap(), Some(3));
        assert_eq!(s.ephemeral_pos("b", "receipts").unwrap(), Some(7));
        assert_eq!(s.queue_for("a").unwrap().len(), 1);
        assert!(s.queue_for("b").unwrap().is_empty());
    }

    #[test]
    fn health_defaults_when_absent() {
        let s = store();
        let h = s.health("a").unwrap();
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.last_ping_at_ms.is_none());
    }

    /// The scheduler's next batch comes from the heads: the lowest waiting sequence numbers
    /// (past 256, where a little-endian key would sort wrong), dead-lettered ones skipped, at
    /// most `max`, with their bodies; the gauges count the same heads.
    #[test]
    fn pending_batch_reads_the_next_waiting_entries_from_their_heads() {
        let s = store();
        for n in 0..300u64 {
            s.enqueue("a", serde_json::json!({ "n": n }), 1_000 + n)
                .unwrap();
        }
        s.enqueue("b", serde_json::json!({ "n": "other" }), 5)
            .unwrap();
        for seq in 1..=3u64 {
            let mut entry = s
                .queue_for("a")
                .unwrap()
                .remove(usize::try_from(seq - 1).unwrap());
            entry.status = QueueStatus::DeadLettered;
            s.put_queue_entry("a", &entry).unwrap();
        }
        s.delete_queue_entry("a", 4).unwrap();

        let batch = s.pending_batch("a", 20).unwrap();
        let seqs: Vec<u64> = batch.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (5..25).collect::<Vec<u64>>());
        assert_eq!(batch[0].body["n"], 4);

        let summary = s.queue_summary("a").unwrap();
        assert_eq!(summary.pending, 296);
        assert_eq!(summary.dead_lettered, 3);
        assert_eq!(summary.oldest_pending_at_ms, Some(1_004));
        assert_eq!(s.pending_batch("b", 20).unwrap().len(), 1);
    }

    /// A queue written before heads existed (the demo's, 2026-10-13) gets them once, when the
    /// store opens: the scheduler and the gauges see every entry.
    #[test]
    fn opening_a_store_builds_heads_for_a_queue_written_without_them() {
        let backend = MemoryBackend::new();
        let s = AppserviceStore::open(backend.clone()).unwrap();
        for n in 0..3u64 {
            s.enqueue("a", serde_json::json!({ "n": n }), 10 + n)
                .unwrap();
        }
        // As an older server left it: entries, no heads, no marker.
        transact(&backend, TransactConfig::default(), |txn| {
            for seq in 1..=3u64 {
                s.txn_queue_head
                    .delete(txn, &("a".to_string(), seq))
                    .map_err(to_kv)?;
            }
            s.pump_meta
                .delete(txn, &(QUEUE_HEADS_BUILT.to_string(),))
                .map_err(to_kv)
        })
        .unwrap();
        assert_eq!(s.queue_summary("a").unwrap().pending, 0);

        let reopened = AppserviceStore::open(backend).unwrap();
        assert_eq!(reopened.queue_summary("a").unwrap().pending, 3);
        let seqs: Vec<u64> = reopened
            .pending_batch("a", 10)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    /// `n` entries for `id`, the first `dead` of them dead-lettered.
    fn long_queue(s: &AppserviceStore<MemoryBackend>, id: &str, n: u64, dead: u64) {
        for i in 0..n {
            s.enqueue(id, serde_json::json!({ "n": i }), 1_000 + i)
                .unwrap();
        }
        let front: Vec<QueuedTransaction> = s
            .queue_for(id)
            .unwrap()
            .into_iter()
            .take(usize::try_from(dead).unwrap())
            .map(|mut e| {
                e.status = QueueStatus::DeadLettered;
                e.attempts = 10;
                e
            })
            .collect();
        for chunk in front.chunks(100) {
            s.put_queue_entries(id, chunk).unwrap();
        }
    }

    fn seqs_of(batch: &[QueuedTransaction]) -> Vec<u64> {
        batch.iter().map(|e| e.seq).collect()
    }

    /// The demo's iMessage queue (2026-10-13): thousands dead-lettered at the front of a long
    /// queue being drained. The first drain walks past them and remembers where the waiting
    /// entries start; each drain after it reads one bounded piece of the heads and its own
    /// bodies, however many were dead-lettered or delivered before them. An entry made to wait
    /// again below that point (a replay, an overwrite) is found by the next drain, and a store
    /// opened again starts from the front.
    #[test]
    fn a_drain_walks_past_the_dead_lettered_front_of_a_long_queue_once() {
        let backend = MemoryBackend::new();
        let s = AppserviceStore::open(backend.clone()).unwrap();
        long_queue(&s, "a", 3_000, 1_000);

        let batch = s.pending_batch("a", 20).unwrap();
        assert_eq!(seqs_of(&batch), (1_001..=1_020).collect::<Vec<u64>>());
        assert_eq!(batch[0].body["n"], 1_000);
        s.delete_queue_entries("a", &seqs_of(&batch)).unwrap();

        for _ in 0..10 {
            let before = s.queue_reads();
            let batch = s.pending_batch("a", 20).unwrap();
            let read = s.queue_reads();
            assert_eq!(batch.len(), 20);
            assert_eq!(read.bodies - before.bodies, 20, "only the batch's bodies");
            assert!(
                read.heads - before.heads <= QUEUE_HEADS_PER_READ as u64,
                "one piece of the heads, not the dead-lettered front: {}",
                read.heads - before.heads
            );
            s.delete_queue_entries("a", &seqs_of(&batch)).unwrap();
        }
        // 1,001 to 1,220 are delivered.
        assert_eq!(s.pending_batch("a", 1).unwrap()[0].seq, 1_221);

        // A replay of one dead-lettered entry puts it first again.
        let replayed = s
            .requeue_dead_lettered("a", |seq, _| seq == 500, 7)
            .unwrap();
        assert_eq!(replayed, 1);
        assert_eq!(
            seqs_of(&s.pending_batch("a", 3).unwrap()),
            vec![500, 1_221, 1_222]
        );
        s.delete_queue_entries("a", &[500]).unwrap();
        assert_eq!(s.pending_batch("a", 1).unwrap()[0].seq, 1_221);

        // So does an entry overwritten as waiting.
        let mut entry = s
            .queue_for("a")
            .unwrap()
            .into_iter()
            .find(|e| e.seq == 600)
            .unwrap();
        entry.status = QueueStatus::Pending;
        s.put_queue_entry("a", &entry).unwrap();
        assert_eq!(seqs_of(&s.pending_batch("a", 2).unwrap()), vec![600, 1_221]);

        // A store opened again reads from the front once.
        let reopened = AppserviceStore::open(backend).unwrap();
        assert_eq!(
            seqs_of(&reopened.pending_batch("a", 2).unwrap()),
            vec![600, 1_221]
        );

        // An emptied queue, then a new entry: found.
        let all: Vec<u64> = seqs_of(&reopened.queue_for("a").unwrap());
        reopened.delete_queue_entries("a", &all).unwrap();
        assert!(reopened.pending_batch("a", 20).unwrap().is_empty());
        let seq = reopened.enqueue("a", serde_json::json!({}), 1).unwrap();
        assert_eq!(
            seqs_of(&reopened.pending_batch("a", 20).unwrap()),
            vec![seq]
        );
    }

    /// The admin API's backlog is read a page at a time: waiting and dead-lettered entries in
    /// order, delivered and deleted ones left out, each page continuing where the last stopped,
    /// and each reading its own entries' bodies and no others.
    #[test]
    fn the_backlog_is_read_a_page_at_a_time() {
        let s = store();
        long_queue(&s, "a", 3_000, 400);
        s.enqueue("b", serde_json::json!({}), 1).unwrap();
        let delivered: Vec<QueuedTransaction> = s
            .queue_for("a")
            .unwrap()
            .into_iter()
            .filter(|e| (1_100..1_110).contains(&e.seq))
            .map(|mut e| {
                e.status = QueueStatus::Delivered;
                e
            })
            .collect();
        s.put_queue_entries("a", &delivered).unwrap();
        let gone: Vec<u64> = (2_000..2_600).collect();
        s.delete_queue_entries("a", &gone).unwrap();
        let expected: Vec<u64> = (1..=3_000)
            .filter(|seq| !(1_100..1_110).contains(seq) && !gone.contains(seq))
            .collect();

        let mut seen = Vec::new();
        let mut after = None;
        let mut pages = 0;
        loop {
            let before = s.queue_reads();
            let page = s.backlog_page("a", after, 50).unwrap();
            let read = s.queue_reads();
            assert_eq!(read.bodies - before.bodies, page.entries.len() as u64);
            assert!(page.entries.len() <= 50);
            for entry in &page.entries {
                assert_eq!(entry.status == QueueStatus::DeadLettered, entry.seq <= 400);
            }
            seen.extend(seqs_of(&page.entries));
            pages += 1;
            match page.next_after {
                Some(next) => {
                    assert_eq!(Some(&next), seen.last());
                    after = Some(next);
                }
                None => break,
            }
        }
        assert_eq!(seen, expected);
        assert_eq!(pages, expected.len().div_ceil(50));
        let summary = s.queue_summary("a").unwrap();
        assert_eq!(
            summary.pending + summary.dead_lettered,
            expected.len() as u64
        );
        assert!(
            s.backlog_page("a", Some(3_000), 50)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// The queue gauges read from the queue's floor: the dead-lettered entries in front are
    /// counted once and remembered, so the next summary reads none of them, and gives the same
    /// answer; entries the floor passes are counted as it passes them; a replay has the front
    /// counted again. Before 2026-10-14 every scrape walked the whole queue, delivered entries'
    /// tombstones included.
    #[test]
    fn a_queue_summary_reads_from_the_floor_and_remembers_the_front() {
        let s = store();
        long_queue(&s, "a", 3_000, 1_000);
        let full = s.queue_summary("a").unwrap();
        assert_eq!((full.pending, full.dead_lettered), (2_000, 1_000));

        // A drain raises the floor past the dead letters.
        assert_eq!(s.pending_batch("a", 10).unwrap()[0].seq, 1_001);
        let before = s.queue_reads().heads;
        assert_eq!(s.queue_summary("a").unwrap(), full);
        let first = s.queue_reads().heads - before;
        assert!(first >= 1_000, "the front is counted once: {first} heads");
        let before = s.queue_reads().heads;
        assert_eq!(s.queue_summary("a").unwrap(), full);
        assert_eq!(s.queue_reads().heads, before, "and remembered");

        // The floor moves on past entries made dead-lettered and delivered ones deleted.
        let passed: Vec<QueuedTransaction> = s
            .pending_batch("a", 20)
            .unwrap()
            .into_iter()
            .map(|mut e| {
                e.status = QueueStatus::DeadLettered;
                e
            })
            .collect();
        s.put_queue_entries("a", &passed[..10]).unwrap();
        let delivered: Vec<u64> = passed[10..].iter().map(|e| e.seq).collect();
        s.delete_queue_entries("a", &delivered).unwrap();
        assert_eq!(s.pending_batch("a", 1).unwrap()[0].seq, 1_021);
        let moved = s.queue_summary("a").unwrap();
        assert_eq!((moved.pending, moved.dead_lettered), (1_980, 1_010));

        // A replay puts dead letters back: the front is counted again.
        assert_eq!(
            s.requeue_dead_lettered("a", |seq, _| seq <= 5, 0).unwrap(),
            5
        );
        let replayed = s.queue_summary("a").unwrap();
        assert_eq!((replayed.pending, replayed.dead_lettered), (1_985, 1_005));
    }

    /// A replay walks the queue by heads and puts back only the dead-lettered entries asked
    /// for, with a fresh retry budget; removing an appservice takes its whole queue.
    #[test]
    fn requeue_and_purge_walk_the_queue_by_heads() {
        let s = store();
        long_queue(&s, "a", 1_500, 1_200);
        let replayed = s
            .requeue_dead_lettered("a", |seq, _| seq % 2 == 0, 42)
            .unwrap();
        assert_eq!(replayed, 600);
        let summary = s.queue_summary("a").unwrap();
        assert_eq!(summary.dead_lettered, 600);
        assert_eq!(summary.pending, 900);
        let first = &s.pending_batch("a", 1).unwrap()[0];
        assert_eq!(first.seq, 2);
        assert_eq!(first.attempts, 0);
        assert_eq!(first.next_attempt_at_ms, 42);
        assert!(first.last_error.is_none());

        s.enqueue("b", serde_json::json!({}), 1).unwrap();
        s.purge_queue("a").unwrap();
        assert!(s.queue_for("a").unwrap().is_empty());
        assert_eq!(s.queue_summary("a").unwrap(), QueueSummary::default());
        assert_eq!(s.queue_for("b").unwrap().len(), 1);
    }
}
