//! The ephemeral pump: what turns typing, receipts, presence, to-device messages and device-list
//! changes into transactions for the appservices that asked for them -- MSC2409
//! (`receive_ephemeral` / `de.sorunome.msc2409.push_ephemeral`), MSC4203 (to-device, including
//! `m.room.encrypted`) and MSC3202 (`org.matrix.msc3202`: device lists and one-time-key counts).
//!
//! [`crate::pump`] does the same for a room's events, from a durable cursor per room, with the
//! room stream as a doorbell. This module has the same shape, with the differences the data
//! forces:
//!
//! # Streams and positions
//!
//! Receipts, presence and to-device messages each have a server-wide stream in the store (one
//! entry per write, appended in the write's own transaction; the traits in [`EphemeralSource`]
//! and [`DeviceSource`] read them), and device-list changes have had one since `hs-e2e` began.
//! For each appservice and each stream the pump keeps, durably, the position it has been sent
//! up to ([`crate::store::AppserviceStore::ephemeral_pos`]), and queueing a transaction and
//! moving the positions it accounts for is one store transaction
//! ([`crate::store::AppserviceStore::enqueue_ephemeral`]): a restart resends nothing, and
//! misses nothing that is in a stream. This is Synapse's design (its
//! `application_services_state` stream positions per appservice and stream), with two
//! deliberate differences: an appservice that has never been sent a stream starts at the
//! stream's head, not its beginning -- a bridge registered today did not ask for last week's
//! receipts -- and every appservice that wants a stream is advanced through it in the same
//! tick, so the streams can be pruned below the lowest position, which Synapse never does.
//!
//! Typing has no stream and no position, here as in Synapse ("due to performance reasons and
//! due to their highly ephemeral nature"): it is in every replica's memory, kept there by the
//! wake batch (`hs_user::cluster`, decision 0018), and the pump is told which rooms' typing
//! changed ([`EphemeralPump::note`]) and sends each such room's current typing set once per
//! tick. Typing that changes while nobody is pumping is lost, and is stale within seconds
//! anyway.
//!
//! # The doorbell
//!
//! `hs_user::hub::SessionHub` tells an installed observer of every typing, receipt and presence
//! change it applies, its own clients' and other replicas' alike; `hs-cli` installs one that
//! calls [`EphemeralPump::note`], which records the room (for typing) and wakes the pump's
//! task. To-device messages and device-list changes have no doorbell and are found by the
//! task's timer. The doorbell is latency, never correctness: a tick reads every stream from
//! its positions whatever woke it.
//!
//! # Who wants to know
//!
//! Synapse's rules, which are what bridges are written against:
//!
//! - Typing and receipts for a room the appservice is interested in: the room's ID or one of
//!   its aliases is in its namespaces, or one of its current members is one of its users (the
//!   same rule as [`crate::pump::interested`], against the room's current membership since an
//!   ephemeral event has no position to read membership as of). A private receipt
//!   (`m.read.private`) is sent only when it is one of the appservice's own users'.
//! - Presence of a user who is one of its users, or who is in a room it is interested in.
//! - To-device messages addressed to one of its users (its bot included). The message stays in
//!   the device's own queue for that device's `/sync` (the trait docs at
//!   `hs_e2e::store::ToDeviceStore` say why).
//! - Device-list changes of a user who is one of its users or in a room it is interested in
//!   (`left` is never filled, as in Synapse).
//! - One-time-key counts and unused fallback key types, in every transaction for an MSC3202
//!   appservice, for its bot, its users in the rooms the transaction's ephemeral events name,
//!   and the recipients of its to-device messages -- Synapse's `interesting users` rule,
//!   computed when the transaction is queued rather than sent.
//!
//! # What runs where
//!
//! One task, on the replica that owns the global shard, like the event pump (`hs-cli`'s
//! `appservice_delivery`): the streams are in the shared store, and every replica's hub applies
//! every typing update, so that replica sees everything.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hs_kv::KvBackend;
use serde_json::Value;
use tokio::sync::Notify;

use crate::error::AppserviceError;
use crate::namespace::{NamespaceKind, Namespaces};
use crate::pump::{Listener, listeners};
use crate::registry::Registry;
use crate::scheduler::wire_body;
use crate::store::EphemeralDelivery;
use crate::transaction::{
    DeviceListsUpdate, OneTimeKeysCount, ToDeviceEntry, Transaction, UnusedFallbackKeyTypes,
    presence_event, receipt_content, receipt_event, typing_event,
};

/// The most entries [`EphemeralPump::tick`] reads from one stream at a time; a stream further
/// behind is read in several pages, each its own store transaction.
pub const EPHEMERAL_PAGE: usize = 200;

/// The receipt stream's name, as positions are stored under.
pub const RECEIPTS: &str = "receipts";
/// The presence stream's name.
pub const PRESENCE: &str = "presence";
/// The to-device stream's name.
pub const TO_DEVICE: &str = "to_device";
/// The device-list stream's name.
pub const DEVICE_LISTS: &str = "device_lists";

/// One receipt, as the receipt stream reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptChange {
    /// Its position in the receipt stream.
    pub pos: u64,
    /// The room.
    pub room_id: String,
    /// Whose receipt.
    pub user_id: String,
    /// `m.read` or `m.read.private`.
    pub kind: String,
    /// The event the receipt points at.
    pub event_id: String,
    /// When the receipt was sent, in milliseconds since the Unix epoch.
    pub ts: u64,
}

/// One presence change, as the presence stream reports it. The record itself is read with
/// [`EphemeralSource::presence_content`]: the current one is what an appservice wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceChange {
    /// Its position in the presence stream.
    pub pos: u64,
    /// Whose presence changed.
    pub user_id: String,
}

/// One queued to-device message, as the to-device stream reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToDeviceChange {
    /// Its position in the to-device stream.
    pub pos: u64,
    /// The recipient.
    pub user_id: String,
    /// The recipient's device.
    pub device_id: String,
    /// The message's position in that device's own queue.
    pub stream_id: u64,
}

/// A to-device message, as [`DeviceSource::to_device_message`] returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToDeviceMessage {
    /// The sender.
    pub sender: String,
    /// The event type (`m.room_key`, `m.room.encrypted`, ...).
    pub event_type: String,
    /// The content, untouched.
    pub content: Value,
}

