//! Soft failure: an event received over federation that passes the auth rules at the state
//! before it but not at the room's current state, held in the graph and kept from clients.
//!
//! The spec's rationale (server-server API, "Soft failure"): a banned user's server can keep
//! sending events that cite the part of the graph from before the ban. Such an event is valid
//! -- it cannot be told from one that was merely delayed -- so it must be accepted and take part
//! in state resolution and the federation protocol as usual, but nobody here needs to see it.
//! So, as the spec says and Synapse does (`_check_for_soft_fail`), the third of the receipt
//! checks is against the *current state*: the state resolved across every forward extremity,
//! and the state before the event itself (Synapse adds that set too: a gap in the graph could
//! leave this server's view of the current state stale, and a gap is easy to manufacture). An
//! event that fails only that check is **soft failed**:
//!
//! - it is stored, indexed, placed in the timeline and fed to the state store as any event is
//!   ([`RoomActor::persist_with`]), with `EventFlags::is_soft_failed` set in its record, so a
//!   load (and a replica's copy, `catch_up`) finds it so again;
//! - it is **not a forward extremity**, and it supersedes none: nothing this server creates
//!   cites it (the spec's "nor be referenced by new events created by the homeserver"). A later
//!   accepted event that cites it supersedes what it stands on -- the walk through soft-failed
//!   (and rejected) prev events to the extremities beneath them,
//!   [`RoomActor::superseded_extremities`], as Synapse's `_get_prevs_before_rejected`; Sytest's
//!   "Inbound federation correctly handles soft failed events as extremities";
//! - it is **not relayed**: no `RoomUpdate` is published for it, so `/sync`, push, appservice
//!   delivery and the federation sender never hear of it, and every client read of the
//!   timeline and by ID leaves it out ([`RoomActor::event_by_id`], `event_at`, `events_around`,
//!   the pages `/messages` and `/sync` read, `events_after`); it is not in the relations index
//!   and not in the joined-rooms index;
//! - federation still serves it ([`RoomActor::held_event`]: `/event`, `/state` at it), as the
//!   spec asks ("A soft failed event should be returned in response to federation requests").
//!
//! Soft-failed state events take part in state resolution, so one may become part of the
//! current state (the spec's note); the state reads then show it, as they show any state.
//!
//! Only an event received over `/send` is checked ([`RoomActor::accept_remote_event`]):
//! history fetched by backfill or a gap fill is judged at its own position (`history`), as in
//! Synapse (`backfilled` events skip the check), and the Synapse importer's copies
//! (`import_event`, quiet) were judged by Synapse.

use std::collections::HashSet;

use hs_kv::KvBackend;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::ids::EventSn;
use hs_state::auth::{self, IncomingEvent};
use ruma::{EventId, UserId};

use super::{RoomActor, extract_redacts};
use crate::error::RoomError;
use crate::pipeline;

/// How many soft-failed or rejected prev events [`RoomActor::superseded_extremities`] walks
/// through before it stops: a chain that long is an attack, not a room.
const MAX_WALK: usize = 1_000;

impl<B: KvBackend> RoomActor<B> {
    /// The third receipt check for `event`, whose prev events (as held) are `prev_sns` and which
    /// passed the first two ([`RoomActor::authorize_remote`]): `Some(reason)` when the auth rules
    /// refuse it at the room's current state (see the module docs for what that is), `None`
    /// when they allow it, or when its prev events *are* the forward extremities (the current
    /// state is then the state before it, already checked).
    ///
    /// # Errors
    /// [`RoomError::State`] if the state store fails.
    pub(super) fn soft_fail_reason(
        &self,
        event: &Event,
        prev_sns: &[EventSn],
    ) -> Result<Option<String>, RoomError> {
        let effective = self.effective_prev_sns(prev_sns);
        let prev_set: HashSet<EventSn> = effective.iter().copied().collect();
        if prev_set.len() == self.forward_extremities.len()
            && self
                .forward_extremities
                .iter()
                .all(|sn| prev_set.contains(sn))
        {
            return Ok(None);
        }
        let mut at = self.forward_extremities_vec();
        for sn in effective {
            if !at.contains(&sn) {
                at.push(sn);
            }
        }
        let current = self.state_view(&at)?;

        let content = event
            .json()
            .get("content")
            .and_then(CanonicalJsonValue::as_object)
            .cloned()
            .unwrap_or_default();
        let redacts = extract_redacts(event);
        let only_prev_is_create = self.only_prev_is_create(prev_sns);
        let incoming = IncomingEvent {
            event_type: &event.header().event_type,
            sender: AsRef::<UserId>::as_ref(&event.header().sender),
            room_id: Some(&self.room_id),
            state_key: event.header().state_key.as_deref(),
            content: &content,
            prev_event_count: prev_sns.len(),
            only_prev_event_is_room_create: only_prev_is_create,
            event_id: Some(event.event_id()),
            redacts: redacts.as_deref(),
        };
        Ok(
            auth::check_event_auth(&self.rules, &incoming, &current.state_fetch())
                .err()
                .map(|e| e.to_string()),
        )
    }

