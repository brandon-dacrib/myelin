//! The deprecated event stream, `GET /events`, and `GET /initialSync`: the client-server API
//! before `/sync`, still listed by the spec (deprecated) and still what Sytest's helpers wait on
//! (`await_event_for`, `local_user_fixture(with_events => 1)`).
//!
//! Both are read from the same feed `/sync` reads ([`crate::sync::build`]), so nothing new is
//! stored and nothing can disagree with `/sync`: `/events` is an incremental sync from its
//! `from` token, flattened into one `chunk` of events, each carrying its `room_id` (room events,
//! typing and receipts), plus presence; its `end` is the sync's `next_batch`, which is also what
//! the next `/events` (or `/sync`) can start from. `/initialSync` is an initial sync rearranged
//! into the old shape: a list of `rooms`, each with its `membership`, `messages` and `state`.
//!
//! Neither touches a device's to-device queue or its sync position: both build the sync with no
//! device, so a client (or Sytest) using `/events` beside `/sync` loses nothing from `/sync`.
//!
//! Not done: `GET /events?room_id=` for a room the caller is not in (the old room-preview
//! "peek" at a `world_readable` room) answers an empty `chunk` rather than the room's events;
//! `GET /rooms/{roomId}/initialSync` is `hs-room`'s.

use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::UserError;
use crate::filter::SyncFilter;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};
use crate::sync::{self, SyncParams};
use crate::token::SyncToken;

/// The longest `/events` holds a request open, as `/sync` (`crate::routes::sync`).
const MAX_TIMEOUT: Duration = Duration::from_secs(60);

/// How many of a room's new events one `/events` response carries at most. A client that falls
/// further behind than this gets the newest ones, as `/sync`'s `limited` timeline would give it.
const EVENTS_PER_ROOM: usize = 100;

/// Query parameters for `GET /events`. Numbers are read leniently as strings so that a bad one
/// is a `400 M_INVALID_PARAM` naming it, not a bare query-string rejection.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    /// Where to read from: an `end` (or `/sync` `next_batch`) this server issued. Absent, or a
    /// token this server does not recognise as one of its stream tokens, starts from now.
    pub from: Option<String>,
    /// Milliseconds to wait for something to happen; default 0, at most 60 seconds.
    pub timeout: Option<String>,
    /// Only this room's events.
    pub room_id: Option<String>,
}

/// Query parameters for `GET /initialSync`.
#[derive(Debug, Default, Deserialize)]
pub struct InitialSyncQuery {
    /// How many of each room's newest events `messages` carries; default 10.
    pub limit: Option<String>,
    /// Include rooms the user has left.
    pub archived: Option<String>,
}

fn parse_number(name: &str, raw: Option<&str>) -> Result<Option<u64>, UserError> {
    raw.map(|raw| {
        raw.parse::<u64>()
            .map_err(|_| UserError::InvalidParam(format!("{name} must be a whole number")))
    })
    .transpose()
}

fn filter_from(value: Value) -> Result<SyncFilter, UserError> {
    serde_json::from_value(value).map_err(|e| UserError::InvalidFilter(e.to_string()))
}

/// `value` as a JSON object's events array at `path`, or nothing.
fn events_at<'a>(value: &'a Value, path: &[&str]) -> impl Iterator<Item = &'a Value> {
    let mut at = Some(value);
    for key in path {
        at = at.and_then(|v| v.get(*key));
    }
    at.and_then(Value::as_array).into_iter().flatten()
}

fn with_room_id(event: &Value, room_id: &str) -> Value {
    let mut event = event.clone();
    if let Some(object) = event.as_object_mut() {
        object.insert("room_id".to_owned(), Value::String(room_id.to_owned()));
    }
    event
}

/// A `/sync` presence event in the old shape, which also named the user inside `content`.
fn legacy_presence(event: &Value) -> Value {
    let mut event = event.clone();
    if let Some(sender) = event.get("sender").cloned()
        && let Some(content) = event.get_mut("content").and_then(Value::as_object_mut)
    {
        content.entry("user_id").or_insert(sender);
    }
    event
}

