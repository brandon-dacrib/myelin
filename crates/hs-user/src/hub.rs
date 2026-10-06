//! [`SessionHub`]: turns `hs-room`'s [`hs_room::protocol::RoomUpdate`] publish stream into every
//! affected user's durable feed (`crate::store`), and wakes their long-polling `/sync` calls.
//!
//! # The hybrid fan-out threshold
//!
//! `PLAN.md` section 6.6: "Fan-out on write to local members is cheap on any server except at
//! matrix.org scale; rooms above a configurable local-member threshold switch to fan-out on
//! read." [`SessionHub::process_room_update`] is where that switch happens:
//! [`SessionHub::fan_out_threshold`] members or fewer, every active member gets a
//! [`crate::store::UserStore::append_feed_entry`] call per update (fan-out on write); above the
//! threshold, this hub records the room as `hot` (`crate::store::MembershipRecord::hot_room`) and
//! *stops* writing feed entries for it. Instead each update to a hot room is one entry on the
//! server-wide hot-room stream ([`crate::store::UserStore::append_hot_position`]: the room and
//! its new position, one write however many members it has), and `crate::sync` reads a hot
//! room's position as of a token from there (fan-out on read). A room's `hot`-ness is
//! recomputed on every update from its live member count, so it can flip in either direction
//! without an explicit migration step.
//!
//! # The discovery gap
//!
//! This hub only ever learns about a room's updates once something calls
//! [`SessionHub::watch_room`] for it. In a real deployment, *every* room this process's
//! `hs_room::registry::RoomRegistry` creates or loads needs that call made for it the moment the
//! registry hands back a fresh handle -- otherwise a user's first-ever invite to a room this
//! process has not been told to watch never reaches their feed. `hs-room`'s registry does not
//! expose a "notify me of every room" hook today (its own `insert`/`get_or_load` are the natural
//! place for one, but that is track 04's crate, not this one -- see
//! `crate::room_source`'s module docs and `docs/rfcs/0011-room-registry-global-updates.md`, which
//! this crate's own status file also points at from "Interfaces needed"). Until that lands, the
//! integration layer (`hs-cli`, or these tests) must call [`SessionHub::watch_room`] itself for
//! every room as it is created or loaded -- see `docs/status/05-sync.md` for the exact,
//! near-mechanical addition this implies for `crates/hs-cli/src/serve.rs`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hs_kv::KvBackend;
use hs_model::Event;
use hs_push::counts::CountsStore;
use hs_push::rulesets::CachedRulesetStore;
use hs_push::rulesets::tables::TablesRulesetStore;
use hs_room::protocol::RoomUpdate;
use ruma::{OwnedUserId, RoomId, UserId};
use tokio::sync::{Mutex, Notify};

use crate::cluster::{
    ClusterLink, EphemeralUpdate, RoomMirror, RoomWake, SessionCluster, WakeBatch,
};
use crate::edu::{EduOutbox, InboundEdu};
use crate::error::UserError;
use crate::presence::PresenceRegistry;
use crate::receipts::{ReceiptKind, ReceiptRegistry};
use crate::room_source::RoomSource;
use crate::store::{DynUserStore, FanOutWrite};
use crate::token::SyncToken;
use crate::typing::TypingRegistry;

/// How long [`SessionHub::settle_before_read`] gives the peers to answer with their positions:
/// one mesh round trip, not a wait for anything to happen. Well inside the whole read budget so
/// a slow peer still leaves time for its wakes to land.
const PEER_POSITIONS_DEADLINE: Duration = Duration::from_millis(250);
/// How often the mirror's idle sweeper runs, and how long a snapshot may go unread before it
/// is dropped. A dropped snapshot costs one reload on the next read; a kept one costs memory.
const MIRROR_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const MIRROR_MAX_IDLE: Duration = Duration::from_secs(600);

/// How long [`SessionHub::receive_wakes`] waits for the mirror to catch up on the rooms a batch
/// names before it wakes the batch's users anyway (decision 0022). Well under the mesh's wake
/// deadline (`hs-cli`'s `sync_cluster`, two seconds).
const PREFETCH_WAIT: Duration = Duration::from_millis(250);

/// How many coalesced feed entries a user keeps by default
/// ([`SessionHub::set_retention`]): `hs-config`'s `server.sync.feed_retention_entries` default.
/// A feed grows by at most one entry per room between two of the user's syncs (the coalescing
/// rule, `crate::store::tables`), so this is thousands of syncs' worth for a client that keeps
/// up, and a device whose token falls behind it is sent its rooms again rather than losing
/// anything.
pub const DEFAULT_FEED_RETENTION_ENTRIES: u64 = 10_000;

/// How many entries the server-wide hot-room stream keeps by default
/// (`server.sync.hot_room_stream_retention_entries`): one per update to a room above the
/// fan-out threshold, server-wide.
pub const DEFAULT_HOT_STREAM_RETENTION_ENTRIES: u64 = 100_000;

/// Set in the environment, makes the hub write a room update's fan-out one member at a time,
/// one or two store round trips each, as it did before the batched
/// [`crate::store::UserStore::apply_fan_out`]: the baseline of the measurement in
/// `docs/status/05-sync.md` (session 14), and an escape hatch.
pub const FAN_OUT_UNBATCHED_ENV: &str = "HS_SYNC_FAN_OUT_UNBATCHED";

pub(crate) fn membership_of(event: &Event) -> Option<String> {
    event
        .json()
        .get("content")
        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        .and_then(|c| c.get("membership"))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
        .map(str::to_owned)
}

fn is_active(membership: &str) -> bool {
    matches!(membership, "join" | "invite" | "knock")
}

/// What [`SessionHub::fan_out`] wrote.
struct FanOut {
    report: crate::store::FanOutReport,
    /// Records rewritten because the room crossed the fan-out threshold.
    flipped: usize,
}

fn content_str(event: &Event, field: &str) -> Option<String> {
    event
        .json()
        .get("content")
        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        .and_then(|c| c.get(field))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
        .map(str::to_owned)
}

/// Builds this room's [`crate::store::PublicRoomEntry`] if it is currently public
/// (`m.room.join_rules`'s `join_rule` is `"public"`) or world-readable (`m.room.history_visibility`
/// is `world_readable`; such a row has `join_rule_public: false` and is for the user directory
/// only), `None` otherwise -- the caller then removes any existing directory entry in the `None`
/// case (a room can stop being public).
fn public_directory_entry<B: KvBackend>(
    actor: &hs_room::actor::RoomActor<B>,
) -> Result<Option<crate::store::PublicRoomEntry>, hs_room::RoomError> {
    // One helper for the shape repeated eight times below: read a state event's `content.<field>`
    // as a string, where "the event is absent" and "the field is absent" are both `None` but a
    // state-store failure propagates.
    let field = |event_type: &str, name: &str| -> Result<Option<String>, hs_room::RoomError> {
        Ok(actor
            .state_event(event_type, "")?
            .and_then(|e| content_str(e, name)))
    };

    let join_rule_public = field("m.room.join_rules", "join_rule")?.as_deref() == Some("public");
    let world_readable = field("m.room.history_visibility", "history_visibility")?.as_deref()
        == Some("world_readable");
    if !join_rule_public && !world_readable {
        return Ok(None);
    }
    let guest_can_join =
        field("m.room.guest_access", "guest_access")?.as_deref() == Some("can_join");
    Ok(Some(crate::store::PublicRoomEntry {
        room_id: actor.room_id().to_owned(),
        join_rule_public,
        name: field("m.room.name", "name")?,
        topic: field("m.room.topic", "topic")?,
        canonical_alias: field("m.room.canonical_alias", "alias")?,
        avatar_url: field("m.room.avatar", "url")?,
        num_joined_members: actor.joined_members()?.len(),
        world_readable,
        guest_can_join,
    }))
}

/// Implements [`hs_room::registry::GlobalTokenResolver`] by decoding a raw string as this crate's
/// own [`SyncToken`] and resolving its `feed_seq` via
/// [`crate::store::UserStore::room_pos_as_of`] -- the exact lookup `crate::sync::resume_mode`
/// uses to resume an incremental sync's own timeline from the same token, so pagination and
/// `/sync` resumption agree on what a given token means. Falls back to the user's last recorded
/// membership-changing position for the room (mirroring `resume_mode`'s own fallback for a room
/// with no feed entry at or before the token -- a brand new room, or one that has been "hot",
/// this module's own doc comment, for as long as the user has been a member) before finally
/// reporting "recognized, but no position" (see [`hs_room::registry::GlobalTokenResolver::resolve`]'s
/// three-way return). Installed by [`SessionHub::new`]; see that constructor's doc comment for
/// why installation happens there rather than in `hs-cli`.
struct FeedTokenResolver {
    store: DynUserStore,
}

#[async_trait::async_trait]
impl hs_room::registry::GlobalTokenResolver for FeedTokenResolver {
    async fn resolve(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        raw: &str,
    ) -> Result<Option<Option<i64>>, hs_room::RoomError> {
        let Ok(token) = SyncToken::decode(raw) else {
            return Ok(None); // Not one of ours either -- let the caller report the real error.
        };
        let to_internal = |e: crate::store::StoreError| hs_room::RoomError::Internal(e.to_string());
        // The feed's position and the hot-room stream's, whichever is newer: the same baseline
        // `crate::sync::resume_mode` takes.
        let from_feed = self
            .store
            .room_pos_as_of(user_id, room_id, token.feed_seq)
            .await
            .map_err(to_internal)?;
        let from_hot = self
            .store
            .hot_room_pos_as_of(room_id, token.hot_seq)
            .await
            .map_err(to_internal)?;
        if let Some(pos) = from_feed.max(from_hot) {
            return Ok(Some(Some(pos)));
        }
        if let Some(membership) = self
            .store
            .get_membership(user_id, room_id)
            .await
            .map_err(to_internal)?
            && membership.room_pos > 0
        {
            return Ok(Some(Some(membership.room_pos)));
        }
        Ok(Some(None))
    }
}

