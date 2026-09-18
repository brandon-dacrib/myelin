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
/// a room whose current `m.room.join_rules` is `"public"`; see [`UserStore::list_public_rooms`]'s
/// doc comment for the coverage gap this implies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicRoomEntry {
    /// The room.
    pub room_id: OwnedRoomId,
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

    /// Every room this process's directory currently believes is public. **Global, not
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
}

/// Shorthand for the trait-object form every consumer (`crate::hub::SessionHub`,
/// `crate::state::UserState`) actually holds.
pub type DynUserStore = std::sync::Arc<dyn UserStore>;
