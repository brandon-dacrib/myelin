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
//! *stops* writing feed entries for it, leaning on `crate::sync`'s incremental-sync path to check
//! a hot room's live position directly (fan-out on read) instead of trusting the feed to have
//! recorded it. A room's `hot`-ness is recomputed on every update from its live member count, so
//! it can flip in either direction without an explicit migration step.
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

use std::collections::HashMap;
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

use crate::error::UserError;
use crate::presence::PresenceRegistry;
use crate::receipts::{ReceiptKind, ReceiptRegistry};
use crate::room_source::RoomSource;
use crate::store::DynUserStore;
use crate::token::SyncToken;
use crate::typing::TypingRegistry;

fn membership_of(event: &Event) -> Option<String> {
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
/// (`m.room.join_rules`'s `join_rule` is `"public"`), `None` otherwise -- the caller then removes
/// any existing directory entry in the `None` case (a room can stop being public).
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

    if field("m.room.join_rules", "join_rule")?.as_deref() != Some("public") {
        return Ok(None);
    }
    let world_readable = field("m.room.history_visibility", "history_visibility")?.as_deref()
        == Some("world_readable");
    let guest_can_join =
        field("m.room.guest_access", "guest_access")?.as_deref() == Some("can_join");
    Ok(Some(crate::store::PublicRoomEntry {
        room_id: actor.room_id().to_owned(),
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
        if let Some(pos) = self
            .store
            .room_pos_as_of(user_id, room_id, token.feed_seq)
            .await
            .map_err(to_internal)?
        {
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

/// Implements [`hs_e2e::state::SyncTokenResolver`] for `GET /keys/changes`: decodes `raw` as this
/// crate's own [`SyncToken`] and reports its `device_list_seq` field directly, with no store
/// lookup at all -- unlike [`FeedTokenResolver`] (which needs `crate::store::UserStore` to turn a
/// `feed_seq` into a room-local position), a device-list stream position *is* one of the token's
/// own fields verbatim, so decoding the token answers the question outright. `user_id` is unused
/// (a `SyncToken` carries no user scope of its own; the caller already knows whose token this is
/// from the authenticated request), kept only to satisfy the trait signature.
///
/// Installed by [`SessionHub::install_device_list_token_resolver`] -- see that method's doc
/// comment for why installation is a separate call rather than a side effect of
/// [`SessionHub::new`] the way [`FeedTokenResolver`] is (this one needs an `E2eState` handle that
/// `new` does not take).
struct DeviceListTokenResolver;

#[async_trait::async_trait]
impl hs_e2e::state::SyncTokenResolver for DeviceListTokenResolver {
    async fn resolve_device_list_position(
        &self,
        _user_id: &UserId,
        raw: &str,
    ) -> Result<Option<u64>, hs_e2e::error::E2eError> {
        Ok(SyncToken::decode(raw).ok().map(|t| t.device_list_seq))
    }
}

/// The per-process hub: one [`crate::store::UserStore`] shared by every user, a [`RoomSource`]
/// for querying room member lists, and the in-memory wakers `/sync` long-polls block on.
///
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
    /// In-memory `m.typing` state. See [`crate::typing`]'s module docs for why this lives here
    /// rather than in `store`: ephemeral, never persisted, and this hub is already the one place
    /// that both knows how to reach a room's member list and owns the wakers a change needs to
    /// touch.
    typing: TypingRegistry,
    /// In-memory `m.presence` state. See [`crate::presence`]'s module docs.
    presence: PresenceRegistry,
    /// In-memory `m.receipt` state. See [`crate::receipts`]'s module docs.
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
        Self {
            store,
            rooms,
            fan_out_threshold,
            wakers: Mutex::new(HashMap::new()),
            typing: TypingRegistry::new(),
            presence: PresenceRegistry::new(),
            receipts: ReceiptRegistry::new(),
            push_rules: OnceLock::new(),
            counts: OnceLock::new(),
            _marker: std::marker::PhantomData,
        }
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

    /// The installed counts store, if any -- see [`SessionHub::install_counts_store`].
    #[must_use]
    pub fn counts_store(&self) -> Option<&Arc<dyn CountsStore>> {
        self.counts.get()
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
    pub fn install_device_list_token_resolver(&self, e2e: &hs_e2e::state::E2eState<B>) {
        e2e.install_sync_token_resolver(Arc::new(DeviceListTokenResolver));
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

    async fn wake(&self, user_id: &UserId) {
        let waker = {
            let wakers = self.wakers.lock().await;
            wakers.get(user_id).cloned()
        };
        if let Some(waker) = waker {
            waker.notify_waiters();
        }
    }

    /// `room_id`'s current joined members, parsed as user ids (a state key that fails to parse --
    /// should not happen for anything this server itself wrote -- is skipped rather than failing
    /// the whole call). Shared by [`SessionHub::set_typing`] and [`SessionHub::set_presence`]: both
    /// need "who should be woken by this change", and both mean exactly this.
    async fn joined_member_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>, UserError> {
        let handle = self.rooms.get_or_load(room_id).await?;
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
        for member in self.joined_member_ids(room_id).await? {
            self.wake(&member).await;
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
        self.presence.set(user_id, presence, status_msg).await;
        for other in self.users_sharing_room_with(user_id).await? {
            self.wake(&other).await;
        }
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
        if !self.presence.touch(user_id, presence).await {
            return Ok(());
        }
        for other in self.users_sharing_room_with(user_id).await? {
            self.wake(&other).await;
        }
        Ok(())
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
        self.receipts
            .set(room_id, user_id, kind, event_id, ts)
            .await;
        for member in self.joined_member_ids(room_id).await? {
            self.wake(&member).await;
        }
        Ok(())
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
    /// # Errors
    /// Returns [`UserError`] if a room could not be loaded.
    pub async fn users_sharing_room_with(
        &self,
        user_id: &UserId,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, UserError> {
        let mut shared = std::collections::BTreeSet::new();
        for m in self.store.list_memberships(user_id).await? {
            if m.membership != "join" {
                continue;
            }
            for member in self.joined_member_ids(&m.room_id).await? {
                if member.as_str() != user_id.as_str() {
                    shared.insert(member);
                }
            }
        }
        Ok(shared)
    }

    /// Everyone `user_id` may find in the user directory: the people they share a joined room
    /// with, and everyone joined to a public room (one whose join rule is `public`) -- the spec's
    /// floor for `POST /user_directory/search`, and this server's ceiling.
    ///
    /// Computed per search by walking rooms, which is the honest cost of having no directory
    /// table: fine at the size this server runs at today, and the first thing to replace with
    /// one if a public room ever has tens of thousands of members.
    ///
    /// # Errors
    /// Returns [`UserError`] if a membership list or a room could not be read.
    pub async fn users_visible_in_directory_to(
        &self,
        user_id: &UserId,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, UserError> {
        let mut visible = self.users_sharing_room_with(user_id).await?;
        for room in self.store.list_public_rooms().await? {
            for member in self.joined_member_ids(&room.room_id).await? {
                if member.as_str() != user_id.as_str() {
                    visible.insert(member);
                }
            }
        }
        Ok(visible)
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
                    if let Err(e) = self.process_room_update(update).await {
                        tracing::warn!(error = %e, "failed to process a room update into user feeds");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // A slow consumer missed `skipped` publishes. There is no way to recover the
                    // exact events from the channel, but nothing is actually lost: the room
                    // actor's own store still has every position, so the next update this hub
                    // *does* see will append (or coalesce into) a feed entry carrying the room's
                    // then-current `room_pos`, and any user who syncs in between reads the room's
                    // live state directly. Logged, not silently dropped.
                    tracing::warn!(skipped, "session hub lagged behind a room's publish stream");
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
        let handle = self.rooms.get_or_load(&update.room_id).await?;
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

        let hot = member_count > self.fan_out_threshold;

        let mut targets: HashMap<OwnedUserId, String> = active_members
            .into_iter()
            .filter(|(_, m)| is_active(m))
            .collect();
        for delta in &update.membership_deltas {
            targets.insert(delta.user_id.clone(), delta.membership.clone());
        }

        // Somebody joining enters the presence audience of everyone already here, whose tokens
        // may well be newer than the joiner's last presence change. Restamp it so that it
        // reaches them (`PresenceRegistry::restamp`); the wake below is the same one the join
        // itself causes. Complement's "Existing members see new members' presence" is this.
        for delta in &update.membership_deltas {
            if delta.membership == "join" {
                self.presence.restamp(&delta.user_id).await;
            }
        }

        for (user_id, membership) in &targets {
            let changed_now = update
                .membership_deltas
                .iter()
                .any(|d| &d.user_id == user_id);
            let missing = self
                .store
                .get_membership(user_id, &update.room_id)
                .await?
                .is_none();
            if changed_now || missing {
                // A membership record's `room_pos` is a resume baseline, and `hs_room`'s forward
                // pagination is *exclusive* of it: whatever sits at that position counts as
                // already delivered. When this update is the user's own membership change, its
                // position is exactly right. When we are only backfilling a record that went
                // missing -- the room existed before anything watched its publish stream, say --
                // this update is an ordinary event the user has *not* seen, so claiming its
                // position would make the next incremental sync skip it. Back off by one so the
                // fallback can only ever repeat an event, never lose one, which is the direction
                // `crate::sync::resume_mode` documents as the safe one.
                let baseline_pos = if changed_now {
                    update.room_pos
                } else {
                    update.room_pos.saturating_sub(1)
                };
                self.store
                    .set_membership(user_id, &update.room_id, membership, baseline_pos, hot)
                    .await?;
            }
            if !hot {
                self.store
                    .append_feed_entry(user_id, &update.room_id, update.room_pos)
                    .await?;
            }
            self.wake(user_id).await;
        }

        Ok(())
    }

    /// A room's current member count, for callers (`crate::sync`'s hot-room fallback) that need
    /// to decide whether to check a room's live position directly rather than trusting the feed.
    ///
    /// # Errors
    /// Returns [`UserError`] if the room could not be loaded.
    pub async fn room_member_count(&self, room_id: &RoomId) -> Result<usize, UserError> {
        let handle = self.rooms.get_or_load(room_id).await?;
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
}
