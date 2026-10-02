//! Implements `hs_admin::user_moderation::UserActivitySource` over [`RoomRegistry`]: the room
//! side of the admin API's per-user operations -- a user's memberships
//! (`users.memberships.list`), what they have done (`users.statistics.get`), and redacting what
//! they sent (`users.redact_events`).
//!
//! # Cost
//!
//! There is no index from a user to the rooms they have *any* membership in (only to the rooms
//! they are joined to), so each call walks every room this server holds, loading each into the
//! registry, as `RoomRegistryDirectory::list_rooms` already does for `GET /rooms`. Rooms the user
//! has never had a membership in are skipped after one state lookup. Fine for an administrator's
//! page; not something to call per request.
//!
//! # Who redacts
//!
//! A redaction is an event, and needs a sender this server can sign for who is allowed to send
//! it. The user themself, while still joined, can always redact their own events. Otherwise the
//! redaction is tried as each of this server's joined members, most powerful first, and the
//! first the room's auth rules accept is used. An event nobody here may redact is an error for
//! that one event, reported by the task, never skipped silently.
//!
//! # Leaving every room
//!
//! `leave_all_rooms` (what `users.deactivate` with `erase: true` does before the account's data
//! goes) leaves each room the user is joined to, invited to or knocking on, as the user, the way
//! `POST /rooms/{id}/leave` does: through the room when a user of this server is joined to it,
//! else through another server in the room ([`RemoteJoin::leave`], when the hook is installed
//! with [`RoomRegistryUserActivity::with_remote_join`]), and if none will take it, the invite or
//! knock is rejected here alone. A room that cannot be left is reported with its reason and the
//! rest are still left; bans and earlier leaves are left as they are.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hs_admin::sources::SourceError;
use hs_admin::user_moderation::{
    AdminUserMembership, LeaveFailure, LeaveReport, RedactTarget, UserActivityCounts,
    UserActivitySource,
};
use hs_kv::KvBackend;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::event::Event;
use ruma::{EventId, OwnedUserId, RoomId, UserId};

use crate::actor::RoomActor;
use crate::actor::RoomActorHandle;
use crate::error::RoomError;
use crate::membership::Action;
use crate::registry::RoomRegistry;
use crate::remote_join::RemoteJoin;

/// How many senders a redaction is tried as before the event is reported as unredactable.
const MAX_REDACTION_SENDERS: usize = 5;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

fn to_source_error(e: RoomError) -> SourceError {
    match e {
        RoomError::RoomNotFound(_) | RoomError::EventNotFound(_) => SourceError::NotFound,
        RoomError::Forbidden(msg) | RoomError::BadRequest(msg) => SourceError::Invalid(msg),
        other => SourceError::Unavailable(other.to_string()),
    }
}

fn parse_user_id(raw: &str) -> Result<OwnedUserId, SourceError> {
    UserId::parse(raw).map_err(|e| SourceError::Invalid(format!("not a valid user id: {e}")))
}

fn content_str<'a>(event: &'a Event, field: &str) -> Option<&'a str> {
    event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|c| c.get(field))
        .and_then(CanonicalJsonValue::as_str)
}

/// The user's current membership event in this room, if any.
fn membership_of<'a, B: KvBackend>(actor: &'a RoomActor<B>, user: &UserId) -> Option<&'a Event> {
    actor
        .state_event("m.room.member", user.as_str())
        .ok()
        .flatten()
}

/// Whether `event` is one `users.redact_events` redacts: a live, non-state event of `user`'s
/// that is not itself a redaction.
fn redactable(event: &Event, user: &UserId) -> bool {
    let header = event.header();
    header.sender == user
        && !header.is_state_event()
        && header.event_type != "m.room.redaction"
        && !header.flags.is_redacted()
        && !header.flags.is_rejected()
}

/// Adapts a [`RoomRegistry`] to [`UserActivitySource`].
pub struct RoomRegistryUserActivity<B: KvBackend> {
    registry: Arc<RoomRegistry<B>>,
    /// How a leave reaches a room no user of this server is joined to (the same hook
    /// `RoomState::remote_join` gives the client routes). `None`: such a room's invite or knock
    /// is rejected here alone.
    remote_join: Option<Arc<dyn RemoteJoin>>,
}

