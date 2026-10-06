//! Room peeking (MSC2753): `POST /peek/{roomIdOrAlias}` and `POST /rooms/{roomId}/unpeek`.
//!
//! A peek lets somebody follow a `world_readable` room through `/sync` without joining it. It is
//! per device: the device that peeked gets the room in its `rooms.peek` section, as a room it
//! had joined would be in `rooms.join`, and the user's other devices do not (Sytest's
//! `31sync/17peeking.pl`). Synapse never implemented it; Dendrite does, and this follows the
//! MSC's shape:
//!
//! - the room must be `world_readable` now, or the peek is refused `403 M_FORBIDDEN` -- under
//!   `shared`, `invited` or `joined` history a non-member may not read the room at all;
//! - a peek is recorded in the user store (`hs_user.peeks`) with the feed position it began at:
//!   the peek writes the room into the user's feed, so the peeking device's next `/sync` sends
//!   the room whole and later ones follow it from there (`crate::sync`'s peek section);
//! - the session hub writes the room's later updates into its peekers' feeds as it does its
//!   members' (`SessionHub::fan_out_to_peekers`), and ends every peek of a user who joins;
//! - a room that stops being world-readable ends the peeks into it the next time a peeking
//!   device syncs.
//!
//! Only rooms this server holds can be peeked: peeking over federation (the MSC's
//! `/_matrix/federation/v1/peek`) is not implemented, and a room this server is not in answers
//! `404 M_NOT_FOUND`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use serde_json::{Value, json};

use crate::error::UserError;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

