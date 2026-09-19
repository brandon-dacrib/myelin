//! Storage traits for everything track 08 owns: device identity keys, one-time and fallback
//! keys, cross-signing keys, the device-list change stream, key backups and to-device messages.
//!
//! One implementation exists: [`tables::TablesE2eStore`], generic over `hs_kv::KvBackend` (in
//! practice `hs_kv::memory::MemoryBackend` for tests, `hs_kv::fjall_backend::FjallBackend` for a
//! real `hs serve` process). Unlike `hs-auth`, this crate does not also carry a hand-rolled
//! in-memory implementation: `hs-auth`'s two implementations are a historical artifact (the
//! in-memory one predates `hs-tables`), and this track starts directly on the tables-backed
//! design its brief asks for — see `docs/status/08-e2ee.md`'s "Reuse considered" for the record.
//!
//! # Why the atomic one-time-key claim is the point of this module
//!
//! [`OneTimeKeyStore::claim_one_time_key`] must remove the key it returns in the same
//! transaction that reads it, so two concurrent claims for the same device and algorithm can
//! never receive the same key — a double claim is the classic bug that produces an undecryptable
//! message (both parties think they hold the one-time key the other used to establish a
//! session). [`tables::TablesE2eStore`]'s implementation relies entirely on `hs-kv`'s
//! serializable snapshot isolation for this: it scans for the lexicographically first key under
//! the device's `(user_id, device_id, algorithm)` prefix and deletes it inside one
//! `hs_kv::transact` closure. Two concurrent claims that both scan and see the same key have that
//! scan recorded in their read sets; whichever commits first deletes the key (removing it from
//! the range), so the second transaction's commit-time validation sees its scanned range has
//! changed and reports [`hs_kv::Conflict`], forcing a retry that then sees the key already gone.
//! No lock, no special "claim" primitive — plain serializability. `store::tables::tests` and
//! `tests/otk_concurrency.rs` are the executable proof, run against both the in-memory and Fjall
//! backends with real OS threads.

pub mod tables;

use std::collections::BTreeMap;

use async_trait::async_trait;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Errors from a storage backend. Deliberately thin, matching `hs-auth`'s `StoreError` — callers
/// map these to `M_UNKNOWN`/500 and log the detail.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The operation conflicts with something that must be unique (a version number, a backup
    /// session already present with no better replacement offered, ...).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The referenced record does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Any other backend failure.
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// Which of the three cross-signing key roles a stored key plays (Matrix spec section 11.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CrossSigningKeyType {
    /// The identity anchor: signs the self-signing and user-signing keys.
    Master,
    /// Signs this user's own devices.
    SelfSigning,
    /// Signs other users' master keys, once this user has verified them.
    UserSigning,
}

impl CrossSigningKeyType {
    /// The stable string this type is keyed and reported by (`master_key` / `self_signing_key` /
    /// `user_signing_key` in `/keys/query`'s response, and this row's table key component).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::SelfSigning => "self_signing",
            Self::UserSigning => "user_signing",
        }
    }
}

/// One local device's identity keys, as last uploaded via `POST /keys/upload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceKeysRow {
    /// The full `device_keys` JSON object as uploaded (`user_id`, `device_id`, `algorithms`,
    /// `keys`, `signatures`), stored and returned verbatim — this crate distributes keys, it
    /// never interprets their content.
    pub keys: Value,
    /// The device-list stream position of the most recent change to this row (upload, or a
    /// cross-signing signature merged onto it by `/keys/signatures/upload`).
    pub stream_id: u64,
}

/// One backup version's metadata (`GET /room_keys/version`'s shape, minus `version` itself which
/// is the row's own key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupVersionRow {
    /// `m.megolm_backup.v1.curve25519-aes-sha2` or a future algorithm — opaque to this crate.
    pub algorithm: String,
    /// Algorithm-specific public data (e.g. the backup's public key), opaque to this crate.
    pub auth_data: Value,
    /// Bumped on every session write accepted into this version; the spec's `etag` is this
    /// counter's decimal string. Clients use it to detect that another device has changed the
    /// backup without downloading it.
    pub etag: u64,
    /// The number of key backups currently stored in this version.
    pub count: u64,
    /// Set when the version has been deleted. Rows are kept (not removed) after deletion so a
    /// deleted version's number is never reused and a late write attempt gets a clean
    /// [`StoreError::NotFound`] rather than silently landing on a reused number.
    pub deleted: bool,
}

