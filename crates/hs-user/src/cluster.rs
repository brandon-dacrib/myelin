//! What `/sync` needs from a cluster of replicas, and the room mirror a non-owner reads through.
//!
//! In `mode=cluster` every room has one owner (`docs/scaling.md`), and only the owner's
//! [`crate::hub::SessionHub`] turns the room's updates into feed entries. Every replica reads the
//! same feeds, memberships and device cursors from the shared store, so any replica can answer
//! any user's `/sync` -- a session lives wherever its request arrived, and nothing is forwarded.
//! What the shared store cannot do on its own is *wake* a long-poll on another replica, or
//! promise that a write the client just made through replica A is in the feeds before replica B
//! answers. Those two are this module's business:
//!
//! - **The wake.** After the owner's hub has fed one update it hands a [`RoomWake`] to
//!   [`SessionCluster::publish`], which sends it to every other live replica (batched per peer,
//!   see [`WakeBatch`]). The receiving hub ([`crate::hub::SessionHub::receive_wakes`]) wakes
//!   those users' long-polls. Broadcast, not registration: O(replicas) small messages per event,
//!   and no per-room interest table that a failover would have to keep consistent.
//! - **Read-your-writes.** A [`WakeBatch`] carries the sender's consumed high-water mark on its
//!   own registry stream. Before reading, a `/sync` asks every peer what it has published
//!   ([`SessionCluster::peer_positions`]) and waits, within the same bounded budget the
//!   single-replica wait already had, until it has received each peer's batches up to that
//!   number ([`crate::hub::SessionHub::settle_before_read`]). The owner sends a batch only after
//!   its feed entries are durable, so when the batch has arrived the entries are readable.
//! - **The mirror.** A replica must not read a room it does not own through its own registry:
//!   that actor would be a stale copy, never told about the owner's writes. [`RoomMirror`] is a
//!   read-only [`RoomActor::load`] snapshot per room, checked against the store's durable
//!   timeline head on every access and reloaded when the store is ahead. The store is the source
//!   of truth; the wake is only the doorbell, the same rule `hs-appservice`'s pump follows.
//! - **Typing, receipts and presence.** None of the three is on the registry stream, so the wake
//!   above never carries them, and each replica's registries (`crate::typing`,
//!   `crate::receipts`, `crate::presence`) are its own memory. A replica that changes one of
//!   them -- a client's `PUT .../typing`, a receipt, a presence change, an EDU from another
//!   server -- hands an [`EphemeralUpdate`] to [`SessionCluster::publish_ephemeral`], which
//!   travels in the same [`WakeBatch`] to every other live replica. The receiver
//!   ([`crate::hub::SessionHub::receive_wakes`]) applies a typing update to its own registry
//!   (typing is nobody's durable state, so the update carries it whole and each replica expires
//!   it on its own clock) and, for a receipt or a presence change, which *are* durable, forgets
//!   what it had cached for that room or user so that its next read comes from the store; then
//!   it wakes the long-polls concerned. A receiver never re-publishes what it was sent.
//!
//! The trait is implemented over the real mesh in `hs-cli` (`crate::sync_cluster` there); this
//! crate defines only what it needs so that a test can stand two hubs up in one process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hs_kv::KvBackend;
use hs_room::RoomError;
use hs_room::actor::{RoomActor, RoomActorHandle};
use hs_room::identity::HomeserverIdentity;
use hs_room::persist::{Tables, TimelineKey};
use hs_tables::keyspace::TypedKeyspace;
use ruma::{OwnedRoomId, OwnedUserId, RoomId};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;

/// One change to typing, receipt or presence state made on one replica, for every other
/// replica to apply. See the module docs, "Typing, receipts and presence".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EphemeralUpdate {
    /// `user_id` started (`typing: true`, for `timeout_ms`) or stopped typing in `room_id`.
    /// Carried whole: typing is in no store, so the receiver's registry is the only copy it
    /// will have, and the timeout lets it expire the entry itself.
    Typing {
        /// The room.
        room_id: OwnedRoomId,
        /// Who is typing, or has stopped.
        user_id: OwnedUserId,
        /// Whether they are typing now.
        typing: bool,
        /// How long the notification is honored for, in milliseconds; meaningless when
        /// `typing` is false.
        timeout_ms: u64,
    },
    /// A receipt in `room_id` was written to the store with stamp `seq`. The receiver drops
    /// what it had cached for the room and reads the store again; `seq` raises its counter so
    /// that nothing it stamps afterwards is older than what it just learned of.
    Receipt {
        /// The room whose receipts changed.
        room_id: OwnedRoomId,
        /// The stamp the writer gave the receipt (`crate::stamp`).
        seq: u64,
    },
    /// `user_id`'s presence was written to the store with stamp `seq`. As for a receipt: the
    /// receiver forgets its cached record and rereads.
    Presence {
        /// Whose presence changed.
        user_id: OwnedUserId,
        /// The stamp the writer gave the record.
        seq: u64,
    },
}

impl EphemeralUpdate {
    /// The update's kind as a short label (`typing`, `receipt`, `presence`), for logs and
    /// counters.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Typing { .. } => "typing",
            Self::Receipt { .. } => "receipt",
            Self::Presence { .. } => "presence",
        }
    }
}

/// What a room's owner tells every other replica once it has fed one update: which room moved,
/// to where, the owner's own stream number for the update, and whose long-polls it woke.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomWake {
    /// The room that changed.
    pub room_id: OwnedRoomId,
    /// The room-local position of the update.
    pub room_pos: i64,
    /// `hs_room::protocol::RoomUpdate::global_seq` on the sender's registry stream; `0` for a
    /// re-read after falling behind, which carries no number.
    pub global_seq: u64,
    /// The users the owner's hub woke for this update: every active member of the room, plus
    /// whoever this update changed the membership of. Empty when the update fed nobody (the
    /// room is not the sender's, or feeding it failed) -- such a wake only advances the
    /// sender's consumed mark.
    pub users: Vec<OwnedUserId>,
}

