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

/// The `m.room.member` content key naming who authorised a restricted join. Only the server
/// decides it (`RoomActor::restricted_join`, or the resident in `make_join`); whatever a client
/// puts there is dropped, as Synapse's `update_membership` does, so a join -> join profile change
/// carrying a stale or bogus value is not refused for it.
pub const AUTHORISING_USER: &str = "join_authorised_via_users_server";

/// Builds the extra `m.room.member` content fields beyond `membership` itself: the client-supplied
/// `reason` (never `join_authorised_via_users_server`, see [`AUTHORISING_USER`]), plus -- for [`Action::Join`], [`Action::Invite`]
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
            if key != "third_party_signed" && key != "membership" && key != AUTHORISING_USER {
                out[key] = value.clone();
            }
        }
    }
    if let Some(reason) = body.get("reason") {
        out["reason"] = reason.clone();
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

/// Performs a membership `action` in a room this server holds.
///
/// Two actions go through another server instead, when the federation hook is installed
/// (`RoomState::remote_join`):
///
/// - A user's own **leave** of a room no user of this server is joined to
///   (`RoomActor::servers_to_join_through`) -- rejecting an invite from another server, or
///   withdrawing a knock. This server holds nothing current to author the leave against, so it
///   asks a server in the room for a template (`make_leave`/`send_leave`). If none will, an
///   invite or knock is rejected here alone (`RoomActorHandle::reject_out_of_band`).
/// - An **invite** of a user of another server: the event is built and signed here but not
///   persisted, sent to the invitee's server with the room's stripped state (`PUT /invite`),
///   and the event that comes back co-signed is what goes into the room. An invitee's server
///   that refuses means no invite.
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
    if let Some(remote) = &state.remote_join {
        if action == Action::Leave
            && sender == target
            && let Some(servers) = handle.query(|actor| actor.servers_to_join_through()).await
        {
            if let Err(error) = remote
                .leave(&sender, &room_id, &servers, content.clone())
                .await
            {
                // No server in the room took the leave. An invite or knock is still rejected
                // here, as Synapse does: the leave goes nowhere, but the user's clients can put
                // the room behind them. Anything else (nothing to reject) keeps the error.
                tracing::info!(%room_id, user = %sender, %error, "no server in the room took the leave; rejecting locally");
                if handle
                    .reject_out_of_band(sender, content, now_ms())
                    .await
                    .is_err()
                {
                    return Err(error);
                }
            }
            return Ok(Json(json!({})).into_response());
        }
        if action == Action::Invite && target.server_name() != &*state.identity.server_name {
            invite_remote(remote.as_ref(), &handle, sender, target, content).await?;
            return Ok(Json(json!({})).into_response());
        }
    }
    handle
        .membership(sender, action, target, content, now_ms())
        .await?;
    Ok(Json(json!({})).into_response())
}

/// Invites `target`, a user of another server, to the room `handle` is for: the invite is built
/// and signed here but not persisted, sent to the invitee's server with the room's stripped
/// state (`PUT /invite`), and the event that comes back co-signed is what goes into the room.
/// An invitee's server that refuses means no invite.
///
/// Also how `crate::routes::create_room` sends the invitations of its `invite` list that are
/// for users of other servers, once the room exists.
pub(crate) async fn invite_remote<B: KvBackend + 'static>(
    remote: &dyn crate::remote_join::RemoteJoin,
    handle: &crate::actor::RoomActorHandle<B>,
    sender: ruma::OwnedUserId,
    target: ruma::OwnedUserId,
    content: Value,
) -> Result<(), RoomError> {
    let inviter = sender.to_string();
    let event = handle
        .build_membership_event(sender, Action::Invite, target, content, now_ms())
        .await?;
    let (room_version, stripped) = handle
        .query(move |actor| {
            (
                actor.room_version().clone(),
                actor.stripped_state(&[inviter.as_str()]),
            )
        })
        .await;
    let cosigned = remote.invite(&room_version, &event, stripped?).await?;
    handle.accept_remote_event(cosigned).await?;
    Ok(())
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
///
/// A guest's join carries `kind: guest` and is made only into a room whose guest access is
/// `can_join` ([`refuse_guest_where_guests_may_not_join`]).
async fn act_join<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &str,
    via: Vec<String>,
    requester: &hs_auth::requester::Requester,
    body: &Value,
) -> Result<Response, RoomError> {
    let sender = requester.user_id.clone();
    let room_id = parse_room_id(room_id)?;
    let mut content = extra(state, Action::Join, &sender, body).await;
    if requester.is_guest {
        // How the room (and Synapse's `kick_guest_users`) tells this server's guests apart
        // from its members when guest access is withdrawn.
        content["kind"] = Value::String("guest".to_owned());
    }
    // `third_party_signed`: the joiner claims a third-party invitation; it becomes an invite
    // first (`crate::third_party_invite::exchange`), as Synapse does, and the join follows it.
    if let Some(signed) = body.get("third_party_signed") {
        if signed.get("mxid").and_then(Value::as_str) != Some(sender.as_str()) {
            return Err(RoomError::Forbidden(
                "third_party_signed names somebody else".into(),
            ));
        }
        crate::third_party_invite::exchange(state, &room_id, signed).await?;
    }
    let joined = join_room(state, &room_id, via, requester, content).await?;
    if requester.is_guest {
        refuse_guest_where_guests_may_not_join(state, requester, &room_id).await?;
    }
    Ok(joined)
}

