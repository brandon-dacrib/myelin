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
//! The same hook fills the *other* place history goes missing: the middle of the timeline,
//! between a leave and a rejoin through another server ([`Backfill::fill_gap`], [`GapAnchor`],
//! `crate::actor::gaps`). A backward page that reaches such a gap asks for it before walking on
//! into what was held before the leave.
//!
//! [`RoomActor::history_before_oldest`]: crate::actor::RoomActor::history_before_oldest
//! [`RoomActor::backfill_anchor`]: crate::actor::RoomActor::backfill_anchor
//! [`RoomActor::accept_backfilled_events`]: crate::actor::RoomActor::accept_backfilled_events

use async_trait::async_trait;
use hs_model::Event;
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

    /// Asks a server in `room_id` for one batch of the history missing from the gap below
    /// timeline position `top` (what happened between a leave and the rejoin at `top`;
    /// `crate::actor::gaps`), verifies it, and stores it through
    /// `RoomActor::accept_gap_events`. Returns how many events were newly placed: `0` when
    /// there is no open gap there, nobody else is in the room to ask, or the answer held
    /// nothing new (which closes the gap). The default does nothing and returns `0`, for an
    /// implementation that only fetches history before the oldest held event.
    ///
    /// # Errors
    /// [`RoomError::BackfillFailed`] if no server could be reached or answered sensibly;
    /// whatever storing the batch can fail with.
    async fn fill_gap(&self, room_id: &RoomId, top: i64) -> Result<usize, RoomError> {
        let _ = (room_id, top);
        Ok(0)
    }
}

/// What an implementation of [`Backfill::fill_gap`] needs to fill a gap in the middle of a
/// room's timeline -- the history between a leave and a rejoin through another server
/// (`crate::actor::gaps`). Produced by `crate::actor::RoomActor::gap_anchor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapAnchor {
    /// The position of the event the gap sits below (the rejoin): what identifies the gap to
    /// `crate::actor::RoomActor::accept_gap_events`.
    pub top: i64,
    /// Events the gap lacks, which its events cite as `prev_events`: what a `/backfill` request
    /// names as `v`. The answer is these events and what came before them.
    pub from: Vec<OwnedEventId>,
    /// Servers in the room other than this one, most promising first, as for
    /// [`BackfillAnchor::servers`].
    pub servers: Vec<String>,
}

/// What `crate::actor::RoomActor::accept_gap_events` did with one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapFill {
    /// How many events were newly placed in the gap, placed outliers included.
    pub added: usize,
    /// Whether the gap is closed now: nothing more will be fetched for it.
    pub closed: bool,
}

/// Which history a batch is: where it goes in the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    /// Before the oldest event held: a room joined elsewhere, read back past the join.
    BeforeOldest,
    /// Inside the gap below timeline position `top`: what happened between a leave and the
    /// rejoin at `top` (`crate::actor::gaps`).
    Gap {
        /// The position of the event the gap sits below.
        top: i64,
    },
}

impl HistoryKind {
    /// The `kind` label of `hs_room_backfilled_events_total` and its siblings: `before_oldest`
    /// or `rejoin_gap`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::BeforeOldest => "before_oldest",
            Self::Gap { .. } => "rejoin_gap",
        }
    }
}

/// What a fetch needs to know about a batch before it is placed: which of its events is the
/// oldest that will be placed (the state is asked for at that one) and which events it cites
/// as `auth_events` that are neither held nor in the batch. Produced by
/// `crate::actor::RoomActor::plan_history`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPlan {
    /// The oldest event of the batch that would be placed: what `/state_ids?event_id=` names.
    pub oldest: OwnedEventId,
    /// `auth_events` the batch's events cite that this server does not hold and the batch does
    /// not contain. Fetched alongside the state, so every event can be authorized.
    pub missing_auth: Vec<OwnedEventId>,
}

/// The state at the oldest event of a batch, as the server that sent the batch answered it:
/// what lets the state at every event of the batch be *derived forward* rather than walked
/// back from what is held (`crate::actor::RoomActor::accept_history`).
#[derive(Debug, Clone)]
pub struct FetchedState {
    /// The event the state was asked at: [`HistoryPlan::oldest`]. A batch whose oldest event
    /// turns out to be another one is placed with the walk instead.
    pub at: OwnedEventId,
    /// The room's state *before* `at`, as event IDs (`/state_ids`' `pdu_ids`, or the IDs of
    /// `/state`'s `pdus`).
    pub state_ids: Vec<OwnedEventId>,
    /// Events this server did not hold that the state, its auth chain or the batch's
    /// `auth_events` name, fetched (`/event/{eventId}`, or `/state`) and verified by the caller
    /// (hashes and signatures). Each is authorized against its own `auth_events` and stored as
    /// an outlier; one that fails is left out, and so is its place in the state.
    pub events: Vec<Event>,
}

/// Where the state at a placed batch's events came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateSource {
    /// Asked for: the state at the batch's oldest event from the server that sent it
    /// ([`FetchedState`]), and every later event's state derived forward from it.
    Fetched,
    /// Walked back from the state held at the event the batch sits below, because nothing was
    /// fetched (the server could not answer, or this is a caller with no federation).
    Walked,
}

impl StateSource {
    /// The `outcome` label of `hs_room_backfill_batches_total`: `state_fetched` or
    /// `state_walked`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Fetched => "state_fetched",
            Self::Walked => "state_walked",
        }
    }
}

/// What `crate::actor::RoomActor::accept_history` did with one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryOutcome {
    /// How many events were newly placed in the timeline, placed outliers included.
    pub added: usize,
    /// How many failed authorization at their position and were not placed.
    pub rejected: usize,
    /// How many were placed without an authorization check because their `auth_events` were
    /// out of reach (only when the state was walked, not fetched).
    pub unchecked: usize,
    /// How many events the state fetch brought were stored as outliers.
    pub state_events_stored: usize,
    /// Where the state at the placed events came from.
    pub state: StateSource,
    /// For a gap: whether it is closed now. Always `false` for history before the oldest event.
    pub gap_closed: bool,
}
