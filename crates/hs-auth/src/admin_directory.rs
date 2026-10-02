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
//! - `display_name`, `avatar_url`: [`crate::store::UserRecord::display_name`]/`avatar_url`
//!   directly, now that the profile endpoints (`GET`/`PUT /profile/{userId}/...`) populate them.
//!
//! Left at [`hs_admin::model::AdminUser::default`]:
//!
//! - `room_count`, `media_count`: this crate has no view onto rooms (track 04) or media (track
//!   09). Wiring those in is those tracks' data-source seam to add, not this one's -- see
//!   `docs/status/07-auth-and-identity.md`'s "Interfaces needed".
//! - `user_type`, `consent_version`: user-type categorization is a Phase 1/2 gap noted in this
//!   crate's own status file. (`appservice_id` is real: the appservice that registered the
//!   account, if one did.)
//!
//! `erased` is real since 2026-10-02: [`crate::store::UserRecord::erased`], set by
//! `UserDirectory::erase` ([`crate::erasure::erase_account`] plus the device-list announcement,
//! so `hs-e2e` drops the devices' keys). An erased account answers `users.reactivate` and
//! `users.reset_password` with `409 conflict`.
//!
//! The same type also implements `hs_admin::user_identity::UserIdentitySource`: one device read
//! or renamed, several signed out at once, the account's 3PIDs and upstream-provider subject
//! links ([`crate::store::IdentityStore`]), and its experimental-feature flags. `users.lookup`
//! finds an account by either kind of link.
//!
//! [`hs_admin::sources::UserFilter::q`] is matched against `user_id` only -- not the display
//! name -- unchanged by this pass; the free-text filter could reasonably grow to search
//! `display_name` too, but no caller has asked for that yet.

use std::sync::Arc;

use std::collections::BTreeMap;

use hs_admin::model::{AdminDevice, AdminPasswordReset, AdminUser, ExternalId, ThreePid};
use hs_admin::sources::{
    ErasureReport, SourceError, UserCreateRequest, UserDirectory, UserFilter, UserLookupQuery,
};
use hs_admin::user_identity::UserIdentitySource;

use crate::admin_verifier::format_rfc3339_ms;
use crate::state::AuthState;
use crate::store::{AuthStore, ExternalIdRecord, StoreError, ThreepidRecord, UserRecord};

/// The user directory `hs-admin`'s `/users` handlers call, over this crate's own [`AuthStore`].
pub struct AuthStoreUserDirectory {
    store: Arc<dyn AuthStore>,
    /// What creating an account needs beyond the store: this server's name, its password policy
    /// and its clock. `None` for a directory built over a bare store ([`Self::new`]), which can
    /// read and flag accounts but answers `users.create` with `503`.
    accounts: Option<AuthState>,
}

impl AuthStoreUserDirectory {
    /// Builds a directory over an already-open store.
    #[must_use]
    pub fn new(store: Arc<dyn AuthStore>) -> Self {
        Self {
            store,
            accounts: None,
        }
    }

