//! Account erasure: what is removed when an account is deleted for good, in one place, so that
//! an administrator's `users.deactivate` with `erase: true`
//! ([`crate::admin_directory::AuthStoreUserDirectory`]'s `UserDirectory::erase`) and the user's
//! own `POST /account/deactivate` with `erase: true` ([`crate::routes::account`]) cannot drift
//! on what "erased" means.
//!
//! Erasure removes: the password hash; every access and refresh token; every device (the caller
//! then announces the device-list change, and `hs-e2e` drops the devices' keys); every bound
//! 3PID and linked external id; the profile display name and avatar URL; and the
//! experimental-feature flags. It keeps: the user id (so it can never be registered again), the
//! `created_at_ms`, `is_admin` and the moderation flags, and `appservice_id`, and it sets
//! [`crate::store::UserRecord::erased`], [`crate::store::UserRecord::erased_at_ms`] and
//! [`crate::store::UserRecord::deactivated`]. Events the account sent stay in their rooms: a
//! room's history is the room's, and redacting a user's events is a separate operation
//! (`users.redact_events`). Leaving the account's rooms is the room layer's work
//! (`hs_room::admin_users::RoomRegistryUserActivity::leave_all_rooms`), which this crate cannot
//! see; the admin API does it before calling here.

use ruma::UserId;

use crate::store::{AuthStore, StoreError};

/// What [`erase_account`] removed, for the audit record and the log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Erased {
    /// Devices deleted (each with its access tokens).
    pub devices_deleted: usize,
    /// Third-party identifiers unbound.
    pub threepids_removed: usize,
    /// Upstream-provider subject links removed.
    pub external_ids_removed: usize,
}

/// Erases `user_id`'s account in `store` (see the module docs for the list). Idempotent: an
/// account already erased is erased again with nothing left to remove, and `erased_at_ms` is
/// moved to `now_ms`; callers that want a no-op check [`crate::store::UserRecord::erased`]
/// first. The caller announces the device-list change afterwards
/// (`AuthState::notify_device_list_changed`) if any device was deleted.
///
/// # Errors
/// [`StoreError::NotFound`] if there is no such account; the store's error otherwise. The
/// steps run in the order tokens, devices, 3PIDs, external ids, features, record, so an error
/// part-way leaves an account that is signed out but not yet marked erased -- and a retry
/// finishes the job.
pub async fn erase_account(
    store: &dyn AuthStore,
    user_id: &UserId,
    now_ms: u64,
) -> Result<Erased, StoreError> {
    if store.get_user(user_id).await?.is_none() {
        return Err(StoreError::NotFound(user_id.to_string()));
    }
    let mut erased = Erased::default();
    store.delete_all_access_tokens_for_user(user_id).await?;
    store.delete_all_refresh_tokens_for_user(user_id).await?;
    for device in store.list_devices(user_id).await? {
        store.delete_device(user_id, &device.device_id).await?;
        erased.devices_deleted += 1;
    }
    for threepid in store.list_threepids(user_id).await? {
        store
            .remove_threepid(user_id, &threepid.medium, &threepid.address)
            .await?;
        erased.threepids_removed += 1;
    }
    for link in store.list_external_ids(user_id).await? {
        store
            .remove_external_id(user_id, &link.provider, &link.external_id)
            .await?;
        erased.external_ids_removed += 1;
    }
    store
        .set_experimental_features(user_id, std::collections::BTreeMap::new())
        .await?;
    store.erase_user(user_id, now_ms).await?;
    Ok(erased)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryAuthStore;
    use crate::store::{
        AccessTokenRecord, DeviceRecord, DeviceStore, ExternalIdRecord, IdentityStore,
        ThreepidRecord, TokenStore, UserRecord, UserStore,
    };
    use crate::token::TokenHash;
    use ruma::user_id;

    #[tokio::test]
    async fn erasure_removes_everything_and_marks_the_record() {
        let store = InMemoryAuthStore::new();
        let uid = user_id!("@alice:example.org").to_owned();
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
                .upsert_device(DeviceRecord {
                    user_id: uid.clone(),
                    device_id: device_id.into(),
                    display_name: None,
                    last_seen_ms: None,
                    last_seen_ip: None,
                })
                .await
                .unwrap();
            store
                .put_access_token(AccessTokenRecord {
                    hash: TokenHash::of(&format!("syt_{device_id}")),
                    user_id: uid.clone(),
                    device_id: Some(device_id.into()),
                    expires_at_ms: None,
                    refresh_token_hash: None,
                    last_used_ms: None,
                })
                .await
                .unwrap();
        }
        store
            .add_threepid(ThreepidRecord {
                user_id: uid.clone(),
                medium: "email".into(),
                address: "alice@example.org".into(),
                added_at_ms: 1,
                validated_at_ms: 1,
            })
            .await
            .unwrap();
        store
            .add_external_id(ExternalIdRecord {
                user_id: uid.clone(),
                provider: "oidc-corp".into(),
                external_id: "sub-1".into(),
                added_at_ms: 1,
            })
            .await
            .unwrap();
        store
            .set_experimental_features(&uid, [("msc0000".to_string(), true)].into())
            .await
            .unwrap();

        let erased = erase_account(&store, &uid, 99).await.unwrap();
        assert_eq!(
            erased,
            Erased {
                devices_deleted: 2,
                threepids_removed: 1,
                external_ids_removed: 1,
            }
        );
        let record = store.get_user(&uid).await.unwrap().unwrap();
        assert!(record.erased && record.deactivated);
        assert_eq!(record.erased_at_ms, Some(99));
        assert_eq!(record.password_hash, None);
        assert_eq!(record.display_name, None);
        assert!(store.list_devices(&uid).await.unwrap().is_empty());
        assert!(
            store
                .get_access_token(&TokenHash::of("syt_A"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.list_threepids(&uid).await.unwrap().is_empty());
        assert!(store.list_external_ids(&uid).await.unwrap().is_empty());
        assert!(store.experimental_features(&uid).await.unwrap().is_empty());
        assert_eq!(
            store
                .get_user_by_threepid("email", "alice@example.org")
                .await
                .unwrap(),
            None
        );

        // Again: nothing left, still erased, not an error.
        let again = erase_account(&store, &uid, 100).await.unwrap();
        assert_eq!(again, Erased::default());
        assert!(
            erase_account(&store, user_id!("@ghost:example.org"), 1)
                .await
                .is_err()
        );
    }
}
