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
//! Nor is a local user's membership another server made the handshake for
//! (`RoomActor::is_proactively_sent`): the join this server's `send_join` made, a leave or knock
//! a resident took. The server that has it sends it on, as Synapse's `proactively_send = False`.
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
//! is told how many updates it missed (`RecvError::Lagged`) and gets nothing else. A missed
//! update is a local event the sender was never handed, so it moved no room's position and the
//! sender's catch-up does not know of it either (see `hs_federation::sender`'s module docs): a
//! remote server receives it only as an ancestor of a later event, which it fetches itself. It
//! is logged at `warn`, with the count, saying exactly that.
//!
//! # Catch-up
//!
//! A destination that was down for longer than its queue holds is caught up from the rooms
//! (`hs_federation::sender`'s "Catch-up"): [`RoomCatchUp`] is the [`CatchUpSource`] the sender
//! asks for each room's latest event, installed by [`OutboundFederation::start`].
//!
//! # In a cluster
//!
//! A room's actor is resident on the replica that owns its shard and publishes only there, so
//! each local event is handed to exactly one replica's sender, which writes it to the shared
//! store for every destination. Which replica *sends* for a destination is a separate question,
//! answered by [`ShardGate`]: the one that owns `ShardLayout::federation_shard(destination)`
//! (RFC 0001's federation shards). [`follow_ownership`] keeps the sender in line with that as
//! shards come and go: acquiring a federation shard resumes the queues now sent for here,
//! releasing or losing one stops the workers that no longer are, leaving their queues in the
//! store for the new owner. In single-node mode every shard is always mine and none of this is
//! visible.

use std::collections::BTreeSet;
use std::sync::Arc;

use hs_cluster::ownership::{Ownership, OwnershipEvent};
use hs_cluster::types::{ShardKind, ShardLayout};
use hs_federation::sender::{CatchUpSource, FederationSender, SendGate};
use hs_kv::KvBackend;
use hs_model::Event;
use hs_room::RoomError;
use hs_room::actor::RoomActor;
use hs_room::protocol::RoomUpdate;
use hs_room::registry::RoomRegistry;
use hs_room::timeline::Direction;
use ruma::{OwnedServerName, RoomId, ServerName};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

/// The sender's gate in `hs serve`: a destination is sent for by the replica that owns its
/// federation shard. See the module docs.
pub struct ShardGate {
    ownership: Arc<dyn Ownership>,
    layout: ShardLayout,
}

impl ShardGate {
    /// A gate over `ownership`, computing shards with `layout`.
    #[must_use]
    pub fn new(ownership: Arc<dyn Ownership>, layout: ShardLayout) -> Self {
        Self { ownership, layout }
    }
}

impl SendGate for ShardGate {
    fn sends_here(&self, destination: &str) -> bool {
        self.ownership
            .is_mine(self.layout.federation_shard(destination))
    }
}

/// The running feeder: stopped by [`OutboundFederation::stop`], which `hs serve`'s shutdown
/// calls.
pub struct OutboundFederation {
    task: tokio::task::AbortHandle,
    ownership_task: tokio::task::AbortHandle,
    sender: Arc<FederationSender>,
}

/// How far the forwarder has read the registry's global update stream: the `global_seq`
/// (`RoomUpdate::global_seq`) of the newest update it has handed to the sender. A reader that
/// wants its own event to be *queued* for its destination before it goes on -- the delivery
/// barrier before a membership handshake, [`crate::remote_join::DeliveryBarrier`] -- takes
/// `RoomRegistry::global_published_seq` and waits for the forwarder to reach it. The same
/// arrangement `hs-user`'s `/sync` uses to read its own writes off the session hub.
#[derive(Default)]
pub struct ForwardedPosition {
    seq: std::sync::atomic::AtomicU64,
    notify: tokio::sync::Notify,
}