    /// Builds a directory that shares `state`'s store, rather than opening a second handle onto
    /// the same backend -- the same reasoning as
    /// [`crate::admin_verifier::AdminTokenVerifier::from_auth_state`].
    #[must_use]
    pub fn from_auth_state(state: &AuthState) -> Self {
        Self {
            store: state.store.clone(),
            accounts: Some(state.clone()),
        }
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
            display_name: record.display_name.clone(),
            avatar_url: record.avatar_url.clone(),
            admin: record.is_admin,
            deactivated: record.deactivated,
            erased: record.erased,
            locked: record.locked,
            suspended: record.suspended,
            shadow_banned: record.shadow_banned,
            is_guest: record.is_guest,
            created_at: format_rfc3339_ms(record.created_at_ms),
            last_seen_at,
            device_count: devices.len() as u64,
            appservice_id: record.appservice_id.clone(),
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

impl AuthStoreUserDirectory {
    /// Now, from the server's clock when there is one (so tests can pin it), else the system's.
    fn now_ms(&self) -> u64 {
        match &self.accounts {
            Some(state) => state.now_ms(),
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0),
        }
    }

    /// The account, or [`SourceError::NotFound`].
    async fn existing_user(&self, user_id: &str) -> Result<ruma::OwnedUserId, SourceError> {
        let uid = parse_user_id(user_id).map_err(|_| SourceError::NotFound)?;
        match self.store.get_user(&uid).await.map_err(store_unavailable)? {
            Some(_) => Ok(uid),
            None => Err(SourceError::NotFound),
        }
    }

    async fn notify_devices_changed(&self, uid: &ruma::UserId) {
        if let Some(state) = &self.accounts {
            state.notify_device_list_changed(uid).await;
        }
    }
}

fn to_admin_device(d: crate::store::DeviceRecord) -> AdminDevice {
    AdminDevice {
        device_id: d.device_id.to_string(),
        display_name: d.display_name,
        last_seen_ip: d.last_seen_ip,
        last_seen_at: d.last_seen_ms.map(format_rfc3339_ms),
    }
}

fn to_admin_threepid(t: ThreepidRecord) -> ThreePid {
    ThreePid {
        medium: t.medium,
        address: t.address,
        added_at: Some(format_rfc3339_ms(t.added_at_ms)),
    }
}

fn map_identity_error(err: StoreError) -> SourceError {
    match err {
        StoreError::NotFound(_) => SourceError::NotFound,
        StoreError::Conflict(detail) => SourceError::Conflict(detail),
        other => SourceError::Unavailable(other.to_string()),
    }
}

/// The admin API's devices-and-identity operations, over the same store as the rest of this
/// directory. Each method answers [`SourceError::NotFound`] for an account this server does
/// not have.
#[async_trait::async_trait]
impl UserIdentitySource for AuthStoreUserDirectory {
    async fn get_device(&self, user_id: &str, device_id: &str) -> Result<AdminDevice, SourceError> {
        let uid = self.existing_user(user_id).await?;
        let device_id: ruma::OwnedDeviceId = device_id.into();
        self.store
            .get_device(&uid, &device_id)
            .await
            .map_err(store_unavailable)?
            .map(to_admin_device)
            .ok_or(SourceError::NotFound)
    }

    /// What `PUT /devices/{deviceId}` does, done by an administrator; the rename reaches the
    /// user's contacts as a device-list change, the same as a self-service one.
    async fn rename_device(
        &self,
        user_id: &str,
        device_id: &str,
        display_name: Option<String>,
    ) -> Result<AdminDevice, SourceError> {
        let uid = self.existing_user(user_id).await?;
        let device_id: ruma::OwnedDeviceId = device_id.into();
        self.store
            .set_display_name(&uid, &device_id, display_name)
            .await
            .map_err(map_identity_error)?;
        self.notify_devices_changed(&uid).await;
        self.get_device(user_id, device_id.as_str()).await
    }

    /// Checks every device first, then signs each out and deletes it, then announces the change
    /// once: `hs-e2e`'s notifier removes the keys of every device that is gone, so none of them
    /// is served by `/keys/query` again.
    async fn delete_devices(
        &self,
        user_id: &str,
        device_ids: &[String],
    ) -> Result<(), SourceError> {
        let uid = self.existing_user(user_id).await?;
        let mut ids = Vec::with_capacity(device_ids.len());
        for device_id in device_ids {
            let device_id: ruma::OwnedDeviceId = device_id.as_str().into();
            if self
                .store
                .get_device(&uid, &device_id)
                .await
                .map_err(store_unavailable)?
                .is_none()
            {
                return Err(SourceError::NotFound);
            }
            ids.push(device_id);
        }
        for device_id in &ids {
            self.store
                .delete_access_tokens_for_device(&uid, device_id)
                .await
                .map_err(store_unavailable)?;
            self.store
                .delete_device(&uid, device_id)
                .await
                .map_err(store_unavailable)?;
        }
        self.notify_devices_changed(&uid).await;
        Ok(())
    }

    async fn list_threepids(&self, user_id: &str) -> Result<Vec<ThreePid>, SourceError> {
        let uid = self.existing_user(user_id).await?;
        Ok(self
            .store
            .list_threepids(&uid)
            .await
            .map_err(store_unavailable)?
            .into_iter()
            .map(to_admin_threepid)
            .collect())
    }

