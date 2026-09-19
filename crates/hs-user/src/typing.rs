//! [`TypingRegistry`]: in-memory `m.typing` state, keyed by room.
//!
//! Typing notifications are the spec's own textbook example of ephemeral data: never persisted
//! (Synapse does not write them to its database either), lost on restart, and meaningful only to
//! whoever is currently long-polling `/sync`. That is a poor fit for `crate::store::UserStore`
//! (durable, per-user, backed by `hs-kv`) -- this registry is a second, much smaller piece of
//! state that [`crate::hub::SessionHub`] owns alongside its wakers, not a table.
//!
//! # Why a global counter, not a per-room boolean
//!
//! [`crate::sync::build`] needs to answer "has this room's typing state changed since the client's
//! last sync" without the client's token carrying a per-room cursor (`crate::token`'s module docs
//! explain why a token stays a handful of small integers, not a vector sized by room count). A
//! single monotonic counter shared by every room, stamped onto a room's entry every time that
//! room's typing set changes, gives every room an independent-enough cursor at the cost of one
//! `u64` on the wire (`SyncToken::typing_seq`): a room whose stamped value exceeds the client's
//! last-seen `typing_seq` has something new to report, and the client's next token simply carries
//! the highest stamp it has now observed across all of its rooms.
//!
//! # Expiry is lazy, not a background timer
//!
//! A `typing: true` call is only ever honored until `timeout` elapses (capped at
//! [`MAX_TYPING_TIMEOUT`]); rather than spawning a timer task per typing user, [`TypingRegistry::current`]
//! prunes anyone whose deadline has passed on every read. Read call sites (`crate::sync::build`'s
//! per-room loop, and `crate::sync::has_new_data`'s long-poll re-check) run often enough --
//! `crate::sync::E2E_POLL_INTERVAL` already forces the long-poll loop to recheck every 500ms for
//! the same reason to-device/device-list changes have no waker hook -- that "someone's typing
//! indicator silently disappears after their timeout" is noticed within one poll interval, not
//! only the next time somebody else calls the endpoint. Pruning bumps the counter too, so a
//! client blocked in a long poll is woken by an expiry exactly like any other change (see
//! `crate::sync::has_new_data`, which calls `current` for every joined room on each recheck).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tokio::sync::Mutex;

/// The longest a single `typing: true` call is honored for, regardless of what timeout the client
/// asked for. Generous enough that no real client's typing indicator (Element's default is 30s,
/// re-sent well before it lapses) is ever cut short, but bounded so a client that sends an
/// unreasonable timeout and then vanishes cannot pin an entry in this map forever.
pub const MAX_TYPING_TIMEOUT: Duration = Duration::from_secs(120);

struct RoomTyping {
    /// Still-typing users and when their notification expires.
    users: HashMap<OwnedUserId, Instant>,
    /// The counter's value the last time this room's typing set changed (by an explicit call or
    /// by a lazy expiry prune finding something to remove).
    seq: u64,
}

/// In-memory `m.typing` state for every room this process has ever seen a typing call for.
///
/// Cheap to construct with no room list up front: an entry is created lazily on first use
/// ([`TypingRegistry::set`]) and a room this process has never heard from reports `seq: 0` from
/// [`TypingRegistry::current`], which is always `<=` any client's baseline (`SyncToken::initial`
/// starts every cursor at `0`), so "no typing ever happened here" and "nothing changed since you
/// last synced" collapse into the same, correct, "do not include an ephemeral event" answer.
pub struct TypingRegistry {
    rooms: Mutex<HashMap<OwnedRoomId, RoomTyping>>,
    counter: AtomicU64,
}

impl TypingRegistry {
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

