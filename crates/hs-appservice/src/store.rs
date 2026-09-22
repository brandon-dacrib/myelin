//! The `hs-tables`/`hs-kv` storage layer: the registry row, health row, and per-appservice
//! transaction queue, plus the keyspace handles that own them.
//!
//! Generic over `B: hs_kv::KvBackend`, following `hs-media`'s pattern
//! (`crates/hs-media/src/metadata.rs`): every method here is synchronous (`hs-kv` transactions may
//! not `.await`), called directly from async handlers/scheduler tasks — cheap against
//! `MemoryBackend` in every test in this crate, and fine against a production `FjallBackend` until
//! profiling says otherwise (no different code path is available yet; see that file's docs for the
//! `spawn_blocking` escape hatch this crate would reach for first).

use hs_kv::{KvBackend, KvWrite, RangeSpec, TransactConfig, transact};
use hs_tables::index::{IndexDef, lookup, maintain_index};
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
}

/// The [`AppserviceStore`] `pump_meta` row whose presence means the pump has started before.
const PUMP_STARTED: &str = "started";

/// The keyspace handles and typed accessors for one appservice registry, wired to a `B: KvBackend`.
pub struct AppserviceStore<B: KvBackend> {
    backend: B,
    registry: TypedKeyspace<B::Keyspace, (String,)>,
    by_as_token: IndexDef<B::Keyspace, (String,), (String,)>,
    by_hs_token: IndexDef<B::Keyspace, (String,), (String,)>,
    health: TypedKeyspace<B::Keyspace, (String,)>,
    txn_queue: TypedKeyspace<B::Keyspace, (String, u64)>,
    txn_seq: TypedKeyspace<B::Keyspace, (String,)>,
    /// `room_id -> position`: how far into each room's timeline [`crate::pump`] has read. See
    /// [`AppserviceStore::enqueue_for_room`].
    room_cursor: TypedKeyspace<B::Keyspace, (String,)>,
    /// Facts about the pump as a whole, by name. One today: [`PUMP_STARTED`].
    pump_meta: TypedKeyspace<B::Keyspace, (String,)>,
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
        let txn_seq = TypedKeyspace::new(backend.keyspace("hs_appservice.txn_seq")?);
        let room_cursor = TypedKeyspace::new(backend.keyspace("hs_appservice.room_cursor")?);
        let pump_meta = TypedKeyspace::new(backend.keyspace("hs_appservice.pump_meta")?);
        Ok(Self {
            backend,
            registry,
            by_as_token,
            by_hs_token,
            health,
            txn_queue,
            txn_seq,
            room_cursor,
            pump_meta,
        })
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
        let snap = self.backend.snapshot();
        for (existing_pk, token_kind, token) in [
            (
                lookup(&snap, &self.by_as_token, &(row.as_token.clone(),))?,
                "as_token",
                &row.as_token,
            ),
            (
                lookup(&snap, &self.by_hs_token, &(row.hs_token.clone(),))?,
                "hs_token",
                &row.hs_token,
            ),
        ] {
            let _ = token;
            if let Some((existing_id,)) = existing_pk.into_iter().next()
                && Some(existing_id.as_str()) != self_id
            {
                return Err(AppserviceError::TokenConflict {
                    token_kind,
                    existing_id,
                });
            }
        }
        Ok(())
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
        };
        let value = serde_json::to_vec(&entry)
            .map_err(|e| hs_kv::KvError::backend(EncodeFail(e.to_string())))?;
        self.txn_queue
            .put(txn, &(id.to_string(), seq), &value)
            .map_err(to_kv)?;
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

    /// Overwrites one queue entry (used by the scheduler to record an attempt's outcome).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn put_queue_entry(
        &self,
        id: &str,
        entry: &QueuedTransaction,
    ) -> Result<(), AppserviceError> {
        let value = encode(entry)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.txn_queue
                .put(txn, &(id.to_string(), entry.seq), &value)
                .map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Deletes a delivered queue entry once it has aged out of the backlog view.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] on failure.
    pub fn delete_queue_entry(&self, id: &str, seq: u64) -> Result<(), AppserviceError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.txn_queue
                .delete(txn, &(id.to_string(), seq))
                .map_err(to_kv)
        })
        .map_err(|e| AppserviceError::Store(e.to_string()))
    }

    /// Deletes every queue entry for `id` (used when an appservice is removed and the caller
    /// wants no residue).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`]/[`AppserviceError::Decode`] on failure.
    pub fn purge_queue(&self, id: &str) -> Result<(), AppserviceError> {
        for entry in self.queue_for(id)? {
            self.delete_queue_entry(id, entry.seq)?;
        }
        Ok(())
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
    fn health_defaults_when_absent() {
        let s = store();
        let h = s.health("a").unwrap();
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.last_ping_at_ms.is_none());
    }
}