/// What deciding whether an appservice is interested in a room needs to know about it now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomFacts {
    /// The room's current joined members.
    pub joined_members: BTreeSet<String>,
    /// The room's current aliases.
    pub aliases: Vec<String>,
}

/// One device's key counts, for MSC3202.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceKeyCounts {
    /// The device.
    pub device_id: String,
    /// Remaining one-time keys by algorithm (only algorithms with at least one).
    pub one_time_keys: BTreeMap<String, u64>,
    /// Algorithms with an unused fallback key.
    pub unused_fallback_key_types: Vec<String>,
}

/// Where the pump reads typing, receipts, presence and room facts from. A trait because this
/// crate does not depend on `hs-user` or `hs-room`: `hs-cli` implements it over the session hub,
/// its store and the room registry.
#[async_trait]
pub trait EphemeralSource: Send + Sync {
    /// Who is typing in `room_id` right now.
    async fn typing_in(&self, room_id: &str) -> Result<Vec<String>, String>;

    /// Receipt stream entries after `since`, oldest first, at most `limit`.
    async fn receipts_since(&self, since: u64, limit: usize) -> Result<Vec<ReceiptChange>, String>;
    /// The receipt stream's newest position (`0` if empty).
    async fn receipts_head(&self) -> Result<u64, String>;
    /// Deletes receipt stream entries below `below`.
    async fn prune_receipts_below(&self, below: u64) -> Result<(), String>;

    /// Presence stream entries after `since`, oldest first, at most `limit`.
    async fn presence_since(&self, since: u64, limit: usize)
    -> Result<Vec<PresenceChange>, String>;
    /// The presence stream's newest position (`0` if empty).
    async fn presence_head(&self) -> Result<u64, String>;
    /// Deletes presence stream entries below `below`.
    async fn prune_presence_below(&self, below: u64) -> Result<(), String>;
    /// `user_id`'s current presence as `m.presence` content (`presence`, `last_active_ago`,
    /// `currently_active`, `status_msg`), or `None` if they have none.
    async fn presence_content(&self, user_id: &str) -> Result<Option<Value>, String>;

    /// `room_id`'s current members and aliases, or `None` for a room this server does not have.
    async fn room_facts(&self, room_id: &str) -> Result<Option<RoomFacts>, String>;
    /// The rooms `user_id` is joined to.
    async fn joined_rooms_of(&self, user_id: &str) -> Result<Vec<String>, String>;
}

/// Where MSC3202's key counts come from: one-time-key counts and unused fallback key types
/// for every device of a user that has uploaded keys. Split from [`DeviceSource`] because the
/// event pump ([`crate::pump`]) needs only this.
#[async_trait]
pub trait KeyCountSource: Send + Sync {
    /// Key counts for each of `user_id`'s devices with uploaded keys.
    async fn key_counts(&self, user_id: &str) -> Result<Vec<DeviceKeyCounts>, String>;
}

/// Where the pump reads to-device messages and device-list changes from: `hs-cli` implements
/// it over `hs-e2e`'s store.
#[async_trait]
pub trait DeviceSource: KeyCountSource {
    /// To-device stream entries after `since`, oldest first, at most `limit`.
    async fn to_device_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<ToDeviceChange>, String>;
    /// The to-device stream's newest position (`0` if empty).
    async fn to_device_head(&self) -> Result<u64, String>;
    /// Deletes to-device stream entries below `below`.
    async fn prune_to_device_below(&self, below: u64) -> Result<(), String>;
    /// The message at `stream_id` in `(user_id, device_id)`'s queue, or `None` if that device's
    /// `/sync` has taken it since.
    async fn to_device_message(
        &self,
        user_id: &str,
        device_id: &str,
        stream_id: u64,
    ) -> Result<Option<ToDeviceMessage>, String>;

    /// The device-list stream's newest position (`0` if nothing ever changed).
    async fn device_lists_head(&self) -> Result<u64, String>;
    /// Users whose device list changed after `since` and up to `upto`.
    async fn device_lists_changed(&self, since: u64, upto: u64)
    -> Result<BTreeSet<String>, String>;
}

/// Synapse's interest rules for one appservice (the module docs).
pub struct Interest<'a> {
    namespaces: &'a Namespaces,
    bot_user_id: &'a str,
}

impl<'a> Interest<'a> {
    /// The rules for an appservice with `namespaces` whose bot is `bot_user_id`.
    #[must_use]
    pub fn new(namespaces: &'a Namespaces, bot_user_id: &'a str) -> Self {
        Self {
            namespaces,
            bot_user_id,
        }
    }

    /// Whether `user_id` is one of the appservice's users: its bot, or in its `users`
    /// namespaces (exclusive or not).
    #[must_use]
    pub fn user(&self, user_id: &str) -> bool {
        user_id == self.bot_user_id || self.namespaces.is_interested(NamespaceKind::Users, user_id)
    }

    /// Whether the appservice is interested in `room_id`, whose current members and aliases
    /// are `facts`.
    #[must_use]
    pub fn room(&self, room_id: &str, facts: &RoomFacts) -> bool {
        self.namespaces.is_interested(NamespaceKind::Rooms, room_id)
            || facts
                .aliases
                .iter()
                .any(|alias| self.namespaces.is_interested(NamespaceKind::Aliases, alias))
            || facts.joined_members.iter().any(|member| self.user(member))
    }
}

/// See the module docs.
pub struct EphemeralPump<B: KvBackend> {
    registry: Arc<Registry<B>>,
    source: Arc<dyn EphemeralSource>,
    devices: Arc<dyn DeviceSource>,
    /// Rooms whose typing changed since the last tick.
    typing_rooms: Mutex<BTreeSet<String>>,
    wake: Notify,
}

/// Typing, receipt or presence: which of the hub's changes [`EphemeralPump::note`] is told of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The typing set of a room changed.
    Typing {
        /// The room.
        room_id: String,
    },
    /// A receipt was written to the store.
    Receipt,
    /// A presence record was written to the store.
    Presence,
}

