//! Registration tokens: what lets a person register while open registration is off, and what an
//! invite link carries.
//!
//! A token has an optional limit on the accounts it may create (`uses_allowed`) and an optional
//! expiry. `completed` counts the accounts created with it; `pending` counts registrations that
//! have presented it (the `m.login.registration_token` stage) and not finished. Both count
//! against the limit, so a token for one person cannot be presented by two people at once and
//! both succeed. A registration that is abandoned stops counting once its user-interactive-auth
//! session would have expired: `pending` is kept per session with the time it was presented,
//! rather than as a bare counter that an abandoned registration would leave incremented forever.
//!
//! [`RegistrationTokenStore`] is the storage seam: [`InMemoryRegistrationTokens`] for tests and
//! [`TablesRegistrationTokens`] (one `hs_auth.registration_tokens` keyspace) for a real server.
//! [`AdminRegistrationTokens`] serves the admin API's `registration_tokens.*` operations from the
//! same store `/register` uses.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use hs_admin::registration_tokens::{
    AdminRegistrationToken, NewRegistrationToken, RegistrationTokenPatch, RegistrationTokenSource,
    token_is_usable,
};
use hs_admin::sources::SourceError;
use hs_kv::{KvBackend, KvError, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::store::StoreError;

/// One stored registration token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationTokenRecord {
    /// The token string.
    pub token: String,
    /// How many accounts it may create; `None` for no limit.
    pub uses_allowed: Option<u64>,
    /// Accounts created with it.
    pub completed: u64,
    /// When it stops working, milliseconds since the Unix epoch; `None` for never.
    pub expires_at_ms: Option<i64>,
    /// When it was created, milliseconds since the Unix epoch.
    pub created_at_ms: i64,
    /// Registrations under way with it: user-interactive-auth session id to the time, in
    /// milliseconds since the Unix epoch, the session presented the token.
    #[serde(default)]
    pub pending: BTreeMap<String, u64>,
}

impl RegistrationTokenRecord {
    /// A fresh token with no uses.
    #[must_use]
    pub fn new(
        token: String,
        uses_allowed: Option<u64>,
        expires_at_ms: Option<i64>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            token,
            uses_allowed,
            completed: 0,
            expires_at_ms,
            created_at_ms,
            pending: BTreeMap::new(),
        }
    }

    /// Registrations under way whose session has not expired at `now_ms`.
    #[must_use]
    pub fn live_pending(&self, now_ms: u64, session_timeout_ms: u64) -> u64 {
        self.pending
            .values()
            .filter(|&&at| now_ms.saturating_sub(at) < session_timeout_ms)
            .count() as u64
    }

    /// Whether the token admits a new registration at `now_ms`.
    #[must_use]
    pub fn usable(&self, now_ms: u64, session_timeout_ms: u64) -> bool {
        token_is_usable(
            self.uses_allowed,
            self.live_pending(now_ms, session_timeout_ms),
            self.completed,
            self.expires_at_ms,
            i64::try_from(now_ms).unwrap_or(i64::MAX),
        )
    }

    /// Records `session_id` as presenting the token, if the token admits it. A session that
    /// already presented it keeps its place (a client may repeat a stage); expired sessions are
    /// dropped on the way. Returns whether the session now holds a place.
    fn reserve(&mut self, session_id: &str, now_ms: u64, session_timeout_ms: u64) -> bool {
        self.pending
            .retain(|_, at| now_ms.saturating_sub(*at) < session_timeout_ms);
        if self.pending.contains_key(session_id) {
            let unexpired = self
                .expires_at_ms
                .is_none_or(|at| at > i64::try_from(now_ms).unwrap_or(i64::MAX));
            return unexpired;
        }
        if !self.usable(now_ms, session_timeout_ms) {
            return false;
        }
        self.pending.insert(session_id.to_owned(), now_ms);
        true
    }

    /// The registration `session_id` presented the token for has created its account.
    fn complete(&mut self, session_id: &str) {
        self.pending.remove(session_id);
        self.completed = self.completed.saturating_add(1);
    }

    fn apply(&mut self, limits: &TokenLimits) {
        if let Some(uses) = limits.uses_allowed {
            self.uses_allowed = uses;
        }
        if let Some(at) = limits.expires_at_ms {
            self.expires_at_ms = at;
        }
    }
}