    async fn add_threepid(
        &self,
        user_id: &str,
        threepid: ThreePid,
    ) -> Result<ThreePid, SourceError> {
        let uid = self.existing_user(user_id).await?;
        let now = self.now_ms();
        self.store
            .add_threepid(ThreepidRecord {
                user_id: uid.clone(),
                medium: threepid.medium.clone(),
                address: threepid.address.clone(),
                added_at_ms: now,
                validated_at_ms: now,
            })
            .await
            .map_err(map_identity_error)?;
        // Binding one the account already had keeps the first binding; answer that one.
        let lower = threepid.address.to_ascii_lowercase();
        self.store
            .list_threepids(&uid)
            .await
            .map_err(store_unavailable)?
            .into_iter()
            .find(|t| t.medium == threepid.medium && t.address.to_ascii_lowercase() == lower)
            .map(to_admin_threepid)
            .ok_or_else(|| SourceError::Unavailable("the 3PID was not stored".to_owned()))
    }

    async fn remove_threepid(
        &self,
        user_id: &str,
        medium: &str,
        address: &str,
    ) -> Result<(), SourceError> {
        let uid = self.existing_user(user_id).await?;
        self.store
            .remove_threepid(&uid, medium, address)
            .await
            .map_err(map_identity_error)
    }

    async fn list_external_ids(&self, user_id: &str) -> Result<Vec<ExternalId>, SourceError> {
        let uid = self.existing_user(user_id).await?;
        Ok(self
            .store
            .list_external_ids(&uid)
            .await
            .map_err(store_unavailable)?
            .into_iter()
            .map(|e| ExternalId {
                provider: e.provider,
                external_id: e.external_id,
            })
            .collect())
    }

    async fn add_external_id(
        &self,
        user_id: &str,
        external_id: ExternalId,
    ) -> Result<ExternalId, SourceError> {
        let uid = self.existing_user(user_id).await?;
        self.store
            .add_external_id(ExternalIdRecord {
                user_id: uid,
                provider: external_id.provider.clone(),
                external_id: external_id.external_id.clone(),
                added_at_ms: self.now_ms(),
            })
            .await
            .map_err(map_identity_error)?;
        Ok(external_id)
    }

    async fn remove_external_id(
        &self,
        user_id: &str,
        provider: &str,
        external_id: &str,
    ) -> Result<(), SourceError> {
        let uid = self.existing_user(user_id).await?;
        self.store
            .remove_external_id(&uid, provider, external_id)
            .await
            .map_err(map_identity_error)
    }

    async fn experimental_features(
        &self,
        user_id: &str,
    ) -> Result<BTreeMap<String, bool>, SourceError> {
        let uid = self.existing_user(user_id).await?;
        self.store
            .experimental_features(&uid)
            .await
            .map_err(store_unavailable)
    }

    async fn set_experimental_features(
        &self,
        user_id: &str,
        features: BTreeMap<String, bool>,
    ) -> Result<BTreeMap<String, bool>, SourceError> {
        let uid = self.existing_user(user_id).await?;
        self.store
            .set_experimental_features(&uid, features)
            .await
            .map_err(store_unavailable)?;
        self.store
            .experimental_features(&uid)
            .await
            .map_err(store_unavailable)
    }
}

#[async_trait::async_trait]
impl UserDirectory for AuthStoreUserDirectory {
    /// `users.lookup`: the account a 3PID is bound to, or an upstream subject is linked to.
    async fn lookup_user(&self, query: UserLookupQuery) -> Result<Option<AdminUser>, SourceError> {
        let found = match query {
            UserLookupQuery::Threepid { medium, address } => self
                .store
                .get_user_by_threepid(&medium, address.trim())
                .await
                .map_err(store_unavailable)?,
            UserLookupQuery::ExternalId {
                provider,
                external_id,
            } => self
                .store
                .get_user_by_external_id(&provider, &external_id)
                .await
                .map_err(store_unavailable)?,
        };
        match found {
            Some(uid) => self.get_user(uid.as_str()).await,
            None => Ok(None),
        }
    }

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

