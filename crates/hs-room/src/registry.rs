//! [`RoomRegistry`]: the per-process map from room ID to [`RoomActorHandle`], with idle eviction.
//! See `crate::protocol`'s module docs, "The hot-state cache and its eviction policy".

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hs_kv::KvBackend;
use ruma::{OwnedRoomId, RoomId, UserId};
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

/// A hook `GET /rooms/{roomId}/messages` (`crate::routes::query::get_messages`) calls when a
/// `from` token is not shaped like this crate's own [`crate::timeline::PaginationToken`], to ask
/// whoever mints a *different* opaque token format (in this workspace, `hs-user`'s `/sync`
/// `hsu1_...` token) whether it recognizes `raw` and, if so, what room-local position it
/// corresponds to for this user.
///
/// # Why this indirection exists
///
/// A real Matrix client's ordinary "sync, then paginate backward from where the sync left off"
/// flow hands `/messages` a token minted by `/sync`, and Complement's own test suite does the
/// same (`room_messages_test.go`'s `TestSendAndFetchMessage` and siblings feed a bare `/sync`
/// `next_batch` straight into `/messages?from=`). Resolving that token into a room-local position
/// needs whatever per-user bookkeeping the minting crate keeps (for `hs-user`, its own durable
/// per-user feed, `crate::store::UserStore::room_pos_as_of` in that crate) -- data this crate has
/// no way to reach without depending on the crate that owns it, which would be a cycle (`hs-user`
/// already depends on `hs-room` for exactly the reverse reason: it renders room timelines using
/// this crate's own `RoomActor`). Defining this trait *here* and letting the token-minting crate
/// implement and [`RoomRegistry::install_global_token_resolver`] it at construction time avoids
/// the cycle in both directions: this crate never names `hs-user` or its token format, and the
/// installing crate never needs `hs-cli` (or anything else) to change how it wires this crate's
/// state together -- see `docs/status/05-sync.md`'s "Decisions made" for the full writeup,
/// including why a single shared wire format across both crates was rejected instead.
#[async_trait::async_trait]
pub trait GlobalTokenResolver: Send + Sync {
    /// Attempts to resolve `raw` for `user_id` reading `room_id`.
    ///
    /// Returns:
    /// - `Ok(None)` if `raw` is not shaped like a token this resolver mints at all -- the caller
    ///   should fall through to treating `raw` as invalid (neither format matched), not silently
    ///   drop the constraint.
    /// - `Ok(Some(None))` if `raw` *is* one of this resolver's own tokens, but does not resolve to
    ///   any specific room-local position for this room (for example: a token issued before this
    ///   user's session ever observed the room). The caller should treat this the same as an
    ///   altogether absent `from` -- paginate from the live end/start -- never as an error, since
    ///   the token is valid, just uninformative for this particular room.
    /// - `Ok(Some(Some(pos)))` if `raw` resolves to room-local position `pos`.
    /// - `Err` only for a genuine backing-store failure, never for "not my format".
    async fn resolve(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        raw: &str,
    ) -> Result<Option<Option<i64>>, RoomError>;
}

/// The registry's one stream of every resident room's updates, numbered. Numbering and sending
/// happen under one lock so that the numbers come out in the order the stream delivers them --
/// every resident room publishes here, from inside its own actor, and two of them racing
/// between "take a number" and "send" would otherwise deliver 6 before 5, and a consumer that
/// had seen 6 would be wrong to say it had seen everything up to it. The lock is held for a
/// `broadcast::send`, which does not block.
pub(crate) struct GlobalStream {
    sender: tokio::sync::broadcast::Sender<RoomUpdate>,
    next_seq: std::sync::Mutex<u64>,
    /// Mirrors `next_seq` for lock-free reads.
    published: std::sync::atomic::AtomicU64,
}