/// Whether `actor`'s room is `world_readable` now.
pub(crate) fn is_world_readable(actor: &hs_room::actor::RoomActor<impl KvBackend>) -> bool {
    actor
        .state_event("m.room.history_visibility", "")
        .ok()
        .flatten()
        .map(hs_room::routes::render::client_event_json)
        .and_then(|event| {
            event
                .pointer("/content/history_visibility")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("world_readable")
}

/// The room `target` names: a room id, or a local alias.
fn resolve_target<B: KvBackend + 'static, R: RoomSource<B>>(
    state: &UserState<B, R>,
    target: &str,
) -> Result<ruma::OwnedRoomId, UserError> {
    if target.starts_with('#') {
        let alias = ruma::RoomAliasId::parse(target)
            .map_err(|e| UserError::InvalidId(format!("{target:?} is not a room alias: {e}")))?;
        return state
            .hub
            .rooms()
            .resolve_alias(&alias)?
            .ok_or_else(|| UserError::NotFound(format!("no room has the alias {alias}")));
    }
    ruma::RoomId::parse(target)
        .map_err(|e| UserError::InvalidId(format!("{target:?} is not a room id: {e}")))
}

/// `POST /peek/{roomIdOrAlias}`: starts peeking into a world-readable room from the
/// requester's device, answering `{"room_id": ...}`.
///
/// # Errors
/// `400` for a malformed target, `404 M_NOT_FOUND` for a room or alias this server does not
/// hold, `403 M_FORBIDDEN` for a room that is not world-readable, or a store failure.
pub async fn post_peek<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path(target): Path<String>,
    UserRequester(requester): UserRequester,
    PermissiveJson(_body): PermissiveJson<Value>,
) -> Result<Response, UserError> {
    let room_id = resolve_target(&state, &target)?;
    let handle = match state.hub.room(&room_id).await {
        Ok(handle) => handle,
        Err(error) if error.is_room_not_found() => {
            return Err(UserError::NotFound(format!(
                "this server does not hold {room_id}; peeking over federation is not supported"
            )));
        }
        Err(error) => return Err(error),
    };
    let user = requester.user_id.clone();
    let (world_readable, joined, head) = handle
        .query(move |actor| {
            let joined = actor
                .state_event("m.room.member", user.as_str())
                .ok()
                .flatten()
                .map(hs_room::routes::render::client_event_json)
                .and_then(|e| {
                    e.pointer("/content/membership")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some("join");
            (
                is_world_readable(actor),
                joined,
                actor.head_update().map(|update| update.room_pos),
            )
        })
        .await;
    if !world_readable {
        return Err(UserError::Forbidden(format!(
            "{room_id} is not world-readable, so it cannot be peeked into"
        )));
    }
    // Already in the room: it is in `rooms.join`, which is more than a peek would give.
    if joined {
        return Ok(Json(json!({"room_id": room_id})).into_response());
    }
    let device = crate::sync::cursor_device_id(requester.device_id.as_deref()).to_owned();
    let store = state.hub.store();
    // The peek's own feed entry: what makes the room this device's news, and where following
    // it begins.
    let feed_seq = store
        .append_feed_entry(&requester.user_id, &room_id, head.unwrap_or(0))
        .await?;
    store
        .put_peek(&requester.user_id, &device, &room_id, feed_seq)
        .await?;
    tracing::info!(
        user_id = %requester.user_id,
        device_id = %device,
        %room_id,
        feed_seq,
        "a device started peeking into a world-readable room"
    );
    state.hub.account_data_changed(&requester.user_id).await;
    Ok(Json(json!({"room_id": room_id})).into_response())
}

/// `POST /rooms/{roomId}/unpeek`: stops the requester's device peeking into `roomId`. Answers
/// `{}` whether or not it was peeking.
///
/// # Errors
/// `400` for a malformed room id, or a store failure.
pub async fn post_unpeek<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path(room_id): Path<String>,
    UserRequester(requester): UserRequester,
) -> Result<Response, UserError> {
    let room_id = ruma::RoomId::parse(room_id.as_str())
        .map_err(|e| UserError::InvalidId(format!("{room_id:?} is not a room id: {e}")))?;
    let device = crate::sync::cursor_device_id(requester.device_id.as_deref()).to_owned();
    let ended = state
        .hub
        .store()
        .remove_peeks(&requester.user_id, Some(&device), &room_id)
        .await?;
    tracing::info!(
        user_id = %requester.user_id,
        device_id = %device,
        %room_id,
        ended,
        "a device stopped peeking into a room"
    );
    Ok(Json(json!({})).into_response())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::CreateRoomRequest;
    use ruma::{OwnedUserId, user_id};
    use serde_json::{Value, json};

    use super::*;
    use crate::hub::SessionHub;
    use crate::room_source::test_support::registry;
    use crate::store::tables::TablesUserStore;
    use crate::sync::{SyncParams, build};
    use crate::token::SyncToken;

    type TestState = UserState<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>;

    fn state() -> TestState {
        state_with_threshold(500)
    }

    /// A state whose rooms are hot above `threshold` members.
    fn state_with_threshold(threshold: usize) -> TestState {
        let store: crate::store::DynUserStore =
            Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        let e2e: Arc<dyn hs_e2e::store::E2eStore> =
            Arc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        let hub = Arc::new(SessionHub::new(store, registry("peek.test"), threshold));
        std::mem::forget(hub.watch_all(hub.rooms().subscribe_global()));
        UserState {
            auth: AuthState::in_memory(),
            hub,
            e2e,
        }
    }

    fn on_device(user: &OwnedUserId, device: &str) -> Requester {
        let mut requester = Requester::for_user(user.clone());
        requester.device_id = Some(device.into());
        requester
    }

    async fn peek(
        state: &TestState,
        requester: Requester,
        target: &str,
    ) -> Result<Value, UserError> {
        let response = post_peek(
            State(state.clone()),
            Path(target.to_owned()),
            UserRequester(requester),
            PermissiveJson(json!({})),
        )
        .await?;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    async fn sync(
        state: &TestState,
        user: &OwnedUserId,
        device: &str,
        since: Option<SyncToken>,
    ) -> (Value, SyncToken) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        build(
            &state.hub,
            &state.e2e,
            user,
            SyncParams {
                since,
                full_state: false,
                timeout: Duration::from_millis(20),
                filter: crate::filter::SyncFilter::none(),
                device_id: Some(device.into()),
            },
        )
        .await
        .unwrap()
    }

    /// A room alice created, with its history visibility set to `visibility`.
    async fn room(
        state: &TestState,
        alice: &OwnedUserId,
        visibility: &str,
        alias: Option<&str>,
    ) -> (
        hs_room::actor::RoomActorHandle<MemoryBackend>,
        ruma::OwnedRoomId,
    ) {
        let handle = state
            .hub
            .rooms()
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    room_alias_name: alias.map(str::to_owned),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        handle
            .send_event(
                alice.clone(),
                "m.room.history_visibility".to_owned(),
                Some(String::new()),
                json!({"history_visibility": visibility}),
                None,
                2,
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        (handle, room_id)
    }

    async fn say(
        handle: &hs_room::actor::RoomActorHandle<MemoryBackend>,
        who: &OwnedUserId,
        body: &str,
    ) {
        handle
            .send_event(
                who.clone(),
                "m.room.message".to_owned(),
                None,
                json!({"body": body, "msgtype": "m.text"}),
                None,
                3,
            )
            .await
            .unwrap();
    }

    fn peeked<'a>(response: &'a Value, room_id: &ruma::RoomId) -> &'a Value {
        &response["rooms"]["peek"][room_id.as_str()]
    }

    fn bodies(entry: &Value) -> Vec<String> {
        entry["timeline"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["content"]["body"].as_str().map(str::to_owned))
            .collect()
    }

    /// Sytest's "Local users can peek into world_readable rooms by room ID" and "Peeked rooms
    /// only turn up in the sync for the device who peeked them".
    #[tokio::test]
    async fn a_device_peeks_into_a_world_readable_room_and_follows_it_alone() {
        let state = state();
        let alice = user_id!("@alice:peek.test").to_owned();
        let bob = user_id!("@bob:peek.test").to_owned();
        let (handle, room_id) = room(&state, &alice, "world_readable", None).await;
        let (_, phone_token) = sync(&state, &bob, "PHONE", None).await;
        let (_, laptop_token) = sync(&state, &bob, "LAPTOP", None).await;

        let answer = peek(&state, on_device(&bob, "PHONE"), room_id.as_str())
            .await
            .unwrap();
        assert_eq!(answer, json!({"room_id": room_id}));
        say(&handle, &alice, "something to peek").await;

        let (response, phone_token) = sync(&state, &bob, "PHONE", Some(phone_token)).await;
        let entry = peeked(&response, &room_id);
        let events = entry["timeline"]["events"].as_array().unwrap();
        assert_eq!(events[0]["type"], "m.room.create", "{response}");
        assert_eq!(bodies(entry), vec!["something to peek"], "{response}");
        assert!(
            entry["state"]["events"].as_array().unwrap().is_empty(),
            "{response}"
        );
        assert!(response["rooms"]["join"].is_null(), "{response}");

        // Nothing new: not in the next batch.
        let (response, phone_token) = sync(&state, &bob, "PHONE", Some(phone_token)).await;
        assert!(peeked(&response, &room_id).is_null(), "{response}");

        say(&handle, &alice, "something else to peek").await;
        let (response, _) = sync(&state, &bob, "PHONE", Some(phone_token)).await;
        assert_eq!(
            bodies(peeked(&response, &room_id)),
            vec!["something else to peek"],
            "{response}"
        );

        // The other device never sees it.
        let (response, _) = sync(&state, &bob, "LAPTOP", Some(laptop_token)).await;
        assert!(response["rooms"]["peek"].is_null(), "{response}");
        let (response, _) = sync(&state, &bob, "LAPTOP", None).await;
        assert!(response["rooms"]["peek"].is_null(), "{response}");
    }

    /// A new event in a peeked room wakes the peeking device's long-poll, for a room above
    /// the fan-out threshold (which writes no feed entries) as for one below it. A hot room's
    /// used to be found only by the next poll, after this one's timeout.
    #[tokio::test]
    async fn a_new_event_in_a_peeked_room_wakes_a_long_poll() {
        for threshold in [500, 0] {
            let state = state_with_threshold(threshold);
            let alice = user_id!("@alice:peek.test").to_owned();
            let bob = user_id!("@bob:peek.test").to_owned();
            let (handle, room_id) = room(&state, &alice, "world_readable", None).await;
            peek(&state, on_device(&bob, "PHONE"), room_id.as_str())
                .await
                .unwrap();
            let (_, token) = sync(&state, &bob, "PHONE", None).await;

            let speaker = {
                let handle = handle.clone();
                let alice = alice.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    say(&handle, &alice, "news").await;
                })
            };
            let started = std::time::Instant::now();
            let (response, _) = build(
                &state.hub,
                &state.e2e,
                &bob,
                SyncParams {
                    since: Some(token),
                    full_state: false,
                    timeout: Duration::from_secs(10),
                    filter: crate::filter::SyncFilter::none(),
                    device_id: Some("PHONE".into()),
                },
            )
            .await
            .unwrap();
            speaker.await.unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "threshold {threshold}: woken, not timed out"
            );
            assert_eq!(
                bodies(peeked(&response, &room_id)),
                vec!["news"],
                "threshold {threshold}: {response}"
            );
        }
    }

    /// A peeker is shown an erased local sender's events pruned, as a member who joined after
    /// them is (`crate::sync::timeline::apply_erasure`).
    #[tokio::test]
    async fn a_peeker_sees_an_erased_senders_messages_pruned() {
        let state = state();
        let accounts: Arc<dyn hs_auth::store::AuthStore> =
            Arc::new(hs_auth::store::memory::InMemoryAuthStore::new());
        state.hub.install_account_store(accounts.clone());
        let alice = user_id!("@alice:peek.test").to_owned();
        let bob = user_id!("@bob:peek.test").to_owned();
        let (handle, room_id) = room(&state, &alice, "world_readable", None).await;
        say(&handle, &alice, "erase me").await;
        let mut record = hs_auth::store::UserRecord::new(alice.clone(), 0);
        record.erased = true;
        accounts.create_user(record).await.unwrap();

        peek(&state, on_device(&bob, "PHONE"), room_id.as_str())
            .await
            .unwrap();
        let (response, _) = sync(&state, &bob, "PHONE", None).await;
        let message = peeked(&response, &room_id)["timeline"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "m.room.message")
            .cloned()
            .unwrap();
        assert_eq!(message["content"], json!({}), "{response}");
    }

    /// Sytest's "We can't peek into rooms with {shared,invited,joined} history_visibility".
    #[tokio::test]
    async fn a_room_that_is_not_world_readable_cannot_be_peeked_into() {
        let state = state();
        let alice = user_id!("@alice:peek.test").to_owned();
        let bob = user_id!("@bob:peek.test").to_owned();
        for visibility in ["shared", "invited", "joined"] {
            let (_, room_id) = room(&state, &alice, visibility, None).await;
            let error = peek(&state, on_device(&bob, "PHONE"), room_id.as_str())
                .await
                .unwrap_err();
            let matrix = error.to_matrix_error();
            assert_eq!(
                matrix.status,
                axum::http::StatusCode::FORBIDDEN,
                "{visibility}"
            );
        }
        let error = peek(&state, on_device(&bob, "PHONE"), "!nowhere:peek.test")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_matrix_error().status,
            axum::http::StatusCode::NOT_FOUND
        );
    }

    /// Sytest's "Local users can peek by room alias".
    #[tokio::test]
    async fn a_room_can_be_peeked_into_by_its_alias() {
        let state = state();
        let alice = user_id!("@alice:peek.test").to_owned();
        let bob = user_id!("@bob:peek.test").to_owned();
        let (handle, room_id) = room(&state, &alice, "world_readable", Some("peektest")).await;
        let answer = peek(&state, on_device(&bob, "PHONE"), "#peektest:peek.test")
            .await
            .unwrap();
        assert_eq!(answer["room_id"], room_id.as_str());
        say(&handle, &alice, "something to peek").await;
        let (response, _) = sync(&state, &bob, "PHONE", None).await;
        assert_eq!(
            bodies(peeked(&response, &room_id)),
            vec!["something to peek"],
            "{response}"
        );
    }

    /// Joining ends the peek (the room moves to `join`); a room that stops being
    /// world-readable ends it too; and `unpeek` ends it on request.
    #[tokio::test]
    async fn a_peek_ends_on_joining_on_losing_world_readability_and_on_unpeek() {
        let state = state();
        let alice = user_id!("@alice:peek.test").to_owned();
        let bob = user_id!("@bob:peek.test").to_owned();
        let store = state.hub.store();

        let (handle, room_id) = room(&state, &alice, "world_readable", None).await;
        peek(&state, on_device(&bob, "PHONE"), room_id.as_str())
            .await
            .unwrap();
        handle
            .membership(
                bob.clone(),
                hs_room::membership::Action::Join,
                bob.clone(),
                json!({}),
                4,
            )
            .await
            .unwrap();
        let (response, _) = sync(&state, &bob, "PHONE", None).await;
        assert!(response["rooms"]["peek"].is_null(), "{response}");
        assert!(
            !response["rooms"]["join"][room_id.as_str()].is_null(),
            "{response}"
        );
        assert!(
            store
                .list_peeks(&bob, "PHONE".into())
                .await
                .unwrap()
                .is_empty()
        );

        let (handle, room_id) = room(&state, &alice, "world_readable", None).await;
        peek(&state, on_device(&bob, "PHONE"), room_id.as_str())
            .await
            .unwrap();
        handle
            .send_event(
                alice.clone(),
                "m.room.history_visibility".to_owned(),
                Some(String::new()),
                json!({"history_visibility": "shared"}),
                None,
                5,
            )
            .await
            .unwrap();
        let (response, _) = sync(&state, &bob, "PHONE", None).await;
        assert!(peeked(&response, &room_id).is_null(), "{response}");
        assert!(
            store
                .list_peeks(&bob, "PHONE".into())
                .await
                .unwrap()
                .is_empty()
        );

        let (_, room_id) = room(&state, &alice, "world_readable", None).await;
        peek(&state, on_device(&bob, "PHONE"), room_id.as_str())
            .await
            .unwrap();
        post_unpeek(
            State(state.clone()),
            Path(room_id.to_string()),
            UserRequester(on_device(&bob, "PHONE")),
        )
        .await
        .unwrap();
        assert!(
            store
                .list_peeks(&bob, "PHONE".into())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