    /// `users.create`. With registration closed, which is the default, this is how every account
    /// after the first administrator comes to exist -- and until it was written the trait's
    /// default answered `503`, so on a real server nothing in the admin API or the interface
    /// could add a user at all.
    ///
    /// A password is required. An account without one could only ever be signed in to through
    /// SSO, which this server does not offer yet; creating one would be creating an account
    /// nobody can use. `threepids` and `external_ids` are bound as `users.threepids.add` and
    /// `users.external_ids.add` would bind them, after checking that no other account has any of
    /// them (a `409` then, and no account is made). `user_type` is refused rather than ignored,
    /// for the same reason `users.update` refuses the fields it cannot apply: a `201` that
    /// silently dropped part of the request would be a lie about what now exists.
    async fn create_user(&self, request: UserCreateRequest) -> Result<AdminUser, SourceError> {
        let Some(state) = &self.accounts else {
            return Err(SourceError::Unavailable(
                "this user directory was built over a bare store and cannot create accounts"
                    .to_string(),
            ));
        };
        if request.user_type.is_some() {
            return Err(SourceError::InvalidField {
                pointer: "/user_type",
                detail: "this server cannot set this when creating an account yet".to_string(),
            });
        }
        // 3PIDs and external ids are bound after the account exists; check first that nobody
        // else has any of them, so a refusal leaves no half-made account behind.
        let mut threepids = Vec::with_capacity(request.threepids.len());
        for threepid in &request.threepids {
            let address = match threepid.medium.as_str() {
                "email" => threepid.address.trim().to_lowercase(),
                "msisdn" => threepid
                    .address
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect(),
                other => {
                    return Err(SourceError::InvalidField {
                        pointer: "/threepids",
                        detail: format!("{other:?} is not a medium; use email or msisdn"),
                    });
                }
            };
            if address.is_empty() {
                return Err(SourceError::InvalidField {
                    pointer: "/threepids",
                    detail: "an address cannot be empty".to_string(),
                });
            }
            if let Some(owner) = self
                .store
                .get_user_by_threepid(&threepid.medium, &address)
                .await
                .map_err(store_unavailable)?
            {
                return Err(SourceError::Conflict(format!(
                    "{} {address} is bound to {owner}",
                    threepid.medium
                )));
            }
            threepids.push((threepid.medium.clone(), address));
        }
        for external in &request.external_ids {
            if external.provider.trim().is_empty() || external.external_id.trim().is_empty() {
                return Err(SourceError::InvalidField {
                    pointer: "/external_ids",
                    detail: "provider and external_id cannot be empty".to_string(),
                });
            }
            if let Some(owner) = self
                .store
                .get_user_by_external_id(&external.provider, &external.external_id)
                .await
                .map_err(store_unavailable)?
            {
                return Err(SourceError::Conflict(format!(
                    "{} {} is linked to {owner}",
                    external.provider, external.external_id
                )));
            }
        }

        // `user_id` wins when both are given, and they must then agree.
        let (pointer, named) = match (&request.user_id, &request.localpart) {
            (Some(user_id), _) => ("/user_id", user_id.as_str()),
            (None, Some(localpart)) => ("/localpart", localpart.as_str()),
            (None, None) => {
                return Err(SourceError::InvalidField {
                    pointer: "/localpart",
                    detail: "either localpart or user_id is required".to_string(),
                });
            }
        };
        let user_id = crate::local_user::local_user_id(state.server_name(), named)
            .map_err(|detail| SourceError::InvalidField { pointer, detail })?;
        if let (Some(_), Some(localpart)) = (&request.user_id, &request.localpart)
            && !localpart.eq_ignore_ascii_case(user_id.localpart())
        {
            return Err(SourceError::InvalidField {
                pointer: "/localpart",
                detail: format!("does not match user_id {user_id}"),
            });
        }

        let Some(password) = request.password.as_deref().filter(|p| !p.is_empty()) else {
            return Err(SourceError::InvalidField {
                pointer: "/password",
                detail: "a password is required".to_string(),
            });
        };
        state
            .config
            .get()
            .password_policy
            .validate(password)
            .map_err(|e| SourceError::InvalidField {
                pointer: "/password",
                detail: e.message().to_owned(),
            })?;

        if !self
            .store
            .is_localpart_available(user_id.localpart())
            .await
            .map_err(store_unavailable)?
        {
            return Err(SourceError::Conflict(format!("{user_id} already exists")));
        }

        let mut record = UserRecord::new(user_id.clone(), state.now_ms());
        record.password_hash = Some(
            crate::password::hash_password(password)
                .map_err(|e| SourceError::Unavailable(e.to_string()))?,
        );
        record.is_admin = request.admin;
        record.display_name = request
            .display_name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
        match self.store.create_user(record.clone()).await {
            Ok(()) => {}
            // Lost a race with another creation of the same name between the check and here.
            Err(StoreError::Conflict(_)) => {
                return Err(SourceError::Conflict(format!("{user_id} already exists")));
            }
            Err(e) => return Err(store_unavailable(e)),
        }
        let now = state.now_ms();
        for (medium, address) in threepids {
            self.store
                .add_threepid(ThreepidRecord {
                    user_id: user_id.clone(),
                    medium,
                    address,
                    added_at_ms: now,
                    validated_at_ms: now,
                })
                .await
                .map_err(map_identity_error)?;
        }
        for external in request.external_ids {
            self.store
                .add_external_id(ExternalIdRecord {
                    user_id: user_id.clone(),
                    provider: external.provider,
                    external_id: external.external_id,
                    added_at_ms: now,
                })
                .await
                .map_err(map_identity_error)?;
        }
        self.to_admin_user(record).await
    }

