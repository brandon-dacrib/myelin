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
//!
//! # Unread is "after the read receipt", per thread (MSC3771, MSC3773)
//!
//! Each notification is kept with its event's room-local position until it is read. A read
//! receipt is a position and a [`ReceiptThread`]: an unthreaded receipt reads everything up to
//! it in every scope; a `main` receipt reads the room's main timeline up to it; a receipt for a
//! thread reads that thread up to it. A scope's count is its notifications after the later of
//! the unthreaded receipt and its own. So a receipt in a thread leaves the main timeline's
//! count alone, and an unthreaded receipt halfway down the room leaves what came after it
//! unread: Complement's `TestThreadedReceipts`, and Synapse's `event_push_actions`.
//!
//! The read positions are kept too ([`CountsStore::mark_read`]), and a notification at or
//! before one is not counted: the pipeline may learn of an event after the receipt that read
//! it.
//!
//! # Read shape
//!
//! [`RoomNotificationCounts`]: `notification_count`/`highlight_count` for the room's main
//! timeline, plus a per-thread breakdown (empty until a room actually has threads), which is
//! the shape `/sync`'s `unread_notifications`/`unread_thread_notifications` need when a client
//! asks for thread counts (`unread_thread_notifications: true` in its filter), and
//! [`RoomNotificationCounts::totals`] when it does not.

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

impl Counts {
    fn add(&mut self, other: Counts) {
        self.notification_count += other.notification_count;
        self.highlight_count += other.highlight_count;
    }
}

/// The full per-room read shape `/sync` needs for one user: the main timeline's counts plus a
/// breakdown per thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomNotificationCounts {
    /// Counts for events outside any thread.
    pub main: Counts,
    /// Counts for events inside a thread, keyed by the thread's root event ID. Only threads with
    /// at least one unread notification are present.
    pub threads: BTreeMap<OwnedEventId, Counts>,
}

impl RoomNotificationCounts {
    /// The spec's flattened view: notification/highlight counts across the whole room, main
    /// timeline plus every thread summed in -- `/sync`'s `unread_notifications` for a client that
    /// did not ask for thread counts.
    #[must_use]
    pub fn totals(&self) -> Counts {
        let mut total = self.main;
        for thread in self.threads.values() {
            total.add(*thread);
        }
        total
    }
}

/// A scope within a room: the main timeline, or a specific thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// The room's main timeline (events with no `m.thread` relation).
    Main,
    /// One thread, named by its root event.
    Thread(&'a EventId),
}

impl Scope<'_> {
    /// The stored form: `""` for the main timeline, else the thread root's event ID.
    fn key_part(self) -> String {
        match self {
            Scope::Main => String::new(),
            Scope::Thread(root) => root.to_string(),
        }
    }

    /// The scope a stored key part names.
    fn parse_key_part(key: &str) -> Result<Option<OwnedEventId>, StoreError> {
        if key.is_empty() {
            return Ok(None);
        }
        key.try_into()
            .map(Some)
            .map_err(|e| StoreError::Backend(format!("stored thread key is not an event id: {e}")))
    }
}

/// Which timeline a read receipt is about: its `thread_id` (MSC3771, spec v1.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReceiptThread {
    /// No `thread_id`: the receipt reads every scope up to its event.
    Unthreaded,
    /// `thread_id: "main"`: the room's main timeline only.
    Main,
    /// `thread_id: <root event ID>`: that thread only.
    Thread(OwnedEventId),
}

impl ReceiptThread {
    /// The receipt's `thread_id` as a client or another server gave it: absent, `main`, or a
    /// thread root's event ID.
    ///
    /// # Errors
    /// The `thread_id` is neither `main` nor an event ID.
    pub fn from_wire(thread_id: Option<&str>) -> Result<Self, String> {
        match thread_id {
            None => Ok(Self::Unthreaded),
            Some("main") => Ok(Self::Main),
            Some(root) => EventId::parse(root)
                .map(Self::Thread)
                .map_err(|_| format!("thread_id {root:?} is neither \"main\" nor an event ID")),
        }
    }

    /// The `thread_id` this receipt carries on the wire (`None` when unthreaded).
    #[must_use]
    pub fn as_wire(&self) -> Option<&str> {
        match self {
            Self::Unthreaded => None,
            Self::Main => Some("main"),
            Self::Thread(root) => Some(root.as_str()),
        }
    }

