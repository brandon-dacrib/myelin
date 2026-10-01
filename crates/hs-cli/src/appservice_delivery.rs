//! Wires `hs-appservice`'s delivery into the running server: the pump that reads rooms
//! (`hs_appservice::pump`), the pump that reads typing, receipts, presence, to-device messages
//! and device lists (`hs_appservice::ephemeral`, MSC2409/MSC3202/MSC4203), the workers that
//! send what they queue (`hs_appservice::delivery`), and the registries and stores they read
//! from.
//!
//! Until this module existed, a bridge could register, ping, and be masqueraded through, and
//! was sent no event, ever: `Scheduler` delivered a queue nothing filled. See
//! `hs_appservice::pump`'s module docs for how the stream is only a doorbell and the cursor is
//! the truth.
//!
//! # In a cluster
//!
//! Every replica sees every room update, and the queue is shared storage, so two replicas each
//! pumping would queue every event twice, and two each draining would send every transaction
//! twice. RFC 0001's shard layout has a place for both: the pumps are singleton background jobs
//! and run on whichever replica owns [`ShardId::GLOBAL`]; the worker for an appservice runs on
//! whichever replica owns that appservice's shard (`ShardLayout::appservice_shard`). A replica
//! that acquires the global shard catches up on every room; one that acquires an appservice
//! shard nudges every appservice in it; one that loses a shard stops the work it was doing for
//! it. In single-node mode every shard is always mine and none of this is visible.
//!
//! The ephemeral pump reads the receipt, presence, to-device and device-list streams from the
//! shared store, so whichever replica owns the global shard reads everything; typing it reads
//! from this replica's session hub, which holds every replica's typing (the wake batch carries
//! it, decision 0018). The hub's ephemeral observer ([`Doorbell`]) rings on every replica; only
//! the owner acts on it, and the others drop their notes ([`EphemeralPump::discard_typing`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hs_appservice::delivery::Delivery;
use hs_appservice::ephemeral::{
    Change, DeviceKeyCounts, DeviceSource, EphemeralPump, EphemeralSource, KeyCountSource,
    PresenceChange, ReceiptChange, RoomFacts, ToDeviceChange, ToDeviceMessage,
};
use hs_appservice::metrics::AppserviceMetrics;
use hs_appservice::pump::{Pump, RoomEvent, RoomPage, RoomSource};
use hs_appservice::registry::Registry;
use hs_appservice::scheduler::{HttpTransactionSender, Scheduler};
use hs_cluster::ownership::{Ownership, OwnershipEvent};
use hs_cluster::{ShardId, ShardKind, ShardLayout};
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;
use hs_room::routes::render::client_event_json;
use hs_user::cluster::EphemeralUpdate;
use hs_user::hub::{EphemeralObserver, SessionHub};

/// How often the ephemeral pump looks at the streams that have no doorbell (to-device messages
/// and device-list changes), and the backstop for the ones that do.
const EPHEMERAL_POLL: Duration = Duration::from_millis(250);

/// The session hub as `hs serve` builds it.
type Hub<B> = SessionHub<B, Arc<RoomRegistry<B>>>;

/// `hs_appservice::pump::RoomSource` over the real room registry.
struct Rooms<B: KvBackend + 'static> {
    registry: Arc<RoomRegistry<B>>,
}

