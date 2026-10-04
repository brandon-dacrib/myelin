//! `GET /keys/changes` between two of this crate's sync tokens: the membership walk `/sync`
//! makes between a `since` token and the `next_batch` it answers with, made again for any pair
//! of tokens a client presents, so that the answer agrees with what the syncs between them
//! said in `device_lists`.
//!
//! The device-list stream alone (`hs_e2e::store::DeviceKeyStore::changed_users_since`) names
//! the users who changed a device. It knows nothing about rooms, so from it alone `left` is
//! always empty and `changed` misses a user who merely started sharing a room (the spec's "or
//! who now share an encrypted room with the client"). What the walk adds, per room the user's
//! feed says changed between the tokens (the same candidate set `/sync` uses, plus the hot
//! rooms, which have no feed entries):
//!
//! - A room the user is joined to now and was not in at `from` (no position for it at the
//!   token, or their own membership event is past it and they were not joined at it): every
//!   current member is possibly changed, an invitee or knocker included.
//! - A room they were already in: every `m.room.member` event after `from`'s position (and at
//!   or before `to`'s, when `to` resolves) is read, bounded by [`MEMBER_WALK_LIMIT`]; a join,
//!   invite or knock puts its subject among the possibly changed, a leave or ban among the
//!   possibly left (the later event wins, as in `/sync`). Past the bound, every current member
//!   is possibly changed -- more than the minimum, and the direction that costs a key query
//!   rather than a message nobody can read.
//! - A room they left or were banned from after `from`: everyone still in it is possibly left.
//!
//! Then, as Synapse's `get_user_ids_changed` does, the possibly changed who share a joined room
//! with the user now (or were invited to or knocked on one of the user's joined rooms within
//! the walk) are `changed`, and everybody possibly changed or possibly left who shares none is
//! `left`. The stream's users are intersected with the users sharing a room now, the same
//! privacy scope `/sync` applies (`crate::sync`'s module docs), the user themself included.
//!
//! `to` bounds the stream exactly and the walk where a room's position at `to` is known;
//! where it is not (a room with no feed entry at or before `to`), the walk runs to the room's
//! head. Synapse ignores `to` altogether, and a user named too early is a key query too many,
//! never a message somebody cannot read.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use hs_e2e::state::DeviceListChanges;
use hs_e2e::store::{DeviceKeyStore, E2eStore};
use hs_kv::KvBackend;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};

use crate::error::UserError;
use crate::hub::SessionHub;
use crate::room_source::RoomSource;
use crate::store::MembershipRecord;
use crate::token::SyncToken;

/// How many events after `from`'s position one room's walk reads before giving up on the
/// exact answer and naming every current member instead. `/sync` answers a gap the same way.
pub const MEMBER_WALK_LIMIT: usize = 1_000;

/// One `m.room.member` event's subject and their new membership, as the walk reads it.
type MemberChange = (OwnedUserId, String);

