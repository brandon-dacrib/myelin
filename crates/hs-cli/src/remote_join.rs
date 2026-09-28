//! `hs_room::remote_join::RemoteJoin` over the federation client: how `POST /join` reaches a
//! room hosted elsewhere.
//!
//! `hs-room` serves the join endpoints and `hs-federation` speaks to other servers; neither
//! depends on the other, and this is where they meet. A join of a room the registry does not hold
//! becomes `hs_federation::outbound_join::join_room_with_content` (the real
//! `make_join`/`send_join` handshake, every returned event verified) followed by
//! `hs_room::registry::RoomRegistry::bootstrap_from_remote_join`, which makes the room resident
//! from that verified snapshot (RFC 0015). Until both existed, the handshake was a diagnostic
//! command (`hs federation-join-room`) whose result nothing could keep.

use std::sync::Arc;

use async_trait::async_trait;
use hs_federation::client::FederationClient;
use hs_federation::keys::DynRemoteKeyCache;
use hs_federation::outbound_join::OutboundJoinError;
use hs_kv::KvBackend;
use hs_room::RoomError;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use ruma::{OwnedRoomId, RoomAliasId, RoomId, UserId};
use serde_json::Value;

/// The `hs serve` implementation of [`hs_room::remote_join::RemoteJoin`]. See the module docs.
pub struct FederationRemoteJoin<B: KvBackend> {
    client: Arc<FederationClient>,
    key_cache: Arc<DynRemoteKeyCache>,
    rooms: Arc<RoomRegistry<B>>,
    identity: HomeserverIdentity,
}

impl<B: KvBackend + 'static> FederationRemoteJoin<B> {
    /// Over the federation mount's own client and key cache, so discovery, TLS trust, request
    /// signing and per-destination backoff are the ones every other outbound call uses.
    #[must_use]
    pub fn new(
        client: Arc<FederationClient>,
        key_cache: Arc<DynRemoteKeyCache>,
        rooms: Arc<RoomRegistry<B>>,
        identity: HomeserverIdentity,
    ) -> Self {
        Self {
            client,
            key_cache,
            rooms,
            identity,
        }
    }
}

impl<B: KvBackend + 'static> FederationRemoteJoin<B> {
    fn check_local(&self, user_id: &UserId) -> Result<(), RoomError> {
        if user_id.server_name() == &*self.identity.server_name {
            Ok(())
        } else {
            Err(RoomError::Forbidden(format!(
                "{user_id} is not a user of this server"
            )))
        }
    }

    /// Runs `attempt` against each server in `via` but this one, in order, until one succeeds:
    /// the room refusing (a `403`) is the answer whoever relays it, anything else is a reason to
    /// ask the next server. `what` names the handshake in the log.
    async fn through_each<T, F, Fut>(
        &self,
        via: &[String],
        what: &str,
        attempt: F,
    ) -> Result<T, RoomError>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<T, OutboundJoinError>>,
    {
        let own_name = self.identity.server_name.as_str();
        let mut last_error: Option<RoomError> = None;
        for destination in via.iter().filter(|d| d.as_str() != own_name) {
            match attempt(destination.clone()).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    tracing::warn!(destination, what, %error, "a server could not complete the handshake");
                    let mapped = match map_outbound_error(&error) {
                        RoomError::RemoteJoinFailed(detail) => {
                            RoomError::RemoteJoinFailed(format!("{what}: {detail}"))
                        }
                        other => other,
                    };
                    if matches!(mapped, RoomError::Forbidden(_)) {
                        return Err(mapped);
                    }
                    last_error = Some(mapped);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            RoomError::RemoteJoinFailed(format!(
                "{what}: no server to ask, the only candidate was this one"
            ))
        }))
    }
}

/// How one server's part in a join went, when it did not end in the join: a failure that is
/// the answer (the room refusing), or one that is a reason to ask the next server.
enum JoinAttempt {
    /// The room refused the join; asking another server would not change that.
    Fatal(RoomError),
    /// This server could not complete the handshake (including `M_UNABLE_TO_AUTHORISE_JOIN`:
    /// it cannot vouch for a restricted join, and the next server named might).
    Next { error: RoomError },
}

