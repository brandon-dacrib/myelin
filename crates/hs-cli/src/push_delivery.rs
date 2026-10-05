//! The room side of the push pipeline (`hs_push::pipeline`): describes events out of the room
//! registry, and forwards the registry's global stream into the pipeline. `hs-push` does not
//! depend on `hs-room`; this is where the two meet, the way `crate::appservice_delivery` joins
//! `hs-appservice` to the rooms. Also where `hs_config::email` becomes `hs_push::email`'s
//! settings, since `hs-push` does not depend on `hs-config` either.

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

/// `field` of the stripped state event `event_type`/`state_key` that `event` (a membership
/// event received from another server) carries in `unsigned.invite_room_state` or
/// `knock_room_state`, when it is a non-empty string.
fn stripped_state_str(
    event: &Event,
    event_type: &str,
    state_key: &str,
    field: &str,
) -> Option<String> {
    let unsigned = event
        .json()
        .get("unsigned")
        .and_then(CanonicalJsonValue::as_object)?;
    hs_room::routes::render::STRIPPED_STATE_KEYS
        .iter()
        .filter_map(|key| unsigned.get(*key).and_then(CanonicalJsonValue::as_array))
        .flatten()
        .filter_map(CanonicalJsonValue::as_object)
        .find(|entry| {
            entry.get("type").and_then(CanonicalJsonValue::as_str) == Some(event_type)
                && entry.get("state_key").and_then(CanonicalJsonValue::as_str) == Some(state_key)
        })
        .and_then(|entry| entry.get("content"))
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|content| content.get(field))
        .and_then(CanonicalJsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
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

    // An invite from another server comes with the room's stripped state, the only state
    // this server has of the room; the rendered event leaves it out, so it is read here.
    let stripped = |event_type: &str, state_key: &str, field: &str| {
        stripped_state_str(event, event_type, state_key, field)
    };
    let room_name = state_content_str(actor, "m.room.name", "", "name")
        .filter(|n| !n.is_empty())
        .or_else(|| stripped("m.room.name", "", "name"))
        .or_else(|| state_content_str(actor, "m.room.canonical_alias", "", "alias"))
        .filter(|n| !n.is_empty())
        .or_else(|| stripped("m.room.canonical_alias", "", "alias"));
    let sender_display_name =
        state_content_str(actor, "m.room.member", sender.as_str(), "displayname")
            .filter(|n| !n.is_empty())
            .or_else(|| stripped("m.room.member", sender.as_str(), "displayname"));

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

/// `hs_push::email`'s settings from the `email` configuration section.
#[must_use]
pub fn email_settings(config: &hs_config::EmailConfig) -> hs_push::email::Settings {
    let n = &config.notifications;
    let s = &n.subjects;
    hs_push::email::Settings {
        enabled: n.enabled,
        from: config.from.clone().unwrap_or_default(),
        app_name: config.app_name.clone(),
        client_base_url: config.client_base_url.clone(),
        delay_before_mail: n.delay_before_mail.into(),
        throttle_start: n.throttle_start.into(),
        throttle_max: n.throttle_max.into(),
        throttle_multiplier: n.throttle_multiplier,
        throttle_reset_after: n.throttle_reset_after.into(),
        subjects: hs_push::email::Subjects {
            message_from_person_in_room: s.message_from_person_in_room.clone(),
            message_from_person: s.message_from_person.clone(),
            messages_from_person: s.messages_from_person.clone(),
            messages_in_room: s.messages_in_room.clone(),
            messages_in_room_and_others: s.messages_in_room_and_others.clone(),
            messages_from_person_and_others: s.messages_from_person_and_others.clone(),
            invite_from_person: s.invite_from_person.clone(),
            invite_from_person_to_room: s.invite_from_person_to_room.clone(),
        },
    }
}

/// The SMTP server from the `email` configuration section: `None` until `smtp.host` and
/// `from` are set, when no email can be sent.
#[must_use]
pub fn smtp_settings(
    config: &hs_config::EmailConfig,
) -> Option<hs_push::email::smtp::SmtpSettings> {
    use hs_config::email::SmtpSecurity;
    use hs_push::email::smtp::Security;
    if !config.is_configured() {
        return None;
    }
    let host = config.smtp.host.clone()?;
    let credentials = match (&config.smtp.username, config.smtp.password.as_str()) {
        (Some(user), Some(pass)) => Some((user.clone(), pass.to_owned())),
        _ => None,
    };
    Some(hs_push::email::smtp::SmtpSettings {
        host,
        port: config.smtp.port,
        security: match config.smtp.security {
            SmtpSecurity::Starttls => Security::Starttls,
            SmtpSecurity::Tls => Security::Tls,
            SmtpSecurity::None => Security::None,
        },
        credentials,
        tls_name: config.smtp.tls_name.clone(),
        timeout: std::time::Duration::from_secs(30),
    })
}

/// Logs what the `email` section amounts to, at boot and after a change.
pub fn describe_email(config: &hs_config::EmailConfig) {
    match smtp_settings(config) {
        Some(smtp) if config.notifications.enabled => tracing::info!(
            host = %smtp.host,
            port = smtp.port,
            security = ?smtp.security,
            authenticated = smtp.credentials.is_some(),
            from = config.from.as_deref().unwrap_or_default(),
            "email: notification emails go through this SMTP server"
        ),
        Some(_) => tracing::info!(
            "email: an SMTP server is configured, but notification emails are off \
             (email.notifications.enabled); email pushers are stored, not delivered to"
        ),
        None => tracing::info!(
            "email: no SMTP server is configured (email.smtp.host); email pushers are stored, \
             not delivered to"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_email_section_maps_onto_the_pushers_settings() {
        let config: hs_config::EmailConfig = serde_yaml_ng::from_str(
            "smtp:\n  host: mail.example.org\n  port: 465\n  security: tls\n  username: u\n  password: p\n\
             from: Myelin <noreply@example.org>\napp_name: Myelin\nclient_base_url: https://app.example.org\n\
             notifications:\n  delay_before_mail: 5m\n  throttle_multiplier: 2\n",
        )
        .unwrap();
        let settings = email_settings(&config);
        assert_eq!(settings.from, "Myelin <noreply@example.org>");
        assert_eq!(settings.app_name, "Myelin");
        assert_eq!(
            settings.delay_before_mail,
            std::time::Duration::from_secs(300)
        );
        assert_eq!(settings.throttle_multiplier, 2);
        assert_eq!(
            settings.throttle_max,
            std::time::Duration::from_secs(86_400)
        );
        let smtp = smtp_settings(&config).unwrap();
        assert_eq!(smtp.port, 465);
        assert_eq!(smtp.security, hs_push::email::smtp::Security::Tls);
        assert_eq!(smtp.credentials, Some(("u".to_owned(), "p".to_owned())));
        assert!(smtp_settings(&hs_config::EmailConfig::default()).is_none());
    }
}