/// One room key backed up to a version (`GET /room_keys/keys/...`'s per-session shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupSessionRow {
    /// The index of the first message the session key can decrypt.
    pub first_message_index: u64,
    /// How many times this key has been forwarded (lower is more trusted).
    pub forwarded_count: u64,
    /// Whether the device that uploaded this key had verified the session's owning device.
    pub is_verified: bool,
    /// The encrypted session data, opaque to this crate.
    pub session_data: Value,
}

impl BackupSessionRow {
    /// Whether `self` should replace `existing` in a backup, per the spec's `room_keys`
    /// algorithm (`POST /room_keys/keys` / `PUT .../{roomId}/{sessionId}`): a session is "better"
    /// only if it has a lower forwarded count, or an equal one and is verified where the existing
    /// one was not, or an equal count and verification but a lower first message index (i.e. it
    /// can decrypt more of the room's history).
    #[must_use]
    pub fn is_better_than(&self, existing: &BackupSessionRow) -> bool {
        if self.forwarded_count != existing.forwarded_count {
            return self.forwarded_count < existing.forwarded_count;
        }
        if self.is_verified != existing.is_verified {
            return self.is_verified && !existing.is_verified;
        }
        self.first_message_index < existing.first_message_index
    }
}

/// One queued to-device message, as it will be delivered to a recipient device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToDeviceMessage {
    /// This message's position in the recipient device's own to-device stream — the cursor
    /// `/sync` (track 05) will read and acknowledge against.
    pub stream_id: u64,
    /// The sending user.
    pub sender: OwnedUserId,
    /// The `m.*` event type (e.g. `m.room.encrypted`, `m.room_key_request`).
    pub event_type: String,
    /// The event content, opaque to this crate.
    pub content: Value,
}

/// Device identity keys and the device-list change stream sync (track 05) and federation
/// (track 06) will consume.
#[async_trait]
pub trait DeviceKeyStore: Send + Sync {
    /// Stores (overwriting any previous upload for this device) `keys` and bumps the device-list
    /// stream, returning the new global stream position.
    async fn upload_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: Value,
    ) -> Result<u64, StoreError>;

    /// Overwrites the stored `keys` value for a device without changing its stream position —
    /// used by `/keys/signatures/upload` to merge in a new signature, which the spec does not
    /// itself treat as a device-list-changing event distinct from the upload that added the key
    /// being signed.
    async fn replace_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: Value,
    ) -> Result<(), StoreError>;

    /// Looks up one device's keys.
    async fn get_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceKeysRow>, StoreError>;

    /// Every device with uploaded keys for a user, ordered by `device_id`.
    async fn list_device_keys(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<(OwnedDeviceId, DeviceKeysRow)>, StoreError>;

    /// Removes a device's stored keys (one-time/fallback keys are not touched; callers that also
    /// want those gone call the corresponding methods) and bumps the device-list stream.
    async fn delete_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<u64, StoreError>;

    /// Records a device-list change for `user_id` with no specific device (used when a
    /// cross-signing key changes, which the spec also requires to appear in `/keys/changes`).
    async fn record_device_list_change(&self, user_id: &UserId) -> Result<u64, StoreError>;

    /// The current (most recently assigned) device-list stream position, or `0` if nothing has
    /// ever changed.
    async fn current_stream_pos(&self) -> Result<u64, StoreError>;

    /// Every distinct user whose device list changed with a stream position greater than `since`
    /// and at most `upto` (`upto = None` means "no upper bound", i.e. up to
    /// [`DeviceKeyStore::current_stream_pos`]).
    async fn changed_users_since(
        &self,
        since: u64,
        upto: Option<u64>,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, StoreError>;
}

/// One-time keys: upload, atomic claim, and per-algorithm counts.
#[async_trait]
pub trait OneTimeKeyStore: Send + Sync {
    /// Adds one-time keys, keyed by `"<algorithm>:<key_id>"` exactly as the spec's
    /// `one_time_keys` upload body names them. Keys already present under the same algorithm and
    /// key id are left unchanged (matching Synapse: a re-upload of an id the server already has
    /// is a harmless no-op, not an overwrite — a client that reuses a key id after the server
    /// already claimed and discarded it must not have that stale re-upload resurrect it under a
    /// new identity).
    async fn upload_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: BTreeMap<String, Value>,
    ) -> Result<(), StoreError>;

    /// Atomically claims and removes one key of `algorithm` for this device, if any remain. See
    /// the module docs for why this is the one operation in this crate that must never be
    /// approximated.
    async fn claim_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        algorithm: &str,
    ) -> Result<Option<(String, Value)>, StoreError>;

    /// The number of remaining one-time keys for this device, by algorithm (only algorithms with
    /// at least one key present are included, matching the spec's `one_time_key_counts`).
    async fn count_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<BTreeMap<String, u64>, StoreError>;
}

