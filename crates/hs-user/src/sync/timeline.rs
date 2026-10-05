//! A room's `timeline` section: which events a `/sync` batch carries, whether it is `limited`,
//! and the `prev_batch` a client pages back from.
//!
//! Both kinds of batch are one walk backwards from where the batch ends ([`collect_backward`]):
//!
//! - an **incremental** batch ([`build_incremental_timeline`]) is the newest `limit` events
//!   after the position the client's token left the room at, `limited` when there were more
//!   (a gap, which the spec answers from its newest end, with `prev_batch` reaching back into
//!   it);
//! - a **fresh** batch ([`build_fresh_timeline`]) -- an initial sync, `full_state`, a room new
//!   to the client -- is the newest `limit` events of the room.
//!
//! What "an event" counts is the same for both: one the requester may see, by the room's
//! history visibility or because it is part of the room's current state while they are joined
//! (as Synapse's `always_include_ids` does: Sytest's "Current state appears in timeline in
//! private history" is somebody who was out of a `joined`-visibility room while another member
//! joined, and must still see that join), and one the timeline filter passes. Events that do not
//! count are walked past, up to [`TIMELINE_SCAN`] of them, so a filter or a visibility rule that
//! hides most of a room's history neither empties the batch nor turns one `/sync` into a scan of
//! the whole room.
//!
//! An incremental batch also stops at a **hole in the history**: an event whose `prev_events`
//! this server does not hold, because a remote server's events arrived with history missing
//! behind them that could not all be fetched. The client cannot be handed what is not here; it
//! gets the events from the hole onwards, `limited`, so that it knows something is missing
//! (Synapse's `get_timeline_gaps`; Complement's `TestSyncTimelineGap`).

use hs_kv::KvBackend;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_room::actor::RoomActor;
use hs_room::routes::render::{attach_replaced_state, client_event_json};
use hs_room::timeline::{Direction, PaginationToken};
use ruma::UserId;
use serde_json::Value;

use crate::filter::RoomEventFilter;

/// How many events a batch walks past without counting them (hidden by history visibility, or
/// by the timeline filter) before it gives up looking for more and calls the batch `limited`.
/// Bounded so that one `/sync` stays one bounded read, whatever the room's history holds.
pub(crate) const TIMELINE_SCAN: usize = 500;

/// One room's rendered timeline, plus whether it was capped and, if so, a token to page further
/// back with (`hs_room`'s own `/messages` pagination tokens).
pub(crate) struct Timeline {
    /// The events, oldest first, rendered for the requester.
    pub(crate) events: Vec<Value>,
    /// Whether there is more the requester may see before the first event.
    pub(crate) limited: bool,
    /// Where `GET /messages` continues backwards from: just before the first event.
    pub(crate) prev_batch: Option<String>,
}

impl Timeline {
    fn empty() -> Self {
        Self {
            events: Vec::new(),
            limited: false,
            prev_batch: None,
        }
    }
}

/// What a batch is built for, beyond its bounds.
#[derive(Clone, Copy)]
pub(crate) struct TimelineScope<'a> {
    /// The most events the batch carries.
    pub(crate) limit: usize,
    /// `room.timeline`'s content filter, if it has one.
    pub(crate) filter: Option<&'a RoomEventFilter>,
    /// Whom the batch is for.
    pub(crate) requester: &'a UserId,
    /// Whether events that are the room's current state count although history visibility
    /// hides them: for a room the requester is joined to.
    pub(crate) current_state_counts: bool,
}

/// Renders one event for a client, carrying `unsigned.prev_content`/`replaces_state`/`prev_sender`
/// when it is a state event that replaced another one.
///
/// Without this a client cannot tell a display-name change from a join and renders both as "Alice
/// joined the room" -- and `/sync` is the endpoint whose output a client's timeline is actually
/// built from, so fixing only `/messages` and `/state` fixes only what scrollback shows.
/// `replaced_state_for` decides the history-visibility question about the *replaced* event, which
/// is why the requesting user has to reach this far down.
pub(crate) fn rendered_with_replaced_state(
    actor: &RoomActor<impl KvBackend>,
    event: &Event,
    requester: &UserId,
) -> Value {
    attach_replaced_state(
        client_event_json(event),
        actor.replaced_state_for(event, requester).as_ref(),
    )
}

