//! [`TablesE2eStore`]: an `hs-kv`/`hs-tables`-backed implementation of every trait in
//! [`super`], generic over `B: hs_kv::KvBackend`. Follows the shape of
//! `hs-auth/src/store/tables.rs::TablesAuthStore` (read for pattern before writing this): every
//! method is synchronous internally (`hs-kv` transactions may not `.await`), wrapped in this
//! crate's `#[async_trait]` traits at the edge.
//!
//! # Keyspaces
//!
//! | keyspace | primary key | used by |
//! |---|---|---|
//! | `hs_e2e.device_keys` | `(user_id, device_id)` | [`DeviceKeyStore`] |
//! | `hs_e2e.device_list_stream` | `(stream_id: u64,)` | [`DeviceKeyStore`]'s change stream |
//! | `hs_e2e.one_time_keys` | `(user_id, device_id, algorithm, key_id)` | [`OneTimeKeyStore`] — the atomic claim scans every row under `(user_id, device_id, algorithm)` and deletes the one with the lowest stored upload sequence number (MSC4225 order, not key-id sort order), see the module docs on [`super`] |
//! | `hs_e2e.claimed_one_time_keys` | `(user_id, device_id, algorithm, key_id)` | [`OneTimeKeyStore`] — a tombstone per key id ever claimed, so a re-upload of that id cannot resurrect it |
//! | `hs_e2e.fallback_keys` | `(user_id, device_id, algorithm)` | [`FallbackKeyStore`] |
//! | `hs_e2e.cross_signing_keys` | `(user_id, key_type)` | [`CrossSigningStore`] |
//! | `hs_e2e.backup_versions` | `(user_id, version: u64)` | [`BackupStore`] |
//! | `hs_e2e.backup_sessions` | `(user_id, version, room_id, session_id)` | [`BackupStore`] |
//! | `hs_e2e.to_device` | `(user_id, device_id, stream_id: u64)` | [`ToDeviceStore`] |
//! | `hs_e2e.to_device_txn` | `(sender_user, sender_device, txn_id)` | [`ToDeviceStore`] idempotency |
//! | `hs_e2e.counters` | varies (raw tuple-encoded) | monotonic counters via `atomic_add` |
//!
//! No secondary indexes are needed anywhere in this table: every lookup this crate performs is
//! either a point read on a full primary key or a prefix scan on a leading subset of one (a
//! user's devices, a device's one-time keys of one algorithm, a version's sessions, ...), which
//! `hs-tables`' tuple key ordering already serves directly.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use bytes::Bytes;
use hs_kv::{KvBackend, KvError, KvRead, KvWrite, RangeSpec, TransactConfig, transact};
use hs_tables::key::TupleKey;
use hs_tables::keyspace::TypedKeyspace;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    BackupSessionRow, BackupStore, BackupVersionRow, CrossSigningKeyType, CrossSigningStore,
    DeviceKeyStore, DeviceKeysRow, FallbackKeyStore, OneTimeKeyStore, StoreError, ToDeviceMessage,
    ToDeviceStore,
};

type DeviceKeysKey = (String, String);
type DeviceListStreamKey = (u64,);
type OtkKey = (String, String, String, String);
type FallbackKeyKey = (String, String, String);
type CrossSigningKeyKey = (String, String);
type BackupVersionKey = (String, u64);
type BackupSessionKey = (String, u64, String, String);
type ToDeviceKey = (String, String, u64);
type ToDeviceTxnKey = (String, String, String);

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::Backend(format!("decode: {e}")))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(value).map_err(|e| StoreError::Backend(format!("encode: {e}")))
}

#[derive(Debug, thiserror::Error)]
#[error("encode/decode failure: {0}")]
struct DecodeFail(String);

#[derive(Debug, thiserror::Error)]
#[error("counter overflowed u64")]
struct CounterOverflow;

fn decode_kv<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, KvError> {
    serde_json::from_slice(bytes).map_err(|e| KvError::backend(DecodeFail(e.to_string())))
}

fn encode_kv<T: Serialize>(value: &T) -> Result<Vec<u8>, KvError> {
    serde_json::to_vec(value).map_err(|e| KvError::backend(DecodeFail(e.to_string())))
}

fn to_kv<E: std::error::Error + Send + Sync + 'static>(e: E) -> KvError {
    KvError::backend(e)
}

#[derive(Debug, thiserror::Error)]
#[error("row {0:?} does not exist")]
struct RowMissing(String);

fn store_err(e: KvError) -> StoreError {
    if let KvError::Backend(inner) = &e
        && let Some(RowMissing(id)) = inner.downcast_ref::<RowMissing>()
    {
        return StoreError::NotFound(id.clone());
    }
    StoreError::Backend(e.to_string())
}

/// Bumps a named monotonic counter by one and returns its new value. `key` should already be a
/// fully tuple-encoded key unique to this counter (typically produced with
/// [`hs_tables::key::TupleKey::encode`] on a descriptive tuple).
fn next_counter<W: KvWrite>(
    txn: &mut W,
    counters: &W::Keyspace,
    key: &[u8],
) -> Result<u64, KvError> {
    let next = txn.atomic_add(counters, key, 1)?;
    u64::try_from(next).map_err(|_| to_kv(CounterOverflow))
}

