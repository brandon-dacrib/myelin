//! [`TablesAuthStore`]: an `hs-kv`/`hs-tables`-backed implementation of every trait in
//! [`super`], generic over `B: hs_kv::KvBackend` — in practice `hs_kv::memory::MemoryBackend` for
//! fast tests of this module itself, or `hs_kv::fjall_backend::FjallBackend` for a real `hs serve`
//! process that must survive a restart. Follows the same shape as
//! `hs-appservice/src/store.rs::AppserviceStore` (read for pattern before writing this): every
//! method is synchronous internally (`hs-kv` transactions may not `.await`), wrapped in this
//! crate's `#[async_trait]` traits at the edge so callers are unaffected by which store backs
//! them.
//!
//! # Keyspaces and indexes
//!
//! | keyspace | primary key | secondary index | used by |
//! |---|---|---|---|
//! | `hs_auth.users` | `(user_id,)` | `hs_auth.users_by_localpart_lower` (unique, `(localpart_lower,)`) | `UserStore::create_user`/`is_localpart_available` — case-insensitive localpart conflict check |
//! | `hs_auth.devices` | `(user_id, device_id)` | none — `DeviceStore::list_devices` is a prefix scan on `(user_id,)`, needing no separate index since the primary key already sorts by user first | `DeviceStore` |
//! | `hs_auth.access_tokens` | `(hash_hex,)` | `hs_auth.access_tokens_by_user` (non-unique, `(user_id,)`) | `TokenStore`'s access-token methods, including the "for device"/"except one" families, which filter the user-scoped index result in memory rather than adding a second index (see `delete_access_tokens_for_device` below) |
//! | `hs_auth.refresh_tokens` | `(hash_hex,)` | `hs_auth.refresh_tokens_by_user` (non-unique, `(user_id,)`) | `TokenStore`'s refresh-token methods |
//! | `hs_auth.login_tokens` | `(hash_hex,)` | none — only ever looked up by its own hash | `TokenStore`'s login-token methods |
//! | `hs_auth.uia_sessions` | `(session_id,)` | none | `UiaStore` |
//! | `hs_auth.threepids` | `(medium, address_lower)` | none — never looked up by user | `UserStore`'s 3PID methods |
//!
//! Every index is maintained by [`hs_tables::index::maintain_index`] inside the same write
//! transaction as the row it indexes (never a separate write that could diverge). The property
//! tests in this module's `tests` submodule check, over randomized sequences of insert/update/
//! delete, that no orphaned index row is ever left behind — the same style
//! `crates/hs-tables/tests/index_proptest.rs` already uses for `hs-tables` itself.

use std::collections::HashMap;

use hs_kv::{KvBackend, KvError, RangeSpec, TransactConfig, transact};
use hs_tables::index::{IndexDef, lookup, maintain_index};
use hs_tables::keyspace::TypedKeyspace;
use rand::Rng;
use rand::distr::Alphanumeric;
use ruma::{DeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};

use super::{
    AccessTokenRecord, DeviceRecord, DeviceStore, LoginTokenRecord, RefreshTokenRecord, StoreError,
    TokenStore, UiaStore, UserRecord, UserStore,
};
use crate::token::TokenHash;

/// One user-interactive-auth session's stored state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UiaSessionRow {
    created_at_ms: u64,
    completed: Vec<String>,
    data: HashMap<String, serde_json::Value>,
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::Backend(format!("decode: {e}")))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(value).map_err(|e| StoreError::Backend(format!("encode: {e}")))
}

/// Wraps any `std::error::Error` (in practice `hs_tables::TableError`/`IndexError`, or this
/// module's own marker errors below) as an opaque `hs_kv::KvError::Backend`, so it can flow
/// through a `hs_kv::transact` closure, which only knows about `KvError`.
fn to_kv<E: std::error::Error + Send + Sync + 'static>(e: E) -> KvError {
    KvError::backend(e)
}

#[derive(Debug, thiserror::Error)]
#[error("row {0:?} already exists")]
struct RowExists(String);

#[derive(Debug, thiserror::Error)]
#[error("row {0:?} does not exist")]
struct RowMissing(String);

