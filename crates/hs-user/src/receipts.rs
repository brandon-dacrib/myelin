//! [`ReceiptRegistry`]: `m.read`/`m.read.private` read-receipt state, keyed by room, held in
//! memory and written through to the store.
//!
//! Mirrors `crate::typing::TypingRegistry`'s shape: a change counter (`crate::stamp`) stamped
//! onto whichever room's receipts changed, exposed to `crate::sync` through
//! [`crate::token::SyncToken::receipts_seq`].
//!
//! # Durable across a restart
//!
//! Read state is not ephemeral in the way typing is: a user does not expect "what have I read"
//! to reset because the server restarted. Every receipt is written through to
//! `crate::store::UserStore::put_receipt` together with its stamp, and a room's receipts are
//! loaded from the store the first time this process is asked about that room. Stamps are
//! restart-safe (`crate::stamp`'s module docs), so a receipt a client already saw before the
//! restart is not news to it afterwards, and one set after the restart is.
//!
//! A store write that fails is logged and the receipt is kept in memory: the receipt still
//! reaches every `/sync` this process answers, and only a restart would forget it.
//!
//! # Receipts from other servers
//!
//! A remote user's receipt (an `m.receipt` EDU, dispatched by `hs-cli`) goes through
//! [`ReceiptRegistry::set`] exactly as a local one does; the registry does not care whose
//! receipt it is.
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

use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::stamp::Stamps;
use crate::store::{DynUserStore, StoredReceipt};

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

/// `m.receipt` state for every room this process has been asked about, loaded from and written
/// through to the store when it has one. See the module docs.
pub struct ReceiptRegistry {
    /// A room present here has been loaded from the store (or had nothing there); one absent
    /// has not been looked at yet.
    rooms: Mutex<HashMap<OwnedRoomId, RoomReceipts>>,
    counter: Stamps,
    store: Option<DynUserStore>,
}

