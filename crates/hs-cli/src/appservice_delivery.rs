//! Wires `hs-appservice`'s event delivery into the running server: the pump that reads rooms
//! (`hs_appservice::pump`), the workers that send what it queues
//! (`hs_appservice::delivery`), and the room registry they read from.
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
//! twice. RFC 0001's shard layout has a place for both: the pump is a singleton background job
//! and runs on whichever replica owns [`ShardId::GLOBAL`]; the worker for an appservice runs on
//! whichever replica owns that appservice's shard (`ShardLayout::appservice_shard`). A replica
//! that acquires the global shard catches up on every room; one that acquires an appservice
//! shard nudges every appservice in it; one that loses a shard stops the work it was doing for
//! it. In single-node mode every shard is always mine and none of this is visible.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use hs_appservice::delivery::Delivery;
use hs_appservice::pump::{Pump, RoomEvent, RoomPage, RoomSource};
use hs_appservice::registry::Registry;
use hs_appservice::scheduler::{HttpTransactionSender, Scheduler};
use hs_cluster::ownership::{Ownership, OwnershipEvent};
use hs_cluster::{ShardId, ShardKind, ShardLayout};
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

/// The running delivery machinery: stopped by [`AppserviceDelivery::stop`], which `hs serve`'s
/// shutdown calls.
pub struct AppserviceDelivery<B: KvBackend + 'static> {
    delivery: Arc<Delivery<B>>,
    pump_task: tokio::task::AbortHandle,
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
        appservices: Arc<Registry<B>>,
        ping: Arc<hs_appservice::ping::PingService<B>>,
        rooms: Arc<RoomRegistry<B>>,
        ownership: Arc<dyn Ownership>,
        layout: ShardLayout,
    ) -> Result<Self, hs_appservice::error::AppserviceError> {
        let clock: Arc<dyn hs_auth::clock::Clock> = Arc::new(hs_auth::clock::SystemClock);
        let scheduler = Arc::new(Scheduler::new(
            appservices.clone(),
            clock,
            Arc::new(HttpTransactionSender::new()),
        ));
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
            ),
        );
        let pump = Arc::new(Pump::new(
            appservices,
            Arc::new(Rooms {
                registry: rooms.clone(),
            }),
        ));

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
        Ok(Self {
            delivery: gate.delivery.clone(),
            pump_task,
            admin_directory,
        })
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
        let delivery = AppserviceDelivery::start(
            registry.clone(),
            ping,
            rooms.clone(),
            ownership.clone(),
            layout,
        )
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