/// Maps a `hs_kv::transact` failure back to this crate's `StoreError`, recognizing the marker
/// errors this module wraps (`RowExists`, `RowMissing`) and `hs_tables::IndexError::UniqueConflict`
/// specifically, so callers see `StoreError::Conflict`/`StoreError::NotFound` rather than an opaque
/// backend error for the cases this trait's contract documents.
fn store_err(e: KvError) -> StoreError {
    if let KvError::Backend(inner) = &e {
        if let Some(RowExists(id)) = inner.downcast_ref::<RowExists>() {
            return StoreError::Conflict(id.clone());
        }
        if let Some(RowMissing(id)) = inner.downcast_ref::<RowMissing>() {
            return StoreError::NotFound(id.clone());
        }
        if let Some(hs_tables::IndexError::UniqueConflict) =
            inner.downcast_ref::<hs_tables::IndexError>()
        {
            return StoreError::Conflict("index key already used by a different row".to_string());
        }
    }
    StoreError::Backend(e.to_string())
}

fn localpart_lower(user_id: &UserId) -> String {
    user_id.localpart().to_ascii_lowercase()
}

fn row_localpart_lower(_pk: &(String,), value: &[u8]) -> Option<(String,)> {
    let record: UserRecord = decode(value).ok()?;
    Some((localpart_lower(&record.user_id),))
}

fn row_owner_user_id(_pk: &(String,), value: &[u8]) -> Option<(String,)> {
    let record: AccessTokenRecord = decode(value).ok()?;
    Some((record.user_id.to_string(),))
}

fn row_owner_user_id_refresh(_pk: &(String,), value: &[u8]) -> Option<(String,)> {
    let record: RefreshTokenRecord = decode(value).ok()?;
    Some((record.user_id.to_string(),))
}

/// The `hs-kv`/`hs-tables`-backed [`super::AuthStore`] implementation. See the module docs for
/// the keyspace/index layout.
pub struct TablesAuthStore<B: KvBackend> {
    backend: B,
    users: TypedKeyspace<B::Keyspace, (String,)>,
    users_by_localpart_lower: IndexDef<B::Keyspace, (String,), (String,)>,
    devices: TypedKeyspace<B::Keyspace, (String, String)>,
    access_tokens: TypedKeyspace<B::Keyspace, (String,)>,
    access_tokens_by_user: IndexDef<B::Keyspace, (String,), (String,)>,
    refresh_tokens: TypedKeyspace<B::Keyspace, (String,)>,
    refresh_tokens_by_user: IndexDef<B::Keyspace, (String,), (String,)>,
    login_tokens: TypedKeyspace<B::Keyspace, (String,)>,
    uia_sessions: TypedKeyspace<B::Keyspace, (String,)>,
    threepids: TypedKeyspace<B::Keyspace, (String, String)>,
}