fn rooms_of<'a>(sync: &'a Value, section: &str) -> impl Iterator<Item = (&'a String, &'a Value)> {
    sync.get("rooms")
        .and_then(|r| r.get(section))
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(Map::iter)
}

/// Flattens an incremental `/sync` response into `/events`' `chunk`: every room's new timeline
/// events, typing and receipts, each with its `room_id`; the caller's own invites; and presence.
#[must_use]
pub fn events_chunk(sync: &Value, user_id: &str) -> Vec<Value> {
    let mut chunk = Vec::new();
    for (room_id, room) in rooms_of(sync, "join") {
        chunk.extend(events_at(room, &["timeline", "events"]).map(|e| with_room_id(e, room_id)));
        chunk.extend(events_at(room, &["ephemeral", "events"]).map(|e| with_room_id(e, room_id)));
    }
    for (room_id, room) in rooms_of(sync, "invite") {
        chunk.extend(
            events_at(room, &["invite_state", "events"])
                .filter(|e| {
                    e.get("type").and_then(Value::as_str) == Some("m.room.member")
                        && e.get("state_key").and_then(Value::as_str) == Some(user_id)
                })
                .map(|e| with_room_id(e, room_id)),
        );
    }
    for (room_id, room) in rooms_of(sync, "leave") {
        chunk.extend(events_at(room, &["timeline", "events"]).map(|e| with_room_id(e, room_id)));
    }
    chunk.extend(events_at(sync, &["presence", "events"]).map(legacy_presence));
    chunk
}

/// The key the legacy stream records its feed cursor under (`crate::sync::cursor_device_id`'s
/// reasoning: a cursor is what stops the feed coalescing an entry somebody has been handed, and
/// only the maximum per user is read), so a client that only ever reads `/events` still sees
/// each new event.
const EVENTS_CURSOR_KEY: &str = "\u{1}hs-user:legacy-events";

async fn record_cursor<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    state: &UserState<B, R>,
    user_id: &ruma::UserId,
    token: &SyncToken,
) -> Result<(), UserError> {
    state
        .hub
        .store()
        .record_device_cursor(user_id, EVENTS_CURSOR_KEY.into(), token.feed_seq)
        .await?;
    Ok(())
}

/// The current position of `user_id`'s stream, without reading any room: an initial sync that
/// asks for no rooms.
async fn current_token<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    state: &UserState<B, R>,
    user_id: &ruma::UserId,
) -> Result<SyncToken, UserError> {
    let (_, token) = sync::build(
        &state.hub,
        &state.e2e,
        user_id,
        SyncParams {
            since: None,
            full_state: false,
            timeout: Duration::ZERO,
            filter: filter_from(json!({"room": {"rooms": []}}))?,
            device_id: None,
        },
    )
    .await?;
    record_cursor(state, user_id, &token).await?;
    Ok(token)
}

/// `GET /events` (deprecated): the events since `from`, waiting up to `timeout` milliseconds for
/// one, as `{chunk, start, end}`. See the module docs.
///
/// # Errors
/// [`UserError::InvalidParam`] for a `timeout` that is not a number; a store or room failure.
pub async fn get_events<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Query(query): Query<EventsQuery>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    let timeout = parse_number("timeout", query.timeout.as_deref())?
        .map(Duration::from_millis)
        .unwrap_or_default()
        .min(MAX_TIMEOUT);
    // Polling the event stream is a client being there, as polling `/sync` is: the caller is
    // marked online (Synapse's `user_syncing`). Sytest's federation presence tests
    // (`flush_events_for`) mark their users online this way.
    state
        .hub
        .touch_presence(&requester.user_id, "online")
        .await?;
    let since = match query.from.as_deref().map(SyncToken::decode) {
        Some(Ok(token)) => token,
        // Not one of this server's stream tokens (an `/initialSync` room's `messages.end`, which
        // is a room pagination token here): start from now, as an absent `from` does.
        Some(Err(_)) | None => current_token(&state, &requester.user_id).await?,
    };
    let room = match &query.room_id {
        Some(room_id) => json!({"rooms": [room_id], "timeline": {"limit": EVENTS_PER_ROOM}}),
        None => json!({"timeline": {"limit": EVENTS_PER_ROOM}}),
    };
    let (response, next) = sync::build(
        &state.hub,
        &state.e2e,
        &requester.user_id,
        SyncParams {
            since: Some(since),
            full_state: false,
            timeout,
            filter: filter_from(json!({"room": room}))?,
            device_id: None,
        },
    )
    .await?;
    record_cursor(&state, &requester.user_id, &next).await?;
    let chunk = events_chunk(&response, requester.user_id.as_str());
    tracing::debug!(user = %requester.user_id, events = chunk.len(), "answered the legacy event stream");
    Ok(Json(json!({
        "chunk": chunk,
        "start": since.to_string(),
        "end": next.to_string(),
    }))
    .into_response())
}

