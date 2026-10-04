//! [`UserStore`]: durable storage for one user's session state -- the coalesced feed, current
//! room memberships, per-device cursors and account data. `crate::store::tables::TablesUserStore`
//! is the one implementation, `hs-kv`/`hs-tables`-backed (the pattern
//! `hs_auth::store::tables::TablesAuthStore` established: read that module's docs first).
//!
//! # Why a trait, not just the concrete store
//!
//! `crate::hub::SessionHub` holds this as `Arc<dyn UserStore>` (mirroring
//! `hs_auth::state::AuthState`'s `Arc<dyn AuthStore>`) so a test can swap in a fake without
//! standing up a real `hs-kv` backend, and so a future non-`hs-kv` implementation (a distributed
//! cache in front of the durable store, say) is a new impl of this trait, not a rewrite of every
//! call site.
//!
//! # Everything here is derivable from the store
//!
//! `PLAN.md` section 5.4: "Everything the user session holds is derivable from room positions
//! and the feed" -- concretely, every field on [`crate::hub::UserSessionActor`]'s in-memory state
//! is reconstructible from a call or two into this trait, which is what makes a session actor
//! safe to drop and recreate on restart or failover (`crate::hub::SessionHub::get_or_create`).

pub mod tables;

use ruma::OwnedRoomId;
use serde::{Deserialize, Serialize};

/// One entry in a user's durable, coalesced feed: "as of `feed_seq`, `room_id` was last known to
/// be at `room_pos`". See `crate::token`'s module docs for why `feed_seq` (not a per-room
/// position vector) is what `SyncToken` carries, and `crate::store::tables`'s module docs for the
/// coalescing invariant that keeps this table's growth bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedEntry {
    /// This feed entry's position in the user's feed.
    pub feed_seq: u64,
    /// Which room changed.
    pub room_id: OwnedRoomId,
    /// The room-local position (`hs_room`'s `room_pos`) the update was published at.
    pub room_pos: i64,
}

/// A user's current (not historical) membership in one room, as last observed from a
/// `hs_room::protocol::RoomUpdate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipRecord {
    /// The room.
    pub room_id: OwnedRoomId,
    /// `"join"`, `"invite"`, `"knock"`, `"leave"` or `"ban"` -- verbatim from the `m.room.member`
    /// event's `membership` field, not re-typed as an enum, so a membership value this crate does
    /// not specifically branch on (there are none today, but the spec could add one) round-trips
    /// rather than being silently dropped.
    pub membership: String,
    /// The room-local position of the event that set this membership.
    pub room_pos: i64,
    /// Whether the room's member count was at or above the fan-out-on-read threshold the moment
    /// this membership was last touched -- `crate::hub`'s module docs, "The hybrid fan-out
    /// threshold".
    pub hot_room: bool,
}

/// One member's part of a room update's fan-out ([`UserStore::apply_fan_out`]): what
/// `crate::hub::SessionHub` decided to write for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanOutWrite {
    /// The member.
    pub user_id: ruma::OwnedUserId,
    /// The membership record to write, as `(membership, baseline room_pos)`
    /// ([`UserStore::set_membership`]'s arguments), or `None` to leave the record as it is.
    pub record: Option<(String, i64)>,
    /// The record's `hot_room` flag ([`MembershipRecord::hot_room`]); only read with `record`.
    pub hot_room: bool,
    /// Whether to append a feed entry at the update's position
    /// ([`UserStore::append_feed_entry`]).
    pub feed_entry: bool,
}

/// What [`UserStore::apply_fan_out`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FanOutReport {
    /// Membership records written.
    pub records: usize,
    /// Feed entries appended or coalesced.
    pub feed_entries: usize,
    /// Store transactions it took.
    pub transactions: usize,
    /// Members written one at a time after their batch's transaction kept conflicting.
    pub fallbacks: usize,
    /// Users whose feed has outgrown its retention and should be compacted
    /// ([`UserStore::compact_feed`]).
    pub feeds_to_compact: Vec<ruma::OwnedUserId>,
}

