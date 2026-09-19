//! Implements `hs_admin::sources::RoomDirectory` over [`crate::registry::RoomRegistry`].
//!
//! `crates/hs-admin/src/sources.rs` defines the trait and its full contract (session 6 of
//! `docs/status/15-admin-api-and-modules.md`); this module is the "real implementation" its own
//! doc comment says belongs in this crate, since `hs-room` is the actual source of room state.
//! `hs-admin` depends on neither `hs-room` nor `hs-auth`, so this crate depending on `hs-admin` to
//! provide the implementation is not a cycle (see `crates/hs-room/Cargo.toml`).
//!
//! Wiring this into a running server needs one change in `hs-cli` (out of this crate's ownership,
//! see `docs/status/04-room-and-events.md`): `crate::router::AdminState::with_rooms(Arc::new(
//! RoomRegistryDirectory::new(rooms.clone())))` at the same call site that already builds
//! `AdminState` in `crates/hs-cli/src/serve.rs`'s `admin_state` function.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hs_admin::model::AdminRoom;
use hs_admin::sources::{RoomDirectory, RoomFilter, SourceError};
use hs_kv::KvBackend;
use ruma::{RoomId, UserId};

use crate::error::RoomError;
use crate::registry::RoomRegistry;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Maps this crate's own error type onto the admin API's data-source error catalog
/// (`crates/hs-admin/src/sources.rs::SourceError`). A room actually not existing is the only case
/// mapped to [`SourceError::NotFound`]; every other rejection from the room actor (blocked room,
/// membership precondition failure, ...) is [`SourceError::Invalid`] -- the admin API is the
/// operator asking for something malformed or currently impossible, not a client being
/// unauthorized in the ordinary sense -- and anything else is [`SourceError::Unavailable`], never
/// silently swallowed.
fn to_source_error(e: RoomError) -> SourceError {
    match e {
        RoomError::RoomNotFound(_) => SourceError::NotFound,
        RoomError::Forbidden(msg)
        | RoomError::BadRequest(msg)
        | RoomError::RoomBlocked(Some(msg)) => SourceError::Invalid(msg),
        RoomError::RoomBlocked(None) => SourceError::Invalid("this room is blocked".to_owned()),
        other => SourceError::Unavailable(other.to_string()),
    }
}

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, SourceError> {
    RoomId::parse(raw).map_err(|e| SourceError::Invalid(format!("not a valid room id: {e}")))
}

fn parse_user_id(raw: &str) -> Result<ruma::OwnedUserId, SourceError> {
    UserId::parse(raw).map_err(|e| SourceError::Invalid(format!("not a valid user id: {e}")))
}

/// Whether `room` matches every constraint `filter` sets, mirroring
/// `hs_admin::sources::InMemoryRoomDirectory::list_rooms`'s own filter semantics exactly (kept as
/// a separate copy rather than a shared helper: that logic is private to `hs-admin`'s own test
/// fake, and duplicating four short comparisons is cheaper than exporting it across the crate
/// boundary for one caller).
fn matches_filter(room: &AdminRoom, filter: &RoomFilter) -> bool {
    if let Some(public) = filter.public
        && room.public != public
    {
        return false;
    }
    if let Some(empty) = filter.empty
        && (room.joined_members_count == 0) != empty
    {
        return false;
    }
    if let Some(blocked) = filter.blocked
        && room.blocked != blocked
    {
        return false;
    }
    if let Some(encrypted) = filter.encrypted
        && room.encrypted != encrypted
    {
        return false;
    }
    if let Some(federatable) = filter.federatable
        && room.federatable != federatable
    {
        return false;
    }
    if let Some(room_type) = &filter.room_type
        && room.room_type.as_deref() != Some(room_type.as_str())
    {
        return false;
    }
    if let Some(version) = &filter.version
        && &room.version != version
    {
        return false;
    }
    if let Some(q) = &filter.q {
        let q = q.to_lowercase();
        let hay = [
            Some(room.room_id.as_str()),
            room.name.as_deref(),
            room.topic.as_deref(),
            room.canonical_alias.as_deref(),
        ];
        if !hay
            .iter()
            .flatten()
            .any(|s| s.to_lowercase().contains(q.as_str()))
        {
            return false;
        }
    }
    true
}

/// Adapts a [`RoomRegistry`] to [`hs_admin::sources::RoomDirectory`].
pub struct RoomRegistryDirectory<B: KvBackend> {
    registry: Arc<RoomRegistry<B>>,
}

