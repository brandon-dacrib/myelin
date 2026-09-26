//! Membership endpoints: join, leave, forget, invite, kick, ban, unban, knock.
//!
//! # Profile propagation
//!
//! Per the spec, `displayname`/`avatar_url` on an `m.room.member` event are a snapshot of the
//! target user's profile *at the time the event was sent*, not a live reference -- a later
//! profile change does not retroactively edit past membership events. [`extra`] reads the
//! target's current profile (`hs_auth::store::UserRecord::display_name`/`avatar_url`, via
//! `RoomState::auth`'s embedded `AuthState::store`) and merges it into the `m.room.member`
//! content for [`Action::Join`], [`Action::Invite`] and [`Action::Knock`] -- the three actions
//! that put the *target's own* profile into their own membership event. A local user's profile
//! lookup is a synchronous, in-process call to `hs-auth`'s store (both crates already share the
//! same store in `hs serve`'s single-process deployment); a remote user's profile is simply
//! whatever `get_user` returns for them locally, which is `None` today (this crate does not
//! query federation for a remote profile) -- their membership event carries no profile fields,
//! same as before this change.
//!
//! **Rewriting a user's already-sent `m.room.member` event in every room they are currently
//! joined to whenever their profile changes** (Synapse's fuller behavior, via
//! `ProfileHandler.on_profile_update`/`_update_join_states`) **is now implemented**, in
//! `crate::routes::profile` (`PUT /profile/{userId}/displayname`/`avatar_url`, mounted from this
//! crate's router rather than `hs-auth`'s -- see that module's doc comment for why and for the
//! full design). It reuses exactly the mechanism this module already had for a different reason:
//! the join transition table (`crate::membership::TRANSITIONS`) allows [`Action::Join`] again from
//! [`crate::membership::PriorState::Join`] as a harmless re-send
//! (`RoomActor::refresh_own_profile` calls `membership_action` the same way `act_join` below
//! does), so a client that wants its already-joined membership event updated with a fresh profile
//! can *also* still get one for free by calling `POST /rooms/{roomId}/join` again -- both paths
//! converge on the same idempotent re-send.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use hs_http::body::PermissiveJson;
use hs_kv::KvBackend;
use ruma::{RoomId, UserId};
use serde_json::{Value, json};

use crate::error::RoomError;
use crate::membership::Action;
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

fn target_user(body: &Value, requester: &ruma::UserId) -> Result<ruma::OwnedUserId, RoomError> {
    match body.get("user_id").and_then(Value::as_str) {
        Some(s) => UserId::parse(s)
            .map(|u| u.to_owned())
            .map_err(|e| RoomError::BadRequest(format!("invalid user_id: {e}"))),
        None => Ok(requester.to_owned()),
    }
}

/// Builds the extra `m.room.member` content fields beyond `membership` itself: the client-supplied
/// `reason`/`join_authorised_via_users_server`, plus -- for [`Action::Join`], [`Action::Invite`]
/// and [`Action::Knock`] -- the target's current profile. See the module docs for exactly what
/// this does and does not cover.
async fn extra<B: hs_kv::KvBackend + 'static>(
    state: &RoomState<B>,
    action: Action,
    target: &ruma::UserId,
    body: &Value,
) -> Value {
    let mut out = json!({});
    // A join's body *is* the content the client wants on its member event -- the spec's join
    // endpoints take no parameters of their own beyond `reason` and `third_party_signed` -- so
    // whatever else it carries is kept (Complement's "can join a room with custom content", and
    // how a client attaches its own keys to a membership). `third_party_signed` is the one key
    // that is an instruction to the server rather than content, and `membership` is decided by
    // the action, never by the body. Every other action's body is parameters (`user_id`), from
    // which only `reason` belongs on the event.
    if action == Action::Join
        && let Some(object) = body.as_object()
    {
        for (key, value) in object {
            if key != "third_party_signed" && key != "membership" {
                out[key] = value.clone();
            }
        }
    }
    if let Some(reason) = body.get("reason") {
        out["reason"] = reason.clone();
    }
    if let Some(via) = body.get("join_authorised_via_users_server") {
        out["join_authorised_via_users_server"] = via.clone();
    }
    if matches!(action, Action::Join | Action::Invite | Action::Knock) {
        fill_in_profile(state, target, &mut out).await;
    }
    out
}

