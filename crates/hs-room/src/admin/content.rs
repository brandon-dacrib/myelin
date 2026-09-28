//! `hs_admin::rooms::RoomContentSource` over the room registry: what the admin API's room long
//! tail reads and does (state, timeline, events, aliases, hierarchy, joins, forward extremities,
//! media, purging history and deleting a room). The actor-level work is in
//! `crate::actor::admin_ops`; this module adapts it to the admin API's shapes and orders the
//! steps of the two long operations.

use std::collections::{HashSet, VecDeque};
use std::str::FromStr;

use async_trait::async_trait;
use hs_admin::model::AdminRoomMember;
use hs_admin::rooms::{
    AdminEventContext, AdminForwardExtremity, AdminHierarchyNode, AdminRoomAlias, AdminRoomEvent,
    AdminStateEvent, DeleteRoomOutcome, DeleteRoomRequest, ForwardExtremitiesPruned, Progress,
    PurgeHistoryOutcome, PurgeHistoryRequest, RoomContentSource, TimelineDirection, TimelinePage,
};
use hs_admin::sources::SourceError;
use hs_kv::KvBackend;
use hs_model::Event;
use ruma::{EventId, OwnedUserId, RoomAliasId, UserId};
use serde_json::Value;

use super::{RoomRegistryDirectory, now_ms, parse_room_id, to_source_error};
use crate::error::RoomError;
use crate::membership::Action;
use crate::routes::render::client_event_json;
use crate::timeline::{Direction, PaginationToken};

/// How many events one purge batch rewrites: small enough that the room's actor is not held
/// for long, so the room keeps serving between batches.
const PURGE_BATCH: usize = 500;

