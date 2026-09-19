//! [`RoomRegistry`]: the per-process map from room ID to [`RoomActorHandle`], with idle eviction.
//! See `crate::protocol`'s module docs, "The hot-state cache and its eviction policy".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hs_kv::KvBackend;
use ruma::OwnedRoomId;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::actor::{RoomActor, RoomActorHandle};
use crate::error::RoomError;
use crate::identity::HomeserverIdentity;
use crate::persist::Tables;
use crate::protocol::RoomUpdate;

struct Entry<B: KvBackend> {
    handle: RoomActorHandle<B>,
    last_used: Instant,
}

/// The registry: `room_id -> RoomActorHandle`, loaded on first access and dropped after
/// [`RoomRegistry::evict_idle`] finds it idle longer than the configured threshold.
///
/// A dropped entry loses nothing durable ([`RoomActor::load`] reconstructs it fully from the
/// store); the next access just pays the reconstruction cost again. This is deliberately
/// room-granularity, not per-event -- see `crate::actor::RoomActor`'s doc comment on its `events`
/// field for the documented next step (a bounded recent-timeline window within one still-resident
/// actor).
pub struct RoomRegistry<B: KvBackend> {
    backend: B,
    tables: Tables<B>,
    identity: HomeserverIdentity,
    rooms: Mutex<HashMap<OwnedRoomId, Entry<B>>>,
    /// Every resident room's updates, fanned into one stream. See
    /// [`RoomRegistry::subscribe_global`] and `docs/rfcs/0012-room-registry-global-updates.md`.
    global: tokio::sync::broadcast::Sender<RoomUpdate>,
}

