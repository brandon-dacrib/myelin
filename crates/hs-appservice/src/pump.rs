//! The pump: what turns "something happened in a room" into "a transaction is queued for every
//! appservice that wants to know".
//!
//! [`crate::scheduler`] delivers what is in the queue, in order, with retries, across restarts.
//! Nothing put anything *in* the queue: the server ran, bridges registered, and no event ever
//! reached one. This module is that missing half, and [`crate::delivery`] is what drives the
//! scheduler once there is something to deliver.
//!
//! # A cursor per room, and the stream as a doorbell
//!
//! A room's events are totally ordered by their room-local position; nothing orders events
//! *across* rooms in a way that is safe to follow. (Events have a global sequence number, but
//! two rooms allocate and commit concurrently, so the higher number can land first -- a reader
//! that has moved past it never sees the lower one.) So the pump keeps, durably, how far it has
//! read into each room ([`crate::store::AppserviceStore::room_cursor`]), and
//! [`Pump::pump_room`] reads from there to the room's head.
//!
//! The room registry's update stream says *when* to do that, and nothing more. It is a broadcast
//! channel: it does not replay, it drops messages for a slow reader, and whatever was published
//! while the server was down was published to nobody. Used as the source of events it would lose
//! them in all three cases. Used as a doorbell it cannot: a missed ring is made up for by the
//! next one, or by [`Pump::catch_up`], because what to read is decided by the cursor.
//!
//! Queueing a room's events and moving its cursor are one transaction
//! ([`crate::store::AppserviceStore::enqueue_for_room`]), so a crash leaves an event either
//! queued and accounted for, or neither.
//!
//! # Who wants to know
//!
//! [`interested`] is Synapse's rule, which is the one bridges are written against: an appservice
//! is sent an event if the sender or (for a membership event) the target is one of its users; if
//! the room's ID or one of its aliases is in its namespaces; or if **any current member of the
//! room is one of its users**. The last is the one that matters: it is how a bridge hears
//! everything said in a room its bot or one of its ghosts is in. Its bot (`sender_localpart`)
//! counts as one of its users whether or not the `users` namespace happens to match it.
//!
//! Membership is membership *as of each event*, not the room's current membership. Synapse
//! reads the current members, and gets away with it because it decides at the moment the event
//! is sent; this pump decides later, from a cursor, and may read the bot's own join and the
//! message before it in one page. Decided against the current members, that message would go
//! out to the bridge, and from there to wherever it bridges to -- which is how the first CI run
//! of the bridge test found this. A [`RoomSource`] supplies, with each event, who was joined
//! once it had happened.
//!
//! # The first start
//!
//! A server that has been running has history, and a bridge registered today did not ask for
//! it. The first time the pump starts against a store it reads nothing: it records where every
//! existing room stands and that it has done so
//! ([`crate::store::AppserviceStore::start_pump_at`]). After that, a room with no cursor is a
//! room created since -- while the server was up or while it was down -- and all of it is news.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use hs_kv::KvBackend;
use serde_json::Value;

use crate::error::AppserviceError;
use crate::namespace::{NamespaceKind, Namespaces};
use crate::registry::Registry;
use crate::scheduler::wire_body;
use crate::store::AppserviceRow;
use crate::transaction::Transaction;

/// The most events [`Pump::pump_room`] reads from a room, and queues, at a time. A room that is
/// further behind than this is read in several steps, each its own transaction.
pub const PUMP_PAGE: usize = 100;

/// One event as the pump reads it from a room.
#[derive(Debug, Clone)]
pub struct RoomEvent {
    /// The event's room-local position: what the room's cursor becomes once this is dealt with.
    pub pos: i64,
    /// The event in the client-server format, which is the format appservice transactions carry
    /// (`room_id`, `event_id`, `sender`, `type`, `state_key`, `content`, `origin_server_ts`,
    /// `unsigned`).
    pub json: Value,
    /// The user IDs joined to the room once this event had happened. Shared between consecutive
    /// events when nothing changed in between, which is nearly always.
    pub joined_members: Arc<BTreeSet<String>>,
}

/// What [`RoomSource::events_after`] returns: some of a room's events, and what the pump needs to
/// know about the room to decide who they are for.
#[derive(Debug, Clone, Default)]
pub struct RoomPage {
    /// Events after the requested position, oldest first.
    pub events: Vec<RoomEvent>,
    /// The room's current aliases.
    pub aliases: Vec<String>,
}

