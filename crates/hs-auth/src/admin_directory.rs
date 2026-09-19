//! [`AuthStoreUserDirectory`]: `hs_admin::sources::UserDirectory` implemented over this crate's
//! own [`crate::store::AuthStore`], the data source `GET /api/v1/users` and friends serve from
//! once `hs-cli` wires it in (see `docs/status/07-auth-and-identity.md`'s "Interfaces provided"
//! for the exact construction line).
//!
//! # What is filled, and what is not
//!
//! This crate's [`crate::store::UserRecord`]/[`crate::store::DeviceRecord`] give
//! [`hs_admin::model::AdminUser`]:
//!
//! - `user_id`, `admin`, `deactivated`, `locked`, `suspended`, `shadow_banned`, `created_at`
//!   directly from [`crate::store::UserRecord`].
//! - `device_count` (the length of [`crate::store::DeviceStore::list_devices`]'s result) and
//!   `last_seen_at` (the max of those devices' `last_seen_ms`, formatted the same way
//!   `created_at` is).
//!
//! Left at [`hs_admin::model::AdminUser::default`]:
//!
//! - `room_count`, `media_count`: this crate has no view onto rooms (track 04) or media (track
//!   09). Wiring those in is those tracks' data-source seam to add, not this one's -- see
//!   `docs/status/07-auth-and-identity.md`'s "Interfaces needed".
//! - `display_name`, `avatar_url`, `user_type`, `consent_version`, `appservice_id`: none of these
//!   exist on `UserRecord` yet (profile data is track 04's territory; user-type categorization
//!   and appservice attribution are Phase 1/2 gaps noted in this crate's own status file).
//! - `erased`: account erasure is a Phase 1/2 lifecycle feature this crate has not built yet (see
//!   this track's brief's "account lifecycle" line).
//!
//! [`hs_admin::sources::UserFilter::q`] is therefore matched against `user_id` only -- there is no
//! display name to search against here yet.

use std::sync::Arc;

use hs_admin::model::AdminUser;
use hs_admin::sources::{SourceError, UserDirectory, UserFilter};

use crate::admin_verifier::format_rfc3339_ms;
use crate::state::AuthState;
use crate::store::{AuthStore, StoreError, UserRecord};

/// The user directory `hs-admin`'s `/users` handlers call, over this crate's own [`AuthStore`].
pub struct AuthStoreUserDirectory {
    store: Arc<dyn AuthStore>,
}

impl AuthStoreUserDirectory {
    /// Builds a directory over an already-open store.
    #[must_use]
    pub fn new(store: Arc<dyn AuthStore>) -> Self {
        Self { store }
    }

    /// Builds a directory that shares `state`'s store, rather than opening a second handle onto
    /// the same backend -- the same reasoning as
    /// [`crate::admin_verifier::AdminTokenVerifier::from_auth_state`].
    #[must_use]
    pub fn from_auth_state(state: &AuthState) -> Self {
        Self::new(state.store.clone())
    }

    async fn to_admin_user(&self, record: UserRecord) -> Result<AdminUser, SourceError> {
        let devices = self
            .store
            .list_devices(&record.user_id)
            .await
            .map_err(store_unavailable)?;
        let last_seen_at = devices
            .iter()
            .filter_map(|d| d.last_seen_ms)
            .max()
            .map(format_rfc3339_ms);
        Ok(AdminUser {
            user_id: record.user_id.to_string(),
            admin: record.is_admin,
            deactivated: record.deactivated,
            locked: record.locked,
            suspended: record.suspended,
            shadow_banned: record.shadow_banned,
            created_at: format_rfc3339_ms(record.created_at_ms),
            last_seen_at,
            device_count: devices.len() as u64,
            ..AdminUser::default()
        })
    }
}

fn store_unavailable(err: StoreError) -> SourceError {
    SourceError::Unavailable(err.to_string())
}

fn map_set_error(err: StoreError) -> SourceError {
    match err {
        StoreError::NotFound(_) => SourceError::NotFound,
        other => SourceError::Unavailable(other.to_string()),
    }
}

fn parse_user_id(user_id: &str) -> Result<ruma::OwnedUserId, SourceError> {
    ruma::UserId::parse(user_id).map_err(|e| SourceError::Invalid(e.to_string()))
}

#[async_trait::async_trait]
impl UserDirectory for AuthStoreUserDirectory {
    async fn get_user(&self, user_id: &str) -> Result<Option<AdminUser>, SourceError> {
        let uid = parse_user_id(user_id)?;
        match self.store.get_user(&uid).await.map_err(store_unavailable)? {
            Some(record) => Ok(Some(self.to_admin_user(record).await?)),
            None => Ok(None),
        }
    }

    async fn list_users(&self, filter: &UserFilter) -> Result<Vec<AdminUser>, SourceError> {
        let records = self.store.list_users().await.map_err(store_unavailable)?;
        let q = filter.q.as_ref().map(|q| q.to_lowercase());
        let mut users = Vec::new();
        for record in records {
            if let Some(admin) = filter.admin
                && record.is_admin != admin
            {
                continue;
            }
            if let Some(deactivated) = filter.deactivated
                && record.deactivated != deactivated
            {
                continue;
            }
            if let Some(locked) = filter.locked
                && record.locked != locked
            {
                continue;
            }
            if let Some(suspended) = filter.suspended
                && record.suspended != suspended
            {
                continue;
            }
            if let Some(guests) = filter.guests
                && record.is_guest != guests
            {
                continue;
            }
            if let Some(q) = &q
                && !record.user_id.as_str().to_lowercase().contains(q.as_str())
            {
                continue;
            }
            users.push(self.to_admin_user(record).await?);
        }
        Ok(users)
    }

