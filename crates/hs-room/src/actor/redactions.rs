//! Redactions: applying an `m.room.redaction` to the event it names whenever the two meet, and
//! keeping which redaction did it, so that every reader shows it.
//!
//! # Which redaction did it
//! A redacted event carries `unsigned.redacted_because` (the redaction event) for a client, and
//! Synapse's older `unsigned.redacted_by` (its ID); Sytest checks the second, Element reads the
//! first. Every client read renders an event through `crate::routes::render::client_event_json`
//! from the [`Event`] alone -- `/sync`, `/messages`, `/event`, `/context`, `/state`, search,
//! relations, threads, appservice delivery -- so the redaction is kept *in the redacted event*:
//! [`RoomActor::apply_redaction_by`] writes the redaction's PDU and ID into the target's
//! `unsigned` (covered by neither its hashes, its signatures nor its reference hash, so its ID
//! and every check on it are unchanged), durably and in memory, and the renderer shows them as a
//! client event. Federation never sees them: a redacted event is served to another server in its
//! redacted form, which keeps no `unsigned` at all (`hs_cli::federation`'s `full_pdu`), as
//! Synapse strips both from a PDU. Until 2026-10-01 nothing rendered either.
//!
//! # A redaction that arrives first
//! A redaction can arrive before the event it redacts (over federation, out of order; or a
//! local redaction of an event this server has not received yet). Every redaction held is
//! indexed by the event it names (`RoomActor::redactions_by_target`), rebuilt from the stored
//! redactions on load -- so the index is as durable as the redactions themselves, with no table
//! of its own. When an event is stored, through `/send`, backfill or a gap fill, the redactions
//! waiting for it are applied ([`RoomActor::apply_waiting_redactions`]) under the same rule a
//! redaction received for an event already held is: its sender is on the original sender's
//! server, or may redact it ([`RoomActor::may_redact`]). Until 2026-10-01 such a redaction was
//! stored and never applied.

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue};
use hs_model::ids::EventSn;
use ruma::{EventId, OwnedEventId, UserId};

use super::{RoomActor, extract_redacts, to_kv};
use crate::error::RoomError;
use crate::persist::PersistedEvent;

/// The `unsigned` key holding the ID of the redaction that redacted an event (Synapse's name,
/// which Sytest reads).
pub const REDACTED_BY: &str = "redacted_by";
/// The `unsigned` key holding the redaction event itself, as the spec names it.
pub const REDACTED_BECAUSE: &str = "redacted_because";

impl<B: KvBackend> RoomActor<B> {
    /// Indexes the event stored under `event_sn` by the event it redacts, if it is a redaction.
    pub(super) fn note_redaction_at(&mut self, event_sn: EventSn) {
        let Some(event) = self.events.get(&event_sn) else {
            return;
        };
        if event.header().event_type != "m.room.redaction" {
            return;
        }
        let Some(target) = extract_redacts(event) else {
            return;
        };
        let redaction = event.event_id().to_owned();
        let waiting = self.redactions_by_target.entry(target).or_default();
        if !waiting.contains(&redaction) {
            waiting.push(redaction);
        }
    }

    /// The redactions held that name `target`, oldest first.
    #[must_use]
    pub fn redactions_of(&self, target: &EventId) -> &[OwnedEventId] {
        self.redactions_by_target
            .get(target)
            .map_or(&[], Vec::as_slice)
    }

    /// Applies the redactions that were held before `event_id` was: each one that may take
    /// effect ([`RoomActor::redaction_may_take_effect`]), the first of them being the one shown.
    /// Called once an event is stored; a no-op for the usual event, which nothing waits for.
    pub(super) fn apply_waiting_redactions(&mut self, event_id: &EventId) {
        let waiting: Vec<OwnedEventId> = self.redactions_of(event_id).to_vec();
        for redaction_id in waiting {
            let Some(sender) = self
                .event_by_id(&redaction_id)
                .map(|redaction| redaction.header().sender.clone())
            else {
                continue;
            };
            if !self.redaction_may_take_effect(&sender, event_id) {
                tracing::info!(
                    room_id = %self.room_id,
                    redaction = %redaction_id,
                    target = %event_id,
                    sender = %sender,
                    "a redaction that arrived before its event is not allowed to take effect; left unapplied"
                );
                continue;
            }
            match self.apply_redaction_by(event_id, &redaction_id) {
                Ok(()) => tracing::info!(
                    room_id = %self.room_id,
                    redaction = %redaction_id,
                    target = %event_id,
                    "applied a redaction that arrived before the event it redacts"
                ),
                Err(error) => tracing::warn!(
                    room_id = %self.room_id,
                    redaction = %redaction_id,
                    target = %event_id,
                    %error,
                    "could not apply a redaction that arrived before the event it redacts"
                ),
            }
        }
    }

