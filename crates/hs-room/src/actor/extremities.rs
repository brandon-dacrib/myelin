//! Keeping a room's forward extremities few: what a new local event cites, which stuck
//! extremities are dropped, and the dummy event that merges the rest.
//!
//! A room's current state is the state resolution of the states at its forward extremities
//! ([`RoomActor::current_view`]). One extremity is a read; more is a resolution, and on the
//! demo (2026-10-16) federated rooms full of forks that nothing merged spent 15-22 s on one,
//! with the room's lock held. Synapse keeps the set small three ways, and so does this module:
//!
//! - **A capped `prev_events`** ([`RoomActor::prev_events_for_new_event`]). Every event this
//!   server creates cites the room's extremities, which merges them, but at most
//!   [`MAX_PREV_EVENTS`], the deepest first (Synapse's `get_prev_events_for_room`, `ORDER BY
//!   depth DESC LIMIT 10`). Synapse refuses a received event with more than 20
//!   (`_check_event_sanity`: "Too many prev_events"); before 2026-10-16 this server cited every
//!   extremity, so one of its events in a room of 21 forks would have been refused there.
//! - **Pruning** ([`RoomActor::maintain_extremities`], Synapse's `_prune_extremities`). After an
//!   operation that persisted events, when the room's resolved state is exactly the state at
//!   some of its extremities, the others change nothing a read of the current state answers.
//!   They are dropped -- all of them or none -- when none was just persisted, none was sent by
//!   this server (or is a dummy event of this server's standing only on other servers' events),
//!   and each is either over a day old and more than 100 events behind the newest just
//!   persisted, or from a server whose event was just persisted and more than 20 behind it.
//!   The rest are "legitimate" forks a new event should still cite.
//! - **Dummy events** ([`RoomActor::send_dummy_event`], Synapse's
//!   `_send_dummy_events_to_fill_extremities`). A room with more than
//!   `server.rooms.dummy_events_threshold` extremities that none of this server's users writes
//!   to is merged by an `org.matrix.dummy_event` one of its joined local users is allowed to
//!   send; `crate::registry::RoomRegistry::spawn_extremity_maintainer` decides which rooms and
//!   how often. Nobody sees a dummy event: [`RoomActor::event_visible_to`] hides it from every
//!   client read, as Synapse's `filter_events_for_client` does.

use std::collections::HashSet;

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_model::Event;
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use ruma::{OwnedServerName, OwnedUserId};

use super::{RoomActor, to_kv};
use crate::error::RoomError;
use crate::pipeline;

/// How many forward extremities an event this server creates cites at most, the deepest first:
/// Synapse's figure (`get_prev_events_for_room`). Synapse refuses a received event citing more
/// than 20.
pub const MAX_PREV_EVENTS: usize = 10;

/// The event type of a dummy event: carries nothing, exists to cite extremities.
pub const DUMMY_EVENT_TYPE: &str = "org.matrix.dummy_event";

/// An extremity older than this (by its `origin_server_ts`) may be pruned when it is also more
/// than [`PRUNE_OLD_DEPTH`] events behind the newest event just persisted (Synapse's one day).
pub const PRUNE_OLD_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// See [`PRUNE_OLD_AGE_MS`] (Synapse's 100).
pub const PRUNE_OLD_DEPTH: i64 = 100;

/// An extremity from a server whose event was just persisted may be pruned when it is more
/// than this many events behind it (Synapse's 20): that server has evidently moved on.
pub const PRUNE_SAME_SERVER_DEPTH: i64 = 20;

/// How many local dummy events the pruning check walks back through before it gives up and
/// keeps the extremities: a chain of this server's dummy events that long is not worth the
/// reads.
const LOCAL_DUMMY_WALK: usize = 64;

/// What the events persisted since the last [`RoomActor::maintain_extremities`] say about
/// which extremities pruning may drop (Synapse's "events being persisted").
#[derive(Debug, Default)]
pub(crate) struct PersistedSinceMaintenance {
    /// The events themselves: never pruned in the same pass.
    events: HashSet<EventSn>,
    /// The servers that sent them.
    servers: HashSet<OwnedServerName>,
    /// The greatest depth among them.
    max_depth: Option<i64>,
}

