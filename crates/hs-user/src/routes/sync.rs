//! `GET /sync`.

use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use serde::Deserialize;

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::routes::presence::VALID_PRESENCE;
use crate::state::{UserRequester, UserState};
use crate::sync::{self, SyncParams};
use crate::token::SyncToken;

/// The longest a caller may ask this server to hold a `/sync` connection open for. Matches
/// Synapse's own default cap (`max_lag`-adjacent bound in `sync.py`), high enough that a real
/// client's `timeout=30000`-style requests are never truncated, low enough to bound how long one
/// HTTP worker is tied up per idle client.
const MAX_TIMEOUT: Duration = Duration::from_secs(60);

/// Query parameters for `GET /sync`.
#[derive(Debug, Deserialize)]
pub struct SyncQuery {
    /// Inline JSON or a previously uploaded filter id.
    pub filter: Option<String>,
    /// The `since` token from a previous sync.
    pub since: Option<String>,
    /// Whether to send full state for every room regardless of what changed.
    #[serde(default)]
    pub full_state: bool,
    /// The presence state this poll puts the caller in. Omitted means `online` -- polling
    /// `/sync` is itself the signal that a client is there, which is the spec's own default.
    pub set_presence: Option<String>,
    /// Milliseconds to long-poll for.
    pub timeout: Option<u64>,
}