    /// The forward extremities an accepted event citing `prev_sns` supersedes: the prev events
    /// that are extremities, and -- through every soft-failed or rejected prev event, which is
    /// never an extremity itself -- the extremities those stand on, recursively (up to
    /// [`MAX_WALK`] such events). Otherwise an event citing a soft-failed event would leave the
    /// extremity beneath it dangling for ever.
    pub(super) fn superseded_extremities(&self, prev_sns: &[EventSn]) -> Vec<EventSn> {
        let mut out = Vec::new();
        let mut seen: HashSet<EventSn> = HashSet::new();
        let mut stack: Vec<EventSn> = prev_sns.iter().rev().copied().collect();
        let mut walked = 0usize;
        while let Some(sn) = stack.pop() {
            if !seen.insert(sn) {
                continue;
            }
            if self.forward_extremities.contains(&sn) {
                out.push(sn);
                continue;
            }
            if !(self.soft_failed.contains(&sn) || self.rejected.contains(&sn)) {
                continue;
            }
            walked += 1;
            if walked > MAX_WALK {
                tracing::warn!(
                    room_id = %self.room_id,
                    "a chain of soft-failed or rejected events is longer than this server walks; cut short"
                );
                break;
            }
            if let Some(event) = self.event(sn) {
                for id in pipeline::decode_event_ids(event.json().get("prev_events"))
                    .iter()
                    .rev()
                {
                    if let Some(prev) = self.sn_of(id) {
                        stack.push(prev);
                    }
                }
            }
        }
        out
    }

    /// Whether `event_id` is held here soft failed.
    #[must_use]
    pub fn is_soft_failed_event(&self, event_id: &EventId) -> bool {
        self.sn_of(event_id)
            .is_some_and(|sn| self.soft_failed.contains(&sn))
    }

    /// The event `event_id`, if this actor holds it in a form another *server* may be given: as
    /// [`RoomActor::event_by_id`], soft-failed events included (the spec: "A soft failed event
    /// should be returned in response to federation requests"); purged and rejected events are
    /// still hidden. What federation's `/event`, `/state` and `/state_ids` read.
    #[must_use]
    pub fn held_event(&self, event_id: &EventId) -> Option<&Event> {
        let sn = self.sn_of(event_id)?;
        if self.purged.contains(&sn) || self.rejected.contains(&sn) {
            return None;
        }
        self.event(sn)
    }