/// One batch of wakes from one peer, coalesced per room. The sender's `consumed` mark is what a
/// receiver's [`crate::hub::SessionHub::settle_before_read`] waits on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeBatch {
    /// The sender, as `replica#generation`: a restarted replica numbers its stream from zero
    /// again, and must not inherit the mark its previous incarnation reached.
    pub from: String,
    /// The highest stream number the sender's hub had finished with when this batch was sent;
    /// every wake numbered up to it has been fed and is in this or an earlier batch.
    pub consumed: u64,
    /// The rooms that moved, one entry per room.
    pub wakes: Vec<RoomWake>,
    /// Typing, receipt and presence changes made on the sender since its last batch, in the
    /// order they were made. Absent from a batch sent by a replica from before this field
    /// existed, which is an empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ephemeral: Vec<EphemeralUpdate>,
}

impl WakeBatch {
    /// A batch from `from` with nothing in it yet.
    #[must_use]
    pub fn new(from: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            consumed: 0,
            wakes: Vec::new(),
            ephemeral: Vec::new(),
        }
    }

    /// Folds `update` in. A later typing update for the same room and user replaces the earlier
    /// one (only the latest state matters, and it carries the fresh timeout); a receipt hint for
    /// a room already hinted, or a presence hint for a user already hinted, keeps the higher
    /// stamp, since the receiver rereads the store either way.
    pub fn push_ephemeral(&mut self, update: EphemeralUpdate) {
        let same = self
            .ephemeral
            .iter_mut()
            .find(|existing| match (&**existing, &update) {
                (
                    EphemeralUpdate::Typing {
                        room_id: r1,
                        user_id: u1,
                        ..
                    },
                    EphemeralUpdate::Typing {
                        room_id: r2,
                        user_id: u2,
                        ..
                    },
                ) => r1 == r2 && u1 == u2,
                (
                    EphemeralUpdate::Receipt { room_id: r1, .. },
                    EphemeralUpdate::Receipt { room_id: r2, .. },
                ) => r1 == r2,
                (
                    EphemeralUpdate::Presence { user_id: u1, .. },
                    EphemeralUpdate::Presence { user_id: u2, .. },
                ) => u1 == u2,
                _ => false,
            });
        match same {
            Some(existing) => match (existing, update) {
                (
                    EphemeralUpdate::Receipt { seq, .. },
                    EphemeralUpdate::Receipt { seq: new, .. },
                )
                | (
                    EphemeralUpdate::Presence { seq, .. },
                    EphemeralUpdate::Presence { seq: new, .. },
                ) => *seq = (*seq).max(new),
                (existing, update) => *existing = update,
            },
            None => self.ephemeral.push(update),
        }
    }

    /// Folds `wake` in: the consumed mark advances to its number, and its users join the entry
    /// for the same room (at the newer position) or start one. A wake with no users only
    /// advances the mark.
    pub fn push(&mut self, wake: RoomWake) {
        self.consumed = self.consumed.max(wake.global_seq);
        if wake.users.is_empty() {
            return;
        }
        if let Some(existing) = self.wakes.iter_mut().find(|w| w.room_id == wake.room_id) {
            existing.room_pos = existing.room_pos.max(wake.room_pos);
            existing.global_seq = existing.global_seq.max(wake.global_seq);
            for user in wake.users {
                if !existing.users.contains(&user) {
                    existing.users.push(user);
                }
            }
        } else {
            self.wakes.push(wake);
        }
    }

    /// Whether the batch carries nothing a peer needs: no wakes, no mark and no ephemeral
    /// update.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.wakes.is_empty() && self.consumed == 0 && self.ephemeral.is_empty()
    }
}

/// A peer's answer to "what have you published": its key and the number of the newest update on
/// its registry stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPosition {
    /// The peer, as `replica#generation` (the same key its [`WakeBatch::from`] carries).
    pub peer: String,
    /// `hs_room::registry::RoomRegistry::global_published_seq` on that peer.
    pub published: u64,
}

/// The cluster as `/sync` sees it. Implemented over the mesh by `hs-cli`; a single-node server
/// installs none, and the hub then behaves exactly as it did before this trait existed.
#[async_trait::async_trait]
pub trait SessionCluster: Send + Sync {
    /// Whether this replica owns `room_id`'s shard right now. Owned rooms are read through the
    /// registry and fed by this hub; every other room is read through the mirror and fed by
    /// its owner.
    fn owns_room(&self, room_id: &RoomId) -> bool;

    /// Queues `wake` for every other live replica and returns at once. Delivery is best effort
    /// and coalesced; the store, not the wake, is what a reader trusts.
    fn publish(&self, wake: RoomWake);

    /// Queues `update` for every other live replica and returns at once, in the same batches
    /// as [`SessionCluster::publish`]'s wakes. Best effort, like the wake: a receipt or
    /// presence hint that is lost leaves the peer serving its cache until the next one, and a
    /// typing update that is lost leaves the peer not knowing until the client sends the next
    /// (real clients repeat it every few seconds while the user types).
    fn publish_ephemeral(&self, update: EphemeralUpdate);

    /// Asks every other live replica what it has published, waiting at most `deadline` for the
    /// slowest. A peer that does not answer in time is left out: a `/sync` cannot wait on a
    /// number it does not have, and the bounded wait is the whole point.
    async fn peer_positions(&self, deadline: Duration) -> Vec<PeerPosition>;
}

struct MirrorEntry<B: KvBackend> {
    handle: RoomActorHandle<B>,
    /// The room-local head this snapshot was loaded at.
    head: i64,
    last_used: Instant,
}