    async fn list_devices(&self, user_id: &str) -> Result<Vec<AdminDevice>, SourceError> {
        let uid = parse_user_id(user_id)?;
        if self
            .store
            .get_user(&uid)
            .await
            .map_err(store_unavailable)?
            .is_none()
        {
            return Err(SourceError::NotFound);
        }
        let mut devices = self
            .store
            .list_devices(&uid)
            .await
            .map_err(store_unavailable)?;
        // Most recently seen first: the one the person is holding, then the one they lost.
        devices.sort_by_key(|d| std::cmp::Reverse(d.last_seen_ms));
        Ok(devices
            .into_iter()
            .map(|d| AdminDevice {
                device_id: d.device_id.to_string(),
                display_name: d.display_name,
                last_seen_ip: d.last_seen_ip,
                last_seen_at: d.last_seen_ms.map(format_rfc3339_ms),
            })
            .collect())
    }

    /// The same three steps as `DELETE /devices/{deviceId}` in `crate::routes::devices`, with
    /// the same announcement to everyone who shares a room with the user (`hs-e2e`'s
    /// device-list stream), and without the user-interactive auth, which is the point: the
    /// person cannot do this themself because the device is the one they lost.
    async fn delete_device(&self, user_id: &str, device_id: &str) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        let device_id: ruma::OwnedDeviceId = device_id.into();
        if self
            .store
            .get_device(&uid, &device_id)
            .await
            .map_err(store_unavailable)?
            .is_none()
        {
            return Err(SourceError::NotFound);
        }
        self.store
            .delete_access_tokens_for_device(&uid, &device_id)
            .await
            .map_err(store_unavailable)?;
        self.store
            .delete_device(&uid, &device_id)
            .await
            .map_err(store_unavailable)?;
        if let Some(state) = &self.accounts {
            state.notify_device_list_changed(&uid).await;
        }
        Ok(())
    }