/// The spec's "Guest Access" module: a guest may join only a room whose `m.room.guest_access`
/// is `can_join`. The room's state is known only once the join is made when the room is joined
/// through another server, so the check comes after the join: a guest that should not be there
/// leaves at once and is refused `403`, the same answer a room held here gives. A room held here
/// that does not let guests in is refused before anything is sent ([`join_room`]).
async fn refuse_guest_where_guests_may_not_join<B: KvBackend + 'static>(
    state: &RoomState<B>,
    requester: &hs_auth::requester::Requester,
    room_id: &ruma::RoomId,
) -> Result<(), RoomError> {
    let handle = state.rooms.get_or_load(room_id).await?;
    if handle.query(|actor| actor.guests_may_join()).await? {
        return Ok(());
    }
    count_guest_refusal();
    tracing::info!(%room_id, user = %requester.user_id, "a guest joined a room through another server whose guest access does not let guests in; leaving it");
    let user = requester.user_id.clone();
    if let Err(error) = act(
        state,
        room_id.as_str(),
        user.clone(),
        Action::Leave,
        user,
        &json!({}),
    )
    .await
    {
        tracing::warn!(%room_id, user = %requester.user_id, %error, "could not leave a room a guest may not be in");
    }
    Err(RoomError::Forbidden(GUEST_ACCESS_FORBIDDEN.to_owned()))
}

/// The refusal a guest gets for a room whose guest access does not let guests in.
const GUEST_ACCESS_FORBIDDEN: &str = "Guest access is not allowed in this room";

/// Counts a guest refused a room by its `m.room.guest_access`.
fn count_guest_refusal() {
    crate::moderation::count_guest_join_refused();
}

