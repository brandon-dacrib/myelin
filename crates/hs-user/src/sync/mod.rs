//! `/sync` v2: full and incremental, long-polling, joined/invited/knocked/left rooms, timeline
//! with `limited`/`prev_batch`, state, account data, `m.typing` ephemeral events, top-level
//! `m.presence` events and unread notification counts (zero -- track 10 has not landed).
//!
//! [`build`] is the entry point; `crate::routes::sync` is the thin HTTP wrapper around it (query
//! parsing, response headers, device-cursor bookkeeping).
//!
//! # What "changed since a room's baseline" means here
//!
//! For every candidate room, this module needs a room-local position to resume the timeline
//! from. [`crate::store::UserStore::room_pos_as_of`] answers that from the user's durable feed;
//! when it returns `None` (the room has no feed history at or before the presented token -- true
//! both for a room the user has never synced before, and for a room that has been "hot"
//! -- `crate::hub`'s module docs -- since before the user's membership even started), this module
//! falls back to treating the room exactly like a fresh room in an initial sync: the most recent
//! `limit` events (newest-first, then reversed to chronological order) plus full current state,
//! rather than an empty or wrong-baseline forward page. See [`resume_mode`].
//!
//! # `m.typing` and `m.presence` (`crate::typing`, `crate::presence`)
//!
//! Both follow the same shape: an in-memory registry owned by [`crate::hub::SessionHub`] (never
//! persisted -- both are genuinely ephemeral, restart loses them, matching Synapse's own
//! behavior), a monotonic counter stamped onto whatever changed, and a cursor in [`SyncToken`]
//! (`typing_seq`, `presence_seq` -- the latter reserved in the token format before either module
//! existed) that this module compares against to decide "does this response need to mention it".
//! See `crate::typing`'s module docs for why a global counter, not a per-room/per-user boolean,
//! and for why expiry is pruned lazily on read rather than by a background timer.
//!
//! # `m.receipt` (`crate::receipts`)
//!
//! Follows the identical shape (in-memory registry, global counter, `SyncToken::receipts_seq` --
//! reserved since session 1, before `crate::receipts` existed): gathered up front alongside
//! typing (a receipt-only change never touches the feed either), folded into `ephemeral.events`
//! next to any `m.typing` event already there. Unlike typing, the *content* built for a given
//! room depends on who is asking: `crate::receipts::ReceiptRegistry::content_for` omits every
//! other user's `m.read.private` receipt, so this module calls it once per response with
//! `user_id` (the syncing user) as the viewer, never a shared, unscoped value. `m.fully_read` is
//! not an ephemeral event at all -- it is private room account data, already carried by this
//! module's existing account-data section with no extra code (see
//! `crate::routes::receipts::post_read_markers`).
//!
//! `m.typing` is scoped per room (only joined members ever see or send it); `m.presence` is
//! scoped per user, reported to (and gated on) everyone who currently shares a *joined* room with
//! the user whose presence changed -- the same privacy scope `device_lists.changed`/`left` uses
//! below, and for the identical reason (`crate::hub::SessionHub::users_sharing_room_with`, which
//! `shared_users` below now delegates to).
//!
//! Presence has no automatic idle/logout-driven transition to `unavailable`/`offline`
//! (Synapse's own timer-based heuristics) -- a user's presence is exactly what they last set it to
//! via `PUT /presence/{userId}/status`, forever, until they set it again. Documented here as a
//! deliberate scope cut (see `docs/status/05-sync.md`), not a silent gap.
//!
//! # Not implemented in this pass (present as documented gaps, not silent omissions)
//!
//! - Presence's idle/logout-driven automatic offline transition (see `crate::presence`'s module
//!   docs).
//! - See `crate::filter`'s own doc comment for the full, precise list of which filter fields this
//!   module applies and which it only parses (`event_fields`, `event_format`, ephemeral/room
//!   account-data content filtering, and `room.state.include_redundant_members`, among others).
//!
//! # `to_device`, `device_lists` and key counts (`docs/rfcs/0013-e2ee-sync-extensions.md`)
//!
//! All four fields `hs-e2e` (track 08) makes possible are populated here, straight from that
//! crate's already-tested store traits (`hs_e2e::store`) -- see [`build`]'s body. Two things are
//! worth recording since they are exactly where a `/sync` implementation goes wrong:
//!
//! - **To-device acknowledgement.** [`SyncToken::to_device_seq`] is the cursor a *device* was
//!   last handed as part of its own `next_batch`. When a request presents token `T` as `since`,
//!   this crate first calls [`hs_e2e::store::ToDeviceStore::delete_up_to`] with `T.to_device_seq`
//!   -- deleting only messages already covered by a response the client has *proven* it received
//!   (by echoing `T` back), never the messages this response is about to hand back (those have
//!   stream ids strictly greater than `T.to_device_seq` and survive `delete_up_to(T.to_device_seq)`
//!   untouched). Concretely: a client that calls `/sync` twice with the *same* `since` (a retried
//!   request, or a client that never advances) gets the same to-device messages both times; only
//!   presenting the *next* token (whose `to_device_seq` covers them) causes them to be deleted --
//!   which is why deletion happens at the top of the *next* call, keyed off the token that call
//!   presents, not eagerly right after this response is built. Deleting eagerly (Synapse's
//!   documented behavior, and this RFC's original suggestion) trades a small window of guaranteed
//!   delivery for simplicity; this crate already has the token machinery to do better cheaply, so
//!   it does.
//! - **`device_lists.changed`/`left` are scoped to [`shared_users`]**, not every user on the
//!   server: `hs_e2e::store::DeviceKeyStore::changed_users_since` has no room-membership notion at
//!   all (by design -- see that trait's module doc), so it is intersected here against the users
//!   this crate's own membership data says `user_id` currently shares a joined room with. This is
//!   both the spec's privacy requirement (a user must not learn about devices belonging to
//!   strangers) and what makes the field usable at all on a server with more than a handful of
//!   users.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hs_e2e::store::{DeviceKeyStore, E2eStore, FallbackKeyStore, OneTimeKeyStore, ToDeviceStore};
use hs_kv::KvBackend;
use hs_model::Event;
use hs_push::rulesets::RulesetStore;
use hs_room::routes::render::{attach_replaced_state, client_event_json};
use hs_room::timeline::{Direction, PaginationToken};
use ruma::{OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::{Value, json};

use crate::error::UserError;
use crate::filter::SyncFilter;
use crate::hub::SessionHub;
use crate::room_source::RoomSource;
use crate::store::MembershipRecord;
use crate::token::SyncToken;

/// The spec's own default (`GET /sync`'s `timeline.limit` defaults to 10 when no filter says
/// otherwise).
pub const DEFAULT_TIMELINE_LIMIT: usize = 10;

/// The most to-device messages a single `/sync` response will carry for one device. Matches this
/// crate's `DEFAULT_TIMELINE_LIMIT`-adjacent philosophy of a bounded response; a device with more
/// than this many queued messages simply gets the rest on its next sync (nothing is lost --
/// `ToDeviceStore::poll_since`'s returned cursor only advances past what was actually returned).
pub const TO_DEVICE_LIMIT: usize = 100;

/// Parameters `crate::routes::sync` parses out of the request and hands to [`build`].
#[derive(Debug, Clone)]
pub struct SyncParams {
    /// The parsed `since` token, if any.
    pub since: Option<SyncToken>,
    /// `full_state=true`: resend full state for every room regardless of what changed.
    pub full_state: bool,
    /// How long to long-poll before returning an empty-but-valid response.
    pub timeout: Duration,
    /// The resolved filter (`crate::filter::resolve`).
    pub filter: SyncFilter,
    /// The responding device, if the requester is bound to one. Ordinary user sessions always
    /// are; a handful of exotic callers (e.g. some appservice requests) are not. `None` skips
    /// `to_device`, `device_one_time_keys_count` and `device_unused_fallback_key_types` entirely
    /// -- all three are meaningless without a specific device -- but `device_lists` is still
    /// computed, since it is scoped to the user, not the device.
    pub device_id: Option<OwnedDeviceId>,
}

/// Which pagination strategy a room's timeline uses this response, and why. See the module docs.
enum ResumeMode {
    /// Resume forward from a known room-local position (an ordinary incremental delta).
    Incremental(i64),
    /// No known baseline for this room: treat it like a fresh room in an initial sync (most
    /// recent `limit` events, full current state).
    FreshRoom,
}

async fn resume_mode<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    user_id: &UserId,
    room_id: &RoomId,
    baseline: &SyncToken,
    is_initial: bool,
    membership: &MembershipRecord,
) -> Result<ResumeMode, UserError> {
    if is_initial {
        // An initial (or `full_state=true`) sync always wants the fresh-room treatment: the most
        // recent history plus full current state, never a forward page from a membership event
        // (which would wrongly exclude pre-join history a joined member is entitled to see under
        // `shared` history visibility).
        return Ok(ResumeMode::FreshRoom);
    }
    if let Some(pos) = hub
        .store()
        .room_pos_as_of(user_id, room_id, baseline.feed_seq)
        .await?
    {
        // A position to resume from is not the same as having been *in* the room at it. Somebody
        // who was invited and has now accepted, or who left and has come back, has feed history
        // from before they were joined; resumed from there, all they are sent is their own join,
        // with no state -- so no `m.room.encryption`, and Element offered to send plain text
        // into an encrypted room. They are new to the room and get it whole. Only worth asking
        // the room when their membership has changed since that position at all.
        if membership.membership == "join" && membership.room_pos > pos {
            let handle = hub.rooms().get_or_load(room_id).await?;
            let user = user_id.to_owned();
            let was_joined = handle
                .query(move |actor| actor.was_joined_at(&user, pos))
                .await?;
            if !was_joined {
                return Ok(ResumeMode::FreshRoom);
            }
        }
        return Ok(ResumeMode::Incremental(pos));
    }
    // No feed history at or before the token for this room. There are two ways to get here, and
    // they want opposite things.
    //
    // A room that is *new to this client* -- created, or joined, after the token was issued. It
    // needs the fresh-room treatment: the room's state and its recent history, as if it had
    // turned up in an initial sync. Resuming "incrementally" from the user's own membership
    // position instead starts at or after their own join, so the create event, the power levels
    // and that join were in no timeline at all. That went unnoticed for as long as every
    // incremental sync also carried the room's whole state.
    //
    // A room that has been "hot" (`crate::hub`'s module docs) for as long as the user has been a
    // member. A hot room never gets feed entries, so it has no feed history *ever*, and cannot be
    // told from a new one by looking for some. For those, `membership.room_pos` (the position of
    // this user's own last membership-changing event, set regardless of hot/cold -- see
    // `crate::hub::SessionHub::process_room_update`) is the baseline: resuming forward from it
    // can only ever *repeat* events the client already received, never skip real ones. The cost
    // is that a hot room joined after the token is resumed from that join rather than sent
    // whole; the client recovers the rest from `/state` and `/messages`, and it is the rarer
    // case by a wide margin.
    if membership.hot_room && membership.room_pos > 0 {
        return Ok(ResumeMode::Incremental(membership.room_pos));
    }
    Ok(ResumeMode::FreshRoom)
}

/// One room's rendered timeline, plus whether it was capped and, if so, a token to page further
/// back with (reusing `hs_room`'s own `/messages`-shaped pagination tokens -- see the module
/// docs).
struct Timeline {
    events: Vec<Value>,
    limited: bool,
    prev_batch: Option<String>,
}

/// How many of a newly joined room's members have their presence sent to the joiner along with
/// the room. See `build`'s `newly_visible`.
const NEWLY_JOINED_PRESENCE_LIMIT: usize = 500;

/// How many raw timeline events a single `/sync` response will fetch in one `paginate` call once
/// `room.timeline` carries a content filter (`types`/`not_types`/`senders`/`not_senders`), rather
/// than the plain `limit` an unfiltered request uses. A filter that excludes nearly everything
/// (e.g. `types: ["m.room.message"]` in a room dominated by reactions and edits) could otherwise
/// need to scan arbitrarily far back to fill `limit` post-filter events; this crate does not loop
/// indefinitely to do so (see [`build_incremental_timeline`]/[`build_fresh_timeline`]'s doc
/// comments for exactly what "conservative" means for `limited` in that case). Matches this
/// crate's existing "bounded response" philosophy (`TO_DEVICE_LIMIT`).
const FILTERED_TIMELINE_SCAN: usize = 500;

/// Renders one event for a client, carrying `unsigned.prev_content`/`replaces_state`/`prev_sender`
/// when it is a state event that replaced another one.
///
/// Without this a client cannot tell a display-name change from a join and renders both as "Alice
/// joined the room" -- and `/sync` is the endpoint whose output a client's timeline is actually
/// built from, so fixing only `/messages` and `/state` fixes only what scrollback shows.
/// `replaced_state_for` decides the history-visibility question about the *replaced* event, which
/// is why the requesting user has to reach this far down.
fn rendered_with_replaced_state(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    event: &Event,
    requester: &UserId,
) -> Value {
    attach_replaced_state(
        client_event_json(event),
        actor.replaced_state_for(event, requester).as_ref(),
    )
}

/// A room's timeline for an incremental sync: what happened after `resume_pos`.
///
/// Usually that is a handful of events and they are all returned, oldest first. When it is more
/// than `limit` there is a *gap*, and the spec is specific about which side of it the client
/// gets: the most recent `limit` events, with `limited: true` and a `prev_batch` from which
/// paginating backwards recovers the rest. So a gap is answered by [`build_fresh_timeline`], the
/// same newest-first page an initial sync uses.
///
/// It used to be answered with the *oldest* `limit` events. The token handed back alongside them
/// is positioned at the end of the room, so everything after that first page was never delivered
/// by any later sync either, and `prev_batch` pointed back past `resume_pos` into history the
/// client already had. Whatever did not fit in one page was simply lost to that client -- which
/// is what happens to a phone that has been offline for an hour in a busy room.
///
/// With a content filter the forward scan is bounded by [`FILTERED_TIMELINE_SCAN`], and running
/// into that bound is treated as a gap as well: there may be matching events beyond it, the token
/// is going to skip past them regardless, and the newest matching events are the ones to send.
fn build_incremental_timeline(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    resume_pos: i64,
    limit: usize,
    content_filter: Option<&crate::filter::RoomEventFilter>,
    requester: &UserId,
    upto: Option<i64>,
) -> Timeline {
    let from = Some(PaginationToken::new(resume_pos, Direction::Forward));
    // One more than could be returned, so that "exactly `limit` new events" (no gap) can be told
    // from "more than `limit`" (a gap) without a second query.
    let request = content_filter.map_or(limit, |_| limit.max(FILTERED_TIMELINE_SCAN)) + 1;
    let (raw, _) = actor.paginate(from, Direction::Forward, request);
    let scan_cut_short = raw.len() == request;

    let events: Vec<&Event> = raw
        .into_iter()
        .filter(|e| visible_in_sync(actor, e, requester))
        .filter(|e| {
            content_filter
                .is_none_or(|f| f.matches(&e.header().event_type, e.header().sender.as_str()))
        })
        .collect();

    if events.len() > limit || scan_cut_short {
        let mut newest = build_fresh_timeline(actor, limit, content_filter, requester, upto);
        newest.limited = true;
        return newest;
    }

    let prev_batch = if events.is_empty() {
        None
    } else {
        Some(PaginationToken::new(resume_pos, Direction::Backward).to_string())
    };
    Timeline {
        events: events
            .into_iter()
            .map(|event| rendered_with_replaced_state(actor, event, requester))
            .collect(),
        limited: false,
        prev_batch,
    }
}