    /// What `POST /logout/all` does, done to somebody else.
    async fn logout_everywhere(&self, user_id: &str) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        if self
            .store
            .get_user(&uid)
            .await
            .map_err(store_unavailable)?
            .is_none()
        {
            return Err(SourceError::NotFound);
        }
        self.store
            .delete_all_access_tokens_for_user(&uid)
            .await
            .map_err(store_unavailable)?;
        self.store
            .delete_all_refresh_tokens_for_user(&uid)
            .await
            .map_err(store_unavailable)?;
        let devices = self
            .store
            .list_devices(&uid)
            .await
            .map_err(store_unavailable)?;
        for device in &devices {
            self.store
                .delete_device(&uid, &device.device_id)
                .await
                .map_err(store_unavailable)?;
        }
        if !devices.is_empty()
            && let Some(state) = &self.accounts
        {
            state.notify_device_list_changed(&uid).await;
        }
        Ok(())
    }

    async fn reset_password(
        &self,
        user_id: &str,
        request: AdminPasswordReset,
    ) -> Result<(), SourceError> {
        let Some(state) = &self.accounts else {
            return Err(SourceError::Unavailable(
                "this user directory was built over a bare store and cannot set passwords"
                    .to_string(),
            ));
        };
        let uid = parse_user_id(user_id)?;
        match self.store.get_user(&uid).await.map_err(store_unavailable)? {
            None => return Err(SourceError::NotFound),
            Some(record) if record.erased => return Err(SourceError::Conflict(ERASED.to_owned())),
            Some(_) => {}
        }
        // The server's own password policy, the same one `/register` and a self-service change
        // apply; an administrator gets its reasons beside the field.
        if let Err(e) = state
            .config
            .get()
            .password_policy
            .validate(&request.password)
        {
            return Err(SourceError::InvalidField {
                pointer: "/password",
                detail: e.message().to_owned(),
            });
        }
        let hash = crate::password::hash_password(&request.password)
            .map_err(|e| SourceError::Unavailable(format!("could not hash the password: {e}")))?;
        // Sign out first, then set: a session that survived a failed sign-out would still be
        // one the old password had opened.
        if request.logout_devices {
            self.logout_everywhere(user_id).await?;
        }
        self.store
            .set_password_hash(&uid, Some(hash))
            .await
            .map_err(map_set_error)
    }

    /// `users.deactivate` with `erase: true`: [`crate::erasure::erase_account`], then the
    /// device-list announcement if any device went, so that `/keys/query` stops serving the
    /// account's keys the way it does after `delete_devices`.
    async fn erase(&self, user_id: &str) -> Result<ErasureReport, SourceError> {
        let uid = self.existing_user(user_id).await?;
        let erased = crate::erasure::erase_account(self.store.as_ref(), &uid, self.now_ms())
            .await
            .map_err(map_set_error)?;
        if erased.devices_deleted > 0 {
            self.notify_devices_changed(&uid).await;
        }
        tracing::info!(
            user = %uid,
            devices_deleted = erased.devices_deleted,
            threepids_removed = erased.threepids_removed,
            external_ids_removed = erased.external_ids_removed,
            "account erased"
        );
        Ok(ErasureReport {
            devices_deleted: erased.devices_deleted as u64,
        })
    }
}

