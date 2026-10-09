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
use std::time::Duration;

use async_trait::async_trait;
use hs_federation::client::FederationClient;
use hs_federation::keys::DynRemoteKeyCache;
use hs_federation::outbound_join::OutboundJoinError;
use hs_kv::KvBackend;
use hs_room::RoomError;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use ruma::{OwnedEventId, OwnedRoomId, RoomAliasId, RoomId, UserId};
use serde_json::Value;

/// The `hs serve` implementation of [`hs_room::remote_join::RemoteJoin`]. See the module docs.
pub struct FederationRemoteJoin<B: KvBackend> {
    client: Arc<FederationClient>,
    key_cache: Arc<DynRemoteKeyCache>,
    rooms: Arc<RoomRegistry<B>>,
    identity: HomeserverIdentity,
    barrier: Option<DeliveryBarrier>,
}

/// The longest a handshake waits for the forwarder to hand this server's latest events to the
/// sender: normally microseconds, since the forwarder reads a broadcast channel.
const FORWARDER_WAIT: Duration = Duration::from_secs(2);

/// The longest a handshake waits for the other server to accept what is queued for it. A
/// destination that is down keeps its queue; the handshake then goes ahead (and most likely
/// fails against the same server) rather than hang the client.
const DELIVERY_WAIT: Duration = Duration::from_secs(3);

/// What a membership handshake through another server waits for first, when the room is one
/// this server holds: that this server's own events of the room -- the leave the user just
/// made, say -- have been queued for that server and accepted by it. Without it a `make_join`
/// sent right after a leave overtakes the leave in flight, finds the user still joined there,
/// and an invite-only room is rejoined without an invite (seen against Synapse on 2026-10-09;
/// `crates/hs-cli/tests/federation_membership.rs`, the leave-then-rejoin test, one round in
/// three on two copies of this server). Two waits, both bounded: the forwarder's position on
/// the registry's global stream ([`crate::federation_sender::ForwardedPosition`]) has to reach
/// what was published before the request, and the sender's queue for the destination has to
/// drain (`FederationSender::wait_until_delivered`).
#[derive(Clone)]
pub struct DeliveryBarrier {
    forwarded: Arc<crate::federation_sender::ForwardedPosition>,
    sender: Arc<hs_federation::sender::FederationSender>,
}

impl DeliveryBarrier {
    /// A barrier over the forwarder `forwarded` reports for and the sender it feeds.
    #[must_use]
    pub fn new(
        forwarded: Arc<crate::federation_sender::ForwardedPosition>,
        sender: Arc<hs_federation::sender::FederationSender>,
    ) -> Self {
        Self { forwarded, sender }
    }

    /// Waits for `destination` to have this server's events of `room_id` published up to
    /// `published_seq` (see the type docs). Logs what it waited for; never fails.
    async fn settle(&self, destination: &str, room_id: &RoomId, published_seq: u64) {
        if !self.forwarded.wait_for(published_seq, FORWARDER_WAIT).await {
            tracing::warn!(
                %room_id,
                destination,
                published_seq,
                forwarded_seq = self.forwarded.processed(),
                "the outbound forwarder has not reached this server's latest events; the \
                 handshake goes ahead without them"
            );
            return;
        }
        match self
            .sender
            .wait_until_delivered(destination, DELIVERY_WAIT)
            .await
        {
            hs_federation::sender::DeliveryWait::NothingPending => {}
            hs_federation::sender::DeliveryWait::Delivered { waited } => {
                tracing::info!(
                    %room_id,
                    destination,
                    waited_ms = waited.as_millis() as u64,
                    "waited for the other server to accept this server's events before the \
                     membership handshake"
                );
            }
            hs_federation::sender::DeliveryWait::TimedOut { .. } => {
                // Logged by the sender.
            }
        }
    }
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
            barrier: None,
        }
    }

    /// Installs the delivery barrier every handshake for a room this server holds waits on
    /// first ([`DeliveryBarrier`]). Without one, a handshake goes out at once.
    #[must_use]
    pub fn with_delivery_barrier(mut self, barrier: DeliveryBarrier) -> Self {
        self.barrier = Some(barrier);
        self
    }

    /// The delivery barrier for `room_id` before a handshake with `destination`: only for a
    /// room this server holds with its state (one it was in, or is in), since only there can
    /// this server have made events the other side has not seen yet. A room held as a shell
    /// (an invite, a knock) or not at all has nothing to wait for.
    async fn settle_before_handshake(&self, destination: &str, room_id: &RoomId) {
        let Some(barrier) = &self.barrier else {
            return;
        };
        let Ok(handle) = self.rooms.get_or_load(room_id).await else {
            return;
        };
        let held_with_state = handle
            .query(|actor| matches!(actor.state_event("m.room.create", ""), Ok(Some(_))))
            .await;
        if !held_with_state {
            return;
        }
        barrier
            .settle(destination, room_id, self.rooms.global_published_seq())
            .await;
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
                    if matches!(
                        mapped,
                        RoomError::Forbidden(_) | RoomError::RemoteRefused(_)
                    ) {
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
        self.settle_before_handshake(destination, room_id).await;
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
                // The room itself saying no is the answer, whoever relays it, and so is the
                // other server's own client error, passed through; keep trying other servers
                // only for failures that are about the server, not the room.
                if matches!(
                    mapped,
                    RoomError::Forbidden(_) | RoomError::RemoteRefused(_)
                ) {
                    Err(JoinAttempt::Fatal(mapped))
                } else {
                    Err(JoinAttempt::Next { error: mapped })
                }
            }
        }
    }
}