/// Adds `target`'s current `displayname`/`avatar_url` to `content`, an `m.room.member` event's
/// content in the making. The profile fills in what is not already there; it does not overrule
/// a join that named its own display name for this room. A user this server holds no profile
/// for (a remote one -- see the module docs) is left as they are.
///
/// Also how `crate::routes::create_room` fills in the creator's join and the invitations it
/// sends, which are membership events like any other and used to go out bare: whoever made a
/// room was `@alice:example.org` to everyone in it, in every client, until they next changed
/// their name.
pub(crate) async fn fill_in_profile<B: hs_kv::KvBackend + 'static>(
    state: &RoomState<B>,
    target: &ruma::UserId,
    content: &mut Value,
) {
    let Ok(Some(profile)) = state.auth.store.get_user(target).await else {
        return;
    };
    if let Some(name) = profile.display_name
        && content.get("displayname").is_none()
    {
        content["displayname"] = Value::String(name);
    }
    if let Some(avatar) = profile.avatar_url
        && content.get("avatar_url").is_none()
    {
        content["avatar_url"] = Value::String(avatar);
    }
}

async fn act<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &str,
    sender: ruma::OwnedUserId,
    action: Action,
    target: ruma::OwnedUserId,
    body: &Value,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(room_id)?;
    let handle = state.rooms.get_or_load(&room_id).await?;
    let content = extra(state, action, &target, body).await;
    handle
        .membership(sender, action, target, content, now_ms())
        .await?;
    Ok(Json(json!({})).into_response())
}

/// The servers a client named as candidates to sponsor a join it asked for: every `server_name`
/// (the spec's parameter) and every `via` (the newer spelling, MSC4156) in the query string, in
/// the order given. Read from the raw query rather than through a typed `Query` extractor
/// because the parameter repeats (`?server_name=a&server_name=b`), which the form decoder axum
/// uses does not represent.
fn requested_via(raw_query: Option<&str>) -> Vec<String> {
    let mut via = Vec::new();
    for pair in raw_query.unwrap_or_default().split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "server_name" || key == "via" {
            let value = percent_decode(value);
            if !value.is_empty() && !via.contains(&value) {
                via.push(value);
            }
        }
    }
    via
}

/// Decodes `%XX` escapes and `+` in one query-string value. Lenient: a malformed escape is kept
/// as it was, since the worst outcome is a server name that does not resolve.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Like [`act`], but for the two join endpoints: per the spec, `POST /rooms/{roomId}/join` and
/// `POST /join/{roomIdOrAlias}` respond `{"room_id": "!..."}`, not `{}`. Found by
/// `crates/hs-loadgen`'s `matrix-rust-sdk` scenario: the SDK's `join_room_by_id` deserializes the
/// response strictly and rejected the empty body `act` had been sending on every join
/// (`missing field `room_id``), which every unit test speaking this crate's own dialect had
/// missed because none of them asserted the join response body, only that joining succeeded.
///
/// A room the registry does not hold is joined through federation
/// (`RoomState::remote_join`, when installed): the servers the client named (`via`), or --
/// for a room ID that carries a server name, as every room version before 12 does -- that
/// server, are asked in turn to sponsor the join, and the room becomes resident here. With no
/// hook installed the room is not found, as before.
///
/// So is a room the registry holds but nobody of this server is in any more
/// (`RoomActor::servers_to_join_through`): a copy nobody here is joined to has not received an
/// event since the last one left, so a join made against it is made against the room as it
/// was then. The resident's answer brings the room's current state with it, the same as a first
/// join does. With no hook installed the join is made here regardless, as before.
async fn act_join<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &str,
    mut via: Vec<String>,
    sender: ruma::OwnedUserId,
    body: &Value,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(room_id)?;
    let content = extra(state, Action::Join, &sender, body).await;
    match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => {
            if let Some(remote) = &state.remote_join
                && let Some(servers) = handle.query(|actor| actor.servers_to_join_through()).await
            {
                for server in servers {
                    if !via.contains(&server) {
                        via.push(server);
                    }
                }
                let joined = remote.join(&sender, &room_id, &via, content).await?;
                return Ok(Json(json!({ "room_id": joined })).into_response());
            }
            handle
                .membership(sender.clone(), Action::Join, sender, content, now_ms())
                .await?;
            Ok(Json(json!({ "room_id": room_id })).into_response())
        }
        Err(RoomError::RoomNotFound(_)) if state.remote_join.is_some() => {
            let remote = state
                .remote_join
                .as_ref()
                .expect("checked by the match guard");
            if let Some(server) = room_id.server_name()
                && server != &*state.identity.server_name
                && !via.iter().any(|v| v == server.as_str())
            {
                via.push(server.to_string());
            }
            if via.is_empty() {
                return Err(RoomError::RoomNotFound(room_id.to_string()));
            }
            let joined = remote.join(&sender, &room_id, &via, content).await?;
            Ok(Json(json!({ "room_id": joined })).into_response())
        }
        Err(e) => Err(e),
    }
}