/// One joined, invited or left room of an initial `/sync`, in `/initialSync`'s shape.
fn legacy_room(room_id: &str, membership: &str, room: &Value, end: &str) -> Value {
    let timeline: Vec<Value> = events_at(room, &["timeline", "events"])
        .map(|e| with_room_id(e, room_id))
        .collect();
    // The room's state: the state before the timeline, then the timeline's own state events,
    // the later of two for the same `(type, state_key)` winning.
    let mut state: Vec<Value> = Vec::new();
    let state_source = if membership == "invite" {
        &["invite_state", "events"][..]
    } else {
        &["state", "events"][..]
    };
    let timeline_state = timeline.iter().filter(|e| e.get("state_key").is_some());
    for event in events_at(room, state_source)
        .map(|e| with_room_id(e, room_id))
        .chain(timeline_state.cloned())
    {
        let key = (event.get("type").cloned(), event.get("state_key").cloned());
        state.retain(|e| (e.get("type").cloned(), e.get("state_key").cloned()) != key);
        state.push(event);
    }
    let start = room
        .get("timeline")
        .and_then(|t| t.get("prev_batch"))
        .and_then(Value::as_str)
        .unwrap_or(end);
    let account_data: Vec<Value> = events_at(room, &["account_data", "events"])
        .cloned()
        .collect();
    let mut out = json!({
        "room_id": room_id,
        "membership": membership,
        "messages": {"chunk": timeline, "start": start, "end": end},
        "state": state,
        "account_data": account_data,
        "visibility": "private",
    });
    if membership == "invite"
        && let Some(invite) = out["state"].as_array().and_then(|s| {
            s.iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("m.room.member"))
                .cloned()
        })
    {
        out["invite"] = invite;
    }
    out
}

/// Rearranges an initial `/sync` response into `/initialSync`'s.
#[must_use]
pub fn initial_sync_body(sync: &Value, end: &str) -> Value {
    let mut rooms = Vec::new();
    for (section, membership) in [("join", "join"), ("invite", "invite"), ("leave", "leave")] {
        for (room_id, room) in rooms_of(sync, section) {
            rooms.push(legacy_room(room_id, membership, room, end));
        }
    }
    let presence: Vec<Value> = events_at(sync, &["presence", "events"])
        .map(legacy_presence)
        .collect();
    let account_data: Vec<Value> = events_at(sync, &["account_data", "events"])
        .cloned()
        .collect();
    json!({
        "end": end,
        "rooms": rooms,
        "presence": presence,
        "account_data": account_data,
    })
}

