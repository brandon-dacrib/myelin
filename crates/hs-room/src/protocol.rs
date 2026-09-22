//! The room actor protocol: the design document `docs/design/04-room-actor-protocol.md`
//! restates in prose, kept here as rustdoc so it stays next to the types it describes.
//!
//! # The unit of consistency
//!
//! `PLAN.md` sections 5.3 and 6.2: one [`crate::actor::RoomActor`] owns exactly one room. It is
//! the *only* thing that ever writes that room's state, timeline, extremities and membership --
//! every mutation (create the room, send an event, change membership, redact, and, once track 06
//! lands it, persist an inbound federation event) goes through it, in the order it decides to
//! apply them. This is what "serializes all writes for one room" means concretely: not a
//! database-level lock, but a single owner that never runs two mutations concurrently against its
//! own in-memory hot state.
//!
//! # The command set
//!
//! [`RoomActorHandle`] is the mailbox. It exposes:
//!
//! - **Mutations**, which this crate additionally models as an explicit [`Command`]/[`Reply`]
//!   protocol (see below) precisely because they are the operations that must serialize:
//!   [`RoomActorHandle::create_room`], [`RoomActorHandle::send_event`],
//!   [`RoomActorHandle::membership`], [`RoomActorHandle::redact`]. [`Command::PersistInbound`] is
//!   the seam track 06 (federation) calls into once inbound `/send` transactions land; it is
//!   wired into the protocol today but returns [`RoomError::Internal`] ("not implemented") --
//!   see `docs/rfcs/0010-room-actor-state-store-seam.md`.
//! - **Queries** (state, timeline, members, relations, aliases), exposed as plain async methods
//!   on [`RoomActorHandle`] rather than `Command` variants. This is a deliberate scope
//!   simplification: reads do not need write-serialization (many can run concurrently against a
//!   stable snapshot of the hot state), so routing them through the same single-writer mailbox
//!   would only add latency without adding correctness. See `crate::actor::RoomActor`'s query
//!   methods for the full read surface.
//!
//! Every mutation runs to completion (built, hashed, signed, authorized, persisted in one
//! `hs-kv` transaction -- `crate::pipeline` and `crate::actor::RoomActor::persist`) before the
//! handle call returns, and before [`RoomUpdate`] is published. There is no "accepted but not yet
//! durable" state a concurrent reader can observe.
//!
//! # Today's implementation: mutex-serialized, not a spawned mailbox task
//!
//! The brief's "command set" and "protocol" language describes the actor as if it were a spawned
//! task reading a channel. This implementation gets the same serialization guarantee (one writer
//! at a time, FIFO per caller ordering not guaranteed across callers but atomicity per call is)
//! from a `tokio::sync::Mutex<RoomActor<B>>` inside [`RoomActorHandle`] instead: every mutation
//! locks it, runs synchronously to completion (via `spawn_blocking`, since `hs-kv`'s transaction
//! API is itself synchronous -- see `docs/status/03-cluster.md`'s note on this same pattern), and
//! unlocks. This is a legitimate implementation of the same protocol (callers see the same
//! command/reply shape either way) and was chosen because it needs no supervisor, no shutdown
//! protocol and no backpressure policy to be correct, all of which a real mailbox task would need
//! designed before it could be trusted -- and none of which matters yet, because nothing in this
//! pass calls a room actor from more than one place at a time (`hs-cluster`'s ownership routing,
//! which would create that situation, is not wired in here; see
//! `docs/status/04-room-and-events.md`). Swapping the mutex for a spawned mailbox task later is an
//! internal change: [`RoomActorHandle`]'s public API does not need to move.
//!
//! # The publish stream: [`RoomUpdate`]
//!
//! Every successful mutation publishes exactly one [`RoomUpdate`] on the room's
//! `tokio::sync::broadcast` channel ([`RoomActorHandle::subscribe`]). This is the interface
//! `docs/workstreams/README.md`'s week-8 seam names: tracks 05 (sync), 06 (federation sender), 10
//! (push) and 11 (appservices) all wake up from this stream instead of polling the store.
//!
//! Field by field, and why each is there:
//!
//! - `room_sn` / `room_id`: which room. Carried as both because most consumers already work in
//!   interned `RoomSn` terms (track 03's ownership routing, track 05's per-user feed) but the
//!   string form saves a reverse lookup for consumers that only need it for logging or a client
//!   response.
//! - `room_pos`: the event's room-local timeline position (`PLAN.md` section 6.6). Track 05's
//!   per-user feed is exactly `(room_sn, room_pos)` pairs; this is the value it appends.
//! - `event_id`, `event_type`, `state_key`, `sender`: enough to filter without a store read for
//!   the common case (an appservice checking its namespace, a push rule checking `event_type`).
//! - `changed_state_keys`: **empty unless this event is a state event**, in which case it is
//!   exactly the one `(event_type, state_key)` this event set. A `Vec` rather than a single
//!   `Option<(String, String)>` because a future inbound-federation path
//!   ([`Command::PersistInbound`]) can apply a resolved state change that touches more than one
//!   key at once (state resolution choosing a different winner for an existing key, not just the
//!   new event's own) -- track 06's eventual consumer should not need a second message shape for
//!   that case.
//! - `membership_deltas`: **empty unless this event is `m.room.member`**, in which case it names
//!   the target user and their new membership value. Split out from `changed_state_keys` even
//!   though it is redundant with it (a membership change is also a state change) because track 05
//!   and track 10 both need "did this user's membership change, and to what" as a first-class
//!   question -- reconstructing that from `changed_state_keys` alone would mean re-parsing the
//!   event content.
//! - `push_evaluation_inputs`: the room's current member list *at the sender's power level and
//!   the room's current push-relevant state* (a minimal cut, not the full state) -- `PLAN.md`'s
//!   "push actions are computed on the room owner" decision means this room actor is where the
//!   input to push rule evaluation is naturally available; track 10 consumes this field instead
//!   of re-deriving it. Not yet populated (`Vec::new()` always) -- computing it needs the push
//!   rules engine (track 10) to specify its exact shape first; the field exists now so track 10 is
//!   not blocked designing its consumer against a placeholder shape.
//!
//! [`RoomUpdate`] is intentionally *not* the event's full content: consumers that need the body
//! (sync, mostly) already hold -- or can cheaply obtain through a query method -- the room's hot
//! state; duplicating full event JSON onto every publish would make the broadcast channel's
//! backlog cost scale with message size instead of staying a small fixed struct, which matters
//! because `tokio::sync::broadcast` buffers a fixed number of not-yet-received messages per
//! subscriber.
//!
//! # The hot-state cache and its eviction policy
//!
//! See `crate::registry::RoomRegistry` for the concrete implementation; summarized here because
//! it is part of the protocol a caller depends on. A [`RoomActorHandle`] is cheap to hold (an
//! `Arc`); a `RoomActor` is not cheap to *construct* (it replays a room's whole persisted
//! timeline, `crate::actor::RoomActor::load`). [`crate::registry::RoomRegistry`] is the layer that
//! decides when to construct one (first access) and when to drop it (idle longer than a
//! configured threshold, checked by a periodic sweep) -- both invisible to a caller holding a
//! handle across an eviction, because [`crate::registry::RoomRegistry::get_or_load`] is the only
//! way to obtain one and it transparently reloads.

