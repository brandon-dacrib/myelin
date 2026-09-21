//! [`PresenceRegistry`]: in-memory `m.presence` state, keyed by user.
//!
//! Deferred in this crate's first pass (`crate::sync`'s module docs used to say "the top-level
//! `presence.events` is always `[]`" -- this module and `crate::routes::presence` are what
//! replaces that). Presence shares typing's shape almost exactly (`crate::typing`'s module docs
//! explain the general pattern this mirrors: a global counter stamped onto each user's record,
//! compared against a cursor carried in [`crate::token::SyncToken`] -- here `presence_seq`, a
//! field this token already reserved before either module existed), with two differences:
//!
//! - Presence is keyed by the user *whose* presence it is, not by room -- a presence update wakes
//!   every user who currently shares a joined room with that user (`crate::hub::SessionHub::set_presence`),
//!   the same privacy scope `crate::sync::shared_users` already enforces for `device_lists`.
//! - There is no expiry to prune lazily: a user's presence stays exactly what they last set it to
//!   until they set it again (or, per the spec, until Synapse-style idle/logout heuristics mark
//!   them offline automatically -- **not implemented here**; see this crate's status file for why
//!   that is deferred rather than half-built).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ruma::{OwnedUserId, UserId};
use tokio::sync::Mutex;

/// One user's current presence, as this registry has it. `last_active` is an [`Instant`]
/// (process-local monotonic clock) rather than a wall-clock timestamp because the only thing any
/// caller ever does with it is compute an elapsed duration (`last_active_ago`) -- see
/// [`PresenceRecord::last_active_ago_ms`].
#[derive(Debug, Clone)]
pub struct PresenceRecord {
    /// `"online"`, `"unavailable"` or `"offline"` -- validated at the HTTP layer
    /// (`crate::routes::presence::put_status`), stored as-is here.
    pub presence: String,
    /// The client-supplied free-text status message, if any.
    pub status_msg: Option<String>,
    last_active: Instant,
    /// This record's stamp on [`PresenceRegistry`]'s shared counter, for the same
    /// changed-since-a-cursor comparison `crate::typing::TypingRegistry` uses.
    pub seq: u64,
}

