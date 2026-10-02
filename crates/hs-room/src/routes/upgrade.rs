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
use hs_model::Event;
use hs_model::room_version::{RoomIdFormat, RoomVersionRules};
use ruma::{OwnedUserId, RoomId, RoomVersionId};
use serde_json::{Value, json};

use crate::actor::{CreateRoomRequest, InitialStateEvent, RoomActorHandle};
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

/// How many opaque room IDs [`post_upgrade`] mints for an upgrade to a room version 1-11 before
/// it gives up on finding one that hashes to a room shard this replica owns: the bound
/// `RoomActor::create_placed` uses at the default 256 room shards. A mint is a random string and
/// a hash, so running out costs a few milliseconds.
const MAX_OPAQUE_ID_ATTEMPTS: u32 = crate::actor::MAX_ID_ATTEMPTS_PER_SHARD * 256;

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

/// The old room's power levels as the replacement room's override, translated across the
/// change in how creators hold power (MSC4289, room version 12):
///
/// - **To version 12 or later.** A room's creators hold their power implicitly, and an
///   `m.room.power_levels` naming any of them in `users` is rejected by the auth rules, so the
///   replacement's creators (`new_creators`: the upgrading user and every
///   `additional_creators` entry) are taken out of it.
/// - **From version 12 or later to an earlier one.** The old room's creators (`old_creators`)
///   had unlimited power and no `users` entry; in the replacement their power is what `users`
///   says, so each is given 100, the most an earlier version expresses. Without it the
///   upgrading user would create a room in which it had no power to send the room's own
///   initial state.
/// - Otherwise the content is carried over as it is.
fn power_levels_for_replacement(
    mut content: Value,
    rules: &RoomVersionRules,
    new_creators: &[OwnedUserId],
    old_privileged_creators: bool,
    old_creators: &[OwnedUserId],
) -> Value {
    let to_privileged = rules.explicitly_privilege_room_creators;
    if !to_privileged && !old_privileged_creators {
        return content;
    }
    let Some(obj) = content.as_object_mut() else {
        return content;
    };
    let users = obj
        .entry("users")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(users) = users.as_object_mut() else {
        return content;
    };
    if to_privileged {
        for creator in new_creators {
            users.remove(creator.as_str());
        }
    } else {
        for creator in old_creators {
            let current = users
                .get(creator.as_str())
                .and_then(Value::as_i64)
                .unwrap_or(0);
            users.insert(creator.to_string(), json!(current.max(100)));
        }
    }
    content
}

/// The power levels the replacement room is created with, and the ones to send once its
/// initial state is in: the upgrader needs enough power to send the copied state (the old
/// room's `events` map may put `m.room.encryption` or the history visibility at 100 while the
/// upgrader is a moderator at 50 who was only allowed to send the tombstone), so when
/// `translated` ranks them below 100 in a version where the creator's power is explicit they
/// are lifted to 100 for the creation, and `translated` itself is sent afterwards to put them
/// back (Synapse's `_upgrade_room`, matrix-org/synapse#6632). Returns `(first, None)` when no
/// lift is needed: the upgrader already holds 100, or the version privileges creators and the
/// `users` map cannot name them.
fn lift_upgrader(
    translated: Value,
    rules: &RoomVersionRules,
    upgrader: &OwnedUserId,
) -> (Value, Option<Value>) {
    if rules.explicitly_privilege_room_creators {
        return (translated, None);
    }
    let users_default = translated
        .get("users_default")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let current = translated
        .get("users")
        .and_then(Value::as_object)
        .and_then(|users| users.get(upgrader.as_str()))
        .and_then(Value::as_i64)
        .unwrap_or(users_default);
    if current >= 100 {
        return (translated, None);
    }
    let mut first = translated.clone();
    if let Some(obj) = first.as_object_mut() {
        let users = obj
            .entry("users")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(users) = users.as_object_mut() {
            users.insert(upgrader.to_string(), json!(100));
        }
    }
    (first, Some(translated))
}

/// Moves the old room's entry in this server's room directory to its replacement: when the old
/// room is published, it is unpublished and the replacement published in its place. Returns
/// whether anything moved. Best-effort, logged: the directory is a server-local fact beside the
/// rooms, and an upgrade that has already happened is not undone for it.
fn move_directory_entry<B: KvBackend>(
    state: &RoomState<B>,
    old_room_id: &RoomId,
    new_room_id: &RoomId,
) -> bool {
    match state.rooms.is_directory_public(old_room_id) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(e) => {
            tracing::warn!(%old_room_id, error = %e, "could not read whether an upgraded room was in the room directory");
            return false;
        }
    }
    if let Err(e) = state.rooms.set_directory_visibility(new_room_id, true) {
        tracing::warn!(%old_room_id, %new_room_id, error = %e, "could not publish an upgraded room's replacement in the room directory");
        return false;
    }
    if let Err(e) = state.rooms.set_directory_visibility(old_room_id, false) {
        tracing::warn!(%old_room_id, %new_room_id, error = %e, "could not take an upgraded room out of the room directory");
    }
    true
}

