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
//!   possibly left (the later event wins, as in `/sync`). The user's own leave and return
//!   within the walk, or the bound being reached, make every current member possibly changed -- more than the minimum, and the direction that costs a key query
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
    // A change only its user hears of (`DeviceKeyStore::record_own_device_list_change`).
    if DeviceKeyStore::own_changes_since(
        &**e2e,
        from.device_list_seq,
        to.map(|t| t.device_list_seq),
    )
    .await?
    .contains(user_id)
    {
        changed.insert(user_id.to_owned());
    }

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
                // The user's own leave followed by their own join within the walk: back in a
                // room they were out of, so everyone in it now is possibly changed.
                let mut own_left = false;
                let mut own_rejoined = false;
                for (other, membership) in changes {
                    if other.as_str() == user_id.as_str() {
                        match membership.as_str() {
                            "leave" | "ban" => own_left = true,
                            "join" if own_left => own_rejoined = true,
                            _ => {}
                        }
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
                if limited || own_rejoined {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::SyncFilter;
    use crate::room_source::test_support::registry;
    use crate::store::DynUserStore;
    use crate::store::tables::TablesUserStore;
    use crate::sync::{SyncParams, build};
    use hs_e2e::store::tables::TablesE2eStore;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::{CreateRoomRequest, RoomActorHandle};
    use hs_room::membership::Action;
    use ruma::user_id;
    use std::time::Duration;

    type TestHub = SessionHub<MemoryBackend, Arc<hs_room::registry::RoomRegistry<MemoryBackend>>>;

    struct World {
        hub: Arc<TestHub>,
        e2e: Arc<dyn E2eStore>,
        alice: OwnedUserId,
        bob: OwnedUserId,
        room: RoomActorHandle<MemoryBackend>,
        ts: std::sync::atomic::AtomicI64,
    }

    impl World {
        /// Alice's public room, watched by the hub; nobody else in it yet.
        async fn new() -> Self {
            let rooms = registry("keys.test");
            let store: DynUserStore =
                Arc::new(TablesUserStore::open(MemoryBackend::new()).unwrap());
            let hub = Arc::new(SessionHub::new(store, rooms, 500));
            let alice = user_id!("@alice:keys.test").to_owned();
            let room = hub
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
            hub.watch_room(room.clone()).await;
            Self {
                hub,
                e2e: Arc::new(TablesE2eStore::open(MemoryBackend::new()).unwrap()),
                alice,
                bob: user_id!("@bob:keys.test").to_owned(),
                room,
                ts: std::sync::atomic::AtomicI64::new(10),
            }
        }

        async fn act(&self, sender: &OwnedUserId, action: Action, target: &OwnedUserId) {
            let ts = self.ts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.room
                .membership(
                    sender.clone(),
                    action,
                    target.clone(),
                    serde_json::json!({}),
                    ts,
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        /// One of alice's syncs, as a client makes it: the response and its `next_batch`, with
        /// the device's cursor recorded so the next change lands in a new feed entry.
        async fn sync(&self, since: Option<SyncToken>) -> (serde_json::Value, SyncToken) {
            let params = SyncParams {
                since,
                full_state: false,
                timeout: Duration::from_millis(20),
                filter: SyncFilter::none(),
                device_id: Some("DEV".into()),
            };
            build(&self.hub, &self.e2e, &self.alice, params)
                .await
                .unwrap()
        }

        async fn changes(&self, from: &SyncToken, to: Option<&SyncToken>) -> DeviceListChanges {
            changes_between(&self.hub, &self.e2e, &self.alice, from, to)
                .await
                .unwrap()
        }
    }

    fn named(list: &serde_json::Value, user: &UserId) -> bool {
        list.as_array()
            .is_some_and(|l| l.iter().any(|u| u.as_str() == Some(user.as_str())))
    }

    /// Synapse puts a user newly invited to (or knocking on) a room the syncer is in into
    /// `device_lists.changed`, before they join; Sytest's cross-signing federation tests
    /// wait for exactly that. This server named only joiners.
    #[tokio::test]
    async fn an_invite_puts_the_invitee_in_sync_and_keys_changes_before_they_join() {
        let w = World::new().await;
        let (_, from) = w.sync(None).await;
        w.act(&w.alice, Action::Invite, &w.bob).await;
        let (response, to) = w.sync(Some(from)).await;
        assert!(
            named(&response["device_lists"]["changed"], &w.bob),
            "the invitee is in changed: {response}"
        );
        let changes = w.changes(&from, Some(&to)).await;
        assert_eq!(changes.changed, vec![w.bob.clone()]);
        assert!(changes.left.is_empty());
    }

    /// Sytest's "New users appear in /keys/changes": somebody joining after `from` is
    /// `changed` between the tokens, though they never touched a key.
    #[tokio::test]
    async fn a_user_who_joined_between_the_tokens_is_changed() {
        let w = World::new().await;
        let (_, from) = w.sync(None).await;
        w.act(&w.bob, Action::Join, &w.bob).await;
        let (response, to) = w.sync(Some(from)).await;
        assert!(named(&response["device_lists"]["changed"], &w.bob));
        assert_eq!(
            w.changes(&from, Some(&to)).await.changed,
            vec![w.bob.clone()]
        );
        // With no `to`: up to now, the same.
        assert_eq!(w.changes(&from, None).await.changed, vec![w.bob.clone()]);
        // And from `to` on, nothing happened.
        assert_eq!(w.changes(&to, None).await, DeviceListChanges::default());
    }

    /// Sytest's two "Get left notifs ... in sync and /keys/changes" tests: the other user
    /// leaving, and the syncing user leaving, both make the other user `left` between the
    /// tokens, as the sync between them said.
    #[tokio::test]
    async fn a_leave_from_either_side_is_left_between_the_tokens() {
        for alice_leaves in [false, true] {
            let w = World::new().await;
            w.act(&w.bob, Action::Join, &w.bob).await;
            let (_, warm) = w.sync(None).await;
            let (_, from) = w.sync(Some(warm)).await;
            if alice_leaves {
                w.act(&w.alice, Action::Leave, &w.alice).await;
            } else {
                w.act(&w.bob, Action::Leave, &w.bob).await;
            }
            let (response, to) = w.sync(Some(from)).await;
            assert!(
                named(&response["device_lists"]["left"], &w.bob),
                "alice_leaves={alice_leaves}: {response}"
            );
            let changes = w.changes(&from, Some(&to)).await;
            assert_eq!(
                changes.left,
                vec![w.bob.clone()],
                "alice_leaves={alice_leaves}"
            );
            assert!(changes.changed.is_empty(), "alice_leaves={alice_leaves}");
        }
    }

    /// Sytest's "If user leaves room, remote user changes device and rejoins we see update in
    /// /sync and /keys/changes": the syncing user leaves and comes back within one batch. The
    /// others in the room are `changed` (their devices may have changed unheard), in `/sync`
    /// and between the tokens, and nobody -- the user least of all -- is `left`.
    #[tokio::test]
    async fn the_users_own_leave_and_return_makes_the_others_changed() {
        let w = World::new().await;
        w.act(&w.bob, Action::Join, &w.bob).await;
        let (_, warm) = w.sync(None).await;
        let (_, from) = w.sync(Some(warm)).await;
        w.act(&w.alice, Action::Leave, &w.alice).await;
        w.act(&w.alice, Action::Join, &w.alice).await;
        let (response, to) = w.sync(Some(from)).await;
        assert!(
            named(&response["device_lists"]["changed"], &w.bob),
            "bob is changed: {response}"
        );
        assert_eq!(response["device_lists"]["left"], serde_json::json!([]));
        let changes = w.changes(&from, Some(&to)).await;
        assert_eq!(changes.changed, vec![w.bob.clone()]);
        assert!(changes.left.is_empty());
    }

    /// A user-signing key change is the user's alone: it is in their own `changed`, and no one
    /// else's.
    #[tokio::test]
    async fn a_change_of_the_users_own_is_theirs_alone() {
        let w = World::new().await;
        w.act(&w.bob, Action::Join, &w.bob).await;
        let (_, warm) = w.sync(None).await;
        let (_, from) = w.sync(Some(warm)).await;
        DeviceKeyStore::record_own_device_list_change(&*w.e2e, &w.bob)
            .await
            .unwrap();
        let (response, to) = w.sync(Some(from)).await;
        assert_eq!(response["device_lists"]["changed"], serde_json::json!([]));
        assert!(w.changes(&from, Some(&to)).await.changed.is_empty());

        DeviceKeyStore::record_own_device_list_change(&*w.e2e, &w.alice)
            .await
            .unwrap();
        let (response, _) = w.sync(Some(to)).await;
        assert!(named(&response["device_lists"]["changed"], &w.alice));
        assert_eq!(w.changes(&to, None).await.changed, vec![w.alice.clone()]);
    }

    /// Sytest's "If remote user leaves room, changes device and rejoins we see update in
    /// /keys/changes": somebody who left and came back between the tokens is `changed`, not
    /// `left`; and a device change by somebody sharing a room is `changed` from the stream.
    #[tokio::test]
    async fn a_leave_and_rejoin_between_the_tokens_is_changed() {
        let w = World::new().await;
        w.act(&w.bob, Action::Join, &w.bob).await;
        let (_, warm) = w.sync(None).await;
        let (_, from) = w.sync(Some(warm)).await;
        w.act(&w.bob, Action::Leave, &w.bob).await;
        w.act(&w.bob, Action::Join, &w.bob).await;
        let (_, to) = w.sync(Some(from)).await;
        let changes = w.changes(&from, Some(&to)).await;
        assert_eq!(changes.changed, vec![w.bob.clone()]);
        assert!(changes.left.is_empty());

        DeviceKeyStore::record_device_list_change(&*w.e2e, &w.bob)
            .await
            .unwrap();
        let stranger = user_id!("@stranger:keys.test");
        DeviceKeyStore::record_device_list_change(&*w.e2e, stranger)
            .await
            .unwrap();
        let changes = w.changes(&to, None).await;
        assert_eq!(
            changes.changed,
            vec![w.bob.clone()],
            "a stranger's device change is not alice's to hear about"
        );
    }
}
