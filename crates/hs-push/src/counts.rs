//! Notification and highlight counts per room and per thread, stored so `/sync` (track 05) can
//! read them cheaply: a direct keyed lookup per room, never a scan of the room's timeline or a
//! re-evaluation of push rules at sync time.
//!
//! # One source of truth
//!
//! The brief's correctness note: a notification count that disagrees with what `/sync` reports
//! is highly visible and erodes trust in the whole server. The design that makes that
//! structurally hard rather than merely tested-around: **the only place a count is ever
//! incremented is [`CountsStore::record_notification`], called once per event per local
//! recipient, driven by exactly the same [`crate::engine::evaluate`] outcome that decided whether
//! to push** — there is no second code path (no "sync recomputes its own idea of unread", no
//! separate badge-count estimator) that could drift from it. `/sync` is meant to call
//! [`CountsStore::get_room_counts`] and report those numbers verbatim; it must not derive its own.
//! `docs/status/10-push.md`'s "Interfaces provided" names this function as the contract with
//! track 05, end-to-end tests for the agreement live in that same status file's plan.
//!
//! # Read shape
//!
//! [`RoomNotificationCounts`]: `notification_count`/`highlight_count` for the room's main
//! timeline, plus a per-thread breakdown (`MSC4306` thread-scoped counts; empty until a room
//! actually has threads). This is deliberately exactly the shape `/sync`'s
//! `unread_notifications`/`unread_thread_notifications` need, so track 05 can serialize it with
//! no reshaping.

pub mod memory;
pub mod tables;

use std::collections::BTreeMap;

use ruma::{EventId, OwnedEventId, RoomId, UserId};

use crate::error::StoreError;

/// Notification and highlight counts for one scope (a room's main timeline, or one thread within
/// it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Number of events matching a rule with the `notify` action the recipient has not yet read
    /// (per their read receipt).
    pub notification_count: u64,
    /// Of those, the number that also matched a rule with a `highlight` tweak.
    pub highlight_count: u64,
}

/// The full per-room read shape `/sync` needs for one user: the main timeline's counts plus a
/// breakdown per thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomNotificationCounts {
    /// Counts for events outside any thread (or on servers/clients not doing thread-scoped
    /// counts at all).
    pub main: Counts,
    /// Counts for events inside a thread, keyed by the thread's root event ID. Only threads with
    /// at least one unread notification are present.
    pub threads: BTreeMap<OwnedEventId, Counts>,
}

impl RoomNotificationCounts {
    /// The spec's flattened view: notification/highlight counts across the whole room, main
    /// timeline plus every thread summed in. Some legacy or non-thread-aware sync paths want this
    /// single pair rather than the per-thread breakdown.
    #[must_use]
    pub fn totals(&self) -> Counts {
        let mut total = self.main;
        for thread in self.threads.values() {
            total.notification_count += thread.notification_count;
            total.highlight_count += thread.highlight_count;
        }
        total
    }
}

/// A scope within a room: the main timeline, or a specific thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// The room's main timeline (events with no `m.thread` relation, or on a room/client not
    /// using threads).
    Main,
    /// One thread, named by its root event.
    Thread(&'a EventId),
}

impl Scope<'_> {
    fn key_part(self) -> String {
        match self {
            Scope::Main => String::new(),
            Scope::Thread(root) => root.to_string(),
        }
    }
}

/// Persistence for per-user, per-room (and per-thread) notification counts.
#[async_trait::async_trait]
pub trait CountsStore: Send + Sync {
    /// Every counted scope for this user in this room, aggregated into [`RoomNotificationCounts`].
    /// A room with no unread notifications returns the all-zero default, not an error.
    async fn get_room_counts(&self, user_id: &UserId, room_id: &RoomId) -> Result<RoomNotificationCounts, StoreError>;

    /// Increments `scope`'s `notification_count` by one, and its `highlight_count` too if
    /// `highlight` is set. Called exactly once per event per local recipient whose
    /// `crate::engine::evaluate` outcome had `notify: true` — see this module's doc comment on
    /// why that is the *only* place this is ever called from.
    async fn record_notification(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
        highlight: bool,
    ) -> Result<(), StoreError>;

    /// Zeroes `scope`'s counts, because the user has read up to (at least) the notifying event —
    /// called when track 05 tells this crate a read receipt advanced past it (see
    /// `docs/status/10-push.md`'s "Interfaces needed": the receipt-to-reset wiring is track 05's
    /// side of this seam, since 05 owns receipts).
    async fn reset(&self, user_id: &UserId, room_id: &RoomId, scope: Scope<'_>) -> Result<(), StoreError>;
}

#[cfg(test)]
mod contract_tests {
    //! Shared assertions every `CountsStore` implementation must satisfy, run against both
    //! backends in each backend's own test module (mirrors
    //! `crates/hs-auth/src/store/shared_tests.rs`'s pattern).

    use super::*;

    pub async fn behaves_correctly(store: &dyn CountsStore) {
        let alice = ruma::user_id!("@alice:example.org");
        let room = ruma::room_id!("!room:example.org");
        let thread_root = ruma::event_id!("$thread:example.org");

        // A room nobody has recorded anything for reads as all-zero, not an error.
        let empty = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(empty, RoomNotificationCounts::default());

        // Two plain notifications on the main timeline.
        store
            .record_notification(alice, room, Scope::Main, false)
            .await
            .unwrap();
        store
            .record_notification(alice, room, Scope::Main, false)
            .await
            .unwrap();
        // One highlighted notification in a thread.
        store
            .record_notification(alice, room, Scope::Thread(thread_root), true)
            .await
            .unwrap();

        let counts = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(counts.main.notification_count, 2);
        assert_eq!(counts.main.highlight_count, 0);
        let thread_counts = counts.threads.get(thread_root).copied().unwrap();
        assert_eq!(thread_counts.notification_count, 1);
        assert_eq!(thread_counts.highlight_count, 1);
        assert_eq!(counts.totals().notification_count, 3);
        assert_eq!(counts.totals().highlight_count, 1);

        // Resetting the main timeline must not touch the thread's counts.
        store.reset(alice, room, Scope::Main).await.unwrap();
        let after_reset = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(after_reset.main, Counts::default());
        assert_eq!(
            after_reset.threads.get(thread_root).copied().unwrap().notification_count,
            1
        );

        // Resetting the thread clears it too (and, since it was the last nonzero scope,
        // `threads` goes back to empty rather than keeping a zeroed entry around).
        store.reset(alice, room, Scope::Thread(thread_root)).await.unwrap();
        let all_clear = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(all_clear, RoomNotificationCounts::default());
    }
}