impl ForwardedPosition {
    /// The newest `global_seq` handed to the sender, `0` before any.
    #[must_use]
    pub fn processed(&self) -> u64 {
        self.seq.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Records that every update up to `seq` has been handled, and wakes the waiters.
    pub fn advance(&self, seq: u64) {
        self.seq.fetch_max(seq, std::sync::atomic::Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    /// Waits until the forwarder has handled every update up to `seq`, for at most `timeout`.
    /// `true` when it has; `false` when the time ran out first (the forwarder is behind, or
    /// not running).
    pub async fn wait_for(&self, seq: u64, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Arm the notification before the check, so an advance between the two is not
            // missed (`Notify::notify_waiters` wakes only waiters already registered).
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.processed() >= seq {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.processed() >= seq;
            }
        }
    }
}

impl OutboundFederation {
    /// Subscribes to `rooms`' update stream and starts following it; gates the sender on
    /// `ownership` (see the module docs) and follows that too; and resumes whatever the
    /// sender's store still holds from a previous run (`FederationSender::resume`: every
    /// destination with a queue that this replica sends for gets its worker back, oldest PDU
    /// first). Subscribe-then-return, so a caller that starts this before binding any listener
    /// is guaranteed to see every event a client sends afterwards; updates published before
    /// this call are not replayed.
    #[must_use]
    pub fn start<B: KvBackend + 'static>(
        rooms: Arc<RoomRegistry<B>>,
        sender: Arc<FederationSender>,
        own_server_name: OwnedServerName,
        ownership: Arc<dyn Ownership>,
        layout: ShardLayout,
    ) -> Self {
        Self::start_with_position(
            rooms,
            sender,
            own_server_name,
            ownership,
            layout,
            Arc::new(ForwardedPosition::default()),
        )
    }

    /// [`OutboundFederation::start`], moving `position` on as each update of the stream is
    /// handled: what [`crate::remote_join::DeliveryBarrier`] waits on before a membership
    /// handshake, so the handshake follows this server's own events to the other server instead
    /// of overtaking them.
    #[must_use]
    pub fn start_with_position<B: KvBackend + 'static>(
        rooms: Arc<RoomRegistry<B>>,
        sender: Arc<FederationSender>,
        own_server_name: OwnedServerName,
        ownership: Arc<dyn Ownership>,
        layout: ShardLayout,
        position: Arc<ForwardedPosition>,
    ) -> Self {
        let updates = rooms.subscribe_global();
        let ownership_events = ownership.subscribe();
        sender.set_gate(Arc::new(ShardGate::new(ownership, layout)));
        sender.install_catch_up_source(Arc::new(RoomCatchUp::new(
            rooms.clone(),
            own_server_name.clone(),
        )));
        resume_logged(&sender);
        let task = tokio::spawn(follow(
            rooms,
            sender.clone(),
            own_server_name,
            updates,
            position,
        ))
        .abort_handle();
        let ownership_task =
            tokio::spawn(follow_ownership(sender.clone(), ownership_events)).abort_handle();
        Self {
            task,
            ownership_task,
            sender,
        }
    }

    /// The sender this feeds.
    #[must_use]
    pub fn sender(&self) -> &Arc<FederationSender> {
        &self.sender
    }

    /// Stops following rooms and ownership and shuts the sender down. Whatever is still queued
    /// stays in the sender's store for the next start, and the sender logs how much.
    pub fn stop(&self) {
        self.task.abort();
        self.ownership_task.abort();
        self.sender.shutdown();
    }
}

fn resume_logged(sender: &FederationSender) {
    match sender.resume() {
        Ok(0) => {}
        Ok(pdus) => tracing::info!(
            pdus,
            "resumed outbound federation queues left by a previous run"
        ),
        Err(error) => tracing::error!(
            %error,
            "could not read the outbound federation queues left by a previous run; what they \
             hold will go out only once something new is queued for the same server"
        ),
    }
}

/// Keeps `sender`'s workers in line with shard ownership (see the module docs): a federation
/// shard acquired resumes the queues now sent for here, one released or lost stops the workers
/// that no longer are. A lagged event stream is answered by doing both, since the answer to
/// "what changed" is then whatever the gate says now. Returns when the stream closes.
pub async fn follow_ownership(
    sender: Arc<FederationSender>,
    mut events: tokio::sync::broadcast::Receiver<OwnershipEvent>,
) {
    loop {
        match events.recv().await {
            Ok(OwnershipEvent::Acquired(fence)) if fence.shard.kind == ShardKind::Federation => {
                tracing::info!(shard = ?fence.shard, "federation shard acquired");
                resume_logged(&sender);
            }
            Ok(OwnershipEvent::Released(shard) | OwnershipEvent::Lost { shard, .. })
                if shard.kind == ShardKind::Federation =>
            {
                tracing::info!(?shard, "federation shard given up");
                sender.stop_workers_not_sent_here();
            }
            Ok(_) => {}
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "ownership events missed; re-reading what is mine");
                sender.stop_workers_not_sent_here();
                resume_logged(&sender);
            }
            Err(RecvError::Closed) => return,
        }
    }
}