    /// Whether a redaction by `sender` of `target` (held) may take effect: the sender is on the
    /// original sender's server (the spec's rule from room version 3, under which the auth rules
    /// admit any member's redaction and leave this check to whoever applies it), or
    /// [`RoomActor::may_redact`] allows it (their own event, or the room's redact power level,
    /// read from the current power levels).
    pub(super) fn redaction_may_take_effect(&self, sender: &UserId, target: &EventId) -> bool {
        let Some(original) = self.event_by_id(target) else {
            return false;
        };
        original.header().sender.server_name() == sender.server_name()
            || self.may_redact(sender, target).unwrap_or(false)
    }

    /// Applies a redaction received from another server to the event it names, when this room
    /// holds that event and the redaction may take effect
    /// ([`RoomActor::redaction_may_take_effect`]). Before 2026-10-01 a redaction arriving over
    /// federation was stored and never applied, so the redacted message kept its content here.
    /// One whose target is not held waits for it ([`RoomActor::apply_waiting_redactions`]); one
    /// that may not take effect is stored and left unapplied. Both are logged, not errors, since
    /// the redaction event itself was accepted.
    pub(super) fn apply_received_redaction(
        &mut self,
        sender: &UserId,
        target: &EventId,
        redaction_id: &EventId,
    ) {
        if self.event_by_id(target).is_none() {
            tracing::debug!(
                room_id = %self.room_id,
                redaction = %redaction_id,
                target = %target,
                "a received redaction names an event this room does not hold yet; it waits for it"
            );
            return;
        }
        if !self.redaction_may_take_effect(sender, target) {
            tracing::info!(
                room_id = %self.room_id,
                redaction = %redaction_id,
                target = %target,
                sender = %sender,
                "a received redaction is not allowed to take effect; stored, not applied"
            );
            return;
        }
        if let Err(error) = self.apply_redaction_by(target, redaction_id) {
            tracing::warn!(
                room_id = %self.room_id,
                redaction = %redaction_id,
                target = %target,
                %error,
                "could not apply a received redaction"
            );
        }
    }

    /// Marks an event redacted, naming the newest redaction held for it (if any) as the one
    /// that did it -- [`RoomActor::apply_redaction_by`] for a caller that knows only the target
    /// (the Synapse importer, which applies the redactions it copies itself). Does not itself
    /// authorize the redaction.
    ///
    /// # Errors
    /// See [`RoomActor::apply_redaction_by`].
    pub fn apply_redaction(&mut self, target: &EventId) -> Result<(), RoomError> {
        match self.redactions_of(target).last().cloned() {
            Some(redaction_id) => self.apply_redaction_by(target, &redaction_id),
            None => self.mark_redacted(target, None),
        }
    }

    /// Marks `target` redacted (`hs_model::event::EventFlags::REDACTED`) by the held redaction
    /// `redaction_id`, and rewrites its stored record: the flag, and the redaction's ID and PDU
    /// in its `unsigned` (`redacted_by`, `redacted_because`; see the module doc). An event
    /// already redacted with a redaction named keeps it: the first redaction to take effect is
    /// the one shown. A redaction too large to fit into the event (both together over the PDU
    /// size limit) is applied without being named, and logged. Does not itself authorize the
    /// redaction -- callers check first.
    ///
    /// # Errors
    /// Returns [`RoomError::EventNotFound`] if `target` is not held by this actor, or
    /// [`RoomError::Store`] on a storage failure.
    pub fn apply_redaction_by(
        &mut self,
        target: &EventId,
        redaction_id: &EventId,
    ) -> Result<(), RoomError> {
        let event = self
            .event_by_id(target)
            .ok_or_else(|| RoomError::EventNotFound(target.to_string()))?;
        if event.header().flags.is_redacted() && redacted_by(event).is_some() {
            return Ok(());
        }
        let rewritten = match self.event_by_id(redaction_id) {
            Some(redaction) => match with_redaction(event, redaction) {
                Ok(rewritten) => Some(rewritten),
                Err(error) => {
                    tracing::warn!(
                        room_id = %self.room_id,
                        target = %target,
                        redaction = %redaction_id,
                        %error,
                        "a redaction does not fit into the event it redacts; applied without naming it"
                    );
                    None
                }
            },
            None => None,
        };
        self.mark_redacted(target, rewritten)
    }