impl PresenceRecord {
    /// Milliseconds since this user was last known active (last called `PUT .../presence/.../status`),
    /// for the response's `last_active_ago` field.
    #[must_use]
    pub fn last_active_ago_ms(&self) -> u64 {
        u64::try_from(self.last_active.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// In-memory presence state for every local user this process has ever seen a presence call for.
/// A user this process has never heard from simply has no record -- see
/// [`crate::routes::presence::get_status`] for how the route distinguishes "never set" (defaults,
/// per spec) from "no such user" (404, checked against `hs-auth`'s own user table, not this
/// registry).
pub struct PresenceRegistry {
    users: Mutex<HashMap<OwnedUserId, PresenceRecord>>,
    counter: AtomicU64,
}

impl PresenceRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// Records `user_id`'s new presence state, bumping the shared counter and refreshing
    /// `last_active` to now (matches Synapse: any presence-setting call, not only transitioning to
    /// `online`, counts as activity). Returns the new stamp, for a caller that wants it without a
    /// second lookup (none currently do, but mirrors [`crate::typing::TypingRegistry::set`]'s
    /// shape).
    pub async fn set(&self, user_id: &UserId, presence: String, status_msg: Option<String>) -> u64 {
        let seq = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let mut users = self.users.lock().await;
        users.insert(
            user_id.to_owned(),
            PresenceRecord {
                presence,
                status_msg,
                last_active: Instant::now(),
                seq,
            },
        );
        seq
    }

    /// Records `presence` for `user_id` without touching their status message, and counts as a
    /// change -- a new stamp, and so a presence event other people's syncs will carry -- only when
    /// the state actually differs from what is stored. Returns whether it was a change.
    ///
    /// This exists because `GET /sync` marks its caller online on *every* poll: that is the
    /// spec's default when `set_presence` is omitted. Bumping the stamp each time would wake
    /// everyone sharing a room with them, whose own `/sync` would return, mark *them* online, and
    /// wake everyone again -- a feedback loop with nothing to damp it. Refreshing `last_active`
    /// without a new stamp keeps `last_active_ago` honest and leaves the loop unstarted.
    ///
    /// The status message is deliberately preserved: `set_presence` on `/sync` says what state
    /// the client is in, not what the user wants to tell people, and clearing somebody's "On
    /// holiday until Monday" because their client polled would be wrong.
    pub async fn touch(&self, user_id: &UserId, presence: &str) -> bool {
        let mut users = self.users.lock().await;
        match users.get_mut(user_id) {
            Some(existing) if existing.presence == presence => {
                existing.last_active = Instant::now();
                false
            }
            Some(existing) => {
                existing.presence = presence.to_owned();
                existing.last_active = Instant::now();
                existing.seq = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
                true
            }
            None => {
                let seq = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
                users.insert(
                    user_id.to_owned(),
                    PresenceRecord {
                        presence: presence.to_owned(),
                        status_msg: None,
                        last_active: Instant::now(),
                        seq,
                    },
                );
                true
            }
        }
    }

    /// Gives `user_id`'s record a new stamp without changing what it says, so that it counts as
    /// news to everyone whose sync token predates this moment. Returns whether there was a record
    /// to restamp.
    ///
    /// Presence has one sequence for the whole server, but the *audience* of a record is
    /// everyone who shares a room with its owner, and that set grows. When somebody joins a
    /// room, the people already in it have tokens newer than the joiner's last presence change
    /// -- they were syncing while the joiner was elsewhere -- so by stamp alone they would never
    /// be sent it. A join is news about who you can see; this makes it news in the stream too.
    pub async fn restamp(&self, user_id: &UserId) -> bool {
        let mut users = self.users.lock().await;
        match users.get_mut(user_id) {
            Some(existing) => {
                existing.seq = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
                true
            }
            None => false,
        }
    }

    /// This user's current record, if this process has ever recorded one.
    pub async fn get(&self, user_id: &UserId) -> Option<PresenceRecord> {
        self.users.lock().await.get(user_id).cloned()
    }
}

impl Default for PresenceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    /// The property the whole `set_presence`-on-`/sync` design rests on. Every poll marks the
    /// caller online; if each one produced a new stamp, it would wake everyone sharing a room with
    /// them, whose syncs would return and mark *them* online, and so on without end.
    #[tokio::test]
    async fn repeatedly_touching_the_same_state_is_not_a_change() {
        let reg = PresenceRegistry::new();
        let uid = user_id!("@alice:example.org");

        assert!(reg.touch(uid, "online").await, "the first one is a change");
        let first = reg.get(uid).await.unwrap().seq;
        for _ in 0..10 {
            assert!(
                !reg.touch(uid, "online").await,
                "polling again is not a presence change"
            );
        }
        assert_eq!(
            reg.get(uid).await.unwrap().seq,
            first,
            "the stamp must not move, or every sync wakes every room-mate"
        );
    }

    #[tokio::test]
    async fn touching_a_different_state_is_a_change() {
        let reg = PresenceRegistry::new();
        let uid = user_id!("@alice:example.org");
        reg.touch(uid, "online").await;
        let first = reg.get(uid).await.unwrap().seq;

        assert!(reg.touch(uid, "unavailable").await);
        let record = reg.get(uid).await.unwrap();
        assert_eq!(record.presence, "unavailable");
        assert!(record.seq > first);
    }

    /// A client polling `/sync` says where it is, not what the user wants people to read. Clearing
    /// somebody's "On holiday until Monday" because their phone polled would be wrong.
    #[tokio::test]
    async fn touching_presence_leaves_the_status_message_alone() {
        let reg = PresenceRegistry::new();
        let uid = user_id!("@alice:example.org");
        reg.set(uid, "online".to_owned(), Some("On holiday".to_owned()))
            .await;

        reg.touch(uid, "unavailable").await;

        let record = reg.get(uid).await.unwrap();
        assert_eq!(record.presence, "unavailable");
        assert_eq!(record.status_msg.as_deref(), Some("On holiday"));
    }

    #[tokio::test]
    async fn unknown_user_has_no_record() {
        let reg = PresenceRegistry::new();
        assert!(reg.get(user_id!("@ghost:example.org")).await.is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips_presence_and_status_msg() {
        let reg = PresenceRegistry::new();
        let uid = user_id!("@alice:example.org");
        reg.set(uid, "online".to_owned(), Some("hi".to_owned()))
            .await;
        let record = reg.get(uid).await.unwrap();
        assert_eq!(record.presence, "online");
        assert_eq!(record.status_msg.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn each_set_bumps_the_shared_counter() {
        let reg = PresenceRegistry::new();
        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let s1 = reg.set(alice, "online".to_owned(), None).await;
        let s2 = reg.set(bob, "online".to_owned(), None).await;
        assert!(s2 > s1);
        assert_eq!(reg.get(alice).await.unwrap().seq, s1);
        assert_eq!(reg.get(bob).await.unwrap().seq, s2);
    }

    #[tokio::test]
    async fn last_active_ago_is_small_immediately_after_set() {
        let reg = PresenceRegistry::new();
        let uid = user_id!("@alice:example.org");
        reg.set(uid, "online".to_owned(), None).await;
        let record = reg.get(uid).await.unwrap();
        assert!(record.last_active_ago_ms() < 5000);
    }
}
