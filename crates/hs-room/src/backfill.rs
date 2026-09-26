//! [`Backfill`]: the seam through which a room's history from before the oldest event this
//! server holds is fetched from another server, on a client's behalf.
//!
//! A room this server's own user joined elsewhere
//! (`crate::registry::RoomRegistry::bootstrap_from_remote_join`, RFC 0015) starts with one
//! timeline event, the join, and the resident's state as outliers. Everything said in the room
//! before that is on the resident and not here, and `GET /messages?dir=b` used to stop at the
//! join as if the room had begun there. This is the hook that fetches it: when a backward page
//! reaches the oldest event this server holds and the room's history continues before it
//! (`RoomActor::history_before_oldest`), `crate::routes::query::get_messages` asks whatever
//! implements this trait for one batch, then pages again.
//!
//! `hs-room` knows nothing about federation, so -- like `crate::remote_join::RemoteJoin` and the
//! registry's fencing and token-resolver hooks -- the implementation lives in `hs-cli`, which owns
//! both this crate and `hs-federation`: it runs `GET /_matrix/federation/v1/backfill` against a
//! server in the room ([`RoomActor::backfill_anchor`] says which event to walk back from and who
//! to ask), verifies every PDU the way any inbound PDU is verified, and hands the batch to
//! [`RoomActor::accept_backfilled_events`], which is where the events get their place in the
//! timeline (negative positions, older than anything held), their state, and their durability.
//! When nothing implements it -- a server with federation off, this crate's own tests -- a room's
//! history is exactly what this server holds, as before.
//!
//! [`RoomActor::history_before_oldest`]: crate::actor::RoomActor::history_before_oldest
//! [`RoomActor::backfill_anchor`]: crate::actor::RoomActor::backfill_anchor
//! [`RoomActor::accept_backfilled_events`]: crate::actor::RoomActor::accept_backfilled_events

use async_trait::async_trait;
use ruma::{OwnedEventId, RoomId};

use crate::error::RoomError;

/// Where a room's held history ends and who can supply what came before it: what an
/// implementation of [`Backfill`] sends as `v` on the `/backfill` request, and the servers it
/// tries, in order. Produced by `crate::actor::RoomActor::backfill_anchor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillAnchor {
    /// The oldest event this server holds in the room's timeline. A `/backfill` from it answers
    /// with that event and what came before it, and the event itself is already held, so it is
    /// skipped.
    pub event_id: OwnedEventId,
    /// Servers in the room other than this one, most promising first: the server the room ID
    /// names (its creator's, for room versions that put one there), then the servers of every
    /// currently joined member. Empty when nobody else is in the room, in which case there is
    /// nobody to ask.
    pub servers: Vec<String>,
}

/// Fetches and stores one batch of a room's history from before the oldest event this server
/// holds. See the module docs.
#[async_trait]
pub trait Backfill: Send + Sync {
    /// Asks a server in `room_id` for the events before this server's oldest one, verifies them,
    /// and stores them through `RoomActor::accept_backfilled_events`. Returns how many events
    /// were newly stored: `0` when the room's history does not continue before what is held,
    /// when nobody else is in the room to ask, or when every server asked answered with nothing
    /// new.
    ///
    /// # Errors
    /// [`RoomError::BackfillFailed`] if no server could be reached or answered sensibly;
    /// whatever storing the batch can fail with.
    async fn backfill(&self, room_id: &RoomId) -> Result<usize, RoomError>;
}