/// Whether `event` is visible to `requester` under the room's history visibility: the rule
/// `/messages` and `/event` apply. An event whose visibility cannot be worked out is not.
fn visible_by_history(
    actor: &RoomActor<impl KvBackend>,
    event: &Event,
    requester: &UserId,
) -> bool {
    actor.event_visible_to(event, requester).unwrap_or(false)
}

/// Whether `event` is the room's current state for its `(type, state_key)`.
fn is_current_state(actor: &RoomActor<impl KvBackend>, event: &Event) -> bool {
    let Some(state_key) = event.header().state_key.as_deref() else {
        return false;
    };
    actor
        .state_event(&event.header().event_type, state_key)
        .ok()
        .flatten()
        .is_some_and(|current| current.event_id() == event.event_id())
}

/// Whether `event` counts in `scope`'s batch: visible (by history, or as current state), and
/// passed by the filter.
fn counts(actor: &RoomActor<impl KvBackend>, event: &Event, scope: &TimelineScope<'_>) -> bool {
    let header = event.header();
    if !scope
        .filter
        .is_none_or(|f| f.matches(&header.event_type, header.sender.as_str()))
    {
        return false;
    }
    visible_by_history(actor, event, scope.requester)
        || (scope.current_state_counts && is_current_state(actor, event))
}

/// Whether `event` names a `prev_event` this server does not hold in the room's timeline: the
/// history behind it is missing here. Room versions 1 and 2 give each prev event as an
/// `[event_id, hashes]` pair; later versions as the id alone.
fn opens_a_hole(actor: &RoomActor<impl KvBackend>, event: &Event) -> bool {
    let Some(CanonicalJsonValue::Array(prev_events)) = event.json().get("prev_events") else {
        return false;
    };
    prev_events.iter().any(|prev| {
        let id = match prev {
            CanonicalJsonValue::String(id) => Some(id.as_str()),
            CanonicalJsonValue::Array(pair) => pair.first().and_then(CanonicalJsonValue::as_str),
            _ => None,
        };
        id.and_then(|id| ruma::EventId::parse(id).ok())
            .is_some_and(|id| actor.timeline_position(&id).is_none())
    })
}

/// What [`collect_backward`] found.
struct Collected<'a> {
    /// Newest first, each with its timeline position.
    events: Vec<(i64, &'a Event)>,
    /// Whether the requester may see more before the oldest of `events` that the batch leaves
    /// out: another counting event, the scan running out, or a hole in the history.
    more: bool,
}

/// Walks the timeline backwards from `upto` (inclusive; the live end when `None`), collecting up
/// to `scope.limit` events that count, and stopping at `floor` (exclusive) when given, at a hole
/// in the history when `stop_at_holes`, or after walking past [`TIMELINE_SCAN`] events that do
/// not count.
fn collect_backward<'a>(
    actor: &'a RoomActor<impl KvBackend>,
    scope: &TimelineScope<'_>,
    upto: Option<i64>,
    floor: Option<i64>,
    stop_at_holes: bool,
) -> Collected<'a> {
    let mut collected = Collected {
        events: Vec::new(),
        more: false,
    };
    let mut from = upto.map(|pos| PaginationToken::new(pos.saturating_add(1), Direction::Backward));
    // The first page is what an ordinary batch needs (`limit` and one more, to tell "exactly
    // `limit`" from a gap); later pages are larger, for a batch walking past hidden events.
    let mut page_size = scope.limit.saturating_add(1);
    let mut walked_past = 0usize;
    loop {
        let (page, next) = actor.paginate(from, Direction::Backward, page_size);
        let exhausted = page.len() < page_size || next.is_none();
        for event in page {
            let Some(pos) = actor.timeline_position(event.event_id()) else {
                continue;
            };
            if floor.is_some_and(|floor| pos <= floor) {
                return collected;
            }
            if counts(actor, event, scope) {
                if collected.events.len() >= scope.limit {
                    collected.more = true;
                    return collected;
                }
                collected.events.push((pos, event));
            } else {
                walked_past += 1;
            }
            if stop_at_holes && opens_a_hole(actor, event) {
                collected.more = true;
                return collected;
            }
        }
        if exhausted {
            return collected;
        }
        if walked_past >= TIMELINE_SCAN {
            collected.more = true;
            return collected;
        }
        from = next;
        page_size = page_size.max(100);
    }
}