/// What this server carries from a room to its replacement when one of its users joins the
/// replacement (`POST /join`): the local aliases it hosts for the old room, and the old
/// room's place in its room directory. The upgrade itself does both for the server that ran
/// it; this is for every other server with a user in the old room, whose directory and
/// aliases still name the old room until one of its users follows the tombstone (Synapse's
/// `transfer_room_state_on_room_upgrade`, run from its join handler). Nothing happens unless
/// the joined room's `m.room.create` names a predecessor this server holds, and nothing it
/// does fails the join: every step is logged on failure. Returns how many aliases moved and
/// whether the directory entry did.
pub(crate) async fn transfer_on_join<B: KvBackend + 'static>(
    state: &RoomState<B>,
    new_room_id: &RoomId,
    joiner: &OwnedUserId,
) -> (usize, bool) {
    let Ok(new_handle) = state.rooms.get_or_load(new_room_id).await else {
        return (0, false);
    };
    let Some(old_room_id) = new_handle.query(|actor| actor.predecessor_room_id()).await else {
        return (0, false);
    };
    let Ok(old_handle) = state.rooms.get_or_load(&old_room_id).await else {
        return (0, false);
    };
    let aliases = old_handle
        .query(|actor| actor.list_aliases().unwrap_or_default())
        .await;
    let mut moved = 0usize;
    for alias in &aliases {
        let Ok(alias_id) = <&ruma::RoomAliasId>::try_from(alias.as_str()) else {
            continue;
        };
        let owned = alias_id.to_owned();
        if let Err(e) = old_handle
            .query(move |actor| actor.remove_alias(&owned))
            .await
        {
            tracing::warn!(%old_room_id, %new_room_id, alias, error = %e, "could not take an alias off an upgraded room");
            continue;
        }
        let owned = alias_id.to_owned();
        let creator = joiner.clone();
        match new_handle
            .query(move |actor| actor.create_alias(&owned, &creator))
            .await
        {
            Ok(()) => moved += 1,
            Err(e) => {
                tracing::warn!(%old_room_id, %new_room_id, alias, error = %e, "could not move an upgraded room's alias to its replacement")
            }
        }
    }
    let directory_moved = move_directory_entry(state, &old_room_id, new_room_id);
    if moved > 0 || directory_moved {
        tracing::info!(
            %old_room_id,
            %new_room_id,
            user = %joiner,
            aliases_moved = moved,
            directory_moved,
            "a user joined an upgraded room's replacement; this server's aliases and directory entry followed it"
        );
    }
    (moved, directory_moved)
}

/// The request body's `additional_creators` (client-server API v1.16, room version 12 and later),
/// every entry a user ID; empty when the field is absent.
fn additional_creators(body: &Value) -> Result<Vec<OwnedUserId>, RoomError> {
    let Some(value) = body.get("additional_creators") else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| RoomError::BadRequest("additional_creators must be an array".into()))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .and_then(|s| ruma::UserId::parse(s).ok())
                .ok_or_else(|| {
                    RoomError::BadRequest(format!(
                        "additional_creators entry {item} is not a user ID"
                    ))
                })
        })
        .collect()
}

/// An opaque room ID (room versions 1-11) for the replacement room that hashes to a room shard
/// this replica owns (decision 0020). The tombstone names it before the room exists, so it must
/// be an ID this replica can then create the room under: `hs-cli`'s shard gate forwards an
/// upgrade to the owner of the *old* room, and a fenced create refuses an ID whose shard this
/// replica does not hold, which would leave the tombstone naming a room that was never made.
fn placed_opaque_room_id<B: KvBackend>(
    state: &RoomState<B>,
) -> Result<ruma::OwnedRoomId, RoomError> {
    for _ in 0..MAX_OPAQUE_ID_ATTEMPTS {
        let id = RoomId::new_v1(&state.identity.server_name);
        if state.rooms.owns_room(&id) {
            return Ok(id);
        }
    }
    tracing::warn!(
        attempts = MAX_OPAQUE_ID_ATTEMPTS,
        "no replacement room id for an upgrade hashed to a room shard this replica owns; \
         refusing the upgrade so that the client retries (decision 0020)"
    );
    Err(RoomError::Fenced(format!(
        "no replacement room id hashed to a room shard this replica owns after \
         {MAX_OPAQUE_ID_ATTEMPTS} attempts"
    )))
}

/// Sends the tombstone into the old room, naming `replacement_room`: spec step 5, and (through
/// the ordinary event-authorization path, which answers [`RoomError::Forbidden`]) the
/// authoritative form of step 1's permission check.
async fn send_tombstone<B: KvBackend + 'static>(
    old_handle: &RoomActorHandle<B>,
    sender: &OwnedUserId,
    replacement_room: &RoomId,
    now: i64,
) -> Result<Event, RoomError> {
    old_handle
        .send_event(
            sender.clone(),
            "m.room.tombstone".to_owned(),
            Some(String::new()),
            json!({
                "body": "This room has been replaced",
                "replacement_room": replacement_room.to_string(),
            }),
            None,
            now,
        )
        .await
}