/// Read-only snapshots of rooms this replica does not own, each reloaded from the store
/// whenever the store's durable timeline head is past the snapshot's. See the module docs.
///
/// A reload is a full [`RoomActor::load`]: correct, and O(room size) per new event in a room
/// this replica does not own but has sessions reading. An incremental catch-up is `hs-room`'s
/// to add (`docs/rfcs/0018-room-actor-catch-up.md`); until then this is the documented cost of
/// reading a room from a replica that is not its owner.
pub struct RoomMirror<B: KvBackend> {
    backend: B,
    tables: Tables<B>,
    identity: HomeserverIdentity,
    rooms: Mutex<HashMap<OwnedRoomId, MirrorEntry<B>>>,
}

impl<B: KvBackend + 'static> RoomMirror<B> {
    /// Opens a mirror over `backend`, the same store the owners write to.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] if `hs-room`'s keyspaces could not be opened.
    pub fn open(backend: B, identity: HomeserverIdentity) -> Result<Self, hs_kv::KvError> {
        let tables = Tables::open(&backend)?;
        Ok(Self {
            backend,
            tables,
            identity,
            rooms: Mutex::new(HashMap::new()),
        })
    }

    /// The snapshot of `room_id`, reloaded first if the store's head has moved past it.
    ///
    /// # Errors
    /// Returns [`RoomError::RoomNotFound`] if the room does not exist, or any error
    /// [`RoomActor::load`] can return.
    pub async fn get_or_load(&self, room_id: &RoomId) -> Result<RoomActorHandle<B>, RoomError> {
        let durable = self.durable_head(room_id).await?;
        let Some(durable) = durable else {
            return Err(RoomError::RoomNotFound(room_id.to_string()));
        };
        {
            let mut rooms = self.rooms.lock().await;
            if let Some(entry) = rooms.get_mut(room_id)
                && entry.head >= durable
            {
                entry.last_used = Instant::now();
                return Ok(entry.handle.clone());
            }
        }

        let backend = self.backend.clone();
        let tables = self.tables.clone();
        let identity = self.identity.clone();
        let owned = room_id.to_owned();
        let loaded =
            tokio::task::spawn_blocking(move || RoomActor::load(backend, tables, identity, &owned))
                .await
                .map_err(|e| RoomError::Internal(format!("room mirror load task failed: {e}")))??;
        let Some(actor) = loaded else {
            return Err(RoomError::RoomNotFound(room_id.to_string()));
        };
        let head = actor.head_update().map_or(0, |u| u.room_pos);
        let handle = RoomActorHandle::new(actor);
        tracing::debug!(%room_id, head, "room mirror loaded a room this replica does not own");

        let mut rooms = self.rooms.lock().await;
        match rooms.entry(room_id.to_owned()) {
            std::collections::hash_map::Entry::Occupied(mut slot) if slot.get().head >= head => {
                // Somebody reloaded a snapshot at least this fresh while this one was loading;
                // theirs stays, this one is dropped unused.
                slot.get_mut().last_used = Instant::now();
                Ok(slot.get().handle.clone())
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                slot.insert(MirrorEntry {
                    handle: handle.clone(),
                    head,
                    last_used: Instant::now(),
                });
                Ok(handle)
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(MirrorEntry {
                    handle: handle.clone(),
                    head,
                    last_used: Instant::now(),
                });
                Ok(handle)
            }
        }
    }

    /// The store's durable timeline head for `room_id`: the newest `room_timeline` key under
    /// the room, `0` for a room with no timeline yet, `None` for a room that does not exist.
    /// Two point reads (the `room_sn` interning lookup, then a reverse range of one), which
    /// PostgreSQL answers as `ORDER BY ... DESC LIMIT 1`. The same read `hs_room::actor::room_heads`
    /// makes for every room; here for one.
    async fn durable_head(&self, room_id: &RoomId) -> Result<Option<i64>, RoomError> {
        let backend = self.backend.clone();
        let tables = self.tables.clone();
        let owned = room_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let snapshot = backend.snapshot();
            let Some(room_sn) = tables.room_sn.lookup(&snapshot, owned.as_bytes())? else {
                return Ok(None);
            };
            let mut newest = TypedKeyspace::<B::Keyspace, TimelineKey>::prefix(&(room_sn,));
            newest.reverse = true;
            newest.limit = Some(1);
            match tables.timeline.range(&snapshot, newest).next() {
                Some(entry) => {
                    let ((_, room_pos), _) = entry?;
                    Ok(Some(room_pos.max(0)))
                }
                None => Ok(Some(0)),
            }
        })
        .await
        .map_err(|e| RoomError::Internal(format!("room mirror head task failed: {e}")))?
    }

    /// Drops every snapshot idle longer than `max_idle`; returns how many. Called by the sweeper
    /// [`crate::hub::SessionHub::install_cluster`] spawns, and directly by tests.
    pub async fn evict_idle(&self, max_idle: Duration) -> usize {
        let mut rooms = self.rooms.lock().await;
        let before = rooms.len();
        let now = Instant::now();
        rooms.retain(|_, entry| now.duration_since(entry.last_used) < max_idle);
        before - rooms.len()
    }

    /// How many snapshots are resident right now. For tests and diagnostics.
    pub async fn resident_count(&self) -> usize {
        self.rooms.lock().await.len()
    }
}

/// The hub's link to the cluster: the trait object and the mirror, installed together.
pub(crate) struct ClusterLink<B: KvBackend> {
    pub(crate) cluster: Arc<dyn SessionCluster>,
    pub(crate) mirror: Arc<RoomMirror<B>>,
}

#[cfg(test)]
pub(crate) mod test_support {
    //! An in-process [`SessionCluster`]: ownership scripted by a predicate, wakes delivered
    //! straight to the peer hubs' `receive_wakes` (optionally after a delay, to make the race
    //! `settle_before_read` closes real), positions answered from the peer registries.

    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::hub::SessionHub;
    use crate::room_source::RoomSource;

    type OwnsFn = Box<dyn Fn(&RoomId) -> bool + Send + Sync>;

    /// A peer as this fake cluster reaches it: its hub (to deliver wakes to) and its registry
    /// (to answer positions from), under the key its batches carry.
    pub(crate) struct FakePeer<B: KvBackend, R: RoomSource<B>> {
        pub(crate) key: String,
        pub(crate) hub: Arc<SessionHub<B, R>>,
    }