/// Fallback keys (MSC2732 / spec section 11.12.2): a single reusable key per algorithm, served
/// when a device's one-time keys have run out.
#[async_trait]
pub trait FallbackKeyStore: Send + Sync {
    /// Replaces the stored fallback key for each algorithm present in `keys` (only the most
    /// recently uploaded fallback key per algorithm is kept, matching the spec), marking each as
    /// unused again.
    async fn upload_fallback_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: BTreeMap<String, Value>,
    ) -> Result<(), StoreError>;

    /// Returns the fallback key id and key for `algorithm`, if one is stored, marking it used.
    /// Unlike a one-time key, a fallback key is not deleted: the spec allows it to be claimed
    /// again by a later request once no one-time keys remain, which is exactly why it is flagged
    /// "used" rather than removed — so the *first* claim after upload is the signal a client
    /// relies on to know it should upload a fresh one. The returned key id is whatever id the
    /// device originally uploaded it under (the `"algorithm:key_id"` composite's second half),
    /// so a claimant can report back exactly which key it received the same way a one-time-key
    /// claim does.
    async fn claim_fallback_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        algorithm: &str,
    ) -> Result<Option<(String, Value)>, StoreError>;

    /// Algorithms with a fallback key stored that has not yet been claimed.
    async fn unused_fallback_key_algorithms(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Vec<String>, StoreError>;
}

/// Cross-signing keys (spec section 11.12): master, self-signing and user-signing.
#[async_trait]
pub trait CrossSigningStore: Send + Sync {
    /// Stores (overwriting) `user_id`'s key of the given type.
    async fn put_cross_signing_key(
        &self,
        user_id: &UserId,
        key_type: CrossSigningKeyType,
        key: Value,
    ) -> Result<(), StoreError>;

    /// Looks up `user_id`'s key of the given type.
    async fn get_cross_signing_key(
        &self,
        user_id: &UserId,
        key_type: CrossSigningKeyType,
    ) -> Result<Option<Value>, StoreError>;
}

/// Key backup versions and their contents.
#[async_trait]
pub trait BackupStore: Send + Sync {
    /// Creates a new backup version and returns its number. Version numbers are a per-user
    /// monotonic counter starting at `1`; they are never reused, including after deletion.
    async fn create_version(
        &self,
        user_id: &UserId,
        algorithm: String,
        auth_data: Value,
    ) -> Result<u64, StoreError>;

    /// Looks up a version's number and metadata. `version: None` means "the highest-numbered
    /// version that has not been deleted" (the spec's "the currently active backup"); `Some(v)`
    /// looks up exactly `v` even if deleted (callers check `.deleted`). The version number is
    /// returned alongside the row (rather than making the caller already know it, as `None`
    /// implies they might not) since both `GET /room_keys/version` and a resolved write both need
    /// it immediately after this call.
    async fn get_version(
        &self,
        user_id: &UserId,
        version: Option<u64>,
    ) -> Result<Option<(u64, BackupVersionRow)>, StoreError>;

