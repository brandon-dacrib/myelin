//! Copying one room's history, a page at a time: [`copy_room`].
//!
//! # Order, and memory
//!
//! This server stores an event only once it holds every event the event cites (its
//! `prev_events` and `auth_events`), and authorizes it against the state those imply -- as it
//! does an event arriving over federation. A room's events are read from Synapse in pages of
//! [`EventKey`] order, `(topological_ordering, stream_ordering)`: Synapse's own index on a
//! room's events, and an order in which an event's ancestors come before it (an event's depth
//! is greater than that of every event it cites). The next page is read while one is written, so
//! the importer holds at most two pages of a room at a time however large the room is, and
//! Synapse's database and this server's store work at the same time.
//!
//! If Synapse's depths are ever out of step with the graph (a remote server's events can carry
//! any depth), an event can arrive before an event it cites. The target answers it as waiting
//! ([`RoomOutcome::waiting`]); it is held aside, at most [`WAITING_LIMIT`] of them, and offered
//! again after each page that stored something, and once more at the end. Whatever still waits
//! then is refused, and logged.

use std::time::Instant;

use async_trait::async_trait;

use super::MigrationError;
use super::model::{SynapseEvent, SynapseRoom};
use super::source::{EventKey, SynapseSource};
use super::target::{MigrationTarget, RoomOutcome, TargetError};
use super::throughput::{RoomStats, peak_rss_bytes};

/// The most events of one room held aside waiting for an event they cite.
pub const WAITING_LIMIT: usize = 1_000;

/// Where a room's events come from, a page at a time in [`EventKey`] order.
#[async_trait]
pub trait EventPages: Send + Sync {
    /// The next `limit` events after `after`.
    ///
    /// # Errors
    /// Reading Synapse failed.
    async fn page(
        &self,
        after: Option<EventKey>,
        limit: i64,
    ) -> Result<Vec<(SynapseEvent, EventKey)>, MigrationError>;
}

/// One room's events in Synapse.
pub struct SynapseRoomPages<'a> {
    /// The source.
    pub source: &'a SynapseSource,
    /// The room.
    pub room_id: &'a str,
    /// Only events Synapse stored after this stream position (a room joined over federation:
    /// what came after the join).
    pub since: Option<i64>,
}

#[async_trait]
impl EventPages for SynapseRoomPages<'_> {
    async fn page(
        &self,
        after: Option<EventKey>,
        limit: i64,
    ) -> Result<Vec<(SynapseEvent, EventKey)>, MigrationError> {
        self.source
            .room_events(self.room_id, after, limit, self.since)
            .await
    }
}

/// Why a room could not be copied.
#[derive(Debug)]
pub enum RoomFailure {
    /// Reading Synapse failed: the copy stops (and can be resumed).
    Source(MigrationError),
    /// This server refused the room, or could not be written.
    Target(TargetError),
}

impl From<MigrationError> for RoomFailure {
    fn from(e: MigrationError) -> Self {
        Self::Source(e)
    }
}

impl From<TargetError> for RoomFailure {
    fn from(e: TargetError) -> Self {
        Self::Target(e)
    }
}

/// What copying a room did.
#[derive(Debug)]
pub struct RoomCopy {
    /// What the target did with the room's events, and with the room.
    pub outcome: RoomOutcome,
    /// How long it took, and how much.
    pub stats: RoomStats,
    /// The copy was cancelled part way (a pause, an abort): the room is not finished, and is
    /// copied again, from its start, when the copy resumes.
    pub stopped: bool,
}

/// Offers `held` to the target again; what still waits stays held.
async fn retry(
    target: &dyn MigrationTarget,
    room: &SynapseRoom,
    held: &mut Vec<SynapseEvent>,
    outcome: &mut RoomOutcome,
) -> Result<u64, TargetError> {
    if held.is_empty() {
        return Ok(0);
    }
    let offered = std::mem::take(held);
    let mut out = target.import_room_events(room, &offered).await?;
    let still: std::collections::HashSet<String> =
        std::mem::take(&mut out.waiting).into_iter().collect();
    let stored = out.stored;
    outcome.absorb(out);
    held.extend(offered.into_iter().filter(|e| still.contains(&e.event_id)));
    Ok(stored)
}