    /// Whether the event at `sn` is kept out of client reads: soft failed.
    pub(super) fn hidden_from_clients(&self, sn: EventSn) -> bool {
        self.soft_failed.contains(&sn)
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
    use crate::timeline::Direction;

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

    fn extremity_ids(actor: &RoomActor<MemoryBackend>) -> Vec<String> {
        let mut ids: Vec<String> = actor
            .forward_extremity_ids()
            .into_iter()
            .map(|(id, _)| id.to_string())
            .collect();
        ids.sort();
        ids
    }

    fn sorted(mut ids: Vec<String>) -> Vec<String> {
        ids.sort();
        ids
    }

    /// Sytest's three soft-failure tests (`52soft-fail.pl`) in one graph:
    ///
    /// ```text
    ///        J            J  = bob's join
    ///       / \           PL = alice raises `test.sf` to 50
    ///     PL   M1         M1 = bob's message citing J: allowed
    ///     |    |          SF1, SF2 = bob's `test.sf` citing M1, SF1: allowed at the state
    ///     |   SF1                before them, refused at the current state -- soft failed
    ///     |    |          M2 = bob's message citing PL and SF2: accepted
    ///     |   SF2
    ///      \   /
    ///       M2
    /// ```
    ///
    /// SF1 and SF2 are held, out of every client read, served to federation, not extremities
    /// (so PL and M1 stay the extremities after each: "accepts a second soft-failed event"),
    /// and M2 supersedes PL and, through SF2 and SF1, M1 ("handles soft failed events as
    /// extremities"); the next local event cites M2 alone. All of it holds after a reload.
    #[test]
    fn soft_failed_events_are_held_hidden_and_never_extremities() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1");
        let bob: &UserId = user_id!("@bob:remote.example");
        let key = signing::SigningKeyPair::generate("1");
        let room_id = actor.room_id().to_string();
        let create = state_id(&actor, "m.room.create", "");
        let power = state_id(&actor, "m.room.power_levels", "");
        let join_rules = state_id(&actor, "m.room.join_rules", "");
        let (head, head_depth) = actor.forward_extremity_ids().remove(0);

        let j = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.member", "state_key": bob.as_str(), "sender": bob.as_str(),
                "room_id": room_id, "origin_server_ts": 10, "depth": head_depth + 1,
                "content": {"membership": "join"},
                "prev_events": [head.as_str()],
                "auth_events": [create, power, join_rules],
            }),
        );
        assert!(matches!(
            actor.accept_remote_event(j.clone()).unwrap(),
            RemoteEventOutcome::Stored(_)
        ));
        let j_id = j.event_id().to_string();

        let mut levels: serde_json::Value = serde_json::from_slice(
            &CanonicalJsonValue::Object(
                actor
                    .state_event("m.room.power_levels", "")
                    .unwrap()
                    .unwrap()
                    .json()
                    .get("content")
                    .and_then(CanonicalJsonValue::as_object)
                    .cloned()
                    .unwrap(),
            )
            .to_canonical_bytes(),
        )
        .unwrap();
        levels["events"]["test.sf"] = serde_json::json!(50);
        let pl = actor
            .send_event(
                alice.to_owned(),
                "m.room.power_levels".to_owned(),
                Some(String::new()),
                levels,
                None,
                20,
            )
            .unwrap();
        let pl_id = pl.event_id().to_string();
        let message = |body: &str, event_type: &str, prev: Vec<&str>, power: &str, depth: i64| {
            remote_pdu(
                &key,
                serde_json::json!({
                    "type": event_type, "sender": bob.as_str(), "room_id": room_id,
                    "origin_server_ts": 30 + depth, "depth": depth,
                    "content": {"body": body},
                    "prev_events": prev,
                    "auth_events": [create, power, j_id],
                }),
            )
        };

        let m1 = message("M1", "m.room.message", vec![&j_id], &power, head_depth + 2);
        assert!(matches!(
            actor.accept_remote_event(m1.clone()).unwrap(),
            RemoteEventOutcome::Stored(_)
        ));
        let m1_id = m1.event_id().to_string();
        assert_eq!(
            extremity_ids(&actor),
            sorted(vec![pl_id.clone(), m1_id.clone()])
        );

        let before = crate::metrics::soft_failed_events();
        let sf1 = message("SF1", "test.sf", vec![&m1_id], &power, head_depth + 3);
        assert!(matches!(
            actor.accept_remote_event(sf1.clone()).unwrap(),
            RemoteEventOutcome::SoftFailed(_)
        ));
        let sf1_id = sf1.event_id().to_string();
        let sf2 = message("SF2", "test.sf", vec![&sf1_id], &power, head_depth + 4);
        assert!(matches!(
            actor.accept_remote_event(sf2.clone()).unwrap(),
            RemoteEventOutcome::SoftFailed(_)
        ));
        assert!(crate::metrics::soft_failed_events() >= before + 2);
        // Sent again, already known.
        assert!(matches!(
            actor.accept_remote_event(sf1.clone()).unwrap(),
            RemoteEventOutcome::AlreadyKnown
        ));
        // "accepts a second soft-failed event": PL and M1 are still the extremities.
        assert_eq!(
            extremity_ids(&actor),
            sorted(vec![pl_id.clone(), m1_id.clone()])
        );
        let check_hidden = |actor: &RoomActor<MemoryBackend>| {
            for sf in [&sf1, &sf2] {
                assert!(actor.event_by_id(sf.event_id()).is_none());
                assert!(actor.held_event(sf.event_id()).is_some());
                assert!(actor.is_soft_failed_event(sf.event_id()));
            }
            let (page, _) = actor.paginate(None, Direction::Backward, 20);
            assert!(
                !page.iter().any(|e| e.header().event_type == "test.sf"),
                "a soft-failed event is in /messages"
            );
            let after: Vec<String> = actor
                .events_after(0, 100)
                .into_iter()
                .map(|(_, e)| e.header().event_type.clone())
                .collect();
            assert!(!after.iter().any(|t| t == "test.sf"), "{after:?}");
            assert_ne!(
                actor.head_update().unwrap().event_type,
                "test.sf",
                "the head a client is told of is never soft failed"
            );
        };
        check_hidden(&actor);

        // "handles soft failed events as extremities": M2 cites PL and SF2.
        let m2 = message(
            "M2",
            "m.room.message",
            vec![&pl_id, sf2.event_id().as_str()],
            &pl_id,
            head_depth + 5,
        );
        assert!(matches!(
            actor.accept_remote_event(m2.clone()).unwrap(),
            RemoteEventOutcome::Stored(_)
        ));
        let m2_id = m2.event_id().to_string();
        assert_eq!(extremity_ids(&actor), vec![m2_id.clone()]);
        let m3 = actor
            .send_event(
                alice.to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "m3"}),
                None,
                99,
            )
            .unwrap();
        let cited: Vec<String> = crate::pipeline::decode_event_ids(m3.json().get("prev_events"))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(cited, vec![m2_id.clone()]);
        check_hidden(&actor);

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
        check_hidden(&reloaded);
        assert_eq!(extremity_ids(&reloaded), vec![m3.event_id().to_string()]);
    }

    /// Sytest's "Inbound federation correctly soft fails events": C (a message citing J, after
    /// PL raised `m.room.message`) is soft failed; D (another type, citing C and PL) is
    /// accepted and is the only extremity.
    #[test]
    fn an_event_citing_a_soft_failed_one_is_accepted_and_supersedes_what_it_stands_on() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1");
        let bob: &UserId = user_id!("@bob:remote.example");
        let key = signing::SigningKeyPair::generate("1");
        let room_id = actor.room_id().to_string();
        let create = state_id(&actor, "m.room.create", "");
        let power = state_id(&actor, "m.room.power_levels", "");
        let join_rules = state_id(&actor, "m.room.join_rules", "");
        let (head, depth) = actor.forward_extremity_ids().remove(0);
        let j = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.member", "state_key": bob.as_str(), "sender": bob.as_str(),
                "room_id": room_id, "origin_server_ts": 10, "depth": depth + 1,
                "content": {"membership": "join"},
                "prev_events": [head.as_str()],
                "auth_events": [create, power, join_rules],
            }),
        );
        actor.accept_remote_event(j.clone()).unwrap();
        let mut levels: serde_json::Value = serde_json::json!({
            "users": {"@alice:hs1": 100},
            "events": {"m.room.message": 50},
        });
        levels["users_default"] = serde_json::json!(0);
        let pl = actor
            .send_event(
                alice.to_owned(),
                "m.room.power_levels".to_owned(),
                Some(String::new()),
                levels,
                None,
                20,
            )
            .unwrap();
        let c = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.message", "sender": bob.as_str(), "room_id": room_id,
                "origin_server_ts": 30, "depth": depth + 2, "content": {"body": "Denied"},
                "prev_events": [j.event_id().as_str()],
                "auth_events": [create, power, j.event_id().as_str()],
            }),
        );
        assert!(matches!(
            actor.accept_remote_event(c.clone()).unwrap(),
            RemoteEventOutcome::SoftFailed(_)
        ));
        let d = remote_pdu(
            &key,
            serde_json::json!({
                "type": "m.room.other_message_type", "sender": bob.as_str(), "room_id": room_id,
                "origin_server_ts": 31, "depth": depth + 3, "content": {"body": "Allowed"},
                "prev_events": [c.event_id().as_str(), pl.event_id().as_str()],
                "auth_events": [create, pl.event_id().as_str(), j.event_id().as_str()],
            }),
        );
        assert!(matches!(
            actor.accept_remote_event(d.clone()).unwrap(),
            RemoteEventOutcome::Stored(_)
        ));
        assert_eq!(extremity_ids(&actor), vec![d.event_id().to_string()]);
        let (page, _) = actor.paginate(None, Direction::Backward, 3);
        assert_eq!(page[0].event_id(), d.event_id());
        assert_eq!(page[1].event_id(), pl.event_id());
        assert!(actor.event_by_id(c.event_id()).is_none());
    }
}