/// A change to a token's limits: the outer `Option` says whether to change the field, the inner
/// one is the new value (`None` removes the limit).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenLimits {
    /// A new `uses_allowed`, if it changes.
    pub uses_allowed: Option<Option<u64>>,
    /// A new expiry, if it changes.
    pub expires_at_ms: Option<Option<i64>>,
}

/// Where registration tokens are kept.
#[async_trait::async_trait]
pub trait RegistrationTokenStore: Send + Sync {
    /// Stores a new token. [`StoreError::Conflict`] if one with the same string exists.
    async fn create(&self, record: RegistrationTokenRecord) -> Result<(), StoreError>;
    /// One token.
    async fn get(&self, token: &str) -> Result<Option<RegistrationTokenRecord>, StoreError>;
    /// Every token, in token order.
    async fn list(&self) -> Result<Vec<RegistrationTokenRecord>, StoreError>;
    /// Changes a token's limits. [`StoreError::NotFound`] if there is no such token.
    async fn update(
        &self,
        token: &str,
        limits: TokenLimits,
    ) -> Result<RegistrationTokenRecord, StoreError>;
    /// Deletes a token. [`StoreError::NotFound`] if there is no such token.
    async fn delete(&self, token: &str) -> Result<(), StoreError>;
    /// Atomically checks the token admits `session_id` and, if it does, counts the session as
    /// pending. `false` for a token that does not exist, has expired or is used up.
    async fn reserve(
        &self,
        token: &str,
        session_id: &str,
        now_ms: u64,
        session_timeout_ms: u64,
    ) -> Result<bool, StoreError>;
    /// The registration `session_id` reserved a place for has created its account: moves it
    /// from pending to completed. A token deleted in between is not an error (the account
    /// exists either way); returns whether the token was still there.
    async fn complete(&self, token: &str, session_id: &str) -> Result<bool, StoreError>;
}

/// A [`RegistrationTokenStore`] in memory, for tests and [`crate::state::AuthState::in_memory`].
#[derive(Default)]
pub struct InMemoryRegistrationTokens {
    rows: Mutex<BTreeMap<String, RegistrationTokenRecord>>,
}

impl InMemoryRegistrationTokens {
    /// No tokens.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, RegistrationTokenRecord>> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl RegistrationTokenStore for InMemoryRegistrationTokens {
    async fn create(&self, record: RegistrationTokenRecord) -> Result<(), StoreError> {
        let mut rows = self.rows();
        if rows.contains_key(&record.token) {
            return Err(StoreError::Conflict(format!(
                "registration token {}",
                record.token
            )));
        }
        rows.insert(record.token.clone(), record);
        Ok(())
    }

    async fn get(&self, token: &str) -> Result<Option<RegistrationTokenRecord>, StoreError> {
        Ok(self.rows().get(token).cloned())
    }

    async fn list(&self) -> Result<Vec<RegistrationTokenRecord>, StoreError> {
        Ok(self.rows().values().cloned().collect())
    }

    async fn update(
        &self,
        token: &str,
        limits: TokenLimits,
    ) -> Result<RegistrationTokenRecord, StoreError> {
        let mut rows = self.rows();
        let row = rows
            .get_mut(token)
            .ok_or_else(|| StoreError::NotFound(format!("registration token {token}")))?;
        row.apply(&limits);
        Ok(row.clone())
    }

    async fn delete(&self, token: &str) -> Result<(), StoreError> {
        self.rows()
            .remove(token)
            .map(|_| ())
            .ok_or_else(|| StoreError::NotFound(format!("registration token {token}")))
    }