/// The `hs-kv`/`hs-tables`-backed [`super::E2eStore`] implementation. See the module docs for
/// the keyspace layout.
pub struct TablesE2eStore<B: KvBackend> {
    backend: B,
    device_keys: TypedKeyspace<B::Keyspace, DeviceKeysKey>,
    device_list_stream: TypedKeyspace<B::Keyspace, DeviceListStreamKey>,
    one_time_keys: TypedKeyspace<B::Keyspace, OtkKey>,
    claimed_one_time_keys: TypedKeyspace<B::Keyspace, OtkKey>,
    fallback_keys: TypedKeyspace<B::Keyspace, FallbackKeyKey>,
    cross_signing_keys: TypedKeyspace<B::Keyspace, CrossSigningKeyKey>,
    backup_versions: TypedKeyspace<B::Keyspace, BackupVersionKey>,
    backup_sessions: TypedKeyspace<B::Keyspace, BackupSessionKey>,
    to_device: TypedKeyspace<B::Keyspace, ToDeviceKey>,
    to_device_txn: TypedKeyspace<B::Keyspace, ToDeviceTxnKey>,
    counters: B::Keyspace,
}

impl<B: KvBackend> TablesE2eStore<B> {
    /// Opens (creating if necessary) every keyspace this store needs.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if a keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let open = |name: &str| -> Result<B::Keyspace, StoreError> {
            backend
                .keyspace(name)
                .map_err(|e| StoreError::Backend(e.to_string()))
        };
        Ok(Self {
            device_keys: TypedKeyspace::new(open("hs_e2e.device_keys")?),
            device_list_stream: TypedKeyspace::new(open("hs_e2e.device_list_stream")?),
            one_time_keys: TypedKeyspace::new(open("hs_e2e.one_time_keys")?),
            claimed_one_time_keys: TypedKeyspace::new(open("hs_e2e.claimed_one_time_keys")?),
            fallback_keys: TypedKeyspace::new(open("hs_e2e.fallback_keys")?),
            cross_signing_keys: TypedKeyspace::new(open("hs_e2e.cross_signing_keys")?),
            backup_versions: TypedKeyspace::new(open("hs_e2e.backup_versions")?),
            backup_sessions: TypedKeyspace::new(open("hs_e2e.backup_sessions")?),
            to_device: TypedKeyspace::new(open("hs_e2e.to_device")?),
            to_device_txn: TypedKeyspace::new(open("hs_e2e.to_device_txn")?),
            counters: open("hs_e2e.counters")?,
            backend,
        })
    }

    /// The underlying backend, for callers that need their own transactions spanning this store
    /// and another table.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    fn bump_device_list(&self, txn: &mut B::Txn, user_id: &UserId) -> Result<u64, KvError> {
        let counter_key = ("device_list_seq".to_string(),).encode();
        let pos = next_counter(txn, &self.counters, &counter_key)?;
        self.device_list_stream
            .put(txn, &(pos,), user_id.as_bytes())
            .map_err(to_kv)?;
        Ok(pos)
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> DeviceKeyStore for TablesE2eStore<B> {
    async fn upload_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: Value,
    ) -> Result<u64, StoreError> {
        let key = (user_id.to_string(), device_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let pos = self.bump_device_list(txn, user_id)?;
            // Cloned per attempt, not moved: `transact` may re-run this body after a write
            // conflict, so the closure is `FnMut` and cannot consume `keys`.
            let row = DeviceKeysRow {
                keys: keys.clone(),
                stream_id: pos,
            };
            let value = encode_kv(&row)?;
            self.device_keys.put(txn, &key, &value).map_err(to_kv)?;
            Ok(pos)
        })
        .map_err(store_err)
    }

    async fn replace_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: Value,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), device_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let stream_id = match self.device_keys.get(txn, &key).map_err(to_kv)? {
                Some(bytes) => decode_kv::<DeviceKeysRow>(&bytes)?.stream_id,
                None => 0,
            };
            let row = DeviceKeysRow {
                keys: keys.clone(),
                stream_id,
            };
            let value = encode_kv(&row)?;
            self.device_keys.put(txn, &key, &value).map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn get_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceKeysRow>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (user_id.to_string(), device_id.to_string());
        match self
            .device_keys
            .get(&snap, &key)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn list_device_keys(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<(OwnedDeviceId, DeviceKeysRow)>, StoreError> {
        let snap = self.backend.snapshot();
        let prefix = TypedKeyspace::<B::Keyspace, DeviceKeysKey>::prefix(&(user_id.to_string(),));
        let mut out = Vec::new();
        for item in self.device_keys.range(&snap, prefix) {
            let ((_u, device_id), value) = item.map_err(|e| StoreError::Backend(e.to_string()))?;
            let row: DeviceKeysRow = decode(&value)?;
            let device_id: OwnedDeviceId = device_id.into();
            out.push((device_id, row));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn delete_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<u64, StoreError> {
        let key = (user_id.to_string(), device_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.device_keys.delete(txn, &key).map_err(to_kv)?;
            self.bump_device_list(txn, user_id)
        })
        .map_err(store_err)
    }

    async fn record_device_list_change(&self, user_id: &UserId) -> Result<u64, StoreError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.bump_device_list(txn, user_id)
        })
        .map_err(store_err)
    }

    async fn current_stream_pos(&self) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        let counter_key = ("device_list_seq".to_string(),).encode();
        match snap
            .get(&self.counters, &counter_key)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            None => Ok(0),
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_ref()
                    .try_into()
                    .map_err(|_| StoreError::Backend("corrupt counter".to_string()))?;
                Ok(u64::try_from(i64::from_be_bytes(arr)).unwrap_or(0))
            }
        }
    }

    async fn changed_users_since(
        &self,
        since: u64,
        upto: Option<u64>,
    ) -> Result<BTreeSet<OwnedUserId>, StoreError> {
        let snap = self.backend.snapshot();
        let start = Bound::Excluded(Bytes::from((since,).encode()));
        let end = match upto {
            Some(u) => Bound::Included(Bytes::from((u,).encode())),
            None => Bound::Unbounded,
        };
        let spec = RangeSpec::new(start, end);
        let mut out = BTreeSet::new();
        for item in self.device_list_stream.range(&snap, spec) {
            let (_k, value) = item.map_err(|e| StoreError::Backend(e.to_string()))?;
            let raw = std::str::from_utf8(&value)
                .map_err(|e| StoreError::Backend(format!("non-utf8 user id in stream: {e}")))?;
            let user_id = UserId::parse(raw)
                .map_err(|e| StoreError::Backend(format!("invalid user id in stream: {e}")))?;
            out.insert(user_id.to_owned());
        }
        Ok(out)
    }
}