/// Where the pump reads rooms from. A trait because this crate does not (and should not) depend
/// on the room engine: `hs-cli` implements it over `hs_room::registry::RoomRegistry`.
#[async_trait]
pub trait RoomSource: Send + Sync {
    /// Up to `limit` of `room_id`'s events after room-local position `after`, oldest first, in
    /// the order the room holds them. Events at negative positions (history fetched from other
    /// servers after the fact) are never returned: they are not news.
    async fn events_after(
        &self,
        room_id: &str,
        after: i64,
        limit: usize,
    ) -> Result<RoomPage, String>;

    /// Every room this server holds, with the position of its newest event. Must not need each
    /// room loaded into memory: it is called at every start.
    async fn room_heads(&self) -> Result<Vec<(String, i64)>, String>;
}

/// Whether `appservice` should be sent `event`, which is one of `page`'s. See the module docs.
#[must_use]
pub fn interested(
    namespaces: &Namespaces,
    bot_user_id: &str,
    room_id: &str,
    event: &RoomEvent,
    page: &RoomPage,
) -> bool {
    let is_theirs = |user_id: &str| {
        user_id == bot_user_id || namespaces.is_interested(NamespaceKind::Users, user_id)
    };
    let field = |name: &str| event.json.get(name).and_then(Value::as_str);

    if field("sender").is_some_and(is_theirs) {
        return true;
    }
    if field("type") == Some("m.room.member") && field("state_key").is_some_and(is_theirs) {
        return true;
    }
    if namespaces.is_interested(NamespaceKind::Rooms, room_id) {
        return true;
    }
    if page
        .aliases
        .iter()
        .any(|alias| namespaces.is_interested(NamespaceKind::Aliases, alias))
    {
        return true;
    }
    event.joined_members.iter().any(|member| is_theirs(member))
}

/// An appservice, as the pump needs it while it reads one page: compiled once, not per event.
struct Listener {
    row: AppserviceRow,
    namespaces: Namespaces,
    bot_user_id: String,
}

/// See the module docs.
pub struct Pump<B: KvBackend> {
    registry: Arc<Registry<B>>,
    source: Arc<dyn RoomSource>,
}

impl<B: KvBackend> Pump<B> {
    /// Builds a pump that reads rooms from `source` and queues for the appservices in `registry`.
    #[must_use]
    pub fn new(registry: Arc<Registry<B>>, source: Arc<dyn RoomSource>) -> Self {
        Self { registry, source }
    }

    /// Every appservice that can be sent anything: one with a `url`. (A registration with
    /// `url: null` exists to give a bridge an `as_token`, and is never pushed to.) A
    /// registration whose namespaces no longer compile is skipped, loudly: it was accepted once,
    /// and one bad pattern must not stop every other bridge from hearing anything.
    fn listeners(&self) -> Result<Vec<Listener>, AppserviceError> {
        let mut out = Vec::new();
        for row in self.registry.list()? {
            if row.url.is_none() {
                continue;
            }
            let namespaces = match row.namespaces.compile() {
                Ok(namespaces) => namespaces,
                Err(error) => {
                    tracing::error!(appservice = %row.id, %error, "this appservice's namespaces do not compile; it is being sent nothing");
                    continue;
                }
            };
            let bot_user_id = format!("@{}:{}", row.sender_localpart, self.registry.server_name());
            out.push(Listener {
                row,
                namespaces,
                bot_user_id,
            });
        }
        Ok(out)
    }

    /// Called at start, before anything else. The first time ever, records the present and reads
    /// nothing (see the module docs); every other time, [`Pump::catch_up`].
    ///
    /// Returns the appservices that now have something new queued.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] if the store or the room source fails.
    pub async fn start(&self) -> Result<Vec<String>, AppserviceError> {
        if self.registry.store().pump_has_started()? {
            return self.catch_up().await;
        }
        let heads = self
            .source
            .room_heads()
            .await
            .map_err(AppserviceError::Store)?;
        self.registry.store().start_pump_at(&heads)?;
        tracing::info!(
            rooms = heads.len(),
            "appservice event delivery starts here: what has already happened in these rooms will not be sent"
        );
        Ok(Vec::new())
    }