impl ReceiptRegistry {
    /// An empty registry that keeps nothing beyond this process.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rooms: Mutex::new(HashMap::new()),
            counter: Stamps::new(),
            store: None,
        }
    }

    /// A registry over `store`: receipts are written through to it and read back from it the
    /// first time a room is asked about. What [`crate::hub::SessionHub`] uses.
    #[must_use]
    pub fn with_store(store: DynUserStore) -> Self {
        Self {
            store: Some(store),
            ..Self::new()
        }
    }

    /// Makes sure `room_id`'s receipts are in `rooms`, reading them from the store if this is
    /// the first time the room is asked about. A store that cannot be read is logged, and the
    /// room starts empty here (so the failure is not retried on every sync).
    async fn load<'a>(
        &self,
        rooms: &'a mut HashMap<OwnedRoomId, RoomReceipts>,
        room_id: &RoomId,
    ) -> &'a mut RoomReceipts {
        if !rooms.contains_key(room_id) {
            let mut loaded = RoomReceipts {
                by_user: HashMap::new(),
                seq: 0,
            };
            if let Some(store) = &self.store {
                match store.list_room_receipts(room_id).await {
                    Ok(rows) => {
                        for row in rows {
                            let (Some(kind), Ok(user), Ok(event_id)) = (
                                ReceiptKind::parse(&row.kind),
                                UserId::parse(row.user_id.as_str()),
                                EventId::parse(row.event_id.as_str()),
                            ) else {
                                continue;
                            };
                            self.counter.observe(row.seq);
                            loaded.seq = loaded.seq.max(row.seq);
                            loaded.by_user.insert(
                                (user, kind),
                                ReceiptEntry {
                                    event_id,
                                    ts: row.ts,
                                },
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            %room_id,
                            %error,
                            "could not read a room's stored receipts; starting from none"
                        );
                    }
                }
            }
            rooms.insert(room_id.to_owned(), loaded);
        }
        rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| RoomReceipts {
                by_user: HashMap::new(),
                seq: 0,
            })
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
        let entry = self.load(&mut rooms, room_id).await;
        let seq = self.counter.next();
        entry.by_user.insert(
            (user_id.to_owned(), kind),
            ReceiptEntry {
                event_id: event_id.clone(),
                ts,
            },
        );
        entry.seq = seq;
        drop(rooms);
        if let Some(store) = &self.store {
            let row = StoredReceipt {
                user_id: user_id.to_string(),
                kind: kind.as_str().to_owned(),
                event_id: event_id.to_string(),
                ts,
                seq,
            };
            if let Err(error) = store.put_receipt(room_id, &row).await {
                tracing::warn!(
                    %room_id,
                    %user_id,
                    %error,
                    "could not store a receipt; it is kept in memory until the next restart"
                );
            }
        }
        seq
    }

    /// Forgets what is cached for `room_id`, so that the next call about the room reads the
    /// store again, and raises the counter past `seq`, the stamp of the receipt that made the
    /// cache stale. How another replica's receipt reaches this one
    /// (`crate::cluster::EphemeralUpdate::Receipt`): the receipt itself is in the shared store,
    /// written by the replica that took it, and only the cache here is behind. Returns whether
    /// anything was cached.
    pub async fn forget(&self, room_id: &RoomId, seq: u64) -> bool {
        self.counter.observe(seq);
        self.rooms.lock().await.remove(room_id).is_some()
    }

    /// This room's current cursor (`0` if this room has never had a receipt, which is always
    /// `<=` any client's baseline -- see `crate::typing`'s identical convention).
    pub async fn seq(&self, room_id: &RoomId) -> u64 {
        let mut rooms = self.rooms.lock().await;
        self.load(&mut rooms, room_id).await.seq
    }

    /// Builds the `m.receipt` event content for `room_id` as `viewer` would see it: every
    /// `m.read` receipt in the room, plus `viewer`'s own `m.read.private` receipts and nobody
    /// else's. Shape is the spec's own: `{event_id: {receipt_type: {user_id: {ts: ...}}}}`.
    /// Returns the empty object (not `null`) and the room's current cursor when there is nothing
    /// to report or the room has never had a receipt.
    pub async fn content_for(&self, room_id: &RoomId, viewer: &UserId) -> (Value, u64) {
        let mut rooms = self.rooms.lock().await;
        let entry = self.load(&mut rooms, room_id).await;
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

    fn store_over(backend: &hs_kv::memory::MemoryBackend) -> DynUserStore {
        std::sync::Arc::new(crate::store::tables::TablesUserStore::open(backend.clone()).unwrap())
    }

    /// The restart property: a second registry over the same store (a new process) reports the
    /// receipts the first one recorded, with the same cursor, and a receipt set afterwards is
    /// newer than every one of them.
    #[tokio::test]
    async fn receipts_outlive_the_registry_that_recorded_them() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let room = room_id!("!r:example.org");
        let before = ReceiptRegistry::with_store(store_over(&backend));
        before
            .set(
                room,
                user_id!("@alice:example.org"),
                ReceiptKind::Read,
                event_id!("$one").to_owned(),
                11,
            )
            .await;
        let old_seq = before
            .set(
                room,
                user_id!("@alice:example.org"),
                ReceiptKind::ReadPrivate,
                event_id!("$two").to_owned(),
                12,
            )
            .await;
        drop(before);

        let after = ReceiptRegistry::with_store(store_over(&backend));
        let (content, seq) = after
            .content_for(room, user_id!("@alice:example.org"))
            .await;
        assert_eq!(
            content,
            json!({
                "$one": {"m.read": {"@alice:example.org": {"ts": 11}}},
                "$two": {"m.read.private": {"@alice:example.org": {"ts": 12}}},
            })
        );
        assert_eq!(seq, old_seq, "a restored room keeps the cursor it had");
        let (for_bob, _) = after.content_for(room, user_id!("@bob:example.org")).await;
        assert_eq!(
            for_bob,
            json!({"$one": {"m.read": {"@alice:example.org": {"ts": 11}}}}),
            "a restored private receipt is still private"
        );

        let new_seq = after
            .set(
                room,
                user_id!("@bob:example.org"),
                ReceiptKind::Read,
                event_id!("$two").to_owned(),
                13,
            )
            .await;
        assert!(new_seq > old_seq);
    }

    /// The other-replica property: two registries over one store are two replicas' caches.
    /// A receipt set through one is not seen by the other, which loaded the room before, until
    /// it is told to forget the room; then it reads the store and has it, with the writer's
    /// stamp, and stamps nothing older than that itself.
    #[tokio::test]
    async fn a_forgotten_room_is_read_from_the_store_again() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let room = room_id!("!r:example.org");
        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let a = ReceiptRegistry::with_store(store_over(&backend));
        let b = ReceiptRegistry::with_store(store_over(&backend));
        let first = a
            .set(
                room,
                alice,
                ReceiptKind::Read,
                event_id!("$one").to_owned(),
                1,
            )
            .await;
        // B loads the room now, and serves that copy from here on.
        assert_eq!(b.seq(room).await, first);

        let second = a
            .set(
                room,
                alice,
                ReceiptKind::Read,
                event_id!("$two").to_owned(),
                2,
            )
            .await;
        assert_eq!(b.seq(room).await, first, "B's cache is behind, as expected");
        let (stale, _) = b.content_for(room, bob).await;
        assert_eq!(
            stale,
            json!({"$one": {"m.read": {"@alice:example.org": {"ts": 1}}}})
        );

        assert!(b.forget(room, second).await);
        assert_eq!(b.seq(room).await, second);
        let (fresh, seq) = b.content_for(room, bob).await;
        assert_eq!(
            fresh,
            json!({"$two": {"m.read": {"@alice:example.org": {"ts": 2}}}})
        );
        assert_eq!(seq, second);
        assert!(
            b.set(
                room,
                bob,
                ReceiptKind::Read,
                event_id!("$two").to_owned(),
                3
            )
            .await
                > second,
            "B's own stamps are past what it learned of"
        );

        // Forgetting a room never cached is nothing, and still raises the floor.
        assert!(
            !b.forget(room_id!("!other:example.org"), second + 1_000)
                .await
        );
        assert!(
            b.set(
                room,
                bob,
                ReceiptKind::Read,
                event_id!("$two").to_owned(),
                4
            )
            .await
                > second + 1_000
        );
    }
}
