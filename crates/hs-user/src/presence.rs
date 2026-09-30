//! [`PresenceRegistry`]: `m.presence` state, keyed by user, held in memory and written through
//! to the store.
//!
//! Presence shares typing's shape almost exactly (`crate::typing`'s module docs explain the
//! general pattern this mirrors: a change counter stamped onto each user's record, compared
//! against a cursor carried in [`crate::token::SyncToken`] -- here `presence_seq`), with these
//! differences:
//!
//! - Presence is keyed by the user *whose* presence it is, not by room -- a presence update wakes
//!   every user who currently shares a joined room with that user (`crate::hub::SessionHub::set_presence`),
//!   the same privacy scope `crate::sync::shared_users` already enforces for `device_lists`.
//! - There is no expiry to prune lazily: a user's presence stays exactly what they last set it to
//!   until they set it again (or, per the spec, until Synapse-style idle/logout heuristics mark
//!   them offline automatically -- **not implemented here**; see this crate's status file for why
//!   that is deferred rather than half-built).
//! - **It survives a restart.** Every change is written through to
//!   `crate::store::UserStore::put_presence` with its stamp, and a user's record is read back the
//!   first time this process is asked about them (a user with no stored record is remembered as
//!   having none, so the store is asked once). The stamps are restart-safe (`crate::stamp`), so a
//!   record a client saw before the restart is not news to it afterwards. `last_active` is a
//!   wall-clock time for the same reason: an `Instant` means nothing to the next process.
//!   Polling `/sync` refreshes `last_active` without a change ([`PresenceRegistry::touch`]); that
//!   refresh is written at most once a minute per user, not on every poll.
//! - A remote user's presence (an `m.presence` EDU, dispatched by `hs-cli`) is recorded through
//!   [`PresenceRegistry::set_remote`], which takes the remote server's `last_active_ago` and
//!   `currently_active` as given.

use std::collections::HashMap;

use ruma::{OwnedUserId, UserId};
use tokio::sync::Mutex;

use crate::stamp::{Stamps, now_ms};
use crate::store::{DynUserStore, StoredPresence};

/// How stale a stored `last_active` may get while a user keeps polling `/sync` without changing
/// state. See the module docs.
const LAST_ACTIVE_WRITE_INTERVAL_MS: u64 = 60_000;

/// One user's current presence, as this registry has it.
#[derive(Debug, Clone)]
pub struct PresenceRecord {
    /// `"online"`, `"unavailable"` or `"offline"` -- validated at the HTTP layer
    /// (`crate::routes::presence::put_status`) for a local user, and by the EDU dispatcher for a
    /// remote one; stored as-is here.
    pub presence: String,
    /// The client-supplied free-text status message, if any.
    pub status_msg: Option<String>,
    /// When the user was last known active, in milliseconds since the Unix epoch.
    pub last_active_ms: u64,
    /// This record's stamp on [`PresenceRegistry`]'s change counter, for the same
    /// changed-since-a-cursor comparison `crate::typing::TypingRegistry` uses.
    pub seq: u64,
    /// What the user's own server said about `currently_active` (remote users only).
    pub remote_currently_active: Option<bool>,
    /// The `last_active_ms` most recently written to the store.
    persisted_active_ms: u64,
}

impl PresenceRecord {
    /// Milliseconds since this user was last known active, for the response's `last_active_ago`
    /// field.
    #[must_use]
    pub fn last_active_ago_ms(&self) -> u64 {
        now_ms().saturating_sub(self.last_active_ms)
    }

    /// The response's `currently_active`: what a remote user's server said, or for a local user
    /// whether they are `online`.
    #[must_use]
    pub fn currently_active(&self) -> bool {
        self.remote_currently_active
            .unwrap_or(self.presence == "online")
    }

    fn stored(&self) -> StoredPresence {
        StoredPresence {
            presence: self.presence.clone(),
            status_msg: self.status_msg.clone(),
            last_active_ms: self.last_active_ms,
            seq: self.seq,
            currently_active: self.remote_currently_active,
        }
    }

    fn from_stored(stored: StoredPresence) -> Self {
        Self {
            presence: stored.presence,
            status_msg: stored.status_msg,
            last_active_ms: stored.last_active_ms,
            seq: stored.seq,
            remote_currently_active: stored.currently_active,
            persisted_active_ms: stored.last_active_ms,
        }
    }
}

/// Presence state for every user this process has been asked about. A user with no record
/// simply has none -- see [`crate::routes::presence::get_status`] for how the route
/// distinguishes "never set" (defaults, per spec) from "no such user" (404, checked against
/// `hs-auth`'s own user table, not this registry).
pub struct PresenceRegistry {
    /// `Some` for a user with a record, `None` for one the store was asked about and had
    /// nothing for; a user absent from the map has not been looked up yet.
    users: Mutex<HashMap<OwnedUserId, Option<PresenceRecord>>>,
    counter: Stamps,
    store: Option<DynUserStore>,
}

