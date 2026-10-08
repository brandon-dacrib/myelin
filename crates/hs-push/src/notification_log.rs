//! The `/notifications` endpoint's backing store: a per-user log of the events the user was
//! notified about (`crate::engine::evaluate` outcomes with `notify: true`), newest first.
//! Deliberately a separate log from `crate::counts` (which holds only aggregate numbers, cheap
//! for `/sync` to read): `/notifications` needs the individual events back, in order, which
//! counts alone cannot reconstruct.
//!
//! An entry carries the event's client-form JSON as it was when the notification fired, so the
//! endpoint can answer without a room lookup (and after the room is gone).
//!
//! # Read follows the receipts, per thread
//!
//! Whether an entry is `read` follows the user's receipts the way the unread counts do
//! (`crate::counts`'s module docs, MSC3771): an entry is read when the unthreaded receipt, or
//! the receipt for its own scope (`main` for the main timeline, its thread's root for an event
//! in a thread), is at or past its event's room position. So a receipt in a thread marks only
//! that thread's entries read, and only up to where it points.
//!
//! The marks are kept per room and per receipt scope ([`ReadMark`]), not per entry, so a
//! receipt costs one small write however long the log is:
//!
//! - a receipt whose event's position is known sets the scope's position mark;
//! - a receipt whose event's position is not known here reads everything the scope has logged
//!   so far (the newest `seq` at the time), as the counts do;
//! - entries logged before positions were kept have none, and are read by any receipt for
//!   their scope logged after them (the counts' rule for their legacy counters). Their scope is
//!   read off the stored event's `m.relates_to`.

pub mod memory;
pub mod tables;

use std::collections::HashMap;

use ruma::push::Action;
use ruma::{EventId, OwnedEventId, OwnedRoomId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::counts::ReceiptThread;
use crate::error::StoreError;

/// One row `/notifications` can return.
#[derive(Debug, Clone)]
pub struct NotificationEntry {
    /// Monotonically increasing per user, oldest first; also this entry's pagination token.
    pub seq: u64,
    /// The room the event occurred in.
    pub room_id: OwnedRoomId,
    /// The event notified about.
    pub event_id: OwnedEventId,
    /// The event, in the client-server API's shape.
    pub event: Value,
    /// The actions of the rule that matched (per the spec's `Notification.actions`).
    pub actions: Vec<Action>,
    /// The profile tag of the pusher the notification was for, if any.
    pub profile_tag: Option<String>,
    /// Milliseconds since the epoch when the notification was recorded.
    pub ts_ms: u64,
    /// Whether the user has read this notification (sent a receipt for the room since).
    pub read: bool,
}

/// What [`NotificationLogStore::append`] records.
#[derive(Debug, Clone)]
pub struct NewNotification {
    /// The room the event occurred in.
    pub room_id: OwnedRoomId,
    /// The event notified about.
    pub event_id: OwnedEventId,
    /// The event, in the client-server API's shape.
    pub event: Value,
    /// The matching rule's actions.
    pub actions: Vec<Action>,
    /// The profile tag of the pusher the notification was for, if any.
    pub profile_tag: Option<String>,
    /// Milliseconds since the epoch.
    pub ts_ms: u64,
    /// The event's room-local timeline position: what a receipt's position is compared with.
    pub pos: Option<i64>,
    /// The root of the thread the event is in (`None`: the room's main timeline).
    pub thread: Option<OwnedEventId>,
}

/// What the receipts for one scope of one room (unthreaded, `main`, or a thread; the key is
/// [`ReceiptThread`]'s stored form) have read of the log. See the module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMark {
    /// Every entry of the scope with `seq` at or below this is read: a receipt whose event's
    /// position was not known here.
    #[serde(default)]
    pub whole_seq: u64,
    /// Entries without a position (logged before positions were kept) at or below this `seq`
    /// are read: any receipt for the scope.
    #[serde(default)]
    pub legacy_seq: u64,
    /// Entries at or before this room position are read.
    #[serde(default)]
    pub pos: Option<i64>,
}

