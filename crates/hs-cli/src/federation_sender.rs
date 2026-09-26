//! Feeds `hs_federation::sender::FederationSender` with this server's own events.
//!
//! `hs-federation` owns the sender (the queues, the transactions, the retries); it deliberately
//! knows nothing about `hs-room`. This module is the adapter between the two, living in `hs-cli`
//! for the same reason [`crate::federation`]'s read adapters do: this is the one crate that
//! already depends on both.
//!
//! It follows the room registry's global update stream (`RoomRegistry::subscribe_global`, the
//! doorbell `hs-user`'s session hub and appservice delivery also listen to) and, for every update
//! whose `sender` is one of this server's users, loads the event exactly as it was stored and
//! signed and queues it for the servers that should receive it. Events whose sender is a remote
//! user are never re-sent: each server distributes its own events, and a resident server that
//! accepted a remote join forwards that one event itself (`hs_federation::join::send_join`).
//!
//! # Who receives an event
//!
//! The servers of every user joined to the room as of immediately after the event
//! (`RoomActor::joined_members_after`), plus -- for a membership event that removes someone --
//! the target's server: a kicked or banned user's server is not "joined after" the event, and if
//! it was that server's last member it would otherwise never learn why its user is gone. This
//! server's own name is always dropped. Invites are not sent this way: the spec's `/invite`
//! handshake is not built yet, and an invitee's server that is not in the room would only reject
//! the event as belonging to an unknown room.
//!
//! # What a lost update means
//!
//! The update stream is a `tokio::sync::broadcast` channel: a consumer that falls too far behind
//! is told how many updates it missed (`RecvError::Lagged`) and gets nothing else. The sender has
//! no catch-up (see `hs_federation::sender`'s module docs), so a missed update is a local event
//! that remote servers are never sent. It is logged at `warn`, with the count, saying exactly
//! that.
//!
//! # In a cluster
//!
//! Not shard-gated. A room's actor is resident on the replica that owns its shard and publishes
//! only there, so each local event is handed to exactly one replica's sender; but nothing here
//! consults `hs-cluster`, and a persisted, sharded sender will need to own that decision.

use std::collections::BTreeSet;
use std::sync::Arc;

use hs_federation::sender::FederationSender;
use hs_kv::KvBackend;
use hs_model::Event;
use hs_room::RoomError;
use hs_room::actor::RoomActor;
use hs_room::protocol::RoomUpdate;
use hs_room::registry::RoomRegistry;
use ruma::{OwnedServerName, ServerName};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

/// The running feeder: stopped by [`OutboundFederation::stop`], which `hs serve`'s shutdown
/// calls.
pub struct OutboundFederation {
    task: tokio::task::AbortHandle,
    sender: Arc<FederationSender>,
}

impl OutboundFederation {
    /// Subscribes to `rooms`' update stream and starts following it. Subscribe-then-return, so a
    /// caller that starts this before binding any listener is guaranteed to see every event a
    /// client sends afterwards; updates published before this call are not replayed.
    #[must_use]
    pub fn start<B: KvBackend + 'static>(
        rooms: Arc<RoomRegistry<B>>,
        sender: Arc<FederationSender>,
        own_server_name: OwnedServerName,
    ) -> Self {
        let updates = rooms.subscribe_global();
        let task =
            tokio::spawn(follow(rooms, sender.clone(), own_server_name, updates)).abort_handle();
        Self { task, sender }
    }

    /// The sender this feeds.
    #[must_use]
    pub fn sender(&self) -> &Arc<FederationSender> {
        &self.sender
    }

    /// Stops following rooms and shuts the sender down. Whatever is still queued is lost, and
    /// the sender logs how much.
    pub fn stop(&self) {
        self.task.abort();
        self.sender.shutdown();
    }
}

async fn follow<B: KvBackend + 'static>(
    rooms: Arc<RoomRegistry<B>>,
    sender: Arc<FederationSender>,
    own_server_name: OwnedServerName,
    mut updates: tokio::sync::broadcast::Receiver<RoomUpdate>,
) {
    loop {
        match updates.recv().await {
            Ok(update) => {
                if update.sender.server_name() != own_server_name {
                    continue;
                }
                if let Err(error) = forward_update(&rooms, &sender, &own_server_name, &update).await
                {
                    tracing::error!(
                        room_id = %update.room_id,
                        event_id = %update.event_id,
                        %error,
                        "could not hand a local event to the federation sender; remote servers \
                         will not receive it"
                    );
                }
            }
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(
                    missed,
                    "outbound federation fell behind the room update stream: the skipped \
                     updates' local events will NOT be sent to remote servers (the sender has no \
                     catch-up yet; see hs_federation::sender)"
                );
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// Loads the event `update` names and queues it for every remote server that should receive it.
/// Returns those servers (empty when the room has no remote members, in which case nothing was
/// queued).
///
/// # Errors
/// Returns [`RoomError`] if the room cannot be loaded, the event is not held by its actor, or
/// the membership at the event cannot be read.
pub async fn forward_update<B: KvBackend + 'static>(
    rooms: &RoomRegistry<B>,
    sender: &FederationSender,
    own_server_name: &ServerName,
    update: &RoomUpdate,
) -> Result<Vec<String>, RoomError> {
    let handle = rooms.get_or_load(&update.room_id).await?;
    let event_id = update.event_id.clone();
    let own = own_server_name.to_owned();
    let (pdu, destinations) = handle
        .query(move |actor| -> Result<(Value, Vec<String>), RoomError> {
            let event = actor
                .event_by_id(&event_id)
                .ok_or_else(|| RoomError::EventNotFound(event_id.to_string()))?;
            let destinations = remote_servers_for(actor, event, &own)?;
            let pdu = serde_json::from_slice(event.canonical_bytes())
                .map_err(|e| RoomError::Internal(format!("stored event is not JSON: {e}")))?;
            Ok((pdu, destinations))
        })
        .await?;
    if !destinations.is_empty() {
        tracing::debug!(
            room_id = %update.room_id,
            event_id = %update.event_id,
            servers = destinations.len(),
            "queueing a local event for federation"
        );
        sender.enqueue_pdu(destinations.clone(), pdu);
    }
    Ok(destinations)
}

/// The servers `event` should be sent to (see the module docs), sorted and without
/// `own_server_name`.
fn remote_servers_for<B: KvBackend>(
    actor: &RoomActor<B>,
    event: &Event,
    own_server_name: &ServerName,
) -> Result<Vec<String>, RoomError> {
    let mut servers: BTreeSet<String> = actor
        .joined_members_after(event)?
        .iter()
        .filter_map(|user_id| server_of(user_id))
        .map(str::to_owned)
        .collect();
    if event.header().event_type == "m.room.member"
        && matches!(membership_of(event), Some("leave" | "ban"))
        && let Some(server) = event.header().state_key.as_deref().and_then(server_of)
    {
        servers.insert(server.to_owned());
    }
    servers.remove(own_server_name.as_str());
    Ok(servers.into_iter().collect())
}

/// The server-name half of a Matrix user ID (`@user:server` -> `server`).
fn server_of(user_id: &str) -> Option<&str> {
    user_id.split_once(':').map(|(_, server)| server)
}

fn membership_of(event: &Event) -> Option<&str> {
    event
        .json()
        .get("content")?
        .as_object()?
        .get("membership")?
        .as_str()
}