impl<B: KvBackend + 'static> FederationRemoteJoin<B> {
    /// One `make_join`/`send_join` handshake through `destination`, and the room made (or kept)
    /// resident from its answer.
    async fn join_through(
        &self,
        destination: &str,
        user_id: &UserId,
        room_id: &RoomId,
        content: &Value,
    ) -> Result<OwnedRoomId, JoinAttempt> {
        match hs_federation::outbound_join::join_room_with_content(
            &self.client,
            &self.key_cache,
            destination,
            room_id.as_str(),
            user_id.as_str(),
            &self.identity.server_name,
            &self.identity.signing_key,
            Some(content),
        )
        .await
        {
            Ok(outcome) => {
                // A room this server is in already (a restricted join no user here could
                // authorise): the join is one more event of a room held for real, placed
                // after what it cites like any event over `/send`. Only if it cites what
                // this copy has not seen yet is the resident's state taken over it.
                if let Ok(handle) = self.rooms.get_or_load(room_id).await
                    && handle.query(|actor| actor.local_user_joined()).await
                {
                    match handle.accept_remote_event(outcome.join_event.clone()).await {
                        Ok(_) => {
                            tracing::info!(%room_id, %user_id, destination, "joined a room this server is in through another server");
                            return Ok(room_id.to_owned());
                        }
                        Err(RoomError::MissingAncestors(_)) => {}
                        Err(error) => return Err(JoinAttempt::Fatal(error)),
                    }
                }
                tracing::info!(
                    %room_id,
                    %user_id,
                    destination,
                    state_events = outcome.state.len(),
                    "joined a room hosted elsewhere; making it resident"
                );
                self.rooms
                    .bootstrap_from_remote_join(
                        room_id,
                        outcome.room_version,
                        outcome.state,
                        outcome.auth_chain,
                        outcome.join_event,
                    )
                    .await
                    .map_err(JoinAttempt::Fatal)?;
                Ok(room_id.to_owned())
            }
            Err(error) => {
                tracing::warn!(%room_id, %user_id, destination, %error, "a server could not sponsor the join");
                let mapped = match map_outbound_error(&error) {
                    RoomError::RemoteJoinFailed(detail) => {
                        RoomError::RemoteJoinFailed(format!("join: {detail}"))
                    }
                    other => other,
                };
                // The room itself saying no is the answer, whoever relays it; keep trying
                // other servers only for failures that are about the server, not the room.
                if matches!(mapped, RoomError::Forbidden(_)) {
                    Err(JoinAttempt::Fatal(mapped))
                } else {
                    Err(JoinAttempt::Next { error: mapped })
                }
            }
        }
    }
}

/// What one sponsoring server's refusal means for the client: a `403` is the room refusing the
/// join and worth reporting as such; a `404` is that server not knowing the room; anything else
/// is a failure to complete the handshake.
fn map_outbound_error(error: &OutboundJoinError) -> RoomError {
    match error {
        OutboundJoinError::Rejected {
            status: 403, body, ..
        } => RoomError::Forbidden(
            body.get("error")
                .and_then(Value::as_str)
                .unwrap_or("the other server refused the request")
                .to_owned(),
        ),
        OutboundJoinError::Rejected { status: 404, .. } => {
            RoomError::RoomNotFound(error.to_string())
        }
        other => RoomError::RemoteJoinFailed(other.to_string()),
    }
}