/// Copies `room`'s history from `pages` into `target`, `page_size` events at a time, then
/// finishes it ([`MigrationTarget::finish_room`]). `cancelled` is asked before each page.
///
/// # Errors
/// [`RoomFailure::Source`] when Synapse cannot be read, [`RoomFailure::Target`] when the target
/// refuses the room or cannot be written.
pub async fn copy_room(
    pages: &dyn EventPages,
    target: &dyn MigrationTarget,
    room: &SynapseRoom,
    page_size: i64,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<RoomCopy, RoomFailure> {
    let started = Instant::now();
    target.begin_room(room).await?;
    let mut outcome = RoomOutcome::default();
    let mut held: Vec<SynapseEvent> = Vec::new();
    let mut events_read = 0_u64;
    let mut bytes = 0_u64;
    let page_size = page_size.max(1);
    let stats = |outcome: &RoomOutcome, events_read, bytes| RoomStats {
        room_id: room.room_id.clone(),
        events_read,
        events_stored: outcome.stored,
        bytes,
        elapsed: started.elapsed(),
        peak_rss_bytes: peak_rss_bytes(),
    };
    let mut page = pages.page(None, page_size).await?;
    loop {
        if cancelled() {
            let stats = stats(&outcome, events_read, bytes);
            return Ok(RoomCopy {
                outcome,
                stats,
                stopped: true,
            });
        }
        let Some((_, last)) = page.last() else {
            break;
        };
        let after = Some(*last);
        let events: Vec<SynapseEvent> = page.into_iter().map(|(event, _)| event).collect();
        events_read += events.len() as u64;
        bytes += events.iter().map(|e| e.json_bytes).sum::<u64>();
        // The next page is read while this one is written: two pages are held at most, and
        // Synapse's database and this server's store are busy at the same time.
        let (out, next) = tokio::join!(
            target.import_room_events(room, &events),
            pages.page(after, page_size)
        );
        page = next?;
        let mut out = out?;
        let waiting: std::collections::HashSet<String> =
            std::mem::take(&mut out.waiting).into_iter().collect();
        let stored = out.stored;
        outcome.absorb(out);
        for event in events {
            if !waiting.contains(&event.event_id) {
                continue;
            }
            if held.len() < WAITING_LIMIT {
                held.push(event);
            } else {
                outcome.refused.push((
                    event.event_id,
                    "it cites an event this server does not hold, and too many of the room's \
                     events were already waiting for theirs"
                        .to_owned(),
                ));
            }
        }
        if stored > 0 {
            retry(target, room, &mut held, &mut outcome).await?;
        }
    }
    // The end of the room: whatever still waits is offered until nothing more lands.
    while !held.is_empty() && retry(target, room, &mut held, &mut outcome).await? > 0 {}
    for event in held {
        outcome.refused.push((
            event.event_id,
            "it cites an event that is not part of the room's history in Synapse".to_owned(),
        ));
    }
    // A room this server would not keep (nothing stored, its create event first) is refused
    // with the first reasons in the message: they are what an operator needs, and the log
    // line for a failed room is all that survives of this outcome.
    let finished = target.finish_room(room).await.map_err(|mut e| {
        if !outcome.refused.is_empty() {
            let first: Vec<String> = outcome
                .refused
                .iter()
                .take(3)
                .map(|(id, why)| format!("{id}: {why}"))
                .collect();
            e.message = format!(
                "{} ({} refused, the first: {})",
                e.message,
                outcome.refused.len(),
                first.join("; ")
            );
        }
        e
    })?;
    outcome.absorb(finished);
    let stats = stats(&outcome, events_read, bytes);
    Ok(RoomCopy {
        outcome,
        stats,
        stopped: false,
    })
}
