//! `POST /rooms/{roomId}/upgrade`.
//!
//! Ported from the spec's documented server behaviour
//! (`refs/matrix-spec/content/client-server-api/modules/room_upgrades.md`, CC-BY-4.0): checks the
//! requester can send `m.room.tombstone`, creates a replacement room with a `predecessor` field,
//! replicates the recommended transferable state events, moves local aliases, sends the
//! tombstone to the old room, and (best-effort) locks the old room down. This crate implements
//! all of that locally; nothing here talks to another server, since this crate does not own
//! federation (`hs-federation`/track 06 owns telling *other* servers about the upgrade, and any
//! other local homeserver-of-a-remote-member concerns).

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{RoomId, RoomVersionId};
use serde_json::{Value, json};

use crate::actor::{CreateRoomRequest, InitialStateEvent};
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

fn parse_room_id(raw: &str) -> Result<ruma::OwnedRoomId, RoomError> {
    RoomId::parse(raw)
        .map(|r| r.to_owned())
        .map_err(|e| RoomError::BadRequest(e.to_string()))
}

/// `events_default`/`invite` in the old room, once the upgrade has happened: "the greater of `50`
/// and `users_default + 1`" (spec step 6). Best-effort only -- see [`post_upgrade`]'s doc comment
/// on why a failure here does not fail the whole call.
fn locked_power_levels(mut content: Value) -> Value {
    let users_default = content
        .get("users_default")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let floor = std::cmp::max(50, users_default + 1);
    if let Some(obj) = content.as_object_mut() {
        obj.insert("events_default".to_owned(), json!(floor));
        obj.insert("invite".to_owned(), json!(floor));
    }
    content
}