impl<B: KvBackend + 'static> RoomRegistry<B> {
    /// Opens a registry over `backend`.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] if opening the shared keyspaces fails.
    pub fn open(backend: B, identity: HomeserverIdentity) -> Result<Self, hs_kv::KvError> {
        let tables = Tables::open(&backend)?;
        // Sized to absorb a burst from many rooms at once without stalling any room actor: a
        // `broadcast` send never blocks, it drops the oldest item and reports `Lagged` to the
        // slow receiver, which the consumer must handle (`hs_user::hub`'s watcher does).
        let (global, _rx) = tokio::sync::broadcast::channel(1024);
        Ok(Self {
            backend,
            tables,
            identity,
            rooms: Mutex::new(HashMap::new()),
            global,
        })
    }

    /// The handle for `room_id`, loading it from the store if it is not already resident.
    ///
    /// # Errors
    /// Returns [`RoomError::RoomNotFound`] if the room does not exist, or any error
    /// [`RoomActor::load`] can return.
    pub async fn get_or_load(
        &self,
        room_id: &ruma::RoomId,
    ) -> Result<RoomActorHandle<B>, RoomError> {
        {
            let mut rooms = self.rooms.lock().await;
            if let Some(entry) = rooms.get_mut(room_id) {
                entry.last_used = Instant::now();
                return Ok(entry.handle.clone());
            }
        }

        let backend = self.backend.clone();
        let tables = self.tables.clone();
        let identity = self.identity.clone();
        let owned_room_id = room_id.to_owned();
        let loaded = tokio::task::spawn_blocking(move || {
            RoomActor::load(backend, tables, identity, &owned_room_id)
        })
        .await
        .expect("room load task panicked")?;

        let Some(actor) = loaded else {
            return Err(RoomError::RoomNotFound(room_id.to_string()));
        };
        let handle = RoomActorHandle::new(actor);

        let mut rooms = self.rooms.lock().await;
        let entry = rooms.entry(room_id.to_owned()).or_insert_with(|| Entry {
            handle: handle.clone(),
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        Ok(entry.handle.clone())
    }

    /// Creates a new room (`crate::actor::RoomActor::create_room`) and registers it.
    ///
    /// # Errors
    /// Returns any error `RoomActor::create_room` can return.
    pub async fn create_room(
        &self,
        creator: ruma::OwnedUserId,
        request: crate::actor::CreateRoomRequest,
        now_ms: i64,
    ) -> Result<RoomActorHandle<B>, RoomError> {
        let backend = self.backend.clone();
        let tables = self.tables.clone();
        let identity = self.identity.clone();
        let actor = tokio::task::spawn_blocking(move || {
            RoomActor::create_room(backend, tables, identity, creator, request, now_ms)
        })
        .await
        .expect("room creation task panicked")?;
        Ok(self.insert(actor).await)
    }

    /// Registers an already-constructed actor (the result of `RoomActor::create_room`), replacing
    /// any existing entry for its room ID.
    pub async fn insert(&self, actor: RoomActor<B>) -> RoomActorHandle<B> {
        let room_id = actor.room_id().to_owned();
        let handle = RoomActorHandle::new(actor);
        let mut rooms = self.rooms.lock().await;
        rooms.insert(
            room_id,
            Entry {
                handle: handle.clone(),
                last_used: Instant::now(),
            },
        );
        self.spawn_global_forwarder(&handle);
        handle
    }

    /// Forwards one newly-resident room's publish stream into this registry's global stream. One
    /// task per inserted handle, which is also one per *residency*: a room evicted and later
    /// reloaded is a new actor with a new publish channel and gets a new forwarder, while the old
    /// task ends on its own when the old actor is dropped and its channel closes.
    fn spawn_global_forwarder(&self, handle: &RoomActorHandle<B>) {
        let handle = handle.clone();
        let global = self.global.clone();
        tokio::spawn(async move {
            let mut rx = handle.subscribe().await;
            // Subscribe first, then announce: a room built by `RoomActor::create_room` published
            // its whole create burst before any handle existed to subscribe with, so without this
            // a freshly created room would never appear on the global stream at all until someone
            // wrote to it. Taking the subscription before reading the head means an event landing
            // in between is seen twice at worst, never missed.
            if let Some(head) = handle.query(|actor| actor.head_update()).await {
                let _ = global.send(head);
            }
            loop {
                match rx.recv().await {
                    // A send failing means nobody is subscribed globally, which is normal (a
                    // server with no `hs-user` wired in, or before the watcher starts).
                    Ok(update) => {
                        let _ = global.send(update);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "the global room-update forwarder fell behind a room's publish stream"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// A stream of every [`RoomUpdate`] published by any room this registry loads or creates, from
    /// the moment the subscription is taken out. This is the fan-in hook
    /// `docs/rfcs/0012-room-registry-global-updates.md` asked for: it is what lets `hs-user`'s
    /// session hub learn that a room exists without this crate knowing `hs-user` does.
    ///
    /// Updates published *before* the first subscription, and before a room is first loaded, are
    /// not replayed. A consumer that must not miss an invite should subscribe at startup, before
    /// serving any request.
    #[must_use]
    pub fn subscribe_global(&self) -> tokio::sync::broadcast::Receiver<RoomUpdate> {
        self.global.subscribe()
    }

    /// Finds one event by ID across every room this registry's backend holds, without loading
    /// (or even knowing) the room it belongs to. The returned row carries its `room_id`, which is
    /// the input to that room's visibility check -- this performs none itself. See
    /// [`crate::actor::find_event_globally`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure, or [`RoomError::Internal`] if the
    /// stored row cannot be decoded.
    pub fn find_event_globally(
        &self,
        event_id: &ruma::EventId,
    ) -> Result<Option<crate::persist::PersistedEvent>, RoomError> {
        crate::actor::find_event_globally(&self.backend, &self.tables, event_id)
    }

    /// Resolves a local alias directly against the store, without loading the target room.
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn resolve_alias(
        &self,
        alias: &ruma::RoomAliasId,
    ) -> Result<Option<OwnedRoomId>, RoomError> {
        crate::actor::resolve_alias(&self.backend, &self.tables, alias)
    }

    /// Publishes or unpublishes `room_id` in the server's room directory
    /// (`PUT /_matrix/client/v3/directory/list/room/{roomId}`). See
    /// [`crate::actor::set_directory_visibility`].
    ///
    /// # Errors
    /// Returns [`RoomError::RoomNotFound`] if `room_id` has never been created, or
    /// [`RoomError::Store`] on a storage failure.
    pub fn set_directory_visibility(
        &self,
        room_id: &ruma::RoomId,
        published: bool,
    ) -> Result<(), RoomError> {
        crate::actor::set_directory_visibility(&self.backend, &self.tables, room_id, published)
    }

    /// Whether `room_id` is currently published. See [`crate::actor::is_directory_public`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn is_directory_public(&self, room_id: &ruma::RoomId) -> Result<bool, RoomError> {
        crate::actor::is_directory_public(&self.backend, &self.tables, room_id)
    }

    /// Every currently published room ID. See [`crate::actor::list_published_room_ids`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn list_published_room_ids(&self) -> Result<Vec<OwnedRoomId>, RoomError> {
        crate::actor::list_published_room_ids(&self.backend, &self.tables)
    }

    /// Drops every resident actor idle longer than `max_idle`. Intended to be called
    /// periodically (see [`RoomRegistry::spawn_eviction_sweeper`]); safe to call directly in
    /// tests for a deterministic assertion instead of waiting on a timer.
    pub async fn evict_idle(&self, max_idle: Duration) -> usize {
        let mut rooms = self.rooms.lock().await;
        let before = rooms.len();
        let now = Instant::now();
        rooms.retain(|_, entry| now.duration_since(entry.last_used) < max_idle);
        before - rooms.len()
    }

    /// Spawns a background task that calls [`RoomRegistry::evict_idle`] every `sweep_interval`.
    /// Optional: a caller that wants deterministic control over eviction (tests, or a server that
    /// wants to drive it from its own scheduler) can call [`RoomRegistry::evict_idle`] directly
    /// instead and never call this.
    pub fn spawn_eviction_sweeper(
        self: &Arc<Self>,
        sweep_interval: Duration,
        max_idle: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(sweep_interval);
            loop {
                interval.tick().await;
                registry.evict_idle(max_idle).await;
            }
        })
    }

    /// How many rooms are currently resident. For tests and diagnostics.
    pub async fn resident_count(&self) -> usize {
        self.rooms.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::CreateRoomRequest;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    fn registry() -> Arc<RoomRegistry<MemoryBackend>> {
        Arc::new(
            RoomRegistry::open(
                MemoryBackend::new(),
                HomeserverIdentity::for_tests("registry.test"),
            )
            .expect("opening an in-memory registry cannot fail"),
        )
    }

    /// The fan-in hook of `docs/rfcs/0012-room-registry-global-updates.md`: a subscriber taken out
    /// before any room exists sees a newly created room, without holding that room's handle.
    #[tokio::test]
    async fn subscribe_global_reports_a_room_created_after_subscribing() {
        let registry = registry();
        let mut updates = registry.subscribe_global();

        let handle = registry
            .create_room(
                user_id!("@alice:registry.test").to_owned(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .expect("create should succeed");
        let room_id = handle.query(|a| a.room_id().to_owned()).await;

        // `create_room` publishes its whole create burst while the actor is still under
        // construction, so the head announcement is what carries the room across -- see
        // `RoomActor::head_update`.
        let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .expect("an update should arrive")
            .expect("the global sender should still be live");
        assert_eq!(update.room_id, room_id);
    }

    /// A subsequent write reaches the same subscriber, which is what makes the stream useful past
    /// discovery: it is the live feed, not a one-shot announcement.
    #[tokio::test]
    async fn subscribe_global_reports_later_events_in_a_known_room() {
        let registry = registry();
        let mut updates = registry.subscribe_global();
        let alice = user_id!("@alice:registry.test").to_owned();

        let handle = registry
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .expect("create should succeed");

        handle
            .send_event(
                alice,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hello"}),
                None,
                2,
            )
            .await
            .expect("send should succeed");

        // Drain until the message shows up: the head announcement and any create-burst event that
        // raced the subscription come first.
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let update = updates.recv().await.expect("sender should be live");
                if update.event_type == "m.room.message" {
                    return update;
                }
            }
        })
        .await
        .expect("the message's update should arrive");
        assert_eq!(found.event_type, "m.room.message");
    }

    /// Nothing requires a global subscriber: a registry nobody listens to serves rooms normally.
    /// (`broadcast::Sender::send` returns `Err` with no receivers, which the forwarder must ignore
    /// rather than treat as a failure.)
    #[tokio::test]
    async fn a_room_works_with_no_global_subscriber() {
        let registry = registry();
        let handle = registry
            .create_room(
                user_id!("@alice:registry.test").to_owned(),
                CreateRoomRequest::default(),
                1,
            )
            .await
            .expect("create should succeed");
        assert_eq!(registry.resident_count().await, 1);
        assert!(handle.query(|a| a.head_update()).await.is_some());
    }
}