/// `POST /rooms/{roomId}/join`.
pub async fn post_join<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let via = requested_via(raw_query.as_deref());
    act_join(&state, &room_id, via, requester.user_id, &body).await
}

/// `POST /join/{roomIdOrAlias}`. An alias on another server is resolved through that server's
/// directory (`RoomState::remote_join`), and the servers its directory names are added to the
/// candidates for the join.
pub async fn post_join_by_id_or_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id_or_alias): Path<String>,
    RawQuery(raw_query): RawQuery,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let mut via = requested_via(raw_query.as_deref());
    let room_id = if room_id_or_alias.starts_with('!') {
        parse_room_id(&room_id_or_alias)?
    } else {
        let alias = ruma::RoomAliasId::parse(&room_id_or_alias)
            .map_err(|e| RoomError::BadRequest(e.to_string()))?;
        match state.rooms.resolve_alias(&alias)? {
            Some(room_id) => room_id,
            None => match &state.remote_join {
                Some(remote) if alias.server_name() != &*state.identity.server_name => {
                    let (room_id, servers) = remote.resolve_alias(&alias).await?;
                    for server in servers {
                        if !via.contains(&server) {
                            via.push(server);
                        }
                    }
                    if !via.iter().any(|v| v == alias.server_name().as_str()) {
                        via.push(alias.server_name().to_string());
                    }
                    room_id
                }
                _ => return Err(RoomError::RoomNotFound(room_id_or_alias.clone())),
            },
        }
    };
    act_join(&state, room_id.as_str(), via, requester.user_id, &body).await
}

/// `POST /rooms/{roomId}/leave`.
pub async fn post_leave<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let user = requester.user_id.clone();
    act(&state, &room_id, user.clone(), Action::Leave, user, &body).await
}

/// `POST /rooms/{roomId}/forget`. Per the spec
/// (`refs/matrix-spec/data/api/client-server/leaving.yaml`, Apache-2.0): `400 M_UNKNOWN` if the
/// requester is still joined to the room, or if the room does not exist at all (this crate does
/// not distinguish the two in its response, matching the spec's one documented error shape for
/// this endpoint -- see [`RoomError::StillJoined`]'s doc comment). Otherwise marks the room
/// forgotten (`crate::actor::RoomActor::forget`), which `GET .../messages` (`can_read_room`) then
/// refuses outright until the requester rejoins.
pub async fn post_forget<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
) -> Result<Response, RoomError> {
    let room_id = parse_room_id(&room_id)?;
    let handle = match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => handle,
        Err(RoomError::RoomNotFound(_)) => {
            return Err(RoomError::StillJoined(format!(
                "room {room_id} does not exist"
            )));
        }
        Err(e) => return Err(e),
    };
    handle.forget(requester.user_id).await?;
    Ok(Json(json!({})).into_response())
}