/// Whether `event` belongs in `requester`'s sync timeline: the history-visibility module's
/// per-event rule, which `/messages` and `/event` already applied and `/sync` did not.
///
/// Without it a timeline was whatever the room held. Somebody who had left, or been kicked or
/// banned, and then did an initial sync with `include_leave` was sent the room's *latest*
/// messages -- everything said since they were gone -- and somebody who joined a room whose
/// history is visible only to members from the point they joined was sent what came before. An
/// event whose visibility cannot be worked out is left out.
fn visible_in_sync(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    event: &Event,
    requester: &UserId,
) -> bool {
    actor.event_visible_to(event, requester).unwrap_or(false)
}

/// The most recent `limit` events `requester` may see, oldest first.
///
/// `upto` is the room position of the requester's own departure, for a room they have left or
/// been removed from: the page then ends there rather than at the room's live end, which for
/// them is a stretch of events they may not read and so would come back empty however much
/// they are entitled to from before.
fn build_fresh_timeline(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    limit: usize,
    content_filter: Option<&crate::filter::RoomEventFilter>,
    requester: &UserId,
    upto: Option<i64>,
) -> Timeline {
    let request = content_filter.map_or(limit, |_| limit.max(FILTERED_TIMELINE_SCAN));
    let from = upto.map(|pos| PaginationToken::new(pos.saturating_add(1), Direction::Backward));
    let (raw, next) = actor.paginate(from, Direction::Backward, request);
    let raw_exhausted = raw.len() < request;
    let raw: Vec<&Event> = raw
        .into_iter()
        .filter(|e| visible_in_sync(actor, e, requester))
        .collect();

    let (mut events, limited): (Vec<&Event>, bool) = match content_filter {
        None => {
            // `raw_exhausted` is about the page as fetched, before visibility was applied: a
            // full page means there may be more behind it, whatever survived the filter.
            let limited = if raw_exhausted {
                false
            } else {
                let (more, _) = actor.paginate(next, Direction::Backward, 1);
                !more.is_empty()
            };
            (raw, limited)
        }
        Some(f) => {
            // `raw` is newest-first; filter first (order-preserving), then take the newest
            // `limit` of what survives -- taking from the *front* here, unlike the incremental
            // case's `take(limit)` from a forward-ordered list, is what keeps this the most
            // recent `limit` matching events rather than the oldest ones in the scanned window.
            let filtered: Vec<&Event> = raw
                .into_iter()
                .filter(|e| f.matches(&e.header().event_type, e.header().sender.as_str()))
                .collect();
            let truncated = filtered.len() > limit;
            let events = if truncated {
                filtered.into_iter().take(limit).collect()
            } else {
                filtered
            };
            // Same conservative reasoning as `build_incremental_timeline` above.
            let limited = truncated || !raw_exhausted;
            (events, limited)
        }
    };
    // The exact continuation token for "everything older than the scanned window" is not
    // knowable here, when a content filter is present, without threading the filtered-out tail's
    // own position through (this crate does not track that) -- `next` (from the raw, unfiltered
    // scan) is still a safe, conservative choice either way: paging from it can only ever
    // *repeat or skip past* already-scanned raw events, never lose events this response already
    // returned.
    let prev_batch = next.map(|t| t.to_string());
    events.reverse(); // paginate(Backward) is newest-first; /sync wants chronological order.
    Timeline {
        events: events
            .into_iter()
            .map(|event| rendered_with_replaced_state(actor, event, requester))
            .collect(),
        limited,
        prev_batch,
    }
}

/// The state event types stripped state carries, from the client-server API's own list
/// ("Stripped state should contain some or all of the following"). `m.room.create` is
/// **required** there as of Matrix v1.16.
const STRIPPED_STATE_TYPES: &[&str] = &[
    "m.room.create",
    "m.room.join_rules",
    "m.room.canonical_alias",
    "m.room.name",
    "m.room.avatar",
    "m.room.topic",
    "m.room.encryption",
];

/// The state of a room as offered to somebody who is not in it: enough to render an invite or a
/// knock without being able to read the room.
///
/// Two things here are easy to get wrong and were both wrong:
///
/// **The recipient's own `m.room.member` event has to be in it.** It is not in the type list
/// above -- that list is about describing the *room* -- but the spec's own `invite_state` example
/// carries it, and it is the only thing in the response that says who was invited and by whom. A
/// client that reads membership out of `invite_state` (Complement's `syncMembershipIn` checker
/// does, and so every invite test in `csapi` did) sees an invite with no invitee at all without
/// it. The inviter's member event goes in too, because "Alice invited you" needs Alice's display
/// name and avatar, and the recipient cannot fetch them from a room they have not joined.
///
/// **A stripped state event may carry only four properties**: `sender`, `type`, `state_key` and
/// `content`. Not `event_id`, not `origin_server_ts`, not `room_id`, not `unsigned` -- which is
/// what [`client_event_json`] produces, and what this used to send.
fn stripped_state(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    recipient: &UserId,
) -> Result<Vec<Value>, hs_room::RoomError> {
    let mut out = Vec::new();
    let mut inviter: Option<String> = None;
    for event in actor.full_state()? {
        let header = event.header();
        if STRIPPED_STATE_TYPES.contains(&header.event_type.as_str()) {
            out.push(strip(event));
        } else if header.event_type == "m.room.member"
            && header.state_key.as_deref() == Some(recipient.as_str())
        {
            inviter = Some(header.sender.to_string());
            out.push(strip(event));
        }
    }
    // Second pass rather than a lookup inside the first: whose event to add is only known once
    // the recipient's own membership has been seen, and `full_state` has no ordering guarantee
    // that would put it first.
    if let Some(inviter) = inviter
        && inviter != recipient.as_str()
        && let Some(event) = actor.state_event("m.room.member", &inviter)?
    {
        out.push(strip(event));
    }
    Ok(out)
}

/// One event as a stripped state event: the four properties the spec allows, and nothing else.
fn strip(event: &Event) -> Value {
    let full = client_event_json(event);
    json!({
        "type": full.get("type").cloned().unwrap_or(Value::Null),
        "state_key": full.get("state_key").cloned().unwrap_or(Value::Null),
        "sender": full.get("sender").cloned().unwrap_or(Value::Null),
        "content": full.get("content").cloned().unwrap_or_else(|| json!({})),
    })
}

/// Full current state, minus whatever event ids are already present in `timeline_events` (avoids
/// duplicating a state event this response's timeline already carries -- see the module docs'
/// caveat that this is an approximation of "state at the start of the timeline", not an exact
/// one), optionally lazy-loaded (`m.room.member` restricted to timeline senders plus the
/// requester's own membership, when `lazy` is set), optionally content-filtered by
/// `room.state.types`/`not_types`/`senders`/`not_senders` (`content_filter`). Unlike the timeline
/// builders above, this has no `limit`/scan-bound concern: `full_state` is already the room's
/// entire *current* state (one event per `(type, state_key)`, not a history), so filtering it is
/// a plain, unbounded `Vec` filter.
fn build_state_section(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    timeline_event_ids: &HashSet<String>,
    lazy: bool,
    timeline_senders: &HashSet<String>,
    self_user: &UserId,
    content_filter: Option<&crate::filter::RoomEventFilter>,
) -> Result<Vec<Value>, hs_room::RoomError> {
    // Through the reader's view, not the room's live state: for somebody who has left, that is
    // the state as of their leaving. The live state told them who had joined since, what the
    // room had been renamed to, and anything else that changed after they were gone.
    Ok(actor
        .full_state_for_reader(self_user)?
        .unwrap_or_default()
        .into_iter()
        .filter(|e| !timeline_event_ids.contains(e.event_id().as_str()))
        .filter(|e| {
            if !lazy || e.header().event_type != "m.room.member" {
                return true;
            }
            let is_self = e.header().state_key.as_deref() == Some(self_user.as_str());
            is_self || timeline_senders.contains(e.header().sender.as_str())
        })
        .filter(|e| {
            content_filter
                .is_none_or(|f| f.matches(&e.header().event_type, e.header().sender.as_str()))
        })
        .map(|e| rendered_with_replaced_state(actor, e, self_user))
        .collect())
}