impl PresenceRegistry {
    /// An empty registry that keeps nothing beyond this process.
    #[must_use]
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            counter: Stamps::new(),
            store: None,
        }
    }

    /// A registry over `store`: every change is written through to it, and a user's record is
    /// read back from it the first time they are asked about. What
    /// [`crate::hub::SessionHub`] uses.
    #[must_use]
    pub fn with_store(store: DynUserStore) -> Self {
        Self {
            store: Some(store),
            ..Self::new()
        }
    }

    /// `user_id`'s slot in `users`, read from the store if this is the first time they are asked
    /// about. A store that cannot be read is logged and treated as holding nothing.
    async fn slot<'a>(
        &self,
        users: &'a mut HashMap<OwnedUserId, Option<PresenceRecord>>,
        user_id: &UserId,
    ) -> &'a mut Option<PresenceRecord> {
        if !users.contains_key(user_id) {
            let loaded = match &self.store {
                Some(store) => match store.get_presence(user_id).await {
                    Ok(stored) => stored.map(|stored| {
                        self.counter.observe(stored.seq);
                        PresenceRecord::from_stored(stored)
                    }),
                    Err(error) => {
                        tracing::warn!(
                            %user_id,
                            %error,
                            "could not read a user's stored presence; starting from none"
                        );
                        None
                    }
                },
                None => None,
            };
            users.insert(user_id.to_owned(), loaded);
        }
        users.entry(user_id.to_owned()).or_insert(None)
    }

    /// Writes `record` through to the store, if there is one, and notes what was written.
    async fn persist(&self, user_id: &UserId, record: &mut PresenceRecord) {
        let Some(store) = &self.store else {
            return;
        };
        match store.put_presence(user_id, &record.stored()).await {
            Ok(()) => record.persisted_active_ms = record.last_active_ms,
            Err(error) => tracing::warn!(
                %user_id,
                %error,
                "could not store a presence change; it is kept in memory until the next restart"
            ),
        }
    }

    /// Records `user_id`'s new presence state, stamping it and refreshing `last_active` to now
    /// (matches Synapse: any presence-setting call, not only transitioning to `online`, counts
    /// as activity). Returns the new stamp.
    pub async fn set(&self, user_id: &UserId, presence: String, status_msg: Option<String>) -> u64 {
        let mut users = self.users.lock().await;
        let slot = self.slot(&mut users, user_id).await;
        let seq = self.counter.next();
        let record = slot.insert(PresenceRecord {
            presence,
            status_msg,
            last_active_ms: now_ms(),
            seq,
            remote_currently_active: None,
            persisted_active_ms: 0,
        });
        self.persist(user_id, record).await;
        seq
    }

    /// Records a remote user's presence as their own server reported it (an `m.presence` EDU):
    /// `last_active_ago` is relative to now, `currently_active` is kept as given. Always a
    /// change -- the remote server only sends what changed. Returns the new stamp.
    pub async fn set_remote(
        &self,
        user_id: &UserId,
        presence: String,
        status_msg: Option<String>,
        last_active_ago_ms: Option<u64>,
        currently_active: Option<bool>,
    ) -> u64 {
        let mut users = self.users.lock().await;
        let slot = self.slot(&mut users, user_id).await;
        let seq = self.counter.next();
        let record = slot.insert(PresenceRecord {
            presence,
            status_msg,
            last_active_ms: now_ms().saturating_sub(last_active_ago_ms.unwrap_or(0)),
            seq,
            remote_currently_active: currently_active,
            persisted_active_ms: 0,
        });
        self.persist(user_id, record).await;
        seq
    }

    /// Records `presence` for `user_id` without touching their status message, and counts as a
    /// change -- a new stamp, and so a presence event other people's syncs will carry -- only when
    /// the state actually differs from what is stored. Returns the new stamp when it was a
    /// change, `None` when it was not.
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
    pub async fn touch(&self, user_id: &UserId, presence: &str) -> Option<u64> {
        let mut users = self.users.lock().await;
        let slot = self.slot(&mut users, user_id).await;
        let now = now_ms();
        match slot.as_mut() {
            Some(existing) if existing.presence == presence => {
                existing.last_active_ms = now;
                if now.saturating_sub(existing.persisted_active_ms) >= LAST_ACTIVE_WRITE_INTERVAL_MS
                {
                    self.persist(user_id, existing).await;
                }
                None
            }
            Some(existing) => {
                existing.presence = presence.to_owned();
                existing.last_active_ms = now;
                existing.remote_currently_active = None;
                existing.seq = self.counter.next();
                self.persist(user_id, existing).await;
                Some(existing.seq)
            }
            None => {
                let record = slot.insert(PresenceRecord {
                    presence: presence.to_owned(),
                    status_msg: None,
                    last_active_ms: now,
                    seq: self.counter.next(),
                    remote_currently_active: None,
                    persisted_active_ms: 0,
                });
                self.persist(user_id, record).await;
                Some(record.seq)
            }
        }
    }

    /// Gives `user_id`'s record a new stamp without changing what it says, so that it counts as
    /// news to everyone whose sync token predates this moment. Returns the new stamp, or `None`
    /// when there was no record to restamp.
    ///
    /// Presence has one sequence for the whole server, but the *audience* of a record is
    /// everyone who shares a room with its owner, and that set grows. When somebody joins a
    /// room, the people already in it have tokens newer than the joiner's last presence change
    /// -- they were syncing while the joiner was elsewhere -- so by stamp alone they would never
    /// be sent it. A join is news about who you can see; this makes it news in the stream too.
    pub async fn restamp(&self, user_id: &UserId) -> Option<u64> {
        let mut users = self.users.lock().await;
        match self.slot(&mut users, user_id).await {
            Some(existing) => {
                existing.seq = self.counter.next();
                self.persist(user_id, existing).await;
                Some(existing.seq)
            }
            None => None,
        }
    }

    /// Forgets what is cached for `user_id` -- a record, or the knowledge that there is none --
    /// so that the next call about them reads the store again, and raises the counter past
    /// `seq`, the stamp of the change that made the cache stale. How another replica's presence
    /// change reaches this one (`crate::cluster::EphemeralUpdate::Presence`): the record is in
    /// the shared store, written by the replica that took the change. Returns whether a record
    /// was cached.
    pub async fn forget(&self, user_id: &UserId, seq: u64) -> bool {
        self.counter.observe(seq);
        self.users.lock().await.remove(user_id).flatten().is_some()
    }

    /// This user's current record, if there is one.
    pub async fn get(&self, user_id: &UserId) -> Option<PresenceRecord> {
        let mut users = self.users.lock().await;
        self.slot(&mut users, user_id).await.clone()
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

        assert!(
            reg.touch(uid, "online").await.is_some(),
            "the first one is a change"
        );
        let first = reg.get(uid).await.unwrap().seq;
        for _ in 0..10 {
            assert!(
                reg.touch(uid, "online").await.is_none(),
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

        assert!(reg.touch(uid, "unavailable").await.is_some());
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

    fn store_over(backend: &hs_kv::memory::MemoryBackend) -> DynUserStore {
        std::sync::Arc::new(crate::store::tables::TablesUserStore::open(backend.clone()).unwrap())
    }

    /// The restart property: a second registry over the same store (a new process) has the
    /// record the first one set -- state, status message, stamp -- and knows a user who never
    /// had one has none. A change after the restart is newer than anything before it.
    #[tokio::test]
    async fn presence_outlives_the_registry_that_recorded_it() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let alice = user_id!("@alice:example.org");
        let before = PresenceRegistry::with_store(store_over(&backend));
        let old_seq = before
            .set(alice, "unavailable".to_owned(), Some("lunch".to_owned()))
            .await;
        drop(before);

        let after = PresenceRegistry::with_store(store_over(&backend));
        let record = after.get(alice).await.expect("the record was restored");
        assert_eq!(record.presence, "unavailable");
        assert_eq!(record.status_msg.as_deref(), Some("lunch"));
        assert_eq!(record.seq, old_seq);
        assert!(record.last_active_ago_ms() < 5000);
        assert!(after.get(user_id!("@nobody:example.org")).await.is_none());

        assert!(after.touch(alice, "online").await.is_some());
        assert!(after.get(alice).await.unwrap().seq > old_seq);
    }

    /// The other-replica property: two registries over one store are two replicas' caches. A
    /// change made through one is invisible to the other, which has the user cached (as a
    /// record, or as "no record"), until it is told to forget them; then it reads the store.
    #[tokio::test]
    async fn a_forgotten_user_is_read_from_the_store_again() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let alice = user_id!("@alice:example.org");
        let a = PresenceRegistry::with_store(store_over(&backend));
        let b = PresenceRegistry::with_store(store_over(&backend));
        // B asks first and caches "no record".
        assert!(b.get(alice).await.is_none());

        let first = a.set(alice, "online".to_owned(), None).await;
        assert!(
            b.get(alice).await.is_none(),
            "B's cache is behind, as expected"
        );
        assert!(
            !b.forget(alice, first).await,
            "nothing but a None was cached"
        );
        assert_eq!(b.get(alice).await.unwrap().seq, first);

        let second = a
            .set(alice, "unavailable".to_owned(), Some("lunch".to_owned()))
            .await;
        assert_eq!(b.get(alice).await.unwrap().presence, "online");
        assert!(b.forget(alice, second).await);
        let record = b.get(alice).await.unwrap();
        assert_eq!(record.presence, "unavailable");
        assert_eq!(record.status_msg.as_deref(), Some("lunch"));
        assert_eq!(record.seq, second);
        // B's own stamps are past what it learned of, and polling with the state the store
        // already has is not a change on B either.
        assert!(b.touch(alice, "unavailable").await.is_none());
        assert!(b.touch(alice, "online").await.unwrap() > second);
    }

    #[tokio::test]
    async fn a_remote_record_keeps_what_its_server_said() {
        let reg = PresenceRegistry::new();
        let bob = user_id!("@bob:remote.example");
        reg.set_remote(bob, "online".to_owned(), None, Some(60_000), Some(false))
            .await;
        let record = reg.get(bob).await.unwrap();
        assert!(!record.currently_active());
        assert!(record.last_active_ago_ms() >= 60_000);
        assert!(record.last_active_ago_ms() < 65_000);
    }
}