/// One piece of account data (global or room-scoped), with the counter value it was last written
/// at -- see `crate::store::tables`'s module docs for how this drives "what account data changed
/// since token T".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDataRecord {
    /// The `m.*` (or custom) event type this account data is stored under.
    pub event_type: String,
    /// The account data content.
    pub content: serde_json::Value,
    /// The account-data counter value as of this write.
    pub changed_seq: u64,
}

/// One room directory row, as `GET /publicRooms` reports it (the `PublicRoomsChunk` shape the
/// spec defines). Populated by `crate::hub::SessionHub::process_room_update` whenever it observes
/// a room whose current `m.room.join_rules` is `"public"` -- or whose history is
/// `world_readable`, which counts for the *user* directory but not for `/publicRooms`
/// ([`PublicRoomEntry::join_rule_public`] tells the two apart; see
/// [`UserStore::list_public_rooms`] and [`UserStore::list_directory_public_rooms`]). See
/// [`UserStore::list_public_rooms`]'s doc comment for the coverage gap this implies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicRoomEntry {
    /// The room.
    pub room_id: OwnedRoomId,
    /// Whether `m.room.join_rules` is `"public"`: what makes a room belong in `/publicRooms`.
    /// `false` for a row kept only because the room is world-readable. Rows from before this
    /// field existed were all written for public join rules, hence the default.
    #[serde(default = "default_true")]
    pub join_rule_public: bool,
    /// `m.room.name`'s content, if set.
    pub name: Option<String>,
    /// `m.room.topic`'s content, if set.
    pub topic: Option<String>,
    /// `m.room.canonical_alias`'s content, if set.
    pub canonical_alias: Option<String>,
    /// `m.room.avatar`'s content, if set.
    pub avatar_url: Option<String>,
    /// Current joined member count.
    pub num_joined_members: usize,
    /// Whether the room is world-readable (`m.room.history_visibility` is `world_readable`).
    pub world_readable: bool,
    /// Whether guests can join without registering.
    pub guest_can_join: bool,
}

fn default_true() -> bool {
    true
}

/// One user's latest read receipt of one kind in one room, as [`UserStore::put_receipt`] keeps
/// it. Written through by `crate::receipts::ReceiptRegistry` so read state survives a restart;
/// the registry is the only reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredReceipt {
    /// The user whose receipt this is (local or remote).
    pub user_id: String,
    /// The receipt type's wire spelling (`m.read` or `m.read.private`).
    pub kind: String,
    /// The event the receipt points at.
    pub event_id: String,
    /// When the receipt was sent, in milliseconds since the Unix epoch.
    pub ts: u64,
    /// The stamp the registry gave this receipt (`crate::stamp`), kept so that a restarted
    /// process does not report an old receipt as news to a client that already saw it.
    pub seq: u64,
}

/// One user's presence, as [`UserStore::put_presence`] keeps it. Written through by
/// `crate::presence::PresenceRegistry` on every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPresence {
    /// `"online"`, `"unavailable"` or `"offline"`.
    pub presence: String,
    /// The user's free-text status message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_msg: Option<String>,
    /// When the user was last known active, in milliseconds since the Unix epoch.
    pub last_active_ms: u64,
    /// The stamp the registry gave this record (`crate::stamp`).
    pub seq: u64,
    /// What a remote server said about `currently_active`, for a remote user; `None` for a
    /// local user, whose value is derived from `presence`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currently_active: Option<bool>,
}

