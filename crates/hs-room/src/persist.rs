//! Durable storage: `hs-kv`/`hs-tables`-backed keyspaces for events, the timeline, extremities,
//! membership and aliases. See `crate::actor` for how these are used inside one persist
//! transaction.

use hs_kv::KvBackend;
use hs_tables::interning::InternTable;
use hs_tables::keyspace::TypedKeyspace;

/// A `(RoomSn, EventSn)` timeline entry: room-local, monotonic, negative for backfilled events.
pub type TimelineKey = (hs_model::RoomSn, i64);

/// `event_sn -> persisted event bytes` (JSON-encoded [`PersistedEvent`]).
pub type EventKey = (hs_model::EventSn,);

/// `(RoomSn, EventSn)`: a room's forward or backward extremity set.
pub type ExtremityKey = (hs_model::RoomSn, hs_model::EventSn);

/// `alias -> RoomSn`: the local alias directory.
pub type AliasKey = (String,);

/// `(RoomSn, alias)`: the reverse index, for listing a room's aliases.
pub type RoomAliasKey = (hs_model::RoomSn, String);

/// `(RoomSn,)`: a room's fixed metadata (room version), set once at creation.
pub type RoomMetaKey = (hs_model::RoomSn,);

/// `(RoomSn,) -> b""`: presence means this room is currently published to the server's room
/// directory (`PUT /_matrix/client/v3/directory/list/room/{roomId}`). Absence means private (the
/// spec's default) -- deleting the key on unpublish, not writing a "private" marker, keeps
/// "published" a simple presence check with no value to decode.
pub type PublicRoomKey = (hs_model::RoomSn,);

/// `(user_id, RoomSn) -> b""`: presence means `user_id`'s current membership in that room is
/// `join`. Maintained by [`crate::actor::RoomActor::persist`] alongside every `m.room.member`
/// event it writes (inserted when the new membership is `join`, deleted otherwise), so
/// [`crate::actor::rooms_joined_by_user`] can answer "every room this user is joined to" with a
/// prefix scan instead of loading (or even knowing about) every room this server hosts. This is
/// the index profile-change propagation needs: re-stamping a user's `m.room.member` event in
/// every room they are joined to (`crates/hs-room/src/routes/profile.rs`) is only tractable
/// without it by scanning every room on the server, which does not scale and (per
/// `docs/status/04-room-and-events.md`, session 4) this crate has never had a way to do anyway.
/// Keyed by `user_id` first (not `RoomSn` first) specifically so a prefix scan over one user's
/// entries is a contiguous range, independent of how many *other* users are also indexed.
pub type UserJoinedRoomKey = (String, hs_model::RoomSn);

/// `(RoomSn,) -> RoomBlock`: presence means this room is currently blocked by a server
/// administrator (`hs-admin`'s `rooms.set_blocked`, `PUT /api/v1/rooms/{room_id}` in RFC 0004
/// terms). See [`RoomBlock`] and `crate::actor::{set_room_blocked, room_block_reason}`.
pub type BlockedRoomKey = (hs_model::RoomSn,);

/// One event as stored durably: enough to reconstruct an [`hs_model::event::Event`] (via
/// [`hs_model::event::Event::parse`] on `json`) plus the room-local bookkeeping
/// [`hs_model::event::EventHeader`] does not carry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedEvent {
    /// The room this event belongs to.
    pub room_id: String,
    /// The event's full JSON (unredacted), as accepted.
    pub json: serde_json::Value,
    /// The room version this event was validated against.
    pub room_version: String,
    /// Internal processing flags, encoded as [`hs_model::event::EventFlags::to_byte`].
    pub flags: u8,
    /// This event's room-local timeline position, if it is part of the timeline (state-only
    /// outliers are not).
    pub room_pos: Option<i64>,
}

/// A room's fixed metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomMeta {
    /// The room's full ID.
    pub room_id: String,
    /// The room version.
    pub room_version: String,
}

/// The value stored at a [`BlockedRoomKey`]: the optional human-readable reason an administrator
/// gave when blocking the room (`rooms.set_blocked`'s request body). Presence of the row is what
/// means "blocked" -- an absent row is "not blocked", not "blocked with no reason" -- so this
/// struct only needs to carry the reason.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RoomBlock {
    /// Why the room was blocked, if the administrator gave one.
    pub reason: Option<String>,
}