/// Renders what [`collect_backward`] found as a [`Timeline`], oldest first.
fn render(
    actor: &RoomActor<impl KvBackend>,
    collected: Collected<'_>,
    requester: &UserId,
) -> Timeline {
    let prev_batch = collected
        .events
        .last()
        .map(|(pos, _)| PaginationToken::new(*pos, Direction::Backward).to_string());
    Timeline {
        events: collected
            .events
            .into_iter()
            .rev()
            .map(|(_, event)| rendered_with_replaced_state(actor, event, requester))
            .collect(),
        limited: collected.more,
        prev_batch,
    }
}

/// Whether `filter` lets no event through at all (`types: []`): such a timeline is empty without
/// walking anything, and never `limited` (Synapse's `blocks_all_room_timeline`).
fn blocks_everything(filter: Option<&RoomEventFilter>) -> bool {
    filter.is_some_and(|f| f.types.as_ref().is_some_and(Vec::is_empty))
}

/// A room's timeline for an incremental sync: what happened after `resume_pos`, up to `upto`.
///
/// Usually that is a handful of events and they are all returned, oldest first. When it is more
/// than `limit` there is a *gap*, and the spec is specific about which side of it the client
/// gets: the most recent `limit` events, with `limited: true` and a `prev_batch` from which
/// paginating backwards recovers the rest. It used to be answered with the *oldest* `limit`
/// events, and whatever did not fit in one page was lost to that client. A hole in the history
/// (the module docs) is a gap too.
///
/// `upto` is where this batch ends: the room's position as of the token being handed out with
/// it ([`crate::store::UserStore::room_pos_at_token`]), or the requester's own departure,
/// whichever is first. Nothing after it is sent, however much the room has moved on since the
/// token was fixed -- an event that lands while the response is being assembled has a feed entry
/// past the token and belongs to the next batch.
pub(crate) fn build_incremental_timeline(
    actor: &RoomActor<impl KvBackend>,
    resume_pos: i64,
    scope: &TimelineScope<'_>,
    upto: Option<i64>,
) -> Timeline {
    if upto.is_some_and(|upto| upto <= resume_pos) || blocks_everything(scope.filter) {
        return Timeline::empty();
    }
    let collected = collect_backward(actor, scope, upto, Some(resume_pos), true);
    render(actor, collected, scope.requester)
}

/// The most recent `limit` events `requester` may see, oldest first.
///
/// `upto` is the room position of the requester's own departure, for a room they have left or
/// been removed from: the page then ends there rather than at the room's live end, which for
/// them is a stretch of events they may not read and so would come back empty however much
/// they are entitled to from before.
pub(crate) fn build_fresh_timeline(
    actor: &RoomActor<impl KvBackend>,
    scope: &TimelineScope<'_>,
    upto: Option<i64>,
) -> Timeline {
    if blocks_everything(scope.filter) {
        return Timeline::empty();
    }
    let collected = collect_backward(actor, scope, upto, None, false);
    render(actor, collected, scope.requester)
}

/// One event in the federation format a filter's `event_format: "federation"` asks for: the
/// event as servers exchange it (`prev_events`, `auth_events`, `depth`, `hashes`,
/// `signatures`), with the client rendering's `event_id`, `room_id` and `unsigned` on it.
/// Synapse's `format_event_raw`; Sytest's "Can request federation format via the filter".
pub(crate) fn federation_format(actor: &RoomActor<impl KvBackend>, client: Value) -> Value {
    let Some(event) = client
        .get("event_id")
        .and_then(Value::as_str)
        .and_then(|id| ruma::EventId::parse(id).ok())
        .and_then(|id| actor.event_by_id(&id))
    else {
        return client;
    };
    let mut pdu = hs_room::routes::render::canonical_to_json(event.json());
    let Some(object) = pdu.as_object_mut() else {
        return client;
    };
    if let Value::Object(client) = client {
        for (key, value) in client {
            // The client rendering decides what the content is (the redacted form for a
            // redacted event), and carries what only it has.
            if matches!(key.as_str(), "content" | "event_id" | "unsigned")
                || !object.contains_key(&key)
            {
                object.insert(key, value);
            }
        }
    }
    object
        .entry("room_id")
        .or_insert_with(|| Value::String(actor.room_id().to_string()));
    pdu
}
