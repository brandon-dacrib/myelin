//! `/sync` v2: full and incremental, long-polling, joined/invited/knocked/left rooms, timeline
//! with `limited`/`prev_batch`, state, account data, ephemeral events (empty -- not implemented,
//! see the module docs) and unread notification counts (zero -- track 10 has not landed).
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
//! # Not implemented in this pass (present as documented gaps, not silent omissions)
//!
//! - **Ephemeral events** (`m.typing`, `m.receipt`): every room's `ephemeral.events` is always
//!   `[]`. Typing and receipt distribution is listed in this track's brief but was not reached
//!   this session -- see `docs/status/05-sync.md`.
//! - **Presence**: the top-level `presence.events` is always `[]`.
//! - **To-device, device lists, one-time-key counts**: track 08 (E2EE) owns these and has not
//!   landed a crate yet (no `hs-e2e` exists in this workspace as of this session). Rather than
//!   fabricate empty placeholders for a wire shape this crate has no way to validate, `build`
//!   omits `to_device`, `device_lists`, `device_one_time_keys_count` and
//!   `device_unused_fallback_key_types` entirely; real Matrix clients treat all four as
//!   optional-and-default-empty when absent. Revisit once track 08's cursors exist --
//!   `docs/status/05-sync.md`'s "Interfaces needed".
//! - **Unread notification counts**: *is* included, per this track's own instructions, shaped
//!   correctly (`{"highlight_count": 0, "notification_count": 0}`) with both counts hard-zero
//!   until track 10 (push) lands.

use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

use hs_kv::KvBackend;
use hs_model::Event;
use hs_room::routes::render::client_event_json;
use hs_room::timeline::{Direction, PaginationToken};
use ruma::{OwnedRoomId, RoomId, UserId};
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
    user_id: &UserId,
    params: SyncParams,
) -> Result<(Value, SyncToken), UserError> {
    let baseline = params.since.unwrap_or_else(SyncToken::initial);
    let is_initial = params.since.is_none();

    if !is_initial {
        long_poll(hub, user_id, &baseline, params.timeout).await?;
    }

    let store = hub.store();
    let timeline_limit = params.filter.timeline_limit(DEFAULT_TIMELINE_LIMIT);

    let candidate_rooms: BTreeSet<OwnedRoomId> = if is_initial {
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

    let mut join = serde_json::Map::new();
    let mut invite = serde_json::Map::new();
    let mut knock = serde_json::Map::new();
    let mut leave = serde_json::Map::new();

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
                    build_state_section(actor, &timeline_ids, lazy, &timeline_senders, &user_id_owned)?
                };
                Ok::<_, hs_room::RoomError>((timeline, state))
            })
            .await?;

        let nothing_changed = timeline.events.is_empty()
            && account_data_json.is_empty()
            && !force_full_state;
        if nothing_changed && !is_initial {
            continue;
        }
        if timeline.events.is_empty() && state_events.is_empty() && account_data_json.is_empty() {
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
                        "ephemeral": {"events": []},
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
    let next_token = SyncToken {
        feed_seq: new_feed_seq,
        account_data_seq: new_account_data_seq,
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

    let response = json!({
        "next_batch": next_token.encode(),
        "rooms": rooms,
        "presence": {"events": []},
        "account_data": {"events": global_account_data_json},
    });

    Ok((response, next_token))
}

/// Whether anything has changed for `user_id` since `baseline` -- feed activity, or the
/// account-data counter having advanced. Used both by the long-poll loop's wake condition and
/// (implicitly, by returning quickly) by a plain non-blocking check.
async fn has_new_data<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    user_id: &UserId,
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
    for m in store.list_memberships(user_id).await? {
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
    }
    Ok(false)
}

/// The long-poll loop: waits until [`has_new_data`] is true or `timeout` elapses. Registers
/// interest on the hub's waker with [`tokio::sync::futures::Notified::enable`] *before* checking,
/// which is what makes this race-free against `hub::SessionHub::process_room_update`'s
/// `notify_waiters` call -- `tokio::sync::Notify::notify_waiters` only wakes futures that have
/// already been polled at least once (registered), so checking the condition first and only then
/// constructing/polling the `Notified` future would have a lost-wakeup window between the check
/// and the registration.
async fn long_poll<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    user_id: &UserId,
    baseline: &SyncToken,
    timeout: Duration,
) -> Result<(), UserError> {
    let deadline = Instant::now() + timeout;
    loop {
        let waker = hub.waker(user_id).await;
        let notified = waker.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if has_new_data(hub, user_id, baseline).await? {
            return Ok(());
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let _ = tokio::time::timeout(remaining, notified).await;
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

    fn params(since: Option<SyncToken>) -> SyncParams {
        SyncParams {
            since,
            full_state: false,
            timeout: Duration::from_millis(50),
            filter: SyncFilter::none(),
        }
    }

    #[tokio::test]
    async fn initial_sync_lists_a_joined_room_with_its_recent_timeline() {
        let hub = hub();
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

        let (response, token) = build(&hub, &alice, params(None)).await.unwrap();
        assert!(token.feed_seq > 0);
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let room = &response["rooms"]["join"][room_id.as_str()];
        assert!(room.is_object(), "room should appear in initial sync: {response}");
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

        let (_first, token) = build(&hub, &alice, params(None)).await.unwrap();
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

        let (response, _) = build(&hub, &alice, params(Some(token))).await.unwrap();
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
        let (_early, early_token) = build(&hub, &alice, params(None)).await.unwrap();

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
        let (response, _) = build(&hub, &alice, params(Some(early_token))).await.unwrap();
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
        let alice = user_id!("@alice:sync.test").to_owned();
        let bob = user_id!("@bob:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        let (_bob_first, bob_token) = build(&hub, &bob, params(None)).await.unwrap();

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

        let (response, _) = build(&hub, &bob, params(Some(bob_token))).await.unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        assert!(
            response["rooms"]["invite"]
                .get(room_id.as_str())
                .is_some(),
            "bob's invite should show up: {response}"
        );
    }

    #[tokio::test]
    async fn incremental_sync_with_nothing_new_returns_no_rooms() {
        let hub = hub();
        let alice = user_id!("@alice:sync.test").to_owned();
        let handle = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .unwrap();
        hub.watch_room(handle.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (_first, token) = build(&hub, &alice, params(None)).await.unwrap();

        let (response, next) = build(&hub, &alice, params(Some(token))).await.unwrap();
        assert_eq!(response["rooms"].as_object().unwrap().len(), 0);
        assert_eq!(next.feed_seq, token.feed_seq);
    }

    #[tokio::test]
    async fn filter_rooms_allowlist_excludes_other_rooms() {
        let hub = hub();
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
        let (response, _) = build(&hub, &alice, p).await.unwrap();
        let join = response["rooms"]["join"].as_object().unwrap();
        assert!(join.contains_key(room_a.as_str()));
        let room_b = handle_b.query(|a| a.room_id().to_owned()).await;
        assert!(!join.contains_key(room_b.as_str()));
    }
}
