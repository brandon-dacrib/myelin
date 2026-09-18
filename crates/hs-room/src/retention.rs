//! Retention and purge: the seam this pass leaves open.
//!
//! `PLAN.md`'s per-room `m.room.retention` policy and admin-triggered purge (deleting events
//! older than a cutoff, or a whole room's history, from the timeline, event store and chain-cover
//! index) is listed in this track's brief as a Phase 1/2 deliverable, after the endpoints in
//! `crate::routes` land. Nothing in `crate::actor::RoomActor`'s persistence pipeline stops a purge
//! from being implemented later as a separate transaction that removes
//! [`crate::persist::Tables::timeline`] entries and the corresponding
//! [`crate::persist::Tables::events`] rows for a room-position range, and updates backward
//! extremities to the new oldest retained event -- that is the intended shape, recorded here so
//! the next pass over this crate does not have to re-derive it.
//!
//! Not implemented in this pass. See `docs/status/04-room-and-events.md`.
