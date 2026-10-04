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