/// Every `hs-kv` keyspace and interning table [`crate::actor::RoomActor`] persists through,
/// opened once and shared (cheap to clone: every field is an interning table or keyspace handle,
/// both cheap-to-clone per `hs-tables`' and `hs-kv`'s own contracts).
#[derive(Clone)]
pub struct Tables<B: KvBackend> {
    /// Room ID <-> `RoomSn` (`hs-tables`' shared interning table).
    pub room_sn: InternTable<B::Keyspace, hs_model::RoomSn>,
    /// Event ID <-> `EventSn`.
    pub event_sn: InternTable<B::Keyspace, hs_model::EventSn>,
    /// `event_sn -> PersistedEvent`.
    pub events: TypedKeyspace<B::Keyspace, EventKey>,
    /// `(RoomSn, room_pos) -> EventSn` (8-byte big-endian value).
    pub timeline: TypedKeyspace<B::Keyspace, TimelineKey>,
    /// `(RoomSn, EventSn) -> b""`: forward extremities.
    pub extremities_fwd: TypedKeyspace<B::Keyspace, ExtremityKey>,
    /// `(RoomSn, EventSn) -> b""`: backward extremities.
    pub extremities_bwd: TypedKeyspace<B::Keyspace, ExtremityKey>,
    /// `alias -> RoomSn` (8-byte big-endian value).
    pub aliases: TypedKeyspace<B::Keyspace, AliasKey>,
    /// `(RoomSn, alias) -> b""`.
    pub room_aliases: TypedKeyspace<B::Keyspace, RoomAliasKey>,
    /// `(RoomSn,) -> RoomMeta`.
    pub room_meta: TypedKeyspace<B::Keyspace, RoomMetaKey>,
    /// `(RoomSn, target_event_sn, rel_type, child_event_sn) -> b""`: `m.relates_to` index.
    pub relations: TypedKeyspace<B::Keyspace, crate::relations::RelationKey>,
    /// `(RoomSn,) -> b""`: the published room directory (`crate::registry::RoomRegistry`'s
    /// `set_directory_visibility`/`list_published_rooms`).
    pub public_rooms: TypedKeyspace<B::Keyspace, PublicRoomKey>,
    /// `(user_id, RoomSn) -> b""`: every room a user currently holds `join` membership in. See
    /// [`UserJoinedRoomKey`].
    pub joined_rooms: TypedKeyspace<B::Keyspace, UserJoinedRoomKey>,
    /// `(RoomSn,) -> RoomBlock`: rooms currently blocked by a server administrator. See
    /// [`BlockedRoomKey`].
    pub blocked_rooms: TypedKeyspace<B::Keyspace, BlockedRoomKey>,
}

impl<B: KvBackend> Tables<B> {
    /// Opens every keyspace and interning table this crate uses.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] if any keyspace fails to open.
    pub fn open(backend: &B) -> Result<Self, hs_kv::KvError> {
        Ok(Self {
            room_sn: hs_tables::interning::room_sn_table(backend)?,
            event_sn: hs_tables::interning::event_sn_table(backend)?,
            events: TypedKeyspace::new(backend.keyspace("room_events")?),
            timeline: TypedKeyspace::new(backend.keyspace("room_timeline")?),
            extremities_fwd: TypedKeyspace::new(backend.keyspace("room_extremities_fwd")?),
            extremities_bwd: TypedKeyspace::new(backend.keyspace("room_extremities_bwd")?),
            aliases: TypedKeyspace::new(backend.keyspace("room_aliases")?),
            room_aliases: TypedKeyspace::new(backend.keyspace("room_aliases_by_room")?),
            room_meta: TypedKeyspace::new(backend.keyspace("room_meta")?),
            relations: TypedKeyspace::new(backend.keyspace("room_relations")?),
            public_rooms: TypedKeyspace::new(backend.keyspace("room_public_directory")?),
            joined_rooms: TypedKeyspace::new(backend.keyspace("room_joined_by_user")?),
            blocked_rooms: TypedKeyspace::new(backend.keyspace("room_blocked")?),
        })
    }
}
