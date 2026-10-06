//! The prev event of a received event that this server could not walk back to, held with the
//! state another server answered for it.
//!
//! An event arrives over `/send` citing a prev event this server does not hold. The usual
//! answer is to fetch the missing history (`/get_missing_events`, `/backfill`:
//! `hs_federation::backfill`); when that does not close the gap -- the sending server does not
//! answer `/backfill`, the history is too long, or it was never there -- the spec's other way is
//! to ask that server for the state at the missing prev event (`/state_ids`), fetch the events
//! it names that this server lacks (`/event`), and take the received event at that state. Synapse
//! does the same (`_compute_event_context_with_maybe_missing_prevs`); Sytest's outlier and
//! `/state_ids` tests expect it.
//!
//! What is held, and how:
//!
//! - every event fetched for the state and its auth chain is an **outlier**
//!   ([`RoomActor::persist_outliers`]): indexed, in the state store, never in the timeline. Each
//!   is authorised against its own `auth_events` first; one that fails is stored rejected
//!   (`rejected`) and left out of the state, so a server answering a made-up state (Sytest's
//!   "Should not be able to take over the room by pretending there is no PL event") gains
//!   nothing;
//! - the missing prev event itself is an outlier too, authorised against its own `auth_events`
//!   (as Synapse's outliers are; not also against the fetched state, which may lack an event
//!   that did not verify), and held **with that state**: a
//!   `Tables::state_snapshots` row (durable; `RoomActor::load` reads it back) and a root in
//!   [`RoomActor::placed_outlier_roots`], so the state after it -- what an event citing it is
//!   resolved from ([`RoomActor::root_after`]) -- is the fetched state with the prev event over
//!   it. It is not a forward extremity, and nothing is published for it: it has no timeline
//!   position, as Synapse's outliers have none;
//! - the received event then goes through [`RoomActor::accept_remote_event`] as any other, its
//!   prev event now held: authorised at the state resolved from its prev events, placed in the
//!   timeline, published, and fed to the state store with that resolved state
//!   ([`PersistKind::AfterFetchedState`]), since the store's own state at an outlier is
//!   meaningless ([`RoomActor::feed_store_outlier`]). It supersedes only the extremities it
//!   cites, which the outlier is not, so the room's earlier extremities stay (Sytest's "Forward
//!   extremities remain so even after the next events are populated as outliers").

use std::collections::HashSet;

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_model::Event;
use hs_model::ids::EventSn;
use hs_state::api::StateStore;
use ruma::OwnedEventId;

use super::{RemoteEventOutcome, RoomActor, StateBefore, to_kv, topological_order};
use crate::error::RoomError;
use crate::persist::encode_event_sns;
use crate::pipeline;

