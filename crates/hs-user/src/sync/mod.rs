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
//! - **`m.receipt`**: read-receipt distribution is listed in this track's brief but was not
//!   reached this session -- `SyncToken::receipts_seq` is reserved for it, unused today.
//! - **Unread notification counts**: *is* included, per this track's own instructions, shaped
//!   correctly (`{"highlight_count": 0, "notification_count": 0}`) with both counts hard-zero
//!   until track 10 (push) lands.
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
use hs_room::routes::render::client_event_json;
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
        return Ok(ResumeMode::Incremental(pos));
    }
    // No feed history at or before the token for this room. This is expected the first time a
    // room is ever synced (brand new to the user), and also -- persistently -- for a room that
    // has been "hot" (`crate::hub`'s module docs) for as long as the user has been a member,
    // since a hot room never gets feed entries at all. `membership.room_pos` (the position of
    // this user's own last membership-changing event, set unconditionally regardless of
    // hot/cold -- see `crate::hub::SessionHub::process_room_update`) is a safe fallback baseline
    // in the hot case: resuming forward from it can only ever *repeat* events the client already
    // received (never skip real ones), which is the correct direction to err in. `0` means no
    // membership event has ever been recorded for this room by this store, which should not
    // happen for a room that made it into the candidate set at all; fall back to the fresh-room
    // treatment (full state plus recent history) rather than resuming from position `0`, which
    // would try to page the room's *entire* history forward.
    if membership.room_pos > 0 {
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

fn build_incremental_timeline(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    resume_pos: i64,
    limit: usize,
) -> Timeline {
    let from = Some(PaginationToken::new(resume_pos, Direction::Forward));
    let (events, next) = actor.paginate(from, Direction::Forward, limit);
    let limited = if events.len() == limit {
        let (more, _) = actor.paginate(next, Direction::Forward, 1);
        !more.is_empty()
    } else {
        false
    };
    let prev_batch = if events.is_empty() {
        None
    } else {
        Some(PaginationToken::new(resume_pos, Direction::Backward).to_string())
    };
    Timeline {
        events: events.into_iter().map(client_event_json).collect(),
        limited,
        prev_batch,
    }
}

fn build_fresh_timeline(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    limit: usize,
) -> Timeline {
    let (events, next) = actor.paginate(None, Direction::Backward, limit);
    let limited = if events.len() == limit {
        let (more, _) = actor.paginate(next, Direction::Backward, 1);
        !more.is_empty()
    } else {
        false
    };
    let prev_batch = next.map(|t| t.to_string());
    let mut ordered: Vec<&Event> = events;
    ordered.reverse(); // paginate(Backward) is newest-first; /sync wants chronological order.
    Timeline {
        events: ordered.into_iter().map(client_event_json).collect(),
        limited,
        prev_batch,
    }
}

/// `m.room.*` types Synapse's default `invite_room_state`/`knock_room_state` sends: enough for a
/// client to render an invite/knock preview without joining.
const STRIPPED_STATE_TYPES: &[&str] = &[
    "m.room.create",
    "m.room.join_rules",
    "m.room.canonical_alias",
    "m.room.name",
    "m.room.avatar",
    "m.room.topic",
    "m.room.encryption",
];

fn stripped_state(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
) -> Result<Vec<Value>, hs_room::RoomError> {
    Ok(actor
        .full_state()?
        .into_iter()
        .filter(|e| STRIPPED_STATE_TYPES.contains(&e.header().event_type.as_str()))
        .map(client_event_json)
        .collect())
}

/// Full current state, minus whatever event ids are already present in `timeline_events` (avoids
/// duplicating a state event this response's timeline already carries -- see the module docs'
/// caveat that this is an approximation of "state at the start of the timeline", not an exact
/// one), optionally lazy-loaded (`m.room.member` restricted to timeline senders plus the
/// requester's own membership, when `lazy` is set).
fn build_state_section(
    actor: &hs_room::actor::RoomActor<impl KvBackend>,
    timeline_event_ids: &HashSet<String>,
    lazy: bool,
    timeline_senders: &HashSet<String>,
    self_user: &UserId,
) -> Result<Vec<Value>, hs_room::RoomError> {
    Ok(actor
        .full_state()?
        .into_iter()
        .filter(|e| !timeline_event_ids.contains(e.event_id().as_str()))
        .filter(|e| {
            if !lazy || e.header().event_type != "m.room.member" {
                return true;
            }
            let is_self = e.header().state_key.as_deref() == Some(self_user.as_str());
            is_self || timeline_senders.contains(e.header().sender.as_str())
        })
        .map(client_event_json)
        .collect())
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
    for m in store.list_memberships(user_id).await? {
        if m.membership != "join" {
            continue;
        }
        let (users, seq) = hub.typing_users(&m.room_id).await;
        new_typing_seq = new_typing_seq.max(seq);
        if seq > baseline.typing_seq {
            candidate_rooms.insert(m.room_id.clone());
            typing_by_room.insert(
                m.room_id,
                vec![json!({
                    "type": "m.typing",
                    "content": {"user_ids": users},
                })],
            );
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

        match membership_value.as_str() {
            "invite" => {
                let events = handle.query(move |actor| stripped_state(actor)).await?;
                invite.insert(
                    room_id_owned.to_string(),
                    json!({"invite_state": {"events": events}}),
                );
                continue;
            }
            "knock" => {
                let events = handle.query(move |actor| stripped_state(actor)).await?;
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

        let (timeline, state_events) = handle
            .query(move |actor| {
                let timeline = match resume {
                    ResumeMode::Incremental(pos) => {
                        build_incremental_timeline(actor, pos, timeline_limit)
                    }
                    ResumeMode::FreshRoom => build_fresh_timeline(actor, timeline_limit),
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
                let state = if force_full_state {
                    build_state_section(
                        actor,
                        &HashSet::new(),
                        lazy,
                        &timeline_senders,
                        &user_id_owned,
                    )?
                } else {
                    build_state_section(
                        actor,
                        &timeline_ids,
                        lazy,
                        &timeline_senders,
                        &user_id_owned,
                    )?
                };
                Ok::<_, hs_room::RoomError>((timeline, state))
            })
            .await?;

        for event in &timeline.events {
            if event.get("type").and_then(Value::as_str) != Some("m.room.member") {
                continue;
            }
            let Some(state_key) = event.get("state_key").and_then(Value::as_str) else {
                continue;
            };
            if state_key == user_id.as_str() {
                continue;
            }
            let membership = event
                .get("content")
                .and_then(|c| c.get("membership"))
                .and_then(Value::as_str);
            if matches!(membership, Some("leave") | Some("ban"))
                && let Ok(other) = ruma::UserId::parse(state_key)
            {
                left_candidates.insert(other);
            }
        }

        // Only a joined room can have typing activity (`typing_by_room` above is only ever
        // populated for `membership == "join"` rows), so a leave/ban room correctly never has an
        // entry here.
        let ephemeral_events = typing_by_room.get(room_id).cloned().unwrap_or_default();

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
                            "highlight_count": 0,
                            "notification_count": 0,
                        },
                        "unread_thread_notifications": {},
                        "summary": {},
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
    let global_account_data_json: Vec<Value> = global_account_data
        .iter()
        .map(|a| json!({"type": a.event_type, "content": a.content}))
        .collect();

    let new_feed_seq = store.latest_feed_seq(user_id).await?.max(baseline.feed_seq);
    let new_account_data_seq = store
        .latest_account_data_seq(user_id)
        .await?
        .max(baseline.account_data_seq);

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
        (None, baseline.device_list_seq)
    } else {
        let upto = DeviceKeyStore::current_stream_pos(&**e2e).await?;
        let changed_all =
            DeviceKeyStore::changed_users_since(&**e2e, baseline.device_list_seq, Some(upto))
                .await?;
        let changed: Vec<Value> = changed_all
            .iter()
            .filter(|u| shared.contains(*u))
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
    for other in &shared {
        let Some(record) = hub.presence_of(other).await else {
            continue;
        };
        new_presence_seq = new_presence_seq.max(record.seq);
        if is_initial || record.seq > baseline.presence_seq {
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
        ..baseline
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
        }
    }
    // Presence: has anyone `user_id` shares a joined room with (or `user_id` itself) posted a
    // presence update since `baseline`? Scoped the same way `device_lists` is (see the module
    // docs) -- an over-broad wake here would just cost an extra response-building pass, same
    // reasoning as the to-device peek below.
    let mut presence_watch: Vec<OwnedUserId> =
        shared_users(hub, user_id).await?.into_iter().collect();
    presence_watch.push(user_id.to_owned());
    for other in &presence_watch {
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
}
