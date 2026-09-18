//! The small query-data seam this router's `/query/*`, `/user/devices/*` and
//! `/openid/userinfo` handlers need, kept separate from [`crate::room_source::RoomDataSource`]
//! because these are not room-scoped lookups (profile/device/alias data belongs to a user or an
//! alias, not a room membership check).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait FederationQuerySource: Send + Sync {
    /// A local user's profile. `field` narrows to just `displayname` or `avatar_url` when
    /// present (per `/query/profile`'s `field` query parameter); `None` returns both.
    async fn profile(&self, user_id: &str, field: Option<&str>) -> Option<Value>;

    /// Resolves a room alias to `(room_id, servers)`.
    async fn resolve_alias(&self, alias: &str) -> Option<(String, Vec<String>)>;

    /// A local user's device list, in the `/user/devices/{userId}` response shape.
    async fn devices(&self, user_id: &str) -> Option<Value>;

    /// Resolves an OpenID access token (from `/openid/userinfo`) to the local Matrix user ID it
    /// belongs to.
    async fn openid_userinfo(&self, access_token: &str) -> Option<String>;
}

/// An in-memory [`FederationQuerySource`] for this crate's own handler tests.
#[derive(Default)]
pub struct InMemoryQuerySource {
    profiles: Mutex<HashMap<String, Value>>,
    aliases: Mutex<HashMap<String, (String, Vec<String>)>>,
    devices: Mutex<HashMap<String, Value>>,
    openid_tokens: Mutex<HashMap<String, String>>,
}

impl InMemoryQuerySource {
    pub fn insert_profile(&self, user_id: &str, profile: Value) {
        self.profiles
            .lock()
            .unwrap()
            .insert(user_id.to_string(), profile);
    }
    pub fn insert_alias(&self, alias: &str, room_id: &str, servers: Vec<String>) {
        self.aliases
            .lock()
            .unwrap()
            .insert(alias.to_string(), (room_id.to_string(), servers));
    }
    pub fn insert_devices(&self, user_id: &str, devices: Value) {
        self.devices
            .lock()
            .unwrap()
            .insert(user_id.to_string(), devices);
    }
    pub fn insert_openid_token(&self, token: &str, user_id: &str) {
        self.openid_tokens
            .lock()
            .unwrap()
            .insert(token.to_string(), user_id.to_string());
    }
}

#[async_trait]
impl FederationQuerySource for InMemoryQuerySource {
    async fn profile(&self, user_id: &str, field: Option<&str>) -> Option<Value> {
        let profile = self.profiles.lock().unwrap().get(user_id).cloned()?;
        match field {
            None => Some(profile),
            Some(f) => profile.get(f).cloned().map(|v| serde_json::json!({ f: v })),
        }
    }

    async fn resolve_alias(&self, alias: &str) -> Option<(String, Vec<String>)> {
        self.aliases.lock().unwrap().get(alias).cloned()
    }

    async fn devices(&self, user_id: &str) -> Option<Value> {
        self.devices.lock().unwrap().get(user_id).cloned()
    }

    async fn openid_userinfo(&self, access_token: &str) -> Option<String> {
        self.openid_tokens
            .lock()
            .unwrap()
            .get(access_token)
            .cloned()
    }
}