/// Implements [`hs_e2e::state::SyncTokenResolver`] for `GET /keys/changes`.
///
/// `resolve_device_list_position` decodes `raw` as this crate's own [`SyncToken`] and reports
/// its `device_list_seq` field directly, with no store lookup at all -- unlike
/// [`FeedTokenResolver`] (which needs `crate::store::UserStore` to turn a `feed_seq` into a
/// room-local position), a device-list stream position *is* one of the token's own fields
/// verbatim, so decoding the token answers the question outright. `user_id` is unused there (a
/// `SyncToken` carries no user scope of its own; the caller already knows whose token this is
/// from the authenticated request).
///
/// `device_list_changes_between` is the whole answer: the device-list stream between the two
/// tokens *and* the membership walk `/sync` makes between them
/// ([`crate::sync::device_lists::changes_between`]), which is what puts a user who merely
/// started sharing a room into `changed` and anyone into `left`.
///
/// Installed by [`SessionHub::install_device_list_token_resolver`] -- see that method's doc
/// comment for why installation is a separate call rather than a side effect of
/// [`SessionHub::new`] the way [`FeedTokenResolver`] is (this one needs an `E2eState` handle that
/// `new` does not take).
struct DeviceListTokenResolver<B: KvBackend, R: RoomSource<B>> {
    hub: Arc<SessionHub<B, R>>,
    e2e: Arc<dyn hs_e2e::store::E2eStore>,
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static, R: RoomSource<B> + 'static> hs_e2e::state::SyncTokenResolver
    for DeviceListTokenResolver<B, R>
{
    async fn resolve_device_list_position(
        &self,
        _user_id: &UserId,
        raw: &str,
    ) -> Result<Option<u64>, hs_e2e::error::E2eError> {
        Ok(SyncToken::decode(raw).ok().map(|t| t.device_list_seq))
    }

    async fn device_list_changes_between(
        &self,
        user_id: &UserId,
        from: &str,
        to: Option<&str>,
    ) -> Result<Option<hs_e2e::state::DeviceListChanges>, hs_e2e::error::E2eError> {
        let Ok(from) = SyncToken::decode(from) else {
            return Ok(None);
        };
        let to = match to {
            Some(raw) => match SyncToken::decode(raw) {
                Ok(token) => Some(token),
                Err(_) => return Ok(None),
            },
            None => None,
        };
        crate::sync::device_lists::changes_between(
            &self.hub,
            &self.e2e,
            user_id,
            &from,
            to.as_ref(),
        )
        .await
        .map(Some)
        .map_err(|error| hs_e2e::store::StoreError::Backend(error.to_string()).into())
    }
}

/// What [`SessionHub::install_remote_device_lists`] installs: where the copies of other servers'
/// users' device lists are kept, and which server this is, so the hub can tell a remote user
/// from a local one.
struct RemoteDeviceLists {
    store: Arc<dyn hs_e2e::store::RemoteDeviceListStore>,
    own_server: ruma::OwnedServerName,
}

/// The per-process hub: one [`crate::store::UserStore`] shared by every user, a [`RoomSource`]
/// for querying room member lists, and the in-memory wakers `/sync` long-polls block on.
///
/// What [`SessionHub::install_ephemeral_observer`] installs: told of every typing, receipt and
/// presence change the hub applies, from its own clients and from other replicas. Must return
/// at once (it is called on the request path); a slow consumer takes a note and does its work
/// on its own task.
pub trait EphemeralObserver: Send + Sync {
    /// A change was applied here. For a receipt or presence hint the data is already in the
    /// store; for typing, in this hub's [`crate::typing::TypingRegistry`]
    /// ([`SessionHub::typing_users`]).
    fn ephemeral_changed(&self, update: &EphemeralUpdate);
}

/// Deliberately holds no per-user in-memory *state* beyond the wakers -- everything a sync
/// response needs comes from `store` (`PLAN.md` section 5.4: "Everything the user session holds
/// is derivable from room positions and the feed"), so this hub is cheap to reconstruct after a
/// restart: a fresh one, over the same store, picks up exactly where the old one left off.
pub struct SessionHub<B: KvBackend, R: RoomSource<B>> {
    store: DynUserStore,
    rooms: R,
    /// Member count above which a room stops receiving per-write feed entries. See the module
    /// docs, "The hybrid fan-out threshold". `usize::MAX` disables the hot path entirely (every
    /// room is always fanned out on write) -- useful for tests that want to reason about the
    /// feed alone.
    fan_out_threshold: usize,
    wakers: Mutex<HashMap<OwnedUserId, Arc<Notify>>>,
    /// The `global_seq` of the last update [`SessionHub::consume_updates`] finished processing
    /// from the registry's global stream. See [`SessionHub::wait_for_consumed`].
    consumed: tokio::sync::watch::Sender<u64>,
    /// Per peer (`replica#generation`), the highest consumed mark its wake batches have carried
    /// here. See [`SessionHub::receive_wakes`] and [`SessionHub::settle_before_read`]. A
    /// `std::sync::Mutex`: every critical section is a map lookup with no `.await` inside.
    peer_consumed: std::sync::Mutex<HashMap<String, tokio::sync::watch::Sender<u64>>>,
    /// The cluster this hub is part of, if any -- see [`SessionHub::install_cluster`]. `None`
    /// (single-node mode, and every test that does not install one) means every room is this
    /// hub's to feed and read through the registry, exactly as before the link existed.
    cluster: OnceLock<ClusterLink<B>>,
    /// Set once, by [`SessionHub::begin_shutdown`]: the server is stopping, and a `/sync` that is
    /// waiting for news should stop waiting.
    shutting_down: std::sync::atomic::AtomicBool,
    /// In-memory `m.typing` state. See [`crate::typing`]'s module docs for why this lives here
    /// rather than in `store`: ephemeral, never persisted, and this hub is already the one place
    /// that both knows how to reach a room's member list and owns the wakers a change needs to
    /// touch.
    typing: Arc<TypingRegistry>,
    /// Where this server's own users' typing, receipts and presence go to reach other servers,
    /// once `hs-cli` installs it ([`SessionHub::install_edu_outbox`]). `None`: nothing leaves
    /// this server, which is what federation being off means.
    edu_outbox: OnceLock<Arc<dyn EduOutbox>>,
    /// The copies of remote users' device lists this server keeps, once `hs-cli` installs them
    /// ([`SessionHub::install_remote_device_lists`]), so a membership change that ends the last
    /// room a remote user shares with this server marks their copy stale. `None`: no copies are
    /// kept (federation off).
    remote_device_lists: OnceLock<RemoteDeviceLists>,
    /// Told of every local user's read receipt, so push counts reset and badges update
    /// (`hs_push::pipeline`), once installed ([`SessionHub::install_read_receipt_sink`]).
    read_receipt_sink: OnceLock<Arc<dyn hs_push::pipeline::ReadReceiptSink>>,
    /// Told of every typing, receipt and presence change this hub applies -- its own users' and
    /// other replicas' alike -- once installed ([`SessionHub::install_ephemeral_observer`]).
    /// `None`: nobody is listening, which is the default.
    ephemeral_observer: OnceLock<Arc<dyn EphemeralObserver>>,
    /// `m.presence` state, written through to `store`. See [`crate::presence`]'s module docs.
    presence: PresenceRegistry,
    /// `m.receipt` state, written through to `store`. See [`crate::receipts`]'s module docs.
    receipts: ReceiptRegistry,
    /// `hs-push`'s cached ruleset store, if installed (see
    /// [`SessionHub::install_push_rules_store`]). `None` until installed -- `/sync` then omits
    /// `m.push_rules` entirely, exactly today's (pre-push-rules) behavior, rather than failing.
    push_rules: OnceLock<Arc<CachedRulesetStore<TablesRulesetStore<B>>>>,
    /// `hs-push`'s notification-count store, if installed (see
    /// [`SessionHub::install_counts_store`]). `None` until installed -- `/sync` then reports
    /// `unread_notifications`/`unread_thread_notifications` as the hard-zero placeholder it
    /// always has, rather than failing.
    counts: OnceLock<Arc<dyn CountsStore>>,
    /// The account store, if installed ([`SessionHub::install_account_store`]): whom `/sync`
    /// asks whether a sender's account was erased. `None` until installed -- nothing is pruned.
    accounts: OnceLock<Arc<dyn hs_auth::store::AuthStore>>,
    /// How many rooms a user-directory search has had to read whole, because the directory
    /// index had nothing for them yet ([`SessionHub::directory_rooms_walked`]).
    directory_walks: std::sync::atomic::AtomicU64,
    /// How many coalesced feed entries a user keeps ([`SessionHub::set_retention`]); `0` keeps
    /// them all.
    feed_retention: std::sync::atomic::AtomicU64,
    /// How many entries the hot-room stream keeps ([`SessionHub::set_retention`]); `0` keeps
    /// them all.
    hot_stream_retention: std::sync::atomic::AtomicU64,
    /// The hot-room stream position this hub last compacted the stream at: the next compaction
    /// is due twice the retention past it. In memory only; a fresh hub checks once.
    hot_compacted_at: std::sync::atomic::AtomicU64,
    /// Whether [`FAN_OUT_UNBATCHED_ENV`] is set: each member written on their own.
    fan_out_unbatched: bool,
    /// Which members each device was sent under lazy loading (`crate::lazy_members`).
    lazy_members: crate::lazy_members::LazyMembersSent,
    _marker: std::marker::PhantomData<fn() -> B>,
}

impl<B: KvBackend + 'static, R: RoomSource<B>> SessionHub<B, R> {
    /// Builds a hub over `store` (durable state) and `rooms` (how to reach a room's actor).
    ///
    /// As a side effect, installs a [`FeedTokenResolver`] on `rooms` via
    /// [`RoomSource::install_global_token_resolver`] (a no-op for any `R` that doesn't override
    /// it) -- see that trait method and [`hs_room::registry::GlobalTokenResolver`]'s doc comment
    /// for why this is done *here*, as a side effect of this crate's own, already-unchanged
    /// construction path, rather than as a step `hs-cli` has to remember to call: it makes a
    /// token minted by this crate's `/sync` work as `hs-room`'s `GET /messages`'s `from` in the
    /// real server without either crate's wiring code (in `hs-cli`) needing to change.
    #[must_use]
    pub fn new(store: DynUserStore, rooms: R, fan_out_threshold: usize) -> Self {
        rooms.install_global_token_resolver(Arc::new(FeedTokenResolver {
            store: store.clone(),
        }));
        let presence = PresenceRegistry::with_store(store.clone());
        let receipts = ReceiptRegistry::with_store(store.clone());
        let fan_out_unbatched = std::env::var_os(FAN_OUT_UNBATCHED_ENV).is_some_and(|v| v == "1");
        if fan_out_unbatched {
            tracing::warn!(
                "{FAN_OUT_UNBATCHED_ENV}=1: room updates are fanned out one member at a time"
            );
        }
        Self {
            store,
            rooms,
            fan_out_threshold,
            wakers: Mutex::new(HashMap::new()),
            consumed: tokio::sync::watch::channel(0).0,
            peer_consumed: std::sync::Mutex::new(HashMap::new()),
            cluster: OnceLock::new(),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            typing: Arc::new(TypingRegistry::new()),
            edu_outbox: OnceLock::new(),
            remote_device_lists: OnceLock::new(),
            read_receipt_sink: OnceLock::new(),
            ephemeral_observer: OnceLock::new(),
            presence,
            receipts,
            push_rules: OnceLock::new(),
            counts: OnceLock::new(),
            accounts: OnceLock::new(),
            directory_walks: std::sync::atomic::AtomicU64::new(0),
            feed_retention: std::sync::atomic::AtomicU64::new(DEFAULT_FEED_RETENTION_ENTRIES),
            hot_stream_retention: std::sync::atomic::AtomicU64::new(
                DEFAULT_HOT_STREAM_RETENTION_ENTRIES,
            ),
            hot_compacted_at: std::sync::atomic::AtomicU64::new(0),
            fan_out_unbatched,
            lazy_members: crate::lazy_members::LazyMembersSent::default(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Sets how much of the feeds and the hot-room stream this hub keeps: `feed_entries`
    /// coalesced entries per user and `hot_stream_entries` on the server-wide hot-room stream,
    /// `0` for all of them. Read on every room update, so a change applies at once
    /// (`hs-config`'s `server.sync`, hot). Below the kept part each room's last position stays
    /// ([`crate::store::UserStore::compact_feed`]); a device whose token is older than the
    /// kept part is sent its rooms again, never less. A compaction runs when a feed (or the
    /// stream) has grown to twice its retention since the last one, so a feed is between one
    /// and two retentions long.
    pub fn set_retention(&self, feed_entries: u64, hot_stream_entries: u64) {
        self.feed_retention
            .store(feed_entries, std::sync::atomic::Ordering::Relaxed);
        self.hot_stream_retention
            .store(hot_stream_entries, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many coalesced feed entries a user keeps ([`SessionHub::set_retention`]).
    #[must_use]
    pub fn feed_retention(&self) -> u64 {
        self.feed_retention
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many entries the hot-room stream keeps ([`SessionHub::set_retention`]).
    #[must_use]
    pub fn hot_stream_retention(&self) -> u64 {
        self.hot_stream_retention
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The durable store backing this hub, for `crate::sync` and `crate::routes` to read from
    /// directly (this hub does not wrap every read -- only the write path that needs the room
    /// source and the wake-up side effect).
    #[must_use]
    pub fn store(&self) -> &DynUserStore {
        &self.store
    }

    /// The room source backing this hub.
    #[must_use]
    pub fn rooms(&self) -> &R {
        &self.rooms
    }

    /// Installs the `hs-push` ruleset store `/sync` consults for `m.push_rules` account data
    /// (`docs/status/10-push.md`'s "Interfaces provided"). Idempotent past the first call, same
    /// convention as [`hs_room::registry::RoomRegistry::install_global_token_resolver`] and
    /// `hs_e2e::state::E2eState::install_sync_token_resolver`: a second install is logged and
    /// ignored rather than panicking.
    ///
    /// Not called by [`SessionHub::new`] (unlike [`FeedTokenResolver`]'s install) because the
    /// `Arc` this needs is built by `hs-cli`'s `build_session_mounts` *alongside* (not before)
    /// the call that builds this hub -- see `docs/status/05-sync.md` for the exact call site and
    /// line this needs added once `hs-cli` is free to change.
    pub fn install_push_rules_store(&self, store: Arc<CachedRulesetStore<TablesRulesetStore<B>>>) {
        if self.push_rules.set(store).is_err() {
            tracing::warn!("a push-rules store was already installed on this hub; ignoring");
        }
    }

    /// The installed push-rules store, if any -- see [`SessionHub::install_push_rules_store`].
    #[must_use]
    pub fn push_rules_store(&self) -> Option<&Arc<CachedRulesetStore<TablesRulesetStore<B>>>> {
        self.push_rules.get()
    }

    /// Installs the `hs-push` notification-count store `/sync` consults for
    /// `unread_notifications`/`unread_thread_notifications` (`docs/status/10-push.md`'s
    /// "Interfaces provided"). Same idempotent-install convention as
    /// [`SessionHub::install_push_rules_store`].
    pub fn install_counts_store(&self, store: Arc<dyn CountsStore>) {
        if self.counts.set(store).is_err() {
            tracing::warn!("a counts store was already installed on this hub; ignoring");
        }
    }

    /// Installs the account store `/sync` asks whether a sender was erased (an erased local
    /// user's events are shown pruned to whoever was not in the room when they were sent, as
    /// the room's own read paths show them: `hs_room::routes::client_events::finish`). Same
    /// idempotent-install convention as [`SessionHub::install_counts_store`].
    pub fn install_account_store(&self, store: Arc<dyn hs_auth::store::AuthStore>) {
        if self.accounts.set(store).is_err() {
            tracing::warn!("an account store was already installed on this hub; ignoring");
        }
    }

    /// The installed account store, if any -- see [`SessionHub::install_account_store`].
    #[must_use]
    pub fn account_store(&self) -> Option<&Arc<dyn hs_auth::store::AuthStore>> {
        self.accounts.get()
    }

    /// The installed counts store, if any -- see [`SessionHub::install_counts_store`].
    #[must_use]
    pub fn counts_store(&self) -> Option<&Arc<dyn CountsStore>> {
        self.counts.get()
    }

    /// Installs where this server's own users' typing, receipts and presence are handed to reach
    /// other servers (`crate::edu`'s module docs). Same idempotent-install convention as
    /// [`SessionHub::install_push_rules_store`]. `hs-cli` installs one over the federation
    /// sender when federation is on.
    pub fn install_edu_outbox(&self, outbox: Arc<dyn EduOutbox>) {
        if self.edu_outbox.set(outbox).is_err() {
            tracing::warn!("an EDU outbox was already installed on this hub; ignoring");
        }
    }

    /// Installs what is told of every read receipt this hub records
    /// ([`SessionHub::set_receipt`]): the push pipeline, which zeroes the room's unread counts
    /// and sends the user's pushers the new badge. Same idempotent-install convention as
    /// [`SessionHub::install_push_rules_store`]; without one, receipts leave counts alone, the
    /// pre-push behaviour.
    pub fn install_read_receipt_sink(&self, sink: Arc<dyn hs_push::pipeline::ReadReceiptSink>) {
        if self.read_receipt_sink.set(sink).is_err() {
            tracing::warn!("a read receipt sink was already installed on this hub; ignoring");
        }
    }

    /// Installs what is told of every typing, receipt and presence change this hub applies: a
    /// change one of this replica's own clients made, or a change another replica made and
    /// sent here in a wake batch ([`SessionHub::apply_ephemeral`]). On every replica together
    /// that is every change on the server, which is what appservice delivery of ephemeral
    /// data (MSC2409, `hs-appservice`'s ephemeral pump, wired by `hs-cli`) needs: typing is in
    /// no store, so the observer is the only way to hear of it, and for receipts and presence
    /// it is the doorbell that spares the pump waiting for its next poll of the store's streams
    /// ([`crate::store::UserStore::receipt_stream_since`]). Same idempotent-install convention
    /// as [`SessionHub::install_edu_outbox`].
    pub fn install_ephemeral_observer(&self, observer: Arc<dyn EphemeralObserver>) {
        if self.ephemeral_observer.set(observer).is_err() {
            tracing::warn!("an ephemeral observer was already installed on this hub; ignoring");
        }
    }

    /// Installs this crate's [`DeviceListTokenResolver`] on `e2e`'s `GET /keys/changes` hook, so
    /// a `from`/`to` value that is one of this crate's own `/sync` tokens (rather than a plain
    /// decimal stream position) resolves instead of `400`ing -- see
    /// [`hs_e2e::state::SyncTokenResolver`]'s doc comment for why this indirection exists and
    /// [`DeviceListTokenResolver`] for why resolving it needs no store access at all.
    ///
    /// Not a side effect of [`SessionHub::new`] for the same reason
    /// [`SessionHub::install_push_rules_store`] isn't: the `E2eState` this needs is built by
    /// `hs-cli`'s `build_session_mounts` as a sibling of this hub, not an input to it -- see
    /// `docs/status/05-sync.md` for the exact call site and line this needs added.
    pub fn install_device_list_token_resolver(self: &Arc<Self>, e2e: &hs_e2e::state::E2eState<B>)
    where
        R: 'static,
    {
        e2e.install_sync_token_resolver(Arc::new(DeviceListTokenResolver {
            hub: Arc::clone(self),
            e2e: e2e.store.clone(),
        }));
    }

    /// Tells this hub where the copies of other servers' users' device lists are kept
    /// (`hs_e2e::store::RemoteDeviceListStore`) and which server this is, so that when a
    /// membership change ends the last room a remote user shares with a user of this server,
    /// their copy is marked stale ([`SessionHub::process_room_update`]). Without this a copy was
    /// only dropped at a `/keys/query` made while no room was shared: a user who left, changed a
    /// device and came back with no query in between was served the old list. Same idempotent
    /// install convention as [`SessionHub::install_edu_outbox`].
    pub fn install_remote_device_lists(
        &self,
        store: Arc<dyn hs_e2e::store::RemoteDeviceListStore>,
        own_server: ruma::OwnedServerName,
    ) {
        if self
            .remote_device_lists
            .set(RemoteDeviceLists { store, own_server })
            .is_err()
        {
            tracing::warn!("remote device lists were already installed on this hub; ignoring");
        }
    }

    /// After `update`'s membership records are written: every remote user a leave or ban in it
    /// may have taken the last shared room from -- the leaver themself if they are remote, or
    /// every remote member of the room if a local user left -- whose device list this server
    /// holds a copy of and who shares no room with a local user any more, has that copy marked
    /// stale, so the next `/keys/query` (after they come back, say) fetches it again. Nothing
    /// without [`SessionHub::install_remote_device_lists`]; a failure is logged, never the
    /// update's.
    async fn mark_unshared_remote_lists_stale(
        &self,
        update: &RoomUpdate,
        targets: &HashMap<OwnedUserId, String>,
    ) {
        let Some(link) = self.remote_device_lists.get() else {
            return;
        };
        let mut candidates: std::collections::BTreeSet<OwnedUserId> =
            std::collections::BTreeSet::new();
        for delta in &update.membership_deltas {
            if !matches!(delta.membership.as_str(), "leave" | "ban") {
                continue;
            }
            if delta.user_id.server_name() == link.own_server {
                candidates.extend(
                    targets
                        .keys()
                        .filter(|user| user.server_name() != link.own_server)
                        .cloned(),
                );
            } else {
                candidates.insert(delta.user_id.clone());
            }
        }
        for user_id in candidates {
            // The cheap question first: most remote users have no copy here at all.
            match link.store.get_remote_user(&user_id).await {
                Ok(Some(_)) => {}
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(%user_id, %error, "cannot read whether a remote user's device list is held");
                    continue;
                }
            }
            let shares = match self.users_sharing_room_with(&user_id).await {
                Ok(users) => users.iter().any(|u| u.server_name() == link.own_server),
                Err(error) => {
                    tracing::warn!(%user_id, %error, "cannot work out whether a remote user still shares a room here");
                    continue;
                }
            };
            if shares {
                continue;
            }
            match link.store.mark_remote_user_stale(&user_id).await {
                Ok(()) => tracing::info!(
                    %user_id,
                    room_id = %update.room_id,
                    "a remote user shares no room here any more; the copy of their device list is stale until they do"
                ),
                Err(error) => {
                    tracing::warn!(%user_id, %error, "cannot mark a remote user's device list stale");
                }
            }
        }
    }

    /// Makes this hub one replica of a cluster (`crate::cluster`'s module docs): rooms this
    /// replica does not own are read through `mirror` and fed by their owners, every update this
    /// hub feeds is announced to the other replicas through `cluster`, and `/sync` waits for the
    /// peers' positions before it reads. Same idempotent-install convention as
    /// [`SessionHub::install_push_rules_store`]. Also spawns the mirror's idle sweeper.
    ///
    /// Not a constructor parameter because `hs-cli` starts the cluster after it has built this
    /// hub; a hub with nothing installed is a single-node hub.
    pub fn install_cluster(
        self: &Arc<Self>,
        cluster: Arc<dyn SessionCluster>,
        mirror: Arc<RoomMirror<B>>,
    ) where
        R: 'static,
    {
        let sweeper = Arc::clone(&mirror);
        if self.cluster.set(ClusterLink { cluster, mirror }).is_err() {
            tracing::warn!("a session cluster was already installed on this hub; ignoring");
            return;
        }
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(MIRROR_SWEEP_INTERVAL);
            loop {
                interval.tick().await;
                sweeper.evict_idle(MIRROR_MAX_IDLE).await;
            }
        });
    }

    /// Whether this hub is the one that feeds `room_id`: always, unless a cluster is installed
    /// and says the room's shard belongs to another replica.
    #[must_use]
    pub fn owns_room(&self, room_id: &RoomId) -> bool {
        self.cluster
            .get()
            .is_none_or(|link| link.cluster.owns_room(room_id))
    }

    /// The actor to read `room_id` through: the registry for a room this replica owns (or when
    /// no cluster is installed), the mirror for any other. Every read this crate makes of a
    /// room goes through here, so that a replica never serves a stale registry copy of a room
    /// another replica is writing to.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room does not exist or could not be loaded.
    pub async fn room(
        &self,
        room_id: &RoomId,
    ) -> Result<hs_room::actor::RoomActorHandle<B>, UserError> {
        match self.cluster.get() {
            Some(link) if !link.cluster.owns_room(room_id) => {
                Ok(link.mirror.get_or_load(room_id).await?)
            }
            _ => Ok(self.rooms.get_or_load(room_id).await?),
        }
    }

    /// Whether `user_id` is a joined member of `room_id`, as the room itself has committed it --
    /// the gate for setting typing state and posting receipts (`crate::routes::typing`,
    /// `crate::routes::receipts`).
    ///
    /// The store's membership record is the cheap answer and the usual one, but it is written by
    /// this hub off the registry's global stream, a moment *after* the room accepted the event.
    /// A client that creates or joins a room and sets typing in the same breath used to be told
    /// it was not a member (`403`), which is what failed CI on a loaded runner. So a record that
    /// does not say `join` is not the last word: the room's own current state is asked next
    /// (what the join wrote, the instant it was accepted), and, should that be a replica's mirror
    /// that is itself a moment behind, this hub then waits -- as `/sync` does
    /// ([`SessionHub::settle_before_read`]), and for at most `at_most` -- to have consumed
    /// everything published before the call, and reads the record again. A room that cannot be
    /// loaded (it does not exist, or was deleted) has no joined members.
    ///
    /// # Errors
    /// Returns [`UserError`] on a store failure, or a room failure other than the room not
    /// existing.
    pub async fn is_joined(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        at_most: Duration,
    ) -> Result<bool, UserError> {
        let store_says_joined = |record: Option<crate::store::MembershipRecord>| {
            record.is_some_and(|m| m.membership == "join")
        };
        if store_says_joined(self.store.get_membership(user_id, room_id).await?) {
            return Ok(true);
        }
        match self.room(room_id).await {
            Ok(handle) => {
                let user = user_id.to_owned();
                let joined = handle
                    .query(move |actor| {
                        actor
                            .state_event("m.room.member", user.as_str())
                            .map(|event| event.and_then(membership_of).as_deref() == Some("join"))
                    })
                    .await?;
                if joined {
                    tracing::debug!(
                        %user_id,
                        %room_id,
                        "membership not yet in the store; the room says joined"
                    );
                    return Ok(true);
                }
            }
            Err(error) if error.is_room_not_found() => return Ok(false),
            Err(error) => return Err(error),
        }
        self.settle_before_read(at_most).await;
        let joined = store_says_joined(self.store.get_membership(user_id, room_id).await?);
        if joined {
            tracing::debug!(
                %user_id,
                %room_id,
                "membership reached the store after waiting for the session hub"
            );
        }
        Ok(joined)
    }

    /// Takes one peer's [`WakeBatch`]: applies its typing, receipt and presence updates
    /// ([`SessionHub::apply_ephemeral`]), advances this replica's copies of the rooms its wakes
    /// name ([`RoomMirror::prefetch`]), wakes every user its wakes name, then records the
    /// sender's consumed mark for [`SessionHub::settle_before_read`]. In that order, so a
    /// `/sync` released by the mark cannot run before the wake that goes with it. Called by the
    /// mesh's peer handler in `hs-cli`; in tests, directly.
    pub async fn receive_wakes(&self, batch: WakeBatch) {
        tracing::debug!(
            from = %batch.from,
            consumed = batch.consumed,
            rooms = batch.wakes.len(),
            users = batch.wakes.iter().map(|w| w.users.len()).sum::<usize>(),
            ephemeral = batch.ephemeral.len(),
            "received a wake batch from a peer"
        );
        for update in batch.ephemeral {
            self.apply_ephemeral(&batch.from, update).await;
        }
        // The copies of the rooms that moved are advanced before anyone is woken, so the
        // long-polls released below read them current rather than each paying for the catch-up
        // (decision 0022). A room this replica owns is never in the mirror. Each catch-up runs
        // as a task of its own and is waited for at most `PREFETCH_WAIT`: a catch-up is a few
        // point reads, but one that turns into a whole reload of a big room must neither hold
        // the peer's request past its deadline (which would cancel it, and these wakes with
        // it) nor delay the wakes; it finishes on its own, and a long-poll that reads the room
        // first waits for it on the room's lock.
        if let Some(link) = self.cluster.get() {
            let mut prefetches = tokio::task::JoinSet::new();
            for wake in &batch.wakes {
                if !link.cluster.owns_room(&wake.room_id) {
                    let mirror = Arc::clone(&link.mirror);
                    let (room_id, room_pos) = (wake.room_id.clone(), wake.room_pos);
                    prefetches.spawn(async move { mirror.prefetch(&room_id, room_pos).await });
                }
            }
            let all = async { while prefetches.join_next().await.is_some() {} };
            if tokio::time::timeout(PREFETCH_WAIT, all).await.is_err() {
                tracing::debug!(
                    from = %batch.from,
                    "room mirror catch-up on a wake is still running; waking without it"
                );
            }
            prefetches.detach_all();
        }
        for wake in &batch.wakes {
            for user in &wake.users {
                self.wake(user).await;
            }
        }
        if batch.consumed > 0 {
            self.advance_peer_consumed(&batch.from, batch.consumed);
        }
    }

    /// Applies one typing, receipt or presence change another replica made
    /// (`crate::cluster`'s module docs, "Typing, receipts and presence"): a typing update goes
    /// into this replica's registry as if the client had sent it here, with its own timeout; a
    /// receipt or presence hint makes the registry forget its cached copy so the next read is
    /// from the store, where the change already is. Then the long-polls concerned are woken,
    /// exactly as the local change would have woken them. Nothing here is published on: the
    /// replica that took the change told every other one.
    ///
    /// A room this replica cannot read (not in the store yet, or unreadable) means nobody here
    /// to wake; the update is still applied, and logged at `debug`.
    pub async fn apply_ephemeral(&self, from: &str, update: EphemeralUpdate) {
        tracing::debug!(
            from,
            kind = update.kind(),
            ?update,
            "applying a peer's ephemeral update"
        );
        if let Some(observer) = self.ephemeral_observer.get() {
            observer.ephemeral_changed(&update);
        }
        match update {
            EphemeralUpdate::Typing {
                room_id,
                user_id,
                typing,
                timeout_ms,
            } => {
                self.typing
                    .set(
                        &room_id,
                        &user_id,
                        typing,
                        Duration::from_millis(timeout_ms),
                    )
                    .await;
                self.wake_room_members(&room_id).await;
            }
            EphemeralUpdate::Receipt { room_id, seq } => {
                self.receipts.forget(&room_id, seq).await;
                self.wake_room_members(&room_id).await;
            }
            EphemeralUpdate::Presence { user_id, seq } => {
                self.presence.forget(&user_id, seq).await;
                self.wake_presence_audience(&user_id).await;
                self.wake(&user_id).await;
            }
        }
    }

    /// Wakes every joined member of `room_id`; a room that cannot be read here wakes nobody
    /// and is logged at `debug`.
    async fn wake_room_members(&self, room_id: &RoomId) {
        match self.joined_member_ids(room_id).await {
            Ok(members) => {
                for member in &members {
                    self.wake(member).await;
                }
            }
            Err(error) => tracing::debug!(
                %room_id,
                %error,
                "a peer's ephemeral update names a room not readable here; nobody to wake"
            ),
        }
    }

    /// Wakes everyone who shares a joined room with `user_id`; logged at `debug` if the walk
    /// fails.
    async fn wake_presence_audience(&self, user_id: &UserId) {
        match self.users_sharing_room_with(user_id).await {
            Ok(audience) => {
                for other in &audience {
                    self.wake(other).await;
                }
            }
            Err(error) => tracing::debug!(
                %user_id,
                %error,
                "could not work out who shares a room with a user whose presence changed"
            ),
        }
    }

    /// Hands `update` to the cluster for every other replica, if this hub is part of one, and
    /// to the ephemeral observer, if one is installed.
    fn publish_ephemeral(&self, update: EphemeralUpdate) {
        if let Some(observer) = self.ephemeral_observer.get() {
            observer.ephemeral_changed(&update);
        }
        if let Some(link) = self.cluster.get() {
            link.cluster.publish_ephemeral(update);
        }
    }

    fn advance_peer_consumed(&self, peer: &str, consumed: u64) {
        let mut peers = self
            .peer_consumed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sender = peers
            .entry(peer.to_owned())
            .or_insert_with(|| tokio::sync::watch::channel(0).0);
        sender.send_if_modified(|current| {
            if consumed > *current {
                *current = consumed;
                true
            } else {
                false
            }
        });
    }

    /// The highest consumed mark `peer` has reported here, for tests and diagnostics.
    #[must_use]
    pub fn peer_consumed(&self, peer: &str) -> u64 {
        self.peer_consumed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(peer)
            .map_or(0, |s| *s.borrow())
    }

    async fn wait_for_peer_consumed(&self, peer: &str, seq: u64, at_most: Duration) {
        let mut rx = {
            let mut peers = self
                .peer_consumed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            peers
                .entry(peer.to_owned())
                .or_insert_with(|| tokio::sync::watch::channel(0).0)
                .subscribe()
        };
        if *rx.borrow() >= seq {
            return;
        }
        let _ = tokio::time::timeout(at_most, rx.wait_for(|consumed| *consumed >= seq)).await;
    }

    /// What a `/sync` does before it reads anything: waits, up to `at_most` in all, for this hub
    /// to have consumed everything its own registry had published when the request arrived
    /// ([`SessionHub::wait_for_consumed`]) and, in a cluster, for every peer's wakes up to what
    /// that peer reports having published ([`SessionCluster::peer_positions`]). The second is
    /// what makes a write a client just made through another replica visible to its next read
    /// here: the owner's hub sends the wake only after the feed entries are durable.
    ///
    /// Bounded, like the single-replica wait: a peer that has fallen seconds behind, or is not
    /// answering, must not hold every `/sync` on this replica with it.
    pub async fn settle_before_read(&self, at_most: Duration) {
        let started = tokio::time::Instant::now();
        let published = self.rooms.global_published_seq();
        self.wait_for_consumed(published, at_most).await;
        let Some(link) = self.cluster.get() else {
            return;
        };
        let remaining = at_most.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return;
        }
        let positions = link
            .cluster
            .peer_positions(remaining.min(PEER_POSITIONS_DEADLINE))
            .await;
        for position in positions {
            let remaining = at_most.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return;
            }
            self.wait_for_peer_consumed(&position.peer, position.published, remaining)
                .await;
        }
    }

    /// Waits until this hub has processed every global-stream update numbered up to `seq`
    /// (`hs_room::protocol::RoomUpdate::global_seq`), or `at_most` has passed.
    ///
    /// The feeds `/sync` reads are written here, off the registry's stream, a moment after the
    /// event that caused them was accepted. A client that joins a room and asks `/sync` in the
    /// same breath -- a bot, a test, a person on a fast link -- could be answered from before its
    /// own join had been written, and told nothing. Waiting here, for what was published before
    /// the request arrived, is what makes a client's own writes visible to its next read. The
    /// wait is bounded: a hub that has fallen seconds behind should not hold every sync with it.
    pub async fn wait_for_consumed(&self, seq: u64, at_most: Duration) {
        if seq == 0 || *self.consumed.borrow() >= seq {
            return;
        }
        let mut rx = self.consumed.subscribe();
        if tokio::time::timeout(at_most, rx.wait_for(|consumed| *consumed >= seq))
            .await
            .is_err()
        {
            // The read goes ahead from before the caller's own writes: worth an operator's
            // attention, since it means the hub is far behind the rooms.
            tracing::warn!(
                waited_for = seq,
                consumed = *self.consumed.borrow(),
                waited_ms = u64::try_from(at_most.as_millis()).unwrap_or(u64::MAX),
                "the session hub did not catch up with the room stream in time; reading anyway"
            );
        }
    }

    /// Tells every `/sync` that is waiting for news to answer now, with whatever it has, and
    /// every later one not to wait at all.
    ///
    /// A long-poll is a request that is *supposed* to stay open, for as long as its client asked
    /// (thirty seconds, from every real client). Graceful shutdown waits for in-flight requests
    /// to finish, so a server with anybody signed in took up to that long to stop -- longer than
    /// Kubernetes' default grace period, which ends in `SIGKILL`, and long enough that a
    /// restart which did not wait found the database still locked. A client handed an early,
    /// empty answer simply asks again, and meets the server that replaces this one.
    pub async fn begin_shutdown(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        for waker in self.wakers.lock().await.values() {
            waker.notify_waiters();
        }
    }

    /// Whether [`SessionHub::begin_shutdown`] has been called.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The per-device memory of lazily loaded members `/sync` consults and updates.
    #[must_use]
    pub fn lazy_members(&self) -> &crate::lazy_members::LazyMembersSent {
        &self.lazy_members
    }

    /// The waker a long-polling `/sync` call should register interest on *before* checking
    /// whether anything is already new (see `crate::sync`'s long-poll loop for why the ordering
    /// matters: registering after the check has a lost-wakeup race).
    pub async fn waker(&self, user_id: &UserId) -> Arc<Notify> {
        let mut wakers = self.wakers.lock().await;
        Arc::clone(
            wakers
                .entry(user_id.to_owned())
                .or_insert_with(|| Arc::new(Notify::new())),
        )
    }

    /// Tells `user_id`'s long-polling `/sync` that their account data changed. The routes that
    /// write account data and tags call this after the store write: `crate::sync::has_new_data`
    /// already noticed the counter move, but nothing woke the poll to look, so a tag set while
    /// a client waited was only seen when the wait timed out or something else happened.
    pub async fn account_data_changed(&self, user_id: &UserId) {
        self.wake(user_id).await;
    }

    async fn wake(&self, user_id: &UserId) {
        let waker = {
            let wakers = self.wakers.lock().await;
            wakers.get(user_id).cloned()
        };
        if let Some(waker) = waker {
            waker.notify_waiters();
        }
    }

    /// [`SessionHub::joined_member_ids`], with a room that no longer exists (deleted by an
    /// administrator, while somebody's records still name it) having no members rather than
    /// failing the caller -- for the walks over a user's rooms (`/sync`, presence audiences, the
    /// user directory) that one gone room must not break.
    pub(crate) async fn joined_member_ids_if_present(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<OwnedUserId>, UserError> {
        match self.joined_member_ids(room_id).await {
            Err(error) if error.is_room_not_found() => {
                tracing::debug!(%room_id, "a room named in a user's records no longer exists");
                Ok(Vec::new())
            }
            other => other,
        }
    }

    /// `room_id`'s current joined members, parsed as user ids (a state key that fails to parse --
    /// should not happen for anything this server itself wrote -- is skipped rather than failing
    /// the whole call). Shared by [`SessionHub::set_typing`] and [`SessionHub::set_presence`]: both
    /// need "who should be woken by this change", and both mean exactly this.
    pub(crate) async fn joined_member_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<OwnedUserId>, UserError> {
        let handle = self.room(room_id).await?;
        Ok(handle
            .query(|actor| {
                Ok::<_, hs_room::RoomError>(
                    actor
                        .joined_members()?
                        .into_iter()
                        .filter_map(|e| e.header().state_key.clone())
                        .filter_map(|s| UserId::parse(&s).ok().map(|u| u.to_owned()))
                        .collect::<Vec<_>>(),
                )
            })
            .await?)
    }

    /// Records `user_id`'s typing state in `room_id` and immediately wakes every joined member's
    /// long-polling `/sync` -- unlike to-device/device-list activity (`crate::sync`'s module
    /// docs), typing has a real waker hook, since this hub already has to look up the room's
    /// member list to know who to update.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room could not be loaded.
    pub async fn set_typing(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        typing: bool,
        timeout: Duration,
    ) -> Result<(), UserError> {
        self.typing.set(room_id, user_id, typing, timeout).await;
        let members = self.joined_member_ids(room_id).await?;
        for member in &members {
            self.wake(member).await;
        }
        self.publish_ephemeral(EphemeralUpdate::Typing {
            room_id: room_id.to_owned(),
            user_id: user_id.to_owned(),
            typing,
            timeout_ms: u64::try_from(timeout.min(crate::typing::MAX_TYPING_TIMEOUT).as_millis())
                .unwrap_or(u64::MAX),
        });
        if let Some(outbox) = self.edu_outbox.get() {
            let destinations = crate::edu::servers_of(&members);
            let key = format!("typing {room_id} {user_id}");
            outbox.send_edu(
                destinations.clone(),
                "m.typing",
                crate::edu::typing_content(room_id, user_id, typing),
                Some(key.clone()),
            );
            if typing {
                // The other servers are told when the typing lapses, as they are when it is
                // stopped: nothing else would tell them before their own backstop
                // (`crate::edu::REMOTE_TYPING_TIMEOUT`).
                let registry = Arc::clone(&self.typing);
                let outbox = Arc::clone(outbox);
                let (room_id, user_id) = (room_id.to_owned(), user_id.to_owned());
                let lapse = timeout.min(crate::typing::MAX_TYPING_TIMEOUT);
                tokio::spawn(async move {
                    tokio::time::sleep(lapse + Duration::from_millis(50)).await;
                    let (still, _) = registry.current(&room_id).await;
                    if !still.contains(&user_id) {
                        outbox.send_edu(
                            destinations,
                            "m.typing",
                            crate::edu::typing_content(&room_id, &user_id, false),
                            Some(key),
                        );
                    }
                });
            }
        }
        Ok(())
    }

    /// `room_id`'s currently-typing users and this room's typing cursor. See [`crate::typing`].
    pub async fn typing_users(&self, room_id: &RoomId) -> (Vec<OwnedUserId>, u64) {
        self.typing.current(room_id).await
    }

    /// Records `user_id`'s new presence and wakes every user who currently shares a *joined* room
    /// with them -- the same privacy scope `crate::sync::shared_users` already enforces for
    /// `device_lists` (a user must not learn about the presence of a stranger they share no room
    /// with).
    ///
    /// # Errors
    /// Returns [`UserError`] if a shared room could not be loaded.
    pub async fn set_presence(
        &self,
        user_id: &UserId,
        presence: String,
        status_msg: Option<String>,
    ) -> Result<(), UserError> {
        let seq = self.presence.set(user_id, presence, status_msg).await;
        let audience = self.users_sharing_room_with(user_id).await?;
        for other in &audience {
            self.wake(other).await;
        }
        self.publish_ephemeral(EphemeralUpdate::Presence {
            user_id: user_id.to_owned(),
            seq,
        });
        self.send_presence(user_id, &audience).await;
        // A user always sees their own just-set presence on their own next sync too (Synapse
        // behavior: a client's own `set_presence` call is reflected back to it), so wake the
        // setter's own long poll as well, not only everyone else's.
        self.wake(user_id).await;
        Ok(())
    }

    /// Marks `user_id` as being in `presence` as a side effect of them polling `/sync`, waking
    /// everyone who shares a room with them **only if** that actually changed their state.
    ///
    /// Unlike [`SessionHub::set_presence`] this does not wake the user themselves: the only
    /// caller is their own in-flight `/sync`, which is about to answer anyway, and waking it
    /// would just make it go round again.
    ///
    /// # Errors
    /// Returns [`UserError`] if a shared room could not be loaded.
    pub async fn touch_presence(&self, user_id: &UserId, presence: &str) -> Result<(), UserError> {
        let Some(seq) = self.presence.touch(user_id, presence).await else {
            return Ok(());
        };
        let audience = self.users_sharing_room_with(user_id).await?;
        for other in &audience {
            self.wake(other).await;
        }
        self.publish_ephemeral(EphemeralUpdate::Presence {
            user_id: user_id.to_owned(),
            seq,
        });
        self.send_presence(user_id, &audience).await;
        Ok(())
    }

    /// Hands `user_id`'s current presence to the outbox for the servers of `audience` (the
    /// people they share a room with), if an outbox is installed.
    async fn send_presence(&self, user_id: &UserId, audience: &BTreeSet<OwnedUserId>) {
        let Some(outbox) = self.edu_outbox.get() else {
            return;
        };
        let Some(record) = self.presence.get(user_id).await else {
            return;
        };
        outbox.send_edu(
            crate::edu::servers_of(audience),
            "m.presence",
            crate::edu::presence_content(user_id, &record),
            Some(format!("presence {user_id}")),
        );
    }

    /// `user_id`'s current presence record, if this process has ever recorded one.
    pub async fn presence_of(&self, user_id: &UserId) -> Option<crate::presence::PresenceRecord> {
        self.presence.get(user_id).await
    }

    /// Records `user_id`'s `kind` receipt for `event_id` in `room_id` and immediately wakes every
    /// joined member's long-polling `/sync` -- same wake-eagerly shape as
    /// [`SessionHub::set_typing`]. Waking is not scoped to who is actually allowed to *see* this
    /// particular receipt (an `m.read.private` receipt still wakes every member, not only its
    /// sender): an over-broad wake just costs a harmless response-building pass, and the privacy
    /// scope is enforced where it matters, in [`SessionHub::receipt_content_for`] /
    /// `crate::receipts::ReceiptRegistry::content_for`.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room could not be loaded.
    pub async fn set_receipt(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        kind: ReceiptKind,
        event_id: ruma::OwnedEventId,
        ts: u64,
    ) -> Result<(), UserError> {
        self.set_threaded_receipt(
            room_id,
            user_id,
            kind,
            &hs_push::counts::ReceiptThread::Unthreaded,
            event_id,
            ts,
        )
        .await
    }

    /// [`SessionHub::set_receipt`] for a receipt in `thread` (MSC3771's `thread_id`): kept
    /// beside the user's receipts in other threads, shown with its `thread_id`, sent to other
    /// servers with it, and read by the push counts for that thread only
    /// (`hs_push::counts`). Returns once the push counts reflect it (bounded by
    /// `hs_push::pipeline::RECEIPT_SETTLE`), so the client's next `/sync` has them.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room could not be loaded.
    pub async fn set_threaded_receipt(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        kind: ReceiptKind,
        thread: &hs_push::counts::ReceiptThread,
        event_id: ruma::OwnedEventId,
        ts: u64,
    ) -> Result<(), UserError> {
        let seq = self
            .receipts
            .set_in_thread(room_id, user_id, kind, thread, event_id.clone(), ts)
            .await;
        // Push counts follow receipts of either kind: a private receipt is just as much "read".
        // The sink ignores other servers' users (an inbound EDU's receipt).
        if let Some(sink) = self.read_receipt_sink.get() {
            sink.read_receipt(hs_push::pipeline::ReadReceipt {
                user_id: user_id.to_owned(),
                room_id: room_id.to_owned(),
                event_id: event_id.clone(),
                thread: thread.clone(),
            })
            .await;
        }
        let members = self.joined_member_ids(room_id).await?;
        for member in &members {
            self.wake(member).await;
        }
        // Every replica is told, a private receipt included: the hint names only the room, and
        // the other replica's registry scopes what it rereads per viewer as this one does.
        self.publish_ephemeral(EphemeralUpdate::Receipt {
            room_id: room_id.to_owned(),
            seq,
        });
        // A private receipt is its sender's alone; only a public one goes to other servers.
        if let (ReceiptKind::Read, Some(outbox)) = (kind, self.edu_outbox.get()) {
            outbox.send_edu(
                crate::edu::servers_of(&members),
                "m.receipt",
                crate::edu::receipt_content(room_id, user_id, &event_id, thread, ts),
                // One receipt per thread: a threaded receipt must not replace an unsent
                // unthreaded one (MSC4102), nor the other way round.
                Some(format!(
                    "receipt {room_id} {user_id} {}",
                    thread.as_wire().unwrap_or_default()
                )),
            );
        }
        Ok(())
    }

    /// Records a receipt copied from another implementation's database for this same server
    /// (the Synapse importer, `hs_compat::migration`): as [`SessionHub::set_receipt`] -- kept
    /// durably, the room's joined members woken, the other replicas told -- except that nothing
    /// is sent to other servers. The receipt is not news to them: Synapse sent it when it was
    /// made. Returns the receipt's stamp.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room could not be loaded.
    pub async fn import_receipt(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        kind: ReceiptKind,
        event_id: ruma::OwnedEventId,
        ts: u64,
    ) -> Result<u64, UserError> {
        let members = self.joined_member_ids(room_id).await?;
        let seq = self
            .receipts
            .set(room_id, user_id, kind, event_id, ts)
            .await;
        for member in &members {
            self.wake(member).await;
        }
        self.publish_ephemeral(EphemeralUpdate::Receipt {
            room_id: room_id.to_owned(),
            seq,
        });
        Ok(seq)
    }

    /// Applies a typing, receipt or presence EDU another server sent (`origin`), and wakes the
    /// local users it concerns. Returns how many updates were applied; what was dropped (a user
    /// of another server, a user not joined to the room here, a room this server does not have)
    /// is logged at `debug`. Nothing applied here is sent on to any other server: each server
    /// distributes its own users' EDUs. See `crate::edu`'s module docs. In a cluster, what is
    /// applied here is published to the other replicas as a local change would be: the
    /// transaction arrived at this one, and they were not told.
    pub async fn receive_edu(
        &self,
        origin: &str,
        edu_type: &str,
        content: &serde_json::Value,
    ) -> usize {
        let mut applied = 0;
        for update in InboundEdu::parse(origin, edu_type, content) {
            match update {
                InboundEdu::Typing {
                    room_id,
                    user_id,
                    typing,
                } => {
                    let Some(members) = self.members_if_joined(&room_id, &user_id).await else {
                        continue;
                    };
                    self.typing
                        .set(
                            &room_id,
                            &user_id,
                            typing,
                            crate::edu::REMOTE_TYPING_TIMEOUT,
                        )
                        .await;
                    for member in &members {
                        self.wake(member).await;
                    }
                    self.publish_ephemeral(EphemeralUpdate::Typing {
                        room_id,
                        user_id,
                        typing,
                        timeout_ms: u64::try_from(crate::edu::REMOTE_TYPING_TIMEOUT.as_millis())
                            .unwrap_or(u64::MAX),
                    });
                    applied += 1;
                }
                InboundEdu::Receipt {
                    room_id,
                    user_id,
                    event_id,
                    ts,
                    thread,
                } => {
                    let Some(members) = self.members_if_joined(&room_id, &user_id).await else {
                        continue;
                    };
                    let seq = self
                        .receipts
                        .set_in_thread(&room_id, &user_id, ReceiptKind::Read, &thread, event_id, ts)
                        .await;
                    for member in &members {
                        self.wake(member).await;
                    }
                    self.publish_ephemeral(EphemeralUpdate::Receipt { room_id, seq });
                    applied += 1;
                }
                InboundEdu::Presence {
                    user_id,
                    presence,
                    status_msg,
                    last_active_ago,
                    currently_active,
                } => {
                    let seq = self
                        .presence
                        .set_remote(
                            &user_id,
                            presence,
                            status_msg,
                            last_active_ago,
                            currently_active,
                        )
                        .await;
                    self.wake_presence_audience(&user_id).await;
                    self.publish_ephemeral(EphemeralUpdate::Presence { user_id, seq });
                    applied += 1;
                }
            }
        }
        applied
    }

    /// `room_id`'s joined members, if `user_id` is one of them; `None` (logged at `debug`) if
    /// they are not or the room cannot be read here.
    async fn members_if_joined(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Option<Vec<OwnedUserId>> {
        match self.joined_member_ids(room_id).await {
            Ok(members) if members.iter().any(|m| m == user_id) => Some(members),
            Ok(_) => {
                tracing::debug!(%room_id, %user_id, "dropping an EDU for a user not joined here");
                None
            }
            Err(error) => {
                tracing::debug!(%room_id, %error, "dropping an EDU for a room not readable here");
                None
            }
        }
    }

    /// `room_id`'s current receipt cursor. See [`crate::receipts`].
    pub async fn receipts_seq(&self, room_id: &RoomId) -> u64 {
        self.receipts.seq(room_id).await
    }

    /// The `m.receipt` event content for `room_id` as `viewer` (privacy-scoped -- see
    /// [`crate::receipts::ReceiptRegistry::content_for`]), plus this room's current cursor.
    pub async fn receipt_content_for(
        &self,
        room_id: &RoomId,
        viewer: &UserId,
    ) -> (serde_json::Value, u64) {
        self.receipts.content_for(room_id, viewer).await
    }

    /// Every user (other than `user_id`) currently sharing at least one *joined* room with
    /// `user_id`. Public (unlike [`SessionHub::joined_member_ids`]) because `crate::sync::shared_users`
    /// -- which needs the identical scope for `device_lists` -- delegates to this rather than
    /// duplicating the membership walk.
    ///
    /// Bounded by the user's own joined rooms (their membership records), never the server's,
    /// and read from the member index (`hs_user.room_members`, the same index
    /// [`SessionHub::users_visible_in_directory_to`] reads: each room's joined members, kept
    /// current from the room updates this hub applies) rather than through each room's actor.
    /// Every `/sync` asks this for its presence and device-list scope, and until 2026-10-02
    /// each call read every shared room through the room itself -- a load, for a room not
    /// resident. A room the index has nothing for (its last update came before the index
    /// existed) is read once and indexed then ([`SessionHub::directory_rooms_walked`] counts
    /// it, as for a search).
    ///
    /// # Errors
    /// Returns [`UserError`] if the membership records, the index, or a room it had to read
    /// could not be read.
    pub async fn users_sharing_room_with(
        &self,
        user_id: &UserId,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, UserError> {
        let mut shared = std::collections::BTreeSet::new();
        for m in self.store.list_memberships(user_id).await? {
            if m.membership != "join" {
                continue;
            }
            let members = match self.store.room_member_ids(&m.room_id).await? {
                Some(members) => members,
                None => self.index_room_by_reading_it(&m.room_id).await?,
            };
            shared.extend(
                members
                    .into_iter()
                    .filter(|member| member.as_str() != user_id.as_str()),
            );
        }
        Ok(shared)
    }

    /// Everyone `user_id` may find in the user directory: the people they share a joined room
    /// with, and everyone joined to a public room (one whose join rule is `public` or whose
    /// history is world-readable) that somebody on this server is in -- the spec's floor for
    /// `POST /user_directory/search`, and this server's ceiling. The requester is in the answer
    /// when they are in such a public room, as on Synapse: Sytest's directory tests search for
    /// the requester's own name and expect to find it there, and not to once they have left.
    ///
    /// Read from the directory index in the store (`hs_user.room_members`: each room's joined
    /// members, kept up to date from the room updates this hub applies,
    /// [`SessionHub::process_room_update`]), never by loading the rooms: the rooms to look in
    /// are the user's joined ones (their membership records) and the public ones (the public
    /// room list this hub keeps), and each one's members are one range read. It used to load
    /// every one of those rooms and read its state on every search -- a public room of 5,000
    /// members cost every searcher 5,000 member events read through the room.
    ///
    /// A room the index has nothing for -- one whose last update this hub applied came before
    /// the index existed (a server upgraded from before it) -- is read once, the old way, and
    /// indexed then ([`SessionHub::directory_rooms_walked`] counts those, and each is an `info`
    /// line); from then on its updates keep it current. A room that no longer exists has no
    /// members, as before.
    ///
    /// # Errors
    /// Returns [`UserError`] if a membership list, the index, or a room it had to read could
    /// not be read.
    pub async fn users_visible_in_directory_to(
        &self,
        user_id: &UserId,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, UserError> {
        // The index and the membership records are written off the room stream a moment after
        // the rooms accept their events: wait for what was published before the search, as
        // `/sync` does, so that somebody who has just joined a room can find its members.
        self.settle_before_read(crate::sync::READ_YOUR_WRITES_WAIT)
            .await;
        self.directory_from_index(user_id).await
    }

    /// [`SessionHub::users_visible_in_directory_to`] without the wait for the hub to catch up:
    /// the search itself.
    async fn directory_from_index(
        &self,
        user_id: &UserId,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, UserError> {
        let joined: BTreeSet<ruma::OwnedRoomId> = self
            .store
            .list_memberships(user_id)
            .await?
            .into_iter()
            .filter(|m| m.membership == "join")
            .map(|m| m.room_id)
            .collect();
        let public: BTreeSet<ruma::OwnedRoomId> = self
            .store
            .list_directory_public_rooms()
            .await?
            .into_iter()
            .map(|room| room.room_id)
            .collect();
        let mut visible = BTreeSet::new();
        for room_id in joined.union(&public) {
            let members = match self.store.room_member_ids(room_id).await? {
                Some(members) => members,
                None => self.index_room_by_reading_it(room_id).await?,
            };
            if public.contains(room_id) {
                // A public room counts while this server is in it -- somebody local is joined
                // (Synapse's `users_in_public_rooms` is emptied for a room the server left).
                // Everybody in it is offered, the requester included.
                if members
                    .iter()
                    .any(|member| member.server_name() == user_id.server_name())
                {
                    visible.extend(members);
                }
            } else {
                visible.extend(
                    members
                        .into_iter()
                        .filter(|member| member.as_str() != user_id.as_str()),
                );
            }
        }
        Ok(visible)
    }

    /// How many rooms user-directory searches on this hub have had to read whole because the
    /// directory index had nothing for them (each is then indexed, so a room is counted once
    /// per process at most, and normally never): the index's rebuild, for an operator wondering
    /// whether searches are still paying for it, and for tests.
    #[must_use]
    pub fn directory_rooms_walked(&self) -> u64 {
        self.directory_walks
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reads `room_id`'s joined members from the room itself and indexes them, for a room the
    /// directory index has nothing for. A room that no longer exists has no members and is not
    /// indexed.
    async fn index_room_by_reading_it(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<OwnedUserId>, UserError> {
        let members = match self.joined_member_ids(room_id).await {
            Ok(members) => members,
            Err(error) if error.is_room_not_found() => {
                tracing::debug!(%room_id, "a room named in a user's records no longer exists");
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        self.directory_walks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let indexed = self
            .store
            .index_room_members_if_absent(room_id, &members)
            .await?;
        tracing::info!(
            %room_id,
            members = members.len(),
            indexed,
            "read a room's members for the user directory, which had no index of it yet"
        );
        Ok(members)
    }

    /// Keeps the directory index (`users_visible_in_directory_to`) current from one room
    /// update: the members `update` changed are added or removed, by what the room says they
    /// are now (`members`, every member and their membership, which the caller has just read);
    /// a room the index has nothing for yet is indexed whole from `members`.
    async fn index_members_for_directory(
        &self,
        update: &RoomUpdate,
        members: &[(OwnedUserId, String)],
    ) -> Result<(), UserError> {
        let joined: Vec<OwnedUserId> = members
            .iter()
            .filter(|(_, membership)| membership == "join")
            .map(|(user, _)| user.clone())
            .collect();
        let joins = update
            .membership_deltas
            .iter()
            .filter(|delta| delta.membership == "join")
            .count();
        if joins > 0 {
            // A join is when a room's state can have arrived whole, with only the join itself
            // in the deltas: a room this server was invited to (indexed from a stub with no
            // members at the invite) is joined through another server, or rejoined after this
            // server was out of it. The index is made to match the room, not just the deltas.
            // The room's member list was read for this update anyway; this is one more pass
            // over it on a join, never on a message.
            if let Some((added, removed)) = self
                .store
                .reconcile_room_members(&update.room_id, &joined)
                .await?
            {
                if added > joins || removed > 0 {
                    tracing::info!(
                        room_id = %update.room_id,
                        added,
                        removed,
                        members = joined.len(),
                        "a room's member index was behind the room on a join; made it match"
                    );
                }
                return Ok(());
            }
        } else {
            let changes: Vec<(OwnedUserId, bool)> = update
                .membership_deltas
                .iter()
                .map(|delta| {
                    let now = members
                        .iter()
                        .find(|(user, _)| user == &delta.user_id)
                        .map_or(delta.membership.as_str(), |(_, membership)| {
                            membership.as_str()
                        });
                    (delta.user_id.clone(), now == "join")
                })
                .collect();
            if self
                .store
                .apply_room_member_changes(&update.room_id, &changes)
                .await?
            {
                return Ok(());
            }
        }
        if self
            .store
            .index_room_members_if_absent(&update.room_id, &joined)
            .await?
        {
            tracing::debug!(
                room_id = %update.room_id,
                members = joined.len(),
                "indexed a room's members for the user directory"
            );
        }
        Ok(())
    }

    /// Spawns a background task forwarding `handle`'s publish stream
    /// ([`hs_room::actor::RoomActorHandle::subscribe`]) into [`SessionHub::process_room_update`]
    /// for as long as the room stays resident. See the module docs, "The discovery gap", for why
    /// a caller must invoke this for every room.
    /// Subscribes before returning, so once this call completes nothing published by `handle` can
    /// be missed. It deliberately does not spawn the subscription: doing that lost every event a
    /// caller wrote between calling this and the spawned task actually reaching `subscribe`, which
    /// is a race a caller has no way to wait out.
    pub async fn watch_room(
        self: &Arc<Self>,
        handle: hs_room::actor::RoomActorHandle<B>,
    ) -> tokio::task::JoinHandle<()>
    where
        B: 'static,
        // The spawned task holds an `Arc<Self>`, so the room source it reaches through must
        // outlive this call. Bounded here rather than on the impl block: every other method on
        // the hub is happy with a borrowed room source.
        R: 'static,
    {
        let rx = handle.subscribe().await;
        let hub = Arc::clone(self);
        tokio::spawn(async move { hub.consume_updates(rx).await })
    }

    /// Spawns a background task draining `updates` into [`SessionHub::process_room_update`], for
    /// the whole server at once: pass [`hs_room::registry::RoomRegistry::subscribe_global`]'s
    /// receiver, and every room the registry loads or creates is followed without anything having
    /// to call [`SessionHub::watch_room`] per room. This is the production wiring the
    /// discovery gap described in this module's docs asked for
    /// (`docs/rfcs/0012-room-registry-global-updates.md`); `watch_room` remains for a caller that
    /// holds one handle and wants only that room.
    ///
    /// Subscribe before serving traffic: the stream does not replay updates published before the
    /// subscription existed, and an invite missed that way would not reach its target's feed.
    pub fn watch_all(
        self: &Arc<Self>,
        updates: tokio::sync::broadcast::Receiver<RoomUpdate>,
    ) -> tokio::task::JoinHandle<()>
    where
        B: 'static,
        R: 'static,
    {
        let hub = Arc::clone(self);
        tokio::spawn(async move { hub.consume_updates(updates).await })
    }

    /// The shared drain loop behind [`SessionHub::watch_room`] and [`SessionHub::watch_all`].
    async fn consume_updates(&self, mut updates: tokio::sync::broadcast::Receiver<RoomUpdate>) {
        loop {
            match updates.recv().await {
                Ok(update) => {
                    let seq = update.global_seq;
                    let room_id = update.room_id.clone();
                    let room_pos = update.room_pos;
                    let woken = match self.handle_room_update(update).await {
                        Ok(woken) => woken,
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to process a room update into user feeds");
                            Vec::new()
                        }
                    };
                    // Processed or failed, it has been dealt with: nobody should wait for it.
                    if seq > 0 {
                        self.consumed.send_if_modified(|current| {
                            if seq > *current {
                                *current = seq;
                                true
                            } else {
                                false
                            }
                        });
                    }
                    // The other replicas hear of it after the feeds are written and the mark
                    // moved, so that a peer released by this number reads what it stands for.
                    if let Some(link) = self.cluster.get() {
                        link.cluster.publish(RoomWake {
                            room_id,
                            room_pos,
                            global_seq: seq,
                            users: woken,
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // This hub missed `skipped` publishes, and cannot get them back from the
                    // channel. Which rooms they were about is unknowable, so every resident room
                    // is re-read from where it is now: `process_room_update` on a room's head
                    // writes any membership row that is missing and gives every active member a
                    // feed entry at the head. At worst that repeats something a client has
                    // already seen; what it prevents is an invitation whose only update was
                    // among the skipped ones never reaching its target, which is what this arm
                    // used to do while saying nothing was lost.
                    tracing::warn!(
                        skipped,
                        "session hub fell behind the room stream; re-reading every resident room"
                    );
                    for handle in self.rooms.resident_handles().await {
                        let Some(head) = handle.query(|actor| actor.head_update()).await else {
                            continue;
                        };
                        let room_id = head.room_id.clone();
                        let room_pos = head.room_pos;
                        match self.handle_room_update(head).await {
                            Ok(woken) => {
                                if let Some(link) = self.cluster.get() {
                                    link.cluster.publish(RoomWake {
                                        room_id,
                                        room_pos,
                                        global_seq: 0,
                                        users: woken,
                                    });
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to re-read a room after falling behind");
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    /// Applies one [`RoomUpdate`] to every affected user's durable state: refreshes
    /// `crate::store::UserStore::set_membership` for members whose membership this exact update
    /// changed (or who this hub has not recorded a membership for yet), appends a feed entry for
    /// every active (join/invite/knock) member unless the room is over
    /// [`SessionHub::fan_out_threshold`], and wakes every affected user's long-polling `/sync`.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room's member list could not be read, or if a store write
    /// failed.
    pub async fn process_room_update(&self, update: RoomUpdate) -> Result<(), UserError> {
        self.handle_room_update(update).await.map(|_| ())
    }

    /// [`SessionHub::apply_room_update`] under the hub's own error handling: the wrapper the
    /// watchers and [`SessionHub::process_room_update`] call. Copying tags and `m.direct` onto
    /// an upgraded room's replacement happens inside, as the join is applied.
    async fn handle_room_update(&self, update: RoomUpdate) -> Result<Vec<OwnedUserId>, UserError> {
        self.apply_room_update(update).await
    }

    /// Copies what `user_id`'s account data says about `old_room_id` onto `new_room_id`, its
    /// replacement, as they join it: every `m.direct` list naming the old room gains the new
    /// one (the old entry is kept, as Synapse keeps it), and every piece of room account data
    /// on the old room (`m.tag` and the rest) the new room does not have yet is written for
    /// it. Nothing for a user with no account data here (another server's user, whose own
    /// server does this). Each write bumps the user's account-data counter, so their next
    /// `/sync` carries it.
    ///
    /// # Errors
    /// Returns [`UserError`] if the account data could not be read or written.
    async fn carry_account_data_on_upgrade(
        &self,
        user_id: &UserId,
        old_room_id: &RoomId,
        new_room_id: &RoomId,
    ) -> Result<(), UserError> {
        let mut direct_updated = false;
        if let Some(record) = self
            .store
            .get_global_account_data(user_id, "m.direct")
            .await?
            && let Some(map) = record.content.as_object()
        {
            let mut content = record.content.clone();
            for (other, rooms) in map {
                let Some(rooms) = rooms.as_array() else {
                    continue;
                };
                let names_old = rooms
                    .iter()
                    .any(|r| r.as_str() == Some(old_room_id.as_str()));
                let names_new = rooms
                    .iter()
                    .any(|r| r.as_str() == Some(new_room_id.as_str()));
                if names_old && !names_new {
                    let mut rooms = rooms.clone();
                    rooms.push(serde_json::Value::String(new_room_id.to_string()));
                    content[other] = serde_json::Value::Array(rooms);
                    direct_updated = true;
                }
            }
            if direct_updated {
                self.store
                    .put_global_account_data(user_id, "m.direct", content)
                    .await?;
            }
        }
        let old_data = self
            .store
            .list_room_account_data(user_id, old_room_id)
            .await?;
        let mut copied = 0usize;
        if !old_data.is_empty() {
            let present: std::collections::HashSet<String> = self
                .store
                .list_room_account_data(user_id, new_room_id)
                .await?
                .into_iter()
                .map(|record| record.event_type)
                .collect();
            for record in old_data {
                if present.contains(&record.event_type) {
                    continue;
                }
                self.store
                    .put_room_account_data(user_id, new_room_id, &record.event_type, record.content)
                    .await?;
                copied += 1;
            }
        }
        if direct_updated || copied > 0 {
            tracing::info!(
                user = %user_id,
                %old_room_id,
                %new_room_id,
                direct_updated,
                room_account_data_copied = copied,
                "a user joined an upgraded room's replacement; their account data on the old room followed"
            );
        }
        Ok(())
    }

    /// Copies `user_id`'s push rules about `old_room_id` onto `new_room_id`, its replacement, as
    /// they join it: the room rule named after the old room, and every override or underride
    /// rule with an `event_match` on `room_id` for the old room (its ID and that condition
    /// rewritten for the new room), each keeping its actions and whether it is enabled. A rule
    /// the new room already has is left as it is. Synapse's
    /// `copy_push_rules_from_room_to_room_for_user`, run from the same join as
    /// [`SessionHub::carry_account_data_on_upgrade`] (Complement's `TestPushRuleRoomUpgrade`:
    /// a local upgrade, a manual one, and a remote server's users joining the replacement). The
    /// write bumps the user's push-rules change-seq, so their next `/sync` carries
    /// `m.push_rules`. Nothing for a user who never changed their rules, or with no push-rules
    /// store installed.
    ///
    /// # Errors
    /// Returns [`UserError::Push`] if the ruleset could not be read or written.
    async fn copy_room_push_rules(
        &self,
        user_id: &UserId,
        old_room_id: &RoomId,
        new_room_id: &RoomId,
    ) -> Result<(), UserError> {
        use hs_push::ruleset::{NewRule, RuleKind};
        use hs_push::rulesets::RulesetStore as _;
        use ruma::push::PushCondition;

        let Some(store) = self.push_rules.get() else {
            return Ok(());
        };
        let Some(mut ruleset) = store.store().get_ruleset(user_id).await? else {
            return Ok(());
        };
        let (old, new) = (old_room_id.as_str(), new_room_id.as_str());
        // (kind, new rule, enabled), collected first: the ruleset is edited after the reads.
        let mut copies: Vec<(NewRule, bool)> = Vec::new();
        if let Some(rule) = ruleset.room.iter().find(|r| r.rule_id == old)
            && !ruleset.room.iter().any(|r| r.rule_id == new)
        {
            copies.push((
                NewRule {
                    kind: RuleKind::Room,
                    rule_id: new.to_owned(),
                    actions: rule.actions.clone(),
                    conditions: Vec::new(),
                    pattern: None,
                },
                rule.enabled,
            ));
        }
        for (kind, list) in [
            (RuleKind::Override, &ruleset.override_),
            (RuleKind::Underride, &ruleset.underride),
        ] {
            for rule in list.iter().filter(|r| !r.default) {
                let names_old_room = rule.conditions.iter().any(|c| {
                    matches!(c, PushCondition::EventMatch(data)
                        if data.key == "room_id" && data.pattern == old)
                });
                if !names_old_room {
                    continue;
                }
                let rule_id = rule.rule_id.replace(old, new);
                if rule_id == rule.rule_id || list.iter().any(|r| r.rule_id == rule_id) {
                    continue;
                }
                let mut conditions = rule.conditions.clone();
                for condition in &mut conditions {
                    if let PushCondition::EventMatch(data) = condition
                        && data.key == "room_id"
                        && data.pattern == old
                    {
                        new.clone_into(&mut data.pattern);
                    }
                }
                copies.push((
                    NewRule {
                        kind,
                        rule_id,
                        actions: rule.actions.clone(),
                        conditions,
                        pattern: None,
                    },
                    rule.enabled,
                ));
            }
        }
        if copies.is_empty() {
            return Ok(());
        }
        let mut copied = 0usize;
        for (rule, enabled) in copies {
            let (kind, rule_id) = (rule.kind, rule.rule_id.clone());
            if let Err(error) = ruleset.insert(rule, None, None) {
                tracing::warn!(user = %user_id, %rule_id, %error, "a push rule could not be copied onto an upgraded room's replacement");
                continue;
            }
            if !enabled {
                let _ = ruleset.set_enabled(kind, &rule_id, false);
            }
            copied += 1;
        }
        if copied > 0 {
            store.set_ruleset(user_id, &ruleset).await?;
            tracing::info!(
                user = %user_id,
                %old_room_id,
                %new_room_id,
                push_rules_copied = copied,
                "a user joined an upgraded room's replacement; their push rules for the old room followed"
            );
        }
        Ok(())
    }

    /// [`SessionHub::process_room_update`], returning the users it woke -- what the other
    /// replicas are told (`crate::cluster::RoomWake::users`). Empty, and nothing written, for a
    /// room this replica does not own: its owner feeds it, and two hubs writing the same
    /// user's feed from two views of one room would race each other.
    async fn apply_room_update(&self, update: RoomUpdate) -> Result<Vec<OwnedUserId>, UserError> {
        if !self.owns_room(&update.room_id) {
            return Ok(Vec::new());
        }
        let handle = match self.rooms.get_or_load(&update.room_id).await {
            Ok(handle) => handle,
            Err(hs_room::RoomError::RoomNotFound(_)) => {
                return self.apply_update_for_a_gone_room(update).await;
            }
            Err(error) => return Err(error.into()),
        };
        let (active_members, member_count, directory) = handle
            .query(|actor| {
                let members = actor.members()?;
                let count = members.len();
                let active: Vec<(OwnedUserId, String)> = members
                    .iter()
                    .filter_map(|e| {
                        let state_key = e.header().state_key.clone()?;
                        let user_id = UserId::parse(state_key.as_str()).ok()?.to_owned();
                        let membership = membership_of(e)?;
                        Some((user_id, membership))
                    })
                    .collect();
                let directory = public_directory_entry(actor)?;
                Ok::<_, hs_room::RoomError>((active, count, directory))
            })
            .await?;

        match directory {
            Some(entry) => self.store.upsert_public_room(entry).await?,
            None => self.store.remove_public_room(&update.room_id).await?,
        }
        self.index_members_for_directory(&update, &active_members)
            .await?;

        let hot = member_count > self.fan_out_threshold;
        let joined_members: Vec<OwnedUserId> = active_members
            .iter()
            .filter(|(_, membership)| membership == "join")
            .map(|(user, _)| user.clone())
            .collect();

        let mut targets: HashMap<OwnedUserId, String> = active_members
            .into_iter()
            .filter(|(_, m)| is_active(m))
            .collect();
        for delta in &update.membership_deltas {
            targets.insert(delta.user_id.clone(), delta.membership.clone());
        }

        // Somebody joining an upgraded room's replacement brings their account data about the
        // old room with them: the replacement is a direct chat if the old room was, and
        // carries the old room's tags (Synapse's `copy_user_state_on_room_upgrade`, from its
        // join handler; Sytest's "/upgrade preserves direct room state").
        if update
            .membership_deltas
            .iter()
            .any(|d| d.membership == "join")
            && let Some(old_room_id) = handle.query(|actor| actor.predecessor_room_id()).await
        {
            for delta in &update.membership_deltas {
                if delta.membership == "join" {
                    self.carry_account_data_on_upgrade(
                        &delta.user_id,
                        &old_room_id,
                        &update.room_id,
                    )
                    .await?;
                    self.copy_room_push_rules(&delta.user_id, &old_room_id, &update.room_id)
                        .await?;
                }
            }
        }

        // Somebody joining enters the presence audience of everyone already here, whose tokens
        // may well be newer than the joiner's last presence change. Restamp it so that it
        // reaches them (`PresenceRegistry::restamp`); the wake below is the same one the join
        // itself causes. Complement's "Existing members see new members' presence" is this.
        for delta in &update.membership_deltas {
            if delta.membership == "join"
                && let Some(seq) = self.presence.restamp(&delta.user_id).await
            {
                // The other replicas hold the joiner's record under its old stamp, if at all.
                self.publish_ephemeral(EphemeralUpdate::Presence {
                    user_id: delta.user_id.clone(),
                    seq,
                });
            }
        }

        self.share_presence_on_join(&update, &joined_members).await;

        let started = std::time::Instant::now();
        let fan_out = self.fan_out(&update, hot, &targets).await?;
        if fan_out.flipped > 0 {
            tracing::info!(
                room_id = %update.room_id,
                member_count,
                threshold = self.fan_out_threshold,
                hot,
                records = fan_out.flipped,
                "a room crossed the fan-out threshold; its members' records now say so"
            );
        }
        // A hot room's position goes on the hot-room stream once, whatever its size: what
        // `/sync` resumes it from and bounds it by (`crate::sync`'s `resume_mode`). Written
        // after the membership records, so that a sync which sees this position also sees the
        // membership the update made -- the other way round, a sync could resume a member who
        // has only just joined from their own join, and send them nothing of the room.
        if hot {
            let hot_seq = self
                .store
                .append_hot_position(&update.room_id, update.room_pos)
                .await?;
            self.compact_hot_stream_if_due(hot_seq).await;
        }
        crate::metrics::observe_fan_out(
            targets.len(),
            started.elapsed(),
            fan_out.report.transactions,
            fan_out.report.fallbacks,
        );
        // The membership records say who shares a room with whom now: a remote user this
        // update took the last shared room from has their device-list copy marked stale,
        // before anyone is woken to ask for it.
        self.mark_unshared_remote_lists_stale(&update, &targets)
            .await;
        let peekers = self.fan_out_to_peekers(&update, hot, &targets).await?;
        // And everyone is woken last, once everything a woken `/sync` will read is written.
        for user_id in targets.keys().chain(peekers.iter()) {
            self.wake(user_id).await;
        }
        // The feeds that outgrew their retention are compacted after the wake: a compaction
        // is a scan the woken syncs need not wait for, and it deletes nothing they read.
        self.compact_feeds(&fan_out.report.feeds_to_compact).await;

        let mut woken: Vec<OwnedUserId> = targets.into_keys().collect();
        woken.extend(peekers);
        Ok(woken)
    }

    /// Peeking (MSC2753, `crate::routes::peek`): a local user who joins a room stops peeking
    /// into it -- the room moves to `join` -- and everyone else with a device peeking into it
    /// gets the update in their feed as a member does (no membership record: a peeker is not in
    /// the room). Returns the peekers to wake. One range read of the room's peekers per update,
    /// nearly always empty.
    async fn fan_out_to_peekers(
        &self,
        update: &RoomUpdate,
        hot: bool,
        targets: &HashMap<OwnedUserId, String>,
    ) -> Result<Vec<OwnedUserId>, UserError> {
        for delta in &update.membership_deltas {
            if delta.membership == "join" {
                let ended = self
                    .store
                    .remove_peeks(&delta.user_id, None, &update.room_id)
                    .await?;
                if ended > 0 {
                    tracing::debug!(
                        user_id = %delta.user_id,
                        room_id = %update.room_id,
                        ended,
                        "a peeker joined the room; their peeks into it end"
                    );
                }
            }
        }
        let mut peekers = Vec::new();
        for peeker in self.store.room_peekers(&update.room_id).await? {
            if targets.contains_key(&peeker) {
                continue;
            }
            // A hot room writes no feed entries; its peekers, like its members, follow the
            // hot-room stream.
            if !hot {
                self.store
                    .append_feed_entry(&peeker, &update.room_id, update.room_pos)
                    .await?;
            }
            peekers.push(peeker);
        }
        Ok(peekers)
    }

    /// Presence across servers when a room gains a member, as Synapse's presence handler does
    /// on a join: a local joiner's presence goes to every other server in the room, and a
    /// remote joiner's server -- which may be new to the room, and so have nothing of this
    /// server's users -- is sent the presence of this server's members. Without it two people
    /// on two servers who have just started a chat see each other as offline until one of them
    /// changes state (Sytest's "New federated private chats get full presence information
    /// (SYN-115)"). Needs an outbox and this server's name; nothing is sent without them.
    async fn share_presence_on_join(&self, update: &RoomUpdate, joined: &[OwnedUserId]) {
        let Some(outbox) = self.edu_outbox.get() else {
            return;
        };
        let Some(own) = self.rooms.server_name() else {
            return;
        };
        for delta in &update.membership_deltas {
            if delta.membership != "join" {
                continue;
            }
            if delta.user_id.server_name() == own {
                let servers: BTreeSet<String> = crate::edu::servers_of(joined)
                    .into_iter()
                    .filter(|server| server.as_str() != own.as_str())
                    .collect();
                if servers.is_empty() {
                    continue;
                }
                if let Some(record) = self.presence.get(&delta.user_id).await {
                    tracing::debug!(
                        user_id = %delta.user_id,
                        room_id = %update.room_id,
                        servers = servers.len(),
                        "a local user joined a room with other servers in it; sending them their presence"
                    );
                    outbox.send_edu(
                        servers,
                        "m.presence",
                        crate::edu::presence_content(&delta.user_id, &record),
                        Some(format!("presence {}", delta.user_id)),
                    );
                }
            } else {
                let mut push = Vec::new();
                for member in joined.iter().filter(|m| m.server_name() == own) {
                    if let Some(record) = self.presence.get(member).await
                        && let Some(entry) = crate::edu::presence_content(member, &record)
                            .get_mut("push")
                            .and_then(|p| p.as_array_mut())
                            .and_then(|p| p.pop())
                    {
                        push.push(entry);
                    }
                }
                if push.is_empty() {
                    continue;
                }
                tracing::debug!(
                    joiner = %delta.user_id,
                    room_id = %update.room_id,
                    users = push.len(),
                    "a remote user joined a room; sending their server our members' presence"
                );
                outbox.send_edu(
                    BTreeSet::from([delta.user_id.server_name().to_string()]),
                    "m.presence",
                    serde_json::json!({"push": push}),
                    None,
                );
            }
        }
    }

    /// Writes one update's membership records and feed entries for `targets` (each active
    /// member, and everyone whose membership the update changed), in a few store transactions
    /// ([`crate::store::UserStore::apply_fan_out`]) -- or one or two per member with
    /// [`FAN_OUT_UNBATCHED_ENV`] set. What is written for whom is decided here, from one read
    /// of every target's record ([`crate::store::UserStore::get_memberships`]).
    async fn fan_out(
        &self,
        update: &RoomUpdate,
        hot: bool,
        targets: &HashMap<OwnedUserId, String>,
    ) -> Result<FanOut, UserError> {
        let user_ids: Vec<OwnedUserId> = targets.keys().cloned().collect();
        let existing = self
            .store
            .get_memberships(&update.room_id, &user_ids)
            .await?;
        let mut flipped = 0usize;
        let mut writes = Vec::with_capacity(user_ids.len());
        for (user_id, existing) in user_ids.into_iter().zip(existing) {
            let membership = &targets[&user_id];
            let changed_now = update
                .membership_deltas
                .iter()
                .any(|d| d.user_id == user_id);
            let missing = existing.is_none();
            // The room crossing the threshold in either direction is written to every member's
            // record, not only the one whose membership this update changed: `crate::sync`
            // trusts `hot_room` to say whether a room's feed entries are being written, and a
            // member whose record still said "cold" for a room that had gone hot was never
            // sent anything from it again (no entries, and not a candidate without them).
            let hot_flipped = existing.as_ref().is_some_and(|m| m.hot_room != hot);
            flipped += usize::from(hot_flipped);
            let record = if changed_now || missing || hot_flipped {
                // A membership record's `room_pos` is a resume baseline, and `hs_room`'s forward
                // pagination is *exclusive* of it: whatever sits at that position counts as
                // already delivered. When this update is the user's own membership change, its
                // position is exactly right. When we are only backfilling a record that went
                // missing -- the room existed before anything watched its publish stream, say --
                // this update is an ordinary event the user has *not* seen, so claiming its
                // position would make the next incremental sync skip it. Back off by one so the
                // fallback can only ever repeat an event, never lose one, which is the direction
                // `crate::sync::resume_mode` documents as the safe one.
                // And a record rewritten only because the room's hot-ness flipped keeps the
                // baseline it had: the user's own membership event has not moved.
                let baseline_pos = match &existing {
                    _ if changed_now => update.room_pos,
                    Some(existing) => existing.room_pos,
                    None => update.room_pos.saturating_sub(1),
                };
                Some((membership.clone(), baseline_pos))
            } else {
                None
            };
            writes.push(FanOutWrite {
                user_id,
                record,
                hot_room: hot,
                feed_entry: !hot,
            });
        }
        let report = if self.fan_out_unbatched {
            let mut report = crate::store::FanOutReport::default();
            for write in &writes {
                if let Some((membership, baseline_pos)) = &write.record {
                    self.store
                        .set_membership(
                            &write.user_id,
                            &update.room_id,
                            membership,
                            *baseline_pos,
                            hot,
                        )
                        .await?;
                    report.records += 1;
                    report.transactions += 1;
                }
                if write.feed_entry {
                    self.store
                        .append_feed_entry(&write.user_id, &update.room_id, update.room_pos)
                        .await?;
                    report.feed_entries += 1;
                    report.transactions += 1;
                }
            }
            report
        } else {
            self.store
                .apply_fan_out(
                    &update.room_id,
                    update.room_pos,
                    &writes,
                    self.feed_retention(),
                )
                .await?
        };
        if report.fallbacks > 0 {
            tracing::info!(
                room_id = %update.room_id,
                members = writes.len(),
                fallbacks = report.fallbacks,
                "a room update's fan-out wrote some members one at a time"
            );
        }
        Ok(FanOut { report, flipped })
    }

    /// Compacts each of `users`' feeds to this hub's retention
    /// ([`crate::store::UserStore::compact_feed`]), counting what went.
    async fn compact_feeds(&self, users: &[OwnedUserId]) {
        let keep = self.feed_retention();
        if keep == 0 {
            return;
        }
        for user_id in users {
            match self.store.compact_feed(user_id, keep).await {
                Ok(pruned) => {
                    crate::metrics::observe_compaction("feed", pruned);
                    tracing::debug!(user_id = %user_id, pruned, keep, "compacted a user's feed");
                }
                Err(e) => {
                    tracing::warn!(user_id = %user_id, error = %e, "failed to compact a user's feed");
                }
            }
        }
    }

    /// Compacts the hot-room stream ([`crate::store::UserStore::compact_hot_stream`]) when it
    /// has grown to twice its retention since this hub last did, `hot_seq` being its newest
    /// position.
    async fn compact_hot_stream_if_due(&self, hot_seq: u64) {
        let keep = self.hot_stream_retention();
        if keep == 0 {
            return;
        }
        let last = self
            .hot_compacted_at
            .load(std::sync::atomic::Ordering::Relaxed);
        if hot_seq.saturating_sub(last) <= keep.saturating_mul(2) {
            return;
        }
        self.hot_compacted_at
            .store(hot_seq, std::sync::atomic::Ordering::Relaxed);
        match self.store.compact_hot_stream(keep).await {
            Ok(pruned) => {
                crate::metrics::observe_compaction("hot_room_stream", pruned);
                tracing::info!(pruned, keep, hot_seq, "compacted the hot-room stream");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to compact the hot-room stream");
            }
        }
    }

    /// [`SessionHub::apply_room_update`] for an update whose room no longer exists by the time
    /// this hub reads it: an administrator's room deletion makes every local member leave and
    /// then purges the room in one go, and a hub a moment behind finds the leaves' room already
    /// gone. Those leaves used to be dropped with a warning, so each member's record went on
    /// saying `join` for a room that was not there, and their next `/sync` failed with `404`.
    ///
    /// What the update itself says is still applied -- each membership change it carries, with
    /// a feed entry so the member's next sync shows the room as left -- and the room's public
    /// directory entry is removed. Nothing else is: there is no state left to read.
    async fn apply_update_for_a_gone_room(
        &self,
        update: RoomUpdate,
    ) -> Result<Vec<OwnedUserId>, UserError> {
        tracing::info!(
            room_id = %update.room_id,
            room_pos = update.room_pos,
            changes = update.membership_deltas.len(),
            "a room update arrived for a room that no longer exists; applying its membership changes"
        );
        self.store.remove_public_room(&update.room_id).await?;
        self.store.forget_room_members(&update.room_id).await?;
        let user_ids: Vec<OwnedUserId> = update
            .membership_deltas
            .iter()
            .map(|d| d.user_id.clone())
            .collect();
        let existing = self
            .store
            .get_memberships(&update.room_id, &user_ids)
            .await?;
        let writes: Vec<FanOutWrite> = update
            .membership_deltas
            .iter()
            .zip(existing)
            .map(|(delta, existing)| {
                let hot = existing.is_some_and(|m| m.hot_room);
                FanOutWrite {
                    user_id: delta.user_id.clone(),
                    record: Some((delta.membership.clone(), update.room_pos)),
                    hot_room: hot,
                    feed_entry: !hot,
                }
            })
            .collect();
        let report = self
            .store
            .apply_fan_out(
                &update.room_id,
                update.room_pos,
                &writes,
                self.feed_retention(),
            )
            .await?;
        for user_id in &user_ids {
            self.wake(user_id).await;
        }
        self.compact_feeds(&report.feeds_to_compact).await;
        Ok(user_ids)
    }

    /// A room's current member count, for callers (`crate::sync`'s hot-room fallback) that need
    /// to decide whether to check a room's live position directly rather than trusting the feed.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room could not be loaded.
    pub async fn room_member_count(&self, room_id: &RoomId) -> Result<usize, UserError> {
        let handle = self.room(room_id).await?;
        Ok(handle
            .query(|actor| actor.members().map(|m| m.len()))
            .await?)
    }
}

/// A convenience for a long-poll loop: waits until `notify` fires or `timeout` elapses.
pub async fn wait_or_timeout(notify: &Notify, timeout: Duration) {
    let notified = notify.notified();
    tokio::pin!(notified);
    let _ = tokio::time::timeout(timeout, notified).await;
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static, R: RoomSource<B> + 'static> hs_auth::state::UserDirectoryVisibility
    for SessionHub<B, R>
{
    async fn visible_to(
        &self,
        requester: &UserId,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, String> {
        self.users_visible_in_directory_to(requester)
            .await
            .map_err(|e| e.to_string())
    }

    /// Each remote user's display name and avatar from their member event in one room they are
    /// joined to here (their membership records name the rooms: this hub keeps one for every
    /// member of a room it follows, whichever server they are on). One room read per user,
    /// for the users a search offers who have no account here.
    async fn remote_profiles(
        &self,
        users: &std::collections::BTreeSet<OwnedUserId>,
    ) -> Result<Vec<hs_auth::state::RemoteProfile>, String> {
        let mut profiles = Vec::new();
        for user in users {
            let memberships = self
                .store
                .list_memberships(user)
                .await
                .map_err(|e| e.to_string())?;
            for record in memberships.into_iter().filter(|m| m.membership == "join") {
                let handle = match self.room(&record.room_id).await {
                    Ok(handle) => handle,
                    Err(error) if error.is_room_not_found() => continue,
                    Err(error) => return Err(error.to_string()),
                };
                let who = user.clone();
                let event = handle
                    .query(move |actor| {
                        actor
                            .state_event("m.room.member", who.as_str())
                            .ok()
                            .flatten()
                            .map(hs_room::routes::render::client_event_json)
                    })
                    .await;
                let Some(event) = event.filter(|e| {
                    e.pointer("/content/membership")
                        .and_then(serde_json::Value::as_str)
                        == Some("join")
                }) else {
                    continue;
                };
                let field = |name: &str| {
                    event
                        .pointer(&format!("/content/{name}"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                profiles.push(hs_auth::state::RemoteProfile {
                    user_id: user.clone(),
                    display_name: field("displayname"),
                    avatar_url: field("avatar_url"),
                });
                break;
            }
        }
        tracing::debug!(
            asked = users.len(),
            found = profiles.len(),
            "read other servers' users' profiles for a user-directory search"
        );
        Ok(profiles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_source::test_support::registry;
    use crate::store::tables::TablesUserStore;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::CreateRoomRequest;
    use hs_room::membership::Action;
    use ruma::user_id;
    use std::sync::Arc as StdArc;

    type TestRoomRegistry = StdArc<hs_room::registry::RoomRegistry<MemoryBackend>>;
    type TestHub = StdArc<SessionHub<MemoryBackend, TestRoomRegistry>>;

    fn hub(threshold: usize) -> (TestHub, TestRoomRegistry) {
        let rooms = registry("hub.test");
        let store: DynUserStore = StdArc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        (
            StdArc::new(SessionHub::new(store, rooms.clone(), threshold)),
            rooms,
        )
    }

    /// The observer hears every change this hub applies: its own clients' typing, receipts and
    /// presence, and a peer's update, in the order they happened -- and hears nothing when
    /// nothing is installed.
    #[tokio::test]
    async fn the_ephemeral_observer_is_told_of_local_and_peer_changes() {
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl EphemeralObserver for Recorder {
            fn ephemeral_changed(&self, update: &EphemeralUpdate) {
                self.0.lock().unwrap().push(update.kind().to_owned());
            }
        }
        let (hub, rooms) = hub(500);
        let alice = user_id!("@alice:hub.test").to_owned();
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let recorder = StdArc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        hub.install_ephemeral_observer(recorder.clone());

        hub.set_typing(&room_id, &alice, true, Duration::from_secs(30))
            .await
            .unwrap();
        hub.set_receipt(
            &room_id,
            &alice,
            crate::receipts::ReceiptKind::Read,
            ruma::event_id!("$e:hub.test").to_owned(),
            1,
        )
        .await
        .unwrap();
        hub.set_presence(&alice, "online".to_owned(), None)
            .await
            .unwrap();
        hub.apply_ephemeral(
            "peer#1",
            EphemeralUpdate::Typing {
                room_id: room_id.clone(),
                user_id: alice.clone(),
                typing: false,
                timeout_ms: 0,
            },
        )
        .await;
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec!["typing", "receipt", "presence", "typing"]
        );
        // The peer's typing stop was applied before the observer heard of it.
        assert!(hub.typing_users(&room_id).await.0.is_empty());
    }

    #[tokio::test]
    async fn processing_a_create_room_update_feeds_the_creator() {
        let (hub, rooms) = hub(500);
        let alice = user_id!("@alice:hub.test").to_owned();
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;

        // Send one more event and give the spawned watcher a moment to process it.
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hi"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let latest = hub.store().latest_feed_seq(&alice).await.unwrap();
        assert!(latest > 0, "the creator's feed should have advanced");
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let membership = hub
            .store()
            .get_membership(&alice, &room_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(membership.membership, "join");
    }

    #[tokio::test]
    async fn an_invite_reaches_the_invitee_even_though_they_never_created_or_joined_anything() {
        let (hub, rooms) = hub(500);
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let handle = rooms
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;

        handle
            .membership(
                alice.clone(),
                Action::Invite,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let membership = hub
            .store()
            .get_membership(&bob, &room_id)
            .await
            .unwrap()
            .expect("bob's invite must be recorded even though he never called anything");
        assert_eq!(membership.membership, "invite");
        assert!(hub.store().latest_feed_seq(&bob).await.unwrap() > 0);
    }

    /// The regression behind Sytest's "If remote user leaves room, changes device and rejoins
    /// we see update in sync": on the invitee's server the member index of a room is made
    /// from the invite's stub, with nobody in it, and the room's state arrives whole with the
    /// join, whose update names only the joiner. The index then said the joiner was alone, so
    /// `users_sharing_room_with` found nobody and the joiner's key changes were announced to
    /// no server at all. A join now makes the index match the room.
    #[tokio::test]
    async fn a_join_makes_a_member_index_that_was_behind_the_room_match_it() {
        let (hub, rooms) = hub(500);
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        // The invite stub's index: the room is indexed, and nobody is in it.
        hub.store().forget_room_members(&room_id).await.unwrap();
        assert!(
            hub.store()
                .index_room_members_if_absent(&room_id, &[])
                .await
                .unwrap()
        );
        hub.watch_room(handle.clone()).await;
        handle
            .membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let shared = hub.users_sharing_room_with(&bob).await.unwrap();
        assert!(
            shared.contains(&alice),
            "bob shares the room with alice, whom the stale index did not name: {shared:?}"
        );
        let mut members = hub
            .store()
            .room_member_ids(&room_id)
            .await
            .unwrap()
            .unwrap();
        members.sort();
        assert_eq!(members, vec![alice, bob]);
    }

    /// Sytest's "Server correctly resyncs when server leaves and rejoins a room" and
    /// Complement's `TestDeviceListUpdates/when_remote_user_rejoins_a_room`: once no local user
    /// shares a room with a remote user, nothing keeps this server's copy of their device list
    /// current, so the membership change that ends the last shared room marks it stale -- from
    /// either side: the remote user leaving, or the last local user leaving. A copy of somebody
    /// still sharing a room is left alone.
    #[tokio::test]
    async fn a_remote_users_device_list_goes_stale_when_no_room_is_shared_any_more() {
        use hs_e2e::store::RemoteDeviceListStore;
        let (hub, rooms) = hub(500);
        let e2e =
            StdArc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        hub.install_remote_device_lists(e2e.clone(), "hub.test".try_into().unwrap());
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:remote.test").to_owned();
        let carol = user_id!("@carol:remote.test").to_owned();
        let held = |stale| hs_e2e::store::RemoteUserRow {
            stream_id: 1,
            master: None,
            self_signing: None,
            stale,
        };
        for user in [&bob, &carol] {
            e2e.replace_remote_device_list(user, held(false), Vec::new())
                .await
                .unwrap();
        }
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        for (user, ts) in [(&bob, 2), (&carol, 3)] {
            handle
                .membership(
                    user.clone(),
                    Action::Join,
                    user.clone(),
                    serde_json::json!({}),
                    ts,
                )
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stale = |user: OwnedUserId| {
            let e2e = e2e.clone();
            async move { e2e.get_remote_user(&user).await.unwrap().unwrap().stale }
        };
        assert!(!stale(bob.clone()).await && !stale(carol.clone()).await);

        // Bob leaves: his copy is stale; carol, still in the room with alice, keeps hers.
        handle
            .membership(
                bob.clone(),
                Action::Leave,
                bob.clone(),
                serde_json::json!({}),
                4,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(stale(bob.clone()).await, "bob shares no room here any more");
        assert!(
            !stale(carol.clone()).await,
            "carol still shares the room with alice"
        );

        // Alice, the last local user, leaves: carol's copy is stale too.
        handle
            .membership(
                alice.clone(),
                Action::Leave,
                alice.clone(),
                serde_json::json!({}),
                5,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            stale(carol).await,
            "no local user is left in the room carol is in"
        );
    }

    #[tokio::test]
    async fn a_room_above_the_threshold_stops_growing_the_feed_but_keeps_memberships() {
        let (hub, rooms) = hub(1); // threshold of 1: the second member makes it "hot"
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        handle
            .membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let before = hub.store().latest_feed_seq(&alice).await.unwrap();
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hi"}),
                None,
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = hub.store().latest_feed_seq(&alice).await.unwrap();
        assert_eq!(
            before, after,
            "a hot room must not keep appending feed entries"
        );

        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let membership = hub
            .store()
            .get_membership(&alice, &room_id)
            .await
            .unwrap()
            .unwrap();
        assert!(membership.hot_room);
    }

    /// A member who was there before the room went hot is told so too. Watched from before the
    /// room exists, as `hs serve` watches every room, the creator's membership record is written
    /// at the create; it used to be rewritten only when *their* membership changed, so the
    /// second member arriving left it saying "cold" for a room whose feed entries had stopped.
    #[tokio::test]
    async fn a_room_going_hot_is_written_to_the_records_of_the_members_already_there() {
        let (hub, rooms) = hub(1);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cold = hub
            .store()
            .get_membership(&alice, &room_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!cold.hot_room);
        assert!(cold.room_pos > 0);

        handle
            .membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let hot = hub
            .store()
            .get_membership(&alice, &room_id)
            .await
            .unwrap()
            .unwrap();
        assert!(hot.hot_room, "{hot:?}");
        assert_eq!(
            hot.room_pos, cold.room_pos,
            "the baseline is still alice's own membership event"
        );
        assert_eq!(hot.membership, "join");
    }

    /// [`SessionHub::install_device_list_token_resolver`] installs a resolver on the given
    /// [`hs_e2e::state::E2eState`] that decodes this crate's own [`SyncToken`] and reports its
    /// `device_list_seq` field verbatim -- the exact contract `GET /keys/changes`
    /// (`crates/hs-e2e/src/routes/keys_changes.rs`) needs from
    /// [`hs_e2e::state::SyncTokenResolver::resolve_device_list_position`].
    #[tokio::test]
    async fn device_list_token_resolver_decodes_a_sync_token_and_rejects_anything_else() {
        let (hub, _rooms) = hub(500);
        let e2e_state = hs_e2e::state::E2eState::new(
            hs_auth::state::AuthState::in_memory(),
            StdArc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap()),
        );
        hub.install_device_list_token_resolver(&e2e_state);

        let alice = user_id!("@alice:hub.test");
        let resolver = e2e_state
            .sync_token_resolver()
            .expect("a resolver should now be installed");

        let token = SyncToken {
            device_list_seq: 42,
            ..SyncToken::initial()
        };
        let resolved = resolver
            .resolve_device_list_position(alice, &token.encode())
            .await
            .unwrap();
        assert_eq!(resolved, Some(42));

        let not_ours = resolver
            .resolve_device_list_position(alice, "not-one-of-our-tokens")
            .await
            .unwrap();
        assert_eq!(
            not_ours, None,
            "a string that isn't one of our tokens must be reported as unrecognized, not 0"
        );
    }

    /// [`SessionHub::install_push_rules_store`]/[`SessionHub::install_counts_store`] are
    /// idempotent past the first call (mirroring
    /// `hs_room::registry::RoomRegistry::install_global_token_resolver`'s convention): a second
    /// install is ignored, the first-installed store stays authoritative.
    #[tokio::test]
    async fn a_second_install_of_the_push_rules_or_counts_store_is_ignored() {
        let (hub, _rooms) = hub(500);
        let first = StdArc::new(hs_push::rulesets::CachedRulesetStore::new(
            hs_push::rulesets::tables::TablesRulesetStore::open(MemoryBackend::new()).unwrap(),
        ));
        hub.install_push_rules_store(first.clone());
        let second = StdArc::new(hs_push::rulesets::CachedRulesetStore::new(
            hs_push::rulesets::tables::TablesRulesetStore::open(MemoryBackend::new()).unwrap(),
        ));
        hub.install_push_rules_store(second);
        assert!(StdArc::ptr_eq(hub.push_rules_store().unwrap(), &first,));

        let first_counts: StdArc<dyn hs_push::counts::CountsStore> = StdArc::new(
            hs_push::counts::tables::TablesCountsStore::open(MemoryBackend::new()).unwrap(),
        );
        hub.install_counts_store(first_counts.clone());
        let second_counts: StdArc<dyn hs_push::counts::CountsStore> = StdArc::new(
            hs_push::counts::tables::TablesCountsStore::open(MemoryBackend::new()).unwrap(),
        );
        hub.install_counts_store(second_counts);
        assert!(StdArc::ptr_eq(hub.counts_store().unwrap(), &first_counts));
    }

    // ---------------------------------------------------------------------------------------
    // The user directory's index.
    // ---------------------------------------------------------------------------------------

    async fn directory(hub: &TestHub, user: &UserId) -> Vec<String> {
        hub.users_visible_in_directory_to(user)
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.to_string())
            .collect()
    }

    async fn create(
        rooms: &TestRoomRegistry,
        creator: &OwnedUserId,
        preset: &str,
    ) -> (
        hs_room::actor::RoomActorHandle<MemoryBackend>,
        ruma::OwnedRoomId,
    ) {
        let handle = rooms
            .create_room(
                creator.clone(),
                CreateRoomRequest {
                    preset: Some(preset.to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        (handle, room_id)
    }

    async fn member(
        handle: &hs_room::actor::RoomActorHandle<MemoryBackend>,
        user: &OwnedUserId,
        action: Action,
        ts: i64,
    ) {
        handle
            .membership(
                user.clone(),
                action,
                user.clone(),
                serde_json::json!({}),
                ts,
            )
            .await
            .unwrap();
    }

    async fn invite_and_join(
        handle: &hs_room::actor::RoomActorHandle<MemoryBackend>,
        inviter: &OwnedUserId,
        user: &OwnedUserId,
        ts: i64,
    ) {
        handle
            .membership(
                inviter.clone(),
                Action::Invite,
                user.clone(),
                serde_json::json!({}),
                ts,
            )
            .await
            .unwrap();
        member(handle, user, Action::Join, ts + 1).await;
    }

    /// Joining the successor of an upgraded room brings the user's tags and `m.direct` entry for
    /// the old room along -- once: a tag then removed in the new room stays removed through a
    /// later join.
    #[tokio::test]
    async fn joining_an_upgraded_rooms_successor_copies_tags_and_m_direct() {
        let (hub, rooms) = hub(500);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let (old, old_id) = create(&rooms, &alice, "private_chat").await;
        invite_and_join(&old, &alice, &bob, 2).await;
        hub.store()
            .put_room_account_data(
                &bob,
                &old_id,
                "m.tag",
                serde_json::json!({"tags": {"test_tag": {"order": 1}}}),
            )
            .await
            .unwrap();
        hub.store()
            .put_global_account_data(
                &bob,
                "m.direct",
                serde_json::json!({alice.as_str(): [old_id.as_str()]}),
            )
            .await
            .unwrap();

        // The successor as `/upgrade` makes it: its create event names the old room.
        let new = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("private_chat".to_owned()),
                    creation_content: serde_json::json!({
                        "predecessor": {"room_id": old_id.as_str(), "event_id": "$tombstone:hub.test"}
                    }),
                    ..Default::default()
                },
                3,
            )
            .await
            .unwrap();
        let new_id = new.query(|a| a.room_id().to_owned()).await;
        invite_and_join(&new, &alice, &bob, 4).await;

        let tags_in = |room: ruma::OwnedRoomId| {
            let hub = StdArc::clone(&hub);
            let bob = bob.clone();
            async move {
                hub.store()
                    .list_room_account_data(&bob, &room)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|a| a.event_type == "m.tag")
                    .map(|a| a.content)
            }
        };
        let mut copied = None;
        for _ in 0..100 {
            copied = tags_in(new_id.clone()).await;
            if copied.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            copied,
            Some(serde_json::json!({"tags": {"test_tag": {"order": 1}}})),
            "bob's tag followed him into the new room"
        );
        let direct = hub
            .store()
            .get_global_account_data(&bob, "m.direct")
            .await
            .unwrap()
            .unwrap()
            .content;
        assert_eq!(
            direct,
            serde_json::json!({alice.as_str(): [old_id.as_str(), new_id.as_str()]})
        );
        assert_eq!(
            tags_in(old_id.clone()).await,
            Some(serde_json::json!({"tags": {"test_tag": {"order": 1}}})),
            "the old room keeps its tag"
        );

        // Removed in the new room, the tag does not come back on a later join.
        hub.store()
            .put_room_account_data(&bob, &new_id, "m.tag", serde_json::json!({"tags": {}}))
            .await
            .unwrap();
        member(&new, &bob, Action::Leave, 6).await;
        invite_and_join(&new, &alice, &bob, 7).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            tags_in(new_id.clone()).await,
            Some(serde_json::json!({"tags": {}}))
        );
    }

    /// Who the directory shows follows membership -- a join, a leave, a room going from public
    /// to invite-only -- from the index the hub keeps, and no search reads a room to answer.
    #[tokio::test]
    async fn the_directory_follows_membership_from_its_index_without_reading_rooms() {
        let (hub, rooms) = hub(500);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let carol = user_id!("@carol:hub.test").to_owned();
        let dave = user_id!("@dave:hub.test").to_owned();
        let (private, _) = create(&rooms, &alice, "private_chat").await;
        invite_and_join(&private, &alice, &bob, 2).await;
        let (public, _) = create(&rooms, &carol, "public_chat").await;

        assert_eq!(
            directory(&hub, &alice).await,
            [bob.as_str(), carol.as_str()]
        );
        assert_eq!(
            directory(&hub, &bob).await,
            [alice.as_str(), carol.as_str()]
        );
        assert_eq!(directory(&hub, &dave).await, [carol.as_str()]);
        // Carol finds herself: she is in a public room (a search for one's own name is what
        // Sytest's directory tests do), as alice, in a private room only, does not.
        assert_eq!(directory(&hub, &carol).await, [carol.as_str()]);

        member(&private, &bob, Action::Leave, 3).await;
        member(&public, &dave, Action::Join, 4).await;
        assert_eq!(
            directory(&hub, &alice).await,
            [carol.as_str(), dave.as_str()]
        );
        assert_eq!(directory(&hub, &bob).await, [carol.as_str(), dave.as_str()]);
        assert_eq!(
            directory(&hub, &dave).await,
            [carol.as_str(), dave.as_str()]
        );

        // Carol's room stops being public: only who shares it with somebody sees them now.
        public
            .send_event(
                carol.clone(),
                "m.room.join_rules".to_owned(),
                Some(String::new()),
                serde_json::json!({"join_rule": "invite"}),
                None,
                5,
            )
            .await
            .unwrap();
        assert!(directory(&hub, &alice).await.is_empty());
        assert_eq!(directory(&hub, &dave).await, [carol.as_str()]);
        assert_eq!(directory(&hub, &carol).await, [dave.as_str()]);

        assert_eq!(hub.directory_rooms_walked(), 0, "no search read a room");
    }

    /// Who shares a room with a user -- every `/sync`'s presence and device-list scope -- is
    /// read from the member index, bounded by the user's own joined rooms: a join and a leave
    /// in a live room change the answer through the index alone, a room alice is in that the
    /// registry does not have answers from its index row without a load, and the hundred
    /// rooms on the server alice is not in are never touched. Until 2026-10-02 every call read
    /// each shared room through its actor.
    #[tokio::test]
    async fn users_sharing_room_with_is_read_from_the_index_within_the_users_rooms() {
        let (hub, rooms) = hub(500);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let carol = user_id!("@carol:hub.test").to_owned();
        let dave = user_id!("@dave:hub.test").to_owned();
        let sharing = |user: OwnedUserId| {
            let hub = hub.clone();
            async move {
                hub.settle_before_read(crate::sync::READ_YOUR_WRITES_WAIT)
                    .await;
                hub.users_sharing_room_with(&user)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
            }
        };

        // A hundred rooms on the server alice is not in, each with members of its own.
        for n in 0..100 {
            let room = ruma::OwnedRoomId::try_from(format!("!other{n}:hub.test")).unwrap();
            hub.store()
                .set_membership(&carol, &room, "join", 1, false)
                .await
                .unwrap();
            hub.store()
                .index_room_members_if_absent(&room, &[carol.clone(), dave.clone()])
                .await
                .unwrap();
        }
        assert!(sharing(alice.clone()).await.is_empty());

        // A live room: the answer follows bob's join and leave through the index.
        let (private, _) = create(&rooms, &alice, "private_chat").await;
        invite_and_join(&private, &alice, &bob, 2).await;
        assert_eq!(sharing(alice.clone()).await, [bob.as_str()]);
        assert_eq!(sharing(bob.clone()).await, [alice.as_str()]);
        member(&private, &bob, Action::Leave, 4).await;
        assert!(sharing(alice.clone()).await.is_empty());
        assert!(sharing(bob.clone()).await.is_empty(), "bob left");

        // A room alice is in that the registry does not have: its index row answers.
        let indexed = ruma::room_id!("!indexed:hub.test");
        hub.store()
            .set_membership(&alice, indexed, "join", 1, false)
            .await
            .unwrap();
        hub.store()
            .index_room_members_if_absent(indexed, &[alice.clone(), dave.clone()])
            .await
            .unwrap();
        assert_eq!(sharing(alice.clone()).await, [dave.as_str()]);
        assert_eq!(
            hub.directory_rooms_walked(),
            0,
            "no room was read to answer any of it"
        );

        // A room alice is in that the index has nothing for yet is read once and indexed.
        let (unindexed, unindexed_id) = create(&rooms, &alice, "private_chat").await;
        invite_and_join(&unindexed, &alice, &carol, 6).await;
        hub.settle_before_read(crate::sync::READ_YOUR_WRITES_WAIT)
            .await;
        hub.store()
            .forget_room_members(&unindexed_id)
            .await
            .unwrap();
        assert_eq!(
            sharing(alice.clone()).await,
            [carol.as_str(), dave.as_str()]
        );
        assert_eq!(hub.directory_rooms_walked(), 1, "read once");
        assert_eq!(
            sharing(alice.clone()).await,
            [carol.as_str(), dave.as_str()]
        );
        assert_eq!(hub.directory_rooms_walked(), 1, "and indexed then");
    }

    /// Joining an upgraded room's replacement carries what the user's account data said about
    /// the old room: a direct chat stays one, and its tags come along; account data the new
    /// room already has is kept, and a join into a room with no predecessor changes nothing.
    #[tokio::test]
    async fn joining_a_replacement_room_carries_the_old_rooms_direct_flag_and_tags() {
        let (hub, rooms) = hub(500);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let (old, old_id) = create(&rooms, &bob, "private_chat").await;
        invite_and_join(&old, &bob, &alice, 2).await;
        hub.store()
            .put_global_account_data(
                &alice,
                "m.direct",
                serde_json::json!({bob.as_str(): [old_id.as_str()]}),
            )
            .await
            .unwrap();
        hub.store()
            .put_room_account_data(
                &alice,
                &old_id,
                "m.tag",
                serde_json::json!({"tags": {"m.favourite": {"order": 0.1}}}),
            )
            .await
            .unwrap();
        hub.store()
            .put_room_account_data(
                &alice,
                &old_id,
                "org.example.note",
                serde_json::json!({"n": 1}),
            )
            .await
            .unwrap();

        let replacement = rooms
            .create_room(
                bob.clone(),
                CreateRoomRequest {
                    preset: Some("private_chat".to_owned()),
                    creation_content: serde_json::json!({"predecessor": {"room_id": old_id.as_str()}}),
                    ..Default::default()
                },
                5,
            )
            .await
            .unwrap();
        let new_id = replacement.query(|a| a.room_id().to_owned()).await;
        // Something alice already set on the new room is not overwritten by the old room's.
        hub.store()
            .put_room_account_data(
                &alice,
                &new_id,
                "org.example.note",
                serde_json::json!({"n": 2}),
            )
            .await
            .unwrap();
        invite_and_join(&replacement, &bob, &alice, 6).await;
        hub.settle_before_read(crate::sync::READ_YOUR_WRITES_WAIT)
            .await;

        let direct = hub
            .store()
            .get_global_account_data(&alice, "m.direct")
            .await
            .unwrap()
            .unwrap()
            .content;
        assert_eq!(
            direct,
            serde_json::json!({bob.as_str(): [old_id.as_str(), new_id.as_str()]}),
            "the replacement is a direct chat with bob too"
        );
        let new_data: std::collections::BTreeMap<String, serde_json::Value> = hub
            .store()
            .list_room_account_data(&alice, &new_id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.event_type, r.content))
            .collect();
        assert_eq!(
            new_data.get("m.tag"),
            Some(&serde_json::json!({"tags": {"m.favourite": {"order": 0.1}}})),
            "the tag came along"
        );
        assert_eq!(
            new_data.get("org.example.note"),
            Some(&serde_json::json!({"n": 2})),
            "what alice set on the new room herself is kept"
        );

        // Bob's m.direct says nothing about the old room, and the plain room has no predecessor:
        // neither changes anything.
        assert!(
            hub.store()
                .get_global_account_data(&bob, "m.direct")
                .await
                .unwrap()
                .is_none()
        );
        let (plain, plain_id) = create(&rooms, &bob, "private_chat").await;
        invite_and_join(&plain, &bob, &alice, 8).await;
        hub.settle_before_read(crate::sync::READ_YOUR_WRITES_WAIT)
            .await;
        assert!(
            hub.store()
                .list_room_account_data(&alice, &plain_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A world-readable room is public to the user directory whatever its join rule (Sytest's
    /// "Users appear/disappear from directory when history_visibility are changed" and "Users
    /// stay in directory when join_rules are changed but history_visibility is world_readable"),
    /// but is not in `/publicRooms` for that alone.
    #[tokio::test]
    async fn a_world_readable_room_is_public_to_the_directory_but_not_to_public_rooms() {
        let (hub, rooms) = hub(500);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        let carol = user_id!("@carol:hub.test").to_owned();
        let dave = user_id!("@dave:hub.test").to_owned();
        let (room, room_id) = create(&rooms, &carol, "private_chat").await;
        let set_state = |event_type: &'static str, content: serde_json::Value, ts: i64| {
            let room = room.clone();
            let carol = carol.clone();
            async move {
                room.send_event(
                    carol,
                    event_type.to_owned(),
                    Some(String::new()),
                    content,
                    None,
                    ts,
                )
                .await
                .unwrap();
            }
        };
        assert!(directory(&hub, &dave).await.is_empty());

        set_state(
            "m.room.history_visibility",
            serde_json::json!({"history_visibility": "world_readable"}),
            2,
        )
        .await;
        assert_eq!(directory(&hub, &dave).await, [carol.as_str()]);
        assert_eq!(
            hub.store().list_public_rooms().await.unwrap(),
            [],
            "world-readable alone is not a /publicRooms listing"
        );
        let directory_rooms = hub.store().list_directory_public_rooms().await.unwrap();
        assert_eq!(directory_rooms.len(), 1);
        assert_eq!(directory_rooms[0].room_id, room_id);
        assert!(!directory_rooms[0].join_rule_public);

        set_state(
            "m.room.join_rules",
            serde_json::json!({"join_rule": "public"}),
            3,
        )
        .await;
        // (`directory` waits for the hub to catch up with the room; a bare store read does not.)
        assert_eq!(directory(&hub, &dave).await, [carol.as_str()]);
        assert_eq!(hub.store().list_public_rooms().await.unwrap().len(), 1);
        set_state(
            "m.room.history_visibility",
            serde_json::json!({"history_visibility": "shared"}),
            4,
        )
        .await;
        assert_eq!(directory(&hub, &dave).await, [carol.as_str()]);

        set_state(
            "m.room.join_rules",
            serde_json::json!({"join_rule": "invite"}),
            5,
        )
        .await;
        assert!(directory(&hub, &dave).await.is_empty());
        assert!(
            hub.store()
                .list_directory_public_rooms()
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A public room counts for the directory only while somebody on this server is in it:
    /// once the last local member leaves, its remote members are no longer offered (Sytest's
    /// "User in remote room doesn't appear in user directory after server left room").
    #[tokio::test]
    async fn a_public_room_nobody_local_is_in_offers_nobody() {
        let (hub, _rooms) = hub(500);
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let remote = user_id!("@remote:elsewhere.test").to_owned();
        let room = ruma::room_id!("!public:elsewhere.test");
        hub.store()
            .upsert_public_room(crate::store::PublicRoomEntry {
                room_id: room.to_owned(),
                join_rule_public: true,
                name: None,
                topic: None,
                canonical_alias: None,
                avatar_url: None,
                num_joined_members: 1,
                world_readable: false,
                guest_can_join: false,
            })
            .await
            .unwrap();
        assert!(
            hub.store()
                .index_room_members_if_absent(room, std::slice::from_ref(&remote))
                .await
                .unwrap()
        );
        assert!(directory(&hub, &alice).await.is_empty());

        hub.store()
            .apply_room_member_changes(room, &[(bob.clone(), true)])
            .await
            .unwrap();
        assert_eq!(
            directory(&hub, &alice).await,
            [bob.as_str(), remote.as_str()]
        );
        hub.store()
            .apply_room_member_changes(room, &[(bob.clone(), false)])
            .await
            .unwrap();
        assert!(directory(&hub, &alice).await.is_empty());
    }

    /// The search answers from the index alone. Seeded here for a room the registry does not
    /// have at all: the old walk loaded each room, found none, and answered nobody.
    #[tokio::test]
    async fn a_directory_search_answers_from_the_index_without_loading_any_room() {
        let (hub, _rooms) = hub(500);
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let room = ruma::room_id!("!indexed:hub.test");
        hub.store()
            .set_membership(&alice, room, "join", 1, false)
            .await
            .unwrap();
        assert!(
            hub.store()
                .index_room_members_if_absent(room, &[alice.clone(), bob.clone()])
                .await
                .unwrap()
        );
        assert_eq!(directory(&hub, &alice).await, [bob.as_str()]);
        assert_eq!(hub.directory_rooms_walked(), 0);
    }

    /// A room whose updates all came before the index existed is read once, by the first search
    /// that needs it, and from then on kept current by its updates.
    #[tokio::test]
    async fn a_room_from_before_the_index_is_read_once_then_kept_current() {
        let (hub, rooms) = hub(500);
        let alice = user_id!("@alice:hub.test").to_owned();
        let bob = user_id!("@bob:hub.test").to_owned();
        let carol = user_id!("@carol:hub.test").to_owned();
        // Made while nothing watched the room stream: no index, and the records the store had
        // from before are written by hand.
        let (handle, room_id) = create(&rooms, &alice, "private_chat").await;
        invite_and_join(&handle, &alice, &bob, 2).await;
        for user in [&alice, &bob] {
            hub.store()
                .set_membership(user, &room_id, "join", 1, false)
                .await
                .unwrap();
        }
        assert_eq!(hub.store().room_member_ids(&room_id).await.unwrap(), None);

        assert_eq!(directory(&hub, &alice).await, [bob.as_str()]);
        assert_eq!(hub.directory_rooms_walked(), 1);
        assert_eq!(directory(&hub, &bob).await, [alice.as_str()]);
        assert_eq!(hub.directory_rooms_walked(), 1, "read once, not per search");

        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        invite_and_join(&handle, &alice, &carol, 4).await;
        assert_eq!(
            directory(&hub, &alice).await,
            [bob.as_str(), carol.as_str()]
        );
        assert_eq!(hub.directory_rooms_walked(), 1);
    }

    /// The timing behind the known gap's note: one public room of 5,000 members, searched by
    /// somebody outside it. Reading the room's members through the room (what every search did)
    /// against the index. `cargo test -p hs-user --lib directory_search_timing -- --ignored
    /// --nocapture`; the numbers are in `docs/status/05-sync.md`.
    #[tokio::test]
    #[ignore = "a timing, not a check: builds a room of 5,000 members"]
    async fn directory_search_timing_in_a_public_room_of_5000() {
        const MEMBERS: usize = 5_000;
        let (hub, rooms) = hub(500);
        let owner = user_id!("@owner:hub.test").to_owned();
        let searcher = user_id!("@searcher:hub.test").to_owned();
        let (handle, room_id) = create(&rooms, &owner, "public_chat").await;
        let built = std::time::Instant::now();
        for i in 0..MEMBERS {
            let user = UserId::parse(format!("@member{i}:hub.test")).unwrap();
            member(&handle, &user, Action::Join, 2 + i64::try_from(i).unwrap()).await;
        }
        eprintln!("built {MEMBERS} joins in {:?}", built.elapsed());
        // The hub catches up with the room as it stands: the public room list and the index.
        let head = handle.query(|a| a.head_update()).await.unwrap();
        hub.process_room_update(head).await.unwrap();

        let rounds = 20u32;
        let walk = std::time::Instant::now();
        for _ in 0..rounds {
            assert_eq!(
                hub.joined_member_ids(&room_id).await.unwrap().len(),
                MEMBERS + 1
            );
        }
        let walk = walk.elapsed() / rounds;
        // The same walk over a room that is not resident -- after a restart, or evicted --
        // which the search had to load from the store first.
        let cold = std::time::Instant::now();
        for _ in 0..rounds {
            rooms.forget_resident(&room_id).await;
            assert_eq!(
                hub.joined_member_ids(&room_id).await.unwrap().len(),
                MEMBERS + 1
            );
        }
        let cold = cold.elapsed() / rounds;
        let indexed = std::time::Instant::now();
        for _ in 0..rounds {
            // The search itself: nothing here consumes the room stream, so the wait for the
            // hub to catch up (`settle_before_read`) would run out its bound every time.
            assert_eq!(
                hub.directory_from_index(&searcher).await.unwrap().len(),
                MEMBERS + 1
            );
        }
        let indexed = indexed.elapsed() / rounds;
        assert_eq!(hub.directory_rooms_walked(), 0);
        eprintln!(
            "per search: reading the resident room {walk:?}, loading and reading it {cold:?}, \
             from the index {indexed:?}"
        );
    }

    /// A feed that has grown to twice the hub's retention since its floor is compacted after
    /// the fan-out that took it there, and the hot-room stream likewise: each stays between
    /// one and two retentions long, with each room's last position kept below the floor.
    #[tokio::test]
    async fn feeds_and_the_hot_stream_past_twice_their_retention_are_compacted() {
        let (hub, rooms) = hub(usize::MAX);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        hub.set_retention(2, 0);
        assert_eq!((hub.feed_retention(), hub.hot_stream_retention()), (2, 0));
        let alice = user_id!("@alice:hub.test").to_owned();
        let did: &ruma::DeviceId = "DEV".into();
        let handle = rooms
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        for i in 0..8 {
            // Each message pins the last entry (a sync handed out a token at it), so the next
            // is a new row rather than coalesced.
            let latest = hub.store().latest_feed_seq(&alice).await.unwrap();
            hub.store()
                .record_device_cursor(&alice, did, latest)
                .await
                .unwrap();
            handle
                .send_event(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"body": format!("{i}")}),
                    None,
                    2 + i,
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let latest = hub.store().latest_feed_seq(&alice).await.unwrap();
        let floor = hub.store().feed_floor(&alice).await.unwrap();
        // The create's entry may be coalesced with the first message's (the cursor was read
        // before the hub had the create), so eight or nine.
        assert!(latest >= 8, "the create and eight messages: {latest}");
        assert!(floor > 0, "compacted at least once");
        assert!(
            latest - floor <= 4,
            "between one and two retentions: {latest} - {floor}"
        );
        let entries = hub.store().feed_since(&alice, 0).await.unwrap();
        assert!(
            entries.len() <= 5,
            "one kept, at most four above: {entries:?}"
        );
        assert_eq!(
            hub.store()
                .room_pos_at_token(&alice, &room_id, latest)
                .await
                .unwrap(),
            Some(entries.last().unwrap().room_pos)
        );

        // The hot-room stream: a threshold of one makes the room hot with the second member.
        let (hub, rooms) = self::tests::hub(1);
        std::mem::forget(hub.watch_all(rooms.subscribe_global()));
        hub.set_retention(0, 2);
        let bob = user_id!("@bob:hub.test").to_owned();
        let handle = rooms
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        handle
            .membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        for i in 0..8 {
            handle
                .send_event(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"body": format!("{i}")}),
                    None,
                    3 + i,
                )
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let head = hub.store().latest_hot_seq().await.unwrap();
        assert!(head >= 9, "the join and eight messages: {head}");
        assert_eq!(
            hub.store().latest_hot_seq_of_room(&room_id).await.unwrap(),
            Some(head),
            "the newest entry is never pruned"
        );
        assert_eq!(
            hub.store().hot_room_pos_as_of(&room_id, 1).await.unwrap(),
            None,
            "the first entries are gone"
        );
        // The hub compacts once the stream has grown by twice the retention since it last
        // did, so what a compaction now could still take is at most that much.
        assert!(
            hub.store().compact_hot_stream(2).await.unwrap() <= 4,
            "between one and two retentions"
        );
    }
}