impl ReadMark {
    /// Takes a receipt into the mark: at `pos` if known, else for everything logged so far
    /// (`newest_seq`). Never moves backwards.
    pub fn apply(&mut self, newest_seq: u64, pos: Option<i64>) {
        match pos {
            Some(pos) => {
                self.pos = Some(self.pos.map_or(pos, |held| held.max(pos)));
                self.legacy_seq = self.legacy_seq.max(newest_seq);
            }
            None => self.whole_seq = self.whole_seq.max(newest_seq),
        }
    }

    /// Whether this mark reads the entry `seq`, at `pos`.
    #[must_use]
    pub fn covers(&self, seq: u64, pos: Option<i64>) -> bool {
        seq <= self.whole_seq
            || match pos {
                Some(pos) => self.pos.is_some_and(|read| pos <= read),
                None => seq <= self.legacy_seq,
            }
    }
}

/// Whether an entry (`seq`, at `pos`, in `thread`) is read, given its room's marks keyed by
/// [`ReceiptThread`]'s stored form: the unthreaded mark or its own scope's covers it.
#[must_use]
pub(crate) fn entry_is_read(
    marks: &HashMap<String, ReadMark>,
    seq: u64,
    pos: Option<i64>,
    thread: Option<&EventId>,
) -> bool {
    [
        ReceiptThread::Unthreaded.mark_key(),
        ReceiptThread::of_scope(thread).mark_key(),
    ]
    .iter()
    .any(|key| marks.get(key).is_some_and(|mark| mark.covers(seq, pos)))
}

/// The thread an entry's event is in, read off its `m.relates_to` (for entries logged before
/// the thread was stored with them).
pub(crate) fn thread_of_event(event: &Value) -> Option<OwnedEventId> {
    let relation = event.get("content")?.get("m.relates_to")?;
    if relation.get("rel_type")?.as_str()? != "m.thread" {
        return None;
    }
    EventId::parse(relation.get("event_id")?.as_str()?).ok()
}

/// Persistence for the per-user notification log.
#[async_trait::async_trait]
pub trait NotificationLogStore: Send + Sync {
    /// Appends a new, unread entry, assigning it the next `seq` for this user. Returns the
    /// assigned `seq`.
    async fn append(
        &self,
        user_id: &ruma::UserId,
        notification: NewNotification,
    ) -> Result<u64, StoreError>;

    /// Up to `limit` entries with `seq < before` (`None` means "from the newest"), newest first.
    async fn page(
        &self,
        user_id: &ruma::UserId,
        before: Option<u64>,
        limit: usize,
        only_highlight: bool,
    ) -> Result<Vec<NotificationEntry>, StoreError>;

    /// A read receipt: the entries of `room_id` that `thread` covers are read up to room
    /// position `pos`, or, when `pos` is not known here, every one logged so far (see the
    /// module docs).
    async fn mark_read(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
        thread: &ReceiptThread,
        pos: Option<i64>,
    ) -> Result<(), StoreError>;
}

fn is_highlight(actions: &[Action]) -> bool {
    actions.iter().any(Action::is_highlight)
}

#[cfg(test)]
pub(crate) mod contract_tests {
    //! Shared assertions every `NotificationLogStore` implementation must satisfy (mirrors
    //! `crate::counts::contract_tests`'s pattern).

    use super::*;

    fn notification(
        room: &ruma::RoomId,
        event_id: &ruma::EventId,
        actions: Vec<Action>,
        ts: u64,
    ) -> NewNotification {
        NewNotification {
            room_id: room.to_owned(),
            event_id: event_id.to_owned(),
            event: serde_json::json!({"event_id": event_id, "type": "m.room.message"}),
            actions,
            profile_tag: None,
            ts_ms: ts,
            pos: i64::try_from(ts).ok(),
            thread: None,
        }
    }

    /// An entry at room position `pos`, in `thread` (`None`: the main timeline).
    fn at(
        room: &ruma::RoomId,
        event_id: &str,
        pos: Option<i64>,
        thread: Option<&ruma::EventId>,
    ) -> NewNotification {
        let mut event = serde_json::json!({"event_id": event_id, "type": "m.room.message"});
        if let Some(root) = thread {
            event["content"] =
                serde_json::json!({"m.relates_to": {"rel_type": "m.thread", "event_id": root}});
        }
        NewNotification {
            room_id: room.to_owned(),
            event_id: ruma::EventId::parse(event_id).unwrap(),
            event,
            actions: vec![Action::Notify],
            profile_tag: None,
            ts_ms: 0,
            pos,
            // An entry from before threads were stored with it: read off the event.
            thread: None,
        }
    }

