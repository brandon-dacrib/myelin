//! Pusher storage (`/pushers`, `/pushers/set`) and delivery (`crate::pushers::http`).
//!
//! A "pusher" is the spec's term for one (user, device/app) subscription to push notifications —
//! `ruma::api::client::push::Pusher` is used directly as the stored record (it is already
//! `Serialize`/`Deserialize` end to end via its hand-written `Deserialize` impl, and is exactly
//! the shape `GET /pushers` must return): this crate does not define a second `Pusher` type.
//!
//! Scoped by `(user_id, app_id, pushkey)` per the spec's own uniqueness rule ("if the pushkey
//! already exists for this application ID and this user... it is updated, else the pusher is
//! added"). This implementation does not yet enforce the spec's *additional* global constraint
//! that a `(app_id, pushkey)` pair identifies one device across every user on the server (so a
//! second user registering the same pushkey should silently replace the first user's pusher for
//! it, not create a second one) — recorded in `docs/status/10-push.md`'s "Decisions made" as a
//! scoped gap: without it, two users could in principle both hold a "pusher" for the same device,
//! which would double-push to that device until one of them is deleted. Closing it needs a
//! secondary `(app_id, pushkey) -> user_id` index, the same pattern
//! `hs-auth`'s `users_by_localpart_lower` already uses.

pub mod http;
pub mod memory;
pub mod tables;

use std::sync::Arc;

use ruma::api::client::push::{Pusher, PusherIds};
use ruma::{DeviceId, OwnedDeviceId, UserId};

use crate::error::StoreError;

/// Persistence for pushers.
#[async_trait::async_trait]
pub trait PusherStore: Send + Sync {
    /// Every pusher registered for `user_id`.
    async fn get_pushers(&self, user_id: &UserId) -> Result<Vec<Pusher>, StoreError>;

    /// Creates a pusher, or replaces the one already registered with the same
    /// `(app_id, pushkey)` for this user. `device_id` is the login that registered it, so
    /// [`PusherStore::delete_pushers_of_other_devices`] can find it when that login ends;
    /// `None` for a pusher with no known device (a migrated one).
    async fn set_pusher(
        &self,
        user_id: &UserId,
        pusher: Pusher,
        device_id: Option<OwnedDeviceId>,
    ) -> Result<(), StoreError>;

    /// Removes every pusher of `user_id` registered by a device other than `kept` (`None`:
    /// every pusher with a known device). Pushers with no known device are kept. What a
    /// password change with `logout_devices` does to the logins it ends.
    async fn delete_pushers_of_other_devices(
        &self,
        user_id: &UserId,
        kept: Option<&DeviceId>,
    ) -> Result<(), StoreError>;

    /// Removes the pusher identified by `ids` for this user. Deleting an absent pusher is not an
    /// error (the spec's `DELETE` semantics for `/pushers/set` with no matching pusher).
    async fn delete_pusher(&self, user_id: &UserId, ids: &PusherIds) -> Result<(), StoreError>;
}

/// The [`hs_auth::state::SessionRevocationObserver`] that removes the pushers of revoked
/// sessions.
pub struct RevokedSessionPushers {
    store: Arc<dyn PusherStore>,
}

impl RevokedSessionPushers {
    /// An observer over `store`.
    #[must_use]
    pub fn new(store: Arc<dyn PusherStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl hs_auth::state::SessionRevocationObserver for RevokedSessionPushers {
    async fn other_sessions_revoked(&self, user_id: &UserId, kept_device: Option<&DeviceId>) {
        if let Err(e) = self
            .store
            .delete_pushers_of_other_devices(user_id, kept_device)
            .await
        {
            tracing::warn!(user = %user_id, error = %e, "could not remove the pushers of revoked sessions");
        }
    }
}

#[cfg(test)]
pub(crate) mod contract_tests {
    //! Shared assertions every `PusherStore` implementation must satisfy.

    use super::*;
    use ruma::api::client::push::{PusherInit, PusherKind};
    use ruma::push::HttpPusherData;

    pub fn sample_pusher(pushkey: &str) -> Pusher {
        PusherInit {
            ids: PusherIds::new(pushkey.to_owned(), "com.example.app".to_owned()),
            kind: PusherKind::Http(HttpPusherData::new(
                "https://gw.example.org/notify".to_owned(),
            )),
            app_display_name: "Example".to_owned(),
            device_display_name: "Phone".to_owned(),
            profile_tag: None,
            lang: "en".to_owned(),
        }
        .into()
    }

    pub async fn devices_scope_deletion(store: &dyn PusherStore) {
        let alice = ruma::user_id!("@alice:example.org");
        let phone = ruma::device_id!("PHONE");
        let laptop = ruma::device_id!("LAPTOP");
        store
            .set_pusher(alice, sample_pusher("phone-key"), Some(phone.to_owned()))
            .await
            .unwrap();
        store
            .set_pusher(alice, sample_pusher("laptop-key"), Some(laptop.to_owned()))
            .await
            .unwrap();
        store
            .set_pusher(alice, sample_pusher("migrated-key"), None)
            .await
            .unwrap();
        assert_eq!(store.get_pushers(alice).await.unwrap().len(), 3);

        // The phone changes the password: the laptop's pusher goes, the migrated one stays.
        store
            .delete_pushers_of_other_devices(alice, Some(phone))
            .await
            .unwrap();
        let mut left: Vec<String> = store
            .get_pushers(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.ids.pushkey)
            .collect();
        left.sort();
        assert_eq!(left, ["migrated-key", "phone-key"]);

        // Re-registering under another device moves it; deleting it removes its device too.
        store
            .set_pusher(alice, sample_pusher("phone-key"), Some(laptop.to_owned()))
            .await
            .unwrap();
        store
            .delete_pushers_of_other_devices(alice, Some(phone))
            .await
            .unwrap();
        let left: Vec<String> = store
            .get_pushers(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.ids.pushkey)
            .collect();
        assert_eq!(left, ["migrated-key"]);
        store
            .delete_pushers_of_other_devices(alice, None)
            .await
            .unwrap();
        assert_eq!(store.get_pushers(alice).await.unwrap().len(), 1);
    }
}