impl<B: KvBackend> TablesAuthStore<B> {
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
        let users = TypedKeyspace::new(open("hs_auth.users")?);
        let users_by_localpart_lower = IndexDef::new(
            open("hs_auth.users_by_localpart_lower")?,
            true,
            row_localpart_lower,
        );
        let devices = TypedKeyspace::new(open("hs_auth.devices")?);
        let access_tokens = TypedKeyspace::new(open("hs_auth.access_tokens")?);
        let access_tokens_by_user = IndexDef::new(
            open("hs_auth.access_tokens_by_user")?,
            false,
            row_owner_user_id,
        );
        let refresh_tokens = TypedKeyspace::new(open("hs_auth.refresh_tokens")?);
        let refresh_tokens_by_user = IndexDef::new(
            open("hs_auth.refresh_tokens_by_user")?,
            false,
            row_owner_user_id_refresh,
        );
        let login_tokens = TypedKeyspace::new(open("hs_auth.login_tokens")?);
        let uia_sessions = TypedKeyspace::new(open("hs_auth.uia_sessions")?);
        let threepids = TypedKeyspace::new(open("hs_auth.threepids")?);
        Ok(Self {
            backend,
            users,
            users_by_localpart_lower,
            devices,
            access_tokens,
            access_tokens_by_user,
            refresh_tokens,
            refresh_tokens_by_user,
            login_tokens,
            uia_sessions,
            threepids,
        })
    }

    /// The underlying backend, for callers that need their own transactions spanning this store
    /// and another table.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> UserStore for TablesAuthStore<B> {
    async fn get_user(&self, user_id: &UserId) -> Result<Option<UserRecord>, StoreError> {
        let snap = self.backend.snapshot();
        match self
            .users
            .get(&snap, &(user_id.to_string(),))
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn create_user(&self, record: UserRecord) -> Result<(), StoreError> {
        let key = (record.user_id.to_string(),);
        let value = encode(&record)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            if self.users.get(txn, &key).map_err(to_kv)?.is_some() {
                return Err(to_kv(RowExists(record.user_id.to_string())));
            }
            self.users.put(txn, &key, &value).map_err(to_kv)?;
            maintain_index(
                txn,
                &self.users_by_localpart_lower,
                &key,
                None,
                Some(&value),
            )
            .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn is_localpart_available(&self, localpart: &str) -> Result<bool, StoreError> {
        let snap = self.backend.snapshot();
        let key = (localpart.to_ascii_lowercase(),);
        let pks = lookup(&snap, &self.users_by_localpart_lower, &key)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(pks.is_empty())
    }

    async fn set_password_hash(
        &self,
        user_id: &UserId,
        hash: Option<String>,
    ) -> Result<(), StoreError> {
        self.update_user(user_id, |u| u.password_hash = hash.clone())
            .await
    }

    async fn set_admin(&self, user_id: &UserId, admin: bool) -> Result<(), StoreError> {
        self.update_user(user_id, |u| u.is_admin = admin).await
    }

    async fn set_locked(&self, user_id: &UserId, locked: bool) -> Result<(), StoreError> {
        self.update_user(user_id, |u| u.locked = locked).await
    }

    async fn set_suspended(&self, user_id: &UserId, suspended: bool) -> Result<(), StoreError> {
        self.update_user(user_id, |u| u.suspended = suspended).await
    }

    async fn set_deactivated(&self, user_id: &UserId, deactivated: bool) -> Result<(), StoreError> {
        self.update_user(user_id, |u| u.deactivated = deactivated)
            .await
    }

    async fn bind_threepid(
        &self,
        user_id: &UserId,
        medium: &str,
        address: &str,
    ) -> Result<(), StoreError> {
        let key = (medium.to_string(), address.to_ascii_lowercase());
        let value = encode(&user_id.to_string())?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.threepids.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(store_err)
    }

    async fn get_user_by_threepid(
        &self,
        medium: &str,
        address: &str,
    ) -> Result<Option<OwnedUserId>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (medium.to_string(), address.to_ascii_lowercase());
        match self
            .threepids
            .get(&snap, &key)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => {
                let raw: String = decode(&bytes)?;
                UserId::parse(&raw)
                    .map(Some)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            }
            None => Ok(None),
        }
    }

    /// A **full keyspace scan** of `hs_auth.users` -- there is no secondary index this could use
    /// instead (`users_by_localpart_lower` is a point-lookup index, not an enumeration one), so
    /// this reads every row in the keyspace on every call, decodes it, and sorts the result. See
    /// [`UserStore::list_users`]'s doc comment for the cost trade-off; acceptable for a single
    /// operator's admin-API user directory, not something to call in a hot path.
    async fn list_users(&self) -> Result<Vec<UserRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let mut users: Vec<UserRecord> = self
            .users
            .range(&snap, RangeSpec::full())
            .map(|item| {
                let (_k, v) = item.map_err(|e| StoreError::Backend(e.to_string()))?;
                decode(&v)
            })
            .collect::<Result<_, StoreError>>()?;
        users.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        Ok(users)
    }
}

impl<B: KvBackend> TablesAuthStore<B> {
    /// Reads a user, applies `f`, writes it back — the shared shape behind every `UserStore`
    /// flag-setter. The localpart-lower index never needs maintaining here since none of these
    /// setters can change `user_id`.
    async fn update_user(
        &self,
        user_id: &UserId,
        f: impl Fn(&mut UserRecord) + Send,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.users.get(txn, &key).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(user_id.to_string())));
            };
            let mut record: UserRecord = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            f(&mut record);
            let value = serde_json::to_vec(&record)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.users.put(txn, &key, &value).map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("encode/decode failure: {0}")]