impl<B: KvBackend + 'static> RoomRegistryUserActivity<B> {
    /// Wraps `registry`.
    #[must_use]
    pub fn new(registry: Arc<RoomRegistry<B>>) -> Self {
        Self {
            registry,
            remote_join: None,
        }
    }

    /// Installs the federation hook `leave_all_rooms` sends a leave through for a room no user
    /// of this server is joined to -- `RoomState::remote_join`, so that an erased user's
    /// rejection of a remote invite reaches the inviting server the way their own
    /// `POST /rooms/{id}/leave` would.
    #[must_use]
    pub fn with_remote_join(mut self, remote_join: Option<Arc<dyn RemoteJoin>>) -> Self {
        self.remote_join = remote_join;
        self
    }

    /// Leaves one room as `user`, the way `crate::routes::membership::act` does for
    /// `Action::Leave`: through another server when nobody of this server is joined
    /// (`servers_to_join_through`), rejecting an invite or knock locally if no server takes
    /// the leave; through the room otherwise.
    async fn leave_room(
        &self,
        handle: &RoomActorHandle<B>,
        room_id: &RoomId,
        user: &UserId,
    ) -> Result<(), RoomError> {
        if let Some(remote) = &self.remote_join
            && let Some(servers) = handle.query(|actor| actor.servers_to_join_through()).await
        {
            if let Err(error) = remote
                .leave(user, room_id, &servers, serde_json::json!({}))
                .await
            {
                tracing::info!(%room_id, %user, %error, "no server in the room took the leave; rejecting locally");
                handle
                    .reject_out_of_band(user.to_owned(), serde_json::json!({}), now_ms())
                    .await
                    .map_err(|_| error)?;
            }
            return Ok(());
        }
        handle
            .membership(
                user.to_owned(),
                Action::Leave,
                user.to_owned(),
                serde_json::json!({}),
                now_ms(),
            )
            .await?;
        Ok(())
    }

    /// Every room `user` has a membership event in, with its handle.
    async fn rooms_of(
        &self,
        user: &UserId,
    ) -> Result<Vec<(ruma::OwnedRoomId, RoomActorHandle<B>)>, SourceError> {
        let mut out = Vec::new();
        for room_id in self.registry.list_all_room_ids().map_err(to_source_error)? {
            let handle = match self.registry.get_or_load(&room_id).await {
                Ok(handle) => handle,
                Err(RoomError::RoomNotFound(_)) => continue,
                Err(e) => return Err(to_source_error(e)),
            };
            let who = user.to_owned();
            let member = handle
                .query(move |actor| membership_of(actor, &who).is_some())
                .await;
            if member {
                out.push((room_id, handle));
            }
        }
        Ok(out)
    }

    /// Who may send the redaction of `target` (which `owner` sent), in the order to try them:
    /// the owner while joined, then this server's joined members by power, strongest first.
    fn redaction_senders(
        actor: &RoomActor<B>,
        owner: &UserId,
    ) -> Result<Vec<OwnedUserId>, RoomError> {
        let mut senders = Vec::new();
        if membership_of(actor, owner).and_then(|e| content_str(e, "membership")) == Some("join") {
            senders.push(owner.to_owned());
        }
        let rules = hs_model::room_version::rules_for(actor.room_version())
            .ok_or_else(|| RoomError::UnsupportedRoomVersion(actor.room_version().to_string()))?;
        let levels = match actor
            .state_event("m.room.power_levels", "")?
            .and_then(|e| e.json().get("content"))
            .and_then(CanonicalJsonValue::as_object)
        {
            Some(content) => hs_model::power_levels::PowerLevels::parse(content, &rules)
                .map_err(|e| RoomError::Internal(e.to_string()))?,
            None => hs_model::power_levels::PowerLevels::default(),
        };
        let creator = actor
            .state_event("m.room.create", "")?
            .map(|e| e.header().sender.clone());
        let mut others: Vec<(i64, OwnedUserId)> = actor
            .joined_members()?
            .into_iter()
            .filter_map(|m| UserId::parse(m.header().state_key.as_deref()?).ok())
            .filter(|u| u.server_name() == owner.server_name() && u != owner)
            .map(|u| {
                // A room version that privileges its creator gives them unlimited power; the
                // parsed levels do not say so, so put them first.
                let power = if rules.explicitly_privilege_room_creators
                    && creator.as_deref() == Some(u.as_ref())
                {
                    i64::MAX
                } else {
                    levels.user_power(&u)
                };
                (power, u)
            })
            .collect();
        others.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        senders.extend(others.into_iter().map(|(_, u)| u));
        senders.truncate(MAX_REDACTION_SENDERS);
        Ok(senders)
    }
}