    /// The stored form of a read position's key: `""` unthreaded, else [`Self::as_wire`].
    pub(crate) fn mark_key(&self) -> String {
        self.as_wire().unwrap_or_default().to_owned()
    }

    /// Whether this receipt reads `scope`.
    pub(crate) fn reads(&self, scope: Option<&EventId>) -> bool {
        match (self, scope) {
            (Self::Unthreaded, _) | (Self::Main, None) => true,
            (Self::Thread(root), Some(scope)) => root == scope,
            _ => false,
        }
    }

    /// The receipt that reads the scope `scope` names (`None`: main).
    pub(crate) fn of_scope(scope: Option<&EventId>) -> Self {
        scope.map_or(Self::Main, |root| Self::Thread(root.to_owned()))
    }
}

/// Persistence for per-user, per-room (and per-thread) notification counts.
#[async_trait::async_trait]
pub trait CountsStore: Send + Sync {
    /// Every counted scope for this user in this room, aggregated into [`RoomNotificationCounts`].
    /// A room with no unread notifications returns the all-zero default, not an error.
    async fn get_room_counts(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<RoomNotificationCounts, StoreError>;

    /// Records one unread notification in `scope`, from the event at room-local position `pos`,
    /// highlighted or not. Called exactly once per event per local recipient whose
    /// `crate::engine::evaluate` outcome had `notify: true` — see this module's doc comment on
    /// why that is the *only* place this is ever called from. Nothing is recorded when a
    /// receipt already read `pos` in that scope.
    async fn record_notification(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
        highlight: bool,
        pos: i64,
    ) -> Result<(), StoreError>;

    /// A read receipt: everything `thread` covers up to room-local position `pos` is read (see
    /// the module docs). A position at or before one already read changes nothing.
    async fn mark_read(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        thread: &ReceiptThread,
        pos: i64,
    ) -> Result<(), StoreError>;

    /// Zeroes `scope`'s counts: a receipt in that scope whose event's position is unknown here.
    async fn reset(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        scope: Scope<'_>,
    ) -> Result<(), StoreError>;

    /// Zeroes every scope of the room, main timeline and threads alike: an unthreaded receipt
    /// whose event's position is unknown here.
    async fn reset_room(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), StoreError>;

    /// The user's unread notifications across every room and thread: the badge a push carries
    /// (`counts.unread` in the Push Gateway API).
    async fn total_unread(&self, user_id: &UserId) -> Result<u64, StoreError>;
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
        let thread = Scope::Thread(thread_root);

        // A room nobody has recorded anything for reads as all-zero, not an error.
        let empty = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(empty, RoomNotificationCounts::default());

        // Two plain notifications on the main timeline, one highlighted in a thread.
        store
            .record_notification(alice, room, Scope::Main, false, 1)
            .await
            .unwrap();
        store
            .record_notification(alice, room, Scope::Main, false, 2)
            .await
            .unwrap();
        store
            .record_notification(alice, room, thread, true, 3)
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
            after_reset
                .threads
                .get(thread_root)
                .copied()
                .unwrap()
                .notification_count,
            1
        );

        // Resetting the thread clears it too (and, since it was the last nonzero scope,
        // `threads` goes back to empty rather than keeping a zeroed entry around).
        store.reset(alice, room, thread).await.unwrap();
        let all_clear = store.get_room_counts(alice, room).await.unwrap();
        assert_eq!(all_clear, RoomNotificationCounts::default());

        // The badge sums every room and thread of one user, and nobody else's.
        let other_room = ruma::room_id!("!other:example.org");
        let bob = ruma::user_id!("@bob:example.org");
        store
            .record_notification(alice, room, Scope::Main, false, 4)
            .await
            .unwrap();
        store
            .record_notification(alice, room, thread, true, 5)
            .await
            .unwrap();
        store
            .record_notification(alice, other_room, Scope::Main, false, 1)
            .await
            .unwrap();
        store
            .record_notification(bob, room, Scope::Main, false, 4)
            .await
            .unwrap();
        assert_eq!(store.total_unread(alice).await.unwrap(), 3);
        assert_eq!(store.total_unread(bob).await.unwrap(), 1);