/// Builds the `summary` key of a joined room's `/sync` entry (the spec's "Room Summary"): always
/// `m.joined_member_count`/`m.invited_member_count`, plus `m.heroes` -- up to five other members'
/// user IDs, lexicographically ordered for a deterministic response -- when (and only when) the
/// room has neither `m.room.name` nor `m.room.canonical_alias` set. Heroes exist purely so a
/// client can synthesize a name for a room that has none of its own; a room that already has a
/// name or alias gets an empty `m.heroes` list, matching every real client's own precedence (own
/// name/alias always wins, heroes are a last resort) and saving the (cheap but pointless) work of
/// picking candidates nobody will use. Ordering heroes lexicographically rather than by "oldest
/// membership" (Synapse's own tiebreak) is a documented simplification -- see
/// `docs/status/05-sync.md` -- since nothing in this crate tracks per-member join order today and
/// the spec does not mandate a particular order.
///
/// # Errors
/// Returns [`hs_room::RoomError`] if the state store fails.
fn build_room_summary(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    user_id: &UserId,
) -> Result<Value, hs_room::RoomError> {
    let mut joined_member_count = 0u64;
    let mut invited_member_count = 0u64;
    let mut hero_candidates: BTreeSet<String> = BTreeSet::new();
    for member in actor.members()? {
        let event = client_event_json(member);
        let Some(state_key) = event.get("state_key").and_then(Value::as_str) else {
            continue;
        };
        let membership = event
            .get("content")
            .and_then(|c| c.get("membership"))
            .and_then(Value::as_str);
        match membership {
            Some("join") => {
                joined_member_count += 1;
                if state_key != user_id.as_str() {
                    hero_candidates.insert(state_key.to_owned());
                }
            }
            Some("invite") => {
                invited_member_count += 1;
                if state_key != user_id.as_str() {
                    hero_candidates.insert(state_key.to_owned());
                }
            }
            _ => {}
        }
    }

    let has_own_name = |event_type: &str, field: &str| -> Result<bool, hs_room::RoomError> {
        Ok(actor
            .state_event(event_type, "")?
            .map(client_event_json)
            .and_then(|e| {
                e.get("content")
                    .and_then(|c| c.get(field))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|s| !s.is_empty()))
    };
    let already_named =
        has_own_name("m.room.name", "name")? || has_own_name("m.room.canonical_alias", "alias")?;

    let heroes: Vec<Value> = if already_named {
        Vec::new()
    } else {
        hero_candidates
            .into_iter()
            .take(5)
            .map(Value::String)
            .collect()
    };

    Ok(json!({
        "m.heroes": heroes,
        "m.joined_member_count": joined_member_count,
        "m.invited_member_count": invited_member_count,
    }))
}

/// Builds a full `/sync` v2 response for `user_id`, long-polling as needed. Returns the response
/// JSON and the [`SyncToken`] its `next_batch` carries (the caller records the device cursor;
/// see the module docs and `crate::routes::sync`).
///
/// # Errors
/// Returns [`UserError`] on a store or room-actor failure.
pub async fn build<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    e2e: &Arc<dyn E2eStore>,
    user_id: &UserId,
    params: SyncParams,
) -> Result<(Value, SyncToken), UserError> {
    let baseline = params.since.unwrap_or_else(SyncToken::initial);
    let is_initial = params.since.is_none();
    let device_id = params.device_id.clone();

    if !is_initial {
        long_poll(
            hub,
            e2e,
            user_id,
            device_id.as_deref(),
            &baseline,
            params.timeout,
        )
        .await?;
    }

    let store = hub.store();
    let timeline_limit = params.filter.timeline_limit(DEFAULT_TIMELINE_LIMIT);

    // The positions the next token will carry are fixed *here*, before anything is read, and
    // not afterwards. Whatever arrives while this response is being put together then has a
    // position beyond the token and is picked up by the next sync -- possibly after also having
    // made it into this one, which a client de-duplicates by event ID. Taken afterwards, as they
    // were, the token covered things that arrived too late to be in the response: reported as
    // consumed, never sent.
    let new_feed_seq = store.latest_feed_seq(user_id).await?.max(baseline.feed_seq);
    let new_account_data_seq = store
        .latest_account_data_seq(user_id)
        .await?
        .max(baseline.account_data_seq);
    // And this device says how far it is about to read before it reads. Feed entries are
    // overwritten in place until a device's cursor has passed them (`append_feed_entry`), so an
    // entry this response is going to report as consumed must be frozen first: otherwise an
    // event landing mid-response is folded into it, its stored position moves on past that
    // event, and the next sync resumes from *after* it. `routes::sync` records the same cursor
    // once the response is built, which is by then a no-op.
    if let Some(device_id) = &device_id {
        store
            .record_device_cursor(user_id, device_id, new_feed_seq)
            .await?;
    }

    let mut candidate_rooms: BTreeSet<OwnedRoomId> = if is_initial {
        store
            .list_memberships(user_id)
            .await?
            .into_iter()
            .map(|m| m.room_id)
            .collect()
    } else {
        let mut set: BTreeSet<OwnedRoomId> = store
            .feed_since(user_id, baseline.feed_seq)
            .await?
            .into_iter()
            .map(|e| e.room_id)
            .collect();
        for m in store.list_memberships(user_id).await? {
            if m.hot_room && matches!(m.membership.as_str(), "join" | "invite" | "knock") {
                set.insert(m.room_id);
            }
        }
        set
    };

    // `m.typing`: gathered up front, against *every* joined room (not just the feed-derived
    // candidate set above), since a typing-only change never touches the feed at all
    // (`crate::typing`'s module docs). Any room with something new gets folded into
    // `candidate_rooms` here so the main loop below renders it even when nothing else changed.
    let mut typing_by_room: HashMap<OwnedRoomId, Vec<Value>> = HashMap::new();
    let mut new_typing_seq = baseline.typing_seq;
    // `m.receipt`: gathered the same way and for the same reason as typing above -- a
    // receipt-only change never touches the feed either. See `crate::receipts`'s module docs for
    // why this is a separate in-memory registry (mirroring typing/presence) rather than a
    // `store` table, and for the privacy scope `receipt_content_for` enforces per viewer
    // (`user_id`, always the syncing user themselves here).
    let mut receipts_by_room: HashMap<OwnedRoomId, Value> = HashMap::new();
    let mut new_receipts_seq = baseline.receipts_seq;
    for m in store.list_memberships(user_id).await? {
        if m.membership != "join" {
            continue;
        }
        let (users, seq) = hub.typing_users(&m.room_id).await;
        new_typing_seq = new_typing_seq.max(seq);
        if seq > baseline.typing_seq {
            candidate_rooms.insert(m.room_id.clone());
            typing_by_room.insert(
                m.room_id.clone(),
                vec![json!({
                    "type": "m.typing",
                    "content": {"user_ids": users},
                })],
            );
        }
        let receipts_seq = hub.receipts_seq(&m.room_id).await;
        new_receipts_seq = new_receipts_seq.max(receipts_seq);
        if receipts_seq > baseline.receipts_seq {
            candidate_rooms.insert(m.room_id.clone());
            let (content, _) = hub.receipt_content_for(&m.room_id, user_id).await;
            if content.as_object().is_some_and(|o| !o.is_empty()) {
                receipts_by_room.insert(m.room_id, content);
            }
        }
    }

    let mut join = serde_json::Map::new();
    let mut invite = serde_json::Map::new();
    let mut knock = serde_json::Map::new();
    let mut leave = serde_json::Map::new();
    // Other users' `m.room.member` events (leave/ban) seen in a room's timeline this response
    // sends -- candidates for `device_lists.left` once intersected with "no longer shared" below.
    // See the module docs; deliberately built from data this loop already computes, not a second
    // pass over history.
    let mut left_candidates: BTreeSet<OwnedUserId> = BTreeSet::new();
    // Everyone in a room this user joined since their token. Their presence is sent whatever its
    // stamp says: see where it is read, below.
    let mut newly_visible: BTreeSet<OwnedUserId> = BTreeSet::new();
    // Everyone this user has come to share a room with since their token, from either side: the
    // members of a room they have just joined, and whoever has just joined a room they were
    // already in. The spec puts both in `device_lists.changed` ("or who now share an encrypted
    // room with the client"), and nothing did: the only thing that ever reached `changed` was a
    // key upload. Not restricted to encrypted rooms, because a room can become one later and
    // nothing would announce its members then.
    let mut newly_shared: BTreeSet<OwnedUserId> = BTreeSet::new();

    for room_id in &candidate_rooms {
        if !params.filter.room_allowed(room_id.as_str()) {
            continue;
        }
        let Some(membership) = store.get_membership(user_id, room_id).await? else {
            continue;
        };
        if (membership.membership == "leave" || membership.membership == "ban")
            && is_initial
            && !params.filter.include_leave()
        {
            // Historical leaves/bans are omitted from an initial sync unless the filter asks
            // for them. A room reached via the *incremental* candidate set (the feed) always
            // means something just happened -- e.g. the user was just kicked -- so it is always
            // included there regardless of this flag: the client needs to see that leave event.
            continue;
        }

        let handle = hub.rooms().get_or_load(room_id).await?;
        let room_id_owned = room_id.clone();
        let membership_value = membership.membership.clone();
        let full_state_requested = params.full_state;
        let lazy = params.filter.lazy_load_members();
        let user_id_owned = user_id.to_owned();
        // Cloned per room (cheap: absent in the overwhelming common case, and even when present
        // this is a handful of small `Vec<String>`s) since the `move` closure below needs owned
        // data, not a borrow of `params` -- see that closure's own call site for why (`query`
        // requires a `'static` closure).
        let timeline_content_filter = params.filter.timeline_content_filter().cloned();
        let state_content_filter = params.filter.state_content_filter().cloned();

        match membership_value.as_str() {
            "invite" => {
                let user_for_invite = user_id_owned.clone();
                let events = handle
                    .query(move |actor| stripped_state(actor, &user_for_invite))
                    .await?;
                invite.insert(
                    room_id_owned.to_string(),
                    json!({"invite_state": {"events": events}}),
                );
                continue;
            }
            "knock" => {
                let user_for_knock = user_id_owned.clone();
                let events = handle
                    .query(move |actor| stripped_state(actor, &user_for_knock))
                    .await?;
                knock.insert(
                    room_id_owned.to_string(),
                    json!({"knock_state": {"events": events}}),
                );
                continue;
            }
            _ => {}
        }

        let resume = resume_mode(hub, user_id, room_id, &baseline, is_initial, &membership).await?;
        let force_full_state = full_state_requested || matches!(resume, ResumeMode::FreshRoom);
        let fresh_room = matches!(resume, ResumeMode::FreshRoom);
        if !is_initial && fresh_room && membership.membership == "join" {
            let members = hub.joined_member_ids(room_id).await?;
            // Bounded, because a room can have tens of thousands of members and this is one
            // response. Past the bound the client still learns about people as they do things.
            newly_visible.extend(members.iter().take(NEWLY_JOINED_PRESENCE_LIMIT).cloned());
            // Not bounded: somebody left out of this is somebody whose devices never get the
            // keys to what this user says.
            newly_shared.extend(members);
        }

        let account_data = if force_full_state {
            store.list_room_account_data(user_id, room_id).await?
        } else {
            store
                .list_room_account_data(user_id, room_id)
                .await?
                .into_iter()
                .filter(|a| a.changed_seq > baseline.account_data_seq)
                .collect()
        };
        let account_data_json: Vec<Value> = account_data
            .iter()
            .map(|a| json!({"type": a.event_type, "content": a.content}))
            .collect();

        // Where this user's view of the room ends, if it has: the position of their own leave,
        // kick or ban (`MembershipRecord::room_pos` is that of their latest membership event).
        let departed_at = matches!(membership.membership.as_str(), "leave" | "ban")
            .then_some(membership.room_pos)
            .filter(|pos| *pos > 0);

        let (timeline, state_events, summary) = handle
            .query(move |actor| {
                let timeline = match resume {
                    ResumeMode::Incremental(pos) => build_incremental_timeline(
                        actor,
                        pos,
                        timeline_limit,
                        timeline_content_filter.as_ref(),
                        &user_id_owned,
                        departed_at,
                    ),
                    ResumeMode::FreshRoom => build_fresh_timeline(
                        actor,
                        timeline_limit,
                        timeline_content_filter.as_ref(),
                        &user_id_owned,
                        departed_at,
                    ),
                };
                let timeline_ids: HashSet<String> = timeline
                    .events
                    .iter()
                    .filter_map(|e| e.get("event_id").and_then(Value::as_str).map(str::to_owned))
                    .collect();
                let timeline_senders: HashSet<String> = timeline
                    .events
                    .iter()
                    .filter_map(|e| e.get("sender").and_then(Value::as_str).map(str::to_owned))
                    .collect();
                let state = if force_full_state || timeline.limited {
                    // A room the client has no baseline for (an initial sync, `full_state`, a
                    // room new to it) or a gap: the client's idea of the room's state cannot be
                    // brought up to date by the timeline alone, so it gets the current state --
                    // less whatever the timeline already carries, which would otherwise reach it
                    // twice. A room sent whole used to skip that subtraction, and Complement's
                    // `TestArchivedRoomsHistory` is a client that counts. For a gap this is more
                    // than the strict minimum -- the spec asks for the state changes *between*
                    // `since` and the start of the timeline -- and it errs in the safe direction:
                    // a repeated state event is harmless, a missed one is a stale room.
                    build_state_section(
                        actor,
                        &timeline_ids,
                        lazy,
                        &timeline_senders,
                        &user_id_owned,
                        state_content_filter.as_ref(),
                    )?
                } else {
                    // An ordinary incremental sync: every state change since `since` is *in* the
                    // timeline, so there is nothing for `state` to add -- except, under lazy
                    // loading, the membership of whoever sent those events, which the client may
                    // never have been given. This used to send the room's entire state on every
                    // sync that had so much as one new message in it.
                    build_state_section(
                        actor,
                        &timeline_ids,
                        lazy,
                        &timeline_senders,
                        &user_id_owned,
                        state_content_filter.as_ref(),
                    )?
                    .into_iter()
                    .filter(|e| {
                        lazy && e.get("type").and_then(Value::as_str) == Some("m.room.member")
                    })
                    .collect()
                };
                let summary = build_room_summary(actor, &user_id_owned)?;
                Ok::<_, hs_room::RoomError>((timeline, state, summary))
            })
            .await?;

        for event in &timeline.events {
            if event.get("type").and_then(Value::as_str) != Some("m.room.member") {
                continue;
            }
            let Some(state_key) = event.get("state_key").and_then(Value::as_str) else {
                continue;
            };
            let membership = event
                .get("content")
                .and_then(|c| c.get("membership"))
                .and_then(Value::as_str);
            if state_key == user_id.as_str() {
                // The user's *own* departure: everybody in the room they have just left is
                // somebody they may no longer share a room with, and so whose device list they
                // will stop hearing about. Whether that is true of each is decided below, against
                // the rooms they are still in. This used to `continue`, so leaving a room never
                // produced a `device_lists.left` at all, and a client went on trusting a device
                // list it was no longer being kept up to date on.
                if matches!(membership, Some("leave") | Some("ban")) {
                    left_candidates.extend(hub.joined_member_ids(room_id).await?);
                }
                continue;
            }
            if matches!(membership, Some("leave") | Some("ban"))
                && let Ok(other) = ruma::UserId::parse(state_key)
            {
                left_candidates.insert(other);
            }
            // Somebody else arriving -- and not merely changing their display name, which is
            // also a `join` event, and in a big room would have every member re-fetching the
            // keys of anyone who so much as changed their avatar.
            let was_joined = event
                .pointer("/unsigned/prev_content/membership")
                .and_then(Value::as_str)
                == Some("join");
            if membership == Some("join")
                && !was_joined
                && let Ok(other) = ruma::UserId::parse(state_key)
            {
                newly_shared.insert(other);
            }
        }
        // A gap: whoever joined inside it is in no timeline this client will be sent. Everyone
        // in the room is named instead -- more than the minimum, and the direction that costs a
        // key query rather than a message nobody can read.
        if !is_initial && !fresh_room && timeline.limited && membership.membership == "join" {
            newly_shared.extend(hub.joined_member_ids(room_id).await?);
        }

        // Only a joined room can have typing or receipt activity (both maps above are only ever
        // populated for `membership == "join"` rows), so a leave/ban room correctly never has an
        // entry here.
        let mut ephemeral_events = typing_by_room.get(room_id).cloned().unwrap_or_default();
        if let Some(content) = receipts_by_room.get(room_id) {
            ephemeral_events.push(json!({
                "type": "m.receipt",
                "content": content,
            }));
        }

        let nothing_changed = timeline.events.is_empty()
            && account_data_json.is_empty()
            && ephemeral_events.is_empty()
            && !force_full_state;
        if nothing_changed && !is_initial {
            continue;
        }
        if timeline.events.is_empty()
            && state_events.is_empty()
            && account_data_json.is_empty()
            && ephemeral_events.is_empty()
        {
            continue;
        }

        let bucket_key = room_id.to_string();
        match membership_value.as_str() {
            "leave" | "ban" => {
                leave.insert(
                    bucket_key,
                    json!({
                        "state": {"events": state_events},
                        "timeline": {
                            "events": timeline.events,
                            "limited": timeline.limited,
                            "prev_batch": timeline.prev_batch,
                        },
                        "account_data": {"events": account_data_json},
                    }),
                );
            }
            _ => {
                // `unread_notifications`/`unread_thread_notifications`
                // (`docs/status/10-push.md`'s "Interfaces provided"): a direct, side-effect-free
                // keyed lookup, safe to call unconditionally for every room already being
                // emitted. Stays the hard-zero placeholder until `hs-cli` installs a store via
                // `SessionHub::install_counts_store` -- see that method's doc comment.
                let room_counts = match hub.counts_store() {
                    Some(counts) => counts.get_room_counts(user_id, room_id).await?,
                    None => hs_push::counts::RoomNotificationCounts::default(),
                };
                let totals = room_counts.totals();
                let thread_counts: serde_json::Map<String, Value> = room_counts
                    .threads
                    .iter()
                    .map(|(thread_root, c)| {
                        (
                            thread_root.to_string(),
                            json!({
                                "highlight_count": c.highlight_count,
                                "notification_count": c.notification_count,
                            }),
                        )
                    })
                    .collect();
                join.insert(
                    bucket_key,
                    json!({
                        "state": {"events": state_events},
                        "timeline": {
                            "events": timeline.events,
                            "limited": timeline.limited,
                            "prev_batch": timeline.prev_batch,
                        },
                        "account_data": {"events": account_data_json},
                        "ephemeral": {"events": ephemeral_events},
                        "unread_notifications": {
                            "highlight_count": totals.highlight_count,
                            "notification_count": totals.notification_count,
                        },
                        "unread_thread_notifications": thread_counts,
                        "summary": summary,
                    }),
                );
            }
        }
    }

    let global_account_data = if is_initial {
        store.list_global_account_data(user_id).await?
    } else {
        store
            .list_global_account_data(user_id)
            .await?
            .into_iter()
            .filter(|a| a.changed_seq > baseline.account_data_seq)
            .collect()
    };
    let mut global_account_data_json: Vec<Value> = global_account_data
        .iter()
        .map(|a| json!({"type": a.event_type, "content": a.content}))
        .collect();

    // `m.push_rules` (`docs/status/10-push.md`'s "Interfaces provided"): always included on an
    // initial sync, included on an incremental one only when the ruleset actually changed since
    // `baseline`. No-op (never emitted, `push_rules_seq` never advances) until `hs-cli` installs
    // a store via `SessionHub::install_push_rules_store` -- see that method's doc comment for the
    // exact call this needs.
    let new_push_rules_seq = match hub.push_rules_store() {
        Some(rulesets) => {
            let push_rules = rulesets.account_data_for_sync(user_id).await?;
            if is_initial || push_rules.changed_seq > baseline.push_rules_seq {
                global_account_data_json.push(json!({
                    "type": "m.push_rules",
                    "content": push_rules.content,
                }));
            }
            push_rules.changed_seq.max(baseline.push_rules_seq)
        }
        None => baseline.push_rules_seq,
    };

    // `to_device`: see the module docs for why `delete_up_to(baseline.to_device_seq)` -- keyed
    // off the token *this* request presented, acknowledging the previous response -- happens
    // before polling for anything new, not after this response is built.
    let (to_device_events, new_to_device_seq) = if let Some(device_id) = device_id.as_deref() {
        ToDeviceStore::delete_up_to(&**e2e, user_id, device_id, baseline.to_device_seq).await?;
        let (messages, next_cursor) = ToDeviceStore::poll_since(
            &**e2e,
            user_id,
            device_id,
            baseline.to_device_seq,
            TO_DEVICE_LIMIT,
        )
        .await?;
        let events: Vec<Value> = messages
            .into_iter()
            .map(|m| json!({"sender": m.sender, "type": m.event_type, "content": m.content}))
            .collect();
        (events, next_cursor)
    } else {
        (Vec::new(), baseline.to_device_seq)
    };

    // `device_one_time_keys_count`/`device_unused_fallback_key_types`: a straight passthrough,
    // populated on every response once a device is known (not only when non-empty -- a client
    // treats an *absent* `device_one_time_keys_count` as "zero keys left", per the spec and
    // `docs/rfcs/0013-e2ee-sync-extensions.md`'s reproduced evidence of what that omission does).
    let (otk_counts_json, fallback_types_json) = if let Some(device_id) = device_id.as_deref() {
        let counts = OneTimeKeyStore::count_one_time_keys(&**e2e, user_id, device_id).await?;
        let fallback =
            FallbackKeyStore::unused_fallback_key_algorithms(&**e2e, user_id, device_id).await?;
        (Some(json!(counts)), Some(json!(fallback)))
    } else {
        (None, None)
    };

    // Computed once, shared by `device_lists` (below) and `m.presence` (further below): both are
    // scoped to "everyone `user_id` currently shares a joined room with".
    let shared = shared_users(hub, user_id).await?;

    // `device_lists.changed`/`left`: per spec, only meaningful (and only sent) on an incremental
    // sync. Scoped to `shared` -- see the module docs.
    let (device_lists_json, new_device_list_seq) = if is_initial {
        // A client starting from nothing holds no device lists to be told are stale, so its
        // token starts at the present. It used to start at zero, and the first incremental sync
        // after every initial one reported everybody who had ever uploaded a key -- which is
        // also what hid, from every test that began with an initial sync, that joining a room
        // put nobody in `changed`.
        (None, DeviceKeyStore::current_stream_pos(&**e2e).await?)
    } else {
        let upto = DeviceKeyStore::current_stream_pos(&**e2e).await?;
        let changed_all =
            DeviceKeyStore::changed_users_since(&**e2e, baseline.device_list_seq, Some(upto))
                .await?;
        // The user's own id belongs here too: it is how the device they are already signed in
        // on hears about the one they have just signed in on.
        let changed: BTreeSet<&OwnedUserId> = changed_all
            .iter()
            .filter(|u| shared.contains(*u) || u.as_str() == user_id.as_str())
            .chain(newly_shared.iter().filter(|u| shared.contains(*u)))
            .collect();
        let changed: Vec<Value> = changed
            .into_iter()
            .map(|u| Value::String(u.to_string()))
            .collect();
        let left: Vec<Value> = left_candidates
            .iter()
            .filter(|u| !shared.contains(*u))
            .map(|u| Value::String(u.to_string()))
            .collect();
        (Some(json!({"changed": changed, "left": left})), upto)
    };

    // `m.presence`: every shared user whose presence record is newer than `baseline.presence_seq`
    // (or, on an initial sync where there is no baseline to compare against, every shared user
    // who has a record at all -- the client has seen nothing yet). See the module docs.
    let mut presence_events: Vec<Value> = Vec::new();
    let mut new_presence_seq = baseline.presence_seq;
    for other in &presence_audience(&shared, user_id) {
        let Some(record) = hub.presence_of(other).await else {
            continue;
        };
        new_presence_seq = new_presence_seq.max(record.seq);
        // Newer than the token -- or belonging to somebody in a room this user has only just
        // joined. Presence has one sequence for the whole server, so the people already in that
        // room may well have last changed state long before this user's token; by stamp alone
        // the newcomer would see an empty room until each of them happened to do something.
        // (The other direction, the room learning about the newcomer, is
        // `PresenceRegistry::restamp`.)
        if is_initial || record.seq > baseline.presence_seq || newly_visible.contains(other) {
            let mut content = json!({
                "presence": record.presence,
                "last_active_ago": record.last_active_ago_ms(),
            });
            if let Some(msg) = &record.status_msg {
                content["status_msg"] = Value::String(msg.clone());
            }
            presence_events.push(json!({
                "type": "m.presence",
                "sender": other.as_str(),
                "content": content,
            }));
        }
    }

    let next_token = SyncToken {
        feed_seq: new_feed_seq,
        account_data_seq: new_account_data_seq,
        to_device_seq: new_to_device_seq,
        device_list_seq: new_device_list_seq,
        typing_seq: new_typing_seq,
        presence_seq: new_presence_seq,
        push_rules_seq: new_push_rules_seq,
        receipts_seq: new_receipts_seq,
    };

    let mut rooms = serde_json::Map::new();
    if !join.is_empty() {
        rooms.insert("join".into(), Value::Object(join));
    }
    if !invite.is_empty() {
        rooms.insert("invite".into(), Value::Object(invite));
    }
    if !knock.is_empty() {
        rooms.insert("knock".into(), Value::Object(knock));
    }
    if !leave.is_empty() {
        rooms.insert("leave".into(), Value::Object(leave));
    }

    let mut response = json!({
        "next_batch": next_token.encode(),
        "rooms": rooms,
        "presence": {"events": presence_events},
        "account_data": {"events": global_account_data_json},
    });
    if device_id.is_some() {
        response["to_device"] = json!({"events": to_device_events});
        response["device_one_time_keys_count"] = otk_counts_json.unwrap_or_else(|| json!({}));
        response["device_unused_fallback_key_types"] =
            fallback_types_json.unwrap_or_else(|| json!([]));
    }
    if let Some(device_lists) = device_lists_json {
        response["device_lists"] = device_lists;
    }

    Ok((response, next_token))
}