    /// Whether each of `ids` reads as read, in that order.
    async fn read_flags(store: &dyn NotificationLogStore, ids: &[&str]) -> Vec<bool> {
        let alice = ruma::user_id!("@alice:example.org");
        let page = store.page(alice, None, 100, false).await.unwrap();
        ids.iter()
            .map(|id| {
                page.iter()
                    .find(|e| e.event_id.as_str() == *id)
                    .unwrap_or_else(|| panic!("{id} is not in the log"))
                    .read
            })
            .collect()
    }

    /// `TestThreadedReceipts`' room as a log: A (main, 1), B and C (thread A, 2-3), D (main,
    /// 4), E (thread A, 5), F (main, 6). Each receipt marks read only what it covers, up to
    /// where it points; a receipt whose event is unknown reads its whole scope as logged so
    /// far; an entry with no position is read by any receipt for its scope after it.
    pub async fn receipts_mark_entries_read_per_thread(store: &dyn NotificationLogStore) {
        let alice = ruma::user_id!("@alice:example.org");
        let room = ruma::room_id!("!threads:example.org");
        let root = ruma::event_id!("$A:example.org");
        let ids = [
            "$A:example.org",
            "$B:example.org",
            "$C:example.org",
            "$D:example.org",
            "$E:example.org",
            "$F:example.org",
        ];
        let threads = [None, Some(root), Some(root), None, Some(root), None];
        for (pos, (id, thread)) in (1..).zip(ids.iter().zip(threads)) {
            store
                .append(alice, at(room, id, Some(pos), thread))
                .await
                .unwrap();
        }
        let flags = || read_flags(store, &ids);
        assert_eq!(flags().await, [false; 6]);

        // `main` up to A: A only.
        store
            .mark_read(alice, room, &ReceiptThread::Main, Some(1))
            .await
            .unwrap();
        assert_eq!(flags().await, [true, false, false, false, false, false]);
        // The thread up to B: B only.
        store
            .mark_read(
                alice,
                room,
                &ReceiptThread::Thread(root.to_owned()),
                Some(2),
            )
            .await
            .unwrap();
        assert_eq!(flags().await, [true, true, false, false, false, false]);
        // Unthreaded up to D: everything up to D, in both scopes.
        store
            .mark_read(alice, room, &ReceiptThread::Unthreaded, Some(4))
            .await
            .unwrap();
        assert_eq!(flags().await, [true, true, true, true, false, false]);
        // A receipt behind one already taken undoes nothing.
        store
            .mark_read(alice, room, &ReceiptThread::Unthreaded, Some(1))
            .await
            .unwrap();
        assert_eq!(flags().await, [true, true, true, true, false, false]);
        // The thread, at an event not known here: the whole thread as logged so far, not F.
        store
            .mark_read(alice, room, &ReceiptThread::Thread(root.to_owned()), None)
            .await
            .unwrap();
        assert_eq!(flags().await, [true, true, true, true, true, false]);

        // An entry from before positions were kept, in the thread: a main receipt leaves it,
        // a thread receipt reads it, and a later entry stays unread.
        let other = ruma::room_id!("!legacy:example.org");
        store
            .append(alice, at(other, "$old:example.org", None, Some(root)))
            .await
            .unwrap();
        store
            .mark_read(alice, other, &ReceiptThread::Main, Some(10))
            .await
            .unwrap();
        assert_eq!(read_flags(store, &["$old:example.org"]).await, [false]);
        store
            .mark_read(
                alice,
                other,
                &ReceiptThread::Thread(root.to_owned()),
                Some(10),
            )
            .await
            .unwrap();
        store
            .append(alice, at(other, "$new:example.org", Some(11), Some(root)))
            .await
            .unwrap();
        assert_eq!(
            read_flags(store, &["$old:example.org", "$new:example.org"]).await,
            [true, false]
        );
    }