        // Resetting the room clears threads too, and leaves other rooms and users alone.
        store.reset_room(alice, room).await.unwrap();
        assert_eq!(
            store.get_room_counts(alice, room).await.unwrap(),
            RoomNotificationCounts::default()
        );
        assert_eq!(store.total_unread(alice).await.unwrap(), 1);
        assert_eq!(store.total_unread(bob).await.unwrap(), 1);
    }

    /// `TestThreadedReceipts`, with its events at positions 1-7: A (main), B, C (thread, C
    /// highlighted), D (main, highlighted), E (thread), F (main), G (a reaction; not
    /// notifying). Each receipt reads only what it covers, up to where it points.
    pub async fn receipts_read_per_thread_up_to_their_position(store: &dyn CountsStore) {
        let bob = ruma::user_id!("@bob:example.org");
        let room = ruma::room_id!("!room:example.org");
        let a = ruma::event_id!("$a:example.org");
        let thread = Scope::Thread(a);
        for (scope, highlight, pos) in [
            (Scope::Main, false, 1),
            (thread, false, 2),
            (thread, true, 3),
            (Scope::Main, true, 4),
            (thread, false, 5),
            (Scope::Main, false, 6),
        ] {
            store
                .record_notification(bob, room, scope, highlight, pos)
                .await
                .unwrap();
        }
        let check = |main: (u64, u64), in_thread: Option<(u64, u64)>| {
            let a = a.to_owned();
            move |c: RoomNotificationCounts| {
                assert_eq!(
                    (c.main.notification_count, c.main.highlight_count),
                    main,
                    "main timeline: {c:?}"
                );
                assert_eq!(
                    c.threads
                        .get(&a)
                        .map(|t| (t.notification_count, t.highlight_count)),
                    in_thread,
                    "thread: {c:?}"
                );
            }
        };
        let counts = || store.get_room_counts(bob, room);
        check((3, 1), Some((3, 1)))(counts().await.unwrap());
        assert_eq!(counts().await.unwrap().totals().notification_count, 6);

        store
            .mark_read(bob, room, &ReceiptThread::Main, 1)
            .await
            .unwrap();
        check((2, 1), Some((3, 1)))(counts().await.unwrap());
        store
            .mark_read(bob, room, &ReceiptThread::Thread(a.to_owned()), 2)
            .await
            .unwrap();
        check((2, 1), Some((2, 1)))(counts().await.unwrap());
        store
            .mark_read(bob, room, &ReceiptThread::Unthreaded, 4)
            .await
            .unwrap();
        check((1, 0), Some((1, 0)))(counts().await.unwrap());
        store
            .mark_read(bob, room, &ReceiptThread::Thread(a.to_owned()), 7)
            .await
            .unwrap();
        check((1, 0), None)(counts().await.unwrap());
        assert_eq!(store.total_unread(bob).await.unwrap(), 1);

        // A receipt behind one already taken reads nothing more and undoes nothing.
        store
            .mark_read(bob, room, &ReceiptThread::Main, 0)
            .await
            .unwrap();
        check((1, 0), None)(counts().await.unwrap());

        // A notification the pipeline learns of after the receipt that read it is not counted;
        // one after every receipt is.
        store
            .record_notification(bob, room, Scope::Main, true, 3)
            .await
            .unwrap();
        store
            .record_notification(bob, room, thread, false, 6)
            .await
            .unwrap();
        check((1, 0), None)(counts().await.unwrap());
        store
            .record_notification(bob, room, thread, false, 8)
            .await
            .unwrap();
        check((1, 0), Some((1, 0)))(counts().await.unwrap());
        store
            .mark_read(bob, room, &ReceiptThread::Unthreaded, 8)
            .await
            .unwrap();
        assert_eq!(counts().await.unwrap(), RoomNotificationCounts::default());
    }

    #[test]
    fn receipt_threads_parse_from_the_wire() {
        assert_eq!(
            ReceiptThread::from_wire(None),
            Ok(ReceiptThread::Unthreaded)
        );
        assert_eq!(
            ReceiptThread::from_wire(Some("main")),
            Ok(ReceiptThread::Main)
        );
        assert_eq!(
            ReceiptThread::from_wire(Some("$root")),
            Ok(ReceiptThread::Thread(ruma::event_id!("$root").to_owned()))
        );
        assert!(ReceiptThread::from_wire(Some("nonsense")).is_err());
        assert_eq!(ReceiptThread::Main.as_wire(), Some("main"));
        assert_eq!(ReceiptThread::Unthreaded.as_wire(), None);
    }
}