    async fn reserve(
        &self,
        token: &str,
        session_id: &str,
        now_ms: u64,
        session_timeout_ms: u64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .rows()
            .get_mut(token)
            .is_some_and(|row| row.reserve(session_id, now_ms, session_timeout_ms)))
    }

    async fn complete(&self, token: &str, session_id: &str) -> Result<bool, StoreError> {
        Ok(self
            .rows()
            .get_mut(token)
            .map(|row| row.complete(session_id))
            .is_some())
    }
}

/// A durable [`RegistrationTokenStore`]: one JSON row per token in the `hs_auth.registration_tokens`
/// keyspace, keyed by the token. Every change is a read-modify-write inside one `hs-kv`
/// transaction, so two registrations racing for a token's last use cannot both get it.
pub struct TablesRegistrationTokens<B: KvBackend> {
    backend: B,
    tokens: TypedKeyspace<B::Keyspace, (String,)>,
}

impl<B: KvBackend> TablesRegistrationTokens<B> {
    /// Opens (creating if necessary) the keyspace.
    ///
    /// # Errors
    /// [`StoreError::Backend`] if the keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let keyspace = backend
            .keyspace("hs_auth.registration_tokens")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(Self {
            backend,
            tokens: TypedKeyspace::new(keyspace),
        })
    }

    /// Reads, changes and writes one row in one transaction. `f` returns what the caller wants
    /// back, or `None` to leave the row as it was.
    fn modify<T>(
        &self,
        token: &str,
        f: impl Fn(&mut RegistrationTokenRecord) -> T,
    ) -> Result<Option<T>, StoreError> {
        let key = (token.to_owned(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.tokens.get(txn, &key).map_err(KvError::backend)? else {
                return Ok(None);
            };
            let mut row: RegistrationTokenRecord =
                serde_json::from_slice(&bytes).map_err(KvError::backend)?;
            let out = f(&mut row);
            let value = serde_json::to_vec(&row).map_err(KvError::backend)?;
            self.tokens
                .put(txn, &key, &value)
                .map_err(KvError::backend)?;
            Ok(Some(out))
        })
        .map_err(|e| StoreError::Backend(e.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("registration token {0} already exists")]
struct Exists(String);

#[async_trait::async_trait]
impl<B: KvBackend> RegistrationTokenStore for TablesRegistrationTokens<B> {
    async fn create(&self, record: RegistrationTokenRecord) -> Result<(), StoreError> {
        let key = (record.token.clone(),);
        let value = serde_json::to_vec(&record).map_err(|e| StoreError::Backend(e.to_string()))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            if self
                .tokens
                .get(txn, &key)
                .map_err(KvError::backend)?
                .is_some()
            {
                return Err(KvError::backend(Exists(record.token.clone())));
            }
            self.tokens.put(txn, &key, &value).map_err(KvError::backend)
        })
        .map_err(|e| match &e {
            KvError::Backend(inner) if inner.downcast_ref::<Exists>().is_some() => {
                StoreError::Conflict(format!("registration token {}", record.token))
            }
            _ => StoreError::Backend(e.to_string()),
        })
    }

    async fn get(&self, token: &str) -> Result<Option<RegistrationTokenRecord>, StoreError> {
        let snap = self.backend.snapshot();
        match self
            .tokens
            .get(&snap, &(token.to_owned(),))
            .map_err(|e| StoreError::Backend(e.to_string()))?
        {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| StoreError::Backend(e.to_string())),
            None => Ok(None),
        }
    }

    async fn list(&self) -> Result<Vec<RegistrationTokenRecord>, StoreError> {
        let snap = self.backend.snapshot();
        self.tokens
            .range(&snap, RangeSpec::full())
            .map(|item| {
                let (_key, bytes) = item.map_err(|e| StoreError::Backend(e.to_string()))?;
                serde_json::from_slice(&bytes).map_err(|e| StoreError::Backend(e.to_string()))
            })
            .collect()
    }

    async fn update(
        &self,
        token: &str,
        limits: TokenLimits,
    ) -> Result<RegistrationTokenRecord, StoreError> {
        self.modify(token, |row| {
            row.apply(&limits);
            row.clone()
        })?
        .ok_or_else(|| StoreError::NotFound(format!("registration token {token}")))
    }

    async fn delete(&self, token: &str) -> Result<(), StoreError> {
        let key = (token.to_owned(),);
        let existed = transact(&self.backend, TransactConfig::default(), |txn| {
            if self
                .tokens
                .get(txn, &key)
                .map_err(KvError::backend)?
                .is_none()
            {
                return Ok(false);
            }
            self.tokens.delete(txn, &key).map_err(KvError::backend)?;
            Ok(true)
        })
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        if existed {
            Ok(())
        } else {
            Err(StoreError::NotFound(format!("registration token {token}")))
        }
    }

    async fn reserve(
        &self,
        token: &str,
        session_id: &str,
        now_ms: u64,
        session_timeout_ms: u64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .modify(token, |row| {
                row.reserve(session_id, now_ms, session_timeout_ms)
            })?
            .unwrap_or(false))
    }

    async fn complete(&self, token: &str, session_id: &str) -> Result<bool, StoreError> {
        Ok(self
            .modify(token, |row| row.complete(session_id))?
            .is_some())
    }
}