    async fn set_admin(&self, user_id: &str, admin: bool) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        self.store
            .set_admin(&uid, admin)
            .await
            .map_err(map_set_error)
    }

    async fn set_locked(&self, user_id: &str, locked: bool) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        self.store
            .set_locked(&uid, locked)
            .await
            .map_err(map_set_error)
    }

    async fn set_deactivated(&self, user_id: &str, deactivated: bool) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        self.store
            .set_deactivated(&uid, deactivated)
            .await
            .map_err(map_set_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryAuthStore;
    use crate::store::{DeviceStore, UserStore};

    fn directory_with(store: InMemoryAuthStore) -> AuthStoreUserDirectory {
        AuthStoreUserDirectory::new(Arc::new(store))
    }

    #[tokio::test]
    async fn get_user_reflects_flags_and_device_count() {
        let store = InMemoryAuthStore::new();
        let uid = ruma::user_id!("@alice:example.org");
        store
            .create_user(UserRecord::new(uid.to_owned(), 1000))
            .await
            .unwrap();
        store.set_admin(uid, true).await.unwrap();
        store
            .upsert_device(crate::store::DeviceRecord {
                user_id: uid.to_owned(),
                device_id: ruma::device_id!("DEV1").to_owned(),
                display_name: None,
                last_seen_ms: Some(5000),
                last_seen_ip: None,
            })
            .await
            .unwrap();
        store
            .upsert_device(crate::store::DeviceRecord {
                user_id: uid.to_owned(),
                device_id: ruma::device_id!("DEV2").to_owned(),
                display_name: None,
                last_seen_ms: Some(9000),
                last_seen_ip: None,
            })
            .await
            .unwrap();

        let directory = directory_with(store);
        let user = directory
            .get_user("@alice:example.org")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.user_id, "@alice:example.org");
        assert!(user.admin);
        assert_eq!(user.device_count, 2);
        assert_eq!(
            user.last_seen_at.as_deref(),
            Some("1970-01-01T00:00:09.000Z")
        );
        assert_eq!(user.created_at, "1970-01-01T00:00:01.000Z");
        assert_eq!(user.room_count, 0);
        assert_eq!(user.media_count, 0);
    }

    #[tokio::test]
    async fn get_user_returns_none_for_unknown_user() {
        let directory = directory_with(InMemoryAuthStore::new());
        assert!(
            directory
                .get_user("@nobody:example.org")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_user_rejects_an_invalid_user_id() {
        let directory = directory_with(InMemoryAuthStore::new());
        let err = directory.get_user("not-a-user-id").await.unwrap_err();
        assert!(matches!(err, SourceError::Invalid(_)));
    }

    #[tokio::test]
    async fn list_users_applies_the_admin_filter() {
        let store = InMemoryAuthStore::new();
        store
            .create_user(UserRecord::new(
                ruma::user_id!("@admin:example.org").to_owned(),
                1,
            ))
            .await
            .unwrap();
        store
            .set_admin(ruma::user_id!("@admin:example.org"), true)
            .await
            .unwrap();
        store
            .create_user(UserRecord::new(
                ruma::user_id!("@plain:example.org").to_owned(),
                2,
            ))
            .await
            .unwrap();

        let directory = directory_with(store);
        let filter = UserFilter {
            admin: Some(true),
            ..UserFilter::default()
        };
        let users = directory.list_users(&filter).await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].user_id, "@admin:example.org");
    }

    #[tokio::test]
    async fn list_users_applies_the_free_text_filter() {
        let store = InMemoryAuthStore::new();
        store
            .create_user(UserRecord::new(
                ruma::user_id!("@findme:example.org").to_owned(),
                1,
            ))
            .await
            .unwrap();
        store
            .create_user(UserRecord::new(
                ruma::user_id!("@other:example.org").to_owned(),
                2,
            ))
            .await
            .unwrap();

        let directory = directory_with(store);
        let filter = UserFilter {
            q: Some("findme".to_string()),
            ..UserFilter::default()
        };
        let users = directory.list_users(&filter).await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].user_id, "@findme:example.org");
    }

    #[tokio::test]
    async fn set_admin_on_unknown_user_is_not_found() {
        let directory = directory_with(InMemoryAuthStore::new());
        let err = directory
            .set_admin("@ghost:example.org", true)
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::NotFound));
    }

    #[tokio::test]
    async fn set_locked_and_set_deactivated_round_trip() {
        let store = InMemoryAuthStore::new();
        let uid = ruma::user_id!("@toggle:example.org");
        store
            .create_user(UserRecord::new(uid.to_owned(), 1))
            .await
            .unwrap();
        let directory = directory_with(store);

        directory
            .set_locked("@toggle:example.org", true)
            .await
            .unwrap();
        directory
            .set_deactivated("@toggle:example.org", true)
            .await
            .unwrap();

        let user = directory
            .get_user("@toggle:example.org")
            .await
            .unwrap()
            .unwrap();
        assert!(user.locked);
        assert!(user.deactivated);
    }
}
