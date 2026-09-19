//! [`ReceiptRegistry`]: in-memory `m.read`/`m.read.private` read-receipt state, keyed by room.
//!
//! Mirrors `crate::typing::TypingRegistry`'s shape and reasoning exactly, per this track's own
//! status file (`docs/status/05-sync.md`, "the typing and presence pattern applies directly"): a
//! global monotonic counter stamped onto whichever room's receipts changed, exposed to
//! `crate::sync` through [`crate::token::SyncToken::receipts_seq`] (a field the token format has
//! reserved since session 1, before this module existed).
//!
//! # Not persisted -- a deliberate, documented cut, same as presence
//!
//! Real read receipts are more durable in spirit than a typing indicator -- a user does not want
//! "what have I read" to reset -- but this crate's `crate::presence::PresenceRegistry` already
//! made the identical call for presence (in-memory, lost on restart, forever until reset) for the
//! same reason: `crate::store::UserStore` is this crate's durable path, and folding receipts into
//! it now would mean a schema/table addition this session was not scoped for. A restart loses
//! read state exactly as it loses typing and presence; nothing about that regresses any test this
//! crate runs today. Moving this into `store` later is a mechanical follow-up, not a redesign --
//! see `docs/status/05-sync.md`'s "What's next".
//!
//! # `m.fully_read` is not handled here
//!
//! The fully-read marker is private room account data (`m.fully_read`, content
//! `{"event_id": ...}`), not a receipt type at all -- `crate::routes::receipts::post_read_markers`
//! writes it straight through `crate::store::UserStore::put_room_account_data`, which already has
//! its own durable storage, its own change counter (`SyncToken::account_data_seq`), and its own
//! `/sync` wiring (`crate::sync::build`'s existing room account-data section). This registry only
//! ever answers "what does the `m.receipt` ephemeral event for this room look like".
//!
//! # Privacy: `m.read` is public, `m.read.private` is not
//!
//! [`ReceiptRegistry::content_for`] takes the viewing user explicitly and omits every other
//! user's `m.read.private` receipt from the built content -- the entire point of the "private"
//! variant (MSC2285, stable since spec v1.4) is that nobody but the sender ever learns about it.
//! `m.read` has no such restriction: every member of the room sees every other member's public
//! read receipt, matching Synapse's own behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

/// The receipt types `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}` and
/// `POST /rooms/{roomId}/read_markers` accept. `m.fully_read` is deliberately absent -- see the
/// module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiptKind {
    /// `m.read`: a public receipt, visible to every other member of the room.
    Read,
    /// `m.read.private`: visible only to the user who sent it.
    ReadPrivate,
}

impl ReceiptKind {
    /// The wire spelling of this receipt type.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "m.read",
            Self::ReadPrivate => "m.read.private",
        }
    }

    /// Parses a `receiptType` path segment. `None` for anything this endpoint does not accept,
    /// including the spec-legal-elsewhere `m.fully_read` (see the module docs).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "m.read" => Some(Self::Read),
            "m.read.private" => Some(Self::ReadPrivate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct ReceiptEntry {
    event_id: OwnedEventId,
    ts: u64,
}

struct RoomReceipts {
    /// `(user, kind) -> latest receipt`. A later call for the same `(user, kind)` overwrites the
    /// earlier one -- this registry does not verify the new event is actually "later" than the
    /// old one (it has no timeline position to compare against without a round trip to the room
    /// actor); a real client only ever advances its own read receipt, so this is not a practical
    /// gap.
    by_user: HashMap<(OwnedUserId, ReceiptKind), ReceiptEntry>,
    seq: u64,
}

/// In-memory `m.receipt` state for every room this process has ever seen a receipt for. See the
/// module docs.
pub struct ReceiptRegistry {
    rooms: Mutex<HashMap<OwnedRoomId, RoomReceipts>>,
    counter: AtomicU64,
}

impl ReceiptRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rooms: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    fn next_seq(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Records `user_id`'s `kind` receipt for `event_id` in `room_id`, bumping this room's
    /// stamped sequence and returning the new value -- the caller
    /// (`crate::hub::SessionHub::set_receipt`) uses this to wake every joined member's long poll
    /// immediately, the same way `crate::typing::TypingRegistry::set` does.
    pub async fn set(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        kind: ReceiptKind,
        event_id: OwnedEventId,
        ts: u64,
    ) -> u64 {
        let mut rooms = self.rooms.lock().await;
        let entry = rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| RoomReceipts {
                by_user: HashMap::new(),
                seq: 0,
            });
        entry
            .by_user
            .insert((user_id.to_owned(), kind), ReceiptEntry { event_id, ts });
        entry.seq = self.next_seq();
        entry.seq
    }

    /// This room's current cursor (`0` if this process has never recorded a receipt here, which
    /// is always `<=` any client's baseline -- see `crate::typing`'s identical convention).
    pub async fn seq(&self, room_id: &RoomId) -> u64 {
        self.rooms.lock().await.get(room_id).map_or(0, |r| r.seq)
    }

    /// Builds the `m.receipt` event content for `room_id` as `viewer` would see it: every
    /// `m.read` receipt in the room, plus `viewer`'s own `m.read.private` receipts and nobody
    /// else's. Shape is the spec's own: `{event_id: {receipt_type: {user_id: {ts: ...}}}}`.
    /// Returns the empty object (not `null`) and the room's current cursor when there is nothing
    /// to report or the room has never had a receipt.
    pub async fn content_for(&self, room_id: &RoomId, viewer: &UserId) -> (Value, u64) {
        let rooms = self.rooms.lock().await;
        let Some(entry) = rooms.get(room_id) else {
            return (Value::Object(Map::new()), 0);
        };
        let mut by_event: HashMap<String, HashMap<&'static str, Map<String, Value>>> =
            HashMap::new();
        for ((user, kind), receipt) in &entry.by_user {
            if matches!(kind, ReceiptKind::ReadPrivate) && user.as_str() != viewer.as_str() {
                continue;
            }
            by_event
                .entry(receipt.event_id.to_string())
                .or_default()
                .entry(kind.as_str())
                .or_default()
                .insert(user.to_string(), json!({"ts": receipt.ts}));
        }
        let content: Map<String, Value> = by_event
            .into_iter()
            .map(|(event_id, kinds)| {
                let obj: Map<String, Value> = kinds
                    .into_iter()
                    .map(|(kind, users)| (kind.to_owned(), Value::Object(users)))
                    .collect();
                (event_id, Value::Object(obj))
            })
            .collect();
        (Value::Object(content), entry.seq)
    }
}