    pub async fn behaves_correctly(store: &dyn NotificationLogStore) {
        let alice = ruma::user_id!("@alice:example.org");
        let room = ruma::room_id!("!room:example.org");
        let other_room = ruma::room_id!("!other:example.org");
        let ev1 = ruma::event_id!("$one:example.org");
        let ev2 = ruma::event_id!("$two:example.org");
        let ev3 = ruma::event_id!("$three:example.org");

        assert!(store.page(alice, None, 10, false).await.unwrap().is_empty());

        let notify_actions = vec![Action::Notify];
        let highlight_actions = vec![
            Action::Notify,
            Action::SetTweak(ruma::push::Tweak::Highlight(
                ruma::push::HighlightTweakValue::Yes,
            )),
        ];

        let seq1 = store
            .append(alice, notification(room, ev1, notify_actions.clone(), 1000))
            .await
            .unwrap();
        let seq2 = store
            .append(
                alice,
                notification(room, ev2, highlight_actions.clone(), 2000),
            )
            .await
            .unwrap();
        assert!(seq2 > seq1);

        let all = store.page(alice, None, 10, false).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].event_id, ev2, "newest first");
        assert_eq!(all[1].event_id, ev1);
        assert_eq!(all[1].event["event_id"], ev1.as_str());
        assert!(!all[0].read);

        let only_highlights = store.page(alice, None, 10, true).await.unwrap();
        assert_eq!(only_highlights.len(), 1);
        assert_eq!(only_highlights[0].event_id, ev2);

        let before_second = store.page(alice, Some(seq2), 10, false).await.unwrap();
        assert_eq!(before_second.len(), 1);
        assert_eq!(before_second[0].event_id, ev1);

        let limited = store.page(alice, None, 1, false).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].event_id, ev2);

        // A receipt for the room marks what is there read; what comes after it is unread, and
        // another room's entries are untouched.
        store
            .append(
                alice,
                notification(other_room, ev3, notify_actions.clone(), 2500),
            )
            .await
            .unwrap();
        store
            .mark_read(alice, room, &ReceiptThread::Unthreaded, None)
            .await
            .unwrap();
        store
            .append(alice, notification(room, ev3, notify_actions.clone(), 3000))
            .await
            .unwrap();
        let after_receipt = store.page(alice, None, 10, false).await.unwrap();
        let read_of = |id: &ruma::EventId, r: &ruma::RoomId| {
            after_receipt
                .iter()
                .find(|e| e.event_id == id && e.room_id == r)
                .unwrap()
                .read
        };
        assert!(!read_of(ev3, room), "newer than the receipt");
        assert!(read_of(ev2, room));
        assert!(read_of(ev1, room));
        assert!(!read_of(ev3, other_room), "another room's receipt");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_never_moves_back_and_reads_legacy_entries_by_seq() {
        let mut mark = ReadMark::default();
        mark.apply(5, Some(10));
        mark.apply(6, Some(3));
        assert_eq!(mark.pos, Some(10));
        assert!(mark.covers(99, Some(10)), "at the receipt");
        assert!(!mark.covers(1, Some(11)), "after it, whatever its seq");
        assert!(mark.covers(6, None), "a legacy entry logged before");
        assert!(!mark.covers(7, None), "a legacy entry logged after");
        mark.apply(8, None);
        assert!(
            mark.covers(8, Some(50)),
            "a whole-scope read covers what was logged"
        );
        assert!(!mark.covers(9, Some(50)));
    }

    #[test]
    fn an_entrys_thread_is_read_off_its_relation() {
        let event = serde_json::json!({"content": {"m.relates_to": {
            "rel_type": "m.thread", "event_id": "$root:example.org"}}});
        assert_eq!(
            thread_of_event(&event).as_deref().map(EventId::as_str),
            Some("$root:example.org")
        );
        let reply = serde_json::json!({"content": {"m.relates_to": {
            "m.in_reply_to": {"event_id": "$x:example.org"}}}});
        assert_eq!(thread_of_event(&reply), None);
    }
}