fn split_algo_key(composite: &str) -> Result<(&str, &str), StoreError> {
    composite.split_once(':').ok_or_else(|| {
        StoreError::Backend(format!(
            "malformed key id {composite:?}, expected \"algorithm:key_id\""
        ))
    })
}

/// A stored one-time key: the client-supplied key content plus the global upload sequence number
/// it was assigned, so [`TablesE2eStore::claim_one_time_key`] can hand keys out in upload order
/// (MSC4225) rather than in the lexicographic order of their key ids -- which would silently
/// reorder keys whenever a client's key ids don't happen to sort the same way they were uploaded
/// (e.g. uploading id `"1"` before id `"0"`, or crossing the `"9"`/`"10"` boundary).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OtkStored {
    seq: u64,
    value: Value,
}

#[async_trait::async_trait]
impl<B: KvBackend> OneTimeKeyStore for TablesE2eStore<B> {
    async fn upload_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: BTreeMap<String, Value>,
    ) -> Result<(), StoreError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut parsed = Vec::with_capacity(keys.len());
        for (composite, value) in &keys {
            let (algorithm, key_id) = split_algo_key(composite)?;
            parsed.push((algorithm.to_string(), key_id.to_string(), value.clone()));
        }
        let otk_seq_key = ("otk_seq".to_string(),).encode();
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (algorithm, key_id, value) in &parsed {
                let key = (
                    user_id.to_string(),
                    device_id.to_string(),
                    algorithm.clone(),
                    key_id.clone(),
                );
                // A re-upload of an id we have ever held is a no-op, whether that id is still
                // unclaimed (`one_time_keys`) or has already been handed out
                // (`claimed_one_time_keys`). The tombstone is what makes the second case work:
                // claiming *deletes* the live row, so without it a client re-uploading a claimed
                // id would resurrect it and the server could hand the same one-time key to two
                // different peers -- exactly the key reuse Olm's forward secrecy depends on not
                // happening, and something a malicious client can trigger at will.
                if self.one_time_keys.get(txn, &key).map_err(to_kv)?.is_some()
                    || self
                        .claimed_one_time_keys
                        .get(txn, &key)
                        .map_err(to_kv)?
                        .is_some()
                {
                    continue;
                }
                let seq = next_counter(txn, &self.counters, &otk_seq_key)?;
                let stored = OtkStored {
                    seq,
                    value: value.clone(),
                };
                let bytes = encode_kv(&stored)?;
                self.one_time_keys.put(txn, &key, &bytes).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn claim_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        algorithm: &str,
    ) -> Result<Option<(String, Value)>, StoreError> {
        let prefix = (
            user_id.to_string(),
            device_id.to_string(),
            algorithm.to_string(),
        );
        transact(&self.backend, TransactConfig::default(), |txn| {
            let spec = TypedKeyspace::<B::Keyspace, OtkKey>::prefix(&prefix);
            // Scan every remaining key under this device/algorithm (not just the first one found)
            // and pick the one with the lowest upload sequence number, so a claim always returns
            // the oldest-uploaded key regardless of how key ids happen to sort lexicographically
            // (MSC4225: "one-time keys must be issued in the same order they were uploaded").
            let mut oldest: Option<(OtkKey, OtkStored)> = None;
            for item in self.one_time_keys.range(&*txn, spec) {
                let (key, bytes) = item.map_err(to_kv)?;
                let stored: OtkStored = decode_kv(&bytes)?;
                if oldest
                    .as_ref()
                    .is_none_or(|(_, current)| stored.seq < current.seq)
                {
                    oldest = Some((key, stored));
                }
            }
            let Some((key, stored)) = oldest else {
                return Ok(None);
            };
            self.one_time_keys.delete(txn, &key).map_err(to_kv)?;
            // Same transaction as the delete: the tombstone and the removal are one atomic fact,
            // so a crash between them cannot leave a claimed id re-uploadable.
            self.claimed_one_time_keys
                .put(txn, &key, &[])
                .map_err(to_kv)?;
            Ok(Some((key.3, stored.value)))
        })
        .map_err(store_err)
    }

    async fn count_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<BTreeMap<String, u64>, StoreError> {
        let snap = self.backend.snapshot();
        let prefix = TypedKeyspace::<B::Keyspace, OtkKey>::prefix(&(
            user_id.to_string(),
            device_id.to_string(),
        ));
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for item in self.one_time_keys.range(&snap, prefix) {
            let ((_u, _d, algorithm, _key_id), _v) =
                item.map_err(|e| StoreError::Backend(e.to_string()))?;
            *counts.entry(algorithm).or_insert(0) += 1;
        }
        Ok(counts)
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct FallbackRow {
    key_id: String,
    key: Value,
    used: bool,
}

#[async_trait::async_trait]
impl<B: KvBackend> FallbackKeyStore for TablesE2eStore<B> {
    async fn upload_fallback_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        keys: BTreeMap<String, Value>,
    ) -> Result<(), StoreError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut parsed = Vec::with_capacity(keys.len());
        for (composite, value) in &keys {
            let (algorithm, key_id) = split_algo_key(composite)?;
            parsed.push((algorithm.to_string(), key_id.to_string(), value.clone()));
        }
        transact(&self.backend, TransactConfig::default(), |txn| {
            for (algorithm, key_id, value) in &parsed {
                let key = (
                    user_id.to_string(),
                    device_id.to_string(),
                    algorithm.clone(),
                );
                let row = FallbackRow {
                    key_id: key_id.clone(),
                    key: value.clone(),
                    used: false,
                };
                let bytes = encode_kv(&row)?;
                self.fallback_keys.put(txn, &key, &bytes).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn claim_fallback_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        algorithm: &str,
    ) -> Result<Option<(String, Value)>, StoreError> {
        let key = (
            user_id.to_string(),
            device_id.to_string(),
            algorithm.to_string(),
        );
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.fallback_keys.get(txn, &key).map_err(to_kv)? else {
                return Ok(None);
            };
            let mut row: FallbackRow = decode_kv(&bytes)?;
            let key_id = row.key_id.clone();
            let key_value = row.key.clone();
            if !row.used {
                row.used = true;
                let new_bytes = encode_kv(&row)?;
                self.fallback_keys
                    .put(txn, &key, &new_bytes)
                    .map_err(to_kv)?;
            }
            Ok(Some((key_id, key_value)))
        })
        .map_err(store_err)
    }

    async fn unused_fallback_key_algorithms(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Vec<String>, StoreError> {
        let snap = self.backend.snapshot();
        let prefix = TypedKeyspace::<B::Keyspace, FallbackKeyKey>::prefix(&(
            user_id.to_string(),
            device_id.to_string(),
        ));
        let mut out = Vec::new();
        for item in self.fallback_keys.range(&snap, prefix) {
            let ((_u, _d, algorithm), value) =
                item.map_err(|e| StoreError::Backend(e.to_string()))?;
            let row: FallbackRow = decode(&value)?;
            if !row.used {
                out.push(algorithm);
            }
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> CrossSigningStore for TablesE2eStore<B> {
    async fn put_cross_signing_key(
        &self,
        user_id: &UserId,
        key_type: CrossSigningKeyType,
        key: Value,
    ) -> Result<(), StoreError> {
        let composite = (user_id.to_string(), key_type.as_str().to_string());
        let value = encode(&key)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.cross_signing_keys
                .put(txn, &composite, &value)
                .map_err(to_kv)
        })
        .map_err(store_err)
    }

    async fn get_cross_signing_key(
        &self,
        user_id: &UserId,
        key_type: CrossSigningKeyType,
    ) -> Result<Option<Value>, StoreError> {
        let snap = self.backend.snapshot();
        let composite = (user_id.to_string(), key_type.as_str().to_string());
        match self
            .cross_signing_keys
            .get(&snap, &composite)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> BackupStore for TablesE2eStore<B> {
    async fn create_version(
        &self,
        user_id: &UserId,
        algorithm: String,
        auth_data: Value,
    ) -> Result<u64, StoreError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let counter_key = (user_id.to_string(), "backup_version".to_string()).encode();
            let version = next_counter(txn, &self.counters, &counter_key)?;
            let row = BackupVersionRow {
                algorithm: algorithm.clone(),
                auth_data: auth_data.clone(),
                etag: 0,
                count: 0,
                deleted: false,
            };
            let value = encode_kv(&row)?;
            let key = (user_id.to_string(), version);
            self.backup_versions.put(txn, &key, &value).map_err(to_kv)?;
            Ok(version)
        })
        .map_err(store_err)
    }

    async fn get_version(
        &self,
        user_id: &UserId,
        version: Option<u64>,
    ) -> Result<Option<(u64, BackupVersionRow)>, StoreError> {
        let snap = self.backend.snapshot();
        match version {
            Some(v) => {
                let key = (user_id.to_string(), v);
                match self
                    .backup_versions
                    .get(&snap, &key)
                    .map_err(|e| StoreError::Backend(e.to_string()))?
                {
                    Some(bytes) => Ok(Some((v, decode(&bytes)?))),
                    None => Ok(None),
                }
            }
            None => {
                let prefix =
                    TypedKeyspace::<B::Keyspace, BackupVersionKey>::prefix(&(user_id.to_string(),))
                        .reverse();
                for item in self.backup_versions.range(&snap, prefix) {
                    let ((_u, v), value) = item.map_err(|e| StoreError::Backend(e.to_string()))?;
                    let row: BackupVersionRow = decode(&value)?;
                    if !row.deleted {
                        return Ok(Some((v, row)));
                    }
                }
                Ok(None)
            }
        }
    }

    async fn update_version_auth_data(
        &self,
        user_id: &UserId,
        version: u64,
        auth_data: Value,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), version);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.backup_versions.get(txn, &key).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            };
            let mut row: BackupVersionRow = decode_kv(&bytes)?;
            if row.deleted {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            }
            row.auth_data = auth_data.clone();
            let value = encode_kv(&row)?;
            self.backup_versions.put(txn, &key, &value).map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn delete_version(&self, user_id: &UserId, version: u64) -> Result<(), StoreError> {
        let vkey = (user_id.to_string(), version);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.backup_versions.get(txn, &vkey).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            };
            let mut row: BackupVersionRow = decode_kv(&bytes)?;
            row.deleted = true;
            row.count = 0;
            let value = encode_kv(&row)?;
            self.backup_versions
                .put(txn, &vkey, &value)
                .map_err(to_kv)?;

            let prefix = TypedKeyspace::<B::Keyspace, BackupSessionKey>::prefix(&(
                user_id.to_string(),
                version,
            ));
            let to_delete: Vec<BackupSessionKey> = {
                let mut keys = Vec::new();
                for item in self.backup_sessions.range(&*txn, prefix) {
                    let (k, _v) = item.map_err(to_kv)?;
                    keys.push(k);
                }
                keys
            };
            for k in to_delete {
                self.backup_sessions.delete(txn, &k).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn put_session(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
        session_id: &str,
        row: BackupSessionRow,
    ) -> Result<bool, StoreError> {
        let vkey = (user_id.to_string(), version);
        let skey = (
            user_id.to_string(),
            version,
            room_id.to_string(),
            session_id.to_string(),
        );
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(vbytes) = self.backup_versions.get(txn, &vkey).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            };
            let mut vrow: BackupVersionRow = decode_kv(&vbytes)?;
            if vrow.deleted {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            }

            let existing = self.backup_sessions.get(txn, &skey).map_err(to_kv)?;
            let is_new = existing.is_none();
            let replace = match &existing {
                None => true,
                Some(bytes) => {
                    let existing_row: BackupSessionRow = decode_kv(bytes)?;
                    row.is_better_than(&existing_row)
                }
            };
            if replace {
                let value = encode_kv(&row)?;
                self.backup_sessions
                    .put(txn, &skey, &value)
                    .map_err(to_kv)?;
                if is_new {
                    vrow.count += 1;
                }
                vrow.etag += 1;
                let vvalue = encode_kv(&vrow)?;
                self.backup_versions
                    .put(txn, &vkey, &vvalue)
                    .map_err(to_kv)?;
            }
            Ok(replace)
        })
        .map_err(store_err)
    }

    async fn get_session(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
        session_id: &str,
    ) -> Result<Option<BackupSessionRow>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (
            user_id.to_string(),
            version,
            room_id.to_string(),
            session_id.to_string(),
        );
        match self
            .backup_sessions
            .get(&snap, &key)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn get_room_sessions(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
    ) -> Result<BTreeMap<String, BackupSessionRow>, StoreError> {
        let snap = self.backend.snapshot();
        let prefix = TypedKeyspace::<B::Keyspace, BackupSessionKey>::prefix(&(
            user_id.to_string(),
            version,
            room_id.to_string(),
        ));
        let mut out = BTreeMap::new();
        for item in self.backup_sessions.range(&snap, prefix) {
            let ((_u, _v, _r, session_id), value) =
                item.map_err(|e| StoreError::Backend(e.to_string()))?;
            out.insert(session_id, decode(&value)?);
        }
        Ok(out)
    }

    async fn get_all_sessions(
        &self,
        user_id: &UserId,
        version: u64,
    ) -> Result<BTreeMap<String, BTreeMap<String, BackupSessionRow>>, StoreError> {
        let snap = self.backend.snapshot();
        let prefix =
            TypedKeyspace::<B::Keyspace, BackupSessionKey>::prefix(&(user_id.to_string(), version));
        let mut out: BTreeMap<String, BTreeMap<String, BackupSessionRow>> = BTreeMap::new();
        for item in self.backup_sessions.range(&snap, prefix) {
            let ((_u, _v, room_id, session_id), value) =
                item.map_err(|e| StoreError::Backend(e.to_string()))?;
            out.entry(room_id)
                .or_default()
                .insert(session_id, decode(&value)?);
        }
        Ok(out)
    }

    async fn delete_session(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
        session_id: &str,
    ) -> Result<(), StoreError> {
        let vkey = (user_id.to_string(), version);
        let skey = (
            user_id.to_string(),
            version,
            room_id.to_string(),
            session_id.to_string(),
        );
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(vbytes) = self.backup_versions.get(txn, &vkey).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            };
            if self
                .backup_sessions
                .get(txn, &skey)
                .map_err(to_kv)?
                .is_some()
            {
                self.backup_sessions.delete(txn, &skey).map_err(to_kv)?;
                let mut vrow: BackupVersionRow = decode_kv(&vbytes)?;
                vrow.count = vrow.count.saturating_sub(1);
                vrow.etag += 1;
                let vvalue = encode_kv(&vrow)?;
                self.backup_versions
                    .put(txn, &vkey, &vvalue)
                    .map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn delete_room_sessions(
        &self,
        user_id: &UserId,
        version: u64,
        room_id: &str,
    ) -> Result<(), StoreError> {
        let vkey = (user_id.to_string(), version);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(vbytes) = self.backup_versions.get(txn, &vkey).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            };
            let prefix = TypedKeyspace::<B::Keyspace, BackupSessionKey>::prefix(&(
                user_id.to_string(),
                version,
                room_id.to_string(),
            ));
            let to_delete: Vec<BackupSessionKey> = {
                let mut keys = Vec::new();
                for item in self.backup_sessions.range(&*txn, prefix) {
                    let (k, _v) = item.map_err(to_kv)?;
                    keys.push(k);
                }
                keys
            };
            if !to_delete.is_empty() {
                let mut vrow: BackupVersionRow = decode_kv(&vbytes)?;
                for k in to_delete {
                    self.backup_sessions.delete(txn, &k).map_err(to_kv)?;
                    vrow.count = vrow.count.saturating_sub(1);
                }
                vrow.etag += 1;
                let vvalue = encode_kv(&vrow)?;
                self.backup_versions
                    .put(txn, &vkey, &vvalue)
                    .map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn delete_all_sessions(&self, user_id: &UserId, version: u64) -> Result<(), StoreError> {
        let vkey = (user_id.to_string(), version);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(vbytes) = self.backup_versions.get(txn, &vkey).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("backup version {version}"))));
            };
            let prefix = TypedKeyspace::<B::Keyspace, BackupSessionKey>::prefix(&(
                user_id.to_string(),
                version,
            ));
            let to_delete: Vec<BackupSessionKey> = {
                let mut keys = Vec::new();
                for item in self.backup_sessions.range(&*txn, prefix) {
                    let (k, _v) = item.map_err(to_kv)?;
                    keys.push(k);
                }
                keys
            };
            for k in to_delete {
                self.backup_sessions.delete(txn, &k).map_err(to_kv)?;
            }
            let mut vrow: BackupVersionRow = decode_kv(&vbytes)?;
            vrow.count = 0;
            vrow.etag += 1;
            let vvalue = encode_kv(&vrow)?;
            self.backup_versions
                .put(txn, &vkey, &vvalue)
                .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ToDeviceRow {
    sender: String,
    event_type: String,
    content: Value,
}