    /// Sets the redacted flag on `target` and, when given, replaces it with `rewritten` (the same
    /// event with the redaction in its `unsigned`, and that event's JSON), in the store and in
    /// memory.
    fn mark_redacted(
        &mut self,
        target: &EventId,
        rewritten: Option<(Event, serde_json::Value)>,
    ) -> Result<(), RoomError> {
        let sn = *self
            .event_id_index
            .get(target)
            .ok_or_else(|| RoomError::EventNotFound(target.to_string()))?;
        let mut flags = self
            .events
            .get(&sn)
            .ok_or_else(|| RoomError::EventNotFound(target.to_string()))?
            .header()
            .flags;
        flags.set_redacted(true);
        let new_json = rewritten.as_ref().map(|(_, json)| json);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.tables.events.get(txn, &(sn,)).map_err(to_kv)? else {
                return Ok(());
            };
            let mut persisted: PersistedEvent =
                serde_json::from_slice(&bytes).map_err(hs_kv::KvError::backend)?;
            persisted.flags = flags.to_byte();
            if let Some(json) = new_json {
                persisted.json = json.clone();
            }
            let bytes = serde_json::to_vec(&persisted).map_err(hs_kv::KvError::backend)?;
            self.tables.events.put(txn, &(sn,), &bytes).map_err(to_kv)?;
            Ok(())
        })
        .map_err(RoomError::from)?;
        match rewritten {
            Some((mut event, _)) => {
                *event.flags_mut() = flags;
                self.events.insert(sn, event);
            }
            None => {
                if let Some(event) = self.events.get_mut(&sn) {
                    *event.flags_mut() = flags;
                }
            }
        }
        Ok(())
    }

    /// On load: an event redacted before redactions were kept in it (2026-10-01) is given, in
    /// memory, the first redaction held for it, so it renders `redacted_because` like any other.
    /// Nothing is written: a load may run on a replica that does not own the room.
    pub(super) fn name_redactions_on_load(&mut self) {
        let targets: Vec<(OwnedEventId, OwnedEventId)> = self
            .redactions_by_target
            .iter()
            .filter_map(|(target, redactions)| Some((target.clone(), redactions.first()?.clone())))
            .collect();
        for (target, redaction_id) in targets {
            let Some(&sn) = self.event_id_index.get(&target) else {
                continue;
            };
            let Some(event) = self.events.get(&sn) else {
                continue;
            };
            if !event.header().flags.is_redacted() || redacted_by(event).is_some() {
                continue;
            }
            let Some(redaction) = self.event_by_id(&redaction_id) else {
                continue;
            };
            if let Ok((mut named, _)) = with_redaction(event, redaction) {
                *named.flags_mut() = event.header().flags;
                self.events.insert(sn, named);
            }
        }
    }
}

/// The ID in `event`'s `unsigned.redacted_by`, if a redaction has been named in it.
#[must_use]
pub fn redacted_by(event: &Event) -> Option<&str> {
    event
        .json()
        .get("unsigned")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|unsigned| unsigned.get(REDACTED_BY))
        .and_then(CanonicalJsonValue::as_str)
}

/// `target` with `redaction` named in its `unsigned`: `redacted_by` its ID, `redacted_because`
/// its PDU (without the redaction's own `unsigned`). Parsed again, and checked to be the same
/// event.
fn with_redaction(
    target: &Event,
    redaction: &Event,
) -> Result<(Event, serde_json::Value), RoomError> {
    let mut json = target.json().clone();
    let mut because = redaction.json().clone();
    because.remove("unsigned");
    let mut unsigned = match json.remove("unsigned") {
        Some(CanonicalJsonValue::Object(unsigned)) => unsigned,
        _ => CanonicalJsonObject::new(),
    };
    unsigned.insert(
        REDACTED_BY.to_owned(),
        CanonicalJsonValue::String(redaction.event_id().to_string()),
    );
    unsigned.insert(
        REDACTED_BECAUSE.to_owned(),
        CanonicalJsonValue::Object(because),
    );
    json.insert("unsigned".to_owned(), CanonicalJsonValue::Object(unsigned));
    let value: serde_json::Value =
        serde_json::from_slice(&CanonicalJsonValue::Object(json).to_canonical_bytes())
            .map_err(|e| RoomError::Internal(e.to_string()))?;
    let parsed = Event::parse(&value, target.header().room_version.clone())?;
    if parsed.event_id() != target.event_id() {
        return Err(RoomError::Internal(format!(
            "naming a redaction in {} changed its ID to {}",
            target.event_id(),
            parsed.event_id()
        )));
    }
    Ok((parsed, value))
}