/// Every user (other than `user_id`) currently sharing at least one *joined* room with
/// `user_id` -- the scope [`build`]'s `device_lists.changed`/`left` must respect. `hs-e2e` has no
/// room-membership notion at all (`hs_e2e::store::DeviceKeyStore`'s module doc), so this lives
/// entirely on this crate's own membership data.
async fn shared_users<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    user_id: &UserId,
) -> Result<BTreeSet<OwnedUserId>, UserError> {
    // Delegates to `SessionHub::users_sharing_room_with`, which needs the identical scope for
    // `crate::hub::SessionHub::set_presence`'s wake fan-out -- one membership walk, not two.
    hub.users_sharing_room_with(user_id).await
}

/// Whose presence `user_id`'s sync carries: everyone they share a joined room with, and
/// themself -- a user's own presence is how their second device learns what their first one set.
///
/// One function, used by both [`build`] and [`has_new_data`], because the two disagreeing is a
/// busy loop. `has_new_data` used to add the user to the set and `build` did not, so a user
/// whose own record was the newest they could see was told "there is news" by one and handed an
/// empty response with an unmoved token by the other -- on every `/sync`, forever, as fast as
/// the client could ask. It became every user's problem the day `GET /sync` started marking its
/// caller online, which gives every user a record of their own.
fn presence_audience(shared: &BTreeSet<OwnedUserId>, user_id: &UserId) -> BTreeSet<OwnedUserId> {
    let mut audience = shared.clone();
    audience.insert(user_id.to_owned());
    audience
}