    /// Replaces a version's `auth_data` in place (`PUT /room_keys/version/{version}`). Errors
    /// with [`StoreError::NotFound`] if the version does not exist or was deleted.
    async fn update_version_auth_data(
        &self,
        user_id: &UserId,
        version: u64,
        auth_data: Value,
    ) -> Result<(), StoreError>;

    /// Marks a version deleted and drops every session stored in it.
    async fn delete_version(&self, user_id: &UserId, version: u64) -> Result<(), StoreError>;

    /// Inserts or replaces one session, applying [`BackupSessionRow::is_better_than`] against
    /// any existing entry with the same `room_id`/`session_id`. Returns `true` if the store now
    /// holds `row` (inserted, or replaced a worse existing entry), `false` if an existing, better
    /// entry was kept unchanged. Bumps the version's `etag` whenever the stored contents
    /// actually change.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] if `version` does not exist or was deleted.
    async fn put_session(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
        session_id: &str,
        row: BackupSessionRow,
    ) -> Result<bool, StoreError>;

    /// Looks up one session.
    async fn get_session(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
        session_id: &str,
    ) -> Result<Option<BackupSessionRow>, StoreError>;

    /// Every session backed up for one room in a version, by session id.
    async fn get_room_sessions(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
    ) -> Result<BTreeMap<String, BackupSessionRow>, StoreError>;

    /// Every session backed up in a version, by room id then session id.
    async fn get_all_sessions(
        &self,
        user_id: &UserId,
        version: u64,
    ) -> Result<BTreeMap<String, BTreeMap<String, BackupSessionRow>>, StoreError>;

    /// Deletes one session.
    async fn delete_session(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
        session_id: &str,
    ) -> Result<(), StoreError>;

    /// Deletes every session backed up for a room in a version.
    async fn delete_room_sessions(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
    ) -> Result<(), StoreError>;

    /// Deletes every session in a version (keeping the version itself, unlike
    /// [`BackupStore::delete_version`]).
    async fn delete_all_sessions(&self, user_id: &UserId, version: u64) -> Result<(), StoreError>;
}

/// The to-device message queue: local delivery and the cursor `/sync` will read.
#[async_trait]
pub trait ToDeviceStore: Send + Sync {
    /// Enqueues one message for a local recipient device, returning its assigned per-device
    /// stream id.
    async fn send_to_device(
        &self,
        sender: &UserId,
        recipient: &UserId,
        recipient_device: &DeviceId,
        event_type: &str,
        content: Value,
    ) -> Result<u64, StoreError>;

    /// Messages for `(user, device)` with stream id greater than `since`, oldest first, capped at
    /// `limit`. Returns the messages and the stream id to use as `since` on the next call (the
    /// highest stream id returned, or `since` unchanged if nothing was found).
    async fn poll_since(
        &self,
        user: &UserId,
        device: &DeviceId,
        since: u64,
        limit: usize,
    ) -> Result<(Vec<ToDeviceMessage>, u64), StoreError>;

    /// Deletes every message for `(user, device)` with stream id at most `upto` — called once a
    /// sync response that included them has been acknowledged.
    async fn delete_up_to(
        &self,
        user: &UserId,
        device: &DeviceId,
        upto: u64,
    ) -> Result<(), StoreError>;

    /// Idempotency for `PUT /sendToDevice/{eventType}/{txnId}`: returns `true` if this
    /// `(sender_user, sender_device, txn_id)` was already processed (the caller must not enqueue
    /// again), and otherwise marks it processed and returns `false`.
    async fn check_and_mark_txn(
        &self,
        sender_user: &UserId,
        sender_device: &DeviceId,
        txn_id: &str,
    ) -> Result<bool, StoreError>;
}

/// The union of every storage trait this crate needs, for callers that want "the e2e store"
/// without naming each capability individually — mirrors `hs_auth::store::AuthStore`.
pub trait E2eStore:
    DeviceKeyStore
    + OneTimeKeyStore
    + FallbackKeyStore
    + CrossSigningStore
    + BackupStore
    + ToDeviceStore
{
}
impl<
    T: DeviceKeyStore
        + OneTimeKeyStore
        + FallbackKeyStore
        + CrossSigningStore
        + BackupStore
        + ToDeviceStore,
> E2eStore for T
{
}
