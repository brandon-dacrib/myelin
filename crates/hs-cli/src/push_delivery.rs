//! The room side of the push pipeline (`hs_push::pipeline`): describes events out of the room
//! registry, and forwards the registry's global stream into the pipeline. `hs-push` does not
//! depend on `hs-room`; this is where the two meet, the way `crate::appservice_delivery` joins
//! `hs-appservice` to the rooms.

use std::sync::Arc;

use hs_kv::KvBackend;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::event::Event;
use hs_push::context::{PushEvaluationInput, PushEvaluationMember};
use hs_push::pipeline::{DescribedEvent, EventSource, PipelineHandle};
use hs_room::RoomError;
use hs_room::actor::RoomActor;
use hs_room::registry::RoomRegistry;
use ruma::{EventId, OwnedServerName, RoomId, UserId};

/// An [`EventSource`] over the room registry: only rooms this replica owns are described, since
/// each room's owner is where its events are evaluated (one push per event per user, cluster or
/// not).
pub struct RegistrySource<B: KvBackend> {
    rooms: Arc<RoomRegistry<B>>,
    server_name: OwnedServerName,
}

impl<B: KvBackend> RegistrySource<B> {
    /// A source over `rooms`.
    #[must_use]
    pub fn new(rooms: Arc<RoomRegistry<B>>) -> Self {
        let server_name = rooms.server_name().to_owned();
        Self { rooms, server_name }
    }
}

fn content_str(event: &Event, field: &str) -> Option<String> {
    event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|c| c.get(field))
        .and_then(CanonicalJsonValue::as_str)
        .map(str::to_owned)
}

fn state_content_str<B: KvBackend>(
    actor: &RoomActor<B>,
    event_type: &str,
    state_key: &str,
    field: &str,
) -> Option<String> {
    actor
        .state_event(event_type, state_key)
        .ok()
        .flatten()
        .and_then(|e| content_str(e, field))
}

/// Everything the pipeline needs about `event_id`, read off the actor.
fn describe<B: KvBackend>(
    actor: &RoomActor<B>,
    event_id: &EventId,
    server_name: &ruma::ServerName,
) -> Result<Option<DescribedEvent>, RoomError> {
    let Some(event) = actor.event_by_id(event_id) else {
        return Ok(None);
    };
    let json = hs_room::routes::render::client_event_json(event);
    let sender = event.header().sender.clone();

    let mut members = Vec::new();
    let mut joined_member_count = 0u64;
    for member_event in actor.members()? {
        let Some(state_key) = member_event.header().state_key.as_deref() else {
            continue;
        };
        let Ok(user_id) = UserId::parse(state_key) else {
            continue;
        };
        let Some(membership) = content_str(member_event, "membership") else {
            continue;
        };
        if membership == "join" {
            joined_member_count += 1;
        }
        if membership != "join" && membership != "invite" {
            continue;
        }
        let display_name =
            content_str(member_event, "displayname").unwrap_or_else(|| user_id.to_string());
        members.push(PushEvaluationMember {
            is_local: user_id.server_name() == server_name,
            user_id,
            membership,
            display_name,
        });
    }

    let rules = hs_model::room_version::rules_for(actor.room_version())
        .ok_or_else(|| RoomError::UnsupportedRoomVersion(actor.room_version().to_string()))?;
    let power_levels = match actor
        .state_event("m.room.power_levels", "")?
        .and_then(|e| e.json().get("content"))
        .and_then(CanonicalJsonValue::as_object)
    {
        Some(content) => Some(
            hs_model::power_levels::PowerLevels::parse(content, &rules)
                .map_err(|e| RoomError::Internal(e.to_string()))?,
        ),
        None => None,
    };

    let room_name = state_content_str(actor, "m.room.name", "", "name")
        .filter(|n| !n.is_empty())
        .or_else(|| state_content_str(actor, "m.room.canonical_alias", "", "alias"))
        .filter(|n| !n.is_empty());
    let sender_display_name =
        state_content_str(actor, "m.room.member", sender.as_str(), "displayname")
            .filter(|n| !n.is_empty());

    Ok(Some(DescribedEvent {
        event: json,
        input: PushEvaluationInput {
            joined_member_count,
            members,
            power_levels,
            room_version: actor.room_version().clone(),
        },
        room_name,
        sender_display_name,
    }))
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> EventSource for RegistrySource<B> {
    async fn describe(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<DescribedEvent>, String> {
        if !self.rooms.owns_room(room_id) {
            return Ok(None);
        }
        let event_id = event_id.to_owned();
        let server_name = self.server_name.clone();
        match self
            .rooms
            .read_room(room_id, move |actor| {
                describe(actor, &event_id, &server_name)
            })
            .await
        {
            Ok(Ok(described)) => Ok(described),
            Err(RoomError::RoomNotFound(_)) => Ok(None),
            Ok(Err(e)) | Err(e) => Err(e.to_string()),
        }
    }
}

/// Forwards every update on the registry's global stream to the pipeline, until the stream
/// closes. Subscribe before any listener is bound: the stream does not replay.
pub fn forward_room_updates<B: KvBackend + 'static>(
    rooms: &RoomRegistry<B>,
    pipeline: PipelineHandle,
) -> tokio::task::JoinHandle<()> {
    let mut updates = rooms.subscribe_global();
    tokio::spawn(async move {
        loop {
            match updates.recv().await {
                Ok(update) => {
                    pipeline.event_persisted(update.room_id, update.event_id, update.room_pos);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    // The events are still in their rooms; what is lost is their announcement.
                    // Their recipients' counts stay behind until the next event in each room,
                    // which is evaluated on its own. Recorded, not hidden.
                    tracing::warn!(
                        missed,
                        "the push pipeline fell behind the room stream; that many events were \
                         not evaluated for push"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}