/// Percent-encodes `value` for a query string: everything but the unreserved characters.
fn query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[async_trait]
impl<B: KvBackend + 'static> hs_room::remote_join::RemoteJoin for FederationRemoteJoin<B> {
    async fn join(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        via: &[String],
        content: Value,
    ) -> Result<OwnedRoomId, RoomError> {
        if user_id.server_name() != &*self.identity.server_name {
            return Err(RoomError::Forbidden(format!(
                "{user_id} is not a user of this server"
            )));
        }
        let own_name = self.identity.server_name.as_str();
        let mut last_error: Option<RoomError> = None;
        for destination in via.iter().filter(|d| d.as_str() != own_name) {
            match self
                .join_through(destination, user_id, room_id, &content)
                .await
            {
                Ok(joined) => return Ok(joined),
                Err(JoinAttempt::Fatal(error)) => return Err(error),
                Err(JoinAttempt::Next { error }) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            RoomError::RemoteJoinFailed(
                "join: no server to ask, the only candidate was this one".into(),
            )
        }))
    }

    async fn leave(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        via: &[String],
        content: Value,
    ) -> Result<(), RoomError> {
        self.check_local(user_id)?;
        let content = &content;
        let outcome = self
            .through_each(via, "leave", |destination| async move {
                hs_federation::outbound_membership::leave_room(
                    &self.client,
                    &destination,
                    room_id.as_str(),
                    user_id.as_str(),
                    &self.identity.server_name,
                    &self.identity.signing_key,
                    Some(content),
                )
                .await
            })
            .await?;
        tracing::info!(%room_id, %user_id, "left a room this server is not in, through a server that is");
        self.rooms
            .accept_out_of_room_membership(room_id, outcome.room_version, outcome.event)
            .await?;
        Ok(())
    }

    async fn knock(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        via: &[String],
        content: Value,
    ) -> Result<OwnedRoomId, RoomError> {
        self.check_local(user_id)?;
        let content = &content;
        let outcome = self
            .through_each(via, "knock", |destination| async move {
                hs_federation::outbound_membership::knock_room(
                    &self.client,
                    &destination,
                    room_id.as_str(),
                    user_id.as_str(),
                    &self.identity.server_name,
                    &self.identity.signing_key,
                    Some(content),
                )
                .await
            })
            .await?;
        tracing::info!(%room_id, %user_id, "knocked on a room hosted elsewhere");
        // The stripped state the resident answered with is what the user's client will show of
        // the room; it is kept on the knock itself, where `hs-user`'s `/sync` reads it.
        let mut json = hs_federation::inbound::event_json(&outcome.event);
        json["unsigned"]["knock_room_state"] = Value::Array(outcome.room_state);
        let event = hs_model::Event::parse(&json, outcome.room_version.clone())
            .map_err(|e| RoomError::Internal(format!("the accepted knock does not parse: {e}")))?;
        self.rooms
            .accept_out_of_room_membership(room_id, outcome.room_version, event)
            .await?;
        Ok(room_id.to_owned())
    }

    async fn invite(
        &self,
        room_version: &ruma::RoomVersionId,
        event: &hs_model::Event,
        invite_room_state: Vec<Value>,
    ) -> Result<hs_model::Event, RoomError> {
        let destination = event
            .header()
            .state_key
            .as_deref()
            .and_then(|key| UserId::parse(key).ok())
            .map(|user| user.server_name().to_string())
            .ok_or_else(|| RoomError::BadRequest("the invite is not about a user".to_owned()))?;
        hs_federation::outbound_membership::send_invite(
            &self.client,
            &self.key_cache,
            &destination,
            room_version,
            event,
            &invite_room_state,
        )
        .await
        .map_err(|error| {
            tracing::warn!(event_id = %event.event_id(), destination, %error, "the invitee's server did not take the invite");
            match map_outbound_error(&error) {
                // The invitee's server not knowing the room is the normal case, not a refusal.
                RoomError::RoomNotFound(_) | RoomError::RemoteJoinFailed(_) => {
                    RoomError::RemoteJoinFailed(format!("invite: {error}"))
                }
                other => other,
            }
        })
    }

    async fn resolve_alias(
        &self,
        alias: &RoomAliasId,
    ) -> Result<(OwnedRoomId, Vec<String>), RoomError> {
        let destination = alias.server_name().as_str();
        let path = format!(
            "/_matrix/federation/v1/query/directory?room_alias={}",
            query_encode(alias.as_str())
        );
        let response = self
            .client
            .send(destination, "GET", &path, None)
            .await
            .map_err(|e| {
                RoomError::RemoteJoinFailed(format!(
                    "could not ask {destination} about {alias}: {e}"
                ))
            })?;
        if response.status == 404 {
            return Err(RoomError::RoomNotFound(alias.to_string()));
        }
        if response.status / 100 != 2 {
            return Err(RoomError::RemoteJoinFailed(format!(
                "{destination} answered the directory query for {alias} with HTTP {}: {}",
                response.status, response.body
            )));
        }
        let room_id = response
            .body
            .get("room_id")
            .and_then(Value::as_str)
            .and_then(|s| RoomId::parse(s).ok())
            .ok_or_else(|| {
                RoomError::RemoteJoinFailed(format!(
                    "{destination}'s directory answer for {alias} carried no usable room_id"
                ))
            })?;
        let servers = response
            .body
            .get("servers")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok((room_id.to_owned(), servers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encoding_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(query_encode("#room:example.org"), "%23room%3Aexample.org");
        assert_eq!(query_encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn a_403_is_the_room_refusing_and_a_404_is_the_server_not_knowing() {
        let refused = OutboundJoinError::Rejected {
            destination: "x".into(),
            step: "make_join",
            status: 403,
            body: serde_json::json!({"errcode": "M_FORBIDDEN", "error": "invite only"}),
        };
        assert!(
            matches!(map_outbound_error(&refused), RoomError::Forbidden(m) if m == "invite only")
        );
        let unknown = OutboundJoinError::Rejected {
            destination: "x".into(),
            step: "make_join",
            status: 404,
            body: serde_json::json!({}),
        };
        assert!(matches!(
            map_outbound_error(&unknown),
            RoomError::RoomNotFound(_)
        ));
        let broken = OutboundJoinError::MalformedTemplate("x".into(), "no event".into());
        assert!(matches!(
            map_outbound_error(&broken),
            RoomError::RemoteJoinFailed(_)
        ));
    }
}
