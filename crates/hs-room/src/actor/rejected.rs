//! Events another server sent that event authorization rejected: stored, flagged, and never
//! shown.
//!
//! A PDU that fails the auth rules (`RoomActor::accept_remote_event`) is not thrown away, as it
//! was until 2026-10-01, but kept the way Synapse keeps one: its record is written with
//! `hs_model::event::EventFlags::is_rejected` set, no timeline position, and a row in
//! `Tables::outliers` so [`RoomActor::load`] finds it again. It has no place in the room: it is
//! not in the timeline, not a forward extremity, not fed to the state store, and every read hides
//! it ([`RoomActor::event_by_id`]; federation's `/event` answers `404` for it as Synapse does).
//! What keeping it buys is that a later event citing it is answered consistently, not with
//! "missing ancestors" and a fetch that brings back the same rejected event:
//!
//! - cited in `prev_events`, it stands for its own `prev_events` -- the state after a rejected
//!   event is the state before it ([`RoomActor::effective_prev_sns`]);
//! - cited in `auth_events`, it makes the citing event rejected too (the auth rules refuse an
//!   event whose auth event was rejected), which is stored the same way;
//! - sent again, it is already known (`{}` in `/send`).
//!
//! Soft failure is not this: an event that passes its own `auth_events` and the state before
//! it but not the room's current state is held and placed, and kept from clients
//! (`soft_fail`).

use std::collections::HashSet;

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_model::Event;
use hs_model::ids::EventSn;
use ruma::EventId;

use super::{RoomActor, to_kv};
use crate::error::RoomError;
use crate::persist::PersistedEvent;
use crate::pipeline;

/// How many rejected events [`RoomActor::effective_prev_sns`] walks through before it stops: a
/// chain of rejected events that long is an attack, not a room.
const MAX_REJECTED_WALK: usize = 1_000;

impl<B: KvBackend> RoomActor<B> {
    /// Stores `event`, which event authorization rejected for `reason`, as rejected: see the
    /// module doc. An event already held is left as it is.
    ///
    /// # Errors
    /// [`RoomError::Store`] or [`RoomError::Fenced`] from the write.
    pub(super) fn store_rejected(
        &mut self,
        mut event: Event,
        reason: &str,
    ) -> Result<(), RoomError> {
        if self.event_id_index.contains_key(event.event_id()) {
            return Ok(());
        }
        event.flags_mut().set_rejected(true);
        let json: serde_json::Value = serde_json::from_slice(event.canonical_bytes())
            .map_err(|e| RoomError::Internal(e.to_string()))?;
        let persisted = PersistedEvent {
            room_id: self.room_id.to_string(),
            json,
            room_version: self.room_version.as_str().to_owned(),
            flags: event.header().flags.to_byte(),
            room_pos: None,
            purged: false,
        };
        let bytes =
            serde_json::to_vec(&persisted).map_err(|e| RoomError::Internal(e.to_string()))?;
        let event_id_bytes = event.event_id().as_bytes().to_vec();
        let room_sn = self.room_sn;
        let fence_failure: std::cell::Cell<Option<String>> = std::cell::Cell::new(None);
        let sn = transact(&self.backend, TransactConfig::default(), |txn| {
            let sn = self.tables.event_sn.get_or_create(txn, &event_id_bytes)?;
            self.tables.events.put(txn, &(sn,), &bytes).map_err(to_kv)?;
            self.tables
                .outliers
                .put(txn, &(room_sn, sn), b"")
                .map_err(to_kv)?;
            self.fence_check(txn, &fence_failure)?;
            Ok(sn)
        })
        .map_err(|e| match fence_failure.take() {
            Some(msg) => RoomError::Fenced(msg),
            None => RoomError::from(e),
        })?;
        tracing::info!(
            room_id = %self.room_id,
            event_id = %event.event_id(),
            sender = %event.header().sender,
            reason,
            "stored an event as rejected: event authorization refused it"
        );
        self.absorb_rejected(sn, event);
        Ok(())
    }

    /// Holds a rejected event in memory (on load, or right after [`RoomActor::store_rejected`]
    /// wrote it): indexed by ID so a later reference finds it, hidden from every read, and
    /// nowhere else.
    pub(super) fn absorb_rejected(&mut self, sn: EventSn, event: Event) {
        self.event_id_index.insert(event.event_id().to_owned(), sn);
        self.rejected.insert(sn);
        self.events.insert(sn, event);
    }