#[async_trait]
impl<B: KvBackend + 'static> UserActivitySource for RoomRegistryUserActivity<B> {
    async fn memberships(&self, user_id: &str) -> Result<Vec<AdminUserMembership>, SourceError> {
        let user = parse_user_id(user_id)?;
        let mut out = Vec::new();
        for (room_id, handle) in self.rooms_of(&user).await? {
            let who = user.clone();
            let membership = handle
                .query(move |actor| {
                    let member = membership_of(actor, &who)?;
                    let room_name = actor
                        .state_event("m.room.name", "")
                        .ok()
                        .flatten()
                        .and_then(|e| content_str(e, "name"))
                        .map(str::to_owned);
                    Some(AdminUserMembership {
                        room_id: room_id.to_string(),
                        room_name,
                        user_id: who.to_string(),
                        membership: content_str(member, "membership")
                            .unwrap_or("leave")
                            .to_owned(),
                        display_name: content_str(member, "displayname").map(str::to_owned),
                        avatar_url: content_str(member, "avatar_url").map(str::to_owned),
                    })
                })
                .await;
            out.extend(membership);
        }
        Ok(out)
    }

    async fn statistics(&self, user_id: &str) -> Result<UserActivityCounts, SourceError> {
        let user = parse_user_id(user_id)?;
        let mut counts = UserActivityCounts::default();
        for (_, handle) in self.rooms_of(&user).await? {
            let who = user.clone();
            let room = handle
                .query(move |actor| {
                    let mut c = UserActivityCounts::default();
                    if membership_of(actor, &who).and_then(|e| content_str(e, "membership"))
                        == Some("join")
                    {
                        c.joins_count = 1;
                    }
                    for (_, event) in actor.events_after(0, usize::MAX) {
                        let header = event.header();
                        if header.sender != who || header.flags.is_rejected() {
                            continue;
                        }
                        c.events_sent_count += 1;
                        match header.event_type.as_str() {
                            "m.room.create" => c.rooms_created_count += 1,
                            "m.room.member"
                                if content_str(event, "membership") == Some("invite")
                                    && header.state_key.as_deref() != Some(who.as_str()) =>
                            {
                                c.invites_sent_count += 1;
                            }
                            _ => {}
                        }
                    }
                    c
                })
                .await;
            counts.joins_count += room.joins_count;
            counts.events_sent_count += room.events_sent_count;
            counts.rooms_created_count += room.rooms_created_count;
            counts.invites_sent_count += room.invites_sent_count;
        }
        Ok(counts)
    }

    async fn events_to_redact(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RedactTarget>, SourceError> {
        let user = parse_user_id(user_id)?;
        let rooms = match room_id {
            Some(raw) => {
                let room_id = RoomId::parse(raw)
                    .map_err(|e| SourceError::Invalid(format!("not a valid room id: {e}")))?;
                let handle = self
                    .registry
                    .get_or_load(&room_id)
                    .await
                    .map_err(to_source_error)?;
                vec![(room_id, handle)]
            }
            None => self.rooms_of(&user).await?,
        };
        let mut found: Vec<(i64, RedactTarget)> = Vec::new();
        for (room_id, handle) in rooms {
            let who = user.clone();
            let events = handle
                .query(move |actor| {
                    actor
                        .events_after(0, usize::MAX)
                        .into_iter()
                        .filter(|(_, e)| redactable(e, &who))
                        .map(|(_, e)| (e.header().origin_server_ts, e.event_id().to_string()))
                        .collect::<Vec<_>>()
                })
                .await;
            found.extend(events.into_iter().map(|(ts, event_id)| {
                (
                    ts,
                    RedactTarget {
                        room_id: room_id.to_string(),
                        event_id,
                    },
                )
            }));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.event_id.cmp(&b.1.event_id)));
        let mut targets: Vec<RedactTarget> = found.into_iter().map(|(_, t)| t).collect();
        if let Some(limit) = limit {
            targets.truncate(limit);
        }
        Ok(targets)
    }

    async fn leave_all_rooms(&self, user_id: &str) -> Result<LeaveReport, SourceError> {
        let user = parse_user_id(user_id)?;
        let mut report = LeaveReport::default();
        for (room_id, handle) in self.rooms_of(&user).await? {
            let who = user.clone();
            let current = handle
                .query(move |actor| {
                    membership_of(actor, &who)
                        .and_then(|e| content_str(e, "membership"))
                        .map(str::to_owned)
                })
                .await;
            if !matches!(current.as_deref(), Some("join" | "invite" | "knock")) {
                continue;
            }
            match self.leave_room(&handle, &room_id, &user).await {
                Ok(()) => {
                    tracing::debug!(room = %room_id, %user, "left for an administrator");
                    report.rooms_left.push(room_id.to_string());
                }
                Err(error) => {
                    tracing::warn!(room = %room_id, %user, %error, "could not leave the room for an administrator");
                    report.rooms_failed.push(LeaveFailure {
                        room_id: room_id.to_string(),
                        reason: error.to_string(),
                    });
                }
            }
        }
        Ok(report)
    }

    async fn redact_event(
        &self,
        target: &RedactTarget,
        reason: Option<&str>,
    ) -> Result<(), SourceError> {
        let room_id = RoomId::parse(&target.room_id)
            .map_err(|e| SourceError::Invalid(format!("not a valid room id: {e}")))?;
        let event_id = EventId::parse(&target.event_id)
            .map_err(|e| SourceError::Invalid(format!("not a valid event id: {e}")))?;
        let handle = self
            .registry
            .get_or_load(&room_id)
            .await
            .map_err(to_source_error)?;
        let wanted = event_id.clone();
        let senders = handle
            .query(move |actor| {
                let Some(event) = actor.event_by_id(&wanted) else {
                    return Err(RoomError::EventNotFound(wanted.to_string()));
                };
                if event.header().flags.is_redacted() {
                    return Ok(None);
                }
                let owner = event.header().sender.clone();
                Self::redaction_senders(actor, &owner).map(Some)
            })
            .await
            .map_err(to_source_error)?;
        let Some(senders) = senders else {
            // Already redacted, by somebody else or an earlier attempt: done either way.
            return Ok(());
        };
        let mut last_refusal = String::from("no member of this server is in the room");
        for sender in senders {
            match handle
                .redact(
                    sender.clone(),
                    None,
                    format!("admin-redact-{event_id}"),
                    event_id.clone(),
                    reason.map(str::to_owned),
                    now_ms(),
                )
                .await
            {
                Ok(_) => {
                    tracing::debug!(room = %room_id, event = %event_id, %sender, "redacted for an administrator");
                    return Ok(());
                }
                Err(RoomError::Forbidden(msg)) => last_refusal = msg,
                Err(e) => return Err(to_source_error(e)),
            }
        }
        Err(SourceError::Invalid(format!(
            "nobody on this server may redact {event_id} in {room_id}: {last_refusal}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::CreateRoomRequest;
    use crate::identity::HomeserverIdentity;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;
    use serde_json::json;

    async fn registry() -> Arc<RoomRegistry<MemoryBackend>> {
        Arc::new(
            RoomRegistry::open(
                MemoryBackend::new(),
                HomeserverIdentity::for_tests("example.org"),
            )
            .unwrap(),
        )
    }

    /// A [`MemoryBackend`] whose writes fail while `fail` is set: how a room comes to be one
    /// that cannot be left (a user's own leave is always authorised, so only the store can
    /// refuse it).
    #[derive(Clone)]
    struct FailingWrites {
        inner: MemoryBackend,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("the disk is full")]
    struct DiskFull;

    impl hs_kv::KvBackend for FailingWrites {
        type Keyspace = <MemoryBackend as hs_kv::KvBackend>::Keyspace;
        type Snapshot = <MemoryBackend as hs_kv::KvBackend>::Snapshot;
        type Txn = <MemoryBackend as hs_kv::KvBackend>::Txn;

        fn keyspace(&self, name: &str) -> Result<Self::Keyspace, hs_kv::KvError> {
            self.inner.keyspace(name)
        }

        fn snapshot(&self) -> Self::Snapshot {
            self.inner.snapshot()
        }

        fn begin(&self) -> Result<Self::Txn, hs_kv::KvError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(hs_kv::KvError::backend(DiskFull));
            }
            self.inner.begin()
        }

        fn commit(&self, txn: Self::Txn) -> Result<Result<(), hs_kv::Conflict>, hs_kv::KvError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(hs_kv::KvError::backend(DiskFull));
            }
            self.inner.commit(txn)
        }

        fn watch(&self, keyspace: &Self::Keyspace, key: &[u8]) -> hs_kv::watch::Watch {
            self.inner.watch(keyspace, key)
        }
    }

    async fn create<B: KvBackend + 'static>(
        registry: &Arc<RoomRegistry<B>>,
        creator: &ruma::UserId,
        name: &str,
    ) -> ruma::OwnedRoomId {
        registry
            .create_room(
                creator.to_owned(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some(name.to_owned()),
                    ..CreateRoomRequest::default()
                },
                now_ms(),
            )
            .await
            .unwrap()
            .query(|a| a.room_id().to_owned())
            .await
    }

    async fn membership_in<B: KvBackend + 'static>(
        registry: &Arc<RoomRegistry<B>>,
        room_id: &RoomId,
        user: &ruma::UserId,
    ) -> Option<String> {
        let who = user.to_owned();
        registry
            .get_or_load(room_id)
            .await
            .unwrap()
            .query(move |actor| {
                membership_of(actor, &who)
                    .and_then(|e| content_str(e, "membership"))
                    .map(str::to_owned)
            })
            .await
    }

    #[tokio::test]
    async fn leave_all_rooms_leaves_joined_and_invited_rooms_and_reports_one_it_cannot() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let registry = Arc::new(
            RoomRegistry::open(
                FailingWrites {
                    inner: MemoryBackend::new(),
                    fail: fail.clone(),
                },
                HomeserverIdentity::for_tests("example.org"),
            )
            .unwrap(),
        );
        let alice = user_id!("@alice:example.org").to_owned();
        let bob = user_id!("@bob:example.org").to_owned();
        // Bob is joined to one room, invited to another, banned from a third and has left a
        // fourth; only the first two are his to leave.
        let joined = create(&registry, &alice, "Joined").await;
        let invited = create(&registry, &alice, "Invited").await;
        let banned = create(&registry, &alice, "Banned").await;
        let gone = create(&registry, &alice, "Gone").await;
        for (room, actions) in [
            (&joined, vec![(Action::Join, &bob)]),
            (&invited, vec![(Action::Invite, &alice)]),
            (&banned, vec![(Action::Ban, &alice)]),
            (&gone, vec![(Action::Join, &bob), (Action::Leave, &bob)]),
        ] {
            let handle = registry.get_or_load(room).await.unwrap();
            for (action, sender) in actions {
                handle
                    .membership((*sender).clone(), action, bob.clone(), json!({}), now_ms())
                    .await
                    .unwrap();
            }
        }

        let source = RoomRegistryUserActivity::new(registry.clone());
        let report = source.leave_all_rooms(bob.as_str()).await.unwrap();
        let mut left = report.rooms_left.clone();
        left.sort();
        let mut expected = vec![joined.to_string(), invited.to_string()];
        expected.sort();
        assert_eq!(left, expected, "{report:?}");
        assert!(report.rooms_failed.is_empty(), "{report:?}");
        assert_eq!(
            membership_in(&registry, &joined, &bob).await.as_deref(),
            Some("leave")
        );
        assert_eq!(
            membership_in(&registry, &invited, &bob).await.as_deref(),
            Some("leave")
        );
        assert_eq!(
            membership_in(&registry, &banned, &bob).await.as_deref(),
            Some("ban")
        );
        // Alice is still in every room: leaving was bob's alone.
        assert_eq!(
            membership_in(&registry, &joined, &alice).await.as_deref(),
            Some("join")
        );

        // Nothing left to leave: an empty report, not an error.
        let again = source.leave_all_rooms(bob.as_str()).await.unwrap();
        assert_eq!(again, LeaveReport::default());

        // A room whose store refuses the write is reported with the reason and bob stays in it;
        // once the store is well again the same call leaves it.
        let stuck = create(&registry, &alice, "Stuck").await;
        registry
            .get_or_load(&stuck)
            .await
            .unwrap()
            .membership(bob.clone(), Action::Join, bob.clone(), json!({}), now_ms())
            .await
            .unwrap();
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let report = source.leave_all_rooms(bob.as_str()).await.unwrap();
        assert!(report.rooms_left.is_empty(), "{report:?}");
        assert_eq!(report.rooms_failed.len(), 1, "{report:?}");
        assert_eq!(report.rooms_failed[0].room_id, stuck.to_string());
        assert!(
            report.rooms_failed[0].reason.contains("disk is full"),
            "{report:?}"
        );
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            membership_in(&registry, &stuck, &bob).await.as_deref(),
            Some("join")
        );
        let report = source.leave_all_rooms(bob.as_str()).await.unwrap();
        assert_eq!(report.rooms_left, vec![stuck.to_string()]);
        assert_eq!(
            membership_in(&registry, &stuck, &bob).await.as_deref(),
            Some("leave")
        );
    }

    #[tokio::test]
    async fn a_users_rooms_counts_and_messages_are_found_and_redacted() {
        let registry = registry().await;
        let alice = user_id!("@alice:example.org").to_owned();
        let bob = user_id!("@bob:example.org").to_owned();
        let handle = registry
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    name: Some("Lobby".to_owned()),
                    ..CreateRoomRequest::default()
                },
                now_ms(),
            )
            .await
            .unwrap();
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        handle
            .membership(
                bob.clone(),
                crate::membership::Action::Join,
                bob.clone(),
                json!({}),
                now_ms(),
            )
            .await
            .unwrap();
        let mut sent = Vec::new();
        for body in ["one", "two"] {
            let event = handle
                .send_event(
                    bob.clone(),
                    "m.room.message".to_owned(),
                    None,
                    json!({"msgtype": "m.text", "body": body}),
                    None,
                    now_ms(),
                )
                .await
                .unwrap();
            sent.push(event.event_id().to_string());
        }
        // Bob leaves: his messages are redacted by alice, the room's most powerful member.
        handle
            .membership(
                bob.clone(),
                crate::membership::Action::Leave,
                bob.clone(),
                json!({}),
                now_ms(),
            )
            .await
            .unwrap();

        let source = RoomRegistryUserActivity::new(registry.clone());
        let memberships = source.memberships(bob.as_str()).await.unwrap();
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].membership, "leave");
        assert_eq!(memberships[0].room_name.as_deref(), Some("Lobby"));
        let stats = source.statistics(alice.as_str()).await.unwrap();
        assert_eq!(stats.rooms_created_count, 1);
        assert_eq!(stats.joins_count, 1);
        let stats = source.statistics(bob.as_str()).await.unwrap();
        assert_eq!(stats.joins_count, 0);
        assert!(stats.events_sent_count >= 4);

        let targets = source
            .events_to_redact(bob.as_str(), None, None)
            .await
            .unwrap();
        assert_eq!(targets.len(), 2);
        let limited = source
            .events_to_redact(bob.as_str(), Some(room_id.as_str()), Some(1))
            .await
            .unwrap();
        assert_eq!(limited.len(), 1);
        for target in &targets {
            source.redact_event(target, Some("spam")).await.unwrap();
        }
        // Redacting again is a no-op, and nothing is left to redact.
        source.redact_event(&targets[0], None).await.unwrap();
        assert!(
            source
                .events_to_redact(bob.as_str(), None, None)
                .await
                .unwrap()
                .is_empty()
        );
        let redactor = handle
            .query(move |a| {
                a.events_after(0, usize::MAX)
                    .into_iter()
                    .filter(|(_, e)| e.header().event_type == "m.room.redaction")
                    .map(|(_, e)| e.header().sender.clone())
                    .collect::<Vec<_>>()
            })
            .await;
        assert_eq!(redactor, vec![alice.clone(), alice.clone()]);
        assert!(matches!(
            source
                .events_to_redact(bob.as_str(), Some("!nope:example.org"), None)
                .await,
            Err(SourceError::NotFound)
        ));
    }
}