/// Errors from [`UserStore`]. Distinct from `crate::error::UserError` so this trait does not
/// force every implementation to depend on `hs-http`'s error-mapping types; `crate::error`
/// converts.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The underlying store reported an error.
    #[error(transparent)]
    Kv(#[from] hs_kv::KvError),
    /// A stored key or value could not be decoded.
    #[error(transparent)]
    KeyCodec(#[from] hs_tables::key::KeyCodecError),
    /// A typed-keyspace operation failed.
    #[error(transparent)]
    Table(#[from] hs_tables::keyspace::TableError),
    /// A stored JSON value failed to (de)serialize.
    #[error("decode/encode failure: {0}")]
    Codec(String),
}

/// Durable storage for one server's worth of users' session state. See the module docs.
#[async_trait::async_trait]
pub trait UserStore: Send + Sync {
    /// Appends (or, per the coalescing rule, merges into the still-unconsumed tail entry for)
    /// `room_id`'s new position, returning the feed_seq the write landed at (the merged entry's
    /// existing `feed_seq` if coalesced, a freshly assigned one otherwise).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn append_feed_entry(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
        room_pos: i64,
    ) -> Result<u64, StoreError>;

    /// Every feed entry strictly after `since_feed_seq`, in ascending `feed_seq` order. The
    /// candidate room set for an incremental sync is this list's distinct `room_id`s.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn feed_since(
        &self,
        user_id: &ruma::UserId,
        since_feed_seq: u64,
    ) -> Result<Vec<FeedEntry>, StoreError>;

    /// The highest `feed_seq` this user's feed has ever recorded (`0` if the feed is empty) --
    /// what a full sync's `next_batch` and an incremental sync with nothing new both report as
    /// the caught-up position.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn latest_feed_seq(&self, user_id: &ruma::UserId) -> Result<u64, StoreError>;

    /// The room-local position `room_id` was at, as of the latest feed entry for that room with
    /// `feed_seq <= as_of`. `None` if the user's feed has no entry for that room at or before
    /// `as_of` (either the room did not exist for this user yet, or -- for a room that only
    /// entered the user's feed *after* `as_of` -- the caller should treat this as "resume from the
    /// start", i.e. the room is new to this sync window).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn room_pos_as_of(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
        as_of_feed_seq: u64,
    ) -> Result<Option<i64>, StoreError>;

    /// The room-local position `room_id` was at as of `as_of_feed_seq`, taking the room's
    /// newest feed entry when that entry is at or before `as_of_feed_seq` -- the common case,
    /// one keyed read -- and walking the feed back from `as_of_feed_seq`
    /// ([`UserStore::room_pos_as_of`]) only when the room has moved on past it. What
    /// `crate::sync` bounds a room's timeline with, so that the batch and the token it hands out
    /// describe the same point of the feed. `None` as for [`UserStore::room_pos_as_of`].
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn room_pos_at_token(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
        as_of_feed_seq: u64,
    ) -> Result<Option<i64>, StoreError> {
        if let Some(entry) = self.current_feed_entry(user_id, room_id).await?
            && entry.feed_seq <= as_of_feed_seq
        {
            return Ok(Some(entry.room_pos));
        }
        self.room_pos_as_of(user_id, room_id, as_of_feed_seq).await
    }

    /// The newest feed entry for `room_id` -- the one [`UserStore::append_feed_entry`] would
    /// coalesce into -- or `None` if the room has never had one.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn current_feed_entry(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
    ) -> Result<Option<FeedEntry>, StoreError>;

    /// Records that `device_id` was just handed a `next_batch` token carrying `feed_seq` -- the
    /// safety bound [`UserStore::append_feed_entry`]'s coalescing decision reads
    /// ([`UserStore::max_device_cursor`]). Called once per successful `/sync` response, not on
    /// presentation of a `since` token (see `crate::store::tables`'s module docs for why that
    /// distinction is what makes coalescing safe for an out-of-order or replayed old token).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn record_device_cursor(
        &self,
        user_id: &ruma::UserId,
        device_id: &ruma::DeviceId,
        feed_seq: u64,
    ) -> Result<(), StoreError>;

    /// The maximum `feed_seq` ever recorded by [`UserStore::record_device_cursor`] across every
    /// device this user has (`0` if none have ever synced).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn max_device_cursor(&self, user_id: &ruma::UserId) -> Result<u64, StoreError>;

    /// Records that hot room `room_id` (one above the fan-out threshold, `crate::hub`'s module
    /// docs) reached `room_pos`, at the next position of the server-wide hot-room stream, and
    /// returns that position. One write per update to a hot room, whatever its size -- the
    /// fan-out-on-read counterpart of a feed entry per member. A token's
    /// [`crate::token::SyncToken::hot_seq`] is this stream's position when it was issued, which
    /// is what lets a hot room be resumed from where that token left it
    /// ([`UserStore::hot_room_pos_as_of`]).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn append_hot_position(
        &self,
        room_id: &ruma::RoomId,
        room_pos: i64,
    ) -> Result<u64, StoreError>;

    /// Indexes `room_id`'s joined members (`members`, the room's own answer) for the user
    /// directory -- unless the room is indexed already, in which case nothing is written and
    /// this returns `false`. One transaction: the marker that says a room is indexed is written
    /// with its rows, so a room is either wholly indexed or not at all, and a second indexer
    /// racing the first (the session hub and a search on another replica, say) leaves the
    /// first's rows alone; what changes after that arrives as
    /// [`UserStore::apply_room_member_changes`]. See `crate::hub::SessionHub`'s
    /// `users_visible_in_directory_to`.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn index_room_members_if_absent(
        &self,
        room_id: &ruma::RoomId,
        members: &[ruma::OwnedUserId],
    ) -> Result<bool, StoreError>;

    /// Applies membership changes to an indexed room's joined-member rows: `true` adds the
    /// user, `false` removes them. Returns `false`, writing nothing, if the room is not indexed
    /// ([`UserStore::index_room_members_if_absent`] is then the caller's next step).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn apply_room_member_changes(
        &self,
        room_id: &ruma::RoomId,
        changes: &[(ruma::OwnedUserId, bool)],
    ) -> Result<bool, StoreError>;

    /// Makes an indexed room's joined-member rows exactly `joined`: adds the rows missing and
    /// removes the rows of users not in `joined`. Returns how many rows were added and removed,
    /// or `None`, writing nothing, if the room is not indexed. For a room whose state arrived
    /// whole (joined through another server after an invite, or rejoined after this server was
    /// out of it), where the update's own membership deltas name only the joiner.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn reconcile_room_members(
        &self,
        room_id: &ruma::RoomId,
        joined: &[ruma::OwnedUserId],
    ) -> Result<Option<(usize, usize)>, StoreError>;

    /// An indexed room's joined members; `None` if the room is not indexed.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn room_member_ids(
        &self,
        room_id: &ruma::RoomId,
    ) -> Result<Option<Vec<ruma::OwnedUserId>>, StoreError>;

    /// Drops a room's joined-member rows and its indexed marker (the room was deleted).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn forget_room_members(&self, room_id: &ruma::RoomId) -> Result<(), StoreError>;

    /// The newest position of the hot-room stream, `0` if nothing was ever appended to it.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn latest_hot_seq(&self) -> Result<u64, StoreError>;

    /// The newest position hot room `room_id` was recorded at with a stream position at or
    /// before `as_of_hot_seq`; `None` if it has none that early.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn hot_room_pos_as_of(
        &self,
        room_id: &ruma::RoomId,
        as_of_hot_seq: u64,
    ) -> Result<Option<i64>, StoreError>;

    /// The stream position of `room_id`'s newest hot-room entry; `None` if it has never had
    /// one (it has never been hot, or has had no update since this stream was introduced).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn latest_hot_seq_of_room(
        &self,
        room_id: &ruma::RoomId,
    ) -> Result<Option<u64>, StoreError>;

    /// The current value of this user's account-data change counter (shared by global and
    /// room-scoped account data -- see [`UserStore::put_global_account_data`]'s doc comment),
    /// `0` if no account data has ever been written. What a sync response's `account_data_seq`
    /// cursor is set to, and what `crate::sync`'s long-poll wake condition compares a presented
    /// token's cursor against.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn latest_account_data_seq(&self, user_id: &ruma::UserId) -> Result<u64, StoreError>;

    /// Records `user_id`'s current membership in `room_id`. Idempotent: overwrites whatever was
    /// there.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn set_membership(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
        membership: &str,
        room_pos: i64,
        hot_room: bool,
    ) -> Result<(), StoreError>;

    /// `user_id`'s current membership in `room_id`, if this store has ever recorded one.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn get_membership(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
    ) -> Result<Option<MembershipRecord>, StoreError>;

    /// Every room this user has a recorded membership in, regardless of its value (join, invite,
    /// knock, leave or ban) -- the full candidate set for an initial sync.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn list_memberships(
        &self,
        user_id: &ruma::UserId,
    ) -> Result<Vec<MembershipRecord>, StoreError>;

    /// [`UserStore::get_membership`] for every user in `user_ids` at once, in the same order:
    /// one read of the store rather than one per user, which is what a room update's fan-out
    /// (`crate::hub::SessionHub`) asks for each of a room's members. The default is the
    /// one-at-a-time loop; `tables::TablesUserStore` makes it one multi-get.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn get_memberships(
        &self,
        room_id: &ruma::RoomId,
        user_ids: &[ruma::OwnedUserId],
    ) -> Result<Vec<Option<MembershipRecord>>, StoreError> {
        let mut out = Vec::with_capacity(user_ids.len());
        for user_id in user_ids {
            out.push(self.get_membership(user_id, room_id).await?);
        }
        Ok(out)
    }

    /// Writes one room update's fan-out -- each member's new membership record, if any, and a
    /// feed entry at `room_pos` for those that get one -- in as few transactions as the store
    /// can manage, rather than one or two per member. The membership records are
    /// [`UserStore::set_membership`]'s and the feed entries [`UserStore::append_feed_entry`]'s,
    /// with the same coalescing rule; only the number of round trips differs. The default is
    /// the one-at-a-time loop, which is also what `tables::TablesUserStore` falls back to for a
    /// batch whose transaction keeps conflicting.
    ///
    /// `feed_retention` is the number of feed entries a user keeps
    /// (`crate::hub::SessionHub::feed_retention`): a user whose feed has grown to twice that
    /// since it was last compacted is named in [`FanOutReport::feeds_to_compact`], for the
    /// caller to [`UserStore::compact_feed`]. `0` never names anyone.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn apply_fan_out(
        &self,
        room_id: &ruma::RoomId,
        room_pos: i64,
        writes: &[FanOutWrite],
        feed_retention: u64,
    ) -> Result<FanOutReport, StoreError> {
        let _ = feed_retention;
        let mut report = FanOutReport::default();
        for write in writes {
            if let Some((membership, baseline_pos)) = &write.record {
                self.set_membership(
                    &write.user_id,
                    room_id,
                    membership,
                    *baseline_pos,
                    write.hot_room,
                )
                .await?;
                report.records += 1;
            }
            if write.feed_entry {
                self.append_feed_entry(&write.user_id, room_id, room_pos)
                    .await?;
                report.feed_entries += 1;
            }
            report.transactions += usize::from(write.record.is_some() || write.feed_entry);
        }
        Ok(report)
    }

    /// Compacts `user_id`'s feed so that at most `keep` entries stay above its *floor*: every
    /// entry at or below the new floor is deleted except the newest one per room, which stays
    /// as the room's position as of the floor, and the floor is recorded
    /// ([`UserStore::feed_floor`]). A token with a `feed_seq` below the floor still finds every
    /// room that changed after it (the kept entry is at or after any deleted one), but may
    /// have lost the room's position as of itself, in which case `crate::sync` sends the room
    /// whole. Returns how many entries were deleted; `0` when the feed is within `keep` of its
    /// floor already. A store with no feed retention does nothing.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn compact_feed(&self, user_id: &ruma::UserId, keep: u64) -> Result<usize, StoreError> {
        let _ = (user_id, keep);
        Ok(0)
    }

    /// `user_id`'s feed floor: the `feed_seq` at or below which [`UserStore::compact_feed`]
    /// has compacted the feed to one entry per room. A token's `feed_seq` below it is older
    /// than the feed remembers. `0` for a feed never compacted.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn feed_floor(&self, user_id: &ruma::UserId) -> Result<u64, StoreError> {
        let _ = user_id;
        Ok(0)
    }

    /// Compacts the server-wide hot-room stream ([`UserStore::append_hot_position`]) so that
    /// at most `keep` entries stay above its floor, the same way [`UserStore::compact_feed`]
    /// does a feed: below the floor, one entry per room (its position as of the floor) stays.
    /// A token's `hot_seq` below the floor resumes a hot room from that kept position, or whole
    /// if the room's kept entry is newer than the token. Returns how many entries were deleted.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn compact_hot_stream(&self, keep: u64) -> Result<usize, StoreError> {
        let _ = keep;
        Ok(0)
    }

    /// Sets one piece of global (not room-scoped) account data, bumping this user's account-data
    /// counter and stamping the write with the new value.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn put_global_account_data(
        &self,
        user_id: &ruma::UserId,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<u64, StoreError>;

    /// Every piece of global account data this user has, regardless of when it last changed.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn list_global_account_data(
        &self,
        user_id: &ruma::UserId,
    ) -> Result<Vec<AccountDataRecord>, StoreError>;

    /// One piece of global account data.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn get_global_account_data(
        &self,
        user_id: &ruma::UserId,
        event_type: &str,
    ) -> Result<Option<AccountDataRecord>, StoreError>;

    /// Sets one piece of room-scoped account data (`m.tag` and friends), bumping the same
    /// per-user counter [`UserStore::put_global_account_data`] does (one counter for both scopes:
    /// simpler, and the spec's `account_data.changed`/room `account_data.events` distinction is
    /// already made by which map a caller reads, not by the counter).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn put_room_account_data(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<u64, StoreError>;

    /// Every piece of room-scoped account data this user has for `room_id`.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn list_room_account_data(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
    ) -> Result<Vec<AccountDataRecord>, StoreError>;

    /// Stores a named filter (`POST /user/{userId}/filter`), returning the id it was assigned.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn put_filter(
        &self,
        user_id: &ruma::UserId,
        filter_json: serde_json::Value,
    ) -> Result<String, StoreError>;

    /// Stores a named filter under an id another server assigned it -- the Synapse importer
    /// (`hs_compat::migration`), so that a client which uploaded filter `0` to Synapse finds it
    /// at `0` here, as `GET /user/{userId}/filter/0` and `/sync?filter=0`. Overwrites whatever
    /// was stored under that id. Ids [`UserStore::put_filter`] assigns are sixteen alphanumeric
    /// characters, so they never collide with Synapse's, which are decimal numbers.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn import_filter(
        &self,
        user_id: &ruma::UserId,
        filter_id: &str,
        filter_json: serde_json::Value,
    ) -> Result<(), StoreError>;

    /// Retrieves a previously stored filter by id.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn get_filter(
        &self,
        user_id: &ruma::UserId,
        filter_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError>;

    /// Records or refreshes a room's public-directory entry.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn upsert_public_room(&self, entry: PublicRoomEntry) -> Result<(), StoreError>;

    /// Removes a room from the public directory (it stopped being public, or was destroyed).
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn remove_public_room(&self, room_id: &ruma::RoomId) -> Result<(), StoreError>;

    /// Every room this process's directory currently believes is public: join rule `public`
    /// (what `/publicRooms` lists; a world-readable room with another join rule is not here,
    /// see [`UserStore::list_directory_public_rooms`]). **Global, not
    /// per-user**, and -- like every room this crate learns about at all -- only as complete as
    /// `crate::hub::SessionHub::watch_room` coverage is (`crate::hub`'s module docs, "The
    /// discovery gap"): a public room this process has never been told to watch never appears
    /// here. Not paginated (`GET /publicRooms`'s `since`/`limit` are applied by
    /// `crate::routes::public_rooms` over this full list) -- acceptable for the directory sizes
    /// this gap already bounds us to.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn list_public_rooms(&self) -> Result<Vec<PublicRoomEntry>, StoreError>;

    /// Every room whose members the user directory offers to everybody: join rule `public`, or
    /// history visibility `world_readable` (Synapse's `users_in_public_rooms` counts both; so
    /// does Sytest's "Users stay in directory when join_rules are changed but
    /// history_visibility is world_readable"). A superset of [`UserStore::list_public_rooms`],
    /// with the same coverage caveat.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn list_directory_public_rooms(&self) -> Result<Vec<PublicRoomEntry>, StoreError>;

    /// Records `receipt` as the latest of its user and kind in `room_id`, replacing any earlier
    /// one.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn put_receipt(
        &self,
        room_id: &ruma::RoomId,
        receipt: &StoredReceipt,
    ) -> Result<(), StoreError>;

    /// Every receipt recorded in `room_id`, in no particular order.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn list_room_receipts(
        &self,
        room_id: &ruma::RoomId,
    ) -> Result<Vec<StoredReceipt>, StoreError>;

    /// Records `presence` as `user_id`'s current presence, replacing any earlier record.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn put_presence(
        &self,
        user_id: &ruma::UserId,
        presence: &StoredPresence,
    ) -> Result<(), StoreError>;

    /// `user_id`'s recorded presence, if any was ever recorded.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn get_presence(
        &self,
        user_id: &ruma::UserId,
    ) -> Result<Option<StoredPresence>, StoreError>;

    /// Receipts written after stream position `since`, oldest first, capped at `limit`. The
    /// server-wide receipt stream: every [`UserStore::put_receipt`] appends one entry, in the
    /// same transaction as the receipt, for a reader that has to follow every room's receipts
    /// from a durable position -- appservice delivery (MSC2409). A room's receipts are a per-room
    /// read (`list_room_receipts`); nothing else orders receipts across rooms.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn receipt_stream_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<ReceiptStreamEntry>, StoreError>;

    /// The position of the newest receipt stream entry, or `0` if no receipt was ever written.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn receipt_stream_head(&self) -> Result<u64, StoreError>;

    /// Deletes receipt stream entries below `below` and returns how many. The receipts
    /// themselves are untouched.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn prune_receipt_stream(&self, below: u64) -> Result<usize, StoreError>;

    /// Presence changes written after stream position `since`, oldest first, capped at `limit`.
    /// The server-wide presence stream, the counterpart of [`UserStore::receipt_stream_since`]:
    /// [`UserStore::put_presence`] appends an entry when the record's stamp (`seq`) changed --
    /// not for a `last_active` refresh written under the same stamp.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn presence_stream_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<PresenceStreamEntry>, StoreError>;

    /// The position of the newest presence stream entry, or `0` if none was ever written.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn presence_stream_head(&self) -> Result<u64, StoreError>;

    /// Deletes presence stream entries below `below` and returns how many.
    ///
    /// # Errors
    /// Returns [`StoreError`] on a storage failure.
    async fn prune_presence_stream(&self, below: u64) -> Result<usize, StoreError>;
}

/// One entry of the server-wide receipt stream ([`UserStore::receipt_stream_since`]): the
/// receipt as it was written, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptStreamEntry {
    /// This entry's position in the stream: strictly increasing, one per receipt written.
    pub pos: u64,
    /// The room the receipt is in.
    pub room_id: String,
    /// The receipt, as [`UserStore::put_receipt`] was given it.
    pub receipt: StoredReceipt,
}

/// One entry of the server-wide presence stream ([`UserStore::presence_stream_since`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceStreamEntry {
    /// This entry's position in the stream: strictly increasing, one per presence change.
    pub pos: u64,
    /// Whose presence changed. The record itself is read with [`UserStore::get_presence`]: the
    /// current one is what a reader wants, not the one that was current at this position.
    pub user_id: String,
}

/// Shorthand for the trait-object form every consumer (`crate::hub::SessionHub`,
/// `crate::state::UserState`) actually holds.
pub type DynUserStore = std::sync::Arc<dyn UserStore>;