/// The admin API's `registration_tokens.*` operations served from a [`RegistrationTokenStore`]:
/// the store `/register` checks, so a token made here is one a person can register with.
pub struct AdminRegistrationTokens {
    store: Arc<dyn RegistrationTokenStore>,
    clock: Arc<dyn Clock>,
    session_timeout_ms: u64,
}

impl AdminRegistrationTokens {
    /// Serves the tokens of `state`'s store, counting a pending registration for as long as its
    /// user-interactive-auth session lives.
    #[must_use]
    pub fn from_auth_state(state: &crate::state::AuthState) -> Self {
        Self {
            store: state.registration_tokens.clone(),
            clock: state.clock.clone(),
            session_timeout_ms: state.config.get().uia_session_timeout_ms,
        }
    }

    fn view(&self, row: &RegistrationTokenRecord) -> AdminRegistrationToken {
        let now = self.clock.now_ms();
        AdminRegistrationToken {
            token: row.token.clone(),
            valid: row.usable(now, self.session_timeout_ms),
            uses_allowed: row.uses_allowed,
            pending: row.live_pending(now, self.session_timeout_ms),
            completed: row.completed,
            expires_at: row.expires_at_ms.map(hs_http::time::rfc3339_from_millis),
            created_at: hs_http::time::rfc3339_from_millis(row.created_at_ms),
        }
    }
}

fn source_error(e: StoreError) -> SourceError {
    match e {
        StoreError::NotFound(_) => SourceError::NotFound,
        StoreError::Conflict(detail) => SourceError::Conflict(format!("{detail} already exists")),
        StoreError::Backend(detail) => SourceError::Unavailable(detail),
    }
}