/// The join itself, wherever it has to be made: here, or through another server. See
/// [`act_join`].
async fn join_room<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &ruma::RoomId,
    mut via: Vec<String>,
    requester: &hs_auth::requester::Requester,
    content: Value,
) -> Result<Response, RoomError> {
    let sender = requester.user_id.clone();
    let room_id = room_id.to_owned();
    match state.rooms.get_or_load(&room_id).await {
        Ok(handle) => {
            if let Some(remote) = &state.remote_join
                && let Some((servers, shell)) = handle
                    .query(|actor| {
                        actor.servers_to_join_through().map(|servers| {
                            let shell = matches!(actor.state_event("m.room.create", ""), Ok(None));
                            (servers, shell)
                        })
                    })
                    .await
            {
                // The servers the client named are the ones asked, as Synapse does (Complement's
                // `TestRestrictedRoomsRemoteJoinFailOver`: a join naming only a server that
                // cannot authorise it fails). Only for a room held through an invite or a knock
                // are the servers it came through added (Synapse adds the inviter's); only when
                // the client named nobody is this server's own guess used.
                let client_named = !via.is_empty();
                if shell || !client_named {
                    for server in servers {
                        if !via.contains(&server) {
                            via.push(server);
                        }
                    }
                }
                if !client_named {
                    for server in allowed_rooms_servers(state, &handle).await {
                        if !via.contains(&server) && server != state.identity.server_name.as_str() {
                            via.push(server);
                        }
                    }
                }
                crate::moderation::check_join_limit(state, requester, true)?;
                let joined = remote.join(&sender, &room_id, &via, content).await?;
                return Ok(Json(json!({ "room_id": joined })).into_response());
            }
            let mut content = content;
            if content.get("join_authorised_via_users_server").is_none() {
                let user = sender.clone();
                let plan = handle
                    .query(move |actor| actor.restricted_join(&user))
                    .await?;
                if let Some(plan) = plan {
                    match plan.local_authoriser {
                        Some(authoriser) => {
                            // Named only when the user is in an allowed room: otherwise the
                            // join is refused by the auth rules, as it should be.
                            if joined_to_any(state, &plan.allowed_rooms, &sender).await {
                                tracing::debug!(%room_id, user = %sender, %authoriser, "authorising a restricted join locally");
                                content["join_authorised_via_users_server"] =
                                    Value::String(authoriser.to_string());
                            }
                        }
                        None => {
                            // Nobody here may invite, so this server cannot vouch for the join:
                            // a server whose users can has to (Synapse's
                            // `_should_perform_remote_join`), and the client's `via` after them.
                            if let Some(remote) = &state.remote_join
                                && !plan.inviting_servers.is_empty()
                            {
                                let mut through = plan.inviting_servers;
                                for server in via {
                                    if !through.contains(&server) {
                                        through.push(server);
                                    }
                                }
                                tracing::info!(%room_id, user = %sender, "no user of this server may authorise the restricted join; joining through another server");
                                crate::moderation::check_join_limit(state, requester, true)?;
                                let joined =
                                    remote.join(&sender, &room_id, &through, content).await?;
                                return Ok(Json(json!({ "room_id": joined })).into_response());
                            }
                        }
                    }
                }
            }
            if requester.is_guest && !handle.query(|actor| actor.guests_may_join()).await? {
                count_guest_refusal();
                tracing::debug!(%room_id, user = %sender, "refused a guest a room whose guest access does not let guests in");
                return Err(RoomError::Forbidden(GUEST_ACCESS_FORBIDDEN.to_owned()));
            }
            crate::moderation::check_join_limit(state, requester, false)?;
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
            // A room ID's server is asked only when the client named nobody (Synapse asks only
            // the servers named; a version 12 room ID names none).
            if via.is_empty()
                && let Some(server) = room_id.server_name()
                && server != &*state.identity.server_name
            {
                via.push(server.to_string());
            }
            if via.is_empty() {
                return Err(RoomError::RoomNotFound(room_id.to_string()));
            }
            crate::moderation::check_join_limit(state, requester, true)?;
            let joined = remote.join(&sender, &room_id, &via, content).await?;
            Ok(Json(json!({ "room_id": joined })).into_response())
        }
        Err(e) => Err(e),
    }
}

/// Whether `user` is joined to any of `rooms`, as this server holds them. A room this server
/// does not hold has no member of this server, so it cannot be one `user` is in.
/// The servers of the rooms `handle`'s join rules allow, as far as this server knows them
/// (`RoomActor::known_allowed_rooms`: from the room's own join rules, or from the stripped state
/// a local user's invite or knock arrived with): each allowed room's `via`, then the servers of
/// its joined members if this server holds it. A server in an allowed room can check the joining
/// user's membership there, and is often in the restricted room too: where a restricted join
/// the client named no server for is sent, after the servers the room itself suggests.
async fn allowed_rooms_servers<B: KvBackend + 'static>(
    state: &RoomState<B>,
    handle: &crate::actor::RoomActorHandle<B>,
) -> Vec<String> {
    let allowed = handle.query(|actor| actor.known_allowed_rooms()).await;
    let mut servers: Vec<String> = Vec::new();
    for (allowed_room, hints) in allowed {
        let mut candidates = hints;
        if let Ok(allowed_handle) = state.rooms.get_or_load(&allowed_room).await {
            candidates.extend(
                allowed_handle
                    .query(|actor| {
                        actor
                            .joined_members()
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|member| member.header().state_key.as_deref())
                            .filter_map(|key| ruma::UserId::parse(key).ok())
                            .map(|user| user.server_name().to_string())
                            .collect::<Vec<_>>()
                    })
                    .await,
            );
        }
        for server in candidates {
            if !servers.contains(&server) {
                servers.push(server);
            }
        }
    }
    servers
}

