//! The `/notifications` endpoint's backing store: a per-user log of the events the user was
//! notified about (`crate::engine::evaluate` outcomes with `notify: true`), newest first.
//! Deliberately a separate log from `crate::counts` (which holds only aggregate numbers, cheap
//! for `/sync` to read): `/notifications` needs the individual events back, in order, which
//! counts alone cannot reconstruct.
//!
//! An entry carries the event's client-form JSON as it was when the notification fired, so the
//! endpoint can answer without a room lookup (and after the room is gone). Whether an entry is
//! `read` follows the user's receipts: [`NotificationLogStore::mark_room_read`] records the
//! newest entry at the time of the receipt, and every entry of that room up to it reads as read.
//! That is a per-room watermark, not a per-entry flag, so a receipt costs one small write
//! however long the log is.

pub mod memory;
pub mod tables;

use ruma::push::Action;
use ruma::{OwnedEventId, OwnedRoomId};
use serde_json::Value;

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

    /// Marks every entry of `room_id` recorded so far as read.
    async fn mark_room_read(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
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
        }
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
        store.mark_room_read(alice, room).await.unwrap();
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
