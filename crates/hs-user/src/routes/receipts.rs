//! `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}` and
//! `POST /rooms/{roomId}/read_markers`.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_kv::KvBackend;
use ruma::{EventId, RoomId};
use serde::Deserialize;
use serde_json::json;

use crate::error::UserError;
use crate::receipts::ReceiptKind;
use crate::room_source::RoomSource;
use crate::state::{UserRequester, UserState};

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, UserError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| UserError::InvalidId(e.to_string()))
}

fn parse_event_id(raw: &str) -> Result<ruma::OwnedEventId, UserError> {
    EventId::parse(raw)
        .map(|e| e.to_owned())
        .map_err(|e| UserError::InvalidId(e.to_string()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Both endpoints in this module require the requester to be a currently-joined member of the
/// room -- a left, banned or merely-invited user has no receipt to publish, matching
/// `crate::routes::typing::put_typing`'s identical rule for the identical reason.
async fn require_joined<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    state: &UserState<B, R>,
    user_id: &ruma::UserId,
    room_id: &RoomId,
) -> Result<(), UserError> {
    let membership = state.hub.store().get_membership(user_id, room_id).await?;
    if !matches!(
        membership.as_ref().map(|m| m.membership.as_str()),
        Some("join")
    ) {
        return Err(UserError::Forbidden(
            "must be a joined member of the room to publish a receipt".to_owned(),
        ));
    }
    Ok(())
}

/// `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}`'s body. Every field is optional and
/// ignored beyond validating the request parses as an object -- this crate does not implement
/// MSC2285's `hidden` flag or thread-scoped receipts (MSC3771's `thread_id`), and the spec allows
/// an empty `{}` body.
#[derive(Debug, Deserialize, Default)]
pub struct ReceiptBody {}

/// `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}`. `receiptType` is `m.read` or
/// `m.read.private` (an ephemeral `m.receipt` event, via [`crate::receipts::ReceiptRegistry`]) --
/// **or**, matching Synapse's own accepted behavior on this same endpoint (not merely the spec's
/// documented `POST .../read_markers`), `m.fully_read`, handled identically to
/// [`post_read_markers`]'s `m.fully_read` field: private room account data, not a receipt at all.
/// `matrix-rust-sdk`'s `Room::send_single_receipt` can be called with any of the three (ruma's own
/// `create_receipt::v3::ReceiptType` includes `FullyRead`), so accepting it here as well as on
/// `/read_markers` is what makes a real client's obvious call actually work.
///
/// # Errors
/// Returns [`UserError`] if `roomId`/`eventId` do not parse, `receiptType` is none of the three
/// above, the requester is not a joined member of the room, or on a room-load/store failure.
pub async fn post_receipt<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path((room_id, receipt_type, event_id)): Path<(String, String, String)>,
    UserRequester(requester): UserRequester,
    body: Option<Json<ReceiptBody>>,
) -> Result<Response, UserError> {
    let _ = body;
    let room_id = parse_room_id(&room_id)?;
    let event_id = parse_event_id(&event_id)?;
    require_joined(&state, &requester.user_id, &room_id).await?;

    if receipt_type == "m.fully_read" {
        state
            .hub
            .store()
            .put_room_account_data(
                &requester.user_id,
                &room_id,
                "m.fully_read",
                json!({"event_id": event_id.as_str()}),
            )
            .await?;
        return Ok(Json(json!({})).into_response());
    }
    let kind = ReceiptKind::parse(&receipt_type).ok_or_else(|| {
        UserError::InvalidParam(format!(
            "unsupported receipt type {receipt_type:?} (expected m.read, m.read.private or \
             m.fully_read)"
        ))
    })?;

    state
        .hub
        .set_receipt(&room_id, &requester.user_id, kind, event_id, now_ms())
        .await?;
    Ok(Json(json!({})).into_response())
}

/// `POST /rooms/{roomId}/read_markers`'s body: every field optional, each naming an event id.
#[derive(Debug, Deserialize, Default)]
pub struct ReadMarkersBody {
    /// The fully-read marker (private room account data, `m.fully_read`).
    #[serde(rename = "m.fully_read")]
    pub fully_read: Option<String>,
    /// Equivalent to also calling `POST .../receipt/m.read/{eventId}`.
    #[serde(rename = "m.read")]
    pub read: Option<String>,
    /// Equivalent to also calling `POST .../receipt/m.read.private/{eventId}`.
    #[serde(rename = "m.read.private")]
    pub read_private: Option<String>,
}

/// `POST /rooms/{roomId}/read_markers`: sets the `m.fully_read` marker (room account data) and,
/// optionally, an `m.read`/`m.read.private` receipt, in one call. Per the spec, this is the
/// endpoint clients are meant to use for the fully-read marker -- `crate::routes::account_data`'s
/// generic `PUT .../account_data/{type}` is not special-cased to reject a direct `m.fully_read`
/// write, but this is the correct path for it.
///
/// # Errors
/// Returns [`UserError`] if `roomId` or any named event id do not parse, the requester is not a
/// joined member of the room, or on a store/room-load failure.
pub async fn post_read_markers<B: KvBackend + 'static, R: RoomSource<B> + 'static>(
    State(state): State<UserState<B, R>>,
    Path(room_id): Path<String>,
    UserRequester(requester): UserRequester,
    Json(body): Json<ReadMarkersBody>,
) -> Result<Response, UserError> {
    let room_id = parse_room_id(&room_id)?;
    require_joined(&state, &requester.user_id, &room_id).await?;

    if let Some(fully_read) = &body.fully_read {
        let event_id = parse_event_id(fully_read)?;
        state
            .hub
            .store()
            .put_room_account_data(
                &requester.user_id,
                &room_id,
                "m.fully_read",
                json!({"event_id": event_id.as_str()}),
            )
            .await?;
    }
    if let Some(read) = &body.read {
        let event_id = parse_event_id(read)?;
        state
            .hub
            .set_receipt(
                &room_id,
                &requester.user_id,
                ReceiptKind::Read,
                event_id,
                now_ms(),
            )
            .await?;
    }
    if let Some(read_private) = &body.read_private {
        let event_id = parse_event_id(read_private)?;
        state
            .hub
            .set_receipt(
                &room_id,
                &requester.user_id,
                ReceiptKind::ReadPrivate,
                event_id,
                now_ms(),
            )
            .await?;
    }
    Ok(Json(json!({})).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::SessionHub;
    use crate::room_source::test_support::registry;
    use crate::store::tables::TablesUserStore;
    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::{CreateRoomRequest, RoomActorHandle};
    use ruma::{event_id, user_id};
    use std::sync::Arc;
    use std::time::Duration;

    async fn test_state() -> (
        UserState<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>,
        ruma::OwnedRoomId,
    ) {
        let store: crate::store::DynUserStore =
            Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
        let e2e: Arc<dyn hs_e2e::store::E2eStore> =
            Arc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        let hub = Arc::new(SessionHub::new(
            store,
            registry("receipts.test"),
            usize::MAX,
        ));
        let alice = user_id!("@alice:receipts.test").to_owned();
        let handle: RoomActorHandle<MemoryBackend> = hub
            .rooms()
            .create_room(alice.clone(), CreateRoomRequest::default(), 1)
            .await
            .expect("create room");
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        hub.watch_room(handle.clone()).await;
        // Same "discovery gap" seeding as `crate::routes::typing`'s tests: the creation events
        // published before `watch_room` subscribed are never seen, so a follow-up event is what
        // actually records the creator's own `join` membership.
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hi"}),
                None,
                2,
            )
            .await
            .expect("seed event");
        tokio::time::sleep(Duration::from_millis(30)).await;
        (
            UserState {
                auth: AuthState::in_memory(),
                hub,
                e2e,
            },
            room_id,
        )
    }

    #[tokio::test]
    async fn a_joined_member_can_post_a_read_receipt() {
        let (state, room_id) = test_state().await;
        let alice = user_id!("@alice:receipts.test");
        let response = post_receipt(
            State(state.clone()),
            Path((
                room_id.to_string(),
                "m.read".to_owned(),
                event_id!("$one").to_string(),
            )),
            UserRequester(Requester::for_user(alice.to_owned())),
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let (content, seq) = state.hub.receipt_content_for(&room_id, alice).await;
        assert!(seq > 0);
        assert!(
            content["$one"]["m.read"]["@alice:receipts.test"]["ts"]
                .as_u64()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_unsupported_receipt_type_is_a_bad_request() {
        let (state, room_id) = test_state().await;
        let alice = user_id!("@alice:receipts.test");
        let err = post_receipt(
            State(state),
            Path((
                room_id.to_string(),
                "m.bogus".to_owned(),
                event_id!("$one").to_string(),
            )),
            UserRequester(Requester::for_user(alice.to_owned())),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.into_response().status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }

    /// `m.fully_read` via the receipt endpoint (not just `/read_markers`) -- Synapse's own
    /// extension to this endpoint, and what `matrix-rust-sdk`'s `send_single_receipt` can be
    /// called with directly. See [`post_receipt`]'s doc comment.
    #[tokio::test]
    async fn fully_read_via_the_receipt_endpoint_writes_room_account_data() {
        let (state, room_id) = test_state().await;
        let alice = user_id!("@alice:receipts.test");
        let response = post_receipt(
            State(state.clone()),
            Path((
                room_id.to_string(),
                "m.fully_read".to_owned(),
                event_id!("$one").to_string(),
            )),
            UserRequester(Requester::for_user(alice.to_owned())),
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let account_data = state
            .hub
            .store()
            .list_room_account_data(alice, &room_id)
            .await
            .unwrap();
        let fully_read = account_data
            .iter()
            .find(|a| a.event_type == "m.fully_read")
            .expect("m.fully_read was written");
        assert_eq!(fully_read.content["event_id"], json!(event_id!("$one")));
        // Must not also have been recorded as a receipt.
        let (content, seq) = state.hub.receipt_content_for(&room_id, alice).await;
        assert_eq!(content, json!({}));
        assert_eq!(seq, 0);
    }

    #[tokio::test]
    async fn a_non_member_cannot_post_a_receipt() {
        let (state, room_id) = test_state().await;
        let carol = user_id!("@carol:receipts.test");
        let err = post_receipt(
            State(state),
            Path((
                room_id.to_string(),
                "m.read".to_owned(),
                event_id!("$one").to_string(),
            )),
            UserRequester(Requester::for_user(carol.to_owned())),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.into_response().status(),
            axum::http::StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn read_markers_sets_fully_read_and_a_receipt_in_one_call() {
        let (state, room_id) = test_state().await;
        let alice = user_id!("@alice:receipts.test");
        let response = post_read_markers(
            State(state.clone()),
            Path(room_id.to_string()),
            UserRequester(Requester::for_user(alice.to_owned())),
            Json(ReadMarkersBody {
                fully_read: Some(event_id!("$one").to_string()),
                read: Some(event_id!("$one").to_string()),
                read_private: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let account_data = state
            .hub
            .store()
            .list_room_account_data(alice, &room_id)
            .await
            .unwrap();
        let fully_read = account_data
            .iter()
            .find(|a| a.event_type == "m.fully_read")
            .expect("m.fully_read was written");
        assert_eq!(fully_read.content["event_id"], json!(event_id!("$one")));

        let (content, _) = state.hub.receipt_content_for(&room_id, alice).await;
        assert!(content["$one"]["m.read"]["@alice:receipts.test"].is_object());
    }

    #[tokio::test]
    async fn read_markers_with_only_fully_read_sets_no_receipt() {
        let (state, room_id) = test_state().await;
        let alice = user_id!("@alice:receipts.test");
        post_read_markers(
            State(state.clone()),
            Path(room_id.to_string()),
            UserRequester(Requester::for_user(alice.to_owned())),
            Json(ReadMarkersBody {
                fully_read: Some(event_id!("$one").to_string()),
                read: None,
                read_private: None,
            }),
        )
        .await
        .unwrap();
        let (content, seq) = state.hub.receipt_content_for(&room_id, alice).await;
        assert_eq!(content, json!({}));
        assert_eq!(seq, 0);
    }
}
