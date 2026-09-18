//! An in-memory implementation of every trait in [`super`], for tests and for running this crate
//! before `hs-tables` (track 01) is ready to back it. Not persistent, not shared across processes.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use rand::Rng;
use rand::distr::Alphanumeric;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};

use super::{
    AccessTokenRecord, DeviceRecord, DeviceStore, LoginTokenRecord, RefreshTokenRecord, StoreError,
    TokenStore, UiaStore, UserRecord, UserStore,
};
use crate::token::TokenHash;

#[derive(Default)]
struct UiaSession {
    created_at_ms: u64,
    completed: Vec<String>,
    data: HashMap<String, serde_json::Value>,
}

#[derive(Default)]
struct Inner {
    users: HashMap<OwnedUserId, UserRecord>,
    devices: HashMap<(OwnedUserId, OwnedDeviceId), DeviceRecord>,
    access_tokens: HashMap<TokenHash, AccessTokenRecord>,
    refresh_tokens: HashMap<TokenHash, RefreshTokenRecord>,
    login_tokens: HashMap<TokenHash, LoginTokenRecord>,
    uia_sessions: HashMap<String, UiaSession>,
    threepids: HashMap<(String, String), OwnedUserId>,
}

/// The in-memory `AuthStore`. Cheap to construct; clone the `Arc` you wrap it in, not this type.
#[derive(Default)]
pub struct InMemoryAuthStore {
    inner: Mutex<Inner>,
}

impl InMemoryAuthStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn localpart_lower(user_id: &UserId) -> String {
    user_id.localpart().to_ascii_lowercase()
}

#[async_trait]
impl UserStore for InMemoryAuthStore {
    async fn get_user(&self, user_id: &UserId) -> Result<Option<UserRecord>, StoreError> {
        Ok(self.lock().users.get(user_id).cloned())
    }

    async fn create_user(&self, record: UserRecord) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if !inner
            .users
            .keys()
            .all(|existing| localpart_lower(existing) != localpart_lower(&record.user_id))
        {
            return Err(StoreError::Conflict(format!(
                "user {} already exists",
                record.user_id
            )));
        }
        inner.users.insert(record.user_id.clone(), record);
        Ok(())
    }

    async fn is_localpart_available(&self, localpart: &str) -> Result<bool, StoreError> {
        let wanted = localpart.to_ascii_lowercase();
        Ok(self
            .lock()
            .users
            .keys()
            .all(|u| u.localpart().to_ascii_lowercase() != wanted))
    }

    async fn set_password_hash(
        &self,
        user_id: &UserId,
        hash: Option<String>,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let user = inner
            .users
            .get_mut(user_id)
            .ok_or_else(|| StoreError::NotFound(user_id.to_string()))?;
        user.password_hash = hash;
        Ok(())
    }

    async fn set_admin(&self, user_id: &UserId, admin: bool) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let user = inner
            .users
            .get_mut(user_id)
            .ok_or_else(|| StoreError::NotFound(user_id.to_string()))?;
        user.is_admin = admin;
        Ok(())
    }

    async fn set_locked(&self, user_id: &UserId, locked: bool) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let user = inner
            .users
            .get_mut(user_id)
            .ok_or_else(|| StoreError::NotFound(user_id.to_string()))?;
        user.locked = locked;
        Ok(())
    }

    async fn set_suspended(&self, user_id: &UserId, suspended: bool) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let user = inner
            .users
            .get_mut(user_id)
            .ok_or_else(|| StoreError::NotFound(user_id.to_string()))?;
        user.suspended = suspended;
        Ok(())
    }

    async fn set_deactivated(&self, user_id: &UserId, deactivated: bool) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let user = inner
            .users
            .get_mut(user_id)
            .ok_or_else(|| StoreError::NotFound(user_id.to_string()))?;
        user.deactivated = deactivated;
        Ok(())
    }

    async fn bind_threepid(
        &self,
        user_id: &UserId,
        medium: &str,
        address: &str,
    ) -> Result<(), StoreError> {
        self.lock().threepids.insert(
            (medium.to_string(), address.to_ascii_lowercase()),
            user_id.to_owned(),
        );
        Ok(())
    }

    async fn get_user_by_threepid(
        &self,
        medium: &str,
        address: &str,
    ) -> Result<Option<OwnedUserId>, StoreError> {
        Ok(self
            .lock()
            .threepids
            .get(&(medium.to_string(), address.to_ascii_lowercase()))
            .cloned())
    }
}

