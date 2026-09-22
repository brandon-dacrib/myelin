//! Wires `hs-appservice`'s event delivery into the running server: the pump that reads rooms
//! (`hs_appservice::pump`), the workers that send what it queues
//! (`hs_appservice::delivery`), and the room registry they read from.
//!
//! Until this module existed, a bridge could register, ping, and be masqueraded through, and
//! was sent no event, ever: `Scheduler` delivered a queue nothing filled. See
//! `hs_appservice::pump`'s module docs for how the stream is only a doorbell and the cursor is
//! the truth.

use std::collections::BTreeSet;
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

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::CreateRoomRequest;
    use hs_room::membership::Action;

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