/// The `409` detail for `users.reactivate` and `users.reset_password` on an erased account.
pub const ERASED: &str =
    "this account is erased; its data is gone and it cannot be reactivated or signed in to";

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
    async fn get_user_reflects_profile_fields() {
        let store = InMemoryAuthStore::new();
        let uid = ruma::user_id!("@profiled:example.org");
        store
            .create_user(UserRecord::new(uid.to_owned(), 1))
            .await
            .unwrap();
        store
            .set_profile_display_name(uid, Some("Profiled".to_string()))
            .await
            .unwrap();
        store
            .set_profile_avatar_url(uid, Some("mxc://example.org/av".to_string()))
            .await
            .unwrap();

        let directory = directory_with(store);
        let user = directory
            .get_user("@profiled:example.org")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.display_name.as_deref(), Some("Profiled"));
        assert_eq!(user.avatar_url.as_deref(), Some("mxc://example.org/av"));
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

    // ---------------------------------------------------------------------------------------
    // users.create
    // ---------------------------------------------------------------------------------------

    fn creating_directory() -> (AuthState, AuthStoreUserDirectory) {
        use crate::config::{AuthConfig, PasswordPolicy};
        let state = AuthState::in_memory_with_config(AuthConfig {
            password_policy: PasswordPolicy {
                minimum_length: Some(8),
                ..PasswordPolicy::default()
            },
            ..AuthConfig::default()
        });
        let directory = AuthStoreUserDirectory::from_auth_state(&state);
        (state, directory)
    }

    fn create(localpart: &str, password: &str) -> UserCreateRequest {
        UserCreateRequest {
            localpart: Some(localpart.to_owned()),
            password: Some(password.to_owned()),
            ..UserCreateRequest::default()
        }
    }

    #[tokio::test]
    async fn create_user_makes_an_account_that_can_sign_in_with_that_password() {
        let (state, directory) = creating_directory();
        let created = directory
            .create_user(UserCreateRequest {
                display_name: Some("  Carol D ".to_owned()),
                ..create("Carol", "hunter2-carol")
            })
            .await
            .unwrap();
        assert_eq!(created.user_id, "@carol:example.org");
        assert_eq!(created.display_name.as_deref(), Some("Carol D"));
        assert!(!created.admin);

        let uid = ruma::UserId::parse("@carol:example.org").unwrap();
        let record = state.store.get_user(&uid).await.unwrap().unwrap();
        let hash = record.password_hash.expect("a password hash");
        assert!(crate::password::verify_password("hunter2-carol", &hash, "").unwrap());
        assert!(!hash.contains("hunter2"));
        assert!(!record.is_admin);
    }

    #[tokio::test]
    async fn create_user_makes_an_administrator_only_when_asked() {
        let (state, directory) = creating_directory();
        let created = directory
            .create_user(UserCreateRequest {
                admin: true,
                ..create("ops2", "hunter2-ops2")
            })
            .await
            .unwrap();
        assert!(created.admin);
        let uid = ruma::UserId::parse("@ops2:example.org").unwrap();
        assert!(state.store.get_user(&uid).await.unwrap().unwrap().is_admin);
    }

    #[tokio::test]
    async fn create_user_refusals_name_the_field_and_create_nothing() {
        let (state, directory) = creating_directory();
        let cases: Vec<(UserCreateRequest, &str)> = vec![
            (create("not a username", "hunter2-carol"), "/localpart"),
            (create("carol", "short"), "/password"),
            (create("carol", ""), "/password"),
            (
                UserCreateRequest {
                    password: None,
                    ..create("carol", "")
                },
                "/password",
            ),
            (
                UserCreateRequest {
                    user_id: Some("@carol:elsewhere.org".to_owned()),
                    localpart: None,
                    ..create("", "hunter2-carol")
                },
                "/user_id",
            ),
            (
                UserCreateRequest {
                    user_id: Some("@carol:example.org".to_owned()),
                    ..create("dave", "hunter2-carol")
                },
                "/localpart",
            ),
            (
                UserCreateRequest {
                    user_type: Some("bot".to_owned()),
                    ..create("carol", "hunter2-carol")
                },
                "/user_type",
            ),
        ];
        for (request, expected) in cases {
            match directory.create_user(request.clone()).await {
                Err(SourceError::InvalidField { pointer, .. }) => {
                    assert_eq!(pointer, expected, "{request:?}")
                }
                other => panic!("{request:?}: expected InvalidField({expected}), got {other:?}"),
            }
        }
        assert!(state.store.list_users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_user_conflicts_whatever_the_case_of_the_name() {
        let (_state, directory) = creating_directory();
        directory
            .create_user(create("carol", "hunter2-carol"))
            .await
            .unwrap();
        for again in ["carol", "Carol", "@CAROL:example.org"] {
            assert!(
                matches!(
                    directory.create_user(create(again, "hunter2-carol")).await,
                    Err(SourceError::Conflict(_))
                ),
                "{again}"
            );
        }
    }

    /// `users.create`'s 3PIDs and external ids are bound, `users.lookup` finds the account by
    /// either, and a second account naming one of them is refused without being made.
    #[tokio::test]
    async fn create_user_binds_threepids_and_external_ids_that_lookup_then_finds() {
        let (state, directory) = creating_directory();
        let with_links = |name: &str| UserCreateRequest {
            threepids: vec![ThreePid {
                medium: "email".to_owned(),
                address: " Carol@Example.org".to_owned(),
                added_at: None,
            }],
            external_ids: vec![ExternalId {
                provider: "oidc".to_owned(),
                external_id: "sub-carol".to_owned(),
            }],
            ..create(name, "hunter2-carol")
        };
        directory.create_user(with_links("carol")).await.unwrap();
        assert!(matches!(
            directory.create_user(with_links("dave")).await,
            Err(SourceError::Conflict(_))
        ));
        assert_eq!(state.store.list_users().await.unwrap().len(), 1);

        let by_email = directory
            .lookup_user(UserLookupQuery::Threepid {
                medium: "email".to_owned(),
                address: "CAROL@example.org".to_owned(),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_email.user_id, "@carol:example.org");
        let by_subject = directory
            .lookup_user(UserLookupQuery::ExternalId {
                provider: "oidc".to_owned(),
                external_id: "sub-carol".to_owned(),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_subject.user_id, "@carol:example.org");
        let listed = directory
            .list_threepids("@carol:example.org")
            .await
            .unwrap();
        assert_eq!(listed[0].address, "carol@example.org");
        assert!(listed[0].added_at.is_some());
        assert!(
            directory
                .lookup_user(UserLookupQuery::ExternalId {
                    provider: "oidc".to_owned(),
                    external_id: "SUB-CAROL".to_owned(),
                })
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Bulk sign-out checks every device before touching any, and takes each one's sessions.
    #[tokio::test]
    async fn delete_devices_is_all_or_nothing() {
        let (state, directory) = creating_directory();
        directory
            .create_user(create("erin", "hunter2-erin"))
            .await
            .unwrap();
        let uid = ruma::user_id!("@erin:example.org");
        for device in ["A", "B"] {
            state
                .store
                .upsert_device(crate::store::DeviceRecord {
                    user_id: uid.to_owned(),
                    device_id: device.into(),
                    display_name: None,
                    last_seen_ms: None,
                    last_seen_ip: None,
                })
                .await
                .unwrap();
        }
        let ids = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(matches!(
            directory
                .delete_devices("@erin:example.org", &ids(&["A", "Z"]))
                .await,
            Err(SourceError::NotFound)
        ));
        assert_eq!(state.store.list_devices(uid).await.unwrap().len(), 2);
        directory
            .delete_devices("@erin:example.org", &ids(&["A", "B"]))
            .await
            .unwrap();
        assert!(state.store.list_devices(uid).await.unwrap().is_empty());
        assert!(matches!(
            directory.get_device("@nobody:example.org", "A").await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn erase_clears_the_account_and_the_record_says_so() {
        let store = InMemoryAuthStore::new();
        let uid = ruma::user_id!("@alice:example.org").to_owned();
        store
            .create_user(UserRecord::new(uid.clone(), 1))
            .await
            .unwrap();
        store
            .set_password_hash(&uid, Some("hash".into()))
            .await
            .unwrap();
        store
            .set_profile_display_name(&uid, Some("Alice".into()))
            .await
            .unwrap();
        for device_id in ["A", "B"] {
            store
                .upsert_device(crate::store::DeviceRecord {
                    user_id: uid.clone(),
                    device_id: device_id.into(),
                    display_name: None,
                    last_seen_ms: None,
                    last_seen_ip: None,
                })
                .await
                .unwrap();
        }
        let directory = directory_with(store);

        let report = directory.erase(uid.as_str()).await.unwrap();
        assert_eq!(report.devices_deleted, 2);
        let user = directory.get_user(uid.as_str()).await.unwrap().unwrap();
        assert!(user.erased && user.deactivated);
        assert_eq!(user.display_name, None);
        assert_eq!(user.device_count, 0);
        // The password is gone, and a new one is refused: the account is not coming back.
        let refused = directory
            .reset_password(
                uid.as_str(),
                AdminPasswordReset {
                    password: "another-long-password".into(),
                    logout_devices: true,
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                refused,
                SourceError::Conflict(_) | SourceError::Unavailable(_)
            ),
            "{refused:?}"
        );
        // Erasing again removes nothing and is not an error; a stranger is not found.
        assert_eq!(
            directory.erase(uid.as_str()).await.unwrap().devices_deleted,
            0
        );
        assert!(matches!(
            directory.erase("@ghost:example.org").await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_directory_over_a_bare_store_says_it_cannot_create_accounts() {
        let directory = directory_with(InMemoryAuthStore::new());
        assert!(matches!(
            directory
                .create_user(create("carol", "hunter2-carol"))
                .await,
            Err(SourceError::Unavailable(_))
        ));
    }
}