#[async_trait]
impl<B: KvBackend + 'static> RoomSource for Rooms<B> {
    async fn events_after(
        &self,
        room_id: &str,
        after: i64,
        limit: usize,
    ) -> Result<RoomPage, String> {
        let room_id = ruma::RoomId::parse(room_id).map_err(|e| e.to_string())?;
        let handle = self
            .registry
            .get_or_load(&room_id)
            .await
            .map_err(|e| e.to_string())?;
        handle
            .query(move |actor| {
                let page = actor.events_after(after, limit);
                // Who was joined as of each event: one state read, for the first event of the
                // page, then walked forward -- membership only changes at a membership event,
                // and the page is the room's own order. Shared between events while unchanged.
                let mut members: Arc<BTreeSet<String>> = Arc::new(
                    page.first()
                        .map(|(_, first)| actor.joined_members_after(first))
                        .transpose()
                        .map_err(|e| e.to_string())?
                        .unwrap_or_default()
                        .into_iter()
                        .collect(),
                );
                let mut events = Vec::with_capacity(page.len());
                for (index, (pos, event)) in page.into_iter().enumerate() {
                    if index > 0
                        && event.header().event_type == "m.room.member"
                        && let Some(user) = event.header().state_key.clone()
                    {
                        let json = event.json();
                        let joined = json
                            .get("content")
                            .and_then(|c| c.as_object())
                            .and_then(|c| c.get("membership"))
                            .and_then(|m| m.as_str())
                            == Some("join");
                        if joined != members.contains(&user) {
                            let mut next = (*members).clone();
                            if joined {
                                next.insert(user);
                            } else {
                                next.remove(&user);
                            }
                            members = Arc::new(next);
                        }
                    }
                    events.push(RoomEvent {
                        pos,
                        json: client_event_json(event),
                        joined_members: members.clone(),
                    });
                }
                let aliases = actor.list_aliases().map_err(|e| e.to_string())?;
                Ok(RoomPage { events, aliases })
            })
            .await
    }

    async fn room_heads(&self) -> Result<Vec<(String, i64)>, String> {
        Ok(self
            .registry
            .room_heads()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(room_id, head)| (room_id.to_string(), head))
            .collect())
    }
}

/// The ephemeral pump's sources over the real server: the session hub (typing, presence),
/// its store (the receipt and presence streams, memberships), the room registry (members and
/// aliases) and the E2EE store (to-device messages, device lists, key counts).
struct Sources<B: KvBackend + 'static> {
    hub: Arc<Hub<B>>,
    rooms: Arc<RoomRegistry<B>>,
    e2e: Arc<dyn hs_e2e::store::E2eStore>,
}

fn user(id: &str) -> Result<ruma::OwnedUserId, String> {
    ruma::UserId::parse(id)
        .map(|u| u.to_owned())
        .map_err(|e| e.to_string())
}

fn room(id: &str) -> Result<ruma::OwnedRoomId, String> {
    ruma::RoomId::parse(id)
        .map(|r| r.to_owned())
        .map_err(|e| e.to_string())
}

#[async_trait]
impl<B: KvBackend + 'static> EphemeralSource for Sources<B> {
    async fn typing_in(&self, room_id: &str) -> Result<Vec<String>, String> {
        let (users, _) = self.hub.typing_users(&room(room_id)?).await;
        Ok(users.into_iter().map(|u| u.to_string()).collect())
    }

    async fn receipts_since(&self, since: u64, limit: usize) -> Result<Vec<ReceiptChange>, String> {
        Ok(self
            .hub
            .store()
            .receipt_stream_since(since, limit)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|entry| ReceiptChange {
                pos: entry.pos,
                room_id: entry.room_id,
                user_id: entry.receipt.user_id,
                kind: entry.receipt.kind,
                event_id: entry.receipt.event_id,
                ts: entry.receipt.ts,
            })
            .collect())
    }

    async fn receipts_head(&self) -> Result<u64, String> {
        self.hub
            .store()
            .receipt_stream_head()
            .await
            .map_err(|e| e.to_string())
    }

    async fn prune_receipts_below(&self, below: u64) -> Result<(), String> {
        self.hub
            .store()
            .prune_receipt_stream(below)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn presence_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<PresenceChange>, String> {
        Ok(self
            .hub
            .store()
            .presence_stream_since(since, limit)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|entry| PresenceChange {
                pos: entry.pos,
                user_id: entry.user_id,
            })
            .collect())
    }

    async fn presence_head(&self) -> Result<u64, String> {
        self.hub
            .store()
            .presence_stream_head()
            .await
            .map_err(|e| e.to_string())
    }

    async fn prune_presence_below(&self, below: u64) -> Result<(), String> {
        self.hub
            .store()
            .prune_presence_stream(below)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn presence_content(&self, user_id: &str) -> Result<Option<serde_json::Value>, String> {
        let Some(record) = self.hub.presence_of(&user(user_id)?).await else {
            return Ok(None);
        };
        let mut content = serde_json::json!({
            "presence": record.presence,
            "last_active_ago": record.last_active_ago_ms(),
            "currently_active": record.currently_active(),
        });
        if let Some(msg) = &record.status_msg {
            content["status_msg"] = serde_json::Value::String(msg.clone());
        }
        Ok(Some(content))
    }

    async fn room_facts(&self, room_id: &str) -> Result<Option<RoomFacts>, String> {
        let handle = match self.rooms.get_or_load(&room(room_id)?).await {
            Ok(handle) => handle,
            Err(hs_room::error::RoomError::RoomNotFound(_)) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        handle
            .query(|actor| {
                let joined_members = actor
                    .joined_members()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter_map(|event| event.header().state_key.clone())
                    .collect();
                let aliases = actor.list_aliases().map_err(|e| e.to_string())?;
                Ok(Some(RoomFacts {
                    joined_members,
                    aliases,
                }))
            })
            .await
    }

    async fn joined_rooms_of(&self, user_id: &str) -> Result<Vec<String>, String> {
        Ok(self
            .hub
            .store()
            .list_memberships(&user(user_id)?)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|record| record.membership == "join")
            .map(|record| record.room_id.to_string())
            .collect())
    }
}