impl<B: KvBackend + 'static> RoomRegistryDirectory<B> {
    /// Wraps `registry` for use as an `hs-admin` data source.
    #[must_use]
    pub fn new(registry: Arc<RoomRegistry<B>>) -> Self {
        Self { registry }
    }

    /// Loads `room_id`'s admin summary, or `Ok(None)` if the room does not exist -- the shape
    /// both [`RoomDirectory::get_room`] and [`RoomDirectory::list_rooms`] need.
    async fn load_summary(&self, room_id: &RoomId) -> Result<Option<AdminRoom>, SourceError> {
        match self.registry.get_or_load(room_id).await {
            Ok(handle) => Ok(Some(handle.admin_summary().await.map_err(to_source_error)?)),
            Err(RoomError::RoomNotFound(_)) => Ok(None),
            Err(e) => Err(to_source_error(e)),
        }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> RoomDirectory for RoomRegistryDirectory<B> {
    async fn get_room(&self, room_id: &str) -> Result<Option<AdminRoom>, SourceError> {
        let room_id = parse_room_id(room_id)?;
        self.load_summary(&room_id).await
    }

    async fn list_rooms(&self, filter: &RoomFilter) -> Result<Vec<AdminRoom>, SourceError> {
        let room_ids = self.registry.list_all_room_ids().map_err(to_source_error)?;
        let mut out = Vec::with_capacity(room_ids.len());
        for room_id in room_ids {
            if let Some(room) = self.load_summary(&room_id).await?
                && matches_filter(&room, filter)
            {
                out.push(room);
            }
        }
        out.sort_by(|a, b| a.room_id.cmp(&b.room_id));
        Ok(out)
    }

    async fn set_blocked(
        &self,
        room_id: &str,
        blocked: bool,
        reason: Option<String>,
    ) -> Result<(), SourceError> {
        let room_id = parse_room_id(room_id)?;
        self.registry
            .set_room_blocked(&room_id, blocked, reason)
            .map_err(to_source_error)
    }

    async fn make_admin(&self, room_id: &str, user_id: &str) -> Result<(), SourceError> {
        let room_id = parse_room_id(room_id)?;
        let user_id = parse_user_id(user_id)?;
        let handle = self
            .registry
            .get_or_load(&room_id)
            .await
            .map_err(to_source_error)?;
        handle
            .make_admin(user_id, now_ms())
            .await
            .map(|_event| ())
            .map_err(to_source_error)
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::actor::CreateRoomRequest;
    use crate::identity::HomeserverIdentity;

    fn registry() -> Arc<RoomRegistry<MemoryBackend>> {
        Arc::new(
            RoomRegistry::open(
                MemoryBackend::new(),
                HomeserverIdentity::for_tests("admin.test"),
            )
            .expect("opening an in-memory registry cannot fail"),
        )
    }

    async fn create_test_room(
        registry: &RoomRegistry<MemoryBackend>,
        creator: &ruma::UserId,
    ) -> ruma::OwnedRoomId {
        let handle = registry
            .create_room(
                creator.to_owned(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("Lounge".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .expect("create should succeed");
        handle.query(|actor| actor.room_id().to_owned()).await
    }

    #[tokio::test]
    async fn get_room_returns_none_for_an_unknown_room() {
        let registry = registry();
        let directory = RoomRegistryDirectory::new(registry);
        let found = directory
            .get_room("!nobody:admin.test")
            .await
            .expect("lookup should not error");
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn get_room_reports_a_real_room_created_through_the_registry() {
        let registry = registry();
        let alice = user_id!("@alice:admin.test");
        let room_id = create_test_room(&registry, alice).await;
        // `preset: "public_chat"` only sets join rules/history visibility; directory publication
        // (`AdminRoom::public`) is `PUT /directory/list/room/{roomId}`'s own separate step.
        registry
            .set_directory_visibility(&room_id, true)
            .expect("publishing should succeed");
        let directory = RoomRegistryDirectory::new(registry);

        let found = directory
            .get_room(room_id.as_str())
            .await
            .expect("lookup should not error")
            .expect("room should be found");
        assert_eq!(found.room_id, room_id.to_string());
        assert_eq!(found.name.as_deref(), Some("Lounge"));
        assert_eq!(found.joined_members_count, 1);
        assert_eq!(found.local_members_count, 1);
        assert_eq!(found.creator.as_deref(), Some(alice.as_str()));
        assert!(found.public);
        assert!(!found.blocked);
        assert!(!found.tombstoned);
        assert!(found.federatable);
    }

    #[tokio::test]
    async fn list_rooms_finds_every_room_this_server_has_ever_created() {
        let registry = registry();
        let alice = user_id!("@alice:admin.test");
        let first = create_test_room(&registry, alice).await;
        let second = create_test_room(&registry, alice).await;
        // Evict both from residency to prove `list_rooms` does not depend on the registry's
        // in-process cache -- it must find rooms purely from durable storage.
        registry.evict_idle(std::time::Duration::from_secs(0)).await;
        assert_eq!(registry.resident_count().await, 0);

        let directory = RoomRegistryDirectory::new(registry);
        let rooms = directory
            .list_rooms(&RoomFilter::default())
            .await
            .expect("listing should not error");
        let ids: Vec<_> = rooms.iter().map(|r| r.room_id.clone()).collect();
        assert!(ids.contains(&first.to_string()));
        assert!(ids.contains(&second.to_string()));
    }

    #[tokio::test]
    async fn list_rooms_applies_the_blocked_filter() {
        let registry = registry();
        let alice = user_id!("@alice:admin.test");
        let blocked_room = create_test_room(&registry, alice).await;
        let _other_room = create_test_room(&registry, alice).await;
        registry
            .set_room_blocked(&blocked_room, true, Some("spam".to_owned()))
            .expect("blocking should succeed");

        let directory = RoomRegistryDirectory::new(registry);
        let rooms = directory
            .list_rooms(&RoomFilter {
                blocked: Some(true),
                ..Default::default()
            })
            .await
            .expect("listing should not error");
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].room_id, blocked_room.to_string());
        assert_eq!(rooms[0].blocked_reason.as_deref(), Some("spam"));
    }

    #[tokio::test]
    async fn set_blocked_has_a_real_effect_a_blocked_room_rejects_a_new_local_event() {
        let registry = registry();
        let alice = user_id!("@alice:admin.test");
        let room_id = create_test_room(&registry, alice).await;
        let directory = RoomRegistryDirectory::new(registry.clone());

        directory
            .set_blocked(room_id.as_str(), true, Some("under review".to_owned()))
            .await
            .expect("blocking should succeed");

        let handle = registry
            .get_or_load(&room_id)
            .await
            .expect("room should load");
        let err = handle
            .send_event(
                alice.to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hello"}),
                None,
                2,
            )
            .await
            .expect_err("a blocked room must reject a new local event");
        assert!(matches!(err, RoomError::RoomBlocked(_)));

        // Unblocking restores ordinary sending.
        directory
            .set_blocked(room_id.as_str(), false, None)
            .await
            .expect("unblocking should succeed");
        handle
            .send_event(
                alice.to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hello again"}),
                None,
                3,
            )
            .await
            .expect("an unblocked room accepts events again");
    }

    #[tokio::test]
    async fn set_blocked_on_an_unknown_room_is_not_found() {
        let registry = registry();
        let directory = RoomRegistryDirectory::new(registry);
        let err = directory
            .set_blocked("!nobody:admin.test", true, None)
            .await
            .expect_err("blocking a nonexistent room must fail");
        assert!(matches!(err, SourceError::NotFound));
    }

    #[tokio::test]
    async fn make_admin_sends_a_real_power_levels_event_granting_the_target_user() {
        let registry = registry();
        let alice = user_id!("@alice:admin.test");
        let bob = user_id!("@bob:admin.test");
        let room_id = create_test_room(&registry, alice).await;
        let handle = registry
            .get_or_load(&room_id)
            .await
            .expect("room should load");
        handle
            .membership(
                alice.to_owned(),
                crate::membership::Action::Invite,
                bob.to_owned(),
                serde_json::json!({}),
                2,
            )
            .await
            .expect("invite should succeed");
        handle
            .membership(
                bob.to_owned(),
                crate::membership::Action::Join,
                bob.to_owned(),
                serde_json::json!({}),
                3,
            )
            .await
            .expect("join should succeed");

        let directory = RoomRegistryDirectory::new(registry);
        directory
            .make_admin(room_id.as_str(), bob.as_str())
            .await
            .expect("make_admin should succeed");

        let power_levels = handle
            .query(|actor| {
                actor
                    .state_event("m.room.power_levels", "")
                    .expect("state read should not error")
                    .expect("power levels event should exist")
                    .json()
                    .get("content")
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|c| c.get("users"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|users| users.get(bob.as_str()))
                    .cloned()
            })
            .await
            .expect("bob should now be in the users map");
        assert_eq!(
            power_levels,
            hs_model::canonical::CanonicalJsonValue::Integer(100)
        );
    }

    #[tokio::test]
    async fn make_admin_refuses_a_user_who_is_not_a_member() {
        let registry = registry();
        let alice = user_id!("@alice:admin.test");
        let room_id = create_test_room(&registry, alice).await;
        let directory = RoomRegistryDirectory::new(registry);
        let err = directory
            .make_admin(room_id.as_str(), "@stranger:admin.test")
            .await
            .expect_err("a non-member cannot be made admin");
        assert!(matches!(err, SourceError::Invalid(_)));
    }
}