/// `GET /initialSync` (deprecated): every room the user is in (and has left, with
/// `archived=true`) with its newest `limit` messages and its state, the user's presence list and
/// account data, and `end`, where `/events` continues from. See the module docs.
///
/// # Errors
/// [`UserError::InvalidParam`] for a `limit` that is not a number; a store or room failure.
pub async fn get_initial_sync<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Query(query): Query<InitialSyncQuery>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    let limit = parse_number("limit", query.limit.as_deref())?.unwrap_or(10);
    let archived = query.archived.as_deref() == Some("true");
    let (response, token) = sync::build(
        &state.hub,
        &state.e2e,
        &requester.user_id,
        SyncParams {
            since: None,
            full_state: false,
            timeout: Duration::ZERO,
            filter: filter_from(json!({
                "room": {"timeline": {"limit": limit}, "include_leave": archived},
            }))?,
            device_id: None,
        },
    )
    .await?;
    record_cursor(&state, &requester.user_id, &token).await?;
    Ok(Json(initial_sync_body(&response, &token.to_string())).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sync() -> Value {
        json!({
            "next_batch": "hsu1_next",
            "rooms": {
                "join": {
                    "!a:hs": {
                        "state": {"events": [
                            {"type": "m.room.name", "state_key": "", "content": {"name": "old"}},
                        ]},
                        "timeline": {"prev_batch": "p1", "events": [
                            {"type": "m.room.message", "event_id": "$1", "content": {"body": "hi"}},
                            {"type": "m.room.name", "state_key": "", "event_id": "$2", "content": {"name": "new"}},
                        ]},
                        "ephemeral": {"events": [{"type": "m.typing", "content": {"user_ids": ["@b:hs"]}}]},
                        "account_data": {"events": [{"type": "m.tag", "content": {"tags": {}}}]},
                    }
                },
                "invite": {
                    "!i:hs": {"invite_state": {"events": [
                        {"type": "m.room.name", "state_key": "", "content": {"name": "x"}},
                        {"type": "m.room.member", "state_key": "@me:hs", "sender": "@b:hs", "content": {"membership": "invite"}},
                    ]}}
                },
                "leave": {
                    "!l:hs": {"timeline": {"events": [
                        {"type": "m.room.member", "state_key": "@me:hs", "event_id": "$3", "content": {"membership": "leave"}},
                    ]}}
                }
            },
            "presence": {"events": [{"type": "m.presence", "sender": "@b:hs", "content": {"presence": "online"}}]},
            "account_data": {"events": [{"type": "m.direct", "content": {}}]},
        })
    }

    #[test]
    fn the_event_stream_is_every_new_event_with_its_room() {
        let chunk = events_chunk(&sample_sync(), "@me:hs");
        let described: Vec<(Option<&str>, Option<&str>)> = chunk
            .iter()
            .map(|e| {
                (
                    e.get("type").and_then(Value::as_str),
                    e.get("room_id").and_then(Value::as_str),
                )
            })
            .collect();
        assert_eq!(
            described,
            vec![
                (Some("m.room.message"), Some("!a:hs")),
                (Some("m.room.name"), Some("!a:hs")),
                (Some("m.typing"), Some("!a:hs")),
                (Some("m.room.member"), Some("!i:hs")),
                (Some("m.room.member"), Some("!l:hs")),
                (Some("m.presence"), None),
            ]
        );
        assert_eq!(chunk[5]["content"]["user_id"], "@b:hs");
    }

    #[test]
    fn initial_sync_lists_rooms_with_messages_and_their_latest_state() {
        let body = initial_sync_body(&sample_sync(), "hsu1_next");
        assert_eq!(body["end"], "hsu1_next");
        let rooms = body["rooms"].as_array().unwrap();
        assert_eq!(rooms.len(), 3);
        let joined = &rooms[0];
        assert_eq!(joined["membership"], "join");
        assert_eq!(joined["messages"]["start"], "p1");
        assert_eq!(joined["messages"]["end"], "hsu1_next");
        assert_eq!(joined["messages"]["chunk"][0]["room_id"], "!a:hs");
        let names: Vec<&Value> = joined["state"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "m.room.name")
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0]["content"]["name"], "new");
        let invited = &rooms[1];
        assert_eq!(invited["membership"], "invite");
        assert_eq!(invited["invite"]["state_key"], "@me:hs");
        assert_eq!(body["presence"][0]["content"]["user_id"], "@b:hs");
        assert_eq!(body["account_data"][0]["type"], "m.direct");
    }

    type TestState = UserState<
        hs_kv::memory::MemoryBackend,
        std::sync::Arc<hs_room::registry::RoomRegistry<hs_kv::memory::MemoryBackend>>,
    >;

    fn state() -> TestState {
        use std::sync::Arc;
        let store: crate::store::DynUserStore = Arc::new(
            crate::store::tables::TablesUserStore::open(hs_kv::memory::MemoryBackend::new())
                .unwrap(),
        );
        let e2e: Arc<dyn hs_e2e::store::E2eStore> = Arc::new(
            hs_e2e::store::tables::TablesE2eStore::open(hs_kv::memory::MemoryBackend::new())
                .unwrap(),
        );
        let hub = Arc::new(crate::hub::SessionHub::new(
            store,
            crate::room_source::test_support::registry("events.test"),
            500,
        ));
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        UserState {
            auth: hs_auth::state::AuthState::in_memory(),
            hub,
            e2e,
        }
    }

    async fn call(response: Result<Response, UserError>) -> Value {
        let bytes = axum::body::to_bytes(response.unwrap().into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn events(state: &TestState, user: &ruma::UserId, from: Option<&str>) -> Value {
        call(
            get_events(
                State(state.clone()),
                Query(EventsQuery {
                    from: from.map(str::to_owned),
                    timeout: Some("0".to_owned()),
                    room_id: None,
                }),
                UserRequester(hs_auth::requester::Requester::for_user(user.to_owned())),
            )
            .await,
        )
        .await
    }

    /// Through the real feed: `/events` with no `from` is "now" with nothing in it; each message
    /// sent after it arrives once, with its room; and `/initialSync` lists the room.
    #[tokio::test]
    async fn the_event_stream_carries_each_new_message_once_and_initial_sync_lists_rooms() {
        let state = state();
        let alice = ruma::user_id!("@alice:events.test").to_owned();
        let handle = state
            .hub
            .rooms()
            .create_room(
                alice.clone(),
                hs_room::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let say = |text: &'static str, at: i64| {
            let handle = handle.clone();
            let alice = alice.clone();
            async move {
                handle
                    .send_event(
                        alice,
                        "m.room.message".to_owned(),
                        None,
                        json!({"msgtype": "m.text", "body": text}),
                        None,
                        at,
                    )
                    .await
                    .unwrap();
            }
        };
        let bodies = |response: &Value| -> Vec<String> {
            response["chunk"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["room_id"] == room_id.as_str())
                .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
                .collect()
        };

        let now = events(&state, &alice, None).await;
        assert!(bodies(&now).is_empty(), "{now}");
        let mut from = now["end"].as_str().unwrap().to_owned();
        for (text, at) in [("one", 2), ("two", 3)] {
            say(text, at).await;
            let next = events(&state, &alice, Some(&from)).await;
            assert_eq!(bodies(&next), vec![text.to_owned()], "{next}");
            assert_eq!(next["start"], from.as_str());
            from = next["end"].as_str().unwrap().to_owned();
        }
        let quiet = events(&state, &alice, Some(&from)).await;
        assert!(bodies(&quiet).is_empty(), "{quiet}");

        let initial = call(
            get_initial_sync(
                State(state.clone()),
                Query(InitialSyncQuery::default()),
                UserRequester(hs_auth::requester::Requester::for_user(alice.clone())),
            )
            .await,
        )
        .await;
        let room = initial["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["room_id"] == room_id.as_str())
            .unwrap_or_else(|| panic!("{initial}"));
        assert_eq!(room["membership"], "join");
        assert!(
            room["messages"]["chunk"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["content"]["body"] == "two"),
            "{room}"
        );
        assert!(initial["end"].is_string());
    }

    #[test]
    fn a_number_that_is_not_one_is_a_bad_parameter() {
        assert!(parse_number("timeout", Some("hello")).is_err());
        assert_eq!(parse_number("timeout", Some("500")).unwrap(), Some(500));
        assert_eq!(parse_number("timeout", None).unwrap(), None);
    }
}