    /// The first of `ids` this server holds in a room other than this one, with that room's ID.
    ///
    /// An event citing an event of another room -- in `auth_events` (Sytest's "Events whose
    /// auth_events are in the wrong room do not mess up the room state") or in `prev_events` --
    /// is refused, not fetched for: Synapse's auth rules reject an auth event of another room
    /// ("found event ... in the state which is in room ..."), and a fetch would only bring back
    /// the event this server already holds, or another server's copy, for a room it is not of.
    /// Before 2026-10-04 such an event was answered "missing ancestors" and a backfill asked
    /// the sending server for the other room's event.
    ///
    /// # Errors
    /// [`RoomError::Store`] or [`RoomError::Internal`] reading the event store.
    pub(super) fn held_in_another_room(
        &self,
        ids: &[ruma::OwnedEventId],
    ) -> Result<Option<(ruma::OwnedEventId, String)>, RoomError> {
        for id in ids {
            if let Some(row) = super::find_event_globally(&self.backend, &self.tables, id)?
                && row.room_id != self.room_id.as_str()
            {
                return Ok(Some((id.clone(), row.room_id)));
            }
        }
        Ok(None)
    }

    /// Whether `event_id` is held here as an event authorization rejected.
    #[must_use]
    pub fn is_rejected_event(&self, event_id: &EventId) -> bool {
        self.event_id_index
            .get(event_id)
            .is_some_and(|sn| self.rejected.contains(sn))
    }

    /// `prev_sns` with every rejected event replaced by its own `prev_events` (recursively, up
    /// to [`MAX_REJECTED_WALK`] rejected events): what the state before an event that cites a
    /// rejected one is computed from, since the state after a rejected event is the state before
    /// it. The input unchanged when it names no rejected event, as it almost always does.
    pub(super) fn effective_prev_sns(&self, prev_sns: &[EventSn]) -> Vec<EventSn> {
        if !prev_sns.iter().any(|sn| self.rejected.contains(sn)) {
            return prev_sns.to_vec();
        }
        let mut out = Vec::new();
        let mut seen: HashSet<EventSn> = HashSet::new();
        let mut stack: Vec<EventSn> = prev_sns.iter().rev().copied().collect();
        let mut walked = 0usize;
        while let Some(sn) = stack.pop() {
            if !seen.insert(sn) {
                continue;
            }
            if !self.rejected.contains(&sn) {
                out.push(sn);
                continue;
            }
            walked += 1;
            if walked > MAX_REJECTED_WALK {
                tracing::warn!(
                    room_id = %self.room_id,
                    "a chain of rejected events is longer than this server walks; cut short"
                );
                break;
            }
            if let Some(rejected) = self.events.get(&sn) {
                let prevs = pipeline::decode_event_ids(rejected.json().get("prev_events"));
                for id in prevs.iter().rev() {
                    if let Some(&prev) = self.event_id_index.get(id) {
                        stack.push(prev);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;
    use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
    use hs_model::{Event, hash, signing};
    use ruma::{RoomVersionId, UserId, user_id};

    use super::super::{CreateRoomRequest, RemoteEventOutcome, RoomActor};
    use crate::error::RoomError;
    use crate::identity::HomeserverIdentity;
    use crate::persist::Tables;

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

    fn public_room(backend: &MemoryBackend) -> RoomActor<MemoryBackend> {
        RoomActor::create_room(
            backend.clone(),
            Tables::open(backend).unwrap(),
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

    fn state_id(actor: &RoomActor<MemoryBackend>, event_type: &str, state_key: &str) -> String {
        actor
            .state_event(event_type, state_key)
            .unwrap()
            .unwrap()
            .event_id()
            .to_string()
    }

    /// Two public rooms on one store, with bob of `remote.example` joined to both; returns
    /// them and bob's join in each.
    fn two_rooms_with_bob(
        key: &signing::SigningKeyPair,
    ) -> (
        RoomActor<MemoryBackend>,
        RoomActor<MemoryBackend>,
        String,
        String,
    ) {
        let backend = MemoryBackend::new();
        let mut one = public_room(&backend);
        let mut two = public_room(&backend);
        let bob: &UserId = user_id!("@bob:remote.example");
        let mut joins = Vec::new();
        for actor in [&mut one, &mut two] {
            let (head, depth) = actor.forward_extremity_ids().remove(0);
            let join = remote_pdu(
                key,
                serde_json::json!({
                    "type": "m.room.member", "state_key": bob.as_str(), "sender": bob.as_str(),
                    "room_id": actor.room_id().as_str(), "origin_server_ts": 10,
                    "depth": depth + 1, "content": {"membership": "join"},
                    "prev_events": [head.as_str()],
                    "auth_events": [
                        state_id(actor, "m.room.create", ""),
                        state_id(actor, "m.room.power_levels", ""),
                        state_id(actor, "m.room.join_rules", ""),
                    ],
                }),
            );
            joins.push(join.event_id().to_string());
            assert!(matches!(
                actor.accept_remote_event(join).unwrap(),
                RemoteEventOutcome::Stored(_)
            ));
        }
        let two_join = joins.pop().unwrap();
        let one_join = joins.pop().unwrap();
        (one, two, one_join, two_join)
    }

    /// Sytest's "Events whose auth_events are in the wrong room do not mess up the room state":
    /// an event of room two citing bob's join to room one among its `auth_events` is rejected
    /// -- stored as such, `{}` to `/send` -- not answered "missing ancestors" (which made the
    /// server ask the sender for room one's event), and room two's state keeps bob's own join.
    /// The same for a `prev_events` entry of another room.
    #[test]
    fn an_event_citing_an_event_of_another_room_is_rejected_not_fetched_for() {
        let key = signing::SigningKeyPair::generate("1");
        let (_one, mut two, one_join, two_join) = two_rooms_with_bob(&key);
        let bob = "@bob:remote.example";
        let (head, depth) = two.forward_extremity_ids().remove(0);
        let dodgy_auth = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob, "room_id": two.room_id().as_str(),
                "origin_server_ts": 20, "depth": depth + 1, "content": {"body": "event P"},
                "prev_events": [head.as_str()],
                "auth_events": [
                    state_id(&two, "m.room.create", ""),
                    state_id(&two, "m.room.power_levels", ""),
                    one_join,
                ],
            }),
        );
        let dodgy_id = dodgy_auth.event_id().to_owned();
        match two.accept_remote_event(dodgy_auth) {
            Err(RoomError::Forbidden(reason)) => {
                assert!(reason.contains("another room"), "{reason}")
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(two.is_rejected_event(&dodgy_id));
        assert_eq!(state_id(&two, "m.room.member", bob), two_join);

        let dodgy_prev = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob, "room_id": two.room_id().as_str(),
                "origin_server_ts": 21, "depth": depth + 1, "content": {"body": "event Q"},
                "prev_events": [one_join],
                "auth_events": [
                    state_id(&two, "m.room.create", ""),
                    state_id(&two, "m.room.power_levels", ""),
                    two_join,
                ],
            }),
        );
        assert!(matches!(
            two.accept_remote_event(dodgy_prev),
            Err(RoomError::Forbidden(_))
        ));
        // An ancestor nobody holds is still missing, and fetched for.
        let unknown = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob, "room_id": two.room_id().as_str(),
                "origin_server_ts": 22, "depth": depth + 1, "content": {"body": "event R"},
                "prev_events": ["$nobody-has-this"],
                "auth_events": [
                    state_id(&two, "m.room.create", ""),
                    state_id(&two, "m.room.power_levels", ""),
                    two_join,
                ],
            }),
        );
        assert!(matches!(
            two.accept_remote_event(unknown),
            Err(RoomError::MissingAncestors(_))
        ));
    }