fn admin_event(event: &Event, room_id: &str) -> AdminRoomEvent {
    let json = client_event_json(event);
    AdminRoomEvent {
        event_id: event.event_id().to_string(),
        room_id: room_id.to_owned(),
        event_type: event.header().event_type.clone(),
        sender: event.header().sender.to_string(),
        content: json.get("content").cloned().unwrap_or(Value::Null),
        origin_server_ts: event.header().origin_server_ts,
        state_key: event.header().state_key.clone(),
        redacted: event.header().flags.is_redacted(),
        redacts: json
            .get("redacts")
            .or_else(|| json.get("content").and_then(|c| c.get("redacts")))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn state_event(event: &Event) -> AdminStateEvent {
    let json = client_event_json(event);
    AdminStateEvent {
        event_id: event.event_id().to_string(),
        event_type: event.header().event_type.clone(),
        state_key: event.header().state_key.clone().unwrap_or_default(),
        sender: event.header().sender.to_string(),
        content: json.get("content").cloned().unwrap_or(Value::Null),
        origin_server_ts: event.header().origin_server_ts,
    }
}

fn extremity(event: &Event) -> AdminForwardExtremity {
    AdminForwardExtremity {
        event_id: event.event_id().to_string(),
        event_type: event.header().event_type.clone(),
        sender: event.header().sender.to_string(),
        depth: event.header().depth,
        origin_server_ts: event.header().origin_server_ts,
        state_key: event.header().state_key.clone(),
    }
}

fn direction(d: TimelineDirection) -> Direction {
    match d {
        TimelineDirection::Forward => Direction::Forward,
        TimelineDirection::Backward => Direction::Backward,
    }
}

fn member_from(event: &Event) -> AdminRoomMember {
    let json = client_event_json(event);
    let content = json.get("content").cloned().unwrap_or(Value::Null);
    let field = |k: &str| content.get(k).and_then(Value::as_str).map(str::to_owned);
    AdminRoomMember {
        user_id: event.header().state_key.clone().unwrap_or_default(),
        membership: field("membership").unwrap_or_else(|| "leave".to_owned()),
        display_name: field("displayname"),
        avatar_url: field("avatar_url"),
    }
}

impl<B: KvBackend + 'static> RoomRegistryDirectory<B> {
    fn own_server(&self) -> String {
        self.registry.server_name().to_string()
    }

    async fn handle(&self, room_id: &str) -> Result<crate::actor::RoomActorHandle<B>, SourceError> {
        let room_id = parse_room_id(room_id)?;
        self.registry
            .get_or_load(&room_id)
            .await
            .map_err(to_source_error)
    }

    /// A local user of this server who exists, or the reason they cannot be used.
    async fn local_user(
        &self,
        raw: &str,
        pointer: &'static str,
    ) -> Result<OwnedUserId, SourceError> {
        let user = UserId::parse(raw).map_err(|e| SourceError::InvalidField {
            pointer,
            detail: format!("{raw:?} is not a user id: {e}"),
        })?;
        if user.server_name().as_str() != self.own_server() {
            return Err(SourceError::InvalidField {
                pointer,
                detail: format!("{user} is not a user of this server"),
            });
        }
        if let Some(auth) = &self.auth {
            match auth.store.get_user(&user).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(SourceError::InvalidField {
                        pointer,
                        detail: format!("{user} does not exist"),
                    });
                }
                Err(e) => return Err(SourceError::Unavailable(e.to_string())),
            }
        }
        Ok(user.to_owned())
    }

    /// `user`'s current profile as `m.room.member` content fields, when this directory can see
    /// profiles.
    async fn profile(&self, user: &UserId) -> Value {
        let mut content = serde_json::json!({});
        if let Some(auth) = &self.auth
            && let Ok(Some(record)) = auth.store.get_user(user).await
        {
            if let Some(name) = record.display_name {
                content["displayname"] = Value::String(name);
            }
            if let Some(url) = record.avatar_url {
                content["avatar_url"] = Value::String(url);
            }
        }
        content
    }

    /// Joins `user` to the room behind `handle`, inviting them first from a local member with
    /// the power to when the join rules do not let them in by themselves.
    async fn force_join(
        &self,
        handle: &crate::actor::RoomActorHandle<B>,
        user: &UserId,
    ) -> Result<Event, RoomError> {
        let profile = self.profile(user).await;
        let first = handle
            .membership(
                user.to_owned(),
                Action::Join,
                user.to_owned(),
                profile.clone(),
                now_ms(),
            )
            .await;
        let refusal = match first {
            Ok(event) => return Ok(event),
            Err(RoomError::Forbidden(reason)) => reason,
            Err(other) => return Err(other),
        };
        let inviters = handle
            .query(|actor| actor.local_inviters())
            .await
            .unwrap_or_default();
        for inviter in inviters {
            if inviter == user {
                continue;
            }
            let invited = handle
                .membership(
                    inviter.clone(),
                    Action::Invite,
                    user.to_owned(),
                    profile.clone(),
                    now_ms(),
                )
                .await;
            if invited.is_ok() {
                return handle
                    .membership(
                        user.to_owned(),
                        Action::Join,
                        user.to_owned(),
                        profile,
                        now_ms(),
                    )
                    .await;
            }
        }
        Err(RoomError::Forbidden(refusal))
    }
}