async fn joined_to_any<B: KvBackend + 'static>(
    state: &RoomState<B>,
    rooms: &[ruma::OwnedRoomId],
    user: &ruma::UserId,
) -> bool {
    for room in rooms {
        let Ok(handle) = state.rooms.get_or_load(room).await else {
            continue;
        };
        let who = user.to_owned();
        if handle.query(move |actor| actor.is_joined(&who)).await {
            return true;
        }
    }
    false
}

/// `POST /rooms/{roomId}/join`.
pub async fn post_join<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    crate::moderation::refuse_if_suspended(&requester)?;
    let via = requested_via(raw_query.as_deref());
    act_join(&state, &room_id, via, &requester, &body).await
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
    crate::moderation::refuse_if_suspended(&requester)?;
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
    act_join(&state, room_id.as_str(), via, &requester, &body).await
}

/// `POST /rooms/{roomId}/leave`.
pub async fn post_leave<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    let user = requester.user_id.clone();
    refuse_rejecting_server_notices(&state, &room_id, &user).await?;
    act(&state, &room_id, user.clone(), Action::Leave, user, &body).await
}

/// A server-notices room's recipient may leave it once they have joined, but not reject the
/// invitation to it: the notice would go unseen, and the next one would re-invite them to the
/// same room anyway. The room is recognised by who created it, so no other room can claim to be
/// one by what it says about itself.
async fn refuse_rejecting_server_notices<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &str,
    user: &ruma::UserId,
) -> Result<(), RoomError> {
    let Some(notices_user) = state.rooms.server_notices_user().map(ToOwned::to_owned) else {
        return Ok(());
    };
    let Ok(room_id) = ruma::RoomId::parse(room_id) else {
        return Ok(());
    };
    let Ok(handle) = state.rooms.get_or_load(&room_id).await else {
        return Ok(());
    };
    let user = user.to_owned();
    let refused = handle
        .query(move |actor| {
            let created_by_notices = actor
                .state_event("m.room.create", "")
                .ok()
                .flatten()
                .is_some_and(|e| e.header().sender == notices_user);
            let invited = actor
                .state_event("m.room.member", user.as_str())
                .ok()
                .flatten()
                .and_then(|e| {
                    e.json()
                        .get("content")
                        .and_then(|c| c.as_object())
                        .and_then(|c| c.get("membership"))
                        .and_then(|m| m.as_str())
                        .map(str::to_owned)
                })
                .as_deref()
                == Some("invite");
            created_by_notices && invited
        })
        .await;
    if refused {
        return Err(RoomError::CannotLeaveServerNoticeRoom);
    }
    Ok(())
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
    crate::moderation::refuse_if_suspended(&requester)?;
    // A shadow-banned inviter is told the invitation was sent; nobody is invited.
    if requester.shadow_banned {
        crate::moderation::note_shadowed(&requester, "invite");
        return Ok(Json(json!({})).into_response());
    }
    // An invite by email address (or another third-party identifier) rather than by user ID.
    // Without this, the body's missing `user_id` made it an invite of the inviter themselves.
    if crate::third_party_invite::is_third_party(&body) {
        let room_id = parse_room_id(&room_id)?;
        crate::third_party_invite::invite(&state, &room_id, &requester.user_id, &body).await?;
        return Ok(Json(json!({})).into_response());
    }
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
    crate::moderation::refuse_if_suspended(&requester)?;
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
    crate::moderation::refuse_if_suspended(&requester)?;
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
    crate::moderation::refuse_if_suspended(&requester)?;
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

/// A knock, through federation when this server cannot make it: exactly the cases a join goes
/// through federation in ([`act_join`]) -- a room not held here at all, or one no user of this
/// server is joined to -- asking the servers the client named (`via`), the room ID's own, and
/// whoever this server knows to be in the room. The resident's answer (the accepted knock and
/// the room's stripped state) is recorded here, and the user's `/sync` shows the knock.
async fn act_knock<B: KvBackend + 'static>(
    state: &RoomState<B>,
    room_id: &RoomId,
    mut via: Vec<String>,
    sender: ruma::OwnedUserId,
    body: &Value,
) -> Result<Response, RoomError> {
    let content = extra(state, Action::Knock, &sender, body).await;
    let handle = match state.rooms.get_or_load(room_id).await {
        Ok(handle) => Some(handle),
        Err(RoomError::RoomNotFound(_)) if state.remote_join.is_some() => None,
        Err(e) => return Err(e),
    };
    if let Some(handle) = &handle {
        let through = match &state.remote_join {
            Some(_) => handle.query(|actor| actor.servers_to_join_through()).await,
            None => None,
        };
        match through {
            Some(servers) => {
                for server in servers {
                    if !via.contains(&server) {
                        via.push(server);
                    }
                }
            }
            None => {
                handle
                    .membership(sender.clone(), Action::Knock, sender, content, now_ms())
                    .await?;
                return Ok(Json(json!({ "room_id": room_id })).into_response());
            }
        }
    }
    let Some(remote) = &state.remote_join else {
        return Err(RoomError::RoomNotFound(room_id.to_string()));
    };
    if let Some(server) = room_id.server_name()
        && server != &*state.identity.server_name
        && !via.iter().any(|v| v == server.as_str())
    {
        via.push(server.to_string());
    }
    if via.is_empty() {
        return Err(RoomError::RoomNotFound(room_id.to_string()));
    }
    let knocked = remote.knock(&sender, room_id, &via, content).await?;
    Ok(Json(json!({ "room_id": knocked })).into_response())
}