use hs_model::ids::{EventSn, RoomSn};

/// One membership change, as carried on [`RoomUpdate::membership_deltas`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipDelta {
    /// The affected user.
    pub user_id: ruma::OwnedUserId,
    /// Their new `membership` value (`"join"`, `"invite"`, `"leave"`, `"ban"`, `"knock"`).
    pub membership: String,
}

/// One `(event_type, state_key)` this update changed. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedStateKey {
    /// The changed event type.
    pub event_type: String,
    /// The changed state key.
    pub state_key: String,
}

/// The publish stream: one message per successful mutation. See the module docs for the field
/// rationale.
#[derive(Debug, Clone)]
pub struct RoomUpdate {
    /// The room's interned short ID.
    pub room_sn: RoomSn,
    /// The room's full ID.
    pub room_id: ruma::OwnedRoomId,
    /// The event's room-local timeline position.
    pub room_pos: i64,
    /// The event's global short ID.
    pub event_sn: EventSn,
    /// The event's ID.
    pub event_id: ruma::OwnedEventId,
    /// The event's `type`.
    pub event_type: String,
    /// The event's `state_key`, if it is a state event.
    pub state_key: Option<String>,
    /// The event's `sender`.
    pub sender: ruma::OwnedUserId,
    /// State keys this event changed. See the module docs.
    pub changed_state_keys: Vec<ChangedStateKey>,
    /// Membership changes this event caused. See the module docs.
    pub membership_deltas: Vec<MembershipDelta>,
    /// Inputs for push rule evaluation. Not yet populated -- see the module docs.
    pub push_evaluation_inputs: Vec<()>,
    /// This update's position on the registry's *global* stream
    /// (`crate::registry::RoomRegistry::subscribe_global`): `1` for the first update ever
    /// published there in this process, then one more each time, in the order the stream
    /// delivers them. `0` on a room's own stream (`RoomActorHandle::subscribe`), which does not
    /// number. What lets a consumer of the global stream say how far it has read, and a reader
    /// of what that consumer writes -- `/sync` -- wait until it has read everything that was
    /// published before the reader asked. See `RoomRegistry::global_published_seq`.
    pub global_seq: u64,
}