#[async_trait]
impl<B: KvBackend + 'static> KeyCountSource for Sources<B> {
    async fn key_counts(&self, user_id: &str) -> Result<Vec<DeviceKeyCounts>, String> {
        let user_id = user(user_id)?;
        let mut out = Vec::new();
        for (device_id, _) in self
            .e2e
            .list_device_keys(&user_id)
            .await
            .map_err(|e| e.to_string())?
        {
            let one_time_keys: BTreeMap<String, u64> = self
                .e2e
                .count_one_time_keys(&user_id, &device_id)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect();
            let unused_fallback_key_types = self
                .e2e
                .unused_fallback_key_algorithms(&user_id, &device_id)
                .await
                .map_err(|e| e.to_string())?;
            out.push(DeviceKeyCounts {
                device_id: device_id.to_string(),
                one_time_keys,
                unused_fallback_key_types,
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl<B: KvBackend + 'static> DeviceSource for Sources<B> {
    async fn to_device_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<ToDeviceChange>, String> {
        Ok(self
            .e2e
            .to_device_stream_since(since, limit)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|entry| ToDeviceChange {
                pos: entry.pos,
                user_id: entry.user_id.to_string(),
                device_id: entry.device_id.to_string(),
                stream_id: entry.stream_id,
            })
            .collect())
    }

    async fn to_device_head(&self) -> Result<u64, String> {
        self.e2e
            .to_device_stream_head()
            .await
            .map_err(|e| e.to_string())
    }

    async fn prune_to_device_below(&self, below: u64) -> Result<(), String> {
        self.e2e
            .prune_to_device_stream(below)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn to_device_message(
        &self,
        user_id: &str,
        device_id: &str,
        stream_id: u64,
    ) -> Result<Option<ToDeviceMessage>, String> {
        let (messages, _) = self
            .e2e
            .poll_since(
                &user(user_id)?,
                &ruma::OwnedDeviceId::from(device_id),
                stream_id.saturating_sub(1),
                1,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(messages
            .into_iter()
            .find(|m| m.stream_id == stream_id)
            .map(|m| ToDeviceMessage {
                sender: m.sender.to_string(),
                event_type: m.event_type,
                content: m.content,
            }))
    }

    async fn device_lists_head(&self) -> Result<u64, String> {
        self.e2e
            .current_stream_pos()
            .await
            .map_err(|e| e.to_string())
    }

    async fn device_lists_changed(
        &self,
        since: u64,
        upto: u64,
    ) -> Result<BTreeSet<String>, String> {
        Ok(self
            .e2e
            .changed_users_since(since, Some(upto))
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|u| u.to_string())
            .collect())
    }
}

/// The hub's ephemeral observer: rings the ephemeral pump's doorbell.
///
/// It holds the pump weakly. The pump reads through the hub ([`Sources`]), and the hub holds
/// this observer, so a strong reference here was a cycle: after shutdown the hub, the room
/// registry, the appservice registry and the stores they hold outlived the server, and kept the
/// embedded store's lock with them. The pump's own task owns it; once that has stopped, a
/// doorbell rings nothing.
struct Doorbell<B: KvBackend + 'static> {
    pump: std::sync::Weak<EphemeralPump<B>>,
}

impl<B: KvBackend + 'static> EphemeralObserver for Doorbell<B> {
    fn ephemeral_changed(&self, update: &EphemeralUpdate) {
        let change = match update {
            EphemeralUpdate::Typing { room_id, .. } => Change::Typing {
                room_id: room_id.to_string(),
            },
            EphemeralUpdate::Receipt { .. } => Change::Receipt,
            EphemeralUpdate::Presence { .. } => Change::Presence,
        };
        if let Some(pump) = self.pump.upgrade() {
            pump.note(&change);
        }
    }
}

/// Which of the delivery work is this replica's: the pump, if it owns the global shard; an
/// appservice's worker, if it owns that appservice's shard. See the module docs.
struct Gate<B: KvBackend + 'static> {
    delivery: Arc<Delivery<B>>,
    ownership: Arc<dyn Ownership>,
    layout: ShardLayout,
    registry: Arc<Registry<B>>,
}