#[async_trait]
impl<B: KvBackend + 'static> RoomContentSource for RoomRegistryDirectory<B> {
    fn observe(&self, operation: &str, outcome: &str, elapsed: std::time::Duration) {
        if let Some(observer) = &self.observer {
            observer(operation, outcome, elapsed);
        }
    }

    async fn exists(&self, room_id: &str) -> Result<bool, SourceError> {
        match self.handle(room_id).await {
            Ok(_) => Ok(true),
            Err(SourceError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn state(&self, room_id: &str) -> Result<Vec<AdminStateEvent>, SourceError> {
        self.handle(room_id)
            .await?
            .query(|actor| {
                actor
                    .full_state()
                    .map(|events| events.into_iter().map(state_event).collect())
            })
            .await
            .map_err(to_source_error)
    }

    async fn timeline(
        &self,
        room_id: &str,
        from: Option<&str>,
        dir: TimelineDirection,
        limit: usize,
    ) -> Result<TimelinePage, SourceError> {
        let from = match from {
            Some(raw) => {
                let token =
                    PaginationToken::from_str(raw).map_err(|_| SourceError::InvalidField {
                        pointer: "/cursor",
                        detail: format!("{raw:?} is not a cursor this timeline handed out"),
                    })?;
                if token.direction != direction(dir) {
                    return Err(SourceError::InvalidField {
                        pointer: "/cursor",
                        detail: "that cursor continues in the other direction".to_owned(),
                    });
                }
                Some(token)
            }
            None => None,
        };
        let room = room_id.to_owned();
        Ok(self
            .handle(room_id)
            .await?
            .query(move |actor| {
                let (events, next) = actor.paginate(from, direction(dir), limit);
                TimelinePage {
                    events: events.into_iter().map(|e| admin_event(e, &room)).collect(),
                    next: next.map(|t| t.to_string()),
                }
            })
            .await)
    }

    async fn event(
        &self,
        room_id: Option<&str>,
        event_id: &str,
    ) -> Result<Option<AdminRoomEvent>, SourceError> {
        let Ok(parsed) = EventId::parse(event_id) else {
            return Ok(None);
        };
        let room_id = match room_id {
            Some(room_id) => room_id.to_owned(),
            None => match self
                .registry
                .find_event_globally(&parsed)
                .map_err(to_source_error)?
            {
                Some(row) if !row.purged => row.room_id,
                _ => return Ok(None),
            },
        };
        let handle = match self.handle(&room_id).await {
            Ok(handle) => handle,
            // The room an event row names is gone (deleted): so is the event.
            Err(SourceError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(handle
            .query(move |actor| actor.event_by_id(&parsed).map(|e| admin_event(e, &room_id)))
            .await)
    }

    async fn event_at(
        &self,
        room_id: &str,
        ts: i64,
        dir: TimelineDirection,
    ) -> Result<Option<AdminRoomEvent>, SourceError> {
        let room = room_id.to_owned();
        Ok(self
            .handle(room_id)
            .await?
            .query(move |actor| {
                actor
                    .event_nearest(ts, direction(dir))
                    .map(|e| admin_event(e, &room))
            })
            .await)
    }

    async fn context(
        &self,
        room_id: &str,
        event_id: &str,
        limit: usize,
    ) -> Result<Option<AdminEventContext>, SourceError> {
        let Ok(parsed) = EventId::parse(event_id) else {
            return Ok(None);
        };
        let room = room_id.to_owned();
        self.handle(room_id)
            .await?
            .query(move |actor| {
                let Some(target) = actor.event_by_id(&parsed) else {
                    return Ok(None);
                };
                let Some(pos) = actor.timeline_position(&parsed) else {
                    return Ok(None);
                };
                let (before, _) = actor.paginate(
                    Some(PaginationToken::new(pos, Direction::Backward)),
                    Direction::Backward,
                    limit,
                );
                let (after, _) = actor.paginate(
                    Some(PaginationToken::new(pos, Direction::Forward)),
                    Direction::Forward,
                    limit,
                );
                let state = actor
                    .state_at_event(&parsed)?
                    .map(|s| s.state.iter().map(state_event).collect())
                    .unwrap_or_default();
                Ok(Some(AdminEventContext {
                    event: admin_event(target, &room),
                    events_before: before.into_iter().map(|e| admin_event(e, &room)).collect(),
                    events_after: after.into_iter().map(|e| admin_event(e, &room)).collect(),
                    state,
                }))
            })
            .await
            .map_err(to_source_error)
    }

    async fn aliases(&self, room_id: &str) -> Result<Vec<AdminRoomAlias>, SourceError> {
        self.handle(room_id)
            .await?
            .query(|actor| {
                let canonical: HashSet<String> = actor
                    .state_event("m.room.canonical_alias", "")?
                    .map(|e| {
                        let json = client_event_json(e);
                        let content = json.get("content").cloned().unwrap_or(Value::Null);
                        content
                            .get("alias")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .into_iter()
                            .collect()
                    })
                    .unwrap_or_default();
                let mut out = Vec::new();
                for alias in actor.list_aliases()? {
                    let creator = RoomAliasId::parse(&alias)
                        .ok()
                        .and_then(|a| actor.alias_creator(&a).ok().flatten())
                        .map(|u| u.to_string());
                    out.push(AdminRoomAlias {
                        canonical: canonical.contains(&alias),
                        alias,
                        created_at: None,
                        creator,
                    });
                }
                Ok(out)
            })
            .await
            .map_err(to_source_error)
    }

    async fn add_alias(
        &self,
        room_id: &str,
        alias: &str,
        by: &str,
    ) -> Result<AdminRoomAlias, SourceError> {
        let parsed = RoomAliasId::parse(alias).map_err(|e| SourceError::InvalidField {
            pointer: "/alias",
            detail: format!("{alias:?} is not a room alias: {e}"),
        })?;
        if parsed.server_name().as_str() != self.own_server() {
            return Err(SourceError::InvalidField {
                pointer: "/alias",
                detail: format!("{alias} is not an alias on this server"),
            });
        }
        let creator = UserId::parse(by).map_err(|_| {
            SourceError::Invalid(format!("{by} cannot be recorded as an alias's creator"))
        })?;
        let handle = self.handle(room_id).await?;
        let alias_owned = parsed.to_owned();
        handle
            .query(move |actor| actor.create_alias(&alias_owned, &creator))
            .await
            .map_err(|e| match e {
                RoomError::RoomAlreadyExists(_) => {
                    SourceError::Conflict(format!("{alias} is already in use"))
                }
                other => to_source_error(other),
            })?;
        Ok(AdminRoomAlias {
            alias: alias.to_owned(),
            created_at: None,
            creator: Some(by.to_owned()),
            canonical: false,
        })
    }

    async fn remove_alias(&self, room_id: &str, alias: &str) -> Result<(), SourceError> {
        let parsed = RoomAliasId::parse(alias).map_err(|_| SourceError::NotFound)?;
        let owned = alias.to_owned();
        self.handle(room_id)
            .await?
            .query(move |actor| {
                if !actor.list_aliases()?.contains(&owned) {
                    return Err(RoomError::RoomNotFound(owned));
                }
                actor.remove_alias(&parsed)
            })
            .await
            .map_err(to_source_error)
    }

    async fn hierarchy(
        &self,
        room_id: &str,
        max_depth: u32,
    ) -> Result<Vec<AdminHierarchyNode>, SourceError> {
        // The room asked about must exist; its children need not.
        self.handle(room_id).await?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([(room_id.to_owned(), 0u32)]);
        while let Some((id, depth)) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let node = match self.handle(&id).await {
                Ok(handle) => {
                    let summary = handle.admin_summary().await.map_err(to_source_error)?;
                    let children = handle
                        .query(|actor| {
                            actor.full_state().map(|events| {
                                events
                                    .into_iter()
                                    .filter(|e| e.header().event_type == "m.space.child")
                                    .filter(|e| {
                                        client_event_json(e)
                                            .get("content")
                                            .and_then(|c| c.get("via"))
                                            .and_then(Value::as_array)
                                            .is_some_and(|via| !via.is_empty())
                                    })
                                    .filter_map(|e| e.header().state_key.clone())
                                    .collect::<Vec<_>>()
                            })
                        })
                        .await
                        .map_err(to_source_error)?;
                    AdminHierarchyNode {
                        room_id: id.clone(),
                        name: summary.name,
                        topic: summary.topic,
                        canonical_alias: summary.canonical_alias,
                        room_type: summary.room_type,
                        join_rule: Some(summary.join_rule),
                        joined_members_count: Some(summary.joined_members_count),
                        depth,
                        known: true,
                        children,
                    }
                }
                Err(SourceError::NotFound | SourceError::Invalid(_)) => AdminHierarchyNode {
                    room_id: id.clone(),
                    name: None,
                    topic: None,
                    canonical_alias: None,
                    room_type: None,
                    join_rule: None,
                    joined_members_count: None,
                    depth,
                    known: false,
                    children: Vec::new(),
                },
                Err(e) => return Err(e),
            };
            if depth < max_depth {
                for child in &node.children {
                    queue.push_back((child.clone(), depth + 1));
                }
            }
            out.push(node);
        }
        Ok(out)
    }

    async fn join(&self, room_id: &str, user_id: &str) -> Result<AdminRoomMember, SourceError> {
        let user = self.local_user(user_id, "/user_id").await?;
        let handle = self.handle(room_id).await?;
        let event = self.force_join(&handle, &user).await.map_err(|e| match e {
            RoomError::Forbidden(reason) | RoomError::BadRequest(reason) => {
                SourceError::Conflict(format!("{user} cannot join {room_id}: {reason}"))
            }
            RoomError::RoomBlocked(_) => SourceError::Conflict(format!("{room_id} is blocked")),
            other => to_source_error(other),
        })?;
        Ok(member_from(&event))
    }

    async fn forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<Vec<AdminForwardExtremity>, SourceError> {
        Ok(self
            .handle(room_id)
            .await?
            .query(|actor| {
                actor
                    .forward_extremity_events()
                    .into_iter()
                    .map(extremity)
                    .collect()
            })
            .await)
    }

    async fn prune_forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<ForwardExtremitiesPruned, SourceError> {
        self.handle(room_id)
            .await?
            .administer(|actor| {
                let deleted = actor.prune_forward_extremities()?;
                Ok(ForwardExtremitiesPruned {
                    deleted: deleted.iter().map(ToString::to_string).collect(),
                    remaining: actor
                        .forward_extremity_events()
                        .into_iter()
                        .map(extremity)
                        .collect(),
                })
            })
            .await
            .map_err(to_source_error)
    }

    async fn media(&self, room_id: &str) -> Result<Vec<(String, String)>, SourceError> {
        self.handle(room_id)
            .await?
            .query(|actor| actor.referenced_media())
            .await
            .map_err(to_source_error)
    }

    async fn purge_history(
        &self,
        room_id: &str,
        request: PurgeHistoryRequest,
        progress: &dyn Progress,
    ) -> Result<PurgeHistoryOutcome, SourceError> {
        let before_event = match &request.before_event_id {
            Some(raw) => Some(EventId::parse(raw).map_err(|e| SourceError::InvalidField {
                pointer: "/before_event_id",
                detail: format!("{raw:?} is not an event id: {e}"),
            })?),
            None => None,
        };
        let handle = self.handle(room_id).await?;
        let before_ts = request.before_ts;
        let delete_local = request.delete_local_events;
        let plan = handle
            .query(move |actor| actor.purge_plan(before_ts, before_event.as_deref(), delete_local))
            .await
            .map_err(|e| match e {
                RoomError::BadRequest(detail) => SourceError::InvalidField {
                    pointer: "/before_event_id",
                    detail,
                },
                other => to_source_error(other),
            })?;
        let total = plan.positions.len() as u64;
        progress
            .report(0, Some(total), "purging the room's history")
            .await;
        let mut purged = 0u64;
        for chunk in plan.positions.chunks(PURGE_BATCH) {
            if progress.cancelled() {
                break;
            }
            let chunk = chunk.to_vec();
            purged += handle
                .administer(move |actor| actor.purge_positions(&chunk))
                .await
                .map_err(to_source_error)?;
            progress
                .report(purged, Some(total), "purging the room's history")
                .await;
        }
        Ok(PurgeHistoryOutcome {
            purged,
            kept_state: plan.kept_state,
            kept_local: plan.kept_local,
        })
    }

    async fn delete_room(
        &self,
        room_id: &str,
        request: DeleteRoomRequest,
        progress: &dyn Progress,
    ) -> Result<DeleteRoomOutcome, SourceError> {
        let parsed_room = parse_room_id(room_id)?;
        let handle = self.handle(room_id).await?;
        let new_room_creator = match &request.new_room {
            Some(new_room) => Some(
                self.local_user(&new_room.creator, "/new_room/creator")
                    .await?,
            ),
            None => None,
        };
        let members = handle
            .query(|actor| actor.local_members_to_remove())
            .await
            .map_err(to_source_error)?;
        // Steps: block, new room, each member, aliases and directory, purge.
        let total = members.len() as u64 + 4;
        let mut step = 0u64;
        let mut outcome = DeleteRoomOutcome {
            blocked: request.block,
            purged: request.purge,
            ..Default::default()
        };

        if request.block {
            self.registry
                .set_room_blocked(
                    &parsed_room,
                    true,
                    Some(format!("deleted by {}", request.requested_by)),
                )
                .map_err(to_source_error)?;
        }
        step += 1;
        progress
            .report(step, Some(total), "blocking the room")
            .await;

        let new_room = match new_room_creator {
            Some(creator) => {
                let name = request
                    .new_room
                    .as_ref()
                    .and_then(|r| r.name.clone())
                    .unwrap_or_else(|| "Content Violation Notification".to_owned());
                let created = self
                    .registry
                    .create_room(
                        creator.clone(),
                        crate::actor::CreateRoomRequest {
                            preset: Some("public_chat".to_owned()),
                            name: Some(name),
                            ..Default::default()
                        },
                        now_ms(),
                    )
                    .await
                    .map_err(to_source_error)?;
                if let Some(message) = &request.message {
                    created
                        .send_event(
                            creator,
                            "m.room.message".to_owned(),
                            None,
                            serde_json::json!({ "msgtype": "m.text", "body": message }),
                            None,
                            now_ms(),
                        )
                        .await
                        .map_err(to_source_error)?;
                }
                outcome.new_room_id =
                    Some(created.query(|actor| actor.room_id().to_string()).await);
                Some(created)
            }
            None => None,
        };
        step += 1;
        progress
            .report(step, Some(total), "creating the new room")
            .await;

        for (user, membership) in &members {
            let left = handle
                .membership(
                    user.clone(),
                    Action::Leave,
                    user.clone(),
                    serde_json::json!({ "reason": "This room has been deleted" }),
                    now_ms(),
                )
                .await;
            match left {
                Ok(_) => {
                    if membership == "join" {
                        outcome.kicked_users.push(user.to_string());
                        if let Some(new_room) = &new_room
                            && let Err(error) = self.force_join(new_room, user).await
                        {
                            tracing::warn!(%user, %error, "a member of a deleted room could not be moved to the new room");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(room = %room_id, %user, %error, "a member of a deleted room could not be made to leave");
                    outcome.failed_to_kick_users.push(user.to_string());
                }
            }
            step += 1;
            progress
                .report(step, Some(total), "removing the room's local members")
                .await;
        }

        let aliases = handle
            .query(|actor| actor.list_aliases())
            .await
            .map_err(to_source_error)?;
        for alias in &aliases {
            if let Ok(parsed) = RoomAliasId::parse(alias) {
                handle
                    .query(move |actor| actor.remove_alias(&parsed))
                    .await
                    .map_err(to_source_error)?;
            }
        }
        outcome.local_aliases = aliases;
        self.registry
            .set_directory_visibility(&parsed_room, false)
            .map_err(to_source_error)?;
        step += 1;
        progress
            .report(step, Some(total), "removing the room's aliases")
            .await;

        if request.purge {
            outcome.events_deleted = handle
                .administer(|actor| actor.delete_everything())
                .await
                .map_err(to_source_error)?;
            self.registry.forget_resident(&parsed_room).await;
        }
        step += 1;
        progress
            .report(step, Some(total), "removing the room's events")
            .await;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