    pub(crate) struct FakeCluster<B: KvBackend, R: RoomSource<B>> {
        me: String,
        owns: OwnsFn,
        peers: StdMutex<Vec<FakePeer<B, R>>>,
        /// Delay before a wake reaches a peer. Zero delivers on the next task switch.
        delivery_delay: Duration,
        /// Wakes are dropped on the floor: what a replica without this feature looks like.
        mute: std::sync::atomic::AtomicBool,
        sent: std::sync::atomic::AtomicUsize,
    }

    impl<B: KvBackend + 'static, R: RoomSource<B> + 'static> FakeCluster<B, R> {
        pub(crate) fn new(
            me: &str,
            owns: impl Fn(&RoomId) -> bool + Send + Sync + 'static,
            delivery_delay: Duration,
        ) -> Arc<Self> {
            Arc::new(Self {
                me: me.to_owned(),
                owns: Box::new(owns),
                peers: StdMutex::new(Vec::new()),
                delivery_delay,
                mute: std::sync::atomic::AtomicBool::new(false),
                sent: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        pub(crate) fn add_peer(&self, key: &str, hub: Arc<SessionHub<B, R>>) {
            self.peers.lock().unwrap().push(FakePeer {
                key: key.to_owned(),
                hub,
            });
        }

        pub(crate) fn mute(&self, mute: bool) {
            self.mute.store(mute, std::sync::atomic::Ordering::SeqCst);
        }

        pub(crate) fn sent(&self) -> usize {
            self.sent.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl<B: KvBackend + 'static, R: RoomSource<B> + 'static> SessionCluster for FakeCluster<B, R> {
        fn owns_room(&self, room_id: &RoomId) -> bool {
            (self.owns)(room_id)
        }

        fn publish(&self, wake: RoomWake) {
            let mut batch = WakeBatch::new(&self.me);
            batch.push(wake);
            self.deliver(batch);
        }

        fn publish_ephemeral(&self, update: EphemeralUpdate) {
            let mut batch = WakeBatch::new(&self.me);
            batch.push_ephemeral(update);
            self.deliver(batch);
        }

        async fn peer_positions(&self, _deadline: Duration) -> Vec<PeerPosition> {
            self.peers
                .lock()
                .unwrap()
                .iter()
                .map(|p| PeerPosition {
                    peer: p.key.clone(),
                    published: p.hub.rooms().global_published_seq(),
                })
                .collect()
        }
    }

    impl<B: KvBackend + 'static, R: RoomSource<B> + 'static> FakeCluster<B, R> {
        /// Sends `batch` to every peer hub, after the configured delay, unless muted.
        fn deliver(&self, batch: WakeBatch) {
            self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.mute.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let delay = self.delivery_delay;
            let hubs: Vec<Arc<SessionHub<B, R>>> = self
                .peers
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.hub.clone())
                .collect();
            for hub in hubs {
                let batch = batch.clone();
                tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    hub.receive_wakes(batch).await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{room_id, user_id};

    #[test]
    fn a_batch_coalesces_wakes_for_the_same_room_and_advances_its_mark() {
        let mut batch = WakeBatch::new("a#1");
        let room = room_id!("!r:test").to_owned();
        batch.push(RoomWake {
            room_id: room.clone(),
            room_pos: 5,
            global_seq: 10,
            users: vec![user_id!("@alice:test").to_owned()],
        });
        batch.push(RoomWake {
            room_id: room.clone(),
            room_pos: 6,
            global_seq: 11,
            users: vec![
                user_id!("@alice:test").to_owned(),
                user_id!("@bob:test").to_owned(),
            ],
        });
        // A wake that fed nobody still moves the mark, and adds no entry.
        batch.push(RoomWake {
            room_id: room_id!("!other:test").to_owned(),
            room_pos: 1,
            global_seq: 12,
            users: vec![],
        });
        assert_eq!(batch.consumed, 12);
        assert_eq!(batch.wakes.len(), 1);
        assert_eq!(batch.wakes[0].room_pos, 6);
        assert_eq!(batch.wakes[0].users.len(), 2);
        assert!(!batch.is_empty());
        assert!(WakeBatch::new("a#1").is_empty());
    }

    #[test]
    fn a_batch_round_trips_through_json() {
        let mut batch = WakeBatch::new("127.0.0.1:1#42");
        batch.push(RoomWake {
            room_id: room_id!("!r:test").to_owned(),
            room_pos: 3,
            global_seq: 7,
            users: vec![user_id!("@alice:test").to_owned()],
        });
        batch.push_ephemeral(EphemeralUpdate::Typing {
            room_id: room_id!("!r:test").to_owned(),
            user_id: user_id!("@alice:test").to_owned(),
            typing: true,
            timeout_ms: 30_000,
        });
        batch.push_ephemeral(EphemeralUpdate::Presence {
            user_id: user_id!("@alice:test").to_owned(),
            seq: 99,
        });
        let json = serde_json::to_vec(&batch).unwrap();
        let back: WakeBatch = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, batch);

        // A batch from a replica that predates the field parses, with nothing ephemeral in it.
        let old: WakeBatch =
            serde_json::from_str(r#"{"from":"a#1","consumed":3,"wakes":[]}"#).unwrap();
        assert!(old.ephemeral.is_empty());
        assert!(!old.is_empty());
    }

    #[test]
    fn ephemeral_updates_coalesce_by_what_they_are_about_and_alone_make_a_batch_worth_sending() {
        let room = room_id!("!r:test").to_owned();
        let alice = user_id!("@alice:test").to_owned();
        let bob = user_id!("@bob:test").to_owned();
        let mut batch = WakeBatch::new("a#1");
        assert!(batch.is_empty());
        batch.push_ephemeral(EphemeralUpdate::Typing {
            room_id: room.clone(),
            user_id: alice.clone(),
            typing: true,
            timeout_ms: 30_000,
        });
        assert!(!batch.is_empty(), "an ephemeral update alone is a batch");
        // Alice stops: the stop replaces the start, in place.
        batch.push_ephemeral(EphemeralUpdate::Typing {
            room_id: room.clone(),
            user_id: alice.clone(),
            typing: false,
            timeout_ms: 0,
        });
        // Bob is a different entry.
        batch.push_ephemeral(EphemeralUpdate::Typing {
            room_id: room.clone(),
            user_id: bob.clone(),
            typing: true,
            timeout_ms: 5_000,
        });
        // Two receipts in one room: one hint, the higher stamp.
        batch.push_ephemeral(EphemeralUpdate::Receipt {
            room_id: room.clone(),
            seq: 10,
        });
        batch.push_ephemeral(EphemeralUpdate::Receipt {
            room_id: room.clone(),
            seq: 8,
        });
        // Two presence changes for one user, likewise.
        batch.push_ephemeral(EphemeralUpdate::Presence {
            user_id: alice.clone(),
            seq: 5,
        });
        batch.push_ephemeral(EphemeralUpdate::Presence {
            user_id: alice.clone(),
            seq: 6,
        });
        assert_eq!(
            batch.ephemeral,
            vec![
                EphemeralUpdate::Typing {
                    room_id: room.clone(),
                    user_id: alice.clone(),
                    typing: false,
                    timeout_ms: 0,
                },
                EphemeralUpdate::Typing {
                    room_id: room.clone(),
                    user_id: bob,
                    typing: true,
                    timeout_ms: 5_000,
                },
                EphemeralUpdate::Receipt {
                    room_id: room,
                    seq: 10
                },
                EphemeralUpdate::Presence {
                    user_id: alice,
                    seq: 6
                },
            ]
        );
        assert_eq!(batch.ephemeral[2].kind(), "receipt");
    }
}

/// Two hubs, two registries, one shared store: replica A owns every room, replica B none, and
/// the fake cluster carries A's wakes to B in-process. What the two-process experiment in
/// `docs/status/05-sync.md` does with the real binary, without PostgreSQL or the mesh.
#[cfg(test)]
mod two_replica_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use hs_e2e::store::E2eStore;
    use hs_e2e::store::tables::TablesE2eStore;
    use hs_kv::memory::MemoryBackend;
    use hs_room::actor::CreateRoomRequest;
    use hs_room::identity::HomeserverIdentity;
    use hs_room::membership::Action;
    use hs_room::registry::RoomRegistry;
    use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId, user_id};
    use serde_json::{Value, json};

    use super::test_support::FakeCluster;
    use super::*;
    use crate::filter::SyncFilter;
    use crate::hub::SessionHub;
    use crate::receipts::ReceiptKind;
    use crate::store::DynUserStore;
    use crate::store::tables::TablesUserStore;
    use crate::sync::{SyncParams, build};
    use crate::token::SyncToken;

    type Registry = Arc<RoomRegistry<MemoryBackend>>;
    type Hub = Arc<SessionHub<MemoryBackend, Registry>>;

    struct Replica {
        hub: Hub,
        registry: Registry,
        cluster: Arc<FakeCluster<MemoryBackend, Registry>>,
        mirror: Arc<RoomMirror<MemoryBackend>>,
        e2e: Arc<dyn E2eStore>,
    }

    fn replica(
        backend: &MemoryBackend,
        key: &str,
        owns: bool,
        delivery_delay: Duration,
    ) -> Replica {
        let identity = HomeserverIdentity::for_tests("cluster.test");
        let registry: Registry =
            Arc::new(RoomRegistry::open(backend.clone(), identity.clone()).unwrap());
        let store: DynUserStore = Arc::new(TablesUserStore::open(backend.clone()).unwrap());
        let hub: Hub = Arc::new(SessionHub::new(store, registry.clone(), 500));
        let cluster = FakeCluster::new(key, move |_: &RoomId| owns, delivery_delay);
        let mirror = Arc::new(RoomMirror::open(backend.clone(), identity).unwrap());
        hub.install_cluster(cluster.clone(), mirror.clone());
        // The drain task is deliberately leaked, as `crate::sync`'s own tests do: it ends with
        // the registry, and the registry ends with the test.
        std::mem::forget(hub.watch_all(registry.subscribe_global()));
        Replica {
            hub,
            registry,
            cluster,
            mirror,
            e2e: Arc::new(TablesE2eStore::open(backend.clone()).unwrap()),
        }
    }

    /// Replica A owns everything and B nothing; each knows the other as its one peer.
    fn two_replicas(delivery_delay: Duration) -> (Replica, Replica) {
        let backend = MemoryBackend::new();
        let a = replica(&backend, "a#1", true, delivery_delay);
        let b = replica(&backend, "b#1", false, delivery_delay);
        a.cluster.add_peer("b#1", b.hub.clone());
        b.cluster.add_peer("a#1", a.hub.clone());
        (a, b)
    }

    /// With a device, as every real session has: `build` then records the device cursor, and
    /// without one a later feed entry for the same room coalesces into the entry this token
    /// already covers (`crate::store`'s coalescing; `docs/status/05-sync.md`, "A test-harness
    /// gotcha").
    fn params(since: Option<SyncToken>, timeout: Duration) -> SyncParams {
        SyncParams {
            since,
            full_state: false,
            timeout,
            filter: SyncFilter::none(),
            device_id: Some(ruma::device_id!("ELEMENT").to_owned()),
        }
    }

    fn timeline_event_ids(response: &Value, room_id: &RoomId) -> Vec<String> {
        response["rooms"]["join"][room_id.as_str()]["timeline"]["events"]
            .as_array()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|e| e["event_id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Alice makes a room on A and Bob joins it there; A's hub has fed both before this returns.
    async fn room_with_alice_and_bob(a: &Replica) -> (OwnedRoomId, OwnedUserId, OwnedUserId) {
        let alice = user_id!("@alice:cluster.test").to_owned();
        let bob = user_id!("@bob:cluster.test").to_owned();
        let handle = a
            .registry
            .create_room(
                alice.clone(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        handle
            .membership(bob.clone(), Action::Join, bob.clone(), json!({}), 2)
            .await
            .unwrap();
        let room_id = handle.query(|actor| actor.room_id().to_owned()).await;
        a.hub
            .wait_for_consumed(a.registry.global_published_seq(), Duration::from_secs(5))
            .await;
        (room_id, alice, bob)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_long_poll_on_the_other_replica_is_woken_by_the_owners_write() {
        let (a, b) = two_replicas(Duration::ZERO);
        let (room_id, alice, bob) = room_with_alice_and_bob(&a).await;

        // Alice's session is on B, which owns nothing.
        let (initial, token) = build(&b.hub, &b.e2e, &alice, params(None, Duration::ZERO))
            .await
            .unwrap();
        assert!(
            initial["rooms"]["join"][room_id.as_str()].is_object(),
            "B reads the room through its mirror: {initial}"
        );
        assert_eq!(b.mirror.resident_count().await, 1);

        let poll = {
            let hub = b.hub.clone();
            let e2e = b.e2e.clone();
            let alice = alice.clone();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let (response, _) = build(
                    &hub,
                    &e2e,
                    &alice,
                    params(Some(token), Duration::from_secs(10)),
                )
                .await
                .unwrap();
                (response, started.elapsed())
            })
        };
        // Long enough for the poll to be waiting; if it is not yet, the event is simply already
        // there when it starts, which can only make the assertion below easier, not wrong.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let handle = a.registry.get_or_load(&room_id).await.unwrap();
        let sent = handle
            .send_event(
                bob.clone(),
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": "hello from A"}),
                None,
                3,
            )
            .await
            .unwrap();

        let (response, elapsed) = poll.await.unwrap();
        // The bar is the long-poll's own 500 ms re-check (`crate::sync`'s `E2E_POLL_INTERVAL`),
        // which would notice the durable feed entry on its own: a wake that does not beat it
        // is no wake. In-process the wake lands in single-digit milliseconds; the margin is
        // for a loaded machine, not for the poll.
        assert!(
            elapsed < Duration::from_millis(400),
            "the long-poll on B was not woken by A's write; it took {elapsed:?}"
        );
        assert!(
            timeline_event_ids(&response, &room_id).contains(&sent.event_id().to_string()),
            "the woken poll must carry the event: {response}"
        );
        assert!(a.cluster.sent() >= 1, "A published no wake at all");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_through_the_owner_is_in_the_very_next_sync_on_the_other_replica() {
        // Wakes take 150 ms to cross: without waiting on A's position, B would read its feeds
        // in the window before A's hub has written them.
        let (a, b) = two_replicas(Duration::from_millis(150));
        let (room_id, alice, _bob) = room_with_alice_and_bob(&a).await;
        let handle = a.registry.get_or_load(&room_id).await.unwrap();

        let (_, mut token) = build(&b.hub, &b.e2e, &alice, params(None, Duration::ZERO))
            .await
            .unwrap();
        for i in 0..10 {
            let sent = handle
                .send_event(
                    alice.clone(),
                    "m.room.message".to_owned(),
                    None,
                    json!({"msgtype": "m.text", "body": format!("mine #{i}")}),
                    None,
                    10 + i,
                )
                .await
                .unwrap();
            let (response, next) =
                build(&b.hub, &b.e2e, &alice, params(Some(token), Duration::ZERO))
                    .await
                    .unwrap();
            assert!(
                timeline_event_ids(&response, &room_id).contains(&sent.event_id().to_string()),
                "send #{i} through A was not in the very next sync on B: {response}"
            );
            token = next;
        }
        assert!(
            b.hub.peer_consumed("a#1") >= a.registry.global_published_seq(),
            "B must have waited for A's mark"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peer_whose_wakes_never_arrive_delays_a_sync_only_by_the_bounded_wait() {
        let (a, b) = two_replicas(Duration::ZERO);
        let (room_id, alice, _bob) = room_with_alice_and_bob(&a).await;
        let (_, token) = build(&b.hub, &b.e2e, &alice, params(None, Duration::ZERO))
            .await
            .unwrap();

        a.cluster.mute(true);
        let handle = a.registry.get_or_load(&room_id).await.unwrap();
        let sent = handle
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": "unannounced"}),
                None,
                10,
            )
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let (response, _) = build(&b.hub, &b.e2e, &alice, params(Some(token), Duration::ZERO))
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "the wait for a silent peer must be bounded, took {elapsed:?}"
        );
        // A's hub fed the store long before the bounded wait ran out, so the store still has
        // the event: the wake is the doorbell, not the truth.
        assert!(
            timeline_event_ids(&response, &room_id).contains(&sent.event_id().to_string()),
            "{response}"
        );
    }

    /// The `m.typing` event for `room_id` in a response, if there is one: its `user_ids`.
    fn typing_user_ids(response: &Value, room_id: &RoomId) -> Option<Vec<String>> {
        response["rooms"]["join"][room_id.as_str()]["ephemeral"]["events"]
            .as_array()?
            .iter()
            .find(|e| e["type"] == "m.typing")
            .map(|e| {
                e["content"]["user_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|u| u.as_str().map(str::to_owned))
                    .collect()
            })
    }

    /// The `m.receipt` content for `room_id` in a response, if there is one.
    fn receipt_content(response: &Value, room_id: &RoomId) -> Option<Value> {
        response["rooms"]["join"][room_id.as_str()]["ephemeral"]["events"]
            .as_array()?
            .iter()
            .find(|e| e["type"] == "m.receipt")
            .map(|e| e["content"].clone())
    }

    /// The presence event from `sender` in a response, if there is one: its content.
    fn presence_from(response: &Value, sender: &UserId) -> Option<Value> {
        response["presence"]["events"]
            .as_array()?
            .iter()
            .find(|e| e["sender"] == sender.as_str())
            .map(|e| e["content"].clone())
    }

    /// Bob's session is on B. Alice types on A (the owner, where every `PUT .../typing` for the
    /// room lands): bob's long-poll on B is woken with her in `m.typing`, and again with her
    /// gone when she stops. A short typing timeout lapses on B by itself, on B's clock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_on_one_replica_is_in_a_long_poll_on_the_other_and_goes_away_when_it_stops() {
        let (a, b) = two_replicas(Duration::ZERO);
        let (room_id, alice, bob) = room_with_alice_and_bob(&a).await;
        let (_, token) = build(&b.hub, &b.e2e, &bob, params(None, Duration::ZERO))
            .await
            .unwrap();

        let poll = |token: SyncToken| {
            let (hub, e2e, bob) = (b.hub.clone(), b.e2e.clone(), bob.clone());
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let out = build(
                    &hub,
                    &e2e,
                    &bob,
                    params(Some(token), Duration::from_secs(10)),
                )
                .await
                .unwrap();
                (out, started.elapsed())
            })
        };

        let waiting = poll(token);
        tokio::time::sleep(Duration::from_millis(100)).await;
        a.hub
            .set_typing(&room_id, &alice, true, Duration::from_secs(30))
            .await
            .unwrap();
        let ((response, token), elapsed) = waiting.await.unwrap();
        assert!(
            elapsed < Duration::from_millis(400),
            "bob's long-poll on B was not woken by alice typing on A; it took {elapsed:?}"
        );
        assert_eq!(
            typing_user_ids(&response, &room_id),
            Some(vec![alice.to_string()]),
            "{response}"
        );

        let waiting = poll(token);
        tokio::time::sleep(Duration::from_millis(100)).await;
        a.hub
            .set_typing(&room_id, &alice, false, Duration::from_secs(30))
            .await
            .unwrap();
        let ((response, token), elapsed) = waiting.await.unwrap();
        assert!(elapsed < Duration::from_millis(400), "{elapsed:?}");
        assert_eq!(
            typing_user_ids(&response, &room_id),
            Some(vec![]),
            "the stop must reach B too: {response}"
        );

        // A timeout lapses on B without anybody saying so: the lazy prune on B's next re-check.
        a.hub
            .set_typing(&room_id, &alice, true, Duration::from_millis(200))
            .await
            .unwrap();
        let ((response, token), _) = poll(token).await.unwrap();
        assert_eq!(
            typing_user_ids(&response, &room_id),
            Some(vec![alice.to_string()])
        );
        let ((response, _), elapsed) = poll(token).await.unwrap();
        assert_eq!(
            typing_user_ids(&response, &room_id),
            Some(vec![]),
            "the lapse must show on B: {response}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "a lapse is noticed within the re-check interval, not the poll timeout: {elapsed:?}"
        );
        assert_eq!(b.cluster.sent(), 0, "B, told, tells nobody");
    }

    /// The receipt is durable, so B could read it from the store -- but B loaded the room's
    /// receipts once (bob's first sync) and served that copy: with nothing telling it, alice's
    /// later receipt never shows. With the hint, it does, and so does the one after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_receipt_on_one_replica_is_in_the_next_sync_on_the_other_and_not_served_stale() {
        let (a, b) = two_replicas(Duration::ZERO);
        let (room_id, alice, bob) = room_with_alice_and_bob(&a).await;
        let handle = a.registry.get_or_load(&room_id).await.unwrap();
        let mut sent = Vec::new();
        for i in 0..3 {
            sent.push(
                handle
                    .send_event(
                        alice.clone(),
                        "m.room.message".to_owned(),
                        None,
                        json!({"msgtype": "m.text", "body": format!("#{i}")}),
                        None,
                        10 + i,
                    )
                    .await
                    .unwrap()
                    .event_id()
                    .to_owned(),
            );
        }
        // Bob's first sync on B loads the room's receipts into B's cache (none yet).
        let (_, mut token) = build(&b.hub, &b.e2e, &bob, params(None, Duration::ZERO))
            .await
            .unwrap();

        // Without the hint: what the gap looked like. B keeps serving its copy.
        a.cluster.mute(true);
        a.hub
            .set_receipt(&room_id, &alice, ReceiptKind::Read, sent[0].clone(), 1)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (stale, next) = build(&b.hub, &b.e2e, &bob, params(Some(token), Duration::ZERO))
            .await
            .unwrap();
        assert_eq!(
            receipt_content(&stale, &room_id),
            None,
            "muted, B serves its cache; this is the gap the hint closes: {stale}"
        );
        token = next;
        a.cluster.mute(false);

        // With it: the receipt, then a later one for the same room.
        for (i, ts) in [(1, 2), (2, 3)] {
            let waiting = {
                let (hub, e2e, bob) = (b.hub.clone(), b.e2e.clone(), bob.clone());
                let token = token;
                tokio::spawn(async move {
                    build(
                        &hub,
                        &e2e,
                        &bob,
                        params(Some(token), Duration::from_secs(10)),
                    )
                    .await
                    .unwrap()
                })
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            a.hub
                .set_receipt(&room_id, &alice, ReceiptKind::Read, sent[i].clone(), ts)
                .await
                .unwrap();
            let (response, next) = tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .expect("bob's long-poll on B is woken by the receipt on A")
                .unwrap();
            let content = receipt_content(&response, &room_id).unwrap_or_else(|| {
                panic!("receipt #{i} on A is not in bob's sync on B: {response}")
            });
            assert_eq!(
                content[sent[i].as_str()]["m.read"][alice.as_str()]["ts"],
                json!(ts),
                "{content}"
            );
            assert!(
                content.get(sent[i - 1].as_str()).is_none(),
                "a later receipt replaces the earlier one: {content}"
            );
            // Nothing new: not sent again.
            let (again, next) = build(&b.hub, &b.e2e, &bob, params(Some(next), Duration::ZERO))
                .await
                .unwrap();
            assert_eq!(receipt_content(&again, &room_id), None, "{again}");
            token = next;
        }
    }

    /// Alice sets her presence on A (a `PUT /presence` lands wherever it arrives; nothing
    /// forwards it): bob on B sees it, and the change after it, though B had her record cached.
    /// Bob's own presence, set on B, reaches alice on A the same way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_presence_change_on_one_replica_reaches_a_room_mate_on_the_other() {
        let (a, b) = two_replicas(Duration::ZERO);
        let (_room_id, alice, bob) = room_with_alice_and_bob(&a).await;
        // Both replicas cache alice's presence (bob's sync on B reads it; alice's on A too).
        let (_, mut bob_token) = build(&b.hub, &b.e2e, &bob, params(None, Duration::ZERO))
            .await
            .unwrap();
        let (_, alice_token) = build(&a.hub, &a.e2e, &alice, params(None, Duration::ZERO))
            .await
            .unwrap();

        for (presence, msg) in [("unavailable", "lunch"), ("online", "back")] {
            let waiting = {
                let (hub, e2e, bob) = (b.hub.clone(), b.e2e.clone(), bob.clone());
                let token = bob_token;
                tokio::spawn(async move {
                    build(
                        &hub,
                        &e2e,
                        &bob,
                        params(Some(token), Duration::from_secs(10)),
                    )
                    .await
                    .unwrap()
                })
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            a.hub
                .set_presence(&alice, presence.to_owned(), Some(msg.to_owned()))
                .await
                .unwrap();
            let (response, next) = tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .expect("bob's long-poll on B is woken by alice's presence change on A")
                .unwrap();
            let content = presence_from(&response, &alice).unwrap_or_else(|| {
                panic!("alice's {presence} is not in bob's sync on B: {response}")
            });
            assert_eq!(content["presence"], presence, "{content}");
            assert_eq!(content["status_msg"], msg, "{content}");
            bob_token = next;
        }

        // And the other way round. Alice's own changes are hers to see too, so her token is
        // moved past them first.
        let (_, alice_token) = build(
            &a.hub,
            &a.e2e,
            &alice,
            params(Some(alice_token), Duration::ZERO),
        )
        .await
        .unwrap();
        let waiting = {
            let (hub, e2e, alice) = (a.hub.clone(), a.e2e.clone(), alice.clone());
            let token = alice_token;
            tokio::spawn(async move {
                build(
                    &hub,
                    &e2e,
                    &alice,
                    params(Some(token), Duration::from_secs(10)),
                )
                .await
                .unwrap()
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        b.hub
            .set_presence(&bob, "unavailable".to_owned(), None)
            .await
            .unwrap();
        let (response, next) = tokio::time::timeout(Duration::from_secs(3), waiting)
            .await
            .expect("alice's long-poll on A is woken by bob's presence change on B")
            .unwrap();
        assert_eq!(
            presence_from(&response, &bob).map(|c| c["presence"].clone()),
            Some(json!("unavailable")),
            "{response}"
        );
        let (again, _) = build(&a.hub, &a.e2e, &alice, params(Some(next), Duration::ZERO))
            .await
            .unwrap();
        assert_eq!(presence_from(&again, &bob), None, "not sent twice: {again}");
    }

    #[tokio::test]
    async fn a_hub_that_does_not_own_a_room_feeds_nobody_for_it() {
        let (a, b) = two_replicas(Duration::ZERO);
        let (room_id, alice, _bob) = room_with_alice_and_bob(&a).await;
        let before = b.hub.store().latest_feed_seq(&alice).await.unwrap();
        // B's own registry loads the room (as something on B other than `/sync` might) and
        // announces its head on B's stream; B's hub must not feed it.
        let handle = b.registry.get_or_load(&room_id).await.unwrap();
        let head = handle.query(|actor| actor.head_update()).await.unwrap();
        b.hub.process_room_update(head).await.unwrap();
        assert_eq!(
            b.hub.store().latest_feed_seq(&alice).await.unwrap(),
            before,
            "only the owner writes feeds"
        );
        assert!(!b.hub.owns_room(&room_id));
        assert!(a.hub.owns_room(&room_id));
    }

    #[tokio::test]
    async fn the_mirror_reloads_a_room_only_when_the_store_is_ahead_of_its_snapshot() {
        let (a, b) = two_replicas(Duration::ZERO);
        let (room_id, alice, _bob) = room_with_alice_and_bob(&a).await;

        let first = b.mirror.get_or_load(&room_id).await.unwrap();
        let head_before = first
            .query(|actor| actor.head_update())
            .await
            .unwrap()
            .room_pos;
        let again = b.mirror.get_or_load(&room_id).await.unwrap();
        assert_eq!(
            again
                .query(|actor| actor.head_update())
                .await
                .unwrap()
                .room_pos,
            head_before,
            "nothing changed, so the same snapshot serves"
        );

        let handle = a.registry.get_or_load(&room_id).await.unwrap();
        handle
            .send_event(
                alice,
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": "moved"}),
                None,
                10,
            )
            .await
            .unwrap();
        let fresh = b.mirror.get_or_load(&room_id).await.unwrap();
        let head_after = fresh
            .query(|actor| actor.head_update())
            .await
            .unwrap()
            .room_pos;
        assert!(head_after > head_before, "{head_after} <= {head_before}");
        // The old handle is the old snapshot: a reader holding it sees the room as it was.
        assert_eq!(
            first
                .query(|actor| actor.head_update())
                .await
                .unwrap()
                .room_pos,
            head_before
        );

        assert_eq!(b.mirror.resident_count().await, 1);
        assert_eq!(b.mirror.evict_idle(Duration::ZERO).await, 1);
        assert_eq!(b.mirror.resident_count().await, 0);
        assert!(matches!(
            b.mirror
                .get_or_load(ruma::room_id!("!nope:cluster.test"))
                .await,
            Err(RoomError::RoomNotFound(_))
        ));
    }
}