#[async_trait]
impl DeviceStore for InMemoryAuthStore {
    async fn upsert_device(&self, record: DeviceRecord) -> Result<(), StoreError> {
        let key = (record.user_id.clone(), record.device_id.clone());
        self.lock().devices.insert(key, record);
        Ok(())
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceRecord>, StoreError> {
        let key = (user_id.to_owned(), device_id.to_owned());
        Ok(self.lock().devices.get(&key).cloned())
    }

    async fn list_devices(&self, user_id: &UserId) -> Result<Vec<DeviceRecord>, StoreError> {
        let mut devices: Vec<DeviceRecord> = self
            .lock()
            .devices
            .values()
            .filter(|d| d.user_id == user_id)
            .cloned()
            .collect();
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        Ok(devices)
    }

    async fn set_display_name(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        display_name: Option<String>,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_owned(), device_id.to_owned());
        let mut inner = self.lock();
        let device = inner
            .devices
            .get_mut(&key)
            .ok_or_else(|| StoreError::NotFound(device_id.to_string()))?;
        device.display_name = display_name;
        Ok(())
    }

    async fn delete_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_owned(), device_id.to_owned());
        self.lock().devices.remove(&key);
        Ok(())
    }

    async fn record_seen(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        seen_at_ms: u64,
        ip: Option<String>,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_owned(), device_id.to_owned());
        let mut inner = self.lock();
        if let Some(device) = inner.devices.get_mut(&key) {
            device.last_seen_ms = Some(seen_at_ms);
            if ip.is_some() {
                device.last_seen_ip = ip;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl TokenStore for InMemoryAuthStore {
    async fn put_access_token(&self, record: AccessTokenRecord) -> Result<(), StoreError> {
        self.lock().access_tokens.insert(record.hash, record);
        Ok(())
    }

    async fn get_access_token(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<AccessTokenRecord>, StoreError> {
        Ok(self.lock().access_tokens.get(hash).cloned())
    }

    async fn delete_access_token(&self, hash: &TokenHash) -> Result<(), StoreError> {
        self.lock().access_tokens.remove(hash);
        Ok(())
    }

    async fn delete_all_access_tokens_for_user(
        &self,
        user_id: &UserId,
    ) -> Result<usize, StoreError> {
        let mut inner = self.lock();
        let before = inner.access_tokens.len();
        inner.access_tokens.retain(|_, rec| rec.user_id != user_id);
        Ok(before - inner.access_tokens.len())
    }

    async fn delete_other_access_tokens_for_user(
        &self,
        user_id: &UserId,
        except: &TokenHash,
    ) -> Result<usize, StoreError> {
        let mut inner = self.lock();
        let before = inner.access_tokens.len();
        let except = *except;
        inner
            .access_tokens
            .retain(|hash, rec| !(rec.user_id == user_id && *hash != except));
        Ok(before - inner.access_tokens.len())
    }

    async fn delete_access_tokens_for_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        inner.access_tokens.retain(|_, rec| {
            !(rec.user_id == user_id && rec.device_id.as_deref() == Some(device_id))
        });
        Ok(())
    }

    async fn mark_access_token_used(
        &self,
        hash: &TokenHash,
        used_at_ms: u64,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if let Some(rec) = inner.access_tokens.get_mut(hash) {
            rec.last_used_ms = Some(used_at_ms);
        }
        Ok(())
    }

    async fn put_refresh_token(&self, record: RefreshTokenRecord) -> Result<(), StoreError> {
        self.lock().refresh_tokens.insert(record.hash, record);
        Ok(())
    }

    async fn get_refresh_token(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<RefreshTokenRecord>, StoreError> {
        Ok(self.lock().refresh_tokens.get(hash).cloned())
    }

    async fn mark_refresh_token_used(
        &self,
        hash: &TokenHash,
        replaced_by: TokenHash,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let rec = inner
            .refresh_tokens
            .get_mut(hash)
            .ok_or_else(|| StoreError::NotFound("refresh token".to_string()))?;
        rec.used = true;
        rec.replaced_by = Some(replaced_by);
        Ok(())
    }

    async fn delete_refresh_token(&self, hash: &TokenHash) -> Result<(), StoreError> {
        self.lock().refresh_tokens.remove(hash);
        Ok(())
    }

    async fn delete_all_refresh_tokens_for_user(
        &self,
        user_id: &UserId,
    ) -> Result<usize, StoreError> {
        let mut inner = self.lock();
        let before = inner.refresh_tokens.len();
        inner.refresh_tokens.retain(|_, rec| rec.user_id != user_id);
        Ok(before - inner.refresh_tokens.len())
    }

    async fn put_login_token(&self, record: LoginTokenRecord) -> Result<(), StoreError> {
        self.lock().login_tokens.insert(record.hash, record);
        Ok(())
    }

    async fn consume_login_token(
        &self,
        hash: &TokenHash,
        now_ms: u64,
    ) -> Result<Option<LoginTokenRecord>, StoreError> {
        let mut inner = self.lock();
        let Some(rec) = inner.login_tokens.get_mut(hash) else {
            return Ok(None);
        };
        if rec.used || rec.expires_at_ms < now_ms {
            return Ok(None);
        }
        rec.used = true;
        Ok(Some(rec.clone()))
    }
}

#[async_trait]
impl UiaStore for InMemoryAuthStore {
    async fn create_session(&self, created_at_ms: u64) -> Result<String, StoreError> {
        let id: String = std::iter::repeat_with(|| rand::rng().sample(Alphanumeric) as char)
            .take(24)
            .collect();
        self.lock().uia_sessions.insert(
            id.clone(),
            UiaSession {
                created_at_ms,
                completed: Vec::new(),
                data: HashMap::new(),
            },
        );
        Ok(id)
    }

    async fn mark_stage_complete(&self, session_id: &str, stage: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let session = inner
            .uia_sessions
            .get_mut(session_id)
            .ok_or_else(|| StoreError::NotFound(format!("UIA session {session_id}")))?;
        if !session.completed.iter().any(|s| s == stage) {
            session.completed.push(stage.to_string());
        }
        Ok(())
    }

    async fn completed_stages(&self, session_id: &str) -> Result<Vec<String>, StoreError> {
        let inner = self.lock();
        let session = inner
            .uia_sessions
            .get(session_id)
            .ok_or_else(|| StoreError::NotFound(format!("UIA session {session_id}")))?;
        Ok(session.completed.clone())
    }

    async fn set_session_data(
        &self,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let session = inner
            .uia_sessions
            .get_mut(session_id)
            .ok_or_else(|| StoreError::NotFound(format!("UIA session {session_id}")))?;
        session.data.insert(key.to_string(), value);
        Ok(())
    }

    async fn get_session_data(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let inner = self.lock();
        let session = inner
            .uia_sessions
            .get(session_id)
            .ok_or_else(|| StoreError::NotFound(format!("UIA session {session_id}")))?;
        Ok(session.data.get(key).cloned())
    }

    async fn session_exists(
        &self,
        session_id: &str,
        now_ms: u64,
        timeout_ms: u64,
    ) -> Result<bool, StoreError> {
        let inner = self.lock();
        Ok(inner
            .uia_sessions
            .get(session_id)
            .is_some_and(|s| now_ms.saturating_sub(s.created_at_ms) < timeout_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AccessTokenRecord;
    use ruma::{device_id, user_id};

    fn store() -> InMemoryAuthStore {
        InMemoryAuthStore::new()
    }

    #[tokio::test]
    async fn create_user_then_get_round_trips() {
        let s = store();
        let uid = user_id!("@alice:example.org").to_owned();
        s.create_user(UserRecord::new(uid.clone(), 1000))
            .await
            .unwrap();
        let got = s.get_user(&uid).await.unwrap().unwrap();
        assert_eq!(got.user_id, uid);
        assert!(!got.is_admin);
    }

    #[tokio::test]
    async fn create_user_conflict_is_case_insensitive() {
        let s = store();
        s.create_user(UserRecord::new(
            user_id!("@Alice:example.org").to_owned(),
            1,
        ))
        .await
        .unwrap();
        let err = s
            .create_user(UserRecord::new(
                user_id!("@alice:example.org").to_owned(),
                2,
            ))
            .await;
        assert!(matches!(err, Err(StoreError::Conflict(_))));
    }

    #[tokio::test]
    async fn is_localpart_available_reflects_existing_users() {
        let s = store();
        assert!(s.is_localpart_available("alice").await.unwrap());
        s.create_user(UserRecord::new(
            user_id!("@alice:example.org").to_owned(),
            1,
        ))
        .await
        .unwrap();
        assert!(!s.is_localpart_available("alice").await.unwrap());
        assert!(!s.is_localpart_available("ALICE").await.unwrap());
    }

    #[tokio::test]
    async fn device_and_token_lifecycle() {
        let s = store();
        let uid = user_id!("@bob:example.org").to_owned();
        let did = device_id!("DEV1").to_owned();
        s.upsert_device(DeviceRecord {
            user_id: uid.clone(),
            device_id: did.clone(),
            display_name: Some("phone".to_string()),
            last_seen_ms: None,
            last_seen_ip: None,
        })
        .await
        .unwrap();
        assert_eq!(s.list_devices(&uid).await.unwrap().len(), 1);

        let hash = TokenHash::of("syt_whatever");
        s.put_access_token(AccessTokenRecord {
            hash,
            user_id: uid.clone(),
            device_id: Some(did.clone()),
            expires_at_ms: None,
            refresh_token_hash: None,
            last_used_ms: None,
        })
        .await
        .unwrap();
        assert!(s.get_access_token(&hash).await.unwrap().is_some());

        s.delete_access_tokens_for_device(&uid, &did).await.unwrap();
        assert!(s.get_access_token(&hash).await.unwrap().is_none());

        s.delete_device(&uid, &did).await.unwrap();
        assert!(s.list_devices(&uid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn login_token_is_single_use() {
        let s = store();
        let uid = user_id!("@carol:example.org").to_owned();
        let hash = TokenHash::of("syl_whatever");
        s.put_login_token(LoginTokenRecord {
            hash,
            user_id: uid,
            expires_at_ms: 10_000,
            used: false,
        })
        .await
        .unwrap();
        let first = s.consume_login_token(&hash, 1_000).await.unwrap();
        assert!(first.is_some());
        let second = s.consume_login_token(&hash, 1_000).await.unwrap();
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn login_token_expiry_is_enforced() {
        let s = store();
        let uid = user_id!("@dave:example.org").to_owned();
        let hash = TokenHash::of("syl_whatever2");
        s.put_login_token(LoginTokenRecord {
            hash,
            user_id: uid,
            expires_at_ms: 1_000,
            used: false,
        })
        .await
        .unwrap();
        let result = s.consume_login_token(&hash, 5_000).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn threepid_lookup_is_case_insensitive_on_address() {
        let s = store();
        let uid = user_id!("@eve:example.org").to_owned();
        s.bind_threepid(&uid, "email", "Eve@Example.Org")
            .await
            .unwrap();
        assert_eq!(
            s.get_user_by_threepid("email", "eve@example.org")
                .await
                .unwrap(),
            Some(uid)
        );
        assert!(
            s.get_user_by_threepid("msisdn", "eve@example.org")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn uia_session_tracks_completed_stages_and_data() {
        let s = store();
        let id = s.create_session(0).await.unwrap();
        assert!(s.session_exists(&id, 100, 10_000).await.unwrap());
        assert!(!s.session_exists(&id, 20_000, 10_000).await.unwrap());

        s.mark_stage_complete(&id, "m.login.dummy").await.unwrap();
        s.mark_stage_complete(&id, "m.login.dummy").await.unwrap(); // idempotent
        assert_eq!(
            s.completed_stages(&id).await.unwrap(),
            vec!["m.login.dummy".to_string()]
        );

        s.set_session_data(&id, "username", serde_json::json!("alice"))
            .await
            .unwrap();
        assert_eq!(
            s.get_session_data(&id, "username").await.unwrap(),
            Some(serde_json::json!("alice"))
        );
    }
}