    /// Sytest's "An event which redacts an event in a different room should be ignored": the
    /// redaction is held (soft-failed: in the graph, served over federation) and kept out of
    /// every client read of its own room; the other room's event keeps its content.
    #[test]
    fn a_redaction_of_an_event_of_another_room_is_withheld() {
        let key = signing::SigningKeyPair::generate("1");
        let (mut one, mut two, one_join, two_join) = two_rooms_with_bob(&key);
        let bob = "@bob:remote.example";
        let (head, depth) = one.forward_extremity_ids().remove(0);
        let message = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob, "room_id": one.room_id().as_str(),
                "origin_server_ts": 20, "depth": depth + 1, "content": {"body": "hi"},
                "prev_events": [head.as_str()],
                "auth_events": [
                    state_id(&one, "m.room.create", ""),
                    state_id(&one, "m.room.power_levels", ""),
                    one_join,
                ],
            }),
        );
        let message_id = message.event_id().to_owned();
        one.accept_remote_event(message).unwrap();

        let (head, depth) = two.forward_extremity_ids().remove(0);
        let redaction = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.redaction", "sender": bob, "room_id": two.room_id().as_str(),
                "origin_server_ts": 21, "depth": depth + 1, "content": {},
                "redacts": message_id.as_str(),
                "prev_events": [head.as_str()],
                "auth_events": [
                    state_id(&two, "m.room.create", ""),
                    state_id(&two, "m.room.power_levels", ""),
                    two_join,
                ],
            }),
        );
        let redaction_id = redaction.event_id().to_owned();
        assert!(matches!(
            two.accept_remote_event(redaction).unwrap(),
            RemoteEventOutcome::SoftFailed(_)
        ));
        assert!(two.event_by_id(&redaction_id).is_none(), "shown to clients");
        assert!(
            two.held_event(&redaction_id).is_some(),
            "not held for federation"
        );
        let original = one.event_by_id(&message_id).unwrap();
        assert!(!original.header().flags.is_redacted());
    }
}