/// The answer to `GET /keys/changes?from=&to=` for `user_id`, between two of this crate's
/// tokens. See the module docs.
///
/// # Errors
/// Returns [`UserError`] on a store or room failure. A room that no longer exists is skipped.
pub async fn changes_between<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    e2e: &Arc<dyn E2eStore>,
    user_id: &UserId,
    from: &SyncToken,
    to: Option<&SyncToken>,
) -> Result<DeviceListChanges, UserError> {
    let store = hub.store();
    let shared = hub.users_sharing_room_with(user_id).await?;

    // The stream, scoped as `/sync` scopes it.
    let stream_changed = DeviceKeyStore::changed_users_since(
        &**e2e,
        from.device_list_seq,
        to.map(|t| t.device_list_seq),
    )
    .await?;
    let mut changed: BTreeSet<OwnedUserId> = stream_changed
        .into_iter()
        .filter(|u| shared.contains(u) || u.as_str() == user_id.as_str())
        .collect();

    // The walk: the rooms with a feed entry between the tokens, and the hot rooms.
    let memberships: HashMap<OwnedRoomId, MembershipRecord> = store
        .list_memberships(user_id)
        .await?
        .into_iter()
        .map(|m| (m.room_id.clone(), m))
        .collect();
    let mut candidates: BTreeSet<OwnedRoomId> = store
        .feed_since(user_id, from.feed_seq)
        .await?
        .into_iter()
        .filter(|e| to.is_none_or(|t| e.feed_seq <= t.feed_seq))
        .map(|e| e.room_id)
        .collect();
    candidates.extend(
        memberships
            .values()
            .filter(|m| m.hot_room)
            .map(|m| m.room_id.clone()),
    );

    let mut possibly_changed: BTreeSet<OwnedUserId> = BTreeSet::new();
    let mut possibly_left: BTreeSet<OwnedUserId> = BTreeSet::new();
    // Invitees and knockers of the user's joined rooms seen in the walk: not joined
    // co-members, but somebody the user is about to share a room with.
    let mut pending: BTreeSet<OwnedUserId> = BTreeSet::new();
    let mut rooms_walked = 0usize;

    for room_id in &candidates {
        let Some(membership) = memberships.get(room_id) else {
            continue;
        };
        let pos_from = position_as_of(hub, user_id, room_id, from).await?;
        let pos_to = match to {
            Some(token) => position_as_of(hub, user_id, room_id, token).await?,
            None => None,
        };
        match membership.membership.as_str() {
            "join" => {
                let handle = match hub.room(room_id).await {
                    Ok(handle) => handle,
                    Err(error) if error.is_room_not_found() => continue,
                    Err(error) => return Err(error),
                };
                rooms_walked += 1;
                let joined_since = match pos_from {
                    None => true,
                    Some(pos) if membership.room_pos > pos => {
                        let user = user_id.to_owned();
                        !handle
                            .query(move |actor| actor.was_joined_at(&user, pos))
                            .await?
                    }
                    Some(_) => false,
                };
                let Some(pos) = pos_from.filter(|_| !joined_since) else {
                    // New to the user since `from`: everyone in it now.
                    for (other, membership) in current_members(&handle).await? {
                        note_arrival(&other, &membership, &mut possibly_changed, &mut pending);
                    }
                    continue;
                };
                let (changes, limited) = handle
                    .query(move |actor| {
                        let events = actor.events_after(pos, MEMBER_WALK_LIMIT);
                        let limited = events.len() >= MEMBER_WALK_LIMIT;
                        let changes: Vec<MemberChange> = events
                            .into_iter()
                            .filter(|(pos, _)| pos_to.is_none_or(|t| *pos <= t))
                            .filter_map(|(_, event)| member_change(event))
                            .collect();
                        (changes, limited)
                    })
                    .await;
                for (other, membership) in changes {
                    if other.as_str() == user_id.as_str() {
                        continue;
                    }
                    match membership.as_str() {
                        "join" | "invite" | "knock" => {
                            possibly_left.remove(&other);
                            note_arrival(&other, &membership, &mut possibly_changed, &mut pending);
                        }
                        _ => {
                            possibly_changed.remove(&other);
                            possibly_left.insert(other);
                        }
                    }
                }
                if limited {
                    for (other, membership) in current_members(&handle).await? {
                        note_arrival(&other, &membership, &mut possibly_changed, &mut pending);
                    }
                }
            }
            "leave" | "ban" => {
                // The user's own departure, after `from` and not after `to`: everyone still
                // in the room is somebody they may no longer share a room with.
                let departed_since = pos_from.is_none_or(|pos| membership.room_pos > pos);
                let departed_after_to =
                    to.is_some() && pos_to.is_some_and(|pos| membership.room_pos > pos);
                if departed_since && !departed_after_to {
                    rooms_walked += 1;
                    possibly_left.extend(hub.joined_member_ids_if_present(room_id).await?);
                }
            }
            _ => {}
        }
    }

    possibly_changed.remove(user_id);
    possibly_left.remove(user_id);
    let shares_now = |u: &OwnedUserId| shared.contains(u) || pending.contains(u);
    let mut left: BTreeSet<OwnedUserId> = possibly_left
        .iter()
        .chain(possibly_changed.iter())
        .filter(|u| !shares_now(u))
        .cloned()
        .collect();
    changed.extend(possibly_changed.into_iter().filter(|u| shares_now(u)));
    left.retain(|u| !changed.contains(u));

    tracing::debug!(
        %user_id,
        rooms = candidates.len(),
        rooms_walked,
        changed = changed.len(),
        left = left.len(),
        "device-list changes between two sync tokens"
    );
    Ok(DeviceListChanges {
        changed: changed.into_iter().collect(),
        left: left.into_iter().collect(),
    })
}

/// Where `room_id` was as of `token` for `user_id`: the newer of the feed's and the hot-room
/// stream's positions, exactly as `crate::sync::resume_mode` and `crate::hub`'s
/// `FeedTokenResolver` take it. `None`: the room was not in this user's view at the token.
async fn position_as_of<B: KvBackend + 'static, R: RoomSource<B>>(
    hub: &SessionHub<B, R>,
    user_id: &UserId,
    room_id: &RoomId,
    token: &SyncToken,
) -> Result<Option<i64>, UserError> {
    let from_feed = hub
        .store()
        .room_pos_as_of(user_id, room_id, token.feed_seq)
        .await?;
    let from_hot = hub
        .store()
        .hot_room_pos_as_of(room_id, token.hot_seq)
        .await?;
    Ok(from_feed.max(from_hot))
}

/// Every current member of the room with their membership (`join`, `invite`, `knock`, ...).
async fn current_members<B: KvBackend + 'static>(
    handle: &hs_room::actor::RoomActorHandle<B>,
) -> Result<Vec<MemberChange>, UserError> {
    Ok(handle
        .query(|actor| {
            Ok::<_, hs_room::RoomError>(
                actor
                    .members()?
                    .into_iter()
                    .filter_map(member_change)
                    .collect::<Vec<_>>(),
            )
        })
        .await?)
}

/// `event`'s subject and their new membership, if it is a well-formed `m.room.member` event.
fn member_change(event: &hs_model::Event) -> Option<MemberChange> {
    let header = event.header();
    if header.event_type != "m.room.member" {
        return None;
    }
    let state_key = header.state_key.as_deref()?;
    let user_id = UserId::parse(state_key).ok()?;
    let membership = crate::hub::membership_of(event)?;
    Some((user_id.to_owned(), membership))
}

/// Records somebody arriving in a room the user is joined to: a joined member is possibly
/// changed; an invitee or knocker is too, and is also somebody the user shares the room with
/// for the purpose of splitting `changed` from `left`.
fn note_arrival(
    other: &OwnedUserId,
    membership: &str,
    possibly_changed: &mut BTreeSet<OwnedUserId>,
    pending: &mut BTreeSet<OwnedUserId>,
) {
    match membership {
        "join" => {
            possibly_changed.insert(other.clone());
        }
        "invite" | "knock" => {
            possibly_changed.insert(other.clone());
            pending.insert(other.clone());
        }
        _ => {}
    }
}