async fn follow<B: KvBackend + 'static>(
    rooms: Arc<RoomRegistry<B>>,
    sender: Arc<FederationSender>,
    own_server_name: OwnedServerName,
    mut updates: tokio::sync::broadcast::Receiver<RoomUpdate>,
    position: Arc<ForwardedPosition>,
) {
    loop {
        match updates.recv().await {
            Ok(update) => {
                if update.sender.server_name() == own_server_name
                    && let Err(error) =
                        forward_update(&rooms, &sender, &own_server_name, &update).await
                {
                    tracing::error!(
                        room_id = %update.room_id,
                        event_id = %update.event_id,
                        %error,
                        "could not hand a local event to the federation sender; remote servers \
                         will not receive it"
                    );
                }
                // Handed to the sender (queued, or nothing to queue, or failed and logged):
                // either way this update is behind the forwarder now.
                position.advance(update.global_seq);
            }
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(
                    missed,
                    "outbound federation fell behind the room update stream: the skipped \
                     updates' local events will NOT be sent to remote servers (they reach one \
                     only as ancestors of a later event; see hs_federation::sender)"
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
            // A join `send_join` made, or a leave or knock a resident took: the server that has
            // it sends it on (`RoomActor::is_proactively_sent`).
            if !actor.is_proactively_sent(&event_id) {
                return Ok((Value::Null, Vec::new()));
            }
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

/// How far back [`RoomCatchUp`] looks for this server's latest event in a room whose forward
/// extremities are all remote.
const CATCH_UP_SCAN: usize = 200;

/// The sender's [`CatchUpSource`] over the room registry: for a room a destination is behind in,
/// the latest event this server's own users sent there -- the newest local forward extremity if
/// there is one (what Synapse prefers too: nothing in the room cites it yet, so the receiver can
/// fetch everything before it from its `prev_events`), otherwise the newest local event among
/// the last [`CATCH_UP_SCAN`] -- and nothing if the destination no longer has a user joined.
pub struct RoomCatchUp<B: KvBackend> {
    rooms: Arc<RoomRegistry<B>>,
    own_server_name: OwnedServerName,
}

impl<B: KvBackend> RoomCatchUp<B> {
    /// A source over `rooms`, choosing events sent by `own_server_name`'s users.
    #[must_use]
    pub fn new(rooms: Arc<RoomRegistry<B>>, own_server_name: OwnedServerName) -> Self {
        Self {
            rooms,
            own_server_name,
        }
    }
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> CatchUpSource for RoomCatchUp<B> {
    async fn latest_pdu(&self, room_id: &str, destination: &str) -> Result<Option<Value>, String> {
        let room_id = RoomId::parse(room_id).map_err(|e| format!("not a room ID: {e}"))?;
        let handle = self
            .rooms
            .get_or_load(&room_id)
            .await
            .map_err(|e| e.to_string())?;
        let own = self.own_server_name.clone();
        let destination = destination.to_owned();
        handle
            .query(move |actor| latest_local_pdu(actor, &own, &destination))
            .await
            .map_err(|e| e.to_string())
    }
}

/// See [`RoomCatchUp`].
fn latest_local_pdu<B: KvBackend>(
    actor: &RoomActor<B>,
    own_server_name: &ServerName,
    destination: &str,
) -> Result<Option<Value>, RoomError> {
    let still_joined = actor.joined_members()?.iter().any(|member| {
        member.header().state_key.as_deref().and_then(server_of) == Some(destination)
    });
    if !still_joined {
        return Ok(None);
    }
    let is_local = |event: &Event| event.header().sender.server_name() == own_server_name;
    let extremity = actor
        .forward_extremity_ids()
        .into_iter()
        .filter_map(|(event_id, _)| {
            let event = actor.event_by_id(&event_id)?;
            let position = actor.timeline_position(&event_id)?;
            is_local(event).then_some((position, event))
        })
        .max_by_key(|(position, _)| *position)
        .map(|(_, event)| event);
    let latest = match extremity {
        Some(event) => Some(event),
        None => actor
            .paginate(None, Direction::Backward, CATCH_UP_SCAN)
            .0
            .into_iter()
            .find(|event| is_local(event)),
    };
    latest
        .map(|event| {
            serde_json::from_slice(event.canonical_bytes())
                .map_err(|e| RoomError::Internal(format!("stored event is not JSON: {e}")))
        })
        .transpose()
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