impl GlobalStream {
    pub(crate) fn publish(&self, mut update: RoomUpdate) {
        let mut next = self
            .next_seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        update.global_seq = *next;
        self.published
            .store(*next, std::sync::atomic::Ordering::Release);
        let _ = self.sender.send(update);
    }
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
    global: Arc<GlobalStream>,
    /// See [`GlobalTokenResolver`] and [`RoomRegistry::install_global_token_resolver`]. Unset
    /// (`None`) means no other crate has installed one -- `crate::routes::query::get_messages`
    /// then treats a `from` token that isn't this crate's own format as a hard error, same as
    /// before this hook existed.
    global_token_resolver: OnceLock<Arc<dyn GlobalTokenResolver>>,
    /// See [`crate::fencing::RoomFencing`] and [`RoomRegistry::install_fencing`]. Unset (`None`,
    /// the default until `hs-cli` installs one) means every actor this registry constructs or
    /// loads runs with no cluster-fencing check at all -- `RoomActor::persist` behaves exactly as
    /// it did before this hook existed.
    fencing: OnceLock<Arc<crate::fencing::RoomFencing<B>>>,
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
        // Room for a burst. An update is a few hundred bytes; a consumer that falls more than
        // this far behind is told so (`Lagged`), and what it does about it is its business --
        // `hs-user`'s session hub re-reads every resident room.
        let (sender, _rx) = tokio::sync::broadcast::channel(16 * 1024);
        let global = Arc::new(GlobalStream {
            sender,
            next_seq: std::sync::Mutex::new(0),
            published: std::sync::atomic::AtomicU64::new(0),
        });
        Ok(Self {
            backend,
            tables,
            identity,
            rooms: Mutex::new(HashMap::new()),
            global,
            global_token_resolver: OnceLock::new(),
            fencing: OnceLock::new(),
        })
    }

    /// Installs the [`GlobalTokenResolver`] this registry's `GET /messages` handler consults for
    /// a `from` token that is not this crate's own [`crate::timeline::PaginationToken`] format.
    /// Idempotent past the first call: a second install is silently ignored (logged, not
    /// panicked) rather than risking a surprising resolver swap under a server that somehow
    /// constructs its wiring twice -- one registry is expected to have exactly one installer for
    /// the lifetime of the process. See [`GlobalTokenResolver`]'s doc comment for who calls this
    /// and why.
    pub fn install_global_token_resolver(&self, resolver: Arc<dyn GlobalTokenResolver>) {
        if self.global_token_resolver.set(resolver).is_err() {
            tracing::warn!(
                "a global token resolver was already installed on this room registry; ignoring \
                 the second install"
            );
        }
    }

    /// The installed [`GlobalTokenResolver`], if any.
    #[must_use]
    pub fn global_token_resolver(&self) -> Option<&Arc<dyn GlobalTokenResolver>> {
        self.global_token_resolver.get()
    }

    /// Installs the cluster-fencing hook every actor this registry constructs or loads from now
    /// on will check inside `RoomActor::persist` (`docs/status/03-cluster.md` item 4). Idempotent
    /// past the first call, same as [`RoomRegistry::install_global_token_resolver`]: a second
    /// install is logged and ignored rather than risking a surprising swap.
    ///
    /// **Not called anywhere in this crate today.** Wiring it in is `hs-cli`'s line to add (out
    /// of this crate's ownership) -- see `docs/status/04-room-and-events.md` for exactly what
    /// that line looks like and why it is not written here.
    pub fn install_fencing(&self, fencing: Arc<crate::fencing::RoomFencing<B>>) {
        if self.fencing.set(fencing).is_err() {
            tracing::warn!(
                "cluster fencing was already installed on this room registry; ignoring the \
                 second install"
            );
        }
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

        let Some(mut actor) = loaded else {
            return Err(RoomError::RoomNotFound(room_id.to_string()));
        };
        actor.set_fencing(self.fencing.get().cloned());

        let mut rooms = self.rooms.lock().await;
        let entry = match rooms.entry(room_id.to_owned()) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                // Loaded twice at once; the other load won, and this actor is dropped unused.
                entry.into_mut()
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                // Joined to the global stream like a created room is. It was not, once, so a
                // room loaded from disk -- every room, after a restart -- published to nobody:
                // nothing that happened in it reached `/sync`, push, or a bridge. Every test
                // created its rooms in the process that read them, and the first one that did
                // not found it. Under the lock, so that the head announcement and the entry are
                // one step for anybody racing to load the same room.
                actor.join_global_stream(self.global.clone());
                slot.insert(Entry {
                    handle: RoomActorHandle::new(actor),
                    last_used: Instant::now(),
                })
            }
        };
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
    /// any existing entry for its room ID. Installs this registry's cluster-fencing hook (if any)
    /// onto `actor` first, same as [`RoomRegistry::get_or_load`] -- every path that puts an actor
    /// into this registry's map goes through here or through `get_or_load` directly.
    pub async fn insert(&self, mut actor: RoomActor<B>) -> RoomActorHandle<B> {
        actor.set_fencing(self.fencing.get().cloned());
        actor.join_global_stream(self.global.clone());
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
        self.global.sender.subscribe()
    }

    /// The `global_seq` of the newest update published on the global stream, or `0` if none has
    /// been. A consumer that has processed an update with this number has processed everything
    /// published so far; see `RoomUpdate::global_seq`.
    #[must_use]
    pub fn global_published_seq(&self) -> u64 {
        self.global
            .published
            .load(std::sync::atomic::Ordering::Acquire)
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

    /// Every room `user_id` currently holds `join` membership in, without loading each room's
    /// actor first. See [`crate::actor::rooms_joined_by_user`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn rooms_joined_by_user(&self, user_id: &UserId) -> Result<Vec<OwnedRoomId>, RoomError> {
        crate::actor::rooms_joined_by_user(&self.backend, &self.tables, user_id)
    }

    /// Every room this server has ever created. See [`crate::actor::list_all_room_ids`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn list_all_room_ids(&self) -> Result<Vec<OwnedRoomId>, RoomError> {
        crate::actor::list_all_room_ids(&self.backend, &self.tables)
    }

    /// Every room this server holds, with the position of its newest event, without loading any
    /// of them. See [`crate::actor::room_heads`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn room_heads(&self) -> Result<Vec<(OwnedRoomId, i64)>, RoomError> {
        crate::actor::room_heads(&self.backend, &self.tables)
    }

    /// Blocks or unblocks `room_id` (`hs-admin`'s `rooms.set_blocked`). See
    /// [`crate::actor::set_room_blocked`].
    ///
    /// # Errors
    /// Returns [`RoomError::RoomNotFound`] if `room_id` has never been created, or
    /// [`RoomError::Store`] on a storage failure.
    pub fn set_room_blocked(
        &self,
        room_id: &ruma::RoomId,
        blocked: bool,
        reason: Option<String>,
    ) -> Result<(), RoomError> {
        crate::actor::set_room_blocked(&self.backend, &self.tables, room_id, blocked, reason)
    }

    /// Whether `room_id` is currently blocked, and if so, its reason. See
    /// [`crate::actor::room_block_reason`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn room_block_reason(
        &self,
        room_id: &ruma::RoomId,
    ) -> Result<Option<Option<String>>, RoomError> {
        crate::actor::room_block_reason(&self.backend, &self.tables, room_id)
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

    /// A handle to every room resident right now. For a consumer of the global stream that has
    /// fallen behind it and wants to look at every room it might have missed something in.
    pub async fn resident_handles(&self) -> Vec<RoomActorHandle<B>> {
        self.rooms
            .lock()
            .await
            .values()
            .map(|entry| entry.handle.clone())
            .collect()
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
    /// (`broadcast::Sender::send` returns `Err` with no receivers, which the stream must ignore
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

    /// What a follower with a cursor needs (appservice delivery): where every room stands,
    /// without loading any of them, and a room's events after a position, each with its own.
    #[tokio::test]
    async fn room_heads_and_events_after_let_a_follower_keep_its_place() {
        let registry = registry();
        let alice = user_id!("@alice:registry.test").to_owned();
        assert!(registry.room_heads().unwrap().is_empty());

        let handle = registry
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let created = handle.query(|a| a.events_after(0, usize::MAX).len()).await;
        assert!(created > 1, "a room is created with more than one event");
        assert_eq!(
            registry.room_heads().unwrap(),
            vec![(room_id.clone(), i64::try_from(created).unwrap())]
        );

        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "one more"}),
                None,
                2,
            )
            .await
            .unwrap();
        let head = i64::try_from(created).unwrap() + 1;
        assert_eq!(registry.room_heads().unwrap(), vec![(room_id, head)]);

        let (positions, bodies) = handle
            .query(move |a| {
                let after = a.events_after(head - 1, 10);
                (
                    after.iter().map(|(pos, _)| *pos).collect::<Vec<_>>(),
                    after
                        .iter()
                        .map(|(_, e)| {
                            crate::routes::render::client_event_json(e)["content"]["body"].clone()
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .await;
        assert_eq!(positions, vec![head]);
        assert_eq!(bodies, vec![serde_json::json!("one more")]);
        // A limit is a limit, and a negative cursor is the beginning, not before it.
        assert_eq!(handle.query(|a| a.events_after(-5, 2).len()).await, 2);
        assert_eq!(handle.query(|a| a.events_after(-5, 2)[0].0).await, 1);
    }

    /// A room loaded from disk is a room like any other to the global stream. It was not: only
    /// `insert` (a created room) joined it to the stream, so after a restart every existing room
    /// published to nobody, and nothing said in one reached `/sync`, push or a bridge. Found by
    /// the first test to send a message to a real server it had restarted.
    #[tokio::test]
    async fn a_room_loaded_from_disk_reports_its_events_on_the_global_stream() {
        let backend = MemoryBackend::new();
        let alice = user_id!("@alice:registry.test").to_owned();
        let room_id = {
            let registry = Arc::new(
                RoomRegistry::open(
                    backend.clone(),
                    HomeserverIdentity::for_tests("registry.test"),
                )
                .unwrap(),
            );
            let handle = registry
                .create_room(alice.clone(), CreateRoomRequest::default(), 1)
                .await
                .unwrap();
            handle.query(|a| a.room_id().to_owned()).await
        };

        // "A restart": a new registry over the same storage, with a subscriber taken out before
        // anything is loaded, as `hs serve` does.
        let registry = Arc::new(
            RoomRegistry::open(backend, HomeserverIdentity::for_tests("registry.test")).unwrap(),
        );
        let mut updates = registry.subscribe_global();
        let handle = registry.get_or_load(&room_id).await.unwrap();
        handle
            .send_event(
                alice,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "after the restart"}),
                None,
                2,
            )
            .await
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let update = tokio::time::timeout_at(deadline, updates.recv())
                .await
                .expect("the message should reach the global stream")
                .unwrap();
            if update.event_type == "m.room.message" {
                assert_eq!(update.room_id, room_id);
                break;
            }
        }
    }
}