impl PersistedSinceMaintenance {
    /// Notes one persisted event.
    pub(crate) fn note(&mut self, sn: EventSn, event: &Event) {
        self.events.insert(sn);
        self.servers
            .insert(event.header().sender.server_name().to_owned());
        let depth = event.header().depth;
        self.max_depth = Some(self.max_depth.map_or(depth, |d| d.max(depth)));
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// What one [`RoomActor::send_dummy_event`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DummyOutcome {
    /// The room has at most one extremity: nothing to merge.
    NotForked,
    /// Sent, as this user, citing this many extremities.
    Sent {
        /// The local member it was sent as.
        sender: OwnedUserId,
        /// The event's ID.
        event_id: ruma::OwnedEventId,
        /// How many extremities it cited.
        cited: usize,
    },
    /// No joined local user may send one (none joined, none with the power, or the room is
    /// blocked).
    NoLocalSender,
}

impl<B: KvBackend> RoomActor<B> {
    /// How many forward extremities the room has.
    #[must_use]
    pub fn forward_extremity_count(&self) -> usize {
        self.forward_extremities.len()
    }

    /// The forward extremities a new event this server creates cites: all of them up to
    /// [`MAX_PREV_EVENTS`], and past that the deepest (then latest in this server's timeline)
    /// [`MAX_PREV_EVENTS`]. In ascending short-id order, as [`RoomActor::forward_extremities_vec`].
    pub(crate) fn prev_events_for_new_event(&self) -> Vec<EventSn> {
        if self.forward_extremities.len() <= MAX_PREV_EVENTS {
            return self.forward_extremities_vec();
        }
        let mut ranked: Vec<(i64, i64, EventSn)> = self
            .forward_extremities
            .iter()
            .map(|sn| {
                let depth = self
                    .event_uncached(*sn)
                    .map_or(i64::MIN, |e| e.header().depth);
                let pos = self.room_pos_of(*sn).unwrap_or(i64::MIN);
                (depth, pos, *sn)
            })
            .collect();
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        let mut chosen: Vec<EventSn> = ranked
            .into_iter()
            .take(MAX_PREV_EVENTS)
            .map(|(_, _, sn)| sn)
            .collect();
        chosen.sort_unstable();
        chosen
    }

    /// Notes an event [`RoomActor::persist_with`] just wrote, for the next
    /// [`RoomActor::maintain_extremities`]. Soft-failed events and the importer's quiet copies
    /// are not noted: neither changes the extremities, and the importer's are Synapse's history.
    pub(super) fn note_persisted_for_maintenance(&mut self, sn: EventSn, event: &Event) {
        if self.quiet || event.header().flags.is_soft_failed() {
            return;
        }
        self.persisted_since_maintenance.note(sn, event);
    }

    /// What `RoomActorHandle` runs under the room's lock after every operation: the pruning
    /// pass ([`RoomActor::maintain_extremities`]) when the operation persisted events. A
    /// failure is logged and changes nothing: the extremities stay as they were, and the next
    /// operation that persists tries again.
    pub fn after_operation(&mut self) {
        if self.persisted_since_maintenance.is_empty() {
            return;
        }
        if let Err(error) = self.maintain_extremities(crate::metrics::now_ms()) {
            tracing::warn!(
                room_id = %self.room_id,
                %error,
                "could not check the room's forward extremities for pruning; they are left as \
                 they are"
            );
        }
    }