#[async_trait::async_trait]
impl<B: KvBackend> ToDeviceStore for TablesE2eStore<B> {
    async fn send_to_device(
        &self,
        sender: &UserId,
        recipient: &UserId,
        recipient_device: &DeviceId,
        event_type: &str,
        content: Value,
    ) -> Result<u64, StoreError> {
        let row = ToDeviceRow {
            sender: sender.to_string(),
            event_type: event_type.to_string(),
            content,
        };
        transact(&self.backend, TransactConfig::default(), |txn| {
            let counter_key = (
                recipient.to_string(),
                recipient_device.to_string(),
                "to_device_seq".to_string(),
            )
                .encode();
            let stream_id = next_counter(txn, &self.counters, &counter_key)?;
            let key = (
                recipient.to_string(),
                recipient_device.to_string(),
                stream_id,
            );
            let value = encode_kv(&row)?;
            self.to_device.put(txn, &key, &value).map_err(to_kv)?;
            Ok(stream_id)
        })
        .map_err(store_err)
    }

    async fn poll_since(
        &self,
        user: &UserId,
        device: &DeviceId,
        since: u64,
        limit: usize,
    ) -> Result<(Vec<ToDeviceMessage>, u64), StoreError> {
        let snap = self.backend.snapshot();
        let start = Bound::Excluded(Bytes::from(
            (user.to_string(), device.to_string(), since).encode(),
        ));
        let prefix_end = TypedKeyspace::<B::Keyspace, ToDeviceKey>::prefix(&(
            user.to_string(),
            device.to_string(),
        ));
        let spec = RangeSpec::new(start, prefix_end.end).limit(limit.max(1));
        let mut out = Vec::new();
        let mut last = since;
        for item in self.to_device.range(&snap, spec) {
            let ((_u, _d, stream_id), value) =
                item.map_err(|e| StoreError::Backend(e.to_string()))?;
            let row: ToDeviceRow = decode(&value)?;
            let sender = ruma::UserId::parse(&row.sender)
                .map_err(|e| StoreError::Backend(format!("invalid sender id: {e}")))?
                .to_owned();
            out.push(ToDeviceMessage {
                stream_id,
                sender,
                event_type: row.event_type,
                content: row.content,
            });
            last = stream_id;
        }
        Ok((out, last))
    }