/// What one tick is building for one appservice.
#[derive(Default)]
struct Build {
    txn: Transaction,
    positions: Vec<(String, u64)>,
    /// Rooms named by the transaction's ephemeral events, for MSC3202's key counts.
    rooms: BTreeSet<String>,
}

/// Per-appservice positions in one stream for this tick: each listener's, `head` for one that
/// has never been sent the stream, and the lowest of them, which is where the scan starts.
struct Positions {
    per_listener: Vec<u64>,
    floor: u64,
}

impl<B: KvBackend> EphemeralPump<B> {
    /// Builds a pump reading from `source` and `devices`, queueing for the appservices in
    /// `registry`.
    #[must_use]
    pub fn new(
        registry: Arc<Registry<B>>,
        source: Arc<dyn EphemeralSource>,
        devices: Arc<dyn DeviceSource>,
    ) -> Self {
        Self {
            registry,
            source,
            devices,
            typing_rooms: Mutex::new(BTreeSet::new()),
            wake: Notify::new(),
        }
    }

    /// The doorbell: notes a typing change's room, and wakes whoever is waiting in
    /// [`EphemeralPump::wait`]. Returns at once; safe to call from a request handler.
    pub fn note(&self, change: &Change) {
        if let Change::Typing { room_id } = change {
            self.typing_rooms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(room_id.clone());
        }
        self.wake.notify_one();
    }

    /// Waits for the next [`EphemeralPump::note`]. A note made while nobody waited is kept
    /// (`Notify::notify_one`'s permit), so a change between two ticks is never slept through.
    pub async fn wait(&self) {
        self.wake.notified().await;
    }