    /// Reads every room that has moved past its cursor. For a start after the first, and for a
    /// doorbell that is known to have been missed (a lagged stream).
    ///
    /// Returns the appservices that now have something new queued.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] if the store or the room source fails.
    pub async fn catch_up(&self) -> Result<Vec<String>, AppserviceError> {
        let heads = self
            .source
            .room_heads()
            .await
            .map_err(AppserviceError::Store)?;
        let mut touched = BTreeMap::new();
        for (room_id, head) in heads {
            let cursor = self.registry.store().room_cursor(&room_id)?.unwrap_or(0);
            if head > cursor {
                for id in self.pump_room(&room_id).await? {
                    touched.insert(id, ());
                }
            }
        }
        Ok(touched.into_keys().collect())
    }

    /// Reads `room_id` from its cursor to its head, queueing each event for every appservice that
    /// wants it, and moving the cursor. Must not be called concurrently for the same room: two
    /// readers starting from one cursor queue everything twice. (One task calls it; see
    /// `hs-cli`.)
    ///
    /// Returns the appservices that now have something new queued.
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] if the store or the room source fails.
    pub async fn pump_room(&self, room_id: &str) -> Result<Vec<String>, AppserviceError> {
        let listeners = self.listeners()?;
        let mut touched = BTreeMap::new();
        loop {
            let cursor = self.registry.store().room_cursor(room_id)?.unwrap_or(0);
            let page = self
                .source
                .events_after(room_id, cursor, PUMP_PAGE)
                .await
                .map_err(AppserviceError::Store)?;
            let Some(last) = page.events.last() else {
                break;
            };
            let new_cursor = last.pos;

            let mut deliveries = Vec::new();
            for listener in &listeners {
                let events: Vec<Value> = page
                    .events
                    .iter()
                    .filter(|event| {
                        interested(
                            &listener.namespaces,
                            &listener.bot_user_id,
                            room_id,
                            event,
                            &page,
                        )
                    })
                    .map(|event| event.json.clone())
                    .collect();
                let transaction = Transaction {
                    events,
                    ..Transaction::default()
                };
                if let Some(body) = wire_body(&listener.row, &transaction) {
                    deliveries.push((listener.row.id.clone(), body));
                }
            }
            // With nobody interested this still runs: it is what moves the cursor.
            self.registry.store().enqueue_for_room(
                room_id,
                new_cursor,
                &deliveries,
                self.registry.now_ms(),
            )?;
            for (id, _) in deliveries {
                touched.insert(id, ());
            }
            if page.events.len() < PUMP_PAGE {
                break;
            }
        }
        Ok(touched.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::Registration;
    use hs_kv::memory::MemoryBackend;
    use serde_json::json;
    use std::sync::Mutex;

    /// Rooms in memory. Each event carries who was joined once it had happened, as the real
    /// source does.
    #[derive(Default)]
    struct FakeRooms {
        rooms: Mutex<BTreeMap<String, RoomPage>>,
    }

    impl FakeRooms {
        fn members_now(room: &RoomPage) -> Arc<BTreeSet<String>> {
            room.events
                .last()
                .map(|e| e.joined_members.clone())
                .unwrap_or_default()
        }

        fn say(&self, room_id: &str, sender: &str, body: &str) {
            let mut rooms = self.rooms.lock().unwrap();
            let room = rooms.entry(room_id.to_owned()).or_default();
            let pos = room.events.last().map_or(1, |e| e.pos + 1);
            let joined_members = Self::members_now(room);
            room.events.push(RoomEvent {
                pos,
                json: json!({
                    "type": "m.room.message",
                    "room_id": room_id,
                    "event_id": format!("${room_id}-{pos}"),
                    "sender": sender,
                    "content": {"msgtype": "m.text", "body": body},
                }),
                joined_members,
            });
        }

        fn join(&self, room_id: &str, user_id: &str) {
            let mut rooms = self.rooms.lock().unwrap();
            let room = rooms.entry(room_id.to_owned()).or_default();
            let pos = room.events.last().map_or(1, |e| e.pos + 1);
            let mut members = (*Self::members_now(room)).clone();
            members.insert(user_id.to_owned());
            room.events.push(RoomEvent {
                pos,
                json: json!({
                    "type": "m.room.member",
                    "room_id": room_id,
                    "event_id": format!("${room_id}-{pos}"),
                    "sender": user_id,
                    "state_key": user_id,
                    "content": {"membership": "join"},
                }),
                joined_members: Arc::new(members),
            });
        }
    }

    #[async_trait]
    impl RoomSource for FakeRooms {
        async fn events_after(
            &self,
            room_id: &str,
            after: i64,
            limit: usize,
        ) -> Result<RoomPage, String> {
            let rooms = self.rooms.lock().unwrap();
            let Some(room) = rooms.get(room_id) else {
                return Ok(RoomPage::default());
            };
            Ok(RoomPage {
                events: room
                    .events
                    .iter()
                    .filter(|e| e.pos > after)
                    .take(limit)
                    .cloned()
                    .collect(),
                aliases: room.aliases.clone(),
            })
        }

        async fn room_heads(&self) -> Result<Vec<(String, i64)>, String> {
            Ok(self
                .rooms
                .lock()
                .unwrap()
                .iter()
                .map(|(id, room)| (id.clone(), room.events.last().map_or(0, |e| e.pos)))
                .collect())
        }
    }

    const BRIDGE: &str = "id: irc\nurl: 'http://localhost:1'\nas_token: as_secret\n\
        hs_token: hs_secret\nsender_localpart: ircbot\nnamespaces:\n  users:\n    \
        - regex: '@irc_.*:example\\.org'\n      exclusive: true\n  aliases:\n    \
        - regex: '#irc_.*:example\\.org'\n      exclusive: true\n";

    fn setup(
        registrations: &[&str],
    ) -> (
        Arc<Registry<MemoryBackend>>,
        Arc<FakeRooms>,
        Pump<MemoryBackend>,
    ) {
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), ruma::server_name!("example.org")).unwrap(),
        );
        for yaml in registrations {
            registry
                .add(&Registration::parse_yaml(yaml).unwrap())
                .unwrap();
        }
        let rooms = Arc::new(FakeRooms::default());
        let pump = Pump::new(registry.clone(), rooms.clone());
        (registry, rooms, pump)
    }

    /// Every event queued for `id`, in queue order, by body text (or membership target).
    fn queued(registry: &Registry<MemoryBackend>, id: &str) -> Vec<String> {
        registry
            .store()
            .queue_for(id)
            .unwrap()
            .iter()
            .flat_map(|entry| entry.body["events"].as_array().cloned().unwrap_or_default())
            .map(|event| {
                event["content"]["body"]
                    .as_str()
                    .or(event["state_key"].as_str())
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_bridge_hears_everything_in_a_room_its_bot_is_in_and_nothing_elsewhere() {
        let (registry, rooms, pump) = setup(&[BRIDGE]);
        pump.start().await.unwrap();

        rooms.join("!bridged:example.org", "@alice:example.org");
        rooms.join("!bridged:example.org", "@ircbot:example.org");
        rooms.say("!bridged:example.org", "@alice:example.org", "hello irc");
        rooms.join("!private:example.org", "@alice:example.org");
        rooms.say(
            "!private:example.org",
            "@alice:example.org",
            "nobody else hears this",
        );

        let touched = pump.pump_room("!bridged:example.org").await.unwrap();
        assert_eq!(touched, vec!["irc".to_owned()]);
        assert!(
            pump.pump_room("!private:example.org")
                .await
                .unwrap()
                .is_empty()
        );

        // From the bot's own join onwards: alice's join, before it, is not the bridge's to
        // hear. None of the private room.
        assert_eq!(
            queued(&registry, "irc"),
            vec!["@ircbot:example.org", "hello irc"]
        );
    }

    #[tokio::test]
    async fn each_way_of_being_interested_is_enough_on_its_own() {
        let (_, _, pump) = setup(&[BRIDGE]);
        let listener = pump.listeners().unwrap().remove(0);
        let ask = |event: RoomEvent, page: &RoomPage, room_id: &str| {
            interested(
                &listener.namespaces,
                &listener.bot_user_id,
                room_id,
                &event,
                page,
            )
        };
        let nobody = RoomPage::default();
        let event = |json: Value, members: &[&str]| RoomEvent {
            pos: 1,
            json,
            joined_members: Arc::new(members.iter().map(|m| (*m).to_owned()).collect()),
        };
        let message = |sender: &str| json!({"type": "m.room.message", "sender": sender});

        assert!(!ask(
            event(message("@alice:example.org"), &[]),
            &nobody,
            "!r:example.org"
        ));
        // Sent by one of its ghosts, or by its bot, which its namespace does not mention.
        assert!(ask(
            event(message("@irc_bob:example.org"), &[]),
            &nobody,
            "!r:example.org"
        ));
        assert!(ask(
            event(message("@ircbot:example.org"), &[]),
            &nobody,
            "!r:example.org"
        ));
        // An invitation to one of its ghosts, who is not in the room yet.
        assert!(ask(
            event(
                json!({"type": "m.room.member", "sender": "@alice:example.org", "state_key": "@irc_bob:example.org"}),
                &[]
            ),
            &nobody,
            "!r:example.org"
        ));
        // ...but a state key is only a user for a membership event.
        assert!(!ask(
            event(
                json!({"type": "org.example.thing", "sender": "@alice:example.org", "state_key": "@irc_bob:example.org"}),
                &[]
            ),
            &nobody,
            "!r:example.org"
        ));
        // One of the room's aliases is in its namespace.
        let aliased = RoomPage {
            aliases: vec!["#irc_libera_rust:example.org".to_owned()],
            ..RoomPage::default()
        };
        assert!(ask(
            event(message("@alice:example.org"), &[]),
            &aliased,
            "!r:example.org"
        ));
        // One of its ghosts was in the room when this was said -- and only then.
        assert!(ask(
            event(
                message("@alice:example.org"),
                &["@alice:example.org", "@irc_bob:example.org"]
            ),
            &nobody,
            "!r:example.org"
        ));
    }

    #[tokio::test]
    async fn the_first_start_sends_no_history_and_every_later_one_sends_what_was_missed() {
        let (registry, rooms, pump) = setup(&[BRIDGE]);
        rooms.join("!old:example.org", "@ircbot:example.org");
        rooms.say(
            "!old:example.org",
            "@alice:example.org",
            "said before delivery existed",
        );

        assert!(pump.start().await.unwrap().is_empty());
        assert!(queued(&registry, "irc").is_empty());

        // The server goes down. Things happen that nobody rings a bell for: in a room it knew,
        // and in one created since.
        rooms.say(
            "!old:example.org",
            "@alice:example.org",
            "said while the pump was not looking",
        );
        rooms.join("!new:example.org", "@ircbot:example.org");
        rooms.say(
            "!new:example.org",
            "@alice:example.org",
            "in a room made since",
        );

        let restarted = Pump::new(registry.clone(), rooms.clone());
        assert_eq!(restarted.start().await.unwrap(), vec!["irc".to_owned()]);
        assert_eq!(
            queued(&registry, "irc"),
            vec![
                "@ircbot:example.org",
                "in a room made since",
                "said while the pump was not looking",
            ]
        );

        // And once more changes nothing: every cursor is at its room's head.
        assert!(restarted.start().await.unwrap().is_empty());
        assert_eq!(queued(&registry, "irc").len(), 3);
    }

    #[tokio::test]
    async fn ringing_twice_queues_once_and_a_long_absence_is_read_in_order() {
        let (registry, rooms, pump) = setup(&[BRIDGE]);
        pump.start().await.unwrap();
        rooms.join("!busy:example.org", "@ircbot:example.org");
        for n in 0..(PUMP_PAGE * 2 + 5) {
            rooms.say(
                "!busy:example.org",
                "@alice:example.org",
                &format!("message {n}"),
            );
        }

        pump.pump_room("!busy:example.org").await.unwrap();
        assert!(
            pump.pump_room("!busy:example.org")
                .await
                .unwrap()
                .is_empty()
        );

        let heard = queued(&registry, "irc");
        assert_eq!(heard.len(), PUMP_PAGE * 2 + 6);
        assert_eq!(heard[1], "message 0");
        assert_eq!(
            heard.last().unwrap(),
            &format!("message {}", PUMP_PAGE * 2 + 4)
        );
    }

    #[tokio::test]
    async fn an_appservice_with_no_url_is_queued_nothing_and_does_not_hold_the_cursor_back() {
        let puppet = "id: puppet\nurl: null\nas_token: puppet_as\nhs_token: puppet_hs\n\
            sender_localpart: puppetbot\nnamespaces:\n  users:\n    - regex: '@.*:example\\.org'\n      \
            exclusive: false\n";
        let (registry, rooms, pump) = setup(&[puppet]);
        pump.start().await.unwrap();
        rooms.say("!r:example.org", "@alice:example.org", "hello");

        assert!(pump.pump_room("!r:example.org").await.unwrap().is_empty());
        assert!(registry.store().queue_for("puppet").unwrap().is_empty());
        assert_eq!(
            registry.store().room_cursor("!r:example.org").unwrap(),
            Some(1)
        );
    }
}