impl<B: KvBackend> RoomActor<B> {
    /// Holds `prev` -- a prev event of a received event, which this server could not walk back
    /// to -- with the state before it: `state_before` names that state's events (as another
    /// server answered `/state_ids`), and `fetched` carries every event of it, its auth chain and
    /// `prev`'s own ancestry that this server lacked (fetched by `/event`), already verified by
    /// the caller (hashes and signatures; this checks neither). See the module docs for what is
    /// held and how. An ID of `state_before` this actor does not hold afterwards is left out.
    ///
    /// # Idempotency
    /// A `prev` already held with a state, or in the timeline, is [`RemoteEventOutcome::AlreadyKnown`];
    /// fetched events already held are skipped.
    ///
    /// # Errors
    /// [`RoomError::Forbidden`] if `prev` fails authorization (it is then stored rejected, as a
    /// received event is, and held with the state before it, which is the state after it);
    /// [`RoomError::Store`], [`RoomError::Fenced`] or [`RoomError::State`]
    /// from persistence.
    pub fn accept_prev_event_with_state(
        &mut self,
        prev: Event,
        state_before: &[OwnedEventId],
        fetched: Vec<Event>,
    ) -> Result<RemoteEventOutcome, RoomError> {
        if let Some(&sn) = self.event_id_index.get(prev.event_id())
            && (self.fetched_state_outliers.contains(&sn) || self.timeline_contains(sn))
        {
            return Ok(RemoteEventOutcome::AlreadyKnown);
        }

        // 1. Every fetched event, oldest first, each judged by its own auth events.
        let mut pending: Vec<Event> = fetched
            .into_iter()
            .filter(|event| {
                event.event_id() != prev.event_id()
                    && !self.event_id_index.contains_key(event.event_id())
            })
            .collect();
        pending.sort_by(topological_order);
        let mut seen: HashSet<OwnedEventId> = HashSet::new();
        let mut rejected = 0usize;
        let mut unjudged = 0usize;
        let mut accepted: Vec<Event> = Vec::with_capacity(pending.len());
        for event in pending {
            if !seen.insert(event.event_id().to_owned()) {
                continue;
            }
            match self.authorize_outlier(&event) {
                Ok(()) => {
                    // Held one by one, so the next one's auth events can be found.
                    self.persist_outliers(vec![event.clone()])?;
                    accepted.push(event);
                }
                Err(RoomError::Forbidden(reason)) => {
                    rejected += 1;
                    self.store_rejected(event, &reason)?;
                }
                // Its auth chain is broken here: left out, and so is whatever stands on it.
                Err(RoomError::MissingAncestors(missing)) => {
                    unjudged += 1;
                    tracing::info!(
                        room_id = %self.room_id,
                        event_id = %event.event_id(),
                        missing = missing.len(),
                        "left out a fetched event whose auth events are not all held"
                    );
                }
                Err(other) => return Err(other),
            }
        }

        // 2. The state before `prev`: what is held and not rejected.
        let state_sns: Vec<EventSn> = state_before
            .iter()
            .filter_map(|id| self.event_id_index.get(id).copied())
            .filter(|sn| !self.rejected.contains(sn))
            .collect();

        // 3. `prev` itself, an outlier like the rest: judged by its own auth events, as
        //    Synapse judges a fetched prev event (`_auth_and_persist_outliers`). Not also at the
        //    fetched state: a state event of it that does not verify (Sytest's made-up power
        //    levels in "... asks for /state_ids and resolves the state") leaves the state
        //    without that key, and an honest prev event would then be refused for it.
        //    One that is refused is stored rejected and still held with the state: the state
        //    after a rejected event is the state before it, and an event citing it (Sytest's R
        //    and S, after a Q whose auth events are of another room) is judged at that state,
        //    where the prev events of the rejected one are not held to walk back to.
        let prev_id = prev.event_id().to_owned();
        let refused = match self.authorize_outlier(&prev) {
            Ok(()) => {
                self.persist_outliers(vec![prev])?;
                None
            }
            Err(RoomError::Forbidden(reason)) => {
                self.store_rejected(prev, &reason)?;
                Some(reason)
            }
            Err(error) => return Err(error),
        };
        let sn = *self.event_id_index.get(&prev_id).ok_or_else(|| {
            RoomError::Internal(format!("{prev_id} was not indexed after persisting"))
        })?;

        // 4. Held with that state, durably.
        let room_sn = self.room_sn;
        let snapshot_bytes = encode_event_sns(&state_sns);
        let fence_failure: std::cell::Cell<Option<String>> = std::cell::Cell::new(None);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.tables
                .state_snapshots
                .put(txn, &(room_sn, sn), &snapshot_bytes)
                .map_err(to_kv)?;
            self.fence_check(txn, &fence_failure)?;
            Ok(())
        })
        .map_err(|e| match fence_failure.take() {
            Some(msg) => RoomError::Fenced(msg),
            None => RoomError::from(e),
        })?;
        if let Some(reason) = refused {
            self.record_rejected_outlier_state(sn, &state_sns)?;
            self.fetched_state_outliers.insert(sn);
            tracing::info!(
                room_id = %self.room_id,
                event_id = %prev_id,
                state_events = state_sns.len(),
                %reason,
                "held a missing prev event, rejected, with the state another server answered for it"
            );
            return Err(RoomError::Forbidden(reason));
        }
        self.record_placed_outlier_state(sn, &state_sns)?;
        self.fetched_state_outliers.insert(sn);
        tracing::info!(
            room_id = %self.room_id,
            event_id = %prev_id,
            state_events = state_sns.len(),
            fetched = accepted.len(),
            rejected,
            unjudged,
            "held a missing prev event with the state another server answered for it"
        );
        Ok(RemoteEventOutcome::Stored(sn))
    }

    /// Holds `events` -- auth events of a received event this server lacked, fetched by `/event`
    /// and verified by the caller -- as outliers, oldest first, each judged by its own
    /// `auth_events` ([`RoomActor::authorize_outlier`]): one that passes is held, one that fails
    /// is stored rejected (`rejected`), so the received event citing it is rejected in turn, as
    /// Complement's `TestInboundFederationRejectsEventsWithRejectedAuthEvents` has it. One whose
    /// own auth events are still not held is left out (it cannot be judged). Returns how many were
    /// held or rejected.
    ///
    /// # Errors
    /// [`RoomError::Store`], [`RoomError::Fenced`] or [`RoomError::State`] from persistence.
    pub fn accept_auth_outliers(&mut self, events: Vec<Event>) -> Result<usize, RoomError> {
        let mut pending: Vec<Event> = events
            .into_iter()
            .filter(|event| !self.event_id_index.contains_key(event.event_id()))
            .collect();
        pending.sort_by(topological_order);
        let mut seen: HashSet<OwnedEventId> = HashSet::new();
        let (mut held, mut rejected, mut unjudged) = (0usize, 0usize, 0usize);
        for event in pending {
            if !seen.insert(event.event_id().to_owned())
                || self.event_id_index.contains_key(event.event_id())
            {
                continue;
            }
            match self.authorize_outlier(&event) {
                Ok(()) => {
                    self.persist_outliers(vec![event])?;
                    held += 1;
                }
                Err(RoomError::Forbidden(reason)) => {
                    self.store_rejected(event, &reason)?;
                    rejected += 1;
                }
                Err(RoomError::MissingAncestors(_)) => unjudged += 1,
                Err(other) => return Err(other),
            }
        }
        tracing::info!(
            room_id = %self.room_id,
            held,
            rejected,
            unjudged,
            "held fetched auth events as outliers"
        );
        Ok(held + rejected)
    }

    /// Whether `sn` has a timeline position.
    fn timeline_contains(&self, sn: EventSn) -> bool {
        self.timeline.values().any(|held| *held == sn)
    }

    /// An outlier fetched for a state is judged by its own `auth_events` only (as the events of
    /// a `send_join` snapshot are), with no state before it: its prev events are not held.
    ///
    /// One whose auth events are not all held cannot be judged, and is
    /// [`RoomError::MissingAncestors`]: the caller leaves it out. Judging it by the auth events
    /// that are held instead let a chain of a user's memberships whose first link nobody would
    /// serve in, each judged without the membership before it -- Complement's
    /// `TestCorruptedAuthChain`, where C, D and E followed a B that `/event` answers `404` for,
    /// and E became the room's state.
    fn authorize_outlier(&self, event: &Event) -> Result<(), RoomError> {
        let ids = pipeline::decode_event_ids(event.json().get("auth_events"));
        let mut auth_sns: Vec<EventSn> = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for id in ids {
            match self.event_id_index.get(&id) {
                Some(sn) => auth_sns.push(*sn),
                None => missing.push(id),
            }
        }
        if !missing.is_empty() {
            // An auth event of another room is not missing, it is wrong: the outlier is
            // rejected (Sytest's "outliers whose auth_events are in a different room are
            // correctly rejected"), as a received event citing one is.
            if let Some((cited, other_room)) = self.held_in_another_room(&missing)? {
                return Err(RoomError::Forbidden(format!(
                    "it cites {cited}, an event of another room ({other_room})"
                )));
            }
            return Err(RoomError::MissingAncestors(missing));
        }
        self.authorize_remote_at(event, &[], &auth_sns, StateBefore::None)
    }

    /// The resolved state before an event whose prev events (`prev_sns`, as held) include an
    /// outlier held with a fetched state, as the state store must be fed it
    /// ([`PersistKind::AfterFetchedState`]); `None` when they include none, the ordinary case.
    ///
    /// # Errors
    /// [`RoomError::State`] if the store fails.
    pub(super) fn fetched_state_snapshot_for(
        &self,
        prev_sns: &[EventSn],
    ) -> Result<Option<Vec<EventSn>>, RoomError> {
        let effective = self.effective_prev_sns(prev_sns);
        if !effective
            .iter()
            .any(|sn| self.fetched_state_outliers.contains(sn))
        {
            return Ok(None);
        }
        let view = self.state_view(&effective)?;
        let diff = self
            .store
            .diff(self.store.empty_root(), view.root)
            .map_err(|e| RoomError::State(e.to_string()))?;
        Ok(Some(diff.added.values().copied().collect()))
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
    use hs_model::{Event, hash, signing};
    use ruma::{RoomVersionId, UserId, user_id};

    use super::super::tests::room;
    use super::super::{RemoteEventOutcome, RoomActor};
    use crate::error::RoomError;
    use crate::membership::Action;
    use crate::timeline::Direction;

    /// A PDU of `remote.example`'s, hashed and signed: `fields` with `prev_events`/`auth_events`
    /// as given, not what this room holds.
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

    fn ids(actor: &RoomActor<MemoryBackend>) -> (String, String, Vec<String>) {
        let create = actor
            .state_event("m.room.create", "")
            .unwrap()
            .unwrap()
            .event_id()
            .to_string();
        let power = actor
            .state_event("m.room.power_levels", "")
            .unwrap()
            .unwrap()
            .event_id()
            .to_string();
        let state: Vec<String> = actor
            .full_state()
            .unwrap()
            .iter()
            .map(|e| e.event_id().to_string())
            .collect();
        (create, power, state)
    }

    /// Sytest's `send_and_await_outlier` shape: S arrives citing R, R cites Q, and Q cannot be
    /// walked back to; the other server answers the state at Q. Q is held as an outlier with that
    /// state, R and S are then accepted ordinarily, the old extremity stays, and the state before
    /// R is the state after Q; across a reload too. The outlier has no state of its own to read.
    #[test]
    fn a_missing_prev_event_held_with_a_fetched_state_lets_the_events_after_it_in() {
        let mut actor = room("public_chat");
        let bob: &UserId = user_id!("@bob:remote.example");
        let key = signing::SigningKeyPair::generate("1");
        let (create, power, state_before_q) = ids(&actor);
        let room_id = actor.room_id().to_string();
        let join_rules = actor
            .state_event("m.room.join_rules", "")
            .unwrap()
            .unwrap()
            .event_id()
            .to_string();
        let old_extremities = actor.forward_extremities_vec();

        // Q: bob's join, whose prev event this server will never hold.
        let q = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.member", "state_key": bob.as_str(), "sender": bob.as_str(),
                "room_id": room_id, "origin_server_ts": 10, "depth": 5,
                "content": {"membership": "join"},
                "prev_events": ["$unknown_prev:remote.example"],
                "auth_events": [create, power, join_rules],
            }),
        );
        let r = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob.as_str(), "room_id": room_id,
                "origin_server_ts": 11, "depth": 6, "content": {"body": "R"},
                "prev_events": [q.event_id().as_str()],
                "auth_events": [create, power, q.event_id().as_str()],
            }),
        );
        let s = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob.as_str(), "room_id": room_id,
                "origin_server_ts": 12, "depth": 7, "content": {"body": "S"},
                "prev_events": [r.event_id().as_str()],
                "auth_events": [create, power, q.event_id().as_str()],
            }),
        );
        // Without the state, R is a gap.
        let err = actor.accept_remote_event(r.clone()).unwrap_err();
        assert!(
            matches!(err, crate::error::RoomError::MissingAncestors(_)),
            "{err}"
        );

        let state_ids: Vec<ruma::OwnedEventId> = state_before_q
            .iter()
            .map(|id| ruma::EventId::parse(id).unwrap().to_owned())
            .collect();
        let outcome = actor
            .accept_prev_event_with_state(q.clone(), &state_ids, Vec::new())
            .unwrap();
        assert!(matches!(outcome, RemoteEventOutcome::Stored(_)));
        assert!(
            actor
                .accept_prev_event_with_state(q.clone(), &state_ids, Vec::new())
                .is_ok_and(|o| matches!(o, RemoteEventOutcome::AlreadyKnown))
        );
        // Q is held, but has no position and no state of its own to read.
        assert!(actor.event_by_id(q.event_id()).is_some());
        assert!(actor.state_before_event(q.event_id()).unwrap().is_some());
        let (timeline, _) = actor.paginate(None, Direction::Backward, 10);
        assert!(!timeline.iter().any(|e| e.event_id() == q.event_id()));

        // R and S come in as ordinary events now, at the state after Q.
        assert!(matches!(
            actor.accept_remote_event(r.clone()).unwrap(),
            RemoteEventOutcome::Stored(_)
        ));
        assert!(matches!(
            actor.accept_remote_event(s.clone()).unwrap(),
            RemoteEventOutcome::Stored(_)
        ));
        let before_r = actor.state_before_event(r.event_id()).unwrap().unwrap();
        let mut before_r_ids: Vec<String> = before_r
            .state
            .iter()
            .map(|e| e.event_id().to_string())
            .collect();
        before_r_ids.sort();
        let mut expected = state_before_q.clone();
        expected.push(q.event_id().to_string());
        expected.sort();
        assert_eq!(before_r_ids, expected);
        // bob is in the room as far as the current state goes, and the old extremity stays.
        let member = actor
            .state_event("m.room.member", bob.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(member.event_id(), q.event_id());
        let extremities = actor.forward_extremities_vec();
        for old in &old_extremities {
            assert!(extremities.contains(old), "the old extremity stays");
        }
        assert!(extremities.contains(&actor.event_id_index[s.event_id()]));
        let (timeline, _) = actor.paginate(None, Direction::Backward, 10);
        assert_eq!(timeline[0].event_id(), s.event_id());
        assert_eq!(timeline[1].event_id(), r.event_id());

        // The same after a reload.
        let backend = actor.backend.clone();
        let tables = actor.tables.clone();
        let identity = actor.identity.clone();
        drop(actor);
        let reloaded = RoomActor::load(
            backend,
            tables,
            identity,
            &ruma::RoomId::parse(room_id.as_str()).unwrap(),
        )
        .unwrap()
        .unwrap();
        let before_r = reloaded.state_before_event(r.event_id()).unwrap().unwrap();
        let mut before_r_ids: Vec<String> = before_r
            .state
            .iter()
            .map(|e| e.event_id().to_string())
            .collect();
        before_r_ids.sort();
        assert_eq!(before_r_ids, expected);
        let member = reloaded
            .state_event("m.room.member", bob.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(member.event_id(), q.event_id());
        let _ = Action::Join;
    }

    /// Sytest's "Should not be able to take over the room by pretending there is no PL event":
    /// a state fetched for a missing prev event is not taken on trust. An event in it that its
    /// own auth events do not authorise (a power-levels event by a user with no power, citing no
    /// power levels) is stored rejected and left out of the state.
    /// Complement's `TestCorruptedAuthChain`: the state answered for a prev event names bob's
    /// membership E, whose auth chain runs E -> D -> C -> B -> A; B is never served. C, D and E
    /// cannot be judged without B and are left out -- not judged by the auth events that are
    /// held, which let them through -- and bob's membership in the room stays his join.
    #[test]
    fn fetched_events_whose_auth_chain_is_broken_are_left_out() {
        let mut actor = room("public_chat");
        let bob: &UserId = user_id!("@bob:remote.example");
        actor
            .membership_action(
                bob.to_owned(),
                Action::Join,
                bob.to_owned(),
                serde_json::json!({}),
                2,
            )
            .unwrap();
        let key = signing::SigningKeyPair::generate("1");
        let (create, power, state) = ids(&actor);
        let room_id = actor.room_id().to_string();
        let join_rules = actor
            .state_event("m.room.join_rules", "")
            .unwrap()
            .unwrap()
            .event_id()
            .to_string();
        let bob_join = actor
            .state_event("m.room.member", bob.as_str())
            .unwrap()
            .unwrap()
            .event_id()
            .to_string();
        let member = |name: &str, prev: &str, auth: &str, depth: i64| {
            remote_pdu(
                &key,
                serde_json::json!({
                    "type": "m.room.member", "state_key": bob.as_str(), "sender": bob.as_str(),
                    "room_id": room_id, "origin_server_ts": 10 + depth, "depth": depth,
                    "content": {"membership": "join", "displayname": name},
                    "prev_events": [prev],
                    "auth_events": [create, power, join_rules, auth],
                }),
            )
        };
        let a = member("A", &bob_join, &bob_join, 10);
        let b = member("B", a.event_id().as_str(), a.event_id().as_str(), 11);
        let c = member("C", b.event_id().as_str(), b.event_id().as_str(), 12);
        let d = member("D", c.event_id().as_str(), c.event_id().as_str(), 13);
        let e = member("E", d.event_id().as_str(), d.event_id().as_str(), 14);
        let prev = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob.as_str(), "room_id": room_id,
                "origin_server_ts": 30, "depth": 15, "content": {"body": "for /state_ids"},
                "prev_events": [e.event_id().as_str()],
                "auth_events": [create, power, e.event_id().as_str()],
            }),
        );
        let mut state_ids: Vec<ruma::OwnedEventId> = state
            .iter()
            .filter(|id| id.as_str() != bob_join)
            .map(|id| ruma::EventId::parse(id).unwrap().to_owned())
            .collect();
        state_ids.push(e.event_id().to_owned());
        let outcome = actor.accept_prev_event_with_state(
            prev,
            &state_ids,
            vec![a.clone(), c.clone(), d.clone(), e.clone()],
        );
        assert!(
            matches!(outcome, Err(RoomError::MissingAncestors(_))),
            "{outcome:?}"
        );
        for left_out in [&c, &d, &e] {
            assert!(actor.held_event(left_out.event_id()).is_none());
            assert!(!actor.is_rejected_event(left_out.event_id()));
        }
        let membership = actor
            .state_event("m.room.member", bob.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(membership.event_id().as_str(), bob_join);
    }

    #[test]
    fn a_fetched_state_event_its_auth_events_do_not_authorise_is_rejected_and_left_out() {
        let mut actor = room("public_chat");
        let bob: &UserId = user_id!("@bob:remote.example");
        actor
            .membership_action(
                bob.to_owned(),
                Action::Join,
                bob.to_owned(),
                serde_json::json!({}),
                2,
            )
            .unwrap();
        let key = signing::SigningKeyPair::generate("1");
        let (create, power, mut state) = ids(&actor);
        let room_id = actor.room_id().to_string();
        let bob_join = actor
            .state_event("m.room.member", bob.as_str())
            .unwrap()
            .unwrap()
            .event_id()
            .to_string();
        let real_power = actor
            .state_event("m.room.power_levels", "")
            .unwrap()
            .unwrap()
            .event_id()
            .to_owned();

        // X: bob gives himself all the power, citing only the create event and his join.
        let x = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.power_levels", "state_key": "", "sender": bob.as_str(),
                "room_id": room_id, "origin_server_ts": 10, "depth": 0,
                "content": {"users": {bob.as_str(): 100, "@alice:hs1": 0}},
                "prev_events": ["$unknown:remote.example"],
                "auth_events": [create, bob_join],
            }),
        );
        // C: a message at a state that claims X is the power levels.
        let c = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob.as_str(), "room_id": room_id,
                "origin_server_ts": 11, "depth": 10, "content": {"body": "hehehe"},
                "prev_events": ["$unknown:remote.example"],
                "auth_events": [create, power, bob_join],
            }),
        );
        state.retain(|id| id != real_power.as_str());
        state.push(x.event_id().to_string());
        let state_ids: Vec<ruma::OwnedEventId> = state
            .iter()
            .map(|id| ruma::EventId::parse(id).unwrap().to_owned())
            .collect();
        actor
            .accept_prev_event_with_state(c.clone(), &state_ids, vec![x.clone()])
            .unwrap();
        assert!(actor.is_rejected_event(x.event_id()));
        let before_c = actor.state_before_event(c.event_id()).unwrap().unwrap();
        assert!(
            !before_c.state.iter().any(|e| e.event_id() == x.event_id()),
            "the made-up power levels are not in the state"
        );
        // And D, after C, sees the real power levels.
        let d = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob.as_str(), "room_id": room_id,
                "origin_server_ts": 12, "depth": 11, "content": {"body": "D"},
                "prev_events": [c.event_id().as_str()],
                "auth_events": [create, power, bob_join],
            }),
        );
        actor.accept_remote_event(d.clone()).unwrap();
        let current = actor
            .state_event("m.room.power_levels", "")
            .unwrap()
            .unwrap();
        assert_eq!(current.event_id(), &*real_power);
    }
}