    /// Records `user_id`'s typing state in `room_id`: `typing: true` (re)inserts them with a
    /// fresh deadline (`timeout`, capped at [`MAX_TYPING_TIMEOUT`]); `typing: false` removes them
    /// outright (per spec, a client can cancel its own notification before the timeout by calling
    /// again with `typing: false`). Either way this counts as a change and bumps this room's
    /// stamped sequence, which the caller (`crate::hub::SessionHub::set_typing`) uses to wake
    /// every affected member's long poll immediately, without waiting for the next lazy re-check.
    pub async fn set(&self, room_id: &RoomId, user_id: &UserId, typing: bool, timeout: Duration) {
        let mut rooms = self.rooms.lock().await;
        let entry = rooms
            .entry(room_id.to_owned())
            .or_insert_with(|| RoomTyping {
                users: HashMap::new(),
                seq: 0,
            });
        if typing {
            let deadline = Instant::now() + timeout.min(MAX_TYPING_TIMEOUT);
            entry.users.insert(user_id.to_owned(), deadline);
        } else {
            entry.users.remove(user_id);
        }
        entry.seq = self.next_seq();
    }

    /// The currently-typing users in `room_id` (sorted, for deterministic output), pruned of
    /// anyone whose timeout has elapsed, plus this room's stamped sequence (`0` if this room has
    /// never had a typing call at all).
    pub async fn current(&self, room_id: &RoomId) -> (Vec<OwnedUserId>, u64) {
        let mut rooms = self.rooms.lock().await;
        let Some(entry) = rooms.get_mut(room_id) else {
            return (Vec::new(), 0);
        };
        let now = Instant::now();
        let before = entry.users.len();
        entry.users.retain(|_, deadline| *deadline > now);
        if entry.users.len() != before {
            entry.seq = self.next_seq();
        }
        let mut users: Vec<OwnedUserId> = entry.users.keys().cloned().collect();
        users.sort();
        (users, entry.seq)
    }
}

impl Default for TypingRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{room_id, user_id};

    #[tokio::test]
    async fn unknown_room_reports_no_one_typing_and_seq_zero() {
        let reg = TypingRegistry::new();
        let (users, seq) = reg.current(room_id!("!none:example.org")).await;
        assert!(users.is_empty());
        assert_eq!(seq, 0);
    }

    #[tokio::test]
    async fn set_typing_true_then_current_reports_the_user_and_a_nonzero_seq() {
        let reg = TypingRegistry::new();
        let room = room_id!("!r:example.org");
        reg.set(
            room,
            user_id!("@alice:example.org"),
            true,
            Duration::from_secs(30),
        )
        .await;
        let (users, seq) = reg.current(room).await;
        assert_eq!(users, vec![user_id!("@alice:example.org").to_owned()]);
        assert!(seq > 0);
    }

    #[tokio::test]
    async fn set_typing_false_removes_the_user_and_bumps_seq_again() {
        let reg = TypingRegistry::new();
        let room = room_id!("!r:example.org");
        reg.set(
            room,
            user_id!("@alice:example.org"),
            true,
            Duration::from_secs(30),
        )
        .await;
        let (_, first_seq) = reg.current(room).await;
        reg.set(
            room,
            user_id!("@alice:example.org"),
            false,
            Duration::from_secs(30),
        )
        .await;
        let (users, second_seq) = reg.current(room).await;
        assert!(users.is_empty());
        assert!(second_seq > first_seq);
    }

    #[tokio::test]
    async fn expiry_is_pruned_lazily_on_read_and_bumps_seq() {
        let reg = TypingRegistry::new();
        let room = room_id!("!r:example.org");
        reg.set(
            room,
            user_id!("@alice:example.org"),
            true,
            Duration::from_millis(10),
        )
        .await;
        let (_, first_seq) = reg.current(room).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (users, second_seq) = reg.current(room).await;
        assert!(users.is_empty(), "expired typer must be pruned");
        assert!(
            second_seq > first_seq,
            "an expiry-driven change must also bump the cursor, or an already-synced client \
             would never learn the typer stopped"
        );
    }

    #[tokio::test]
    async fn users_are_sorted_for_deterministic_output() {
        let reg = TypingRegistry::new();
        let room = room_id!("!r:example.org");
        reg.set(
            room,
            user_id!("@bob:example.org"),
            true,
            Duration::from_secs(30),
        )
        .await;
        reg.set(
            room,
            user_id!("@alice:example.org"),
            true,
            Duration::from_secs(30),
        )
        .await;
        let (users, _) = reg.current(room).await;
        assert_eq!(
            users,
            vec![
                user_id!("@alice:example.org").to_owned(),
                user_id!("@bob:example.org").to_owned(),
            ]
        );
    }
}