/// What one sponsoring server's refusal means for the client: a `403` is the room refusing the
/// join and worth reporting as such; a `404` is that server not knowing the room; any other
/// `4xx` with a Matrix `errcode` is the other server's own answer, passed through to the client
/// as it came (`M_INCOMPATIBLE_ROOM_VERSION` with its `room_version`, say) and not asked of the
/// next server, as Synapse does -- except `M_UNABLE_TO_AUTHORISE_JOIN`, which says another
/// server might vouch for the join and so is a reason to ask the next one; anything else is a
/// failure to complete the handshake.
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
        OutboundJoinError::Rejected {
            status: status @ 400..=499,
            body,
            ..
        } if body
            .get("errcode")
            .and_then(Value::as_str)
            .is_some_and(|code| code != "M_UNABLE_TO_AUTHORISE_JOIN") =>
        {
            let mut extra = body.as_object().cloned().unwrap_or_default();
            let errcode = extra
                .remove("errcode")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let message = extra
                .remove("error")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| error.to_string());
            RoomError::RemoteRefused(Box::new(hs_room::error::RemoteRefusal {
                status: *status,
                errcode,
                error: message,
                extra,
            }))
        }
        OutboundJoinError::NotCanonicalJson { .. } => RoomError::BadRequest(error.to_string()),
        other => RoomError::RemoteJoinFailed(other.to_string()),
    }
}

/// The method, path and body of a directory request to another server: `GET` with the paging
/// in the query string, or `POST` carrying it with the search filter.
fn public_rooms_request(
    limit: Option<usize>,
    since: Option<&str>,
    search: Option<&str>,
) -> (&'static str, String, Option<Value>) {
    const PATH: &str = "/_matrix/federation/v1/publicRooms";
    match search {
        Some(term) => {
            let mut body = serde_json::json!({"filter": {"generic_search_term": term}});
            if let Some(limit) = limit {
                body["limit"] = serde_json::json!(limit);
            }
            if let Some(since) = since {
                body["since"] = serde_json::json!(since);
            }
            ("POST", PATH.to_owned(), Some(body))
        }
        None => {
            let mut params = Vec::new();
            if let Some(limit) = limit {
                params.push(format!("limit={limit}"));
            }
            if let Some(since) = since {
                params.push(format!("since={}", query_encode(since)));
            }
            let path = if params.is_empty() {
                PATH.to_owned()
            } else {
                format!("{PATH}?{}", params.join("&"))
            };
            ("GET", path, None)
        }
    }
}

