//! [`RoomRegistry`]: the per-process map from room ID to [`RoomActorHandle`], with idle eviction.
//! See `crate::protocol`'s module docs, "The hot-state cache and its eviction policy".

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hs_kv::KvBackend;
use ruma::{OwnedRoomId, RoomId, UserId};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::actor::event_cache::CacheCapacity;
use crate::actor::{RoomActor, RoomActorHandle};
use crate::error::RoomError;
use crate::identity::HomeserverIdentity;
use crate::persist::Tables;
use crate::protocol::RoomUpdate;

struct Entry<B: KvBackend> {
    handle: RoomActorHandle<B>,
    last_used: Instant,
    /// The fence this replica held for the room's shard when the actor was loaded or created:
    /// `None` with no fencing installed, or when this replica did not own the shard then. A
    /// resident copy is only as current as the ownership it was loaded under: once the shard
    /// has changed hands (the fencing epoch moved) another replica may have written rows the
    /// copy never read, and [`RoomRegistry::get_or_load`] drops it rather than hand it out.
    fence: Option<hs_cluster::Fence>,
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
    /// See [`crate::backfill::Backfill`] and [`RoomRegistry::install_backfill`]. Unset (`None`,
    /// the default until `hs-cli` installs one) means a room's history is exactly what this
    /// server holds: `crate::routes::query::get_messages` reaches the oldest held event and
    /// says so, as it did before this hook existed.
    backfill: OnceLock<Arc<dyn crate::backfill::Backfill>>,
    /// See [`crate::hierarchy::RemoteHierarchy`] and [`RoomRegistry::install_remote_hierarchy`].
    /// Unset (`None`, the default until `hs-cli` installs one) means the space hierarchy shows
    /// only the rooms this server holds: a child on another server is left out.
    remote_hierarchy: OnceLock<Arc<dyn crate::hierarchy::RemoteHierarchy>>,
    /// The `GET /hierarchy` pagination tokens this process has handed out
    /// (`crate::hierarchy::PaginationSessions`).
    hierarchy_sessions: crate::hierarchy::PaginationSessions,
    /// See [`crate::fencing::RoomFencing`] and [`RoomRegistry::install_fencing`]. Unset (`None`,
    /// the default until `hs-cli` installs one) means every actor this registry constructs or
    /// loads runs with no cluster-fencing check at all -- `RoomActor::persist` behaves exactly as
    /// it did before this hook existed.
    fencing: OnceLock<Arc<crate::fencing::RoomFencing<B>>>,
    /// See [`crate::third_party_invite::IdentityService`] and
    /// [`RoomRegistry::install_identity_service`]. Unset (the default until `hs-cli` installs one)
    /// refuses every third-party invite `M_THREEPID_DENIED`.
    identity_service: OnceLock<Arc<dyn crate::third_party_invite::IdentityService>>,
    /// See [`RoomRegistry::install_server_notices_user`]. Unset means this server sends no
    /// server notices, and no room is one.
    server_notices_user: OnceLock<ruma::OwnedUserId>,

    /// Reports users have filed about events, rooms and other users (`crate::reports`).
    reports: crate::reports::ReportStore<B>,
    /// The per-user rate-limit overrides' token buckets (`crate::moderation`).
    send_limiter: crate::moderation::SendLimiter,
    /// The room-event search index (`crate::search`), in this registry's store.
    search: crate::search::SearchIndex<B>,
    /// The rooms a user of this server is joining through another server right now, with how
    /// many such joins are under way ([`RoomRegistry::remote_join_started`]).
    joining: Arc<std::sync::Mutex<HashMap<OwnedRoomId, usize>>>,
    /// How many event bodies each resident room keeps in memory
    /// (`server.rooms.event_cache_size`): installed on every actor this registry constructs or
    /// loads, and read by each on every insert, so [`RoomRegistry::set_event_cache_size`]
    /// applies at once. See `crate::actor::event_cache`.
    event_cache_capacity: CacheCapacity,
    /// How long an unused room stays resident, in seconds; `0` for ever
    /// (`server.rooms.idle_unload_after`). Read by [`RoomRegistry::spawn_idle_unloader`]'s task
    /// on every sweep.
    idle_unload_after_secs: Arc<std::sync::atomic::AtomicU64>,
}

/// How often [`RoomRegistry::spawn_idle_unloader`] looks for rooms idle longer than
/// `server.rooms.idle_unload_after`.
pub const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// A join through another server under way, from [`RoomRegistry::remote_join_started`] until
/// this is dropped.
#[must_use = "the join counts as under way only while this is held"]
pub struct RemoteJoinInProgress {
    joining: Arc<std::sync::Mutex<HashMap<OwnedRoomId, usize>>>,
    room_id: OwnedRoomId,
}

impl Drop for RemoteJoinInProgress {
    fn drop(&mut self) {
        let mut joining = self
            .joining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = joining.get_mut(&self.room_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                joining.remove(&self.room_id);
            }
        }
    }
}