/// `POST /rooms/{roomId}/upgrade`.
pub async fn post_upgrade<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let old_room_id = parse_room_id(&room_id)?;
    let new_version_str = body
        .get("new_version")
        .and_then(Value::as_str)
        .ok_or_else(|| RoomError::BadRequest("missing new_version".into()))?;
    let new_version = RoomVersionId::try_from(new_version_str)
        .map_err(|_| RoomError::UnsupportedRoomVersion(new_version_str.to_owned()))?;
    // Validate the target version *before* touching anything -- `RoomVersionId::try_from` accepts
    // any syntactically opaque token, known or not (see `crate::routes::create_room`'s doc
    // comment on the same distinction); `hs_model::room_version::rules_for` is the real
    // "supported" gate, and it must run before the tombstone below, not after.
    if hs_model::room_version::rules_for(&new_version).is_none() {
        return Err(RoomError::UnsupportedRoomVersion(
            new_version_str.to_owned(),
        ));
    }

    let old_handle = state.rooms.get_or_load(&old_room_id).await?;
    let sender = requester.user_id.clone();
    let now = now_ms();

    // Reserve the replacement room's ID up front. The old room's tombstone (below) must name it
    // in `content.replacement_room`, and the new room's own `m.room.create` must name the
    // tombstone's event ID in `content.predecessor` -- a genuine circular dependency the spec's
    // reference implementation (Synapse's `RoomCreationHandler._upgrade_room`) breaks the same
    // way: mint the new room's ID first (free -- it needs no room to already exist), so both
    // events can be built with the other's final identifier already in hand.
    let new_room_id = RoomId::new_v1(&state.identity.server_name);

    // Step 1 ("checks that the user has permission to send `m.room.tombstone` events") + step 5
    // ("sends a tombstone event to the old room"), combined: an unauthorized sender gets
    // `RoomError::Forbidden` straight out of the ordinary event-authorization path
    // `RoomActorHandle::send_event` already runs, and nothing below this call executes -- no
    // replacement room is created for a rejected upgrade.
    let tombstone_content = json!({
        "body": "This room has been replaced",
        "replacement_room": new_room_id.to_string(),
    });
    let tombstone = old_handle
        .send_event(
            sender.clone(),
            "m.room.tombstone".to_owned(),
            Some(String::new()),
            tombstone_content,
            None,
            now,
        )
        .await?;

    // Step 3 + reading the `m.room.create`'s `type` (step 2's "a `type` field which is copied
    // from the predecessor room") + the old room's local aliases (step 4).
    let (transferable, creation_type, old_aliases, canonical_alias) = old_handle
        .query(|actor| {
            let canonical_alias = actor
                .state_event("m.room.canonical_alias", "")
                .ok()
                .flatten()
                .and_then(|e| e.json().get("content"))
                .map(|v| {
                    serde_json::from_slice::<Value>(&v.to_canonical_bytes()).unwrap_or(Value::Null)
                });
            (
                actor.transferable_state(),
                actor.creation_type(),
                actor.list_aliases().unwrap_or_default(),
                canonical_alias,
            )
        })
        .await;

    let mut power_level_content_override = None;
    let mut initial_state = Vec::with_capacity(transferable.len());
    for (event_type, content) in transferable {
        if event_type == "m.room.power_levels" {
            power_level_content_override = Some(content);
        } else {
            initial_state.push(InitialStateEvent {
                event_type: event_type.to_owned(),
                state_key: String::new(),
                content,
            });
        }
    }

    let mut creation_content = json!({
        "predecessor": {
            "room_id": old_room_id.to_string(),
            "event_id": tombstone.event_id().to_string(),
        }
    });
    if let Some(room_type) = creation_type
        && let Some(obj) = creation_content.as_object_mut()
    {
        obj.insert("type".to_owned(), Value::String(room_type));
    }

    let request = CreateRoomRequest {
        room_version: Some(new_version),
        room_id: Some(new_room_id.clone()),
        initial_state,
        power_level_content_override,
        creation_content,
        ..Default::default()
    };
    let new_handle = state
        .rooms
        .create_room(sender.clone(), request, now)
        .await?;

    // Step 4, continued: move every local alias this server hosts for the old room onto the new
    // one. Best-effort per alias -- `create_alias` can fail if the alias somehow already points
    // elsewhere (should not happen: it was just read off the old room's own alias set a moment
    // ago), and one alias failing to move must not undo the room upgrade itself or block the
    // others.
    for alias in &old_aliases {
        if let Ok(alias_id) = <&ruma::RoomAliasId>::try_from(alias.as_str()) {
            // The upgrade moves the alias; whoever asked for the upgrade becomes its creator on
            // the new room. The original creator is not carried across because the alias on the
            // new room is a new grant -- and the person doing the upgrade necessarily had the
            // power to make it.
            let alias_creator = sender.clone();
            let owned = alias_id.to_owned();
            let _ = old_handle
                .query(move |actor| actor.remove_alias(&owned))
                .await;
            let owned = alias_id.to_owned();
            let _ = new_handle
                .query(move |actor| actor.create_alias(&owned, &alias_creator))
                .await;
        }
    }
    // If the old room had a canonical alias naming one of the aliases just moved, give the new
    // room the same canonical-alias content (Synapse's own additional behavior beyond the spec's
    // required list, `_move_aliases_to_new_room`) -- best-effort, same reasoning as above.
    if let Some(content) = canonical_alias {
        let _ = new_handle
            .send_event(
                sender.clone(),
                "m.room.canonical_alias".to_owned(),
                Some(String::new()),
                content,
                None,
                now,
            )
            .await;
        let _ = old_handle
            .send_event(
                sender.clone(),
                "m.room.canonical_alias".to_owned(),
                Some(String::new()),
                json!({}),
                None,
                now,
            )
            .await;
    }

    // Step 6 ("if possible..."): lock the old room down. The spec's own hedge means a failure
    // here (the upgrading user often has just enough power to tombstone but not to re-author
    // power levels) must not fail the upgrade that has, by this point, already succeeded.
    if let Some(current) = old_handle
        .query(|actor| {
            actor
                .state_event("m.room.power_levels", "")
                .ok()
                .flatten()
                .and_then(|e| e.json().get("content"))
                .map(|v| {
                    serde_json::from_slice::<Value>(&v.to_canonical_bytes()).unwrap_or(Value::Null)
                })
        })
        .await
    {
        let _ = old_handle
            .send_event(
                sender,
                "m.room.power_levels".to_owned(),
                Some(String::new()),
                locked_power_levels(current),
                None,
                now,
            )
            .await;
    }

    Ok(Json(json!({"replacement_room": new_room_id.to_string()})).into_response())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;
    use crate::routes::create_room::post_create_room;
    use crate::state::RoomState;

    fn requester(user: &ruma::UserId) -> RoomRequester {
        RoomRequester(Requester {
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

    fn app() -> RoomState<MemoryBackend> {
        let backend = MemoryBackend::new();
        let identity = HomeserverIdentity::for_tests("hs1");
        let rooms = Arc::new(RoomRegistry::open(backend, identity.clone()).unwrap());
        RoomState {
            auth: AuthState::in_memory(),
            rooms,
            identity,
        }
    }

    async fn json_body(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// `POST /rooms/{roomId}/upgrade`, end to end through this crate's own route handlers (no
    /// HTTP/auth layer -- `RoomRequester` is constructed directly, the same shortcut this
    /// crate's other route-level tests do not currently take but which needs no more than
    /// constructing the extractor types by hand): creates a replacement room with a
    /// `predecessor`, carries over transferable state (`m.room.topic` here), and sends a
    /// tombstone to the old room naming the new one.
    #[tokio::test]
    async fn upgrade_creates_a_replacement_room_with_predecessor_and_tombstone() {
        let state = app();
        let alice = user_id!("@alice:hs1");

        let created = post_create_room::<MemoryBackend>(
            State(state.clone()),
            requester(alice),
            PermissiveJson(json!({"preset": "public_chat", "topic": "before the upgrade"})),
        )
        .await
        .unwrap();
        let old_room_id = json_body(created).await["room_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let old_room_id_parsed = RoomId::parse(&old_room_id).unwrap().to_owned();

        let upgraded = post_upgrade::<MemoryBackend>(
            State(state.clone()),
            Path(old_room_id.clone()),
            requester(alice),
            PermissiveJson(json!({"new_version": "10"})),
        )
        .await
        .unwrap();
        let upgraded = json_body(upgraded).await;
        let new_room_id = upgraded["replacement_room"].as_str().unwrap().to_owned();
        assert_ne!(
            new_room_id, old_room_id,
            "the replacement room must be a new room"
        );

        let new_room_id_parsed = RoomId::parse(&new_room_id).unwrap().to_owned();
        let new_handle = state.rooms.get_or_load(&new_room_id_parsed).await.unwrap();

        assert_eq!(
            new_handle.query(|actor| actor.room_version().clone()).await,
            RoomVersionId::V10,
            "the replacement room must actually be at the requested version"
        );

        let create_content: Value = new_handle
            .query(|actor| {
                let event = actor.state_event("m.room.create", "").unwrap().unwrap();
                let content = event.json().get("content").unwrap();
                serde_json::from_slice(&content.to_canonical_bytes()).unwrap()
            })
            .await;
        assert_eq!(
            create_content["predecessor"]["room_id"].as_str().unwrap(),
            old_room_id,
            "the new room's create event must name the old room as its predecessor"
        );

        let new_topic: Option<Value> = new_handle
            .query(|actor| {
                actor.state_event("m.room.topic", "").unwrap().map(|e| {
                    let content = e.json().get("content").unwrap();
                    serde_json::from_slice(&content.to_canonical_bytes()).unwrap()
                })
            })
            .await;
        assert_eq!(
            new_topic.expect("topic must have been carried over")["topic"],
            "before the upgrade",
            "the recommended transferable state (m.room.topic) must be replicated"
        );

        let old_handle = state.rooms.get_or_load(&old_room_id_parsed).await.unwrap();
        let tombstone_content: Value = old_handle
            .query(|actor| {
                let event = actor.state_event("m.room.tombstone", "").unwrap().unwrap();
                let content = event.json().get("content").unwrap();
                serde_json::from_slice(&content.to_canonical_bytes()).unwrap()
            })
            .await;
        assert_eq!(
            tombstone_content["replacement_room"].as_str().unwrap(),
            new_room_id,
            "the old room's tombstone must name the new room"
        );
    }

    /// An unsupported room version is rejected before anything else happens: no tombstone lands
    /// in the old room, and no replacement room is created.
    #[tokio::test]
    async fn upgrade_to_an_unsupported_version_is_rejected_before_any_side_effect() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let created = post_create_room::<MemoryBackend>(
            State(state.clone()),
            requester(alice),
            PermissiveJson(json!({"preset": "public_chat"})),
        )
        .await
        .unwrap();
        let old_room_id = json_body(created).await["room_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let err = post_upgrade::<MemoryBackend>(
            State(state.clone()),
            Path(old_room_id.clone()),
            requester(alice),
            PermissiveJson(json!({"new_version": "not-a-real-version"})),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RoomError::UnsupportedRoomVersion(_)));

        let old_room_id_parsed = RoomId::parse(&old_room_id).unwrap().to_owned();
        let old_handle = state.rooms.get_or_load(&old_room_id_parsed).await.unwrap();
        let tombstoned = old_handle
            .query(|actor| actor.state_event("m.room.tombstone", "").unwrap().is_some())
            .await;
        assert!(!tombstoned, "a rejected upgrade must not send a tombstone");
    }
}
