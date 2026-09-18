//! The `/notifications` endpoint's backing store: a per-user, paginated log of events the user
//! was (or would have been) notified about, per `crate::engine::evaluate` outcomes with
//! `notify: true`. Deliberately a separate append-only log from `crate::counts` (which holds only
//! aggregate numbers, cheap for `/sync` to read): `/notifications` needs the individual events
//! back, in order, which counts alone cannot reconstruct.

pub mod memory;
pub mod tables;

use ruma::{OwnedEventId, OwnedRoomId};
use ruma::push::Action;

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
    /// The actions of the rule that matched (per the spec's `Notification.actions`).
    pub actions: Vec<Action>,
    /// Milliseconds since the epoch when the notification was recorded.
    pub ts_ms: u64,
    /// Whether the user has read this notification (advanced past it with a receipt).
    pub read: bool,
}

/// Persistence for the per-user notification log.
#[async_trait::async_trait]
pub trait NotificationLogStore: Send + Sync {
    /// Appends a new, unread entry, assigning it the next `seq` for this user. Returns the
    /// assigned `seq`.
    async fn append(
        &self,
        user_id: &ruma::UserId,
        room_id: &ruma::RoomId,
        event_id: &ruma::EventId,
        actions: Vec<Action>,
        ts_ms: u64,
    ) -> Result<u64, StoreError>;

    /// Up to `limit` entries with `seq > after` (`None` means "from the start"), oldest first.
    async fn page(
        &self,
        user_id: &ruma::UserId,
        after: Option<u64>,
        limit: usize,
        only_highlight: bool,
    ) -> Result<Vec<NotificationEntry>, StoreError>;
}

fn is_highlight(actions: &[Action]) -> bool {
    actions.iter().any(Action::is_highlight)
}

#[cfg(test)]
pub(crate) mod contract_tests {
    //! Shared assertions every `NotificationLogStore` implementation must satisfy (mirrors
    //! `crate::counts::contract_tests`'s pattern).

    use super::*;

    pub async fn behaves_correctly(store: &dyn NotificationLogStore) {
        let alice = ruma::user_id!("@alice:example.org");
        let room = ruma::room_id!("!room:example.org");
        let ev1 = ruma::event_id!("$one:example.org");
        let ev2 = ruma::event_id!("$two:example.org");

        assert!(store.page(alice, None, 10, false).await.unwrap().is_empty());

        let notify_actions = vec![Action::Notify];
        let highlight_actions = vec![
            Action::Notify,
            Action::SetTweak(ruma::push::Tweak::Highlight(
                ruma::push::HighlightTweakValue::Yes,
            )),
        ];

        let seq1 = store
            .append(alice, room, ev1, notify_actions.clone(), 1000)
            .await
            .unwrap();
        let seq2 = store
            .append(alice, room, ev2, highlight_actions.clone(), 2000)
            .await
            .unwrap();
        assert!(seq2 > seq1);

        let all = store.page(alice, None, 10, false).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].event_id, ev1);
        assert_eq!(all[1].event_id, ev2);

        let only_highlights = store.page(alice, None, 10, true).await.unwrap();
        assert_eq!(only_highlights.len(), 1);
        assert_eq!(only_highlights[0].event_id, ev2);

        let after_first = store.page(alice, Some(seq1), 10, false).await.unwrap();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].event_id, ev2);

        let limited = store.page(alice, None, 1, false).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].event_id, ev1);
    }
}