impl<B: KvBackend + 'static> RoomRegistry<B> {
    /// Opens a registry over `backend`.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] if opening the shared keyspaces fails.
    pub fn open(backend: B, identity: HomeserverIdentity) -> Result<Self, hs_kv::KvError> {
        let tables = Tables::open(&backend)?;
        let reports = crate::reports::ReportStore::open(backend.clone())?;
        let search = crate::search::SearchIndex::open(backend.clone(), tables.clone())?;
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
            backfill: OnceLock::new(),
            remote_hierarchy: OnceLock::new(),
            hierarchy_sessions: crate::hierarchy::PaginationSessions::default(),
            fencing: OnceLock::new(),
            server_notices_user: OnceLock::new(),
            identity_service: OnceLock::new(),
            reports,
            send_limiter: crate::moderation::SendLimiter::new(),
            search,
            joining: Arc::default(),
            event_cache_capacity: CacheCapacity::default(),
            idle_unload_after_secs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Sets how many event bodies each resident room keeps in memory
    /// (`server.rooms.event_cache_size`). Applies to every room already resident on its next
    /// cached event, and to every room loaded from now on.
    pub fn set_event_cache_size(&self, events: usize) {
        self.event_cache_capacity.set(events);
    }

    /// The per-room event cache capacity as it stands.
    #[must_use]
    pub fn event_cache_size(&self) -> usize {
        self.event_cache_capacity.get()
    }

    /// Sets how long an unused room stays resident before
    /// [`RoomRegistry::spawn_idle_unloader`]'s task unloads it; `None` keeps every room for as
    /// long as the process runs (`server.rooms.idle_unload_after`). Read on the next sweep.
    pub fn set_idle_unload_after(&self, after: Option<Duration>) {
        self.idle_unload_after_secs.store(
            after.map_or(0, |d| d.as_secs().max(1)),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// How long an unused room stays resident, as it stands; `None` for ever.
    #[must_use]
    pub fn idle_unload_after(&self) -> Option<Duration> {
        match self
            .idle_unload_after_secs
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        }
    }

    /// Spawns the task that unloads rooms idle longer than
    /// [`RoomRegistry::idle_unload_after`], every [`IDLE_SWEEP_INTERVAL`]; a sweep with the
    /// setting unset does nothing, so the task is spawned once whatever the setting and follows
    /// it as it changes. What `hs serve` installs; [`RoomRegistry::spawn_eviction_sweeper`] is
    /// the fixed-threshold variant for a caller with its own schedule.
    pub fn spawn_idle_unloader(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(IDLE_SWEEP_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(max_idle) = registry.idle_unload_after() else {
                    continue;
                };
                let unloaded = registry.evict_idle(max_idle).await;
                if unloaded > 0 {
                    tracing::info!(
                        unloaded,
                        idle_for = ?max_idle,
                        "unloaded rooms nobody had used for a while; each is loaded again on its next use"
                    );
                }
            }
        })
    }

    /// Records how many rooms are resident (`hs_room_resident_rooms`); called under the map
    /// lock after every change to it.
    fn note_residents(rooms: &HashMap<OwnedRoomId, Entry<B>>) {
        crate::metrics::set_resident_rooms(rooms.len());
    }

    /// Marks a join of `room_id` through another server as under way, until the returned guard
    /// is dropped. Between the resident server accepting the join and this server holding it,
    /// the resident already sends this server the room's new events, while no user of this
    /// server is joined here yet; `/send` takes them rather than ignoring them as it ignores a
    /// room this server is not in ([`RoomRegistry::remote_join_in_progress`]; Synapse queues
    /// them for the same reason). Per process.
    pub fn remote_join_started(&self, room_id: &ruma::RoomId) -> RemoteJoinInProgress {
        *self
            .joining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(room_id.to_owned())
            .or_default() += 1;
        RemoteJoinInProgress {
            joining: self.joining.clone(),
            room_id: room_id.to_owned(),
        }
    }

    /// Whether a join of `room_id` through another server is under way in this process
    /// ([`RoomRegistry::remote_join_started`]).
    #[must_use]
    pub fn remote_join_in_progress(&self, room_id: &ruma::RoomId) -> bool {
        self.joining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(room_id)
    }

    /// The room-event search index (`crate::search`).
    #[must_use]
    pub fn search_index(&self) -> &crate::search::SearchIndex<B> {
        &self.search
    }

    /// Whether this replica owns `room_id`'s shard: always, when no cluster fencing is installed
    /// (single-node mode, or a caller that never wired `hs-cluster` in).
    #[must_use]
    pub fn owns_room(&self, room_id: &ruma::RoomId) -> bool {
        self.fencing
            .get()
            .is_none_or(|f| f.ownership.is_mine(f.layout.room_shard(room_id.as_str())))
    }

    /// Runs `f` over `room_id`'s actor for a read. On the replica that owns the room that is the
    /// resident actor (loaded if need be); on another replica it is a fresh load of what the
    /// shared store holds, not kept resident, since nothing here would keep it current.
    ///
    /// # Errors
    /// [`RoomError::RoomNotFound`] if the room does not exist, or whatever loading it can return.
    pub async fn read_room<T, F>(&self, room_id: &ruma::RoomId, f: F) -> Result<T, RoomError>
    where
        T: Send + 'static,
        F: FnOnce(&RoomActor<B>) -> T + Send + 'static,
    {
        if self.owns_room(room_id) {
            let handle = self.get_or_load(room_id).await?;
            return Ok(handle.query(f).await);
        }
        let (backend, tables, identity) = (
            self.backend.clone(),
            self.tables.clone(),
            self.identity.clone(),
        );
        let room_id = room_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let actor = RoomActor::load(backend, tables, identity, &room_id)?
                .ok_or_else(|| RoomError::RoomNotFound(room_id.to_string()))?;
            Ok(f(&actor))
        })
        .await
        .map_err(|e| RoomError::Internal(format!("room read task failed: {e}")))?
    }

    /// This server's name.
    #[must_use]
    pub fn server_name(&self) -> &ruma::ServerName {
        &self.identity.server_name
    }

    /// The token buckets of the users an administrator has given a rate-limit override
    /// (`crate::moderation`).
    #[must_use]
    pub fn send_limiter(&self) -> &crate::moderation::SendLimiter {
        &self.send_limiter
    }

    /// The reports users have filed (`crate::routes::report` writes them, the admin API reads
    /// them through [`crate::reports::RoomReports`]).
    #[must_use]
    pub fn reports(&self) -> &crate::reports::ReportStore<B> {
        &self.reports
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

    /// Installs the identity-server client third-party invites go through
    /// (`crate::third_party_invite`). Idempotent past the first call, like the other hooks.
    pub fn install_identity_service(
        &self,
        service: Arc<dyn crate::third_party_invite::IdentityService>,
    ) {
        if self.identity_service.set(service).is_err() {
            tracing::warn!(
                "an identity service was already installed on this room registry; ignoring the \
                 second install"
            );
        }
    }

    /// The installed [`crate::third_party_invite::IdentityService`], if any.
    #[must_use]
    pub fn identity_service(&self) -> Option<&Arc<dyn crate::third_party_invite::IdentityService>> {
        self.identity_service.get()
    }

    /// Installs the hook `crate::routes::query::get_messages` uses to fetch a room's history from
    /// before the oldest event this server holds (`crate::backfill`). Idempotent past the first
    /// call, same as [`RoomRegistry::install_global_token_resolver`]: a second install is logged
    /// and ignored.
    pub fn install_backfill(&self, backfill: Arc<dyn crate::backfill::Backfill>) {
        if self.backfill.set(backfill).is_err() {
            tracing::warn!(
                "a backfill hook was already installed on this room registry; ignoring the \
                 second install"
            );
        }
    }

    /// The installed [`crate::backfill::Backfill`] hook, if any.
    #[must_use]
    pub fn backfill_hook(&self) -> Option<&Arc<dyn crate::backfill::Backfill>> {
        self.backfill.get()
    }

    /// Installs the hook `GET /rooms/{roomId}/hierarchy` (`crate::hierarchy::walk`) uses to ask
    /// another server about a room of a space this server does not hold. Idempotent past the
    /// first call, same as [`RoomRegistry::install_backfill`]: a second install is logged and
    /// ignored.
    pub fn install_remote_hierarchy(&self, hook: Arc<dyn crate::hierarchy::RemoteHierarchy>) {
        if self.remote_hierarchy.set(hook).is_err() {
            tracing::warn!(
                "a remote-hierarchy hook was already installed on this room registry; ignoring \
                 the second install"
            );
        }
    }

    /// The installed [`crate::hierarchy::RemoteHierarchy`] hook, if any.
    #[must_use]
    pub fn remote_hierarchy_hook(&self) -> Option<&Arc<dyn crate::hierarchy::RemoteHierarchy>> {
        self.remote_hierarchy.get()
    }

    /// The `GET /hierarchy` pagination sessions this process holds.
    #[must_use]
    pub fn hierarchy_sessions(&self) -> &crate::hierarchy::PaginationSessions {
        &self.hierarchy_sessions
    }

    /// Names the user server notices are sent as (the Matrix specification's "Server Notices"
    /// module). A room that user created is a server-notices room, and its recipient cannot
    /// reject the invitation to it (`crate::routes::membership::post_leave`): the notice has to
    /// be seen. Idempotent past the first call, like the other installs here.
    pub fn install_server_notices_user(&self, user_id: ruma::OwnedUserId) {
        if self.server_notices_user.set(user_id).is_err() {
            tracing::warn!(
                "a server-notices user was already installed on this room registry; ignoring \
                 the second install"
            );
        }
    }

    /// The user server notices are sent as, if this server sends them.
    #[must_use]
    pub fn server_notices_user(&self) -> Option<&ruma::UserId> {
        self.server_notices_user.get().map(|u| u.as_ref())
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

    /// The fence this replica holds for `room_id`'s shard right now: `None` with no fencing
    /// installed, or when this replica does not own the shard. Computed fresh on every call
    /// (`hs_cluster::Ownership::fence`), never captured.
    fn current_fence(&self, room_id: &ruma::RoomId) -> Option<hs_cluster::Fence> {
        let fencing = self.fencing.get()?;
        fencing
            .ownership
            .fence(fencing.layout.room_shard(room_id.as_str()))
    }

    /// Whether a resident copy loaded under `loaded_under` may be handed out now that this
    /// replica holds `current` for its shard: not when it owns the shard under a different
    /// fence than the copy was loaded under. The epoch advances on every release and every
    /// acquisition (decision 0023), so a shard this replica lost and got back always shows a
    /// new one, and so does a shard it never owned when the copy was loaded. A shard it does
    /// not own now (`current` is `None`) keeps the copy: nothing writes through it (the fence
    /// check in `RoomActor::persist` refuses), and reads of a room another replica owns go
    /// through `hs-user`'s mirror, not here.
    fn copy_is_stale(
        loaded_under: Option<hs_cluster::Fence>,
        current: Option<hs_cluster::Fence>,
    ) -> bool {
        current.is_some() && current != loaded_under
    }

    /// The handle for `room_id`, loading it from the store if it is not already resident.
    ///
    /// A resident copy is dropped and the room loaded again when the room's shard has changed
    /// hands since the copy was loaded ([`Entry::fence`]). Until 2026-10-09 the copy was handed
    /// out as it was: a replica that lost a shard to a peer and got it back (a scale-down after a
    /// scale-up, a roll) wrote the next event from a copy whose timeline head was behind the
    /// store, onto the position of an event the peer had written, and every replica that then
    /// loaded the room from the store found a forward extremity its timeline no longer held
    /// (`/sync` `500 unknown event EventSn#...`, sends `500 cited event not in history`).
    ///
    /// # Errors
    /// Returns [`RoomError::RoomNotFound`] if the room does not exist, or any error
    /// [`RoomActor::load`] can return.
    pub async fn get_or_load(
        &self,
        room_id: &ruma::RoomId,
    ) -> Result<RoomActorHandle<B>, RoomError> {
        // Read before the load, so that a handoff during the load shows as a mismatch on the
        // next call rather than being hidden behind a fence read after it.
        let fence = self.current_fence(room_id);
        {
            let mut rooms = self.rooms.lock().await;
            if let Some(entry) = rooms.get_mut(room_id) {
                if Self::copy_is_stale(entry.fence, fence) {
                    let shard = self
                        .fencing
                        .get()
                        .map(|f| f.layout.room_shard(room_id.as_str()).to_string())
                        .unwrap_or_default();
                    tracing::info!(
                        %room_id,
                        shard,
                        loaded_under_epoch = ?entry.fence.and_then(|f| f.epoch).map(|e| e.0),
                        epoch = ?fence.and_then(|f| f.epoch).map(|e| e.0),
                        "the room's shard changed hands since this copy was loaded; dropping \
                         the copy and loading the room again from the store"
                    );
                    crate::metrics::count_stale_copy_reloaded();
                    rooms.remove(room_id);
                    Self::note_residents(&rooms);
                } else {
                    entry.last_used = Instant::now();
                    return Ok(entry.handle.clone());
                }
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
        actor.set_cache_capacity(self.event_cache_capacity.clone());
        actor.set_fencing(self.fencing.get().cloned());
        match actor.persist_repaired_outlier_states() {
            Ok(0) => {}
            Ok(written) => {
                tracing::info!(%room_id, written, "wrote back the state rows of placed outliers the load repaired")
            }
            Err(RoomError::Fenced(msg)) => {
                tracing::debug!(%room_id, %msg, "the state rows of placed outliers the load repaired stay unwritten: this replica does not own the room")
            }
            Err(error) => {
                tracing::warn!(%room_id, %error, "could not write back the state rows of placed outliers the load repaired; they will be repaired again on the next load")
            }
        }

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
                    fence,
                })
            }
        };
        entry.last_used = Instant::now();
        let handle = entry.handle.clone();
        Self::note_residents(&rooms);
        Ok(handle)
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
        let fencing = self.fencing.get().cloned();
        let joined = creator.clone();
        let mut actor = tokio::task::spawn_blocking(move || {
            RoomActor::create_room_placed(
                backend, tables, identity, creator, request, now_ms, fencing,
            )
        })
        .await
        .expect("room creation task panicked")?;
        // An upgraded room's replacement says its creator joined: the create burst reached
        // no stream, and what follows a join into a replacement -- the joiner's `m.direct`
        // and tags about the old room carried onto the new one (`hs-user`'s
        // `carry_account_data_on_upgrade`) -- runs off that delta. Without it the upgrader's
        // own direct chat stopped being one (Sytest's "/upgrade preserves direct room state").
        if actor.predecessor_room_id().is_some() {
            actor.set_fencing(self.fencing.get().cloned());
            actor.join_global_stream_announcing(
                self.global.clone(),
                vec![crate::protocol::MembershipDelta {
                    user_id: joined,
                    membership: "join".to_owned(),
                }],
            );
            return Ok(self.register(actor).await);
        }
        Ok(self.insert(actor).await)
    }

    /// Registers an already-constructed actor (the result of `RoomActor::create_room`), replacing
    /// any existing entry for its room ID. Installs this registry's cluster-fencing hook (if any)
    /// onto `actor` first, same as [`RoomRegistry::get_or_load`] -- every path that puts an actor
    /// into this registry's map goes through here, [`RoomRegistry::insert_if_absent`] or
    /// `get_or_load` directly.
    pub async fn insert(&self, mut actor: RoomActor<B>) -> RoomActorHandle<B> {
        actor.set_fencing(self.fencing.get().cloned());
        actor.join_global_stream(self.global.clone());
        self.register(actor).await
    }

    /// The map half of [`RoomRegistry::insert`], for an actor already fenced and on the global
    /// stream.
    async fn register(&self, mut actor: RoomActor<B>) -> RoomActorHandle<B> {
        actor.set_cache_capacity(self.event_cache_capacity.clone());
        let room_id = actor.room_id().to_owned();
        let fence = self.current_fence(&room_id);
        let handle = RoomActorHandle::new(actor);
        let mut rooms = self.rooms.lock().await;
        rooms.insert(
            room_id,
            Entry {
                handle: handle.clone(),
                last_used: Instant::now(),
                fence,
            },
        );
        Self::note_residents(&rooms);
        handle
    }

    /// [`RoomRegistry::insert`], except that an entry already present for the room ID wins: the
    /// existing handle is returned and `actor` is dropped unused. For a caller that constructed
    /// an actor for a room it found absent a moment ago and must not displace one a concurrent
    /// caller registered in between (two users joining the same remote room at once). Installs
    /// fencing and joins the global stream under the map lock, so the announcement and the
    /// entry are one step for anyone racing to load the same room, as `get_or_load` does.
    async fn insert_if_absent(&self, mut actor: RoomActor<B>) -> RoomActorHandle<B> {
        actor.set_cache_capacity(self.event_cache_capacity.clone());
        actor.set_fencing(self.fencing.get().cloned());
        let room_id = actor.room_id().to_owned();
        let fence = self.current_fence(&room_id);
        let mut rooms = self.rooms.lock().await;
        let entry = match rooms.entry(room_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                actor.join_global_stream(self.global.clone());
                slot.insert(Entry {
                    handle: RoomActorHandle::new(actor),
                    last_used: Instant::now(),
                    fence,
                })
            }
        };
        entry.last_used = Instant::now();
        let handle = entry.handle.clone();
        Self::note_residents(&rooms);
        handle
    }

    /// The room to import `room_id`'s history into (the Synapse importer): its handle if the room
    /// exists here already -- an import that stopped part way resumes into it, and every event
    /// it already holds is answered `AlreadyKnown` -- and otherwise a new, empty shell for it at
    /// `room_version`, registered at once. Nothing durable says the room exists until its first
    /// event (the `m.room.create`) is imported through [`RoomActorHandle::import_event`].
    ///
    /// # Errors
    /// Any error [`RoomRegistry::get_or_load`] or `RoomActor::empty_for` can return
    /// ([`RoomError::UnsupportedRoomVersion`] for a version this server does not implement).
    pub async fn import_shell(
        &self,
        room_id: &ruma::RoomId,
        room_version: ruma::RoomVersionId,
    ) -> Result<RoomActorHandle<B>, RoomError> {
        match self.get_or_load(room_id).await {
            Ok(handle) => Ok(handle),
            Err(RoomError::RoomNotFound(_)) => {
                let backend = self.backend.clone();
                let tables = self.tables.clone();
                let identity = self.identity.clone();
                let owned_room_id = room_id.to_owned();
                let shell = tokio::task::spawn_blocking(move || {
                    RoomActor::empty_for(backend, tables, identity, &owned_room_id, room_version)
                })
                .await
                .map_err(|e| RoomError::Internal(format!("room shell task failed: {e}")))??;
                Ok(self.insert_if_absent(shell).await)
            }
            Err(e) => Err(e),
        }
    }

    /// Forgets a shell [`RoomRegistry::import_shell`] made whose first event was then refused, so
    /// that the registry does not answer a room that does not exist. A room that holds any event
    /// is left alone.
    pub async fn discard_import_shell(&self, room_id: &ruma::RoomId, handle: &RoomActorHandle<B>) {
        self.drop_if_unbootstrapped(room_id, handle).await;
    }

    /// Drops the registry's entry for `room_id` if it is still `handle`'s actor and that actor
    /// holds no timeline at all -- a shell [`RoomRegistry::bootstrap_from_remote_join`] created
    /// for a join that was then refused. Such a shell has nothing durable behind it
    /// (`RoomActor::empty_for`), so leaving it resident would only make `get_or_load` answer a
    /// room that does not exist. An actor another caller has since filled in is left alone.
    async fn drop_if_unbootstrapped(&self, room_id: &ruma::RoomId, handle: &RoomActorHandle<B>) {
        if handle.query(|actor| actor.head_update().is_some()).await {
            return;
        }
        let mut rooms = self.rooms.lock().await;
        if rooms
            .get(room_id)
            .is_some_and(|entry| entry.handle.ptr_eq(handle))
        {
            rooms.remove(room_id);
            Self::note_residents(&rooms);
        }
    }

    /// Makes a room this server's own user has just joined on a resident server exist here, from
    /// that server's **verified** `send_join` response -- the entry point
    /// `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md` asked for. `state`,
    /// `auth_chain` and `join_event` are `hs_federation::outbound_join::RemoteJoinOutcome`'s
    /// fields of the same names, every event already hash- and signature-checked by the caller;
    /// `room_version` is the version `make_join` reported. See
    /// [`RoomActor::accept_remote_join_with_state`] for what is trusted, checked and written.
    ///
    /// If the room already exists here (a user who left and is rejoining, or a second local user
    /// joining a room the first already brought over), the response is applied to the existing
    /// actor. Otherwise an empty shell for the room is registered *first* and the join is applied
    /// through its handle, so that the join's [`RoomUpdate`] -- carrying the user's
    /// `membership_deltas`, which is how `hs-user`'s session hub learns they are in the room --
    /// is published on the global stream from an actor that is already resident and already on
    /// that stream. Applying the join before registering would publish it from an actor nobody
    /// could look up yet: a consumer reacting to the update would load a second copy of the room
    /// from disk, and the two would then diverge. A shell whose join is refused is dropped again
    /// ([`RoomRegistry::drop_if_unbootstrapped`]); nothing durable records it.
    ///
    /// Returns the room's handle, whether the join was newly stored or already known.
    ///
    /// # Errors
    /// Any error [`RoomActor::accept_remote_join_with_state`], `RoomActor::empty_for` or
    /// [`RoomRegistry::get_or_load`] can return.
    pub async fn bootstrap_from_remote_join(
        &self,
        room_id: &ruma::RoomId,
        room_version: ruma::RoomVersionId,
        state: Vec<hs_model::Event>,
        auth_chain: Vec<hs_model::Event>,
        join_event: hs_model::Event,
    ) -> Result<RoomActorHandle<B>, RoomError> {
        let handle = match self.get_or_load(room_id).await {
            Ok(handle) => handle,
            Err(RoomError::RoomNotFound(_)) => {
                let backend = self.backend.clone();
                let tables = self.tables.clone();
                let identity = self.identity.clone();
                let owned_room_id = room_id.to_owned();
                let shell = tokio::task::spawn_blocking(move || {
                    RoomActor::empty_for(backend, tables, identity, &owned_room_id, room_version)
                })
                .await
                .expect("room shell task panicked")?;
                self.insert_if_absent(shell).await
            }
            Err(e) => return Err(e),
        };
        match handle
            .accept_remote_join_with_state(state, auth_chain, join_event)
            .await
        {
            Ok(_) => Ok(handle),
            Err(e) => {
                self.drop_if_unbootstrapped(room_id, &handle).await;
                Err(e)
            }
        }
    }

    /// Records a membership event for one of this server's users in a room this server is not
    /// in -- an invite from another server, the leave or ban that ends it, or the user's own
    /// leave or knock made through a resident. See
    /// [`crate::actor::RoomActor::accept_out_of_room_membership`] for what is checked and how
    /// it is held. A room not held here at all is created for it, the way
    /// [`RoomRegistry::bootstrap_from_remote_join`] creates one: an empty shell registered
    /// first, so the event's [`RoomUpdate`] (how `hs-user` learns of the invite) is published
    /// from an actor that is already resident, and dropped again if the event is refused.
    ///
    /// # Errors
    /// Any error `accept_out_of_room_membership`, `RoomActor::empty_for` or
    /// [`RoomRegistry::get_or_load`] can return.
    pub async fn accept_out_of_room_membership(
        &self,
        room_id: &ruma::RoomId,
        room_version: ruma::RoomVersionId,
        event: hs_model::Event,
    ) -> Result<RoomActorHandle<B>, RoomError> {
        let handle = match self.get_or_load(room_id).await {
            Ok(handle) => handle,
            Err(RoomError::RoomNotFound(_)) => {
                let backend = self.backend.clone();
                let tables = self.tables.clone();
                let identity = self.identity.clone();
                let owned_room_id = room_id.to_owned();
                let shell = tokio::task::spawn_blocking(move || {
                    RoomActor::empty_for(backend, tables, identity, &owned_room_id, room_version)
                })
                .await
                .expect("room shell task panicked")?;
                self.insert_if_absent(shell).await
            }
            Err(e) => return Err(e),
        };
        match handle.accept_out_of_room_membership(event).await {
            Ok(_) => Ok(handle),
            Err(e) => {
                self.drop_if_unbootstrapped(room_id, &handle).await;
                Err(e)
            }
        }
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
        Self::note_residents(&rooms);
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

    /// Drops `room_id`'s resident actor, if there is one, whatever its idle time: what a room
    /// deletion does once the room's records are gone, so the next access finds it missing.
    pub async fn forget_resident(&self, room_id: &ruma::RoomId) {
        let mut rooms = self.rooms.lock().await;
        rooms.remove(room_id);
        Self::note_residents(&rooms);
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

    /// A scripted [`hs_cluster::Ownership`] whose fence can be switched: what a replica holds
    /// for a shard across losing it to a peer and getting it back.
    struct SwitchableFence {
        me: hs_cluster::ReplicaId,
        fence: std::sync::Mutex<Option<hs_cluster::Fence>>,
    }

    impl SwitchableFence {
        fn set(&self, fence: Option<hs_cluster::Fence>) {
            *self.fence.lock().unwrap() = fence;
        }
    }

    impl hs_cluster::Ownership for SwitchableFence {
        fn me(&self) -> &hs_cluster::ReplicaId {
            &self.me
        }

        fn owner_of(&self, _shard: hs_cluster::ShardId) -> Option<hs_cluster::ReplicaId> {
            self.fence.lock().unwrap().map(|_| self.me.clone())
        }

        fn is_mine(&self, _shard: hs_cluster::ShardId) -> bool {
            self.fence.lock().unwrap().is_some()
        }

        fn fence(&self, _shard: hs_cluster::ShardId) -> Option<hs_cluster::Fence> {
            *self.fence.lock().unwrap()
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<hs_cluster::OwnershipEvent> {
            tokio::sync::broadcast::channel(1).1
        }

        fn shard_map(&self) -> tokio::sync::watch::Receiver<Arc<hs_cluster::ShardMap>> {
            tokio::sync::watch::channel(Arc::new(hs_cluster::ShardMap::default())).1
        }
    }

    /// The scale 1 -> 2 bug of 2026-10-09 (`crates/hs-cli/tests/cluster_rejoin.rs` is the same
    /// on real replicas): a copy loaded while this replica owned the shard is handed out again
    /// while it still does, and loaded again from the store once the shard has changed hands
    /// -- a peer took it (and wrote to the room) and this replica got it back at a new epoch.
    /// The copy handed out then holds the peer's event, and writes after it.
    #[tokio::test]
    async fn a_copy_is_loaded_again_once_its_shard_changed_hands() {
        use hs_cluster::{Fence, Generation, ReplicaId, ShardId, ShardKind};
        let backend = MemoryBackend::new();
        let cluster_store = hs_cluster::store::ClusterStore::open(backend.clone()).unwrap();
        let identity = HomeserverIdentity::for_tests("registry.test");
        let registry = Arc::new(RoomRegistry::open(backend.clone(), identity.clone()).unwrap());
        let (me, peer) = (ReplicaId::new("hs-a"), ReplicaId::new("hs-b"));
        // Every room hashes to this one shard's fence here; `Fence::check` reads its row.
        let shard = ShardId::new(ShardKind::Room, 0);
        let ownership = Arc::new(SwitchableFence {
            me: me.clone(),
            fence: std::sync::Mutex::new(None),
        });
        registry.install_fencing(Arc::new(crate::fencing::RoomFencing {
            ownership: ownership.clone(),
            layout: hs_cluster::ShardLayout::small(4),
            cluster_store: cluster_store.clone(),
        }));
        let alice = user_id!("@alice:registry.test").to_owned();

        // Owned: the room is made, and handed out again as the same copy.
        let first = cluster_store
            .acquire_shard(shard, &me, Generation(1), |_| false)
            .unwrap()
            .unwrap();
        ownership.set(Some(Fence::clustered(shard, first.epoch)));
        let created = registry
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        let room_id = created.query(|actor| actor.room_id().to_owned()).await;
        let again = registry.get_or_load(&room_id).await.unwrap();
        assert!(
            again.same_actor(&created),
            "the owner's copy is handed out again"
        );
        let before = crate::metrics::stale_copies_reloaded();

        // Lost: a peer takes the shard and writes to the room. This replica's copy knows
        // nothing of it.
        ownership.set(None);
        let taken = cluster_store
            .acquire_shard(shard, &peer, Generation(1), |_| true)
            .unwrap()
            .unwrap();
        let mut peers_copy = RoomActor::load(
            backend.clone(),
            Tables::open(&backend).unwrap(),
            identity,
            &room_id,
        )
        .unwrap()
        .unwrap();
        peers_copy.set_fencing(Some(Arc::new(crate::fencing::RoomFencing {
            ownership: Arc::new(SwitchableFence {
                me: peer.clone(),
                fence: std::sync::Mutex::new(Some(Fence::clustered(shard, taken.epoch))),
            }),
            layout: hs_cluster::ShardLayout::small(4),
            cluster_store: cluster_store.clone(),
        })));
        let peers_event = peers_copy
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "from the peer"}),
                None,
                2,
            )
            .unwrap();
        let peers_position = peers_copy
            .timeline_position(peers_event.event_id())
            .unwrap();

        // Back, at a new epoch: the next request gets a copy loaded from the store, which
        // holds the peer's event, and the next event lands after it.
        let back = cluster_store
            .acquire_shard(shard, &me, Generation(2), |_| true)
            .unwrap()
            .unwrap();
        assert_ne!(back.epoch, first.epoch);
        ownership.set(Some(Fence::clustered(shard, back.epoch)));
        let reloaded = registry.get_or_load(&room_id).await.unwrap();
        assert!(
            !reloaded.same_actor(&created),
            "a copy from before the shard changed hands must not be handed out"
        );
        assert_eq!(crate::metrics::stale_copies_reloaded(), before + 1);
        assert_eq!(registry.resident_count().await, 1);
        let peers_event_id = peers_event.event_id().to_owned();
        let seen_at = reloaded
            .query(move |actor| actor.timeline_position(&peers_event_id))
            .await;
        assert_eq!(
            seen_at,
            Some(peers_position),
            "the reloaded copy holds the peer's event"
        );
        let ours = reloaded
            .send_event(
                alice,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "after getting the shard back"}),
                None,
                3,
            )
            .await
            .unwrap();
        let ours_id = ours.event_id().to_owned();
        let ours_at = reloaded
            .query(move |actor| actor.timeline_position(&ours_id))
            .await;
        assert_eq!(ours_at, Some(peers_position + 1));
        // And while the shard stays with this replica, the copy is handed out again.
        let same = registry.get_or_load(&room_id).await.unwrap();
        assert!(same.same_actor(&reloaded));
        assert_eq!(crate::metrics::stale_copies_reloaded(), before + 1);
    }

    /// A join through another server counts as under way while any guard for it is held, and
    /// only for its own room.
    #[test]
    fn a_remote_join_is_under_way_while_its_guard_is_held() {
        let registry = registry();
        let room = ruma::room_id!("!joining:elsewhere.test");
        let other = ruma::room_id!("!other:elsewhere.test");
        assert!(!registry.remote_join_in_progress(room));
        let first = registry.remote_join_started(room);
        let second = registry.remote_join_started(room);
        assert!(registry.remote_join_in_progress(room));
        assert!(!registry.remote_join_in_progress(other));
        drop(first);
        assert!(registry.remote_join_in_progress(room));
        drop(second);
        assert!(!registry.remote_join_in_progress(room));
    }

    /// A room ID is the create event's hash from room version 12, so one user creating two rooms
    /// with the same request in the same millisecond derived one ID twice, and the second room
    /// was written over the first (Sytest's "GET /publicRooms lists rooms": two of its five rooms
    /// came back with one ID and each other's settings). Each creation gets a room of its own.
    #[tokio::test]
    async fn two_identical_creations_in_one_millisecond_make_two_rooms() {
        let registry = registry();
        let alice = user_id!("@alice:registry.test");
        let mut rooms = Vec::new();
        for topic in ["first", "second", "third"] {
            let handle = registry
                .create_room(
                    alice.to_owned(),
                    CreateRoomRequest {
                        room_version: Some(ruma::RoomVersionId::V12),
                        ..Default::default()
                    },
                    7,
                )
                .await
                .expect("create should succeed");
            handle
                .send_event(
                    alice.to_owned(),
                    "m.room.topic".to_owned(),
                    Some(String::new()),
                    serde_json::json!({"topic": topic}),
                    None,
                    8,
                )
                .await
                .unwrap();
            rooms.push(handle.query(|a| a.room_id().to_owned()).await);
        }
        let distinct: std::collections::HashSet<_> = rooms.iter().collect();
        assert_eq!(distinct.len(), 3, "{rooms:?}");
        for (room_id, topic) in rooms.iter().zip(["first", "second", "third"]) {
            let handle = registry.get_or_load(room_id).await.unwrap();
            let held = handle
                .query(|a| {
                    a.state_event("m.room.topic", "")
                        .unwrap()
                        .and_then(|e| e.json().get("content").cloned())
                        .and_then(|c| {
                            c.as_object()
                                .and_then(|o| o.get("topic"))
                                .and_then(|t| t.as_str().map(str::to_owned))
                        })
                })
                .await;
            assert_eq!(held.as_deref(), Some(topic));
        }
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