    async fn delete_up_to(
        &self,
        user: &UserId,
        device: &DeviceId,
        upto: u64,
    ) -> Result<(), StoreError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let prefix = TypedKeyspace::<B::Keyspace, ToDeviceKey>::prefix(&(
                user.to_string(),
                device.to_string(),
            ));
            let end = Bound::Included(Bytes::from(
                (user.to_string(), device.to_string(), upto).encode(),
            ));
            let spec = RangeSpec::new(prefix.start, end);
            let to_delete: Vec<ToDeviceKey> = {
                let mut keys = Vec::new();
                for item in self.to_device.range(&*txn, spec) {
                    let (k, _v) = item.map_err(to_kv)?;
                    keys.push(k);
                }
                keys
            };
            for k in to_delete {
                self.to_device.delete(txn, &k).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn check_and_mark_txn(
        &self,
        sender_user: &UserId,
        sender_device: &DeviceId,
        txn_id: &str,
    ) -> Result<bool, StoreError> {
        let key = (
            sender_user.to_string(),
            sender_device.to_string(),
            txn_id.to_string(),
        );
        transact(&self.backend, TransactConfig::default(), |txn| {
            if self.to_device_txn.get(txn, &key).map_err(to_kv)?.is_some() {
                return Ok(true);
            }
            self.to_device_txn.put(txn, &key, &[]).map_err(to_kv)?;
            Ok(false)
        })
        .map_err(store_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn store() -> TablesE2eStore<MemoryBackend> {
        TablesE2eStore::open(MemoryBackend::new()).unwrap()
    }

    fn uid(s: &str) -> ruma::OwnedUserId {
        ruma::UserId::parse(s).unwrap().to_owned()
    }

    fn did(s: &str) -> ruma::OwnedDeviceId {
        ruma::OwnedDeviceId::from(s)
    }

    #[tokio::test]
    async fn device_key_upload_and_fetch_round_trips() {
        let s = store();
        let u = uid("@alice:example.org");
        let d = did("AAAA");
        let keys = serde_json::json!({"algorithms": ["m.olm.v1.curve25519-aes-sha2"], "device_id": "AAAA"});
        let pos = s.upload_device_keys(&u, &d, keys.clone()).await.unwrap();
        assert_eq!(pos, 1);
        let row = s.get_device_keys(&u, &d).await.unwrap().unwrap();
        assert_eq!(row.keys, keys);
        assert_eq!(row.stream_id, 1);
    }

    #[tokio::test]
    async fn changed_users_since_reflects_uploads_but_not_earlier_history() {
        let s = store();
        let alice = uid("@alice:example.org");
        let bob = uid("@bob:example.org");
        s.upload_device_keys(&alice, &did("A1"), serde_json::json!({}))
            .await
            .unwrap();
        let after_alice = s.current_stream_pos().await.unwrap();
        s.upload_device_keys(&bob, &did("B1"), serde_json::json!({}))
            .await
            .unwrap();
        let changed = s.changed_users_since(after_alice, None).await.unwrap();
        assert!(changed.contains(&bob));
        assert!(!changed.contains(&alice));
    }

    #[tokio::test]
    async fn one_time_key_claim_removes_it_so_a_second_claim_gets_nothing() {
        let s = store();
        let u = uid("@alice:example.org");
        let d = did("AAAA");
        let mut keys = BTreeMap::new();
        keys.insert(
            "signed_curve25519:AAAAAQ".to_string(),
            serde_json::json!({"key": "base64key"}),
        );
        s.upload_one_time_keys(&u, &d, keys).await.unwrap();
        assert_eq!(
            s.count_one_time_keys(&u, &d).await.unwrap()["signed_curve25519"],
            1
        );

        let claimed = s
            .claim_one_time_key(&u, &d, "signed_curve25519")
            .await
            .unwrap();
        assert_eq!(claimed.unwrap().0, "AAAAAQ");
        assert!(s.count_one_time_keys(&u, &d).await.unwrap().is_empty());

        let second = s
            .claim_one_time_key(&u, &d, "signed_curve25519")
            .await
            .unwrap();
        assert!(
            second.is_none(),
            "a claimed key must never be handed out twice"
        );
    }

    #[tokio::test]
    async fn re_uploading_the_same_key_id_does_not_resurrect_a_claimed_key() {
        let s = store();
        let u = uid("@alice:example.org");
        let d = did("AAAA");
        let mut keys = BTreeMap::new();
        keys.insert(
            "signed_curve25519:AAAAAQ".to_string(),
            serde_json::json!({"key": "first"}),
        );
        s.upload_one_time_keys(&u, &d, keys.clone()).await.unwrap();
        s.claim_one_time_key(&u, &d, "signed_curve25519")
            .await
            .unwrap();

        // Re-upload with the same composite id but different content: must stay absent.
        keys.insert(
            "signed_curve25519:AAAAAQ".to_string(),
            serde_json::json!({"key": "second"}),
        );
        s.upload_one_time_keys(&u, &d, keys).await.unwrap();
        assert!(s.count_one_time_keys(&u, &d).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fallback_key_is_reusable_but_flagged_used_after_first_claim() {
        let s = store();
        let u = uid("@alice:example.org");
        let d = did("AAAA");
        let mut keys = BTreeMap::new();
        keys.insert(
            "signed_curve25519:FALLBACK".to_string(),
            serde_json::json!({"key": "fb", "fallback": true}),
        );
        s.upload_fallback_keys(&u, &d, keys).await.unwrap();
        assert_eq!(
            s.unused_fallback_key_algorithms(&u, &d).await.unwrap(),
            vec!["signed_curve25519".to_string()]
        );
        let claimed1 = s
            .claim_fallback_key(&u, &d, "signed_curve25519")
            .await
            .unwrap();
        assert!(claimed1.is_some());
        assert!(
            s.unused_fallback_key_algorithms(&u, &d)
                .await
                .unwrap()
                .is_empty()
        );
        // Still claimable a second time (reusable).
        let claimed2 = s
            .claim_fallback_key(&u, &d, "signed_curve25519")
            .await
            .unwrap();
        assert_eq!(claimed1, claimed2);
    }

    #[tokio::test]
    async fn backup_session_replacement_follows_is_better_than() {
        let s = store();
        let u = uid("@alice:example.org");
        let version = s
            .create_version(&u, "m.megolm_backup.v1".to_string(), serde_json::json!({}))
            .await
            .unwrap();
        let worse = BackupSessionRow {
            first_message_index: 10,
            forwarded_count: 2,
            is_verified: false,
            session_data: serde_json::json!({"v": 1}),
        };
        let better = BackupSessionRow {
            first_message_index: 5,
            forwarded_count: 0,
            is_verified: true,
            session_data: serde_json::json!({"v": 2}),
        };
        assert!(
            s.put_session(&u, version, "!r:x", "s1", worse.clone())
                .await
                .unwrap()
        );
        assert!(
            s.put_session(&u, version, "!r:x", "s1", better.clone())
                .await
                .unwrap()
        );
        // Attempting to put the worse one back must not replace the better one already stored.
        assert!(
            !s.put_session(&u, version, "!r:x", "s1", worse)
                .await
                .unwrap()
        );
        let stored = s
            .get_session(&u, version, "!r:x", "s1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, better);

        let (returned_version, vrow) = s.get_version(&u, Some(version)).await.unwrap().unwrap();
        assert_eq!(returned_version, version);
        assert_eq!(vrow.count, 1);
        assert_eq!(vrow.etag, 2);
    }

    #[tokio::test]
    async fn deleting_a_backup_version_drops_its_sessions() {
        let s = store();
        let u = uid("@alice:example.org");
        let version = s
            .create_version(&u, "m.megolm_backup.v1".to_string(), serde_json::json!({}))
            .await
            .unwrap();
        let row = BackupSessionRow {
            first_message_index: 0,
            forwarded_count: 0,
            is_verified: true,
            session_data: serde_json::json!({}),
        };
        s.put_session(&u, version, "!r:x", "s1", row).await.unwrap();
        s.delete_version(&u, version).await.unwrap();
        assert!(s.get_version(&u, None).await.unwrap().is_none());
        assert!(
            s.get_session(&u, version, "!r:x", "s1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn to_device_poll_since_and_delete_up_to() {
        let s = store();
        let alice = uid("@alice:example.org");
        let bob = uid("@bob:example.org");
        let bob_device = did("BBBB");
        s.send_to_device(
            &alice,
            &bob,
            &bob_device,
            "m.room_key",
            serde_json::json!({"n": 1}),
        )
        .await
        .unwrap();
        s.send_to_device(
            &alice,
            &bob,
            &bob_device,
            "m.room_key",
            serde_json::json!({"n": 2}),
        )
        .await
        .unwrap();

        let (msgs, next) = s.poll_since(&bob, &bob_device, 0, 10).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(next, 2);

        s.delete_up_to(&bob, &bob_device, next).await.unwrap();
        let (msgs2, _) = s.poll_since(&bob, &bob_device, 0, 10).await.unwrap();
        assert!(msgs2.is_empty());
    }

    #[tokio::test]
    async fn to_device_txn_dedup() {
        let s = store();
        let alice = uid("@alice:example.org");
        let alice_device = did("AAAA");
        assert!(
            !s.check_and_mark_txn(&alice, &alice_device, "txn1")
                .await
                .unwrap()
        );
        assert!(
            s.check_and_mark_txn(&alice, &alice_device, "txn1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn fjall_backed_store_survives_reopen_from_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let u = uid("@durable:example.org");
        let d = did("DUR1");
        {
            let backend = hs_kv::fjall_backend::FjallBackend::open(dir.path()).unwrap();
            let store = TablesE2eStore::open(backend).unwrap();
            store
                .upload_device_keys(&u, &d, serde_json::json!({"device_id": "DUR1"}))
                .await
                .unwrap();
            let mut keys = BTreeMap::new();
            keys.insert(
                "signed_curve25519:K1".to_string(),
                serde_json::json!({"key": "k"}),
            );
            store.upload_one_time_keys(&u, &d, keys).await.unwrap();
        }
        let backend = hs_kv::fjall_backend::FjallBackend::open(dir.path()).unwrap();
        let store = TablesE2eStore::open(backend).unwrap();
        assert!(store.get_device_keys(&u, &d).await.unwrap().is_some());
        assert_eq!(
            store.count_one_time_keys(&u, &d).await.unwrap()["signed_curve25519"],
            1
        );
    }
}