    /// The pruning pass the module docs describe, over the events persisted since the last one:
    /// answers how many extremities it dropped. `RoomActorHandle` runs it after every operation
    /// (a no-op when the operation persisted nothing); code driving an actor directly calls it
    /// itself. `now_ms` is the clock pruning's "over a day old" reads.
    ///
    /// The current state is the same before and after: only extremities whose state the
    /// resolution did not answer are dropped, and the resolution of the rest -- one state, or
    /// several identical ones -- is the state it did answer.
    ///
    /// # Errors
    /// [`RoomError::State`] if the state resolution fails, [`RoomError::Store`] if the write
    /// does; on either nothing changed.
    pub fn maintain_extremities(&mut self, now_ms: i64) -> Result<usize, RoomError> {
        let persisted = std::mem::take(&mut self.persisted_since_maintenance);
        if persisted.is_empty() || self.forward_extremities.len() <= 1 || self.deleted {
            return Ok(0);
        }
        let extremities = self.forward_extremities_vec();
        let mut roots = Vec::with_capacity(extremities.len());
        for sn in &extremities {
            roots.push((*sn, self.root_after(*sn)?));
        }
        let resolved = self.current_view()?.root;
        // A root names a state by how it was built, not only by what it holds: the resolution's
        // answer is a root of its own even when its state is exactly some extremity's. So the
        // test is that the two states differ in nothing (Synapse compares state group ids,
        // which a resolution answering an existing group's state reuses).
        let mut dropped: Vec<EventSn> = Vec::new();
        for (sn, root) in &roots {
            let same = *root == resolved
                || self
                    .store
                    .diff(*root, resolved)
                    .map_err(|e| RoomError::State(e.to_string()))?
                    .is_empty();
            if !same {
                dropped.push(*sn);
            }
        }
        // The resolution answered a state none of the extremities has (a genuine merge of
        // forks), or exactly every one's: nothing to prune either way.
        if dropped.is_empty() || dropped.len() == extremities.len() {
            return Ok(0);
        }
        if !self.may_prune(&dropped, &persisted, now_ms) {
            return Ok(0);
        }
        let room_sn = self.room_sn;
        // No `bump_rewrites`: a read copy that still holds a pruned extremity resolves the
        // same state with it as without it, and re-reads the set with its next event
        // (`catch_up`), where a bump would make it load the whole room again.
        transact(&self.backend, TransactConfig::default(), |txn| {
            for sn in &dropped {
                self.tables
                    .extremities_fwd
                    .delete(txn, &(room_sn, *sn))
                    .map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(RoomError::from)?;
        for sn in &dropped {
            self.forward_extremities.remove(sn);
        }
        crate::metrics::count_extremities_pruned(dropped.len());
        tracing::info!(
            room_id = %self.room_id,
            pruned = dropped.len(),
            left = self.forward_extremities.len(),
            "dropped forward extremities that do not change the room's resolved state \
             (hs_room_extremities_pruned_total)"
        );
        Ok(dropped.len())
    }

    /// Synapse's conditions on the extremities pruning would drop (module docs): all must hold
    /// for every one, or none is dropped.
    fn may_prune(
        &self,
        dropped: &[EventSn],
        persisted: &PersistedSinceMaintenance,
        now_ms: i64,
    ) -> bool {
        let Some(current_depth) = persisted.max_depth else {
            return false;
        };
        let one_day_ago = now_ms.saturating_sub(PRUNE_OLD_AGE_MS);
        for sn in dropped {
            if persisted.events.contains(sn) {
                return false;
            }
            let Some(event) = self.event_uncached(*sn) else {
                return false;
            };
            if !self.stands_only_on_remote_events(event) {
                return false;
            }
            let header = event.header();
            let old = header.origin_server_ts < one_day_ago
                && header.depth < current_depth.saturating_sub(PRUNE_OLD_DEPTH);
            let superseded_by_its_server = persisted.servers.contains(header.sender.server_name())
                && header.depth < current_depth.saturating_sub(PRUNE_SAME_SERVER_DEPTH);
            if !(old || superseded_by_its_server) {
                return false;
            }
        }
        true
    }

    /// Whether `event` is another server's, or a dummy event of this server's that stands --
    /// through any number of this server's dummy events -- only on other servers' events. A
    /// new event here should keep citing this server's own events, so those are never pruned.
    fn stands_only_on_remote_events(&self, event: &Event) -> bool {
        let own = &self.identity.server_name;
        let mut to_check: Vec<&Event> = vec![event];
        let mut walked = 0usize;
        while let Some(event) = to_check.pop() {
            if event.header().sender.server_name() != own {
                continue;
            }
            if event.header().event_type != DUMMY_EVENT_TYPE {
                return false;
            }
            walked += 1;
            if walked > LOCAL_DUMMY_WALK {
                return false;
            }
            for id in pipeline::decode_event_ids(event.json().get("prev_events")) {
                let Some(prev) = self.sn_of(&id).and_then(|sn| self.event_uncached(sn)) else {
                    // A prev event this server does not hold is not one of its own.
                    continue;
                };
                to_check.push(prev);
            }
        }
        true
    }

    /// Sends an `org.matrix.dummy_event` citing the room's extremities (at most
    /// [`MAX_PREV_EVENTS`] of them, the deepest), as the first joined local user, in user-id
    /// order, the auth rules allow, when the room has more than one extremity. It is
    /// published and federated as any event this server sends, and hidden from clients
    /// ([`RoomActor::event_visible_to`]).
    ///
    /// # Errors
    /// [`RoomError::RoomNotFound`] for a deleted room; anything else
    /// [`RoomActor::send_event`] fails with except the refusals that mean "not this user"
    /// (`Forbidden`) or "nobody" (a blocked room).
    pub fn send_dummy_event(&mut self, now_ms: i64) -> Result<DummyOutcome, RoomError> {
        if self.deleted {
            return Err(RoomError::RoomNotFound(self.room_id.to_string()));
        }
        if self.forward_extremities.len() <= 1 {
            return Ok(DummyOutcome::NotForked);
        }
        let own = self.identity.server_name.clone();
        let mut local: Vec<OwnedUserId> = self
            .membership_list()?
            .iter()
            .filter(|(user, membership)| membership == "join" && user.server_name() == own)
            .map(|(user, _)| user.clone())
            .collect();
        local.sort();
        let cited = self.prev_events_for_new_event().len();
        for sender in local {
            match self.send_event(
                sender.clone(),
                DUMMY_EVENT_TYPE.to_owned(),
                None,
                serde_json::json!({}),
                None,
                now_ms,
            ) {
                Ok(event) => {
                    crate::metrics::count_dummy_event("sent");
                    return Ok(DummyOutcome::Sent {
                        sender,
                        event_id: event.event_id().to_owned(),
                        cited,
                    });
                }
                Err(RoomError::Forbidden(reason)) => {
                    tracing::debug!(
                        room_id = %self.room_id,
                        user_id = %sender,
                        %reason,
                        "this user may not send a dummy event here; trying the next"
                    );
                }
                Err(RoomError::RoomBlocked(_)) => break,
                Err(error) => {
                    crate::metrics::count_dummy_event("failed");
                    return Err(error);
                }
            }
        }
        crate::metrics::count_dummy_event("no_local_sender");
        Ok(DummyOutcome::NoLocalSender)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use hs_kv::memory::MemoryBackend;
    use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
    use hs_model::{Event, hash, signing};
    use ruma::{OwnedEventId, RoomVersionId, UserId, user_id};

    use super::super::{CreateRoomRequest, RemoteEventOutcome, RoomActor};
    use super::*;
    use crate::identity::HomeserverIdentity;
    use crate::persist::Tables;

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    fn room() -> RoomActor<MemoryBackend> {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        RoomActor::create_room(
            backend,
            tables,
            HomeserverIdentity::for_tests("hs1"),
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                room_version: Some(RoomVersionId::V11),
                ..Default::default()
            },
            1,
        )
        .unwrap()
    }

    /// A PDU of `remote.example`'s, hashed and signed, with the fields as given.
    fn remote_pdu(key: &signing::SigningKeyPair, fields: serde_json::Value) -> Event {
        let mut canonical = to_canonical_object(&fields, true).unwrap();
        let content_hash = hash::content_hash_base64(&canonical);
        canonical.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(content_hash),
            )])),
        );
        let server = ruma::ServerName::parse("remote.example").unwrap();
        signing::sign_object(&mut canonical, &server, key).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&CanonicalJsonValue::Object(canonical).to_canonical_bytes())
                .unwrap();
        Event::parse(&value, RoomVersionId::V11).unwrap()
    }

    fn state_id(actor: &RoomActor<MemoryBackend>, event_type: &str, state_key: &str) -> String {
        actor
            .state_event(event_type, state_key)
            .unwrap()
            .unwrap()
            .event_id()
            .to_string()
    }

    fn extremity_ids(actor: &RoomActor<MemoryBackend>) -> BTreeSet<OwnedEventId> {
        actor
            .forward_extremity_ids()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    fn state_ids(actor: &RoomActor<MemoryBackend>) -> BTreeSet<String> {
        actor
            .full_state()
            .unwrap()
            .iter()
            .map(|e| e.event_id().to_string())
            .collect()
    }

    fn accept(actor: &mut RoomActor<MemoryBackend>, event: &Event) {
        assert!(matches!(
            actor.accept_remote_event(event.clone()).unwrap(),
            RemoteEventOutcome::Stored(_)
        ));
    }

    /// Alice's room with `bob@remote.example` joined, as the head the tests fork from.
    struct Joined {
        actor: RoomActor<MemoryBackend>,
        key: signing::SigningKeyPair,
        bob: &'static UserId,
        room_id: String,
        create: String,
        power: String,
        join_rules: String,
        join: String,
        depth: i64,
    }

    fn joined() -> Joined {
        let mut actor = room();
        let bob: &'static UserId = user_id!("@bob:remote.example");
        let key = signing::SigningKeyPair::generate("1");
        let room_id = actor.room_id().to_string();
        let create = state_id(&actor, "m.room.create", "");
        let power = state_id(&actor, "m.room.power_levels", "");
        let join_rules = state_id(&actor, "m.room.join_rules", "");
        let (head, head_depth) = actor.forward_extremity_ids().remove(0);
        let join = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.member", "state_key": bob.as_str(), "sender": bob.as_str(),
                "room_id": room_id, "origin_server_ts": 10, "depth": head_depth + 1,
                "content": {"membership": "join"},
                "prev_events": [head.as_str()],
                "auth_events": [create, power, join_rules],
            }),
        );
        accept(&mut actor, &join);
        Joined {
            actor,
            key,
            bob,
            room_id,
            create,
            power,
            join_rules,
            join: join.event_id().to_string(),
            depth: head_depth + 1,
        }
    }

    impl Joined {
        /// Bob's PDU citing `prev`: a change of his displayname when `displayname` is set, a
        /// message otherwise; authorized by his join, or by `member_auth`, a later member event
        /// of his.
        fn bob_event(
            &self,
            prev: &str,
            depth: i64,
            ts: i64,
            displayname: Option<&str>,
            member_auth: Option<&str>,
        ) -> Event {
            let member = member_auth.unwrap_or(&self.join);
            let fields = match displayname {
                Some(name) => serde_json::json!({
                    "type": "m.room.member", "state_key": self.bob.as_str(),
                    "sender": self.bob.as_str(), "room_id": self.room_id,
                    "origin_server_ts": ts, "depth": depth,
                    "content": {"membership": "join", "displayname": name},
                    "prev_events": [prev],
                    "auth_events": [self.create, self.power, self.join_rules, member],
                }),
                None => serde_json::json!({
                    "type": "m.room.message", "sender": self.bob.as_str(),
                    "room_id": self.room_id, "origin_server_ts": ts, "depth": depth,
                    "content": {"msgtype": "m.text", "body": format!("at {depth}")},
                    "prev_events": [prev],
                    "auth_events": [self.create, self.power, member],
                }),
            };
            remote_pdu(&self.key, fields)
        }

        /// The shape `_prune_extremities` exists for: bob's displayname change `S` citing his
        /// join, stuck (nothing cites it), and from the same join bob's later displayname
        /// change, which the resolution prefers, then `chain` of his messages on top of it.
        /// Answers `S`'s event id and the chain's head.
        fn stuck_and_chain(&mut self, chain: i64) -> (OwnedEventId, OwnedEventId) {
            let join = self.join.clone();
            let stuck = self.bob_event(&join, self.depth + 1, 20, Some("stuck"), None);
            accept(&mut self.actor, &stuck);
            let fresh = self.bob_event(&join, self.depth + 1, 30, Some("fresh"), None);
            accept(&mut self.actor, &fresh);
            let fresh_id = fresh.event_id().to_string();
            let mut head = fresh_id.clone();
            for n in 0..chain {
                let depth = self.depth + 2 + n;
                let message = self.bob_event(&head, depth, 40 + n, None, Some(&fresh_id));
                accept(&mut self.actor, &message);
                head = message.event_id().to_string();
            }
            (
                stuck.event_id().to_owned(),
                OwnedEventId::try_from(head).unwrap(),
            )
        }
    }

    /// A stuck remote extremity whose state the resolution does not pick is dropped once its
    /// server has moved more than 20 events on from it, and the room's current state is the
    /// same before and after; before that it is kept.
    #[test]
    fn a_stuck_remote_extremity_that_changes_nothing_is_pruned_and_the_state_is_unchanged() {
        let mut room = joined();
        let (stuck, head) = room.stuck_and_chain(5);
        assert_eq!(
            extremity_ids(&room.actor),
            BTreeSet::from([stuck.clone(), head.clone()])
        );
        let fresh = state_id(&room.actor, "m.room.member", room.bob.as_str());
        assert_ne!(
            fresh,
            stuck.to_string(),
            "the resolution prefers the later change"
        );
        // Five events on: too recent to drop.
        assert_eq!(room.actor.maintain_extremities(1_000).unwrap(), 0);
        assert_eq!(room.actor.forward_extremity_count(), 2);

        let head_depth = room
            .actor
            .forward_extremity_ids()
            .into_iter()
            .find(|(id, _)| *id == head)
            .unwrap()
            .1;
        let mut head = head.to_string();
        for n in 0..20 {
            let message = room.bob_event(&head, head_depth + 1 + n, 100 + n, None, Some(&fresh));
            accept(&mut room.actor, &message);
            head = message.event_id().to_string();
        }
        let before = state_ids(&room.actor);
        let pruned_before = crate::metrics::extremities_pruned();
        assert_eq!(room.actor.maintain_extremities(1_000).unwrap(), 1);
        assert!(crate::metrics::extremities_pruned() > pruned_before);
        assert_eq!(
            extremity_ids(&room.actor),
            BTreeSet::from([OwnedEventId::try_from(head.as_str()).unwrap()])
        );
        assert_eq!(state_ids(&room.actor), before, "pruning changes no state");
        assert_eq!(
            state_id(&room.actor, "m.room.member", room.bob.as_str()),
            fresh
        );

        // Durable: a reload finds the same single extremity and the same state.
        let reloaded = RoomActor::load(
            room.actor.backend.clone(),
            room.actor.tables.clone(),
            HomeserverIdentity::for_tests("hs1"),
            room.actor.room_id(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(extremity_ids(&reloaded), extremity_ids(&room.actor));
        assert_eq!(state_ids(&reloaded), before);
    }

    /// Through the handle, as federation delivers events, the pruning pass runs by itself after
    /// the operation that persisted them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_handle_prunes_after_an_operation_that_persisted_events() {
        let mut room = joined();
        let (_, head) = room.stuck_and_chain(5);
        let fresh = state_id(&room.actor, "m.room.member", room.bob.as_str());
        let head_depth = room
            .actor
            .forward_extremity_ids()
            .into_iter()
            .find(|(id, _)| *id == head)
            .unwrap()
            .1;
        let mut events = Vec::new();
        let mut prev = head.to_string();
        for n in 0..20 {
            let message = room.bob_event(&prev, head_depth + 1 + n, 100 + n, None, Some(&fresh));
            prev = message.event_id().to_string();
            events.push(message);
        }
        let handle = super::super::RoomActorHandle::new(room.actor);
        for event in events {
            handle.accept_remote_event(event).await.unwrap();
        }
        assert_eq!(
            handle.query(|actor| actor.forward_extremity_ids()).await,
            vec![(
                OwnedEventId::try_from(prev.as_str()).unwrap(),
                head_depth + 20
            )]
        );
    }

    /// The same shape with the stuck extremity alice's own: never pruned, since a new event
    /// here should cite this server's own events.
    #[test]
    fn this_servers_own_extremity_is_never_pruned() {
        let mut room = joined();
        let join_sn = room
            .actor
            .sn_of(&OwnedEventId::try_from(room.join.as_str()).unwrap())
            .unwrap();
        let stuck = room
            .actor
            .send_event_citing(
                user_id!("@alice:hs1").to_owned(),
                "m.room.member".to_owned(),
                Some("@alice:hs1".to_owned()),
                serde_json::json!({"membership": "join", "displayname": "stuck"}),
                None,
                20,
                &[join_sn],
            )
            .unwrap();
        let mut head = room.join.clone();
        for n in 0..30 {
            let message = room.bob_event(&head, room.depth + 1 + n, 40 + n, None, None);
            accept(&mut room.actor, &message);
            head = message.event_id().to_string();
        }
        assert_eq!(room.actor.forward_extremity_count(), 2);
        // Old by any measure, and still kept.
        assert_eq!(room.actor.maintain_extremities(400 * DAY_MS).unwrap(), 0);
        assert!(extremity_ids(&room.actor).contains(stuck.event_id()));
    }

    /// Past ten extremities a new local event cites ten, the deepest and then the latest in the
    /// timeline, and merges exactly those.
    #[test]
    fn a_local_event_cites_at_most_ten_extremities_the_deepest() {
        let mut actor = room();
        let alice = user_id!("@alice:hs1").to_owned();
        let head = actor.forward_extremities_vec();
        let mut forks = Vec::new();
        for n in 0..15 {
            let event = actor
                .send_event_citing(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"msgtype": "m.text", "body": format!("fork {n}")}),
                    None,
                    10 + n,
                    &head,
                )
                .unwrap();
            forks.push(event);
        }
        // One fork a level deeper than the rest, made first: it outranks every later one.
        let first_sn = actor.sn_of(forks[0].event_id()).unwrap();
        let deeper = actor
            .send_event_citing(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "deeper"}),
                None,
                30,
                &[first_sn],
            )
            .unwrap();
        assert_eq!(actor.forward_extremity_count(), 15);
        let event = actor
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "merge"}),
                None,
                100,
            )
            .unwrap();
        let cited: BTreeSet<OwnedEventId> =
            pipeline::decode_event_ids(event.json().get("prev_events"))
                .into_iter()
                .collect();
        assert_eq!(cited.len(), MAX_PREV_EVENTS);
        let mut expected: BTreeSet<OwnedEventId> =
            forks[6..].iter().map(|e| e.event_id().to_owned()).collect();
        expected.insert(deeper.event_id().to_owned());
        assert_eq!(cited, expected);
        let mut left: BTreeSet<OwnedEventId> = forks[1..6]
            .iter()
            .map(|e| e.event_id().to_owned())
            .collect();
        left.insert(event.event_id().to_owned());
        assert_eq!(extremity_ids(&actor), left);
    }

    /// A dummy event merges up to ten extremities, as a joined local user, and nobody sees it;
    /// a room of one extremity is not sent one.
    #[test]
    fn a_dummy_event_merges_forks_and_is_hidden_from_clients() {
        let mut actor = room();
        let alice = user_id!("@alice:hs1").to_owned();
        let head = actor.forward_extremities_vec();
        for n in 0..14 {
            actor
                .send_event_citing(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"msgtype": "m.text", "body": format!("fork {n}")}),
                    None,
                    10 + n,
                    &head,
                )
                .unwrap();
        }
        let before = state_ids(&actor);
        let sent_before = crate::metrics::dummy_events("sent");
        let DummyOutcome::Sent {
            sender,
            event_id,
            cited,
        } = actor.send_dummy_event(1_000).unwrap()
        else {
            panic!("a dummy event is sent");
        };
        assert_eq!(sender, alice);
        assert_eq!(cited, MAX_PREV_EVENTS);
        assert_eq!(actor.forward_extremity_count(), 14 - 10 + 1);
        assert!(crate::metrics::dummy_events("sent") > sent_before);
        let dummy = actor.event_by_id(&event_id).unwrap().clone();
        assert_eq!(dummy.header().event_type, DUMMY_EVENT_TYPE);
        assert!(!actor.event_visible_to(&dummy, &alice).unwrap());
        assert_eq!(state_ids(&actor), before, "a dummy event is no state");

        assert!(matches!(
            actor.send_dummy_event(2_000).unwrap(),
            DummyOutcome::Sent { cited: 5, .. }
        ));
        assert_eq!(actor.forward_extremity_count(), 1);
        assert_eq!(
            actor.send_dummy_event(3_000).unwrap(),
            DummyOutcome::NotForked
        );
    }

    /// A room none of this server's users is joined to gets no dummy event.
    #[test]
    fn no_dummy_event_without_a_joined_local_user() {
        let mut room = joined();
        let alice = user_id!("@alice:hs1").to_owned();
        let head = room.actor.forward_extremities_vec();
        let mut forks = Vec::new();
        for n in 0..3 {
            let event = room
                .actor
                .send_event_citing(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    serde_json::json!({"msgtype": "m.text", "body": format!("fork {n}")}),
                    None,
                    10 + n,
                    &head,
                )
                .unwrap();
            forks.push(room.actor.sn_of(event.event_id()).unwrap());
        }
        room.actor
            .send_event_citing(
                alice.clone(),
                "m.room.member".to_owned(),
                Some(alice.to_string()),
                serde_json::json!({"membership": "leave"}),
                None,
                50,
                &forks[..1],
            )
            .unwrap();
        assert_eq!(room.actor.forward_extremity_count(), 3);
        assert_eq!(
            room.actor.send_dummy_event(1_000).unwrap(),
            DummyOutcome::NoLocalSender
        );
        assert_eq!(room.actor.forward_extremity_count(), 3);
    }
}