/// `POST /rooms/{roomId}/upgrade`.
///
/// The old room's tombstone names the replacement room, and below room version 12 the
/// replacement's `m.room.create` names the tombstone in `predecessor.event_id`. How that circle
/// is broken depends on the target version's room-ID format:
///
/// - **Opaque IDs (versions 1-11).** The replacement's ID is minted first, on a room shard this
///   replica owns, then the tombstone is sent naming it, then the room is created under that ID
///   with `predecessor: {room_id, event_id}` -- the order Synapse's
///   `RoomCreationHandler._upgrade_room` uses.
/// - **Hash-derived IDs (version 12+, MSC4291).** No ID can be chosen ahead of the create event,
///   whose reference hash the ID is. So the replacement room is created first (placed on an
///   owned shard by `RoomActor::create_placed`, decision 0020) with `predecessor: {room_id}`
///   only (`event_id` is optional from client-server API v1.16), and the tombstone is sent
///   after, naming the room's real ID. The requester's membership and power to tombstone the
///   old room are checked before the replacement is created, so a refused upgrade creates
///   nothing.
///
/// Moving aliases and locking down the old room's power levels follow in both cases and are
/// best-effort: the upgrade has already happened by then. A completed upgrade is logged at
/// `info` and counted in `hs_room_upgrades_total{outcome="completed"}`; one that wrote one half
/// (a tombstone naming a room whose create then failed, or a replacement whose tombstone was
/// refused) is logged at `warn` and counted with `outcome="replacement_orphaned"`.
///
/// # Errors
/// [`RoomError::BadRequest`] for a malformed body, [`RoomError::UnsupportedRoomVersion`] for an
/// unknown `new_version`, [`RoomError::Forbidden`] when the requester may not tombstone the old
/// room, [`RoomError::Fenced`] when no replacement ID could be placed on this replica, and
/// whatever loading the old room or creating the new one can return.
pub async fn post_upgrade<B: KvBackend + 'static>(
    State(state): State<RoomState<B>>,
    Path(room_id): Path<String>,
    RoomRequester(requester): RoomRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, RoomError> {
    crate::moderation::refuse_if_suspended(&requester)?;
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
    // "supported" gate, and it must run before anything is written, not after.
    let Some(rules) = hs_model::room_version::rules_for(&new_version) else {
        return Err(RoomError::UnsupportedRoomVersion(
            new_version_str.to_owned(),
        ));
    };
    let hash_based = rules.room_id_format == RoomIdFormat::V2HashBased;
    let extra_creators = additional_creators(&body)?;
    if !extra_creators.is_empty() && !rules.additional_room_creators {
        return Err(RoomError::BadRequest(format!(
            "room version {new_version} has no additional_creators"
        )));
    }

    let old_handle = state.rooms.get_or_load(&old_room_id).await?;
    let sender = requester.user_id.clone();
    let now = now_ms();

    // Step 3 + reading the `m.room.create`'s `type` (step 2's "a `type` field which is copied
    // from the predecessor room") + the old room's local aliases (step 4), and whether the
    // requester may tombstone the old room at all (step 1).
    let probe = sender.clone();
    let (
        may_tombstone,
        transferable,
        (creation_type, federate, bans),
        old_aliases,
        canonical_alias,
        (old_privileged_creators, old_creators),
    ) = old_handle
        .query(move |actor| {
            let joined = actor
                .state_event("m.room.member", probe.as_str())
                .ok()
                .flatten()
                .and_then(|e| {
                    e.json()
                        .get("content")?
                        .as_object()?
                        .get("membership")?
                        .as_str()
                        .map(|m| m == "join")
                })
                .unwrap_or(false);
            let may_tombstone = joined
                && actor
                    .can_send_state(&probe, "m.room.tombstone")
                    .unwrap_or(false);
            let canonical_alias = actor
                .state_event("m.room.canonical_alias", "")
                .ok()
                .flatten()
                .and_then(|e| e.json().get("content"))
                .map(|v| {
                    serde_json::from_slice::<Value>(&v.to_canonical_bytes()).unwrap_or(Value::Null)
                });
            (
                may_tombstone,
                actor.transferable_state(),
                (
                    actor.creation_type(),
                    actor.creation_federate(),
                    actor.banned_members().unwrap_or_default(),
                ),
                actor.list_aliases().unwrap_or_default(),
                canonical_alias,
                (
                    actor.privileges_creators(),
                    actor.creators().unwrap_or_default(),
                ),
            )
        })
        .await;

    let mut creators = vec![sender.clone()];
    creators.extend(extra_creators.iter().cloned());
    let mut power_level_content_override = None;
    // The old room's power levels as they will stand in the replacement once the upgrade is
    // done, when the upgrader had to be lifted to 100 to send them (`lift_upgrader`).
    let mut power_levels_to_restore = None;
    let mut initial_state = Vec::with_capacity(transferable.len());
    for (event_type, content) in transferable {
        if event_type == "m.room.power_levels" {
            let translated = power_levels_for_replacement(
                content,
                &rules,
                &creators,
                old_privileged_creators,
                &old_creators,
            );
            let (first, restore) = lift_upgrader(translated, &rules, &sender);
            power_level_content_override = Some(first);
            power_levels_to_restore = restore;
        } else {
            initial_state.push(InitialStateEvent {
                event_type: event_type.to_owned(),
                state_key: String::new(),
                content,
            });
        }
    }

    let mut creation_content = serde_json::Map::new();
    if let Some(room_type) = creation_type {
        creation_content.insert("type".to_owned(), Value::String(room_type));
    }
    if !federate {
        // A room closed to other servers stays closed: `m.federate` is in the create event and
        // nowhere else, so it is the one thing an upgrade cannot carry as state.
        creation_content.insert("m.federate".to_owned(), Value::Bool(false));
    }
    if !extra_creators.is_empty() {
        creation_content.insert(
            "additional_creators".to_owned(),
            extra_creators
                .iter()
                .map(|u| Value::String(u.to_string()))
                .collect(),
        );
    }
    let mut predecessor = serde_json::Map::new();
    predecessor.insert("room_id".to_owned(), Value::String(old_room_id.to_string()));
    let mut request = CreateRoomRequest {
        room_version: Some(new_version.clone()),
        initial_state,
        power_level_content_override,
        ..Default::default()
    };

    let new_handle = if hash_based {
        // The replacement room is created first here, so nothing is written before the
        // requester is known to be allowed to tombstone the old room: a refused upgrade must
        // not leave a room behind.
        if !may_tombstone {
            return Err(RoomError::Forbidden(
                "you may not send m.room.tombstone in this room, so you may not upgrade it".into(),
            ));
        }
        creation_content.insert("predecessor".to_owned(), Value::Object(predecessor));
        request.creation_content = Value::Object(creation_content);
        let new_handle = state
            .rooms
            .create_room(sender.clone(), request, now)
            .await?;
        let new_room_id = new_handle.query(|actor| actor.room_id().to_owned()).await;
        if let Err(e) = send_tombstone(&old_handle, &sender, &new_room_id, now).await {
            crate::metrics::count_room_upgrade("replacement_orphaned");
            tracing::warn!(
                old_room_id = %old_room_id,
                new_room_id = %new_room_id,
                error = %e,
                "an upgrade created its replacement room but the old room refused the \
                 tombstone; nothing points at the replacement"
            );
            return Err(e);
        }
        new_handle
    } else {
        let new_room_id = placed_opaque_room_id(&state)?;
        let tombstone = send_tombstone(&old_handle, &sender, &new_room_id, now).await?;
        predecessor.insert(
            "event_id".to_owned(),
            Value::String(tombstone.event_id().to_string()),
        );
        creation_content.insert("predecessor".to_owned(), Value::Object(predecessor));
        request.creation_content = Value::Object(creation_content);
        request.room_id = Some(new_room_id.clone());
        match state.rooms.create_room(sender.clone(), request, now).await {
            Ok(handle) => handle,
            Err(e) => {
                crate::metrics::count_room_upgrade("replacement_orphaned");
                tracing::warn!(
                    old_room_id = %old_room_id,
                    new_room_id = %new_room_id,
                    error = %e,
                    "an upgrade tombstoned the old room but creating the replacement failed; \
                     the tombstone names a room that does not exist"
                );
                return Err(e);
            }
        }
    };
    let new_room_id = new_handle.query(|actor| actor.room_id().to_owned()).await;

    // The old room's bans, each sent into the replacement by the upgrader (who holds 100 here
    // until `power_levels_to_restore` is sent, so the room's ban level cannot stop them). Best
    // effort per ban, as Synapse's are: one the auth rules refuse (a banned user the copied
    // power levels rank above the upgrader) is logged and skipped, and the upgrade stands.
    let mut bans_carried = 0usize;
    for (banned, content) in &bans {
        match new_handle
            .send_event(
                sender.clone(),
                "m.room.member".to_owned(),
                Some(banned.clone()),
                content.clone(),
                None,
                now,
            )
            .await
        {
            Ok(_) => bans_carried += 1,
            Err(e) => tracing::warn!(
                old_room_id = %old_room_id,
                new_room_id = %new_room_id,
                banned = %banned,
                error = %e,
                "an upgrade could not carry a ban into the replacement room"
            ),
        }
    }

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

    // The upgrader was lifted to 100 to send the replacement's initial state; now that it is
    // sent, the power levels become what the old room's were (Synapse's two power-level events
    // in an upgraded room, matrix-org/synapse#6632). Best-effort: the room exists either way.
    if let Some(restore) = power_levels_to_restore
        && let Err(e) = new_handle
            .send_event(
                sender.clone(),
                "m.room.power_levels".to_owned(),
                Some(String::new()),
                restore,
                None,
                now,
            )
            .await
    {
        tracing::warn!(
            old_room_id = %old_room_id,
            new_room_id = %new_room_id,
            error = %e,
            "an upgrade could not restore the upgrader's old power level in the replacement room"
        );
    }

    // A published room's replacement takes its place in this server's room directory
    // (Synapse's `transfer_room_state_on_room_upgrade`); the old room, tombstoned, leaves it.
    let directory_moved = move_directory_entry(&state, &old_room_id, &new_room_id);

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

    crate::metrics::count_room_upgrade("completed");
    tracing::info!(
        old_room_id = %old_room_id,
        new_room_id = %new_room_id,
        new_version = %new_version,
        bans_carried,
        bans_in_old_room = bans.len(),
        aliases_moved = old_aliases.len(),
        directory_moved,
        "upgraded a room"
    );
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
            remote_join: None,
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
            None,
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

    /// The content of `room`'s `(event_type, state_key)` state event.
    async fn state_content(
        state: &RoomState<MemoryBackend>,
        room: &ruma::RoomId,
        event_type: &'static str,
        state_key: &'static str,
    ) -> Option<Value> {
        let handle = state.rooms.get_or_load(room).await.unwrap();
        handle
            .query(move |actor| {
                actor.state_event(event_type, state_key).unwrap().map(|e| {
                    let content = e.json().get("content").unwrap();
                    serde_json::from_slice(&content.to_canonical_bytes()).unwrap()
                })
            })
            .await
    }

    async fn create(state: &RoomState<MemoryBackend>, user: &ruma::UserId, body: Value) -> String {
        let created = post_create_room::<MemoryBackend>(
            State(state.clone()),
            requester(user),
            None,
            PermissiveJson(body),
        )
        .await
        .unwrap();
        json_body(created).await["room_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn upgrade(
        state: &RoomState<MemoryBackend>,
        user: &ruma::UserId,
        room: &str,
        body: Value,
    ) -> Result<String, RoomError> {
        let response = post_upgrade::<MemoryBackend>(
            State(state.clone()),
            Path(room.to_owned()),
            requester(user),
            PermissiveJson(body),
        )
        .await?;
        Ok(json_body(response).await["replacement_room"]
            .as_str()
            .unwrap()
            .to_owned())
    }

    /// Upgrading to room version 12, whose room IDs are the create event's hash: the old room's
    /// tombstone names a room that exists, at version 12, whose create event names the old room
    /// as its predecessor, whose power levels carried over without naming its creator (which
    /// version 12's auth rules refuse), and which holds the moved alias and the creator's join.
    /// Before the fix the tombstone named an opaque ID minted ahead, which a version-12 create
    /// ignores, and the replacement's power levels were refused besides.
    #[tokio::test]
    async fn an_upgrade_to_v12_tombstones_the_old_room_with_the_real_replacement_id() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let old = create(
            &state,
            alice,
            json!({"preset": "public_chat", "topic": "before", "room_alias_name": "lobby"}),
        )
        .await;
        let old_id = RoomId::parse(&old).unwrap().to_owned();

        let replacement = upgrade(&state, alice, &old, json!({"new_version": "12"}))
            .await
            .expect("an upgrade to room version 12 succeeds");

        let tombstone = state_content(&state, &old_id, "m.room.tombstone", "")
            .await
            .expect("the old room is tombstoned");
        let named = tombstone["replacement_room"].as_str().unwrap();
        assert_eq!(named, replacement, "the response and the tombstone agree");
        let new_id = RoomId::parse(named).unwrap().to_owned();
        assert!(
            state.rooms.list_all_room_ids().unwrap().contains(&new_id),
            "the tombstone's replacement_room {new_id} is a room this server has"
        );
        let new_handle = state.rooms.get_or_load(&new_id).await.unwrap();
        assert_eq!(
            new_handle.query(|a| a.room_version().clone()).await,
            RoomVersionId::try_from("12").unwrap()
        );
        assert_eq!(
            new_handle.query(|a| a.room_id().to_owned()).await,
            new_id,
            "the room loaded under the tombstone's id is that room"
        );

        let create_content = state_content(&state, &new_id, "m.room.create", "")
            .await
            .unwrap();
        assert_eq!(create_content["predecessor"]["room_id"], old.as_str());
        assert_eq!(create_content["room_version"], "12");

        let topic = state_content(&state, &new_id, "m.room.topic", "")
            .await
            .expect("the topic carried over");
        assert_eq!(topic["topic"], "before");
        let power = state_content(&state, &new_id, "m.room.power_levels", "")
            .await
            .expect("the power levels carried over");
        assert!(
            power["users"].get(alice.as_str()).is_none(),
            "a version-12 room's creator is not named in its power levels: {power}"
        );
        let member = state_content(&state, &new_id, "m.room.member", "@alice:hs1")
            .await
            .expect("the upgrader joined the replacement");
        assert_eq!(member["membership"], "join");

        let alias = <&ruma::RoomAliasId>::try_from("#lobby:hs1").unwrap();
        assert_eq!(
            state.rooms.resolve_alias(alias).unwrap(),
            Some(new_id.clone()),
            "the alias moved to the replacement"
        );
    }

    /// A version-12 upgrade the requester may not make (joined, but without the power to send
    /// `m.room.tombstone`) is refused before the replacement room is created, since that is
    /// written first for this version.
    #[tokio::test]
    async fn a_refused_upgrade_to_v12_creates_no_room() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let old = create(&state, alice, json!({"preset": "public_chat"})).await;
        let old_id = RoomId::parse(&old).unwrap().to_owned();
        state
            .rooms
            .get_or_load(&old_id)
            .await
            .unwrap()
            .membership(
                bob.to_owned(),
                crate::membership::Action::Join,
                bob.to_owned(),
                json!({}),
                now_ms(),
            )
            .await
            .unwrap();
        let rooms_before = state.rooms.list_all_room_ids().unwrap().len();

        let err = upgrade(&state, bob, &old, json!({"new_version": "12"}))
            .await
            .unwrap_err();
        assert!(matches!(err, RoomError::Forbidden(_)), "got {err:?}");
        assert_eq!(state.rooms.list_all_room_ids().unwrap().len(), rooms_before);
        assert!(
            state_content(&state, &old_id, "m.room.tombstone", "")
                .await
                .is_none()
        );
    }

    /// `additional_creators` reaches the version-12 replacement's create event, and its entries
    /// are taken out of the carried-over power levels with the upgrader.
    #[tokio::test]
    async fn an_upgrade_to_v12_names_additional_creators() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let old = create(
            &state,
            alice,
            json!({"preset": "public_chat", "power_level_content_override": {
                "users": {"@alice:hs1": 100, "@carol:hs1": 100}
            }}),
        )
        .await;
        let replacement = upgrade(
            &state,
            alice,
            &old,
            json!({"new_version": "12", "additional_creators": ["@carol:hs1"]}),
        )
        .await
        .unwrap();
        let new_id = RoomId::parse(&replacement).unwrap().to_owned();
        let create_content = state_content(&state, &new_id, "m.room.create", "")
            .await
            .unwrap();
        assert_eq!(create_content["additional_creators"], json!(["@carol:hs1"]));
        let power = state_content(&state, &new_id, "m.room.power_levels", "")
            .await
            .unwrap();
        assert!(power["users"].get("@carol:hs1").is_none(), "{power}");

        let err = upgrade(
            &state,
            alice,
            &replacement,
            json!({"new_version": "11", "additional_creators": ["@carol:hs1"]}),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RoomError::BadRequest(_)), "got {err:?}");
    }

    /// Upgrading a version-12 room to an earlier version: its creators held unlimited power and
    /// had no `users` entry, so in the replacement they are given 100. Without that the upgrader
    /// created a room in which it had no power to send the room's initial state, and the
    /// upgrade failed after the tombstone was written.
    #[tokio::test]
    async fn an_upgrade_from_v12_to_an_earlier_version_keeps_the_creator_in_charge() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let old = create(
            &state,
            alice,
            json!({"preset": "public_chat", "room_version": "12", "topic": "hashed"}),
        )
        .await;
        let replacement = upgrade(&state, alice, &old, json!({"new_version": "11"}))
            .await
            .expect("an upgrade from 12 to 11 succeeds");
        let new_id = RoomId::parse(&replacement).unwrap().to_owned();
        let power = state_content(&state, &new_id, "m.room.power_levels", "")
            .await
            .unwrap();
        assert_eq!(power["users"]["@alice:hs1"], 100, "{power}");
        let topic = state_content(&state, &new_id, "m.room.topic", "")
            .await
            .unwrap();
        assert_eq!(topic["topic"], "hashed");
        let create_content = state_content(&state, &new_id, "m.room.create", "")
            .await
            .unwrap();
        let tombstone = state_content(
            &state,
            &RoomId::parse(&old).unwrap().to_owned(),
            "m.room.tombstone",
            "",
        )
        .await
        .unwrap();
        assert_eq!(tombstone["replacement_room"], replacement.as_str());
        assert_eq!(create_content["predecessor"]["room_id"], old.as_str());
        assert!(
            create_content["predecessor"]["event_id"].is_string(),
            "an opaque-id replacement still names the tombstone: {create_content}"
        );
    }

    /// In a cluster (fencing installed, this replica owning one room shard of four) an upgrade's
    /// replacement room lands on a shard this replica owns, for both ID formats: version 12 by
    /// `create_placed`'s retry, an opaque version by minting until the ID is placed. The shard
    /// gate forwards an upgrade to the owner of the old room, so this is what makes the
    /// replacement local to the replica that ran it. Before the fix an opaque replacement ID was
    /// minted at random, so three upgrades in four tombstoned the old room and then had the
    /// create refused by the fence.
    #[tokio::test]
    async fn an_upgrades_replacement_lands_on_a_shard_this_replica_owns() {
        use crate::fencing::tests::{LAYOUT, owning};
        let backend = MemoryBackend::new();
        let identity = HomeserverIdentity::for_tests("hs1");
        let rooms = Arc::new(RoomRegistry::open(backend.clone(), identity.clone()).unwrap());
        rooms.install_fencing(owning(&backend, vec![2]));
        let state = RoomState {
            auth: AuthState::in_memory(),
            rooms,
            identity,
            remote_join: None,
        };
        let alice = user_id!("@alice:hs1");
        for (i, version) in ["10", "12", "11", "12", "9", "12", "10", "11"]
            .into_iter()
            .enumerate()
        {
            let old = create(&state, alice, json!({"preset": "public_chat"})).await;
            let replacement = upgrade(&state, alice, &old, json!({"new_version": version}))
                .await
                .unwrap_or_else(|e| panic!("upgrade {i} to {version} failed: {e:?}"));
            assert_eq!(
                LAYOUT.room_shard(&replacement).index,
                2,
                "the replacement {replacement} (version {version}) is on the owned shard"
            );
            let old_id = RoomId::parse(&old).unwrap().to_owned();
            let tombstone = state_content(&state, &old_id, "m.room.tombstone", "")
                .await
                .unwrap();
            assert_eq!(tombstone["replacement_room"], replacement.as_str());
        }
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
            None,
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

    async fn join(
        state: &RoomState<MemoryBackend>,
        user: &ruma::UserId,
        room: &str,
    ) -> Result<Value, RoomError> {
        let response = crate::routes::membership::post_join::<MemoryBackend>(
            State(state.clone()),
            Path(room.to_owned()),
            axum::extract::RawQuery(None),
            requester(user),
            PermissiveJson(json!({})),
        )
        .await?;
        Ok(json_body(response).await)
    }

    /// How many events of `event_type` the room's timeline holds, oldest to newest.
    async fn timeline_count(
        state: &RoomState<MemoryBackend>,
        room: &ruma::RoomId,
        event_type: &'static str,
    ) -> usize {
        let handle = state.rooms.get_or_load(room).await.unwrap();
        handle
            .query(move |actor| {
                actor
                    .events_after(i64::MIN, usize::MAX)
                    .iter()
                    .filter(|(_, e)| e.header().event_type == event_type)
                    .count()
            })
            .await
    }

    /// The old room's bans go with it: a user banned before the upgrade is banned in the
    /// replacement, with the ban's content (the reason), and cannot join it. Before this the
    /// replacement got the spec's recommended state only, and the banned user walked in through
    /// the tombstone (Sytest's "/upgrade copies ban events to the new room").
    #[tokio::test]
    async fn an_upgrade_carries_the_old_rooms_bans() {
        for version in ["10", "12"] {
            let state = app();
            let alice = user_id!("@alice:hs1");
            let bob = user_id!("@bob:hs1");
            let old = create(&state, alice, json!({"preset": "public_chat"})).await;
            let old_id = RoomId::parse(&old).unwrap().to_owned();
            join(&state, bob, &old).await.unwrap();
            let old_handle = state.rooms.get_or_load(&old_id).await.unwrap();
            old_handle
                .membership(
                    alice.to_owned(),
                    crate::membership::Action::Ban,
                    bob.to_owned(),
                    json!({"reason": "spam"}),
                    now_ms(),
                )
                .await
                .unwrap();
            // Somebody who was never in the room, banned ahead of time, is carried too.
            old_handle
                .membership(
                    alice.to_owned(),
                    crate::membership::Action::Ban,
                    user_id!("@mallory:elsewhere.example").to_owned(),
                    json!({}),
                    now_ms(),
                )
                .await
                .unwrap();

            let replacement = upgrade(&state, alice, &old, json!({"new_version": version}))
                .await
                .unwrap();
            let new_id = RoomId::parse(&replacement).unwrap().to_owned();

            let ban = state_content(&state, &new_id, "m.room.member", "@bob:hs1")
                .await
                .unwrap_or_else(|| panic!("bob's ban is in the version-{version} replacement"));
            assert_eq!(ban["membership"], "ban");
            assert_eq!(ban["reason"], "spam", "the ban's content came with it");
            let other = state_content(
                &state,
                &new_id,
                "m.room.member",
                "@mallory:elsewhere.example",
            )
            .await
            .expect("a ban of a stranger is carried too");
            assert_eq!(other["membership"], "ban");
            let refused = join(&state, bob, &replacement).await.unwrap_err();
            assert!(
                matches!(refused, RoomError::Forbidden(_)),
                "bob stays out of the version-{version} replacement: {refused:?}"
            );
        }
    }

    /// A room closed to federation (`"m.federate": false` on its create event) stays closed
    /// across an upgrade: the replacement's create event says so too (Sytest's "/upgrade
    /// preserves room federation ability").
    #[tokio::test]
    async fn an_upgrade_keeps_a_room_closed_to_federation() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let closed = create(
            &state,
            alice,
            json!({"creation_content": {"m.federate": false}}),
        )
        .await;
        let replacement = upgrade(&state, alice, &closed, json!({"new_version": "11"}))
            .await
            .unwrap();
        let create_content = state_content(
            &state,
            &RoomId::parse(&replacement).unwrap().to_owned(),
            "m.room.create",
            "",
        )
        .await
        .unwrap();
        assert_eq!(create_content["m.federate"], false);

        let open = create(&state, alice, json!({})).await;
        let replacement = upgrade(&state, alice, &open, json!({"new_version": "11"}))
            .await
            .unwrap();
        let create_content = state_content(
            &state,
            &RoomId::parse(&replacement).unwrap().to_owned(),
            "m.room.create",
            "",
        )
        .await
        .unwrap();
        assert!(
            create_content.get("m.federate").is_none(),
            "an open room's replacement does not grow an m.federate key: {create_content}"
        );
    }

    /// A published room's replacement takes its place in the room directory, and the old
    /// room leaves it (Sytest's "/upgrade should preserve room visibility for public rooms").
    #[tokio::test]
    async fn an_upgrade_moves_the_directory_entry_to_the_replacement() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let old = create(
            &state,
            alice,
            json!({"preset": "public_chat", "visibility": "public"}),
        )
        .await;
        let old_id = RoomId::parse(&old).unwrap().to_owned();
        assert!(state.rooms.is_directory_public(&old_id).unwrap());

        let replacement = upgrade(&state, alice, &old, json!({"new_version": "11"}))
            .await
            .unwrap();
        let new_id = RoomId::parse(&replacement).unwrap().to_owned();
        assert!(
            state.rooms.is_directory_public(&new_id).unwrap(),
            "the replacement is published"
        );
        assert!(
            !state.rooms.is_directory_public(&old_id).unwrap(),
            "the tombstoned room is not"
        );

        let private = create(&state, alice, json!({"preset": "public_chat"})).await;
        let replacement = upgrade(&state, alice, &private, json!({"new_version": "11"}))
            .await
            .unwrap();
        assert!(
            !state
                .rooms
                .is_directory_public(&RoomId::parse(&replacement).unwrap().to_owned())
                .unwrap(),
            "an unpublished room's replacement is not published"
        );
    }

    /// A moderator allowed to send the tombstone upgrades the room: in the replacement they
    /// hold 100 only long enough to send the copied state, after which the power levels are
    /// the old room's exactly (the moderator at 50 again), and the old room's power levels --
    /// which a moderator cannot rewrite -- are left as they were (Sytest's "/upgrade preserves
    /// the power level of the upgrading user in old and new rooms", matrix-org/synapse#6632).
    #[tokio::test]
    async fn a_moderators_upgrade_restores_their_level_in_the_replacement() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let old = create(&state, alice, json!({"preset": "public_chat"})).await;
        let old_id = RoomId::parse(&old).unwrap().to_owned();
        join(&state, bob, &old).await.unwrap();
        let old_handle = state.rooms.get_or_load(&old_id).await.unwrap();
        let mut levels = state_content(&state, &old_id, "m.room.power_levels", "")
            .await
            .unwrap();
        levels["users"]["@bob:hs1"] = json!(50);
        levels["events"]["m.room.tombstone"] = json!(50);
        old_handle
            .send_event(
                alice.to_owned(),
                "m.room.power_levels".to_owned(),
                Some(String::new()),
                levels.clone(),
                None,
                now_ms(),
            )
            .await
            .unwrap();
        // State a moderator may not send: the default `events` map puts encryption at 100.
        old_handle
            .send_event(
                alice.to_owned(),
                "m.room.encryption".to_owned(),
                Some(String::new()),
                json!({"algorithm": "m.megolm.v1.aes-sha2"}),
                None,
                now_ms(),
            )
            .await
            .unwrap();

        let replacement = upgrade(&state, bob, &old, json!({"new_version": "11"}))
            .await
            .expect("a moderator who may send the tombstone may upgrade");
        let new_id = RoomId::parse(&replacement).unwrap().to_owned();
        let new_levels = state_content(&state, &new_id, "m.room.power_levels", "")
            .await
            .unwrap();
        assert_eq!(
            new_levels, levels,
            "the replacement's power levels are the old room's"
        );
        assert_eq!(
            timeline_count(&state, &new_id, "m.room.power_levels").await,
            2,
            "one power-levels event to create the room with bob at 100, one to put him back"
        );
        let encryption = state_content(&state, &new_id, "m.room.encryption", "")
            .await
            .expect("the encryption state, at level 100, was still copied");
        assert_eq!(encryption["algorithm"], "m.megolm.v1.aes-sha2");
        let old_levels = state_content(&state, &old_id, "m.room.power_levels", "")
            .await
            .unwrap();
        assert_eq!(
            old_levels, levels,
            "a moderator cannot lock the old room down, so its power levels are unchanged"
        );
    }

    /// The copied state stands in for the preset's: a replacement whose predecessor had
    /// `history_visibility: joined` has exactly one history-visibility event, saying `joined`,
    /// not the preset's `shared` first and the copy after it (Sytest's "/upgrade copies
    /// important state to the new room", which reads the first such event in the timeline).
    #[tokio::test]
    async fn the_copied_state_replaces_the_presets_in_the_replacement() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let old = create(
            &state,
            alice,
            json!({
                "preset": "public_chat",
                "initial_state": [
                    {"type": "m.room.history_visibility", "state_key": "", "content": {"history_visibility": "joined"}},
                    {"type": "m.room.guest_access", "state_key": "", "content": {"guest_access": "can_join"}},
                ],
            }),
        )
        .await;
        let old_id = RoomId::parse(&old).unwrap().to_owned();
        assert_eq!(
            timeline_count(&state, &old_id, "m.room.history_visibility").await,
            1,
            "initial_state takes precedence over the preset at creation too"
        );
        let replacement = upgrade(&state, alice, &old, json!({"new_version": "11"}))
            .await
            .unwrap();
        let new_id = RoomId::parse(&replacement).unwrap().to_owned();
        for (event_type, key, value) in [
            ("m.room.history_visibility", "history_visibility", "joined"),
            ("m.room.guest_access", "guest_access", "can_join"),
            ("m.room.join_rules", "join_rule", "public"),
        ] {
            assert_eq!(
                timeline_count(&state, &new_id, event_type).await,
                1,
                "{event_type} is sent once in the replacement"
            );
            assert_eq!(
                state_content(&state, &new_id, event_type, "")
                    .await
                    .unwrap()[key],
                value
            );
        }
    }

    /// Another server upgraded a room this server's users were in: when one of them joins the
    /// replacement (whose create names the old room), this server's aliases for the old room
    /// and its place in this server's directory follow, as Synapse's do on join. Here the
    /// replacement is made locally with a `predecessor` in its create, which is what a remote
    /// one looks like once it is held.
    #[tokio::test]
    async fn a_join_into_a_replacement_moves_this_servers_aliases_and_directory_entry() {
        let state = app();
        let alice = user_id!("@alice:hs1");
        let bob = user_id!("@bob:hs1");
        let old = create(
            &state,
            alice,
            json!({"preset": "public_chat", "visibility": "public", "room_alias_name": "ours"}),
        )
        .await;
        let old_id = RoomId::parse(&old).unwrap().to_owned();
        let replacement = create(
            &state,
            alice,
            json!({"preset": "public_chat", "creation_content": {"predecessor": {"room_id": old}}}),
        )
        .await;
        let new_id = RoomId::parse(&replacement).unwrap().to_owned();
        let alias = <&ruma::RoomAliasId>::try_from("#ours:hs1").unwrap();
        assert_eq!(
            state.rooms.resolve_alias(alias).unwrap(),
            Some(old_id.clone())
        );
        assert!(!state.rooms.is_directory_public(&new_id).unwrap());

        join(&state, bob, &replacement).await.unwrap();

        assert_eq!(
            state.rooms.resolve_alias(alias).unwrap(),
            Some(new_id.clone()),
            "the alias followed bob's join"
        );
        assert!(state.rooms.is_directory_public(&new_id).unwrap());
        assert!(!state.rooms.is_directory_public(&old_id).unwrap());
        assert!(
            state
                .rooms
                .get_or_load(&old_id)
                .await
                .unwrap()
                .query(|actor| actor.list_aliases().unwrap())
                .await
                .is_empty()
        );
    }
}