/// `POST /rooms/{roomId}/knock`.
pub async fn post_knock<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    crate::moderation::refuse_if_suspended(&requester)?;
    let room_id = parse_room_id(&room_id)?;
    let via = requested_via(raw_query.as_deref());
    act_knock(&state, &room_id, via, requester.user_id, &body).await
}

/// `POST /knock/{roomIdOrAlias}`. An alias on another server is resolved through that server's
/// directory, as for a join.
pub async fn post_knock_by_id_or_alias<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id_or_alias): Path<String>,
    RawQuery(raw_query): RawQuery,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    crate::moderation::refuse_if_suspended(&requester)?;
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
    act_knock(&state, &room_id, via, requester.user_id, &body).await
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

    fn guest() -> RoomRequester {
        let mut requester = Requester::for_user(UserId::parse("@guest:hs1").unwrap().to_owned());
        requester.is_guest = true;
        RoomRequester(requester)
    }

    fn membership_of(
        actor: &crate::actor::RoomActor<MemoryBackend>,
        user: &str,
    ) -> (Option<String>, Option<String>) {
        let event = actor.state_event("m.room.member", user).unwrap();
        let content = event.map(|e| e.json().get("content").cloned());
        let field = |key: &str| {
            content
                .clone()
                .flatten()
                .and_then(|c| c.as_object().and_then(|o| o.get(key).cloned()))
                .and_then(|v| v.as_str().map(str::to_owned))
        };
        (field("membership"), field("kind"))
    }

    /// The spec's "Guest Access" module: a guest joins only a room whose `m.room.guest_access`
    /// is `can_join`, and is made to leave when it stops being.
    #[tokio::test]
    async fn a_guest_joins_only_while_guest_access_is_can_join_and_leaves_when_it_is_withdrawn() {
        let state = state(None);
        let alice_id = UserId::parse("@alice:hs1").unwrap().to_owned();
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
        let join = |state: RoomState<MemoryBackend>| {
            post_join::<MemoryBackend>(
                State(state),
                Path(room_id.to_string()),
                RawQuery(None),
                guest(),
                PermissiveJson(json!({})),
            )
        };

        // A public room is not a guest room until it says so.
        let err = join(state.clone()).await.unwrap_err();
        assert!(matches!(err, RoomError::Forbidden(_)), "{err}");
        assert_eq!(
            handle
                .query(|actor| membership_of(actor, "@guest:hs1"))
                .await
                .0,
            None
        );

        let set_guest_access = |value: &'static str, at: i64| {
            let handle = handle.clone();
            let alice_id = alice_id.clone();
            async move {
                handle
                    .send_event(
                        alice_id,
                        "m.room.guest_access".to_owned(),
                        Some(String::new()),
                        json!({"guest_access": value}),
                        None,
                        at,
                    )
                    .await
                    .unwrap();
            }
        };
        set_guest_access("can_join", 2).await;
        let response = join(state.clone()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            handle
                .query(|actor| membership_of(actor, "@guest:hs1"))
                .await,
            (Some("join".to_owned()), Some("guest".to_owned()))
        );

        // A full member is not a guest, and stays.
        let bob = UserId::parse("@bob:hs1").unwrap().to_owned();
        handle
            .membership(bob.clone(), Action::Join, bob.clone(), json!({}), 3)
            .await
            .unwrap();

        set_guest_access("forbidden", 4).await;
        assert_eq!(
            handle
                .query(|actor| membership_of(actor, "@guest:hs1"))
                .await
                .0
                .as_deref(),
            Some("leave")
        );
        assert_eq!(
            handle
                .query(|actor| membership_of(actor, "@bob:hs1"))
                .await
                .0
                .as_deref(),
            Some("join")
        );
    }

    /// A guest joined through another server to a room that does not let guests in leaves it
    /// again and is refused, like a room held here.
    #[tokio::test]
    async fn a_guest_joined_through_another_server_to_a_room_without_guest_access_leaves_it() {
        let remote = Arc::new(RecordingRemoteJoin::default());
        let state = state(Some(remote.clone()));
        let err = post_join::<MemoryBackend>(
            State(state.clone()),
            Path("!nowhere:remote.example".to_owned()),
            RawQuery(Some("server_name=remote.example".to_owned())),
            guest(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap_err();
        // The recording hook makes no room here, so the check finds none to let the guest in.
        assert!(
            matches!(err, RoomError::Forbidden(_) | RoomError::RoomNotFound(_)),
            "{err}"
        );
        let joins = remote.joins.lock().unwrap();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].3["kind"], "guest");
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

        // Everybody here leaves; bob stays. Alice's rejoin names nobody, so it goes through
        // bob's server; carol's names a server, and only that one is asked.
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
            RawQuery(None),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = post_join::<MemoryBackend>(
            State(state.clone()),
            Path(room_id.to_string()),
            RawQuery(Some("server_name=sponsor.example".to_owned())),
            RoomRequester(Requester::for_user(carol_id.clone())),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let joins = remote.joins.lock().unwrap();
        assert_eq!(joins.len(), 2, "both rejoins went through federation");
        let (user, room, via, _) = &joins[0];
        assert_eq!(user, "@alice:hs1");
        assert_eq!(room, room_id.as_str());
        assert_eq!(via, &["remote.example"]);
        let (user, _, via, _) = &joins[1];
        assert_eq!(user, "@carol:hs1");
        assert_eq!(via, &["sponsor.example"]);
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
        // Only the client's servers: the room ID's own server is asked when it named none.
        assert_eq!(via, &["sponsor.example", "other.example"]);
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

    /// A server-notices room's recipient cannot reject the invitation, can leave once joined,
    /// and a room anyone else created that merely invites them is theirs to reject as usual.
    #[tokio::test]
    async fn a_server_notices_invitation_cannot_be_rejected_but_the_room_can_be_left() {
        let state = state(None);
        let notices = UserId::parse("@_server:hs1").unwrap().to_owned();
        let alice_id = UserId::parse("@alice:hs1").unwrap().to_owned();
        let carol_id = UserId::parse("@carol:hs1").unwrap().to_owned();
        state.rooms.install_server_notices_user(notices.clone());

        let mut rooms = Vec::new();
        for creator in [notices, carol_id] {
            let handle = state
                .rooms
                .create_room(
                    creator,
                    crate::actor::CreateRoomRequest {
                        preset: Some("private_chat".to_owned()),
                        invite: vec![alice_id.clone()],
                        ..Default::default()
                    },
                    1,
                )
                .await
                .unwrap();
            rooms.push(handle.query(|actor| actor.room_id().to_owned()).await);
        }
        let (notice_room, other_room) = (rooms[0].clone(), rooms[1].clone());

        let err = post_leave::<MemoryBackend>(
            State(state.clone()),
            Path(notice_room.to_string()),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RoomError::CannotLeaveServerNoticeRoom),
            "{err}"
        );
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            body_json(response).await["errcode"],
            "M_CANNOT_LEAVE_SERVER_NOTICE_ROOM"
        );

        // Any other room's invitation is rejected as usual.
        let response = post_leave::<MemoryBackend>(
            State(state.clone()),
            Path(other_room.to_string()),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Joined, alice may leave the notices room.
        post_join::<MemoryBackend>(
            State(state.clone()),
            Path(notice_room.to_string()),
            RawQuery(None),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        let response = post_leave::<MemoryBackend>(
            State(state.clone()),
            Path(notice_room.to_string()),
            alice(),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