impl<B: KvBackend + 'static> Gate<B> {
    fn pumps_here(&self) -> bool {
        self.ownership.is_mine(ShardId::GLOBAL)
    }

    fn delivers_here(&self, appservice_id: &str) -> bool {
        self.ownership
            .is_mine(self.layout.appservice_shard(appservice_id))
    }

    /// Wakes `appservice_id`'s worker, if delivering to it is this replica's job. The replica
    /// whose job it is finds the queued work on its next timer tick at the latest.
    fn nudge(&self, appservice_id: &str) {
        if self.delivers_here(appservice_id) {
            self.delivery.nudge(appservice_id);
        }
    }

    /// Wakes the worker of every registered appservice this replica delivers to.
    fn nudge_everything_mine(&self) {
        match self.registry.list() {
            Ok(rows) => {
                for row in rows {
                    self.nudge(&row.id);
                }
            }
            Err(error) => {
                tracing::error!(%error, "could not list appservices to start their delivery workers");
            }
        }
    }

    /// Stops the worker of every appservice this replica no longer delivers to.
    fn stop_workers_not_mine(&self) {
        let ownership = self.ownership.clone();
        let layout = self.layout;
        self.delivery
            .retain(move |id| ownership.is_mine(layout.appservice_shard(id)));
    }
}

/// What [`AppserviceDelivery::start`] needs from the rest of the server.
pub struct DeliveryDeps<B: KvBackend + 'static> {
    /// The appservice registry: who to deliver to, and the queues.
    pub appservices: Arc<Registry<B>>,
    /// The ping service, for the admin API's view.
    pub ping: Arc<hs_appservice::ping::PingService<B>>,
    /// The rooms, read by the event pump and for interest.
    pub rooms: Arc<RoomRegistry<B>>,
    /// The session hub: typing, presence, and the receipt and presence streams through its store.
    pub hub: Arc<Hub<B>>,
    /// The E2EE store: to-device messages, device lists and key counts.
    pub e2e: Arc<dyn hs_e2e::store::E2eStore>,
    /// Which shards are this replica's.
    pub ownership: Arc<dyn Ownership>,
    /// Which shard an appservice belongs to.
    pub layout: ShardLayout,
    /// The `hs_appservice_*` counters, if registered.
    pub metrics: Option<AppserviceMetrics>,
}

/// The running delivery machinery: stopped by [`AppserviceDelivery::stop`], which `hs serve`'s
/// shutdown calls.
pub struct AppserviceDelivery<B: KvBackend + 'static> {
    delivery: Arc<Delivery<B>>,
    pump_task: tokio::task::AbortHandle,
    ephemeral_task: tokio::task::AbortHandle,
    /// What the admin API's `appservices.*` operations run against: the same registry,
    /// scheduler and workers, so a replay from the interface is delivered by the worker that
    /// delivers everything else.
    admin_directory: Arc<dyn hs_admin::sources::AppserviceDirectory>,
}