/// `POST /rooms/{roomId}/invite`.
pub async fn post_invite<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Invite,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/kick`.
pub async fn post_kick<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Kick,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/ban`.
pub async fn post_ban<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Ban,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/unban`.
pub async fn post_unban<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let target = target_user(&body, &requester.user_id)?;
    act(
        &state,
        &room_id,
        requester.user_id.clone(),
        Action::Unban,
        target,
        &body,
    )
    .await
}

/// `POST /rooms/{roomId}/knock`.
pub async fn post_knock<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let user = requester.user_id.clone();
    act(&state, &room_id, user.clone(), Action::Knock, user, &body).await
}

/// `POST /knock/{roomIdOrAlias}`.
pub async fn post_knock_by_id_or_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id_or_alias): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let room_id = if room_id_or_alias.starts_with('!') {
        parse_room_id(&room_id_or_alias)?
    } else {
        let alias = ruma::RoomAliasId::parse(&room_id_or_alias)
            .map_err(|e| RoomError::BadRequest(e.to_string()))?;
        state
            .rooms
            .resolve_alias(&alias)?
            .ok_or_else(|| RoomError::RoomNotFound(room_id_or_alias.clone()))?
    };
    let user = requester.user_id.clone();
    act(
        &state,
        room_id.as_str(),
        user.clone(),
        Action::Knock,
        user,
        &body,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use axum::http::StatusCode;
    use hs_auth::requester::Requester;
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use ruma::{OwnedRoomId, RoomAliasId, UserId};

    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::registry::RoomRegistry;

    #[test]
    fn requested_via_reads_both_spellings_in_order_without_repeats() {
        let via = requested_via(Some(
            "server_name=a.example&via=b.example&server_name=a.example&server_name=127.0.0.1%3A8448&other=x",
        ));
        assert_eq!(via, vec!["a.example", "b.example", "127.0.0.1:8448"]);
        assert!(requested_via(None).is_empty());
        assert!(requested_via(Some("server_name=")).is_empty());
    }

    #[test]
    fn percent_decode_is_lenient() {
        assert_eq!(percent_decode("a%3Ab+c"), "a:b c");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("trailing%4"), "trailing%4");
    }

    /// One recorded join request: user, room, the `via` list, the member content.
    type RecordedJoin = (String, String, Vec<String>, Value);

    /// Records what the route asked for, and answers as a federation join would.
    #[derive(Default)]
    struct RecordingRemoteJoin {
        joins: Mutex<Vec<RecordedJoin>>,
        resolved: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl crate::remote_join::RemoteJoin for RecordingRemoteJoin {
        async fn join(
            &self,
            user_id: &UserId,
            room_id: &RoomId,
            via: &[String],
            content: Value,
        ) -> Result<OwnedRoomId, RoomError> {
            self.joins.lock().unwrap().push((
                user_id.to_string(),
                room_id.to_string(),
                via.to_vec(),
                content,
            ));
            Ok(room_id.to_owned())
        }

        async fn resolve_alias(
            &self,
            alias: &RoomAliasId,
        ) -> Result<(OwnedRoomId, Vec<String>), RoomError> {
            self.resolved.lock().unwrap().push(alias.to_string());
            Ok((
                RoomId::parse("!resolved:remote.example")
                    .unwrap()
                    .to_owned(),
                vec!["remote.example".to_owned(), "third.example".to_owned()],
            ))
        }
    }

    fn state(remote: Option<Arc<RecordingRemoteJoin>>) -> RoomState<MemoryBackend> {
        let identity = HomeserverIdentity::for_tests("hs1");
        let rooms = Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).unwrap());
        RoomState {
            auth: AuthState::in_memory(),
            rooms,
            identity,
            remote_join: remote.map(|r| r as Arc<dyn crate::remote_join::RemoteJoin>),
        }
    }

    fn alice() -> RoomRequester {
        RoomRequester(Requester::for_user(
            UserId::parse("@alice:hs1").unwrap().to_owned(),
        ))
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn an_unknown_room_is_not_found_without_a_remote_join_hook() {
        let err = post_join::<MemoryBackend>(
            State(state(None)),
            Path("!nowhere:remote.example".to_owned()),
            RawQuery(Some("server_name=remote.example".to_owned())),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err}");
    }

    /// A room this server holds, whose last local member left while a remote member stayed, is
    /// rejoined through that member's server rather than against the stale copy; a room with a
    /// local member still in it is joined here, as always.
    #[tokio::test]
    async fn a_held_room_nobody_here_is_in_is_rejoined_through_a_resident() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        let state = state(Some(remote.clone()));
        let alice_id = UserId::parse("@alice:hs1").unwrap().to_owned();
        let bob_id = UserId::parse("@bob:remote.example").unwrap().to_owned();
        let carol_id = UserId::parse("@carol:hs1").unwrap().to_owned();
        // Alice creates the room here; bob, elsewhere, joins it.
        let handle = state
            .rooms
            .create_room(
                alice_id.clone(),
                crate::actor::CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        handle
            .membership(bob_id.clone(), Action::Join, bob_id.clone(), json!({}), 2)
            .await
            .unwrap();

        // Carol, here, joins while alice is still in the room: made here, no resident asked.
        let response = post_join::<MemoryBackend>(
            State(state.clone()),
            Path(room_id.to_string()),
            RawQuery(None),
            RoomRequester(Requester::for_user(carol_id.clone())),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(remote.joins.lock().unwrap().is_empty());

        // Everybody here leaves; bob stays. Alice's rejoin goes through bob's server.
        handle
            .membership(
                alice_id.clone(),
                Action::Leave,
                alice_id.clone(),
                json!({}),
                3,
            )
            .await
            .unwrap();
        handle
            .membership(
                carol_id.clone(),
                Action::Leave,
                carol_id.clone(),
                json!({}),
                4,
            )
            .await
            .unwrap();
        let response = post_join::<MemoryBackend>(
            State(state.clone()),
            Path(room_id.to_string()),
            RawQuery(Some("server_name=sponsor.example".to_owned())),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let joins = remote.joins.lock().unwrap();
        assert_eq!(joins.len(), 1, "the rejoin went through federation");
        let (user, room, via, _) = &joins[0];
        assert_eq!(user, "@alice:hs1");
        assert_eq!(room, room_id.as_str());
        assert_eq!(via, &["sponsor.example", "remote.example"]);
    }

    #[tokio::test]
    async fn an_unknown_room_is_joined_through_the_servers_the_client_named() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        let response = post_join::<MemoryBackend>(
            State(state(Some(remote.clone()))),
            Path("!nowhere:remote.example".to_owned()),
            RawQuery(Some(
                "server_name=sponsor.example&via=other.example".to_owned(),
            )),
            alice(),
            PermissiveJson(json!({"reason": "curious"})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({"room_id": "!nowhere:remote.example"})
        );
        let joins = remote.joins.lock().unwrap();
        let (user, room, via, content) = &joins[0];
        assert_eq!(user, "@alice:hs1");
        assert_eq!(room, "!nowhere:remote.example");
        // The client's servers first, then the room ID's own server as a last resort.
        assert_eq!(via, &["sponsor.example", "other.example", "remote.example"]);
        assert_eq!(content["reason"], "curious");
        assert!(
            content.get("membership").is_none(),
            "membership is the actor's to set"
        );
    }

    #[tokio::test]
    async fn the_room_ids_server_is_enough_to_try_when_the_client_named_none() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        post_join::<MemoryBackend>(
            State(state(Some(remote.clone()))),
            Path("!nowhere:remote.example".to_owned()),
            RawQuery(None),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(remote.joins.lock().unwrap()[0].2, vec!["remote.example"]);
    }

    #[tokio::test]
    async fn a_remote_alias_is_resolved_through_its_server_and_joined_via_its_servers() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        let response = post_join_by_id_or_alias::<MemoryBackend>(
            State(state(Some(remote.clone()))),
            Path("#somewhere:remote.example".to_owned()),
            RawQuery(None),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(
            body_json(response).await,
            json!({"room_id": "!resolved:remote.example"})
        );
        assert_eq!(
            remote.resolved.lock().unwrap().as_slice(),
            ["#somewhere:remote.example"]
        );
        assert_eq!(
            remote.joins.lock().unwrap()[0].2,
            vec!["remote.example", "third.example"]
        );
    }

    #[tokio::test]
    async fn an_unknown_local_alias_is_not_resolved_remotely() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        let err = post_join_by_id_or_alias::<MemoryBackend>(
            State(state(Some(remote.clone()))),
            Path("#nowhere:hs1".to_owned()),
            RawQuery(None),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RoomError::RoomNotFound(_)), "{err}");
        assert!(remote.resolved.lock().unwrap().is_empty());
    }
}