/// Whether anything has changed for `user_id` since `baseline` -- feed activity, the
/// account-data counter having advanced, or (when `device_id` is known) new to-device messages
/// or a device-list change. Used both by the long-poll loop's wake condition and (implicitly, by
/// returning quickly) by a plain non-blocking check.
async fn has_new_data<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    e2e: &Arc<dyn E2eStore>,
    user_id: &UserId,
    device_id: Option<&ruma::DeviceId>,
    baseline: &SyncToken,
) -> Result<bool, UserError> {
    let store = hub.store();
    if store.latest_feed_seq(user_id).await? > baseline.feed_seq {
        return Ok(true);
    }
    if store.latest_account_data_seq(user_id).await? > baseline.account_data_seq {
        return Ok(true);
    }
    // `m.push_rules`: a rule change should wake a blocked long-poll the same way any other
    // account-data change does. `store()` bypasses `CachedRulesetStore`'s cache -- fine here,
    // this is a cheap counter read, not the full ruleset (`docs/status/10-push.md`'s "Interfaces
    // provided"). No-op (never wakes) until `hs-cli` installs a store via
    // `SessionHub::install_push_rules_store` -- see that method's doc comment.
    if let Some(push_rules) = hub.push_rules_store()
        && push_rules.store().changed_seq(user_id).await? > baseline.push_rules_seq
    {
        return Ok(true);
    }
    // Hot rooms never advance the feed on write (`crate::hub`'s module docs), so their
    // possible new activity would otherwise never wake a long-poll. Check each one directly.
    // While walking every joined/invited/knocked room anyway, also check typing: unlike
    // to-device/device-list activity, `crate::hub::SessionHub::set_typing` *does* call this hub's
    // waker directly, but a lazy expiry (`crate::typing::TypingRegistry::current`'s pruning) has
    // no explicit wake call at all, so it still needs this same periodic re-check to be noticed.
    let memberships = store.list_memberships(user_id).await?;
    for m in &memberships {
        if m.hot_room && matches!(m.membership.as_str(), "join" | "invite" | "knock") {
            let handle = hub.rooms().get_or_load(&m.room_id).await?;
            // Copied out before the closure: `query` requires a `'static` closure, so it may not
            // borrow a field of this loop's membership row.
            let room_pos = m.room_pos;
            let has_more = handle
                .query(move |actor| {
                    let (events, _) = actor.paginate(
                        Some(PaginationToken::new(room_pos, Direction::Forward)),
                        Direction::Forward,
                        1,
                    );
                    !events.is_empty()
                })
                .await;
            if has_more {
                return Ok(true);
            }
        }
        if m.membership == "join" {
            let (_, typing_seq) = hub.typing_users(&m.room_id).await;
            if typing_seq > baseline.typing_seq {
                return Ok(true);
            }
            if hub.receipts_seq(&m.room_id).await > baseline.receipts_seq {
                return Ok(true);
            }
        }
    }
    // Presence: has anyone `user_id` shares a joined room with (or `user_id` itself) posted a
    // presence update since `baseline`? Scoped the same way `device_lists` is (see the module
    // docs) -- an over-broad wake here would just cost an extra response-building pass, same
    // reasoning as the to-device peek below.
    for other in &presence_audience(&shared_users(hub, user_id).await?, user_id) {
        if let Some(record) = hub.presence_of(other).await
            && record.seq > baseline.presence_seq
        {
            return Ok(true);
        }
    }
    // `hs-e2e`'s to-device queue and device-list stream have no waker hook into this hub (its
    // routes are outside this crate -- see the module docs and
    // `docs/rfcs/0013-e2ee-sync-extensions.md`), so a long-poll can only learn about them by
    // asking directly. `poll_since` with `limit: 1` is a cheap, non-destructive peek (it does not
    // delete anything -- only `delete_up_to` does).
    if let Some(device_id) = device_id {
        let (peek, _) =
            ToDeviceStore::poll_since(&**e2e, user_id, device_id, baseline.to_device_seq, 1)
                .await?;
        if !peek.is_empty() {
            return Ok(true);
        }
        // Not scoped to `shared_users` here -- an over-broad wake condition just costs an extra,
        // harmless response-building pass; only the response actually sent out (`build`'s own
        // `device_lists` computation) enforces the privacy scope.
        if DeviceKeyStore::current_stream_pos(&**e2e).await? > baseline.device_list_seq {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How often [`long_poll`] re-checks [`has_new_data`] even without an explicit wake. Needed
/// because to-device/device-list activity (unlike room activity) has no waker hook into this
/// hub's `Notify` -- see [`has_new_data`]'s module-doc-adjacent comment. Short enough that an
/// encrypted message's to-device room-key share is noticed promptly, long enough not to turn a
/// long-poll into a busy loop.
const E2E_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The long-poll loop: waits until [`has_new_data`] is true or `timeout` elapses. Registers
/// interest on the hub's waker with [`tokio::sync::futures::Notified::enable`] *before* checking,
/// which is what makes this race-free against `hub::SessionHub::process_room_update`'s
/// `notify_waiters` call -- `tokio::sync::Notify::notify_waiters` only wakes futures that have
/// already been polled at least once (registered), so checking the condition first and only then
/// constructing/polling the `Notified` future would have a lost-wakeup window between the check
/// and the registration. Also polls every [`E2E_POLL_INTERVAL`] regardless of that wake, since
/// to-device/device-list changes have no wake hook at all (see [`has_new_data`]).
async fn long_poll<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    e2e: &Arc<dyn E2eStore>,
    user_id: &UserId,
    device_id: Option<&ruma::DeviceId>,
    baseline: &SyncToken,
    timeout: Duration,
) -> Result<(), UserError> {
    let deadline = Instant::now() + timeout;
    loop {
        let waker = hub.waker(user_id).await;
        let notified = waker.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if has_new_data(hub, e2e, user_id, device_id, baseline).await? {
            return Ok(());
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let wait = remaining.min(E2E_POLL_INTERVAL);
        let _ = tokio::time::timeout(wait, notified).await;
        if Instant::now() >= deadline {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_source::test_support::registry;
    use crate::store::DynUserStore;
    use crate::store::tables::TablesUserStore;
    use hs_e2e::store::tables::TablesE2eStore;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::CreateRoomRequest;
    use hs_room::membership::Action;
    use ruma::user_id;
    use std::sync::Arc;

    type TestHub = SessionHub<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>;

    fn hub() -> Arc<TestHub> {
        let rooms = registry("sync.test");
        let store: DynUserStore = Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        Arc::new(SessionHub::new(store, rooms, 500))
    }

    /// A throwaway `hs-e2e` store for tests that don't otherwise care about it (most of this
    /// module's tests): every `build` call needs one, but only the tests under "e2ee sync
    /// extensions" below actually populate it with anything.
    fn e2e_store() -> Arc<dyn E2eStore> {
        Arc::new(TablesE2eStore::open(MemoryBackend::new()).unwrap())
    }

    fn params(since: Option<SyncToken>) -> SyncParams {
        SyncParams {
            since,
            full_state: false,
            timeout: Duration::from_millis(50),
            filter: SyncFilter::none(),
            device_id: None,
        }
    }

    #[tokio::test]
    async fn initial_sync_lists_a_joined_room_with_its_recent_timeline() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("Room".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hello"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        assert!(token.feed_seq > 0);
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let room = &response["rooms"]["join"][room_id.as_str()];
        assert!(
            room.is_object(),
            "room should appear in initial sync: {response}"
        );
        let events = room["timeline"]["events"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["type"] == "m.room.message"),
            "the message should be in the initial timeline: {events:?}"
        );
        assert!(!room["state"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_message_sent_after_a_token_was_issued_appears_in_the_next_incremental_sync() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(20)).await;

        let (_first, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        hub.store()
            .record_device_cursor(&alice, "DEV1".into(), token.feed_seq)
            .await
            .unwrap();

        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "second"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let events = response["rooms"]["join"][room_id.as_str()]["timeline"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            events.iter().any(|e| e["content"]["body"] == "second"),
            "incremental sync should carry the new message: {response}"
        );
    }

    /// The case this track's brief singles out as the one most implementations get subtly
    /// wrong: a token issued *before* a message must still return that message when presented,
    /// even though other activity (another sync by the same user, advancing state) happened in
    /// between.
    #[tokio::test]
    async fn a_token_from_before_a_message_still_returns_that_message() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(20)).await;

        // The token under test, issued before the message exists.
        let (_early, early_token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "you should still see me"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Present the *early* token, not the latest one.
        let (response, _) = build(&hub, &e2e, &alice, params(Some(early_token)))
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let events = response["rooms"]["join"][room_id.as_str()]["timeline"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            events
                .iter()
                .any(|e| e["content"]["body"] == "you should still see me"),
            "a token from before the message must still surface it: {response}"
        );
    }

    #[tokio::test]
    async fn an_invite_appears_in_the_invitees_incremental_sync() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        let (_bob_first, bob_token) = build(&hub, &e2e, &bob, params(None)).await.unwrap();

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
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &bob, params(Some(bob_token)))
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        assert!(
            response["rooms"]["invite"].get(room_id.as_str()).is_some(),
            "bob's invite should show up: {response}"
        );
    }

    #[tokio::test]
    async fn incremental_sync_with_nothing_new_returns_no_rooms() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (_first, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

        let (response, next) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        assert_eq!(response["rooms"].as_object().unwrap().len(), 0);
        assert_eq!(next.feed_seq, token.feed_seq);
    }

    #[tokio::test]
    async fn filter_rooms_allowlist_excludes_other_rooms() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle_a = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle_a.clone()).await;
        let handle_b = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 2)
            .await
            .unwrap();
        hub.watch_room(handle_b.clone()).await;

        // A room the hub has never seen an update for has no membership record, and a sync
        // reports only rooms it has records for (`crate::hub`'s module docs, "The discovery
        // gap": the create-time publishes happen before anything is subscribed). Send one event
        // in each room after watching, so both are discovered and the filter has two rooms to
        // choose between rather than none.
        for (handle, ts) in [(&handle_a, 3), (&handle_b, 4)] {
            handle
                .send_event(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"body": "hello"}),
                    None,
                    ts,
                )
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(30)).await;

        let room_a = handle_a.query(|a| a.room_id().to_owned()).await;
        let filter: SyncFilter = serde_json::from_value(serde_json::json!({
            "room": {"rooms": [room_a.to_string()]}
        }))
        .unwrap();
        let mut p = params(None);
        p.filter = filter;
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        let join = response["rooms"]["join"].as_object().unwrap();
        assert!(join.contains_key(room_a.as_str()));
        let room_b = handle_b.query(|a| a.room_id().to_owned()).await;
        assert!(!join.contains_key(room_b.as_str()));
    }

    /// `room.timeline.types` on a *fresh* room (an initial sync, exercising
    /// `build_fresh_timeline`'s filtered branch): a non-matching event is excluded from the
    /// timeline while a matching one still appears.
    #[tokio::test]
    async fn initial_sync_timeline_type_filter_excludes_non_matching_events() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "a message"}),
                None,
                2,
            )
            .await
            .unwrap();
        handle
            .send_event(
                alice.clone(),
                "m.reaction".to_owned(),
                None,
                serde_json::json!({"key": "x"}),
                None,
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut p = params(None);
        p.filter = serde_json::from_value(serde_json::json!({
            "room": {"timeline": {"types": ["m.room.message"]}}
        }))
        .unwrap();
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let events = response["rooms"]["join"][room_id.as_str()]["timeline"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            events.iter().any(|e| e["type"] == "m.room.message"),
            "the matching event must still appear: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e["type"] == "m.reaction"),
            "m.reaction should have been filtered out of the timeline: {events:?}"
        );
    }

    /// The same filter on an *incremental* sync (`build_incremental_timeline`'s filtered branch).
    #[tokio::test]
    async fn incremental_sync_timeline_type_filter_excludes_non_matching_events() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "seed"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_first, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        // Without a recorded device cursor, the two sends below coalesce into the room's
        // still-unconsumed feed entry from the seed message (`crate::store`'s own module docs,
        // "coalescing") instead of creating a fresh entry past `token.feed_seq` -- same reason
        // `a_message_sent_after_a_token_was_issued_appears_in_the_next_incremental_sync` (above)
        // does this.
        hub.store()
            .record_device_cursor(&alice, "DEV1".into(), token.feed_seq)
            .await
            .unwrap();

        handle
            .send_event(
                alice.clone(),
                "m.reaction".to_owned(),
                None,
                serde_json::json!({"key": "x"}),
                None,
                3,
            )
            .await
            .unwrap();
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "second"}),
                None,
                4,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut p = params(Some(token));
        p.filter = serde_json::from_value(serde_json::json!({
            "room": {"timeline": {"types": ["m.room.message"]}}
        }))
        .unwrap();
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let events = response["rooms"]["join"][room_id.as_str()]["timeline"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            events.iter().any(|e| e["content"]["body"] == "second"),
            "the matching event must still appear: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e["type"] == "m.reaction"),
            "m.reaction should have been filtered out of the timeline: {events:?}"
        );
    }

    /// `room.state.not_types` excludes a matching state event from the `state` section while
    /// leaving other state (`m.room.create`) present.
    #[tokio::test]
    async fn state_not_types_filter_excludes_a_matching_state_event() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        handle
            .send_event(
                alice.clone(),
                "m.room.topic".to_owned(),
                Some(String::new()),
                serde_json::json!({"topic": "hello"}),
                None,
                2,
            )
            .await
            .unwrap();
        // Enough said since that the room's state is no longer in its timeline: `state` leaves
        // out what the timeline already carries, and it is `state` this filter is about.
        for n in 0..DEFAULT_TIMELINE_LIMIT as i64 {
            say(&handle, &alice, "filler", 3 + n).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut p = params(None);
        p.filter = serde_json::from_value(serde_json::json!({
            "room": {"state": {"not_types": ["m.room.topic"]}}
        }))
        .unwrap();
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let state = response["rooms"]["join"][room_id.as_str()]["state"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            !state.iter().any(|e| e["type"] == "m.room.topic"),
            "m.room.topic should have been filtered out of state: {state:?}"
        );
        assert!(
            state.iter().any(|e| e["type"] == "m.room.create"),
            "m.room.create should still be present: {state:?}"
        );
    }

    /// `docs/rfcs/0013-e2ee-sync-extensions.md`'s acceptance test, in miniature: a to-device
    /// message must survive a retried sync that presents the *same* `since` token (the response
    /// carrying it may never have reached the client), and must be gone once the client presents
    /// the *next* token -- proof that the response was received. See the module docs for why
    /// deletion is keyed off the token a request *presents*, not eagerly right after a response
    /// carrying the message is built.
    #[tokio::test]
    async fn to_device_message_is_redelivered_on_the_same_token_and_gone_after_the_next() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let alice_device: ruma::OwnedDeviceId = "ALICEDEV".into();

        let mut baseline_params = params(None);
        baseline_params.device_id = Some(alice_device.clone());
        let (_baseline, baseline_token) = build(&hub, &e2e, &alice, baseline_params).await.unwrap();

        ToDeviceStore::send_to_device(
            &*e2e,
            &bob,
            &alice,
            &alice_device,
            "m.room_key",
            serde_json::json!({"session_id": "s1"}),
        )
        .await
        .unwrap();

        let mut p = params(Some(baseline_token));
        p.device_id = Some(alice_device.clone());

        let (first, next_token) = build(&hub, &e2e, &alice, p.clone()).await.unwrap();
        let events = first["to_device"]["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "the message should be delivered: {first}");
        assert_eq!(events[0]["sender"], bob.to_string());
        assert_eq!(events[0]["type"], "m.room_key");

        // Same token again: the message must still be there.
        let (retry, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        assert_eq!(
            retry["to_device"]["events"].as_array().unwrap().len(),
            1,
            "a retried sync with the same since token must redeliver the to-device message: \
             {retry}"
        );

        // The next token proves receipt: the message must now be gone, here and later.
        let mut p2 = params(Some(next_token));
        p2.device_id = Some(alice_device.clone());
        let (after, after_token) = build(&hub, &e2e, &alice, p2).await.unwrap();
        assert!(
            after["to_device"]["events"].as_array().unwrap().is_empty(),
            "presenting the next token should not redeliver the message: {after}"
        );

        let mut p3 = params(Some(after_token));
        p3.device_id = Some(alice_device);
        let (again, _) = build(&hub, &e2e, &alice, p3).await.unwrap();
        assert!(
            again["to_device"]["events"].as_array().unwrap().is_empty(),
            "the message must not resurface on a later sync either: {again}"
        );
    }

    /// `device_one_time_keys_count`/`device_unused_fallback_key_types` must be populated on every
    /// response once a device is known -- including the initial sync -- per
    /// `docs/rfcs/0013-e2ee-sync-extensions.md`: a client treats an *absent*
    /// `device_one_time_keys_count` as "the server has zero keys", which is what drove the
    /// unbounded key-reupload behavior that RFC documents.
    #[tokio::test]
    async fn one_time_key_and_fallback_counts_are_populated_on_the_initial_sync() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let device: ruma::OwnedDeviceId = "ALICEDEV".into();

        OneTimeKeyStore::upload_one_time_keys(
            &*e2e,
            &alice,
            &device,
            std::collections::BTreeMap::from([(
                "signed_curve25519:AAAA".to_owned(),
                serde_json::json!({"key": "x"}),
            )]),
        )
        .await
        .unwrap();

        let mut p = params(None);
        p.device_id = Some(device);
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        assert_eq!(
            response["device_one_time_keys_count"]["signed_curve25519"], 1,
            "one-time-key counts must be present even on an initial sync: {response}"
        );
        assert_eq!(
            response["device_unused_fallback_key_types"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    /// The privacy property this track's brief calls out by name: `device_lists.changed` must
    /// only ever name a user the syncing user actually shares a room with, never every user whose
    /// device list happens to have changed on the server.
    #[tokio::test]
    async fn device_lists_changed_is_scoped_to_users_who_share_a_room() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let carol = user_id!("@carol:sync.test").to_owned();

        let handle = hub
            .rooms()
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
        handle
            .membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (_baseline, baseline_token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

        // Both bob (shares the room above with alice) and carol (shares nothing with alice) have
        // a device-list change recorded.
        DeviceKeyStore::record_device_list_change(&*e2e, &bob)
            .await
            .unwrap();
        DeviceKeyStore::record_device_list_change(&*e2e, &carol)
            .await
            .unwrap();

        let (response, _) = build(&hub, &e2e, &alice, params(Some(baseline_token)))
            .await
            .unwrap();
        let changed: Vec<&str> = response["device_lists"]["changed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            changed.contains(&bob.as_str()),
            "bob shares a room with alice and must be reported: {response}"
        );
        assert!(
            !changed.contains(&carol.as_str()),
            "carol shares no room with alice and must not be reported: {response}"
        );
    }

    /// `device_lists.left`: when a user the syncing user shared a room with leaves it (and shares
    /// no other room with them), the next incremental sync should report them as left. Built
    /// entirely from this crate's own room/membership data (`hs-e2e` has no notion of room
    /// membership at all) -- see the module docs.
    #[tokio::test]
    async fn device_lists_left_reports_a_user_who_left_the_only_shared_room() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();

        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (_baseline, baseline_token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        // Marks `baseline_token.feed_seq` consumed so the leave below lands in a *new* feed
        // entry rather than coalescing into the still-unconsumed one from bob's join
        // (`crate::store::tables`'s module docs) -- mirrors
        // `a_message_sent_after_a_token_was_issued_appears_in_the_next_incremental_sync` above.
        hub.store()
            .record_device_cursor(&alice, "DEV1".into(), baseline_token.feed_seq)
            .await
            .unwrap();

        handle
            .membership(
                bob.clone(),
                Action::Leave,
                bob.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &alice, params(Some(baseline_token)))
            .await
            .unwrap();
        let left: Vec<&str> = response["device_lists"]["left"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            left.contains(&bob.as_str()),
            "bob left the only room he shared with alice and should be reported: {response}"
        );
    }

    /// A read receipt appears in the *next* sync's `ephemeral.events` as `m.receipt`, wakes a
    /// blocked long poll immediately (`SessionHub::set_receipt`, same wake shape as typing), and
    /// does not resurface once already delivered with nothing further changed.
    #[tokio::test]
    async fn a_receipt_wakes_a_long_poll_and_appears_as_m_receipt() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();

        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "seed"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (_baseline, baseline_token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;

        let hub2 = hub.clone();
        let room_id2 = room_id.clone();
        let alice2 = alice.clone();
        let setter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            hub2.set_receipt(
                &room_id2,
                &alice2,
                crate::receipts::ReceiptKind::Read,
                ruma::event_id!("$one").to_owned(),
                123,
            )
            .await
            .unwrap();
        });

        let mut p = params(Some(baseline_token));
        p.timeout = Duration::from_secs(5);
        let started = std::time::Instant::now();
        let (response, next_token) = build(&hub, &e2e, &alice, p).await.unwrap();
        setter.await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the long poll should have been woken well before its 5s timeout, took {:?}",
            started.elapsed()
        );
        let ephemeral = response["rooms"]["join"][room_id.as_str()]["ephemeral"]["events"]
            .as_array()
            .unwrap_or_else(|| panic!("room should be present with ephemeral events: {response}"));
        assert!(
            ephemeral.iter().any(|e| e["type"] == "m.receipt"
                && e["content"]["$one"]["m.read"]["@alice:sync.test"]["ts"] == 123),
            "expected an m.receipt event naming alice's read receipt on $one: {ephemeral:?}"
        );
        assert!(next_token.receipts_seq > 0);

        // A second sync from the new token, with nothing further changed, must not repeat it.
        let (again, _) = build(&hub, &e2e, &alice, params(Some(next_token)))
            .await
            .unwrap();
        let room_again = &again["rooms"]["join"][room_id.as_str()];
        assert!(
            room_again.is_null(),
            "an already-delivered receipt must not resurface with nothing else changed: {again}"
        );
    }

    /// A typing change appears in the *next* sync's `ephemeral.events` for a joined room, without
    /// waiting for the long-poll timeout -- `crate::hub::SessionHub::set_typing` wakes the waker
    /// directly, unlike to-device/device-list activity.
    #[tokio::test]
    async fn a_typing_change_wakes_a_long_poll_and_appears_in_ephemeral_events() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();

        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        // The room-creation events were published before `watch_room` subscribed, so this hub
        // never saw them (`crate::hub::SessionHub`'s module docs, "The discovery gap"); a
        // follow-up event is what actually backfills alice's own `join` membership into the
        // store that `list_memberships` (which `has_new_data`'s per-room typing check walks)
        // reads from -- same pattern `crate::hub::tests` uses for the identical reason.
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "seed"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (_baseline, baseline_token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;

        // Set typing concurrently with a long-poll blocked well past that point -- proves the
        // wake is real (`SessionHub::set_typing` calling `notify_waiters` directly), not merely
        // that a *subsequent* poll would eventually notice it via `E2E_POLL_INTERVAL`.
        let hub2 = hub.clone();
        let room_id2 = room_id.clone();
        let alice2 = alice.clone();
        let setter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            hub2.set_typing(&room_id2, &alice2, true, Duration::from_secs(30))
                .await
                .unwrap();
        });

        let mut p = params(Some(baseline_token));
        p.timeout = Duration::from_secs(5);
        let started = std::time::Instant::now();
        let (response, next_token) = build(&hub, &e2e, &alice, p).await.unwrap();
        setter.await.unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the long poll should have been woken well before its 5s timeout, took {:?}",
            started.elapsed()
        );
        let ephemeral = response["rooms"]["join"][room_id.as_str()]["ephemeral"]["events"]
            .as_array()
            .unwrap_or_else(|| panic!("room should be present with ephemeral events: {response}"));
        assert!(
            ephemeral.iter().any(|e| e["type"] == "m.typing"
                && e["content"]["user_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|u| u == alice.as_str())),
            "expected an m.typing event naming alice: {ephemeral:?}"
        );
        assert!(next_token.typing_seq > 0);

        // A second sync from the new token, with nothing further changed, must not repeat it.
        let (again, _) = build(&hub, &e2e, &alice, params(Some(next_token)))
            .await
            .unwrap();
        let room_again = &again["rooms"]["join"][room_id.as_str()];
        assert!(
            room_again.is_null(),
            "an already-delivered typing state must not resurface with nothing else changed: \
             {again}"
        );
    }

    /// `m.presence` at the top level: scoped to users sharing a joined room, gated on
    /// `presence_seq`, and woken immediately (`SessionHub::set_presence`).
    #[tokio::test]
    async fn a_presence_change_appears_for_a_shared_room_member_only() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let carol = user_id!("@carol:sync.test").to_owned();

        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (_baseline, bob_token) = build(&hub, &e2e, &bob, params(None)).await.unwrap();

        hub.set_presence(&alice, "online".to_owned(), Some("hi".to_owned()))
            .await
            .unwrap();
        // Carol shares no room with anyone here; her presence must never reach bob.
        hub.set_presence(&carol, "online".to_owned(), None)
            .await
            .unwrap();

        let (response, _) = build(&hub, &e2e, &bob, params(Some(bob_token)))
            .await
            .unwrap();
        let events = response["presence"]["events"].as_array().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["sender"] == alice.as_str() && e["content"]["presence"] == "online"),
            "bob shares a room with alice and should see her presence: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e["sender"] == carol.as_str()),
            "bob shares no room with carol and must not see her presence: {events:?}"
        );
    }

    /// `docs/status/10-push.md`'s "Interfaces provided": an initial sync always carries
    /// `m.push_rules` once a store is installed, and a never-customized user's `push_rules_seq`
    /// stays `0` -- see `crate::token`'s doc comment on why `0` means "never changed".
    #[tokio::test]
    async fn push_rules_are_carried_on_an_initial_sync_once_a_store_is_installed() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let rulesets = Arc::new(hs_push::rulesets::CachedRulesetStore::new(
            hs_push::rulesets::tables::TablesRulesetStore::open(MemoryBackend::new()).unwrap(),
        ));
        hub.install_push_rules_store(rulesets);

        let (response, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let events = response["account_data"]["events"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["type"] == "m.push_rules"),
            "an initial sync should carry m.push_rules once a store is installed: {events:?}"
        );
        assert_eq!(
            token.push_rules_seq, 0,
            "a never-customized user's change-seq stays 0"
        );
    }

    /// The change-seq gate: an incremental sync omits `m.push_rules` while the baseline is
    /// current, and includes it again the moment the ruleset actually changes.
    #[tokio::test]
    async fn push_rules_only_repeat_on_an_incremental_sync_once_the_ruleset_changes() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let rulesets = Arc::new(hs_push::rulesets::CachedRulesetStore::new(
            hs_push::rulesets::tables::TablesRulesetStore::open(MemoryBackend::new()).unwrap(),
        ));
        hub.install_push_rules_store(rulesets.clone());

        let (_first, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let (second, token2) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        let events = second["account_data"]["events"].as_array().unwrap();
        assert!(
            !events.iter().any(|e| e["type"] == "m.push_rules"),
            "an unchanged ruleset must not repeat on an incremental sync: {events:?}"
        );

        let mut edited = hs_push::rulesets::default_ruleset(&alice);
        edited
            .set_enabled(ruma::push::RuleKind::Underride, ".m.rule.message", false)
            .unwrap();
        rulesets.set_ruleset(&alice, &edited).await.unwrap();

        let (third, token3) = build(&hub, &e2e, &alice, params(Some(token2)))
            .await
            .unwrap();
        let events = third["account_data"]["events"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["type"] == "m.push_rules"),
            "a changed ruleset should reappear on the next incremental sync: {events:?}"
        );
        assert!(token3.push_rules_seq > 0);
    }

    /// `docs/status/10-push.md`'s other half of the seam: `unread_notifications` reports whatever
    /// `CountsStore::get_room_counts` returns, verbatim, once a store is installed -- no
    /// independent computation on this crate's side.
    #[tokio::test]
    async fn unread_notifications_reflect_an_installed_counts_store() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
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
        // The room-creation events were published before `watch_room` subscribed
        // (`crate::hub::SessionHub`'s module docs, "The discovery gap"), so a follow-up event is
        // what actually backfills alice's own `join` membership into this hub -- same pattern
        // `filter_rooms_allowlist_excludes_other_rooms` (above) documents and relies on.
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "seed"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let room_id = handle.query(|a| a.room_id().to_owned()).await;

        let counts: Arc<dyn hs_push::counts::CountsStore> = Arc::new(
            hs_push::counts::tables::TablesCountsStore::open(MemoryBackend::new()).unwrap(),
        );
        counts
            .record_notification(&alice, &room_id, hs_push::counts::Scope::Main, true)
            .await
            .unwrap();
        hub.install_counts_store(counts);

        let (response, _) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let room = &response["rooms"]["join"][room_id.as_str()];
        assert_eq!(room["unread_notifications"]["notification_count"], 1);
        assert_eq!(room["unread_notifications"]["highlight_count"], 1);
    }

    /// The motivating case named in this track's status file: a room with no `m.room.name`
    /// carries heroes (excluding the syncing user) and accurate join/invite counts.
    #[tokio::test]
    async fn room_summary_reports_heroes_and_counts_for_an_unnamed_room() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let handle = hub
            .rooms()
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
                alice.clone(),
                Action::Invite,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let summary = &response["rooms"]["join"][room_id.as_str()]["summary"];
        assert_eq!(summary["m.joined_member_count"], 1);
        assert_eq!(summary["m.invited_member_count"], 1);
        let heroes = summary["m.heroes"].as_array().unwrap();
        assert!(
            heroes.iter().any(|h| h == bob.as_str()),
            "bob should be a hero candidate: {heroes:?}"
        );
        assert!(
            !heroes.iter().any(|h| h == alice.as_str()),
            "heroes must exclude the syncing user: {heroes:?}"
        );
    }

    /// A room with its own name needs no heroes -- see [`build_room_summary`]'s doc comment.
    #[tokio::test]
    async fn room_summary_has_no_heroes_once_the_room_has_its_own_name() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("Room".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        // See `unread_notifications_reflect_an_installed_counts_store`'s identical comment: a
        // follow-up event is what backfills alice's own `join` membership into this hub.
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "seed"}),
                None,
                2,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let summary = &response["rooms"]["join"][room_id.as_str()]["summary"];
        assert!(
            summary["m.heroes"].as_array().unwrap().is_empty(),
            "a named room needs no heroes: {summary}"
        );
        assert_eq!(summary["m.joined_member_count"], 1);
    }

    /// The mirror image of the test above: it is the *syncing* user who leaves. Everybody in the
    /// room they walked out of, and share nothing else with, is somebody whose device list they
    /// will no longer hear about -- Complement's "when leaving a room with a local user". Only
    /// other people's departures were considered, so leaving a room never produced a
    /// `device_lists.left` and a client kept trusting a list nobody was updating for it.
    #[tokio::test]
    async fn leaving_a_room_reports_the_people_left_behind_in_it() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let carol = user_id!("@carol:sync.test").to_owned();
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));

        // Two rooms of bob's. Alice is in both; carol is only in the one alice will leave.
        let mut rooms = Vec::new();
        for ts in [1, 10] {
            let handle = hub
                .rooms()
                .create_room(
                    bob.clone(),
                    CreateRoomRequest {
                        preset: Some("public_chat".to_owned()),
                        ..Default::default()
                    },
                    ts,
                )
                .await
                .unwrap();
            handle
                .membership(
                    alice.clone(),
                    Action::Join,
                    alice.clone(),
                    serde_json::json!({}),
                    ts + 1,
                )
                .await
                .unwrap();
            rooms.push(handle);
        }
        rooms[0]
            .membership(
                carol.clone(),
                Action::Join,
                carol.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        hub.store()
            .record_device_cursor(&alice, ruma::device_id!("DEV1"), token.feed_seq)
            .await
            .unwrap();

        rooms[0]
            .membership(
                alice.clone(),
                Action::Leave,
                alice.clone(),
                serde_json::json!({}),
                20,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        let left: Vec<&str> = response["device_lists"]["left"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(left, vec!["@carol:sync.test"], "{response}");
        // Bob is still in a room with alice, so she still hears about his devices.
        assert!(!left.contains(&"@bob:sync.test"));
    }

    // ---------------------------------------------------------------------------------------
    // The long-poll actually waits.
    // ---------------------------------------------------------------------------------------

    /// The invariant, stated once for every stream at the same time: the token a sync hands back
    /// must not itself be news. If [`has_new_data`] says "yes" about the token [`build`] just
    /// returned, the client's next `/sync` returns at once, with nothing in it and the same
    /// token, and so does the one after -- a long-poll turned into a busy loop. That is what
    /// Complement saw on 2026-09-21: 12,040 empty `/sync` responses inside one five-second wait.
    ///
    /// The cause that time was presence. `has_new_data` watches the syncing user's *own* record,
    /// `build` only walked the users they share a room with, and since `GET /sync` started
    /// marking its caller online every user has a record of their own. Whoever's own record was
    /// the newest they could see never got a token that caught up with it.
    async fn assert_settled(hub: &TestHub, e2e: &Arc<dyn E2eStore>, user: &UserId) -> SyncToken {
        let (_, token) = build(hub, e2e, user, params(None)).await.unwrap();
        assert!(
            !has_new_data(hub, e2e, user, None, &token).await.unwrap(),
            "the token an initial sync returned already counts as new data"
        );
        let (response, again) = build(hub, e2e, user, params(Some(token))).await.unwrap();
        assert!(
            !has_new_data(hub, e2e, user, None, &again).await.unwrap(),
            "the token an incremental sync returned already counts as new data: {response}"
        );
        again
    }

    #[tokio::test]
    async fn a_user_with_a_presence_record_and_nobody_to_share_it_with_is_not_news_to_themself() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        // What `GET /sync` does on every poll.
        hub.touch_presence(&alice, "online").await.unwrap();
        assert_settled(&hub, &e2e, &alice).await;
    }

    #[tokio::test]
    async fn the_newest_presence_in_a_room_being_your_own_is_not_news_either() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Bob polls first, then alice: hers is the newer record, which is the ordering that
        // left *her* spinning while bob's token, carried past his own by hers, was fine.
        hub.touch_presence(&bob, "online").await.unwrap();
        hub.touch_presence(&alice, "online").await.unwrap();
        assert_settled(&hub, &e2e, &alice).await;
        assert_settled(&hub, &e2e, &bob).await;
    }

    /// Complement's "Existing members see new members' presence (in incremental sync)", which
    /// fixing the busy loop broke. Bob's presence is stamped *before* alice's token; then he joins
    /// her room. By stamp alone he is older than anything she has yet to see, and she would
    /// never be sent him. It used to work only because alice's token never caught up with
    /// anything at all.
    #[tokio::test]
    async fn somebody_joining_your_room_brings_their_presence_with_them() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let handle = hub
            .rooms()
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

        hub.set_presence(&bob, "online".to_owned(), None)
            .await
            .unwrap();
        // Alice polls after that, as `GET /sync` does, so her own record and her token are both
        // newer than bob's.
        hub.touch_presence(&alice, "online").await.unwrap();
        let (_, before_bob_joins) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

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
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, token) = build(&hub, &e2e, &alice, params(Some(before_bob_joins)))
            .await
            .unwrap();
        let senders: Vec<&str> = response["presence"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["sender"].as_str().unwrap())
            .collect();
        assert!(senders.contains(&"@bob:sync.test"), "{response}");

        // Once. A join is news one time, not on every sync after it.
        let (response, token) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        assert!(
            response["presence"]["events"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{response}"
        );
        assert!(
            !has_new_data(&hub, &e2e, &alice, None, &token)
                .await
                .unwrap()
        );
    }

    /// The same invariant with every in-memory stream moving at once -- typing, receipts (one of
    /// them private), account data, presence on both sides -- for the streams `build` and
    /// `has_new_data` each keep their own idea of.
    #[tokio::test]
    async fn a_busy_room_settles_too() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let handle = hub
            .rooms()
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
        let message = handle
            .send_event(
                bob.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hello"}),
                None,
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        hub.touch_presence(&bob, "online").await.unwrap();
        hub.set_typing(&room_id, &bob, true, Duration::from_secs(30))
            .await
            .unwrap();
        hub.set_receipt(
            &room_id,
            &bob,
            crate::receipts::ReceiptKind::Read,
            message.event_id().to_owned(),
            4,
        )
        .await
        .unwrap();
        hub.set_receipt(
            &room_id,
            &alice,
            crate::receipts::ReceiptKind::ReadPrivate,
            message.event_id().to_owned(),
            5,
        )
        .await
        .unwrap();
        hub.store()
            .put_global_account_data(&alice, "m.direct", serde_json::json!({}))
            .await
            .unwrap();
        hub.touch_presence(&alice, "unavailable").await.unwrap();

        assert_settled(&hub, &e2e, &alice).await;
        assert_settled(&hub, &e2e, &bob).await;
    }

    /// And the behaviour a client sees: with nothing new, a sync with a timeout takes the
    /// timeout. Before the fix this returned in well under a millisecond.
    #[tokio::test]
    async fn an_incremental_sync_with_nothing_new_waits_out_its_timeout() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        hub.touch_presence(&alice, "online").await.unwrap();
        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

        let mut p = params(Some(token));
        p.timeout = Duration::from_millis(400);
        let started = Instant::now();
        let (response, next) = build(&hub, &e2e, &alice, p).await.unwrap();
        let waited = started.elapsed();

        assert!(
            waited >= Duration::from_millis(350),
            "returned after {waited:?} with {response}"
        );
        assert_eq!(
            next.encode(),
            token.encode(),
            "nothing happened, so nothing moved"
        );
    }

    /// A user's own presence is theirs to see: it is how a second device learns what the first
    /// one set. It is delivered once, not on every sync after.
    #[tokio::test]
    async fn your_own_presence_change_reaches_you_once() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

        hub.set_presence(
            &alice,
            "unavailable".to_owned(),
            Some("back soon".to_owned()),
        )
        .await
        .unwrap();

        let (response, token) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        let events = response["presence"]["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "{response}");
        assert_eq!(events[0]["sender"], "@alice:sync.test");
        assert_eq!(events[0]["content"]["presence"], "unavailable");
        assert_eq!(events[0]["content"]["status_msg"], "back soon");

        let (response, _) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        assert!(
            response["presence"]["events"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{response}"
        );
    }

    // ---------------------------------------------------------------------------------------
    // A gap is filled from the new end, not the old one.
    // ---------------------------------------------------------------------------------------

    /// Twenty-five messages arrive between two syncs and the timeline limit is ten. The client
    /// must get the *newest* ten, be told the timeline is `limited`, and be given a `prev_batch`
    /// that pages back into the fifteen it was not sent.
    ///
    /// This server returned the *oldest* ten -- while handing back a token positioned at the end
    /// of the room. The other fifteen were never delivered by any later sync, and `prev_batch`
    /// pointed at history the client already had. Anybody whose phone had been offline for an
    /// hour lost whatever did not fit in the first page. Complement's "sync token points to a
    /// redaction of an unknown event" had been failing on exactly this, under a name that
    /// suggests something else.
    async fn room_with_a_gap(
        filter: serde_json::Value,
    ) -> (
        Arc<TestHub>,
        Arc<dyn E2eStore>,
        OwnedUserId,
        OwnedRoomId,
        SyncToken,
        SyncFilter,
    ) {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        // Followed from before the room exists, as `hs serve` follows every room: a room the hub
        // only starts watching once it is built has no feed history from before the client's
        // token, which is what a room *new to the client* looks like.
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        // What `routes::sync` does after every sync, and these tests, which call `build`
        // directly, otherwise never do: say how far this device has read. Until some device
        // has, new activity is coalesced into the feed entry it has "not consumed yet" rather
        // than given a new one, and an incremental sync sees no change at all.
        hub.store()
            .record_device_cursor(&alice, ruma::device_id!("GAPTEST"), token.feed_seq)
            .await
            .unwrap();

        for n in 1..=25 {
            // Every third event is a reaction, so that a filter for messages has something to
            // leave out.
            let event_type = if n % 3 == 0 {
                "m.reaction"
            } else {
                "m.room.message"
            };
            handle
                .send_event(
                    alice.clone(),
                    event_type.to_owned(),
                    None,
                    serde_json::json!({"body": format!("message {n}")}),
                    None,
                    10 + n,
                )
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let filter: SyncFilter = serde_json::from_value(filter).unwrap();
        (hub, e2e, alice, room_id, token, filter)
    }

    fn bodies(timeline: &Value) -> Vec<String> {
        timeline["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["content"]["body"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn after_a_gap_an_incremental_sync_returns_the_newest_events_and_a_way_back() {
        let (hub, e2e, alice, room_id, token, filter) =
            room_with_a_gap(serde_json::json!({"room": {"timeline": {"limit": 10}}})).await;
        let mut p = params(Some(token));
        p.filter = filter;
        let (response, next) = build(&hub, &e2e, &alice, p).await.unwrap();

        let timeline = &response["rooms"]["join"][room_id.as_str()]["timeline"];
        let expected: Vec<String> = (16..=25).map(|n| format!("message {n}")).collect();
        assert_eq!(bodies(timeline), expected, "the newest ten, oldest first");
        assert_eq!(timeline["limited"], true);

        // `prev_batch` leads back into the gap: the page before it ends with message 15.
        let prev_batch: PaginationToken = timeline["prev_batch"].as_str().unwrap().parse().unwrap();
        let handle = hub.rooms().get_or_load(&room_id).await.unwrap();
        let before: Vec<String> = handle
            .query(move |actor| {
                let (page, _) = actor.paginate(Some(prev_batch), Direction::Backward, 3);
                page.iter()
                    .map(|e| {
                        client_event_json(e)["content"]["body"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned()
                    })
                    .collect()
            })
            .await;
        assert_eq!(before, vec!["message 15", "message 14", "message 13"]);

        // And nothing is left over: the next sync has no more of this room to give.
        let mut p = params(Some(next));
        p.filter = serde_json::from_value(serde_json::json!({"room": {"timeline": {"limit": 10}}}))
            .unwrap();
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        assert!(
            response["rooms"]["join"].get(room_id.as_str()).is_none(),
            "{response}"
        );
    }

    #[tokio::test]
    async fn without_a_gap_an_incremental_sync_is_everything_in_order_and_not_limited() {
        let (hub, e2e, alice, room_id, token, filter) =
            room_with_a_gap(serde_json::json!({"room": {"timeline": {"limit": 25}}})).await;
        let mut p = params(Some(token));
        p.filter = filter;
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();

        let timeline = &response["rooms"]["join"][room_id.as_str()]["timeline"];
        let expected: Vec<String> = (1..=25).map(|n| format!("message {n}")).collect();
        assert_eq!(bodies(timeline), expected);
        assert_eq!(
            timeline["limited"], false,
            "exactly `limit` events is not a gap"
        );
    }

    #[tokio::test]
    async fn a_gap_is_filled_from_the_new_end_under_a_content_filter_too() {
        let (hub, e2e, alice, room_id, token, filter) = room_with_a_gap(serde_json::json!({
            "room": {"timeline": {"limit": 5, "types": ["m.room.message"]}}
        }))
        .await;
        let mut p = params(Some(token));
        p.filter = filter;
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();

        let timeline = &response["rooms"]["join"][room_id.as_str()]["timeline"];
        // Multiples of three are reactions; the newest five *messages* are these.
        let expected: Vec<String> = [19, 20, 22, 23, 25]
            .iter()
            .map(|n| format!("message {n}"))
            .collect();
        assert_eq!(bodies(timeline), expected);
        assert_eq!(timeline["limited"], true);
    }

    /// What `state` is for. On an ordinary incremental sync the timeline *is* the delta, so
    /// `state` has nothing to add; it used to carry the room's entire current state on every
    /// sync with so much as one new message, which in a large room is most of the response.
    #[tokio::test]
    async fn an_ordinary_incremental_sync_does_not_resend_the_rooms_state() {
        let (hub, e2e, alice, room_id, token, _) =
            room_with_a_gap(serde_json::json!({"room": {"timeline": {"limit": 50}}})).await;
        let mut p = params(Some(token));
        p.filter = serde_json::from_value(serde_json::json!({"room": {"timeline": {"limit": 50}}}))
            .unwrap();
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        let room = &response["rooms"]["join"][room_id.as_str()];
        assert_eq!(room["timeline"]["events"].as_array().unwrap().len(), 25);
        assert_eq!(room["timeline"]["limited"], false);
        assert_eq!(
            room["state"]["events"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0),
            0,
            "{}",
            room["state"]
        );
    }

    /// Across a gap it is the opposite: a state change that fell inside the gap is in no
    /// timeline the client will be sent, so `state` is the only way it arrives.
    #[tokio::test]
    async fn a_state_change_inside_a_gap_still_reaches_the_client() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("Before".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

        // Renamed, and then enough chatter to push the rename out of the newest page.
        handle
            .send_event(
                alice.clone(),
                "m.room.name".to_owned(),
                Some(String::new()),
                serde_json::json!({"name": "After"}),
                None,
                2,
            )
            .await
            .unwrap();
        for n in 1..=12 {
            handle
                .send_event(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"body": format!("message {n}")}),
                    None,
                    10 + n,
                )
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut p = params(Some(token));
        p.filter = serde_json::from_value(serde_json::json!({"room": {"timeline": {"limit": 5}}}))
            .unwrap();
        let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
        let room = &response["rooms"]["join"][room_id.as_str()];
        assert_eq!(room["timeline"]["limited"], true);
        assert!(
            !bodies(&room["timeline"]).is_empty()
                && room["timeline"]["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|e| e["type"] == "m.room.message"),
            "the rename is not in the newest page"
        );
        let name = room["state"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "m.room.name")
            .expect("the rename arrives in `state`");
        assert_eq!(name["content"]["name"], "After");
    }

    /// A room that came into being after the client's token is *new to the client*: it needs the
    /// room's state and recent history, exactly as if it had turned up in an initial sync.
    ///
    /// It was instead resumed "incrementally" from the user's own membership position -- a
    /// fallback meant for very large rooms -- which is at or after their own join, so the create
    /// event, the power levels and the user's own join were in no timeline at all. Nobody
    /// noticed, because every incremental sync also carried the room's entire state; when that
    /// stopped, Complement's `TestRoomSummary` could no longer find alice joined to the room she
    /// had just created.
    #[tokio::test]
    async fn a_room_created_after_the_token_arrives_whole() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        // As `hs serve` does it: following every room from before any exists, so that the whole
        // creation burst is seen, rather than `watch_room` on a room that is already built.
        let _following = hub.watch_all(hub.rooms().subscribe_global());
        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();

        let handle = hub
            .rooms()
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("New".to_owned()),
                    invite: vec![bob.clone()],
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, next) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        let room = &response["rooms"]["join"][room_id.as_str()];
        let everything: Vec<&Value> = room["state"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(room["timeline"]["events"].as_array().into_iter().flatten())
            .collect();
        let has = |event_type: &str, state_key: &str| {
            everything
                .iter()
                .any(|e| e["type"] == event_type && e["state_key"] == state_key)
        };
        assert!(has("m.room.create", ""), "{room}");
        assert!(has("m.room.power_levels", ""), "{room}");
        assert!(has("m.room.name", ""), "{room}");
        assert!(
            has("m.room.member", alice.as_str()),
            "alice's own join: {room}"
        );
        assert!(has("m.room.member", bob.as_str()), "bob's invite: {room}");
        assert_eq!(room["summary"]["m.joined_member_count"], 1);
        assert_eq!(room["summary"]["m.invited_member_count"], 1);

        // Once it has arrived it is an ordinary room: nothing more to say about it.
        let (response, _) = build(&hub, &e2e, &alice, params(Some(next))).await.unwrap();
        assert!(
            response["rooms"]["join"].get(room_id.as_str()).is_none(),
            "{response}"
        );
    }

    /// An event that lands while a sync response is on its way out must reach that client on
    /// its next sync.
    ///
    /// Feed entries are coalesced in place until some device's cursor has passed them, and the
    /// cursor used to be recorded by the route *after* the response was built. An event arriving
    /// in that window was folded into the entry the response had already reported as consumed:
    /// the token pointed past it, the entry's stored position moved on to cover it, and the
    /// resume point for the following sync was therefore *after* it. It was in no timeline, ever.
    /// `build` now claims the feed position it is about to report before it reads anything.
    #[tokio::test]
    async fn an_event_that_arrives_while_a_sync_is_in_flight_is_not_lost() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let device = ruma::device_id!("PHONE");
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(30)).await;

        let as_device = |since: Option<SyncToken>| {
            let mut p = params(since);
            p.device_id = Some(device.to_owned());
            p
        };
        let say = |body: &'static str, ts: i64| {
            let (handle, alice) = (handle.clone(), alice.clone());
            async move {
                handle
                    .send_event(
                        alice,
                        "m.room.message".to_owned(),
                        None,
                        serde_json::json!({"body": body}),
                        None,
                        ts,
                    )
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        };

        // `routes::sync`, step for step: build the response, and only then record how far this
        // device has read. The gap between the two is the window.
        let (_, token) = build(&hub, &e2e, &alice, as_device(None)).await.unwrap();
        hub.store()
            .record_device_cursor(&alice, device, token.feed_seq)
            .await
            .unwrap();

        say("first", 10).await;
        let (response, token) = build(&hub, &e2e, &alice, as_device(Some(token)))
            .await
            .unwrap();
        assert_eq!(
            bodies(&response["rooms"]["join"][room_id.as_str()]["timeline"]),
            vec!["first"]
        );
        // The response is built and the cursor is not recorded yet. An event lands.
        say("second", 11).await;
        hub.store()
            .record_device_cursor(&alice, device, token.feed_seq)
            .await
            .unwrap();

        let (response, token) = build(&hub, &e2e, &alice, as_device(Some(token)))
            .await
            .unwrap();
        assert_eq!(
            bodies(&response["rooms"]["join"][room_id.as_str()]["timeline"]),
            vec!["second"],
            "{response}"
        );
        hub.store()
            .record_device_cursor(&alice, device, token.feed_seq)
            .await
            .unwrap();

        // And it stays delivered exactly once as the room carries on.
        say("third", 12).await;
        let (response, _) = build(&hub, &e2e, &alice, as_device(Some(token)))
            .await
            .unwrap();
        assert_eq!(
            bodies(&response["rooms"]["join"][room_id.as_str()]["timeline"]),
            vec!["third"]
        );
    }

    /// The other half of presence on join, Complement's "Newly joined room includes presence in
    /// incremental sync": the joiner is told about the people already there, once.
    #[tokio::test]
    async fn joining_a_room_brings_the_presence_of_the_people_already_in_it() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
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
        // Alice was last seen changing state well before bob's token exists.
        hub.set_presence(&alice, "online".to_owned(), None)
            .await
            .unwrap();
        hub.touch_presence(&bob, "online").await.unwrap();
        let (_, before_joining) = build(&hub, &e2e, &bob, params(None)).await.unwrap();

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
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, token) = build(&hub, &e2e, &bob, params(Some(before_joining)))
            .await
            .unwrap();
        let senders: Vec<&str> = response["presence"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["sender"].as_str().unwrap())
            .collect();
        assert!(senders.contains(&"@alice:sync.test"), "{response}");

        // "There should be no new presence events": it is news once.
        let (response, _) = build(&hub, &e2e, &bob, params(Some(token))).await.unwrap();
        assert!(
            response["presence"]["events"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{response}"
        );
    }

    // ---------------------------------------------------------------------------------------
    // /sync shows a user what they may see, and nothing else.
    // ---------------------------------------------------------------------------------------

    async fn say(
        handle: &hs_room::actor::RoomActorHandle<MemoryBackend>,
        who: &UserId,
        body: &str,
        ts: i64,
    ) {
        handle
            .send_event(
                who.to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": body}),
                None,
                ts,
            )
            .await
            .unwrap();
    }

    fn all_bodies(room: &Value) -> Vec<String> {
        room["timeline"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
            .collect()
    }

    /// Somebody who has left a room is sent what they could see before they went, and nothing
    /// from after: not the messages, and not the state. `/sync` used to build a left room's
    /// timeline and state exactly as it builds a joined one's -- the latest events, the current
    /// state -- so an initial sync with `include_leave` handed a departed (or kicked, or banned)
    /// user everything said since. Complement's `TestArchivedRoomsHistory` is this.
    #[tokio::test]
    async fn a_user_who_left_is_not_sent_what_happened_after_they_went() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("Before".to_owned()),
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
        say(&handle, &alice, "before", 3).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, while_joined) = build(&hub, &e2e, &bob, params(None)).await.unwrap();
        hub.store()
            .record_device_cursor(&bob, ruma::device_id!("BOB"), while_joined.feed_seq)
            .await
            .unwrap();

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
        say(&handle, &alice, "after", 5).await;
        handle
            .send_event(
                alice.clone(),
                "m.room.name".to_owned(),
                Some(String::new()),
                serde_json::json!({"name": "After"}),
                None,
                6,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let include_leave: SyncFilter =
            serde_json::from_value(serde_json::json!({"room": {"include_leave": true}})).unwrap();

        // An initial sync that asks for left rooms.
        let mut p = params(None);
        p.filter = include_leave.clone();
        let (response, _) = build(&hub, &e2e, &bob, p).await.unwrap();
        let room = &response["rooms"]["leave"][room_id.as_str()];
        assert!(room.is_object(), "the left room is listed: {response}");
        let said = all_bodies(room);
        assert!(said.contains(&"before".to_owned()), "{room}");
        assert!(
            !said.contains(&"after".to_owned()),
            "a message from after bob left: {room}"
        );
        let everything = room.to_string();
        assert!(
            !everything.contains("\"After\""),
            "the room's later name: {room}"
        );
        assert!(
            everything.contains("\"Before\""),
            "the name as bob knew it: {room}"
        );

        // And the incremental sync that tells bob he has left.
        let mut p = params(Some(while_joined));
        p.filter = include_leave;
        let (response, _) = build(&hub, &e2e, &bob, p).await.unwrap();
        let room = &response["rooms"]["leave"][room_id.as_str()];
        assert!(room.is_object(), "{response}");
        assert!(!all_bodies(room).contains(&"after".to_owned()), "{room}");
        assert!(!room.to_string().contains("\"After\""), "{room}");
        let leave_is_there = room["timeline"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e["type"] == "m.room.member" && e["content"]["membership"] == "leave");
        assert!(leave_is_there, "bob is told that he left: {room}");
    }

    /// The same rule from the other end: a room whose history is for members, from when they
    /// joined. Somebody joining it is not sent what was said before they arrived.
    #[tokio::test]
    async fn a_new_member_is_not_sent_history_the_room_keeps_for_those_who_were_there() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    initial_state: vec![hs_room::actor::InitialStateEvent {
                        event_type: "m.room.history_visibility".to_owned(),
                        state_key: String::new(),
                        content: serde_json::json!({"history_visibility": "joined"}),
                    }],
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        say(&handle, &alice, "before bob", 2).await;
        handle
            .membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        say(&handle, &alice, "after bob", 4).await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &bob, params(None)).await.unwrap();
        let said = all_bodies(&response["rooms"]["join"][room_id.as_str()]);
        assert!(said.contains(&"after bob".to_owned()), "{response}");
        assert!(!said.contains(&"before bob".to_owned()), "{response}");

        // Alice was there for all of it.
        let (response, _) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        let said = all_bodies(&response["rooms"]["join"][room_id.as_str()]);
        assert!(said.contains(&"before bob".to_owned()) && said.contains(&"after bob".to_owned()));
    }

    // ---------------------------------------------------------------------------------------
    // Coming into a room you already had history with: an invitation accepted, a return.
    // ---------------------------------------------------------------------------------------

    /// Every `(type, state_key)` a joined room's entry gives the client, from either section.
    fn state_known_to_client(room: &Value) -> BTreeSet<(String, String)> {
        ["state", "timeline"]
            .iter()
            .flat_map(|section| {
                room[section]["events"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|e| {
                Some((
                    e.get("type")?.as_str()?.to_owned(),
                    e.get("state_key")?.as_str()?.to_owned(),
                ))
            })
            .collect()
    }

    /// An encrypted private room, the way Element makes one.
    async fn encrypted_private_room(
        hub: &TestHub,
        creator: &UserId,
    ) -> hs_room::actor::RoomActorHandle<MemoryBackend> {
        hub.rooms()
            .create_room(
                creator.to_owned(),
                CreateRoomRequest {
                    preset: Some("private_chat".to_owned()),
                    name: Some("Plans".to_owned()),
                    initial_state: vec![
                        hs_room::actor::InitialStateEvent {
                            event_type: "m.room.encryption".to_owned(),
                            state_key: String::new(),
                            content: serde_json::json!({"algorithm": "m.megolm.v1.aes-sha2"}),
                        },
                        hs_room::actor::InitialStateEvent {
                            event_type: "m.room.history_visibility".to_owned(),
                            state_key: String::new(),
                            content: serde_json::json!({"history_visibility": "invited"}),
                        },
                    ],
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap()
    }

    /// Found with Element, not with a test: Bob accepted an invitation to an encrypted room and
    /// his composer offered to "Send an unencrypted message". The invitation had left history
    /// in his feed, so his join was resumed from it as if he had been following the room all
    /// along, and all he was sent was his own join event: no state, so no `m.room.encryption`.
    #[tokio::test]
    async fn accepting_an_invitation_brings_the_whole_room_not_just_your_own_join() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let device = ruma::device_id!("BOB");
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let (_, token) = build(&hub, &e2e, &bob, params(None)).await.unwrap();

        let handle = encrypted_private_room(&hub, &alice).await;
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        say(&handle, &alice, "before bob was asked", 2).await;
        handle
            .membership(
                alice.clone(),
                Action::Invite,
                bob.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Bob's client sees the invitation, and says so by syncing again from the new token.
        let (response, token) = build(&hub, &e2e, &bob, params(Some(token))).await.unwrap();
        assert!(
            response["rooms"]["invite"].get(room_id.as_str()).is_some(),
            "{response}"
        );
        hub.store()
            .record_device_cursor(&bob, device, token.feed_seq)
            .await
            .unwrap();

        handle
            .membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                4,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, token) = build(&hub, &e2e, &bob, params(Some(token))).await.unwrap();
        let room = &response["rooms"]["join"][room_id.as_str()];
        let known = state_known_to_client(room);
        for event_type in [
            "m.room.create",
            "m.room.encryption",
            "m.room.power_levels",
            "m.room.join_rules",
            "m.room.history_visibility",
            "m.room.name",
        ] {
            assert!(
                known.contains(&(event_type.to_owned(), String::new())),
                "{event_type} never reached bob: {response}"
            );
        }
        assert!(
            known.contains(&("m.room.member".to_owned(), alice.to_string())),
            "{response}"
        );
        // History visibility is `invited`, and this was said before he was.
        assert!(
            !all_bodies(room).contains(&"before bob was asked".to_owned()),
            "{response}"
        );
        // The people he now shares a room with are people whose keys he needs.
        let changed = response["device_lists"]["changed"].as_array().unwrap();
        assert!(
            changed.contains(&Value::String(alice.to_string())),
            "{response}"
        );

        // And it is news once: the room is one he is following now.
        hub.store()
            .record_device_cursor(&bob, device, token.feed_seq)
            .await
            .unwrap();
        say(&handle, &alice, "welcome", 5).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (response, _) = build(&hub, &e2e, &bob, params(Some(token))).await.unwrap();
        let room = &response["rooms"]["join"][room_id.as_str()];
        assert_eq!(
            bodies(&room["timeline"]),
            vec!["welcome".to_owned()],
            "{response}"
        );
        assert_eq!(room["timeline"]["limited"], false, "{response}");
        assert!(
            room["state"]["events"].as_array().unwrap().is_empty(),
            "{response}"
        );
    }

    /// The same mistake from the other direction: somebody who left and has come back has feed
    /// history from before they went. What changed while they were away was never sent to them
    /// (rightly), so resuming from there tells them nothing about the room they have rejoined.
    #[tokio::test]
    async fn coming_back_to_a_room_brings_what_changed_while_you_were_away() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let device = ruma::device_id!("BOB");
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
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
        let join = |ts| {
            handle.membership(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                ts,
            )
        };
        join(2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, token) = build(&hub, &e2e, &bob, params(None)).await.unwrap();
        hub.store()
            .record_device_cursor(&bob, device, token.feed_seq)
            .await
            .unwrap();

        handle
            .membership(
                bob.clone(),
                Action::Leave,
                bob.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (response, token) = build(&hub, &e2e, &bob, params(Some(token))).await.unwrap();
        assert!(
            response["rooms"]["leave"].get(room_id.as_str()).is_some(),
            "{response}"
        );
        hub.store()
            .record_device_cursor(&bob, device, token.feed_seq)
            .await
            .unwrap();

        handle
            .send_event(
                alice.clone(),
                "m.room.name".to_owned(),
                Some(String::new()),
                serde_json::json!({"name": "Renamed while bob was out"}),
                None,
                4,
            )
            .await
            .unwrap();
        join(5).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &bob, params(Some(token))).await.unwrap();
        let room = &response["rooms"]["join"][room_id.as_str()];
        // Wherever it arrives -- a room sent whole carries its state in both sections.
        let names: BTreeSet<&str> = ["state", "timeline"]
            .iter()
            .flat_map(|section| room[section]["events"].as_array().unwrap())
            .filter(|e| e["type"] == "m.room.name")
            .filter_map(|e| e["content"]["name"].as_str())
            .collect();
        assert_eq!(
            names,
            BTreeSet::from(["Renamed while bob was out"]),
            "{response}"
        );
    }

    /// The other side of a join: the people already in the room now share it with the newcomer,
    /// and have to be told so or they never fetch the newcomer's keys. Complement's version of
    /// this passed only because its client had just done an initial sync, whose token used to
    /// carry a device-list position of zero and so re-reported everybody.
    #[tokio::test]
    async fn somebody_joining_your_room_is_somebody_whose_keys_you_now_need() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let device = ruma::device_id!("ALICE");
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        // Bob's keys were uploaded long ago, as far as alice's token will be concerned.
        DeviceKeyStore::record_device_list_change(&*e2e, &bob)
            .await
            .unwrap();
        let handle = hub
            .rooms()
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
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        // Settled: nothing about bob is pending before he arrives.
        let (response, token) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        assert_eq!(
            response["device_lists"]["changed"],
            serde_json::json!([]),
            "{response}"
        );
        hub.store()
            .record_device_cursor(&alice, device, token.feed_seq)
            .await
            .unwrap();

        let join = |content: Value, ts| {
            handle.membership(bob.clone(), Action::Join, bob.clone(), content, ts)
        };
        join(serde_json::json!({}), 2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (response, token) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        assert_eq!(
            response["device_lists"]["changed"],
            serde_json::json!([bob.as_str()]),
            "{response}"
        );

        // A new display name is a `join` event too, and is not an arrival.
        hub.store()
            .record_device_cursor(&alice, device, token.feed_seq)
            .await
            .unwrap();
        join(serde_json::json!({"displayname": "Robert"}), 3)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (response, _) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        assert_eq!(
            response["device_lists"]["changed"],
            serde_json::json!([]),
            "{response}"
        );
    }

    /// A user's own id in `device_lists.changed` is how the device they are signed in on hears
    /// about the one they have just signed in on. It was filtered out with everybody else who
    /// "does not share a room" with them.
    #[tokio::test]
    async fn your_own_new_device_is_news_to_your_other_devices() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let (_, token) = build(&hub, &e2e, &alice, params(None)).await.unwrap();
        DeviceKeyStore::record_device_list_change(&*e2e, &alice)
            .await
            .unwrap();
        let (response, _) = build(&hub, &e2e, &alice, params(Some(token)))
            .await
            .unwrap();
        assert_eq!(
            response["device_lists"]["changed"],
            serde_json::json!([alice.as_str()]),
            "{response}"
        );
    }

    /// Complement caught this one (`TestLeaveEventInviteRejection`, and "Invited user can reject
    /// invite for empty room") the run after `/sync` began applying history visibility: somebody
    /// who declines an invitation was never joined, so by the letter of the visibility rules
    /// they may not see their own leave -- and the invitation never left their client.
    #[tokio::test]
    async fn declining_an_invitation_moves_the_room_to_leave() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
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
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (response, token) = build(&hub, &e2e, &bob, params(None)).await.unwrap();
        assert!(
            response["rooms"]["invite"].get(room_id.as_str()).is_some(),
            "{response}"
        );
        hub.store()
            .record_device_cursor(&bob, ruma::device_id!("BOB"), token.feed_seq)
            .await
            .unwrap();

        handle
            .membership(
                bob.clone(),
                Action::Leave,
                bob.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let (response, _) = build(&hub, &e2e, &bob, params(Some(token))).await.unwrap();
        let events = response["rooms"]["leave"][room_id.as_str()]["timeline"]["events"]
            .as_array()
            .unwrap_or_else(|| panic!("the room never moved to leave: {response}"));
        assert!(
            events.iter().any(|e| e["type"] == "m.room.member"
                && e["state_key"] == bob.as_str()
                && e["content"]["membership"] == "leave"),
            "{response}"
        );
        // And nothing else of a room he was never in.
        assert!(
            events.iter().all(|e| e["state_key"] == bob.as_str()),
            "{response}"
        );
    }

    /// A room sent whole -- an initial sync, a room the client has just come into -- carries its
    /// state in `state` *or* in `timeline`, never the same event in both. It used to send every
    /// state event the timeline held a second time, which Complement's
    /// `TestArchivedRoomsHistory` counts, and which is most of a small room sent twice.
    #[tokio::test]
    async fn a_room_sent_whole_says_each_thing_once() {
        let hub = hub();
        let e2e = e2e_store();
        let alice = user_id!("@alice:sync.test").to_owned();
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut full_state = params(None);
        full_state.full_state = true;
        for p in [params(None), full_state] {
            let (response, _) = build(&hub, &e2e, &alice, p).await.unwrap();
            let room = &response["rooms"]["join"][room_id.as_str()];
            let ids = |section: &str| -> Vec<String> {
                room[section]["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["event_id"].as_str().unwrap().to_owned())
                    .collect()
            };
            let (state, timeline) = (ids("state"), ids("timeline"));
            assert!(!timeline.is_empty(), "{response}");
            assert!(
                state.iter().all(|id| !timeline.contains(id)),
                "said twice: {response}"
            );
            // Nothing went missing in the subtraction.
            let known = state_known_to_client(room);
            assert!(
                known.contains(&("m.room.create".to_owned(), String::new())),
                "{response}"
            );
        }
    }
}