impl Default for ReceiptRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{event_id, room_id, user_id};

    #[tokio::test]
    async fn unknown_room_reports_empty_content_and_seq_zero() {
        let reg = ReceiptRegistry::new();
        let (content, seq) = reg
            .content_for(
                room_id!("!none:example.org"),
                user_id!("@alice:example.org"),
            )
            .await;
        assert_eq!(content, json!({}));
        assert_eq!(seq, 0);
    }

    #[tokio::test]
    async fn a_public_read_receipt_is_visible_to_every_viewer() {
        let reg = ReceiptRegistry::new();
        let room = room_id!("!r:example.org");
        let seq = reg
            .set(
                room,
                user_id!("@alice:example.org"),
                ReceiptKind::Read,
                event_id!("$one").to_owned(),
                42,
            )
            .await;
        assert!(seq > 0);
        for viewer in [user_id!("@alice:example.org"), user_id!("@bob:example.org")] {
            let (content, seq) = reg.content_for(room, viewer).await;
            assert_eq!(
                content,
                json!({"$one": {"m.read": {"@alice:example.org": {"ts": 42}}}})
            );
            assert!(seq > 0);
        }
    }

    #[tokio::test]
    async fn a_private_read_receipt_is_visible_only_to_its_own_sender() {
        let reg = ReceiptRegistry::new();
        let room = room_id!("!r:example.org");
        reg.set(
            room,
            user_id!("@alice:example.org"),
            ReceiptKind::ReadPrivate,
            event_id!("$one").to_owned(),
            7,
        )
        .await;

        let (own, _) = reg.content_for(room, user_id!("@alice:example.org")).await;
        assert_eq!(
            own,
            json!({"$one": {"m.read.private": {"@alice:example.org": {"ts": 7}}}})
        );

        let (other, _) = reg.content_for(room, user_id!("@bob:example.org")).await;
        assert_eq!(
            other,
            json!({}),
            "a private receipt must never be visible to anyone but its sender"
        );
    }

    #[tokio::test]
    async fn a_later_receipt_for_the_same_user_and_kind_replaces_the_earlier_one() {
        let reg = ReceiptRegistry::new();
        let room = room_id!("!r:example.org");
        reg.set(
            room,
            user_id!("@alice:example.org"),
            ReceiptKind::Read,
            event_id!("$one").to_owned(),
            1,
        )
        .await;
        reg.set(
            room,
            user_id!("@alice:example.org"),
            ReceiptKind::Read,
            event_id!("$two").to_owned(),
            2,
        )
        .await;
        let (content, _) = reg.content_for(room, user_id!("@alice:example.org")).await;
        assert_eq!(
            content,
            json!({"$two": {"m.read": {"@alice:example.org": {"ts": 2}}}})
        );
    }

    #[test]
    fn receipt_kind_round_trips_and_rejects_fully_read() {
        assert_eq!(ReceiptKind::parse("m.read"), Some(ReceiptKind::Read));
        assert_eq!(
            ReceiptKind::parse("m.read.private"),
            Some(ReceiptKind::ReadPrivate)
        );
        assert_eq!(ReceiptKind::parse("m.fully_read"), None);
        assert_eq!(ReceiptKind::parse("bogus"), None);
    }
}