/// Percent-encodes `value` for a query string: everything but the unreserved characters.
pub(crate) fn query_encode(value: &str) -> String {
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

/// A join made through another server carries `avatar_url` (and `displayname`) even when the
/// user has none, as `null`: what Synapse's remote join puts in the content
/// (`RoomMemberHandler.update_membership_locked` sets both from the profile for a remote join,
/// and only the ones that are set for a local one). So a later join made here once the room is
/// resident -- whose content leaves an unset avatar out -- is a new event, not the idempotent
/// no-op an identical content is, as on Synapse. Sytest's "Guest users are kicked from
/// guest_access rooms on revocation of guest_access over federation" joins a remote user twice
/// and waits for the second join in `/sync`; with no new event it waited forever whenever the
/// room's latest events reached the user's server before Sytest took its sync position (the
/// test's flakiness on `main`).
fn with_synapse_profile_keys(mut content: Value) -> Value {
    if let Some(object) = content.as_object_mut() {
        object.entry("displayname").or_insert(Value::Null);
        object.entry("avatar_url").or_insert(Value::Null);
    }
    content
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
        let content = with_synapse_profile_keys(content);
        // Until the join is held here, what the resident sends over `/send` for the room is
        // taken, not ignored as for a room nobody of this server is in.
        let _joining = self.rooms.remote_join_started(room_id);
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
                self.settle_before_handshake(&destination, room_id).await;
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
                self.settle_before_handshake(&destination, room_id).await;
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
        // MSC4311: the room's stripped state goes as whole events, which the invitee's server
        // can verify; the stripped form the caller built is kept for a room this server does
        // not hold, which cannot happen for an invite made here.
        let inviter = event.header().sender.to_string();
        let room_id = event
            .json()
            .get("room_id")
            .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
            .and_then(|r| RoomId::parse(r).ok());
        let handle = match room_id {
            Some(room_id) => self.rooms.get_or_load(&room_id).await,
            None => Err(RoomError::BadRequest("the invite names no room".to_owned())),
        };
        let invite_room_state = match handle {
            Ok(handle) => handle
                .query(move |actor| actor.stripped_state_pdus(&[inviter.as_str()]))
                .await
                .unwrap_or(invite_room_state),
            Err(_) => invite_room_state,
        };
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

    async fn exchange_third_party_invite(
        &self,
        destination: &str,
        room_id: &RoomId,
        event: Value,
    ) -> Result<(), RoomError> {
        let path = format!(
            "/_matrix/federation/v1/exchange_third_party_invite/{}",
            hs_federation::client::encode_path_segment(room_id.as_str())
        );
        let response = self
            .client
            .send(destination, "PUT", &path, Some(&event))
            .await
            .map_err(|e| {
                RoomError::RemoteJoinFailed(format!(
                    "could not hand the third-party invitation for {room_id} to {destination}: {e}"
                ))
            })?;
        match response.status {
            200..=299 => {
                tracing::info!(%room_id, destination, "handed a bound third-party invitation to the inviter's server");
                Ok(())
            }
            403 => Err(RoomError::Forbidden(format!(
                "{destination} refused the third-party invitation: {}",
                response.body
            ))),
            status => Err(RoomError::RemoteJoinFailed(format!(
                "{destination} answered the third-party invitation for {room_id} with HTTP {status}: {}",
                response.body
            ))),
        }
    }

    async fn public_rooms(
        &self,
        server: &str,
        limit: Option<usize>,
        since: Option<&str>,
        search: Option<&str>,
    ) -> Result<Value, RoomError> {
        let (method, path, body) = public_rooms_request(limit, since, search);
        let response = self
            .client
            .send(server, method, &path, body.as_ref())
            .await
            .map_err(|e| {
                RoomError::RemoteJoinFailed(format!(
                    "could not ask {server} for its room list: {e}"
                ))
            })?;
        if response.status / 100 != 2 {
            return Err(RoomError::RemoteJoinFailed(format!(
                "{server} answered the room list request with HTTP {}: {}",
                response.status, response.body
            )));
        }
        if !response.body.get("chunk").is_some_and(Value::is_array) {
            return Err(RoomError::RemoteJoinFailed(format!(
                "{server} answered the room list request with something other than a room list: {}",
                response.body
            )));
        }
        tracing::debug!(server, "fetched another server's public room list");
        Ok(response.body)
    }

    async fn timestamp_to_event(
        &self,
        server: &str,
        room_id: &RoomId,
        ts: i64,
        direction: hs_room::timeline::Direction,
    ) -> Result<Option<(OwnedEventId, i64)>, RoomError> {
        let dir = match direction {
            hs_room::timeline::Direction::Forward => "f",
            hs_room::timeline::Direction::Backward => "b",
        };
        let path = format!(
            "/_matrix/federation/v1/timestamp_to_event/{}?ts={ts}&dir={dir}",
            query_encode(room_id.as_str())
        );
        let response = self
            .client
            .send(server, "GET", &path, None)
            .await
            .map_err(|e| {
                RoomError::RemoteJoinFailed(format!(
                    "could not ask {server} for an event by time: {e}"
                ))
            })?;
        if response.status == 404 {
            return Ok(None);
        }
        if response.status / 100 != 2 {
            return Err(RoomError::RemoteJoinFailed(format!(
                "{server} answered timestamp_to_event with HTTP {}: {}",
                response.status, response.body
            )));
        }
        let event_id = response
            .body
            .get("event_id")
            .and_then(Value::as_str)
            .and_then(|id| ruma::EventId::parse(id).ok());
        let at = response
            .body
            .get("origin_server_ts")
            .and_then(Value::as_i64);
        match (event_id, at) {
            (Some(event_id), Some(at)) => Ok(Some((event_id, at))),
            _ => Err(RoomError::RemoteJoinFailed(format!(
                "{server} answered timestamp_to_event with something else: {}",
                response.body
            ))),
        }
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

    /// Another server's own client error reaches the client as it came: status, `errcode`,
    /// `error` and the rest of its body. Until 2026-10-01 it was `502 M_UNKNOWN` with the
    /// answer in the text (Sytest's "Outbound federation passes make_join failures through to
    /// the client" and "Outbound federation correctly handles unsupported room versions").
    /// `M_UNABLE_TO_AUTHORISE_JOIN` is still a reason to ask the next server.
    #[test]
    fn another_servers_client_error_is_passed_through_to_the_client() {
        let incompatible = OutboundJoinError::Rejected {
            destination: "x".into(),
            step: "make_join",
            status: 400,
            body: serde_json::json!({
                "errcode": "M_INCOMPATIBLE_ROOM_VERSION",
                "error": "y u no upgrade",
                "room_version": "sytest-room-ver",
            }),
        };
        let error = map_outbound_error(&incompatible).to_matrix_error();
        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.errcode.as_str(), "M_INCOMPATIBLE_ROOM_VERSION");
        assert_eq!(error.error, "y u no upgrade");
        assert_eq!(error.extra["room_version"], "sytest-room-ver");

        let custom = OutboundJoinError::Rejected {
            destination: "x".into(),
            step: "make_join",
            status: 400,
            body: serde_json::json!({"errcode": "M_TEST_ERROR_CODE", "error": "denied!"}),
        };
        let error = map_outbound_error(&custom).to_matrix_error();
        assert_eq!(error.errcode.as_str(), "M_TEST_ERROR_CODE");

        let unable = OutboundJoinError::Rejected {
            destination: "x".into(),
            step: "make_join",
            status: 400,
            body: serde_json::json!({"errcode": "M_UNABLE_TO_AUTHORISE_JOIN", "error": "no"}),
        };
        assert!(matches!(
            map_outbound_error(&unable),
            RoomError::RemoteJoinFailed(_)
        ));
    }

    #[test]
    fn a_directory_request_pages_in_the_query_string_and_a_search_makes_it_a_post() {
        assert_eq!(
            public_rooms_request(None, None, None),
            ("GET", "/_matrix/federation/v1/publicRooms".to_owned(), None)
        );
        assert_eq!(
            public_rooms_request(Some(5), Some("10"), None),
            (
                "GET",
                "/_matrix/federation/v1/publicRooms?limit=5&since=10".to_owned(),
                None
            )
        );
        assert_eq!(
            public_rooms_request(Some(5), None, Some("tea")),
            (
                "POST",
                "/_matrix/federation/v1/publicRooms".to_owned(),
                Some(serde_json::json!({"filter": {"generic_search_term": "tea"}, "limit": 5}))
            )
        );
    }
}
