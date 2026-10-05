//! `PUT /rooms/{roomId}/redact/{eventId}/{txnId}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{EventId, RoomId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::state::{RoomRequester, RoomState};

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// `PUT /rooms/{roomId}/redact/{eventId}/{txnId}`.
///
/// Deduplicated on `(sender, device, txnId)`, like `crate::routes::send_state::put_send`
/// (`RoomActor::redact_txn`).
pub async fn put_redact<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path((room_id, event_id, txn_id)): Path<(String, String, String)>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let room_id = RoomId::parse(&room_id)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))?;
    let target = EventId::parse(&event_id)
        .map(|e| e.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))?;
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // An event of another room is not this room's to redact: Synapse answers `400` ("Cannot
    // redact event from a different room") rather than send a redaction naming an event its
    // room does not hold. An event this server holds nowhere may still be redacted (the spec
    // lets a redaction name an event not yet received). Sytest's "PUT /redact disallows
    // redaction of event in different room".
    if let Some(row) = state.rooms.find_event_globally(&target)?
        && row.room_id != room_id.as_str()
    {
        tracing::debug!(
            %room_id,
            target = %target,
            target_room = %row.room_id,
            "refused a redaction naming an event of a different room"
        );
        return Err(RoomError::InvalidParam(
            "Cannot redact event from a different room".to_owned(),
        ));
    }
    let handle = state.rooms.get_or_load(&room_id).await?;
    // MSC3823: a suspended account may still redact its own events (cleaning up after itself
    // is what suspension leaves it able to do), and nobody else's.
    if requester.suspended {
        let own = target.clone();
        let sender = handle
            .query(move |actor| actor.event_by_id(&own).map(|e| e.header().sender.clone()))
            .await;
        if sender.as_ref().is_some_and(|s| *s != requester.user_id) {
            return Err(RoomError::UserSuspended);
        }
    }
    crate::moderation::check_redaction_limit(&state, &requester).await?;
    if requester.shadow_banned {
        crate::moderation::note_shadowed(&requester, "redact");
        return Ok(Json(json!({"event_id": crate::moderation::shadow_event_id()})).into_response());
    }
    let event = handle
        .redact(
            requester.user_id.clone(),
            requester.device_id.clone(),
            txn_id,
            target,
            reason,
            now_ms(),
        )
        .await?;
    Ok(Json(json!({"event_id": event.event_id().to_string()})).into_response())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hs_auth::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;

    fn requester(user: &ruma::UserId) -> RoomRequester {
        RoomRequester(hs_auth::requester::Requester {
            user_id: user.to_owned(),
            device_id: None,
            is_guest: false,
            is_admin: false,
            shadow_banned: false,
            suspended: false,
            appservice: None,
            access_token_id: None,
        })
    }

    /// Sytest's "PUT /redact disallows redaction of event in different room": bob may redact
    /// in his own room, but not name alice's event of another room there.
    #[tokio::test]
    async fn an_event_of_another_room_is_not_redacted() {
        let identity = HomeserverIdentity::for_tests("hs1");
        let state = RoomState {
            auth: AuthState::in_memory(),
            rooms: Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).unwrap()),
            identity,
            remote_join: None,
        };
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let mut rooms = Vec::new();
        for who in [alice, bob] {
            let handle = state
                .rooms
                .create_room(
                    who.to_owned(),
                    crate::actor::CreateRoomRequest::default(),
                    1,
                )
                .await
                .unwrap();
            let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
            let event = handle
                .send_event(
                    who.to_owned(),
                    "m.room.message".to_owned(),
                    None,
                    json!({"msgtype": "m.text", "body": "test"}),
                    None,
                    2,
                )
                .await
                .unwrap();
            rooms.push((room_id, event.event_id().to_owned()));
        }
        let redact = |room: &ruma::OwnedRoomId, event: &ruma::OwnedEventId, txn: &str| {
            put_redact::<MemoryBackend>(
                State(state.clone()),
                Path((room.to_string(), event.to_string(), txn.to_owned())),
                requester(bob),
                PermissiveJson(json!({})),
            )
        };
        let err = redact(&rooms[1].0, &rooms[0].1, "t1").await.unwrap_err();
        assert!(matches!(err, RoomError::InvalidParam(_)), "{err:?}");
        redact(&rooms[1].0, &rooms[1].1, "t2")
            .await
            .expect("his own room's event is his to redact");
    }
}