    /// Forgets the typing changes noted so far: for a replica that is not the one pumping,
    /// whose notes are the same as the pumping replica's own.
    pub fn discard_typing(&self) {
        self.typing_rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn positions(
        &self,
        listeners: &[&Listener],
        stream: &str,
        head: u64,
    ) -> Result<Positions, AppserviceError> {
        let mut per_listener = Vec::with_capacity(listeners.len());
        for listener in listeners {
            per_listener.push(
                self.registry
                    .store()
                    .ephemeral_pos(&listener.row.id, stream)?
                    .unwrap_or(head),
            );
        }
        let floor = per_listener.iter().copied().min().unwrap_or(u64::MAX);
        Ok(Positions {
            per_listener,
            floor,
        })
    }

    /// Reads every stream from each appservice's position, and the typing rooms noted since the
    /// last tick, and queues one transaction per appservice that has something coming. Returns
    /// the appservices that now have something queued. Must not run concurrently with itself
    /// (one task calls it; see `hs-cli`).
    ///
    /// # Errors
    /// Returns [`AppserviceError::Store`] if the store or a source fails; the tick is then
    /// abandoned before anything is queued, and the next one reads the same positions again.
    pub async fn tick(&self) -> Result<Vec<String>, AppserviceError> {
        let all = listeners(&self.registry)?;
        let ephemeral: Vec<&Listener> = all
            .iter()
            .filter(|l| l.row.receive_ephemeral || l.row.push_ephemeral_legacy)
            .collect();
        let msc3202: Vec<&Listener> = all.iter().filter(|l| l.row.msc3202).collect();
        let typing_rooms = std::mem::take(
            &mut *self
                .typing_rooms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );

        let mut builds: BTreeMap<String, Build> = BTreeMap::new();
        let mut facts_cache: HashMap<String, Option<RoomFacts>> = HashMap::new();
        let source = self.source.as_ref();

        // Typing: each noted room's current set, once, to whoever is interested.
        if !ephemeral.is_empty() {
            for room_id in &typing_rooms {
                let Some(facts) = room_facts(source, &mut facts_cache, room_id).await? else {
                    continue;
                };
                let users = source
                    .typing_in(room_id)
                    .await
                    .map_err(AppserviceError::Store)?;
                for listener in &ephemeral {
                    if listener.interest().room(room_id, facts) {
                        let build = builds.entry(listener.row.id.clone()).or_default();
                        build.txn.ephemeral.push(typing_event(room_id, &users));
                        build.rooms.insert(room_id.clone());
                    }
                }
            }
        }

        // Receipts, presence and to-device messages: each stream from the lowest position,
        // page by page, every listener told of what is past its own position.
        let receipts_head = source
            .receipts_head()
            .await
            .map_err(AppserviceError::Store)?;
        let presence_head = source
            .presence_head()
            .await
            .map_err(AppserviceError::Store)?;
        let to_device_head = self
            .devices
            .to_device_head()
            .await
            .map_err(AppserviceError::Store)?;
        let mut receipt_positions = self.positions(&ephemeral, RECEIPTS, receipts_head)?;
        let mut presence_positions = self.positions(&ephemeral, PRESENCE, presence_head)?;
        let mut to_device_positions = self.positions(&ephemeral, TO_DEVICE, to_device_head)?;

        if !ephemeral.is_empty() {
            self.pump_receipts(
                &ephemeral,
                &mut receipt_positions,
                &mut builds,
                &mut facts_cache,
            )
            .await?;
            self.pump_presence(
                &ephemeral,
                &mut presence_positions,
                &mut builds,
                &mut facts_cache,
            )
            .await?;
            self.pump_to_device(&ephemeral, &mut to_device_positions, &mut builds)
                .await?;
        }
        for (index, listener) in ephemeral.iter().enumerate() {
            let build = builds.entry(listener.row.id.clone()).or_default();
            build
                .positions
                .push((RECEIPTS.to_owned(), receipt_positions.per_listener[index]));
            build
                .positions
                .push((PRESENCE.to_owned(), presence_positions.per_listener[index]));
            build.positions.push((
                TO_DEVICE.to_owned(),
                to_device_positions.per_listener[index],
            ));
        }

        // Device lists: one read per MSC3202 listener, from its own position to the head.
        let device_lists_head = self
            .devices
            .device_lists_head()
            .await
            .map_err(AppserviceError::Store)?;
        for listener in &msc3202 {
            let pos = self
                .registry
                .store()
                .ephemeral_pos(&listener.row.id, DEVICE_LISTS)?
                .unwrap_or(device_lists_head);
            let build = builds.entry(listener.row.id.clone()).or_default();
            if device_lists_head > pos {
                let changed = self
                    .devices
                    .device_lists_changed(pos, device_lists_head)
                    .await
                    .map_err(AppserviceError::Store)?;
                for user_id in changed {
                    if self
                        .interested_in_user_or_their_rooms(listener, &user_id, &mut facts_cache)
                        .await?
                    {
                        build.txn.device_lists.changed.push(user_id);
                    }
                }
            }
            build
                .positions
                .push((DEVICE_LISTS.to_owned(), device_lists_head));
        }

        // MSC3202 key counts, for every transaction with something in it.
        for listener in &msc3202 {
            let Some(build) = builds.get_mut(&listener.row.id) else {
                continue;
            };
            if build.txn.is_empty() {
                continue;
            }
            let mut users: BTreeSet<String> = BTreeSet::from([listener.bot_user_id.clone()]);
            for room_id in &build.rooms {
                if let Some(facts) = room_facts(source, &mut facts_cache, room_id).await? {
                    users.extend(
                        facts
                            .joined_members
                            .iter()
                            .filter(|m| listener.interest().user(m))
                            .cloned(),
                    );
                }
            }
            users.extend(build.txn.to_device.iter().map(|m| m.to_user_id.clone()));
            let (counts, fallback) = key_counts_for(self.devices.as_ref(), &users).await?;
            build.txn.one_time_keys_count = counts;
            build.txn.unused_fallback_key_types = fallback;
        }

        // One store transaction for every body and every position.
        let mut deliveries = Vec::with_capacity(builds.len());
        let mut touched = Vec::new();
        for listener in &all {
            let Some(build) = builds.remove(&listener.row.id) else {
                continue;
            };
            let body = wire_body(&listener.row, &build.txn);
            if body.is_some() {
                touched.push(listener.row.id.clone());
            }
            deliveries.push(EphemeralDelivery {
                appservice_id: listener.row.id.clone(),
                body,
                positions: build.positions,
            });
        }
        if !deliveries.is_empty() {
            self.registry
                .store()
                .enqueue_ephemeral(&deliveries, self.registry.now_ms())?;
        }

        // Nobody needs what every listener has been sent (a position is the last entry dealt
        // with, so everything at or below the lowest one). With nobody listening, nothing is
        // kept.
        source
            .prune_receipts_below(receipt_positions.floor.saturating_add(1))
            .await
            .map_err(AppserviceError::Store)?;
        source
            .prune_presence_below(presence_positions.floor.saturating_add(1))
            .await
            .map_err(AppserviceError::Store)?;
        self.devices
            .prune_to_device_below(to_device_positions.floor.saturating_add(1))
            .await
            .map_err(AppserviceError::Store)?;
        Ok(touched)
    }

    async fn pump_receipts(
        &self,
        listeners: &[&Listener],
        positions: &mut Positions,
        builds: &mut BTreeMap<String, Build>,
        facts_cache: &mut HashMap<String, Option<RoomFacts>>,
    ) -> Result<(), AppserviceError> {
        let source = self.source.as_ref();
        let mut from = positions.floor;
        loop {
            let page = source
                .receipts_since(from, EPHEMERAL_PAGE)
                .await
                .map_err(AppserviceError::Store)?;
            let Some(last) = page.last() else {
                break;
            };
            let last_pos = last.pos;
            // Per room, in the order the rooms first appear.
            let mut by_room: BTreeMap<&str, Vec<&ReceiptChange>> = BTreeMap::new();
            for change in &page {
                by_room.entry(&change.room_id).or_default().push(change);
            }
            for (index, listener) in listeners.iter().enumerate() {
                let mine = positions.per_listener[index];
                if mine >= last_pos {
                    continue;
                }
                let interest = listener.interest();
                for (room_id, changes) in &by_room {
                    let Some(facts) = room_facts(source, facts_cache, room_id).await? else {
                        continue;
                    };
                    if !interest.room(room_id, facts) {
                        continue;
                    }
                    let rows: Vec<(&str, &str, &str, u64)> = changes
                        .iter()
                        .filter(|c| c.pos > mine)
                        // A private receipt is its sender's alone.
                        .filter(|c| c.kind != "m.read.private" || interest.user(&c.user_id))
                        .map(|c| {
                            (
                                c.user_id.as_str(),
                                c.kind.as_str(),
                                c.event_id.as_str(),
                                c.ts,
                            )
                        })
                        .collect();
                    if rows.is_empty() {
                        continue;
                    }
                    let build = builds.entry(listener.row.id.clone()).or_default();
                    build
                        .txn
                        .ephemeral
                        .push(receipt_event(room_id, receipt_content(rows)));
                    build.rooms.insert((*room_id).to_owned());
                }
                positions.per_listener[index] = last_pos;
            }
            from = last_pos;
            if page.len() < EPHEMERAL_PAGE {
                break;
            }
        }
        Ok(())
    }

    async fn pump_presence(
        &self,
        listeners: &[&Listener],
        positions: &mut Positions,
        builds: &mut BTreeMap<String, Build>,
        facts_cache: &mut HashMap<String, Option<RoomFacts>>,
    ) -> Result<(), AppserviceError> {
        let source = self.source.as_ref();
        let mut from = positions.floor;
        loop {
            let page = source
                .presence_since(from, EPHEMERAL_PAGE)
                .await
                .map_err(AppserviceError::Store)?;
            let Some(last) = page.last() else {
                break;
            };
            let last_pos = last.pos;
            // A user who changed twice in the page is sent once, with their current record.
            let mut users: Vec<(u64, &str)> = Vec::new();
            for change in &page {
                match users.iter_mut().find(|(_, u)| *u == change.user_id) {
                    Some(entry) => entry.0 = change.pos,
                    None => users.push((change.pos, &change.user_id)),
                }
            }
            let mut contents: HashMap<&str, Option<Value>> = HashMap::new();
            for (index, listener) in listeners.iter().enumerate() {
                let mine = positions.per_listener[index];
                if mine >= last_pos {
                    continue;
                }
                for (pos, user_id) in &users {
                    if *pos <= mine
                        || !self
                            .interested_in_user_or_their_rooms(listener, user_id, facts_cache)
                            .await?
                    {
                        continue;
                    }
                    if !contents.contains_key(user_id) {
                        let content = source
                            .presence_content(user_id)
                            .await
                            .map_err(AppserviceError::Store)?;
                        contents.insert(user_id, content);
                    }
                    if let Some(Some(content)) = contents.get(user_id) {
                        builds
                            .entry(listener.row.id.clone())
                            .or_default()
                            .txn
                            .ephemeral
                            .push(presence_event(user_id, content.clone()));
                    }
                }
                positions.per_listener[index] = last_pos;
            }
            from = last_pos;
            if page.len() < EPHEMERAL_PAGE {
                break;
            }
        }
        Ok(())
    }

    async fn pump_to_device(
        &self,
        listeners: &[&Listener],
        positions: &mut Positions,
        builds: &mut BTreeMap<String, Build>,
    ) -> Result<(), AppserviceError> {
        let mut from = positions.floor;
        loop {
            let page = self
                .devices
                .to_device_since(from, EPHEMERAL_PAGE)
                .await
                .map_err(AppserviceError::Store)?;
            let Some(last) = page.last() else {
                break;
            };
            let last_pos = last.pos;
            let mut messages: HashMap<u64, Option<ToDeviceMessage>> = HashMap::new();
            for (index, listener) in listeners.iter().enumerate() {
                let mine = positions.per_listener[index];
                if mine >= last_pos {
                    continue;
                }
                let interest = listener.interest();
                for change in page.iter().filter(|c| c.pos > mine) {
                    if !interest.user(&change.user_id) {
                        continue;
                    }
                    if let std::collections::hash_map::Entry::Vacant(slot) =
                        messages.entry(change.pos)
                    {
                        let message = self
                            .devices
                            .to_device_message(&change.user_id, &change.device_id, change.stream_id)
                            .await
                            .map_err(AppserviceError::Store)?;
                        slot.insert(message);
                    }
                    if let Some(Some(message)) = messages.get(&change.pos) {
                        builds
                            .entry(listener.row.id.clone())
                            .or_default()
                            .txn
                            .to_device
                            .push(ToDeviceEntry {
                                to_user_id: change.user_id.clone(),
                                to_device_id: change.device_id.clone(),
                                event: serde_json::json!({
                                    "type": message.event_type,
                                    "sender": message.sender,
                                    "content": message.content,
                                }),
                            });
                    }
                }
                positions.per_listener[index] = last_pos;
            }
            from = last_pos;
            if page.len() < EPHEMERAL_PAGE {
                break;
            }
        }
        Ok(())
    }

    /// Synapse's `is_interested_in_presence` (and its device-list twin): one of the
    /// appservice's users, or in a room it is interested in.
    async fn interested_in_user_or_their_rooms(
        &self,
        listener: &Listener,
        user_id: &str,
        facts_cache: &mut HashMap<String, Option<RoomFacts>>,
    ) -> Result<bool, AppserviceError> {
        let interest = listener.interest();
        if interest.user(user_id) {
            return Ok(true);
        }
        for room_id in self
            .source
            .joined_rooms_of(user_id)
            .await
            .map_err(AppserviceError::Store)?
        {
            if let Some(facts) = room_facts(self.source.as_ref(), facts_cache, &room_id).await?
                && interest.room(&room_id, facts)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// `room_id`'s facts, read once per tick.
async fn room_facts<'c>(
    source: &dyn EphemeralSource,
    cache: &'c mut HashMap<String, Option<RoomFacts>>,
    room_id: &str,
) -> Result<Option<&'c RoomFacts>, AppserviceError> {
    if !cache.contains_key(room_id) {
        let facts = source
            .room_facts(room_id)
            .await
            .map_err(AppserviceError::Store)?;
        cache.insert(room_id.to_owned(), facts);
    }
    Ok(cache.get(room_id).and_then(Option::as_ref))
}

/// MSC3202's `device_one_time_keys_count` and `device_unused_fallback_key_types` for `users`:
/// every device of theirs with uploaded keys, from `source`. Used by this pump and by
/// [`crate::pump`] alike.
///
/// # Errors
/// Returns [`AppserviceError::Store`] if the source fails.
pub async fn key_counts_for(
    source: &dyn KeyCountSource,
    users: &BTreeSet<String>,
) -> Result<(OneTimeKeysCount, UnusedFallbackKeyTypes), AppserviceError> {
    let mut counts = OneTimeKeysCount::new();
    let mut fallback = UnusedFallbackKeyTypes::new();
    for user_id in users {
        for device in source
            .key_counts(user_id)
            .await
            .map_err(AppserviceError::Store)?
        {
            counts
                .entry(user_id.clone())
                .or_default()
                .insert(device.device_id.clone(), device.one_time_keys);
            fallback
                .entry(user_id.clone())
                .or_default()
                .insert(device.device_id, device.unused_fallback_key_types);
        }
    }
    Ok((counts, fallback))
}

/// A [`DeviceListsUpdate`] is what MSC3202 calls the field; re-exported here so a caller
/// building one next to this module's types finds it.
pub type DeviceLists = DeviceListsUpdate;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::Registration;
    use hs_kv::memory::MemoryBackend;
    use serde_json::json;

    /// Everything the pump reads, in memory, with the streams as the real stores keep them.
    #[derive(Default)]
    struct Fake {
        typing: Mutex<HashMap<String, Vec<String>>>,
        receipts: Mutex<Vec<ReceiptChange>>,
        presence: Mutex<Vec<PresenceChange>>,
        presence_content: Mutex<HashMap<String, Value>>,
        rooms: Mutex<HashMap<String, RoomFacts>>,
        to_device: Mutex<Vec<ToDeviceChange>>,
        messages: Mutex<HashMap<(String, String, u64), ToDeviceMessage>>,
        device_lists: Mutex<Vec<(u64, String)>>,
        keys: Mutex<HashMap<String, Vec<DeviceKeyCounts>>>,
        heads: Mutex<HashMap<&'static str, u64>>,
    }

    impl Fake {
        fn next(&self, stream: &'static str) -> u64 {
            let mut heads = self.heads.lock().unwrap();
            let head = heads.entry(stream).or_insert(0);
            *head += 1;
            *head
        }

        fn head(&self, stream: &'static str) -> u64 {
            self.heads.lock().unwrap().get(stream).copied().unwrap_or(0)
        }

        fn room(&self, room_id: &str, members: &[&str]) {
            self.rooms.lock().unwrap().insert(
                room_id.to_owned(),
                RoomFacts {
                    joined_members: members.iter().map(|m| (*m).to_owned()).collect(),
                    aliases: vec![],
                },
            );
        }

        fn typing(&self, room_id: &str, users: &[&str]) {
            self.typing.lock().unwrap().insert(
                room_id.to_owned(),
                users.iter().map(|u| (*u).to_owned()).collect(),
            );
        }

        fn receipt(&self, room_id: &str, user_id: &str, kind: &str, event_id: &str) -> u64 {
            let pos = self.next("receipts");
            self.receipts.lock().unwrap().push(ReceiptChange {
                pos,
                room_id: room_id.to_owned(),
                user_id: user_id.to_owned(),
                kind: kind.to_owned(),
                event_id: event_id.to_owned(),
                ts: 1000 + pos,
            });
            pos
        }

        fn presence(&self, user_id: &str, presence: &str) -> u64 {
            let pos = self.next("presence");
            self.presence.lock().unwrap().push(PresenceChange {
                pos,
                user_id: user_id.to_owned(),
            });
            self.presence_content.lock().unwrap().insert(
                user_id.to_owned(),
                json!({"presence": presence, "last_active_ago": 0, "currently_active": presence == "online"}),
            );
            pos
        }

        fn send_to_device(&self, user_id: &str, device_id: &str, event_type: &str) -> u64 {
            let pos = self.next("to_device");
            let stream_id = pos;
            self.to_device.lock().unwrap().push(ToDeviceChange {
                pos,
                user_id: user_id.to_owned(),
                device_id: device_id.to_owned(),
                stream_id,
            });
            self.messages.lock().unwrap().insert(
                (user_id.to_owned(), device_id.to_owned(), stream_id),
                ToDeviceMessage {
                    sender: "@alice:example.org".to_owned(),
                    event_type: event_type.to_owned(),
                    content: json!({"n": pos}),
                },
            );
            pos
        }

        fn device_changed(&self, user_id: &str) -> u64 {
            let pos = self.next("device_lists");
            self.device_lists
                .lock()
                .unwrap()
                .push((pos, user_id.to_owned()));
            pos
        }

        fn keys(&self, user_id: &str, device_id: &str, otks: u64) {
            self.keys
                .lock()
                .unwrap()
                .entry(user_id.to_owned())
                .or_default()
                .push(DeviceKeyCounts {
                    device_id: device_id.to_owned(),
                    one_time_keys: BTreeMap::from([("signed_curve25519".to_owned(), otks)]),
                    unused_fallback_key_types: vec!["signed_curve25519".to_owned()],
                });
        }
    }

    fn page<T: Clone>(items: &[T], pos: impl Fn(&T) -> u64, since: u64, limit: usize) -> Vec<T> {
        items
            .iter()
            .filter(|i| pos(i) > since)
            .take(limit)
            .cloned()
            .collect()
    }

    #[async_trait]
    impl EphemeralSource for Fake {
        async fn typing_in(&self, room_id: &str) -> Result<Vec<String>, String> {
            Ok(self
                .typing
                .lock()
                .unwrap()
                .get(room_id)
                .cloned()
                .unwrap_or_default())
        }
        async fn receipts_since(
            &self,
            since: u64,
            limit: usize,
        ) -> Result<Vec<ReceiptChange>, String> {
            Ok(page(
                &self.receipts.lock().unwrap(),
                |r| r.pos,
                since,
                limit,
            ))
        }
        async fn receipts_head(&self) -> Result<u64, String> {
            Ok(self.head("receipts"))
        }
        async fn prune_receipts_below(&self, below: u64) -> Result<(), String> {
            self.receipts.lock().unwrap().retain(|r| r.pos >= below);
            Ok(())
        }
        async fn presence_since(
            &self,
            since: u64,
            limit: usize,
        ) -> Result<Vec<PresenceChange>, String> {
            Ok(page(
                &self.presence.lock().unwrap(),
                |p| p.pos,
                since,
                limit,
            ))
        }
        async fn presence_head(&self) -> Result<u64, String> {
            Ok(self.head("presence"))
        }
        async fn prune_presence_below(&self, below: u64) -> Result<(), String> {
            self.presence.lock().unwrap().retain(|p| p.pos >= below);
            Ok(())
        }
        async fn presence_content(&self, user_id: &str) -> Result<Option<Value>, String> {
            Ok(self.presence_content.lock().unwrap().get(user_id).cloned())
        }
        async fn room_facts(&self, room_id: &str) -> Result<Option<RoomFacts>, String> {
            Ok(self.rooms.lock().unwrap().get(room_id).cloned())
        }
        async fn joined_rooms_of(&self, user_id: &str) -> Result<Vec<String>, String> {
            Ok(self
                .rooms
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, facts)| facts.joined_members.contains(user_id))
                .map(|(id, _)| id.clone())
                .collect())
        }
    }

    #[async_trait]
    impl KeyCountSource for Fake {
        async fn key_counts(&self, user_id: &str) -> Result<Vec<DeviceKeyCounts>, String> {
            Ok(self
                .keys
                .lock()
                .unwrap()
                .get(user_id)
                .cloned()
                .unwrap_or_default())
        }
    }

    #[async_trait]
    impl DeviceSource for Fake {
        async fn to_device_since(
            &self,
            since: u64,
            limit: usize,
        ) -> Result<Vec<ToDeviceChange>, String> {
            Ok(page(
                &self.to_device.lock().unwrap(),
                |t| t.pos,
                since,
                limit,
            ))
        }
        async fn to_device_head(&self) -> Result<u64, String> {
            Ok(self.head("to_device"))
        }
        async fn prune_to_device_below(&self, below: u64) -> Result<(), String> {
            self.to_device.lock().unwrap().retain(|t| t.pos >= below);
            Ok(())
        }
        async fn to_device_message(
            &self,
            user_id: &str,
            device_id: &str,
            stream_id: u64,
        ) -> Result<Option<ToDeviceMessage>, String> {
            Ok(self
                .messages
                .lock()
                .unwrap()
                .get(&(user_id.to_owned(), device_id.to_owned(), stream_id))
                .cloned())
        }
        async fn device_lists_head(&self) -> Result<u64, String> {
            Ok(self.head("device_lists"))
        }
        async fn device_lists_changed(
            &self,
            since: u64,
            upto: u64,
        ) -> Result<BTreeSet<String>, String> {
            Ok(self
                .device_lists
                .lock()
                .unwrap()
                .iter()
                .filter(|(pos, _)| *pos > since && *pos <= upto)
                .map(|(_, u)| u.clone())
                .collect())
        }
    }

    const BRIDGE: &str = "id: irc\nurl: 'http://localhost:1'\nas_token: as_secret\n\
        hs_token: hs_secret\nsender_localpart: ircbot\nreceive_ephemeral: true\n\
        org.matrix.msc3202: true\nnamespaces:\n  users:\n    \
        - regex: '@irc_.*:example\\.org'\n      exclusive: true\n";

    /// Wants events only.
    const DEAF: &str = "id: deaf\nurl: 'http://localhost:2'\nas_token: deaf_as\n\
        hs_token: deaf_hs\nsender_localpart: deafbot\nnamespaces:\n  users:\n    \
        - regex: '@deaf_.*:example\\.org'\n      exclusive: true\n";

    fn setup(
        registrations: &[&str],
    ) -> (
        Arc<Registry<MemoryBackend>>,
        Arc<Fake>,
        EphemeralPump<MemoryBackend>,
    ) {
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), ruma::server_name!("example.org")).unwrap(),
        );
        for yaml in registrations {
            registry
                .add(&Registration::parse_yaml(yaml).unwrap())
                .unwrap();
        }
        let fake = Arc::new(Fake::default());
        let pump = EphemeralPump::new(registry.clone(), fake.clone(), fake.clone());
        (registry, fake, pump)
    }

    /// Every queued body for `id`, in queue order.
    fn queued(registry: &Registry<MemoryBackend>, id: &str) -> Vec<Value> {
        registry
            .store()
            .queue_for(id)
            .unwrap()
            .into_iter()
            .map(|entry| entry.body)
            .collect()
    }

    fn ephemeral_types(body: &Value) -> Vec<String> {
        body["ephemeral"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn typing_receipts_and_presence_reach_an_interested_appservice_and_nobody_else() {
        let (registry, fake, pump) = setup(&[BRIDGE, DEAF]);
        fake.room(
            "!bridged:example.org",
            &["@alice:example.org", "@irc_bob:example.org"],
        );
        fake.room("!private:example.org", &["@alice:example.org"]);
        // The first tick sets every position to the streams' heads: nothing from before.
        fake.receipt(
            "!bridged:example.org",
            "@alice:example.org",
            "m.read",
            "$old",
        );
        assert!(pump.tick().await.unwrap().is_empty());

        fake.typing("!bridged:example.org", &["@alice:example.org"]);
        pump.note(&Change::Typing {
            room_id: "!bridged:example.org".to_owned(),
        });
        fake.typing("!private:example.org", &["@alice:example.org"]);
        pump.note(&Change::Typing {
            room_id: "!private:example.org".to_owned(),
        });
        fake.receipt(
            "!bridged:example.org",
            "@alice:example.org",
            "m.read",
            "$one",
        );
        fake.receipt(
            "!bridged:example.org",
            "@alice:example.org",
            "m.read.private",
            "$one",
        );
        fake.receipt(
            "!bridged:example.org",
            "@irc_bob:example.org",
            "m.read.private",
            "$one",
        );
        fake.receipt(
            "!private:example.org",
            "@alice:example.org",
            "m.read",
            "$two",
        );
        fake.presence("@alice:example.org", "online");
        fake.presence("@carol:example.org", "online");

        assert_eq!(pump.tick().await.unwrap(), vec!["irc".to_owned()]);
        let bodies = queued(&registry, "irc");
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        assert_eq!(
            ephemeral_types(body),
            vec!["m.typing", "m.receipt", "m.presence"]
        );
        let ephemeral = body["ephemeral"].as_array().unwrap();
        assert_eq!(
            ephemeral[0],
            json!({"type": "m.typing", "room_id": "!bridged:example.org", "content": {"user_ids": ["@alice:example.org"]}})
        );
        // Alice's public receipt and the ghost's private one; alice's private one is hers alone.
        assert_eq!(
            ephemeral[1],
            json!({
                "type": "m.receipt",
                "room_id": "!bridged:example.org",
                "content": {"$one": {
                    "m.read": {"@alice:example.org": {"ts": 1002}},
                    "m.read.private": {"@irc_bob:example.org": {"ts": 1004}},
                }},
            })
        );
        assert_eq!(ephemeral[2]["sender"], "@alice:example.org");
        assert_eq!(ephemeral[2]["content"]["presence"], "online");
        // The transaction was for an MSC3202 appservice, so it carries key counts for its
        // users in the rooms it names: none have keys, so the fields are absent.
        assert!(body.get("device_one_time_keys_count").is_none());
        assert!(queued(&registry, "deaf").is_empty());

        // Positions moved: the same again sends nothing.
        assert!(pump.tick().await.unwrap().is_empty());
        assert_eq!(queued(&registry, "irc").len(), 1);
        assert_eq!(
            registry.store().ephemeral_pos("irc", RECEIPTS).unwrap(),
            Some(5)
        );
        // ...and the streams were pruned below them.
        assert!(fake.receipts_since(0, 10).await.unwrap().is_empty());
        assert!(fake.presence_since(0, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn to_device_messages_for_its_users_and_device_lists_with_key_counts() {
        let (registry, fake, pump) = setup(&[BRIDGE]);
        fake.room(
            "!bridged:example.org",
            &["@alice:example.org", "@irc_bob:example.org"],
        );
        assert!(pump.tick().await.unwrap().is_empty());

        fake.send_to_device("@ircbot:example.org", "BOTDEV", "m.room_key");
        fake.send_to_device("@alice:example.org", "ALICEDEV", "m.room_key");
        fake.send_to_device("@irc_bob:example.org", "GHOSTDEV", "m.room.encrypted");
        fake.keys("@ircbot:example.org", "BOTDEV", 7);
        fake.device_changed("@alice:example.org");
        fake.device_changed("@stranger:example.org");

        assert_eq!(pump.tick().await.unwrap(), vec!["irc".to_owned()]);
        let body = &queued(&registry, "irc")[0];
        let to_device = body["to_device"].as_array().unwrap();
        assert_eq!(to_device.len(), 2);
        assert_eq!(
            to_device[0],
            json!({
                "type": "m.room_key", "sender": "@alice:example.org", "content": {"n": 1},
                "to_user_id": "@ircbot:example.org", "to_device_id": "BOTDEV",
            })
        );
        assert_eq!(to_device[1]["to_user_id"], "@irc_bob:example.org");
        assert_eq!(to_device[1]["type"], "m.room.encrypted");
        assert_eq!(body["de.sorunome.msc2409.to_device"], body["to_device"]);
        // Alice shares a room with a ghost; the stranger shares nothing.
        assert_eq!(
            body["device_lists"],
            json!({"changed": ["@alice:example.org"], "left": []})
        );
        assert_eq!(
            body["device_one_time_keys_count"],
            json!({"@ircbot:example.org": {"BOTDEV": {"signed_curve25519": 7}}})
        );
        assert_eq!(
            body["org.matrix.msc3202.device_unused_fallback_key_types"],
            json!({"@ircbot:example.org": {"BOTDEV": ["signed_curve25519"]}})
        );

        assert!(pump.tick().await.unwrap().is_empty());
        assert_eq!(queued(&registry, "irc").len(), 1);
        assert_eq!(
            registry.store().ephemeral_pos("irc", DEVICE_LISTS).unwrap(),
            Some(2)
        );
        assert!(fake.to_device_since(0, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_restarted_pump_continues_from_the_stored_positions() {
        let (registry, fake, pump) = setup(&[BRIDGE]);
        fake.room("!bridged:example.org", &["@irc_bob:example.org"]);
        assert!(pump.tick().await.unwrap().is_empty());
        fake.receipt(
            "!bridged:example.org",
            "@alice:example.org",
            "m.read",
            "$one",
        );
        pump.tick().await.unwrap();
        assert_eq!(queued(&registry, "irc").len(), 1);

        // Things happen while nothing pumps; a new pump over the same store finds them once.
        fake.receipt(
            "!bridged:example.org",
            "@alice:example.org",
            "m.read",
            "$two",
        );
        fake.send_to_device("@ircbot:example.org", "BOTDEV", "m.room_key");
        let restarted = EphemeralPump::new(registry.clone(), fake.clone(), fake.clone());
        assert_eq!(restarted.tick().await.unwrap(), vec!["irc".to_owned()]);
        let bodies = queued(&registry, "irc");
        assert_eq!(bodies.len(), 2);
        assert_eq!(
            bodies[1]["ephemeral"][0]["content"]["$two"]["m.read"]["@alice:example.org"]["ts"],
            1002
        );
        assert_eq!(bodies[1]["to_device"].as_array().unwrap().len(), 1);
        assert!(restarted.tick().await.unwrap().is_empty());
        assert_eq!(queued(&registry, "irc").len(), 2);
    }

    #[tokio::test]
    async fn a_stream_further_behind_than_a_page_is_read_whole_and_in_order() {
        let (registry, fake, pump) = setup(&[BRIDGE]);
        fake.room("!bridged:example.org", &["@irc_bob:example.org"]);
        assert!(pump.tick().await.unwrap().is_empty());
        for n in 0..(EPHEMERAL_PAGE * 2 + 5) {
            fake.send_to_device("@ircbot:example.org", "BOTDEV", &format!("t{n}"));
        }
        pump.tick().await.unwrap();
        let bodies = queued(&registry, "irc");
        assert_eq!(bodies.len(), 1);
        let types: Vec<&str> = bodies[0]["to_device"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["type"].as_str().unwrap())
            .collect();
        assert_eq!(types.len(), EPHEMERAL_PAGE * 2 + 5);
        assert_eq!(types[0], "t0");
        assert_eq!(
            types[types.len() - 1],
            format!("t{}", EPHEMERAL_PAGE * 2 + 4)
        );
    }

    #[tokio::test]
    async fn with_nobody_listening_the_streams_are_kept_empty() {
        let (_, fake, pump) = setup(&[DEAF]);
        fake.receipt("!r:example.org", "@alice:example.org", "m.read", "$one");
        fake.presence("@alice:example.org", "online");
        fake.send_to_device("@alice:example.org", "DEV", "m.room_key");
        assert!(pump.tick().await.unwrap().is_empty());
        assert!(fake.receipts_since(0, 10).await.unwrap().is_empty());
        assert!(fake.presence_since(0, 10).await.unwrap().is_empty());
        assert!(fake.to_device_since(0, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn interest_follows_synapses_rules() {
        let (_, _, pump) = setup(&[BRIDGE]);
        let listener = listeners(&pump.registry).unwrap().remove(0);
        let interest = listener.interest();
        assert!(interest.user("@ircbot:example.org"));
        assert!(interest.user("@irc_bob:example.org"));
        assert!(!interest.user("@alice:example.org"));
        let nobody = RoomFacts::default();
        assert!(!interest.room("!r:example.org", &nobody));
        assert!(interest.room(
            "!r:example.org",
            &RoomFacts {
                joined_members: BTreeSet::from(["@ircbot:example.org".to_owned()]),
                aliases: vec![],
            }
        ));
        let mut yaml = BRIDGE.to_owned();
        yaml.push_str("  aliases:\n    - regex: '#irc_.*:example\\.org'\n      exclusive: true\n");
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), ruma::server_name!("example.org")).unwrap(),
        );
        registry
            .add(&Registration::parse_yaml(&yaml).unwrap())
            .unwrap();
        let listener = listeners(&registry).unwrap().remove(0);
        assert!(listener.interest().room(
            "!r:example.org",
            &RoomFacts {
                joined_members: BTreeSet::new(),
                aliases: vec!["#irc_rust:example.org".to_owned()],
            }
        ));
    }
}
