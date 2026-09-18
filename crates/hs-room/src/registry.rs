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
}

impl<B: KvBackend + 'static> RoomRegistry<B> {
    /// Opens a registry over `backend`.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] if opening the shared keyspaces fails.
    pub fn open(backend: B, identity: HomeserverIdentity) -> Result<Self, hs_kv::KvError> {
        let tables = Tables::open(&backend)?;
        Ok(Self {
            backend,
            tables,
            identity,
            rooms: Mutex::new(HashMap::new()),
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
        handle
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