impl<B: KvBackend + 'static> AppserviceDelivery<B> {
    /// Starts delivery: catches up on whatever happened since the last run (or, the first time,
    /// notes where things stand), then follows `rooms`' update stream. Nothing is sent from
    /// inside this call; the workers do that on their own tasks.
    ///
    /// # Errors
    /// Returns the store error if the pump cannot read its cursors or the room heads.
    pub async fn start(
        deps: DeliveryDeps<B>,
    ) -> Result<Self, hs_appservice::error::AppserviceError> {
        let DeliveryDeps {
            appservices,
            ping,
            rooms,
            hub,
            e2e,
            ownership,
            layout,
            metrics,
        } = deps;
        let clock: Arc<dyn hs_auth::clock::Clock> = Arc::new(hs_auth::clock::SystemClock);
        let mut scheduler = Scheduler::new(
            appservices.clone(),
            clock,
            Arc::new(HttpTransactionSender::new()),
        );
        // The admin API's `appservices.logins` asks bridges through this, counted with the
        // other appservice series (`hs_admin_bridge_login_queries_total`).
        let mut bridge_logins = hs_appservice::provisioning::BridgeLogins::new();
        if let Some(metrics) = metrics {
            bridge_logins = bridge_logins.with_metrics(metrics.clone());
            scheduler = scheduler.with_metrics(metrics);
        }
        let scheduler = Arc::new(scheduler);
        let gate = Arc::new(Gate {
            delivery: Delivery::new(scheduler.clone()),
            ownership,
            layout,
            registry: appservices.clone(),
        });
        let admin_directory = Arc::new(
            hs_appservice::admin_directory::RegistryAppserviceDirectory::new(
                appservices.clone(),
                ping,
                scheduler,
                {
                    let gate = gate.clone();
                    Arc::new(move |id: &str| gate.nudge(id))
                },
            )
            .with_bridge_logins(bridge_logins),
        );
        let sources = Arc::new(Sources {
            hub: hub.clone(),
            rooms: rooms.clone(),
            e2e,
        });
        let pump = Arc::new(
            Pump::new(
                appservices.clone(),
                Arc::new(Rooms {
                    registry: rooms.clone(),
                }),
            )
            .with_key_counts(sources.clone()),
        );
        let ephemeral = Arc::new(EphemeralPump::new(appservices, sources.clone(), sources));
        // Installed before the first tick, so that a change in between rings a bell the tick
        // answers, rather than waiting for the timer.
        hub.install_ephemeral_observer(Arc::new(Doorbell {
            pump: Arc::downgrade(&ephemeral),
        }));

        // Subscribed before catching up, so that nothing published in between is missed: an
        // update the catch-up already covered is read again and found to be nothing new.
        let updates = rooms.subscribe_global();
        let ownership_events = gate.ownership.subscribe();
        if gate.pumps_here() {
            for id in pump.start().await? {
                gate.nudge(&id);
            }
        }
        gate.nudge_everything_mine();
        let pump_task = tokio::spawn(Self::follow(pump, gate.clone(), updates, ownership_events))
            .abort_handle();
        let ephemeral_task =
            tokio::spawn(Self::follow_ephemeral(ephemeral, gate.clone())).abort_handle();
        Ok(Self {
            delivery: gate.delivery.clone(),
            pump_task,
            ephemeral_task,
            admin_directory,
        })
    }

    /// The ephemeral pump's task: a tick on every doorbell and every [`EPHEMERAL_POLL`], on the
    /// replica that owns the global shard. Another replica drops the typing rooms it was told
    /// of; the owner was told of the same ones.
    async fn follow_ephemeral(pump: Arc<EphemeralPump<B>>, gate: Arc<Gate<B>>) {
        loop {
            if gate.pumps_here() {
                match pump.tick().await {
                    Ok(ids) => {
                        for id in ids {
                            gate.nudge(&id);
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "appservice ephemeral delivery could not read a stream; the next tick will try again");
                    }
                }
            } else {
                pump.discard_typing();
            }
            let _ = tokio::time::timeout(EPHEMERAL_POLL, pump.wait()).await;
        }
    }

    /// The admin API's view onto this machinery.
    #[must_use]
    pub fn admin_directory(&self) -> Arc<dyn hs_admin::sources::AppserviceDirectory> {
        self.admin_directory.clone()
    }

    async fn follow(
        pump: Arc<Pump<B>>,
        gate: Arc<Gate<B>>,
        mut updates: tokio::sync::broadcast::Receiver<hs_room::protocol::RoomUpdate>,
        mut ownership_events: tokio::sync::broadcast::Receiver<OwnershipEvent>,
    ) {
        loop {
            let touched = tokio::select! {
                update = updates.recv() => match update {
                    Ok(update) => {
                        if !gate.pumps_here() {
                            continue;
                        }
                        pump.pump_room(update.room_id.as_str()).await
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        if !gate.pumps_here() {
                            continue;
                        }
                        // Rings were missed; which rooms is unknowable, so every room is asked.
                        tracing::warn!(missed, "appservice delivery fell behind the room stream; catching up on every room");
                        pump.catch_up().await
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                event = ownership_events.recv() => match event {
                    // The global shard is this replica's now: whatever happened in any room
                    // while somebody else (or nobody) was pumping is caught up on from the
                    // cursors, which are in shared storage.
                    Ok(OwnershipEvent::Acquired(fence)) if fence.shard == ShardId::GLOBAL => {
                        tracing::info!("this replica now runs appservice event delivery");
                        pump.catch_up().await
                    }
                    Ok(OwnershipEvent::Acquired(fence))
                        if fence.shard.kind == ShardKind::Appservice =>
                    {
                        gate.nudge_everything_mine();
                        continue;
                    }
                    Ok(OwnershipEvent::Released(shard) | OwnershipEvent::Lost { shard, .. })
                        if shard.kind == ShardKind::Appservice =>
                    {
                        // Its new owner delivers from here; a batch in flight is abandoned and
                        // stays pending, to be sent again under the same transaction id.
                        gate.stop_workers_not_mine();
                        continue;
                    }
                    // Said as loudly as the acquisition, so the logs of two replicas never read
                    // as if both ran delivery (the first two-pod run's did, 2026-09-28).
                    Ok(OwnershipEvent::Released(shard) | OwnershipEvent::Lost { shard, .. })
                        if shard == ShardId::GLOBAL =>
                    {
                        tracing::info!("this replica no longer runs appservice event delivery");
                        continue;
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        gate.nudge_everything_mine();
                        gate.stop_workers_not_mine();
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => continue,
                },
            };
            match touched {
                Ok(ids) => {
                    for id in ids {
                        gate.nudge(&id);
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "appservice delivery could not read a room; the next update will try again");
                }
            }
        }
    }

    /// Stops following rooms and stops every worker. Anything queued stays queued for the next
    /// start.
    pub fn stop(&self) {
        self.pump_task.abort();
        self.ephemeral_task.abort();
        self.delivery.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::CreateRoomRequest;
    use hs_room::membership::Action;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// An ownership whose answers a test scripts: which shards are mine, changed at will, with
    /// the event a real acquisition or release would publish.
    struct Scripted {
        me: hs_cluster::types::ReplicaId,
        mine: Mutex<HashSet<ShardId>>,
        events: tokio::sync::broadcast::Sender<OwnershipEvent>,
    }

    impl Scripted {
        fn owning_nothing() -> Arc<Self> {
            Arc::new(Self {
                me: hs_cluster::types::ReplicaId::new("replica-b"),
                mine: Mutex::new(HashSet::new()),
                events: tokio::sync::broadcast::channel(16).0,
            })
        }

        fn acquire(&self, shard: ShardId) {
            self.mine.lock().unwrap().insert(shard);
            let _ = self
                .events
                .send(OwnershipEvent::Acquired(hs_cluster::fence::Fence::inert(
                    shard,
                )));
        }

        fn release(&self, shard: ShardId) {
            self.mine.lock().unwrap().remove(&shard);
            let _ = self.events.send(OwnershipEvent::Released(shard));
        }
    }

    impl Ownership for Scripted {
        fn me(&self) -> &hs_cluster::types::ReplicaId {
            &self.me
        }
        fn owner_of(&self, shard: ShardId) -> Option<hs_cluster::types::ReplicaId> {
            self.is_mine(shard).then(|| self.me.clone())
        }
        fn is_mine(&self, shard: ShardId) -> bool {
            self.mine.lock().unwrap().contains(&shard)
        }
        fn fence(&self, shard: ShardId) -> Option<hs_cluster::fence::Fence> {
            self.is_mine(shard)
                .then(|| hs_cluster::fence::Fence::inert(shard))
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<OwnershipEvent> {
            self.events.subscribe()
        }
        fn shard_map(&self) -> tokio::sync::watch::Receiver<Arc<hs_cluster::ownership::ShardMap>> {
            tokio::sync::watch::channel(Arc::new(hs_cluster::ownership::ShardMap::default())).1
        }
    }

    /// A bridge that records what it is sent.
    async fn listening_bridge() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
        #[derive(Clone)]
        struct Received(Arc<Mutex<Vec<serde_json::Value>>>);
        async fn take(
            axum::extract::State(received): axum::extract::State<Received>,
            axum::Json(body): axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            received.0.lock().unwrap().push(body);
            axum::Json(serde_json::json!({}))
        }
        let received = Received(Arc::new(Mutex::new(Vec::new())));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                axum::routing::put(take),
            )
            .with_state(received.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, received.0)
    }

    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    /// A session hub and an E2EE store over the room registry's backend, as `hs serve` has.
    fn hub_and_e2e(
        rooms: &Arc<RoomRegistry<MemoryBackend>>,
    ) -> (Arc<Hub<MemoryBackend>>, Arc<dyn hs_e2e::store::E2eStore>) {
        let store: hs_user::store::DynUserStore =
            Arc::new(hs_user::store::tables::TablesUserStore::open(MemoryBackend::new()).unwrap());
        let hub = Arc::new(SessionHub::new(store, rooms.clone(), usize::MAX));
        let e2e: Arc<dyn hs_e2e::store::E2eStore> =
            Arc::new(hs_e2e::store::tables::TablesE2eStore::open(MemoryBackend::new()).unwrap());
        (hub, e2e)
    }

    /// Two replicas would each queue every event and each send every transaction. So a replica
    /// does the pump's work only while it owns the global shard, and an appservice's delivery
    /// only while it owns that appservice's shard -- and picks each up, from shared storage,
    /// the moment it acquires the shard.
    #[tokio::test]
    async fn a_replica_pumps_and_delivers_only_for_the_shards_it_owns() {
        let backend = MemoryBackend::new();
        let (bridge_url, received) = listening_bridge().await;
        let registry =
            Arc::new(Registry::open(backend.clone(), ruma::server_name!("example.org")).unwrap());
        registry
            .add(
                &hs_appservice::registration::Registration::parse_yaml(&format!(
                    "id: irc\nurl: '{bridge_url}'\nas_token: a\nhs_token: h\n\
                     sender_localpart: ircbot\nnamespaces: {{}}\n"
                ))
                .unwrap(),
            )
            .unwrap();
        let ping = Arc::new(hs_appservice::ping::PingService::new(
            registry.clone(),
            Arc::new(hs_appservice::ping::HttpPingTransport::new()),
        ));
        let rooms = Arc::new(
            RoomRegistry::open(
                backend,
                hs_room::identity::HomeserverIdentity::for_tests("example.org"),
            )
            .unwrap(),
        );
        let ownership = Scripted::owning_nothing();
        let layout = ShardLayout::default();
        let (hub, e2e) = hub_and_e2e(&rooms);
        let delivery = AppserviceDelivery::start(DeliveryDeps {
            appservices: registry.clone(),
            ping,
            rooms: rooms.clone(),
            hub,
            e2e,
            ownership: ownership.clone(),
            layout,
            metrics: None,
        })
        .await
        .unwrap();

        // A room with the bot in it, and something said: this replica owns nothing, so nothing
        // is queued and nothing is sent.
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        let bot = ruma::user_id!("@ircbot:example.org").to_owned();
        let handle = rooms
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
        handle
            .membership(
                bot.clone(),
                Action::Join,
                bot.clone(),
                serde_json::json!({}),
                2,
            )
            .await
            .unwrap();
        handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "while nobody owned anything"}),
                None,
                3,
            )
            .await
            .unwrap();
        settle().await;
        assert!(registry.backlog("irc").unwrap().is_empty());
        assert!(received.lock().unwrap().is_empty());

        // The global shard becomes this replica's: the pump catches up on the room.
        ownership.acquire(ShardId::GLOBAL);
        settle().await;
        assert_eq!(registry.backlog("irc").unwrap().len(), 1);
        // ...but the appservice's shard is still somebody else's, so nothing is sent.
        assert!(received.lock().unwrap().is_empty());

        // Now the appservice's shard too: delivery starts, from the queue.
        ownership.acquire(layout.appservice_shard("irc"));
        settle().await;
        assert_eq!(
            received.lock().unwrap().len(),
            1,
            "{:?}",
            received.lock().unwrap()
        );

        // Losing the appservice shard stops its worker; a new message is queued (the pump is
        // still ours) and not sent.
        ownership.release(layout.appservice_shard("irc"));
        handle
            .send_event(
                alice,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "after the shard moved away"}),
                None,
                4,
            )
            .await
            .unwrap();
        settle().await;
        let pending = registry.backlog("irc").unwrap();
        assert_eq!(pending.len(), 1, "queued for the new owner to send");
        assert_eq!(
            received.lock().unwrap().len(),
            1,
            "not sent by this replica"
        );

        delivery.stop();
    }

    /// The property the first CI run of the bridge test found missing, deterministically: read in
    /// one page, a message from before the bot joined is still not the bot's. Each event carries
    /// the membership as of itself, not the room's membership now.
    #[tokio::test]
    async fn each_event_says_who_was_in_the_room_when_it_happened() {
        let registry = Arc::new(
            RoomRegistry::open(
                MemoryBackend::new(),
                hs_room::identity::HomeserverIdentity::for_tests("example.org"),
            )
            .unwrap(),
        );
        let alice = ruma::user_id!("@alice:example.org").to_owned();
        let bot = ruma::user_id!("@ircbot:example.org").to_owned();
        let handle = registry
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
        let room_id = handle.query(|a| a.room_id().to_owned()).await;
        let say = |body: &'static str, ts: i64| {
            let (handle, alice) = (handle.clone(), alice.clone());
            async move {
                handle
                    .send_event(
                        alice,
                        "m.room.message".to_owned(),
                        None,
                        serde_json::json!({"body": body}),
                        None,
                        ts,
                    )
                    .await
                    .unwrap();
            }
        };
        say("before the bot", 2).await;
        handle
            .membership(
                bot.clone(),
                Action::Join,
                bot.clone(),
                serde_json::json!({}),
                3,
            )
            .await
            .unwrap();
        say("with the bot", 4).await;
        handle
            .membership(
                bot.clone(),
                Action::Leave,
                bot.clone(),
                serde_json::json!({}),
                5,
            )
            .await
            .unwrap();
        say("after the bot left", 6).await;

        let rooms = Rooms { registry };
        let page = rooms.events_after(room_id.as_str(), 0, 100).await.unwrap();
        let bot_was_there = |body: &str| -> bool {
            page.events
                .iter()
                .find(|e| e.json["content"]["body"] == body)
                .unwrap_or_else(|| panic!("{body} is not in the page"))
                .joined_members
                .contains(bot.as_str())
        };
        assert!(!bot_was_there("before the bot"));
        assert!(bot_was_there("with the bot"));
        assert!(!bot_was_there("after the bot left"));
        // The bot's own join and leave count it as there and gone, respectively: the state
        // *after* each event.
        let membership_events: Vec<bool> = page
            .events
            .iter()
            .filter(|e| e.json["type"] == "m.room.member" && e.json["state_key"] == bot.as_str())
            .map(|e| e.joined_members.contains(bot.as_str()))
            .collect();
        assert_eq!(membership_events, vec![true, false]);
    }
}