struct DecodeFail(String);

#[async_trait::async_trait]
impl<B: KvBackend> DeviceStore for TablesAuthStore<B> {
    async fn upsert_device(&self, record: DeviceRecord) -> Result<(), StoreError> {
        let key = (record.user_id.to_string(), record.device_id.to_string());
        let value = encode(&record)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.devices.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(store_err)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (user_id.to_string(), device_id.to_string());
        match self
            .devices
            .get(&snap, &key)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn list_devices(&self, user_id: &UserId) -> Result<Vec<DeviceRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let prefix =
            TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(user_id.to_string(),));
        let mut devices: Vec<DeviceRecord> = self
            .devices
            .range(&snap, prefix)
            .map(|item| {
                let (_k, v) = item.map_err(|e| StoreError::Backend(e.to_string()))?;
                decode(&v)
            })
            .collect::<Result<_, StoreError>>()?;
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        Ok(devices)
    }

    async fn set_display_name(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        display_name: Option<String>,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), device_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.devices.get(txn, &key).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(device_id.to_string())));
            };
            let mut record: DeviceRecord = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            record.display_name = display_name.clone();
            let value = serde_json::to_vec(&record)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.devices.put(txn, &key, &value).map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn delete_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), device_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.devices.delete(txn, &key).map_err(to_kv)
        })
        .map_err(store_err)
    }

    async fn record_seen(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        seen_at_ms: u64,
        ip: Option<String>,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), device_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.devices.get(txn, &key).map_err(to_kv)? else {
                return Ok(());
            };
            let mut record: DeviceRecord = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            record.last_seen_ms = Some(seen_at_ms);
            if ip.is_some() {
                record.last_seen_ip = ip.clone();
            }
            let value = serde_json::to_vec(&record)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.devices.put(txn, &key, &value).map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> TokenStore for TablesAuthStore<B> {
    async fn put_access_token(&self, record: AccessTokenRecord) -> Result<(), StoreError> {
        let key = (record.hash.to_hex(),);
        let value = encode(&record)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let old = self.access_tokens.get(txn, &key).map_err(to_kv)?;
            self.access_tokens.put(txn, &key, &value).map_err(to_kv)?;
            maintain_index(
                txn,
                &self.access_tokens_by_user,
                &key,
                old.as_deref(),
                Some(&value),
            )
            .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn get_access_token(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<AccessTokenRecord>, StoreError> {
        let snap = self.backend.snapshot();
        match self
            .access_tokens
            .get(&snap, &(hash.to_hex(),))
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn delete_access_token(&self, hash: &TokenHash) -> Result<(), StoreError> {
        let key = (hash.to_hex(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let old = self.access_tokens.get(txn, &key).map_err(to_kv)?;
            self.access_tokens.delete(txn, &key).map_err(to_kv)?;
            maintain_index(txn, &self.access_tokens_by_user, &key, old.as_deref(), None)
                .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn delete_all_access_tokens_for_user(
        &self,
        user_id: &UserId,
    ) -> Result<usize, StoreError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let pks =
                lookup(txn, &self.access_tokens_by_user, &(user_id.to_string(),)).map_err(to_kv)?;
            let mut removed = 0usize;
            for pk in pks {
                let old = self.access_tokens.get(txn, &pk).map_err(to_kv)?;
                self.access_tokens.delete(txn, &pk).map_err(to_kv)?;
                maintain_index(txn, &self.access_tokens_by_user, &pk, old.as_deref(), None)
                    .map_err(to_kv)?;
                removed += 1;
            }
            Ok(removed)
        })
        .map_err(store_err)
    }

    async fn delete_other_access_tokens_for_user(
        &self,
        user_id: &UserId,
        except: &TokenHash,
    ) -> Result<usize, StoreError> {
        let except_key = except.to_hex();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let pks =
                lookup(txn, &self.access_tokens_by_user, &(user_id.to_string(),)).map_err(to_kv)?;
            let mut removed = 0usize;
            for pk in pks {
                if pk.0 == except_key {
                    continue;
                }
                let old = self.access_tokens.get(txn, &pk).map_err(to_kv)?;
                self.access_tokens.delete(txn, &pk).map_err(to_kv)?;
                maintain_index(txn, &self.access_tokens_by_user, &pk, old.as_deref(), None)
                    .map_err(to_kv)?;
                removed += 1;
            }
            Ok(removed)
        })
        .map_err(store_err)
    }

    async fn delete_access_tokens_for_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<(), StoreError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let pks =
                lookup(txn, &self.access_tokens_by_user, &(user_id.to_string(),)).map_err(to_kv)?;
            for pk in pks {
                let Some(bytes) = self.access_tokens.get(txn, &pk).map_err(to_kv)? else {
                    continue;
                };
                let record: AccessTokenRecord = serde_json::from_slice(&bytes)
                    .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
                if record.device_id.as_deref() == Some(device_id) {
                    self.access_tokens.delete(txn, &pk).map_err(to_kv)?;
                    maintain_index(txn, &self.access_tokens_by_user, &pk, Some(&bytes), None)
                        .map_err(to_kv)?;
                }
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn mark_access_token_used(
        &self,
        hash: &TokenHash,
        used_at_ms: u64,
    ) -> Result<(), StoreError> {
        let key = (hash.to_hex(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.access_tokens.get(txn, &key).map_err(to_kv)? else {
                return Ok(());
            };
            let mut record: AccessTokenRecord = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            record.last_used_ms = Some(used_at_ms);
            let value = serde_json::to_vec(&record)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.access_tokens.put(txn, &key, &value).map_err(to_kv)?;
            maintain_index(
                txn,
                &self.access_tokens_by_user,
                &key,
                Some(&bytes),
                Some(&value),
            )
            .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn put_refresh_token(&self, record: RefreshTokenRecord) -> Result<(), StoreError> {
        let key = (record.hash.to_hex(),);
        let value = encode(&record)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let old = self.refresh_tokens.get(txn, &key).map_err(to_kv)?;
            self.refresh_tokens.put(txn, &key, &value).map_err(to_kv)?;
            maintain_index(
                txn,
                &self.refresh_tokens_by_user,
                &key,
                old.as_deref(),
                Some(&value),
            )
            .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn get_refresh_token(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<RefreshTokenRecord>, StoreError> {
        let snap = self.backend.snapshot();
        match self
            .refresh_tokens
            .get(&snap, &(hash.to_hex(),))
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn mark_refresh_token_used(
        &self,
        hash: &TokenHash,
        replaced_by: TokenHash,
    ) -> Result<(), StoreError> {
        let key = (hash.to_hex(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.refresh_tokens.get(txn, &key).map_err(to_kv)? else {
                return Err(to_kv(RowMissing("refresh token".to_string())));
            };
            let mut record: RefreshTokenRecord = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            record.used = true;
            record.replaced_by = Some(replaced_by);
            let value = serde_json::to_vec(&record)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.refresh_tokens.put(txn, &key, &value).map_err(to_kv)?;
            maintain_index(
                txn,
                &self.refresh_tokens_by_user,
                &key,
                Some(&bytes),
                Some(&value),
            )
            .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn delete_refresh_token(&self, hash: &TokenHash) -> Result<(), StoreError> {
        let key = (hash.to_hex(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let old = self.refresh_tokens.get(txn, &key).map_err(to_kv)?;
            self.refresh_tokens.delete(txn, &key).map_err(to_kv)?;
            maintain_index(
                txn,
                &self.refresh_tokens_by_user,
                &key,
                old.as_deref(),
                None,
            )
            .map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn delete_all_refresh_tokens_for_user(
        &self,
        user_id: &UserId,
    ) -> Result<usize, StoreError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let pks = lookup(txn, &self.refresh_tokens_by_user, &(user_id.to_string(),))
                .map_err(to_kv)?;
            let mut removed = 0usize;
            for pk in pks {
                let old = self.refresh_tokens.get(txn, &pk).map_err(to_kv)?;
                self.refresh_tokens.delete(txn, &pk).map_err(to_kv)?;
                maintain_index(txn, &self.refresh_tokens_by_user, &pk, old.as_deref(), None)
                    .map_err(to_kv)?;
                removed += 1;
            }
            Ok(removed)
        })
        .map_err(store_err)
    }

    async fn put_login_token(&self, record: LoginTokenRecord) -> Result<(), StoreError> {
        let key = (record.hash.to_hex(),);
        let value = encode(&record)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.login_tokens.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(store_err)
    }

    async fn consume_login_token(
        &self,
        hash: &TokenHash,
        now_ms: u64,
    ) -> Result<Option<LoginTokenRecord>, StoreError> {
        let key = (hash.to_hex(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.login_tokens.get(txn, &key).map_err(to_kv)? else {
                return Ok(None);
            };
            let mut record: LoginTokenRecord = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            if record.used || record.expires_at_ms < now_ms {
                return Ok(None);
            }
            record.used = true;
            let value = serde_json::to_vec(&record)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.login_tokens.put(txn, &key, &value).map_err(to_kv)?;
            Ok(Some(record))
        })
        .map_err(store_err)
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> UiaStore for TablesAuthStore<B> {
    async fn create_session(&self, created_at_ms: u64) -> Result<String, StoreError> {
        let id: String = std::iter::repeat_with(|| rand::rng().sample(Alphanumeric) as char)
            .take(24)
            .collect();
        let row = UiaSessionRow {
            created_at_ms,
            completed: Vec::new(),
            data: HashMap::new(),
        };
        let value = encode(&row)?;
        let key = (id.clone(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.uia_sessions.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(store_err)?;
        Ok(id)
    }

    async fn mark_stage_complete(&self, session_id: &str, stage: &str) -> Result<(), StoreError> {
        let key = (session_id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.uia_sessions.get(txn, &key).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("UIA session {session_id}"))));
            };
            let mut row: UiaSessionRow = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            if !row.completed.iter().any(|s| s == stage) {
                row.completed.push(stage.to_string());
            }
            let value = serde_json::to_vec(&row)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.uia_sessions.put(txn, &key, &value).map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn completed_stages(&self, session_id: &str) -> Result<Vec<String>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (session_id.to_string(),);
        let Some(bytes) = self
            .uia_sessions
            .get(&snap, &key)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        else {
            return Err(StoreError::NotFound(format!("UIA session {session_id}")));
        };
        let row: UiaSessionRow = decode(&bytes)?;
        Ok(row.completed)
    }

    async fn set_session_data(
        &self,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), StoreError> {
        let pk = (session_id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.uia_sessions.get(txn, &pk).map_err(to_kv)? else {
                return Err(to_kv(RowMissing(format!("UIA session {session_id}"))));
            };
            let mut row: UiaSessionRow = serde_json::from_slice(&bytes)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            row.data.insert(key.to_string(), value.clone());
            let new_value = serde_json::to_vec(&row)
                .map_err(|e| KvError::backend(DecodeFail(e.to_string())))?;
            self.uia_sessions.put(txn, &pk, &new_value).map_err(to_kv)?;
            Ok(())
        })
        .map_err(store_err)
    }

    async fn get_session_data(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let snap = self.backend.snapshot();
        let pk = (session_id.to_string(),);
        let Some(bytes) = self
            .uia_sessions
            .get(&snap, &pk)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        else {
            return Err(StoreError::NotFound(format!("UIA session {session_id}")));
        };
        let row: UiaSessionRow = decode(&bytes)?;
        Ok(row.data.get(key).cloned())
    }

    async fn session_exists(
        &self,
        session_id: &str,
        now_ms: u64,
        timeout_ms: u64,
    ) -> Result<bool, StoreError> {
        let snap = self.backend.snapshot();
        let key = (session_id.to_string(),);
        let Some(bytes) = self
            .uia_sessions
            .get(&snap, &key)
            .map_err(|e| StoreError::Backend(e.to_string()))?
        else {
            return Ok(false);
        };
        let row: UiaSessionRow = decode(&bytes)?;
        Ok(now_ms.saturating_sub(row.created_at_ms) < timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn store() -> TablesAuthStore<MemoryBackend> {
        TablesAuthStore::open(MemoryBackend::new()).unwrap()
    }

    /// The entire behavioral test suite in `crate::store::shared_tests`, run against
    /// `TablesAuthStore<MemoryBackend>` — the same suite `memory::tests` runs against
    /// `InMemoryAuthStore`. A behavioral difference between the two implementations shows up as a
    /// failure here, not as a bug discovered later by a route handler.
    #[tokio::test]
    async fn shared_behavior_suite() {
        crate::store::shared_tests::run_all(store).await;
    }

    // The tests below check things specific to this implementation: index bookkeeping and
    // cross-open persistence, which an implementation shared with the in-memory store cannot
    // exercise.

    #[tokio::test]
    async fn open_is_idempotent_and_reopening_shares_state() {
        let backend = MemoryBackend::new();
        let a = TablesAuthStore::open(backend.clone()).unwrap();
        let uid = ruma::user_id!("@alice:example.org").to_owned();
        a.create_user(UserRecord::new(uid.clone(), 1))
            .await
            .unwrap();

        // A second store opened over the *same* backend sees what the first wrote — this is the
        // property that makes `TablesAuthStore` meaningfully different from
        // `InMemoryAuthStore`: state lives in the backend, not in the store object.
        let b = TablesAuthStore::open(backend).unwrap();
        assert!(b.get_user(&uid).await.unwrap().is_some());
    }

    /// The claim this whole module exists to make good on: a user, a device and an access token
    /// written through a `FjallBackend` rooted at a temporary directory are still there — and the
    /// user can still be looked up and authenticated — after the store (and the backend handle
    /// underneath it) is dropped and reopened from that same directory, simulating a restart of
    /// `hs serve`. `InMemoryAuthStore` cannot make this claim at all; this is the test that
    /// justifies this module's existence.
    #[tokio::test]
    async fn fjall_backed_store_survives_reopen_from_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let uid = ruma::user_id!("@durable:example.org").to_owned();
        let did: ruma::OwnedDeviceId = "DUR1".into();
        let hash = TokenHash::of("syt_durable_test_token");

        {
            let backend = hs_kv::fjall_backend::FjallBackend::open(dir.path()).unwrap();
            let store = TablesAuthStore::open(backend).unwrap();
            store
                .create_user(UserRecord::new(uid.clone(), 1))
                .await
                .unwrap();
            store
                .set_password_hash(&uid, Some("argon2-hash-stand-in".to_string()))
                .await
                .unwrap();
            store
                .upsert_device(DeviceRecord {
                    user_id: uid.clone(),
                    device_id: did.clone(),
                    display_name: Some("durable device".to_string()),
                    last_seen_ms: None,
                    last_seen_ip: None,
                })
                .await
                .unwrap();
            store
                .put_access_token(AccessTokenRecord {
                    hash,
                    user_id: uid.clone(),
                    device_id: Some(did.clone()),
                    expires_at_ms: None,
                    refresh_token_hash: None,
                    last_used_ms: None,
                })
                .await
                .unwrap();
            // `store` and its `FjallBackend` handle are dropped here, at the end of this block —
            // nothing keeps the database open past this point, matching a process exit.
        }

        // Reopen from the same directory with a brand new backend handle and store object: this
        // is what `hs serve` restarting looks like.
        let backend = hs_kv::fjall_backend::FjallBackend::open(dir.path()).unwrap();
        let store = TablesAuthStore::open(backend).unwrap();

        let user = store
            .get_user(&uid)
            .await
            .unwrap()
            .expect("user survives a restart");
        assert_eq!(
            user.password_hash.as_deref(),
            Some("argon2-hash-stand-in"),
            "password hash survives a restart"
        );

        let devices = store.list_devices(&uid).await.unwrap();
        assert_eq!(devices.len(), 1, "device survives a restart");
        assert_eq!(devices[0].device_id, did);

        let token = store
            .get_access_token(&hash)
            .await
            .unwrap()
            .expect("access token survives a restart — this is the login credential itself");
        assert_eq!(token.user_id, uid);
        assert_eq!(token.device_id.as_deref(), Some(did.as_ref()));

        // The is_localpart_available/index path also has to survive: a fresh conflict check
        // against the reopened store must still see the durable user.
        assert!(!store.is_localpart_available("durable").await.unwrap());
    }

    #[tokio::test]
    async fn deleting_an_access_token_leaves_no_index_row() {
        let s = store();
        let uid = ruma::user_id!("@bob:example.org").to_owned();
        let hash = TokenHash::of("syt_one");
        s.put_access_token(AccessTokenRecord {
            hash,
            user_id: uid.clone(),
            device_id: None,
            expires_at_ms: None,
            refresh_token_hash: None,
            last_used_ms: None,
        })
        .await
        .unwrap();
        s.delete_access_token(&hash).await.unwrap();

        // If the index row were orphaned, `delete_all_access_tokens_for_user` (which is driven
        // entirely by the index) would still find and try to delete a nonexistent row, but the
        // count it reports would be wrong — assert it reports zero.
        let removed = s.delete_all_access_tokens_for_user(&uid).await.unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn overwriting_a_token_with_a_different_owner_moves_the_index_entry() {
        let s = store();
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        let bob = ruma::user_id!("@bob:example.org").to_owned();
        let hash = TokenHash::of("syt_shared_hash_for_test");
        s.put_access_token(AccessTokenRecord {
            hash,
            user_id: alice.clone(),
            device_id: None,
            expires_at_ms: None,
            refresh_token_hash: None,
            last_used_ms: None,
        })
        .await
        .unwrap();
        // Re-put the same primary key (same hash) but a different owner — `maintain_index` must
        // move the index entry from alice to bob, not leave a stale alice entry behind.
        s.put_access_token(AccessTokenRecord {
            hash,
            user_id: bob.clone(),
            device_id: None,
            expires_at_ms: None,
            refresh_token_hash: None,
            last_used_ms: None,
        })
        .await
        .unwrap();

        assert_eq!(
            s.delete_all_access_tokens_for_user(&alice).await.unwrap(),
            0
        );
        assert_eq!(s.delete_all_access_tokens_for_user(&bob).await.unwrap(), 1);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// Property: after any sequence of access-token puts/deletes, the number of index entries
        /// reachable for a user (via `lookup`) always equals the number of rows that actually
        /// belong to that user in the primary table — i.e. no orphaned index row, and no missing
        /// one. This is the executable version of this module's "no divergence" claim, in the
        /// style of `crates/hs-tables/tests/index_proptest.rs`.
        #[test]
        fn access_token_index_never_diverges_from_primary_rows(
            ops in proptest::collection::vec(0u8..3, 1..40),
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let s = store();
                let user_a = ruma::user_id!("@a:example.org").to_owned();
                let user_b = ruma::user_id!("@b:example.org").to_owned();
                let hashes: Vec<TokenHash> = (0..4)
                    .map(|i| TokenHash::of(&format!("syt_fixed_{i}")))
                    .collect();

                for (i, op) in ops.iter().enumerate() {
                    let hash = hashes[i % hashes.len()];
                    match op {
                        0 => {
                            s.put_access_token(AccessTokenRecord {
                                hash,
                                user_id: user_a.clone(),
                                device_id: None,
                                expires_at_ms: None,
                                refresh_token_hash: None,
                                last_used_ms: None,
                            })
                            .await
                            .unwrap();
                        }
                        1 => {
                            s.put_access_token(AccessTokenRecord {
                                hash,
                                user_id: user_b.clone(),
                                device_id: None,
                                expires_at_ms: None,
                                refresh_token_hash: None,
                                last_used_ms: None,
                            })
                            .await
                            .unwrap();
                        }
                        _ => {
                            s.delete_access_token(&hash).await.unwrap();
                        }
                    }

                    // Cross-check every hash: whichever store (primary table) says owns it must be
                    // exactly the same answer the index gives for that user's `lookup`, and no
                    // other user's lookup includes it.
                    for h in &hashes {
                        let owner = s.get_access_token(h).await.unwrap().map(|r| r.user_id);
                        for user in [&user_a, &user_b] {
                            let snap = s.backend.snapshot();
                            let pks = hs_tables::index::lookup(
                                &snap,
                                &s.access_tokens_by_user,
                                &(user.to_string(),),
                            )
                            .unwrap();
                            let indexed = pks.iter().any(|(pk,)| *pk == h.to_hex());
                            let should_be_indexed = owner.as_ref() == Some(user);
                            assert_eq!(
                                indexed, should_be_indexed,
                                "index entry for {user} owning token {h} should be {should_be_indexed} but was {indexed}"
                            );
                        }
                    }
                }
            });
        }
    }
}