#[async_trait::async_trait]
impl RegistrationTokenSource for AdminRegistrationTokens {
    async fn list(&self) -> Result<Vec<AdminRegistrationToken>, SourceError> {
        let mut rows = self.store.list().await.map_err(source_error)?;
        rows.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then(a.token.cmp(&b.token))
        });
        Ok(rows.iter().map(|r| self.view(r)).collect())
    }

    async fn get(&self, token: &str) -> Result<Option<AdminRegistrationToken>, SourceError> {
        Ok(self
            .store
            .get(token)
            .await
            .map_err(source_error)?
            .map(|r| self.view(&r)))
    }

    async fn create(
        &self,
        token: NewRegistrationToken,
    ) -> Result<AdminRegistrationToken, SourceError> {
        let record = RegistrationTokenRecord::new(
            token.token,
            token.uses_allowed,
            token.expires_at_ms,
            i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX),
        );
        self.store
            .create(record.clone())
            .await
            .map_err(source_error)?;
        Ok(self.view(&record))
    }

    async fn update(
        &self,
        token: &str,
        patch: RegistrationTokenPatch,
    ) -> Result<AdminRegistrationToken, SourceError> {
        let row = self
            .store
            .update(
                token,
                TokenLimits {
                    uses_allowed: patch.uses_allowed,
                    expires_at_ms: patch.expires_at_ms,
                },
            )
            .await
            .map_err(source_error)?;
        Ok(self.view(&row))
    }

    async fn delete(&self, token: &str) -> Result<(), SourceError> {
        self.store.delete(token).await.map_err(source_error)
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;

    use super::*;

    const TIMEOUT: u64 = 1000;

    async fn exercise(store: &dyn RegistrationTokenStore) {
        store
            .create(RegistrationTokenRecord::new("t".into(), Some(2), None, 0))
            .await
            .unwrap();
        assert!(matches!(
            store
                .create(RegistrationTokenRecord::new("t".into(), None, None, 0))
                .await,
            Err(StoreError::Conflict(_))
        ));

        // Two sessions take both places; a third is refused; the first repeating its stage keeps
        // its place rather than taking another.
        assert!(store.reserve("t", "s1", 10, TIMEOUT).await.unwrap());
        assert!(store.reserve("t", "s1", 11, TIMEOUT).await.unwrap());
        assert!(store.reserve("t", "s2", 12, TIMEOUT).await.unwrap());
        assert!(!store.reserve("t", "s3", 13, TIMEOUT).await.unwrap());

        // s1 finishes: one completed, one pending, still full.
        assert!(store.complete("t", "s1").await.unwrap());
        let row = store.get("t").await.unwrap().unwrap();
        assert_eq!(row.completed, 1);
        assert_eq!(row.live_pending(20, TIMEOUT), 1);
        assert!(!store.reserve("t", "s3", 20, TIMEOUT).await.unwrap());

        // s2 abandons: once its session would have expired, its place is free again.
        assert!(
            store
                .reserve("t", "s3", 12 + TIMEOUT, TIMEOUT)
                .await
                .unwrap()
        );

        // Unknown tokens are refused, not errors.
        assert!(!store.reserve("nope", "s", 0, TIMEOUT).await.unwrap());
        assert!(!store.complete("nope", "s").await.unwrap());

        // An expired token admits nobody new, not even with room to spare.
        let row = store
            .update(
                "t",
                TokenLimits {
                    uses_allowed: Some(None),
                    expires_at_ms: Some(Some(5000)),
                },
            )
            .await
            .unwrap();
        assert_eq!(row.uses_allowed, None);
        assert!(!store.reserve("t", "s9", 5000, TIMEOUT).await.unwrap());
        assert!(store.reserve("t", "s9", 4999, TIMEOUT).await.unwrap());

        assert_eq!(store.list().await.unwrap().len(), 1);
        store.delete("t").await.unwrap();
        assert!(matches!(
            store.delete("t").await,
            Err(StoreError::NotFound(_))
        ));
        assert!(store.get("t").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_in_memory_store_counts_uses_and_pending_sessions() {
        exercise(&InMemoryRegistrationTokens::new()).await;
    }

    #[tokio::test]
    async fn the_tables_store_counts_uses_and_pending_sessions() {
        exercise(&TablesRegistrationTokens::open(MemoryBackend::new()).unwrap()).await;
    }

    #[tokio::test]
    async fn the_tables_store_survives_reopening_the_backend() {
        let backend = MemoryBackend::new();
        let store = TablesRegistrationTokens::open(backend.clone()).unwrap();
        store
            .create(RegistrationTokenRecord::new(
                "keep".into(),
                Some(1),
                None,
                7,
            ))
            .await
            .unwrap();
        let reopened = TablesRegistrationTokens::open(backend).unwrap();
        let row = reopened.get("keep").await.unwrap().unwrap();
        assert_eq!(row.created_at_ms, 7);
        assert_eq!(row.uses_allowed, Some(1));
    }
}
