//! [`UserDataSource`] for the admin API's `users.account_data.list` and `users.pushers.list`:
//! what a user's clients have stored on this server, read from `hs-user`'s account-data store and
//! `hs-push`'s pusher store. It lives here, where the server is assembled, because neither crate
//! knows about the other or about the admin API.

use std::collections::BTreeMap;
use std::sync::Arc;

use hs_admin::sources::SourceError;
use hs_admin::user_identity::UserDataSource;
use serde_json::Value;

/// Reads a user's global account data and pushers.
pub struct StoredUserData {
    account_data: hs_user::store::DynUserStore,
    pushers: Arc<dyn hs_push::pushers::PusherStore>,
}

impl StoredUserData {
    /// Over the session hub's user store and the push state's pusher store.
    #[must_use]
    pub fn new(
        account_data: hs_user::store::DynUserStore,
        pushers: Arc<dyn hs_push::pushers::PusherStore>,
    ) -> Self {
        Self {
            account_data,
            pushers,
        }
    }
}

fn parse_user_id(user_id: &str) -> Result<ruma::OwnedUserId, SourceError> {
    ruma::UserId::parse(user_id).map_err(|_| SourceError::NotFound)
}

#[async_trait::async_trait]
impl UserDataSource for StoredUserData {
    async fn account_data(&self, user_id: &str) -> Result<BTreeMap<String, Value>, SourceError> {
        let uid = parse_user_id(user_id)?;
        let records = self
            .account_data
            .list_global_account_data(&uid)
            .await
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        Ok(records
            .into_iter()
            .map(|r| (r.event_type, r.content))
            .collect())
    }

    async fn pushers(&self, user_id: &str) -> Result<Vec<Value>, SourceError> {
        let uid = parse_user_id(user_id)?;
        let pushers = self
            .pushers
            .get_pushers(&uid)
            .await
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        pushers
            .into_iter()
            .map(|p| serde_json::to_value(p).map_err(|e| SourceError::Unavailable(e.to_string())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    #[tokio::test]
    async fn reads_global_account_data_and_pushers() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let store: hs_user::store::DynUserStore =
            Arc::new(hs_user::store::tables::TablesUserStore::open(backend.clone()).unwrap());
        let pushers: Arc<dyn hs_push::pushers::PusherStore> =
            Arc::new(hs_push::pushers::memory::InMemoryPusherStore::new());
        let alice = user_id!("@alice:example.org");
        store
            .put_global_account_data(
                alice,
                "m.direct",
                serde_json::json!({"@bob:example.org": []}),
            )
            .await
            .unwrap();
        let pusher: ruma::api::client::push::Pusher = serde_json::from_value(serde_json::json!({
            "pushkey": "abc",
            "kind": "http",
            "app_id": "im.example.app",
            "app_display_name": "Example",
            "device_display_name": "Phone",
            "lang": "en",
            "data": {"url": "https://push.example.org/_matrix/push/v1/notify"}
        }))
        .unwrap();
        pushers.set_pusher(alice, pusher, None).await.unwrap();

        let source = StoredUserData::new(store, pushers);
        let data = source.account_data("@alice:example.org").await.unwrap();
        assert_eq!(data["m.direct"]["@bob:example.org"], serde_json::json!([]));
        let listed = source.pushers("@alice:example.org").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["app_id"], "im.example.app");
        assert!(
            source
                .pushers("@nobody:example.org")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