/// `GET /sync`.
///
/// # Errors
/// Returns [`UserError`] if `since` or `filter` do not parse, or on a store/room failure.
pub async fn get_sync<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Query(query): Query<SyncQuery>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    let since = query.since.as_deref().map(SyncToken::decode).transpose()?;
    let filter = crate::filter::resolve(
        state.hub.store(),
        &requester.user_id,
        query.filter.as_deref(),
    )
    .await?;
    let timeout = query
        .timeout
        .map(Duration::from_millis)
        .unwrap_or_default()
        .min(MAX_TIMEOUT);

    // The presence side effect, before the long poll rather than after it: a client that polls
    // with a 30-second timeout is present *now*, and everyone who shares a room with them should
    // learn it now rather than half a minute later.
    //
    // The spec: omitting `set_presence` marks the client online, `offline` means "do not mark me
    // online" (leave whatever is stored alone -- it does not say to mark them offline), and
    // `unavailable` marks them idle.
    match query.set_presence.as_deref() {
        Some("offline") => {}
        Some(other) => {
            if !VALID_PRESENCE.contains(&other) {
                return Err(UserError::InvalidId(format!(
                    "set_presence must be one of {VALID_PRESENCE:?}, got {other:?}"
                )));
            }
            state.hub.touch_presence(&requester.user_id, other).await?;
        }
        None => {
            state
                .hub
                .touch_presence(&requester.user_id, "online")
                .await?;
        }
    }

    let (response, token) = sync::build(
        &state.hub,
        &state.e2e,
        &requester.user_id,
        SyncParams {
            since,
            full_state: query.full_state,
            timeout,
            filter,
            device_id: requester.device_id.clone(),
        },
    )
    .await?;

    // Recorded for a requester with no device too (an appservice's own `as_token`), under a key
    // of its own: see `sync::cursor_device_id` for what went wrong without one.
    state
        .hub
        .store()
        .record_device_cursor(
            &requester.user_id,
            sync::cursor_device_id(requester.device_id.as_deref()),
            token.feed_seq,
        )
        .await?;

    Ok(Json(response).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::SessionHub;
    use crate::room_source::test_support::registry;
    use crate::store::tables::TablesUserStore;
    use hs_auth::requester::{AppserviceIdentity, Requester};
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::CreateRoomRequest;
    use hs_room::membership::Action;
    use ruma::{OwnedUserId, user_id};
    use serde_json::Value;
    use std::sync::Arc;

    type TestState = UserState<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>;

    fn state() -> TestState {
        let store: crate::store::DynUserStore =
            Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        let e2e: Arc<dyn hs_e2e::store::E2eStore> =
            Arc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        let hub = Arc::new(SessionHub::new(store, registry("sync.test"), 500));
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        UserState {
            auth: AuthState::in_memory(),
            hub,
            e2e,
        }
    }

    /// An appservice acting as one of its users through its own `as_token`, with no
    /// `device_id`: what a bridge's puppet does.
    fn masquerading(user: &OwnedUserId) -> Requester {
        let mut requester = Requester::for_user(user.clone());
        requester.appservice = Some(AppserviceIdentity {
            appservice_id: "bridge".to_owned(),
            sender: user_id!("@bridgebot:sync.test").to_owned(),
            masqueraded_user: true,
            masqueraded_device_id: None,
            rate_limited: false,
            msc4190_enabled: false,
        });
        requester
    }

    async fn sync(state: &TestState, requester: Requester, since: Option<&str>) -> Value {
        let response = get_sync(
            State(state.clone()),
            Query(SyncQuery {
                filter: None,
                since: since.map(str::to_owned),
                full_state: false,
                set_presence: Some("offline".to_owned()),
                timeout: Some(0),
            }),
            UserRequester(requester),
        )
        .await
        .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn bodies(response: &Value, room_id: &ruma::RoomId) -> Vec<String> {
        response["rooms"]["join"][room_id.as_str()]["timeline"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
            .collect()
    }

    async fn say(
        handle: &hs_room::actor::RoomActorHandle<MemoryBackend>,
        who: &OwnedUserId,
        body: &str,
        ts: i64,
    ) {
        handle
            .send_event(
                who.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": body}),
                None,
                ts,
            )
            .await
            .unwrap();
    }

    /// The known gap "a requester with no device never records a feed cursor": two bridge
    /// puppets syncing through the appservice's token, with no device. The entry each one's
    /// token pointed at went on coalescing every later update to the room, so the incremental
    /// sync after a message saw no change at all. Now each records a cursor under the
    /// device-less key, per user: the message arrives once, for each of them, and is not
    /// repeated.
    #[tokio::test]
    async fn a_requester_with_no_device_sees_each_new_event_once() {
        let state = state();
        let alice = user_id!("@alice:sync.test").to_owned();
        let puppets = [
            user_id!("@bridge_one:sync.test").to_owned(),
            user_id!("@bridge_two:sync.test").to_owned(),
        ];
        let handle = state
            .hub
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
        for (ts, puppet) in (2..).zip(&puppets) {
            handle
                .membership(
                    puppet.clone(),
                    Action::Join,
                    puppet.clone(),
                    serde_json::json!({}),
                    ts,
                )
                .await
                .unwrap();
        }
        say(&handle, &alice, "before", 5).await;

        let mut tokens = Vec::new();
        for puppet in &puppets {
            let first = sync(&state, masquerading(puppet), None).await;
            assert!(
                bodies(&first, &room_id).contains(&"before".to_owned()),
                "{first}"
            );
            tokens.push(first["next_batch"].as_str().unwrap().to_owned());
        }

        say(&handle, &alice, "news", 6).await;

        for (puppet, token) in puppets.iter().zip(&tokens) {
            let second = sync(&state, masquerading(puppet), Some(token)).await;
            assert_eq!(
                bodies(&second, &room_id),
                vec!["news".to_owned()],
                "{puppet}'s incremental sync carries the new message: {second}"
            );
            let next = second["next_batch"].as_str().unwrap();
            let third = sync(&state, masquerading(puppet), Some(next)).await;
            assert!(
                bodies(&third, &room_id).is_empty(),
                "{puppet} is not sent it again: {third}"
            );
        }

        // Each puppet's cursor is its own, under the device-less key.
        for puppet in &puppets {
            assert!(state.hub.store().max_device_cursor(puppet).await.unwrap() > 0);
        }
    }
}
