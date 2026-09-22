//! Wires `hs-appservice`'s event delivery into the running server: the pump that reads rooms
//! (`hs_appservice::pump`), the workers that send what it queues
//! (`hs_appservice::delivery`), and the room registry they read from.
//!
//! Until this module existed, a bridge could register, ping, and be masqueraded through, and
//! was sent no event, ever: `Scheduler` delivered a queue nothing filled. See
//! `hs_appservice::pump`'s module docs for how the stream is only a doorbell and the cursor is
//! the truth.

use std::sync::Arc;

use async_trait::async_trait;
use hs_appservice::delivery::Delivery;
use hs_appservice::pump::{Pump, RoomEvent, RoomPage, RoomSource};
use hs_appservice::registry::Registry;
use hs_appservice::scheduler::{HttpTransactionSender, Scheduler};
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;
use hs_room::routes::render::client_event_json;

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
                let events = actor
                    .events_after(after, limit)
                    .into_iter()
                    .map(|(pos, event)| RoomEvent {
                        pos,
                        json: client_event_json(event),
                    })
                    .collect();
                let aliases = actor.list_aliases().map_err(|e| e.to_string())?;
                let joined_members = actor
                    .joined_members()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter_map(|event| event.header().state_key.clone())
                    .collect();
                Ok(RoomPage {
                    events,
                    aliases,
                    joined_members,
                })
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

/// The running delivery machinery: stopped by [`AppserviceDelivery::stop`], which `hs serve`'s
/// shutdown calls.
pub struct AppserviceDelivery<B: KvBackend + 'static> {
    delivery: Arc<Delivery<B>>,
    pump_task: tokio::task::AbortHandle,
}

impl<B: KvBackend + 'static> AppserviceDelivery<B> {
    /// Starts delivery: catches up on whatever happened since the last run (or, the first time,
    /// notes where things stand), then follows `rooms`' update stream. Nothing is sent from
    /// inside this call; the workers do that on their own tasks.
    ///
    /// # Errors
    /// Returns the store error if the pump cannot read its cursors or the room heads.
    pub async fn start(
        appservices: Arc<Registry<B>>,
        rooms: Arc<RoomRegistry<B>>,
    ) -> Result<Self, hs_appservice::error::AppserviceError> {
        let clock: Arc<dyn hs_auth::clock::Clock> = Arc::new(hs_auth::clock::SystemClock);
        let scheduler = Arc::new(Scheduler::new(
            appservices.clone(),
            clock,
            Arc::new(HttpTransactionSender::new()),
        ));
        let delivery = Delivery::new(scheduler);
        let pump = Arc::new(Pump::new(
            appservices,
            Arc::new(Rooms {
                registry: rooms.clone(),
            }),
        ));

        // Subscribed before catching up, so that nothing published in between is missed: an
        // update the catch-up already covered is read again and found to be nothing new.
        let updates = rooms.subscribe_global();
        for id in pump.start().await? {
            delivery.nudge(&id);
        }
        let pump_task = tokio::spawn(Self::follow(pump, delivery.clone(), updates)).abort_handle();
        Ok(Self {
            delivery,
            pump_task,
        })
    }

    async fn follow(
        pump: Arc<Pump<B>>,
        delivery: Arc<Delivery<B>>,
        mut updates: tokio::sync::broadcast::Receiver<hs_room::protocol::RoomUpdate>,
    ) {
        loop {
            let touched = match updates.recv().await {
                Ok(update) => pump.pump_room(update.room_id.as_str()).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    // Rings were missed; which rooms is unknowable, so every room is asked.
                    tracing::warn!(
                        missed,
                        "appservice delivery fell behind the room stream; catching up on every room"
                    );
                    pump.catch_up().await
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            match touched {
                Ok(ids) => {
                    for id in ids {
                        delivery.nudge(&id);
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
        self.delivery.stop();
    }
}
