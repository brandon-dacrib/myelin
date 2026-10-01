//! [`TablesUserStore`]: an `hs-kv`/`hs-tables`-backed [`super::UserStore`], generic over
//! `B: hs_kv::KvBackend` -- `hs_kv::memory::MemoryBackend` for tests, `hs_kv::fjall_backend::FjallBackend`
//! for a real `hs serve` process. Follows `hs_auth::store::tables::TablesAuthStore`'s shape (read
//! that module first): every method is synchronous internally (`hs-kv` transactions may not
//! `.await`), wrapped in `#[async_trait::async_trait]` at the edge.
//!
//! # Keyspaces
//!
//! | keyspace | primary key | used by |
//! |---|---|---|
//! | `hs_user.feed` | `(user_id, feed_seq)` | the durable, coalesced feed -- `crate::token`'s `feed_seq` indexes here |
//! | `hs_user.feed_by_room` | `(user_id, room_id)` | pointer to the current pending feed entry for that room, if any (coalescing target) |
//! | `hs_user.device_cursors` | `(user_id, device_id)` | the `feed_seq` most recently handed to each device as a `next_batch` -- the coalescing safety bound |
//! | `hs_user.memberships` | `(user_id, room_id)` | current (not historical) membership snapshot -- the room set for an initial sync |
//! | `hs_user.account_data_global` | `(user_id, event_type)` | global account data |
//! | `hs_user.account_data_room` | `(user_id, room_id, event_type)` | room-scoped account data (`m.tag` and friends) |
//! | `hs_user.account_data_counter` | `user_id` (raw `atomic_add` key, not a [`hs_tables::keyspace::TypedKeyspace`]) | the shared global/room account-data change counter |
//! | `hs_user.filters` | `(user_id, filter_id)` | uploaded named filters (`POST /user/{userId}/filter`) |
//! | `hs_user.receipts` | `(room_id, user_id, kind)` | the latest read receipt of each kind per user per room |
//! | `hs_user.presence` | `user_id` | each user's latest presence |
//! | `hs_user.receipt_stream` | `(pos: u64,)` | the server-wide receipt stream: one entry per receipt written, for appservice delivery |
//! | `hs_user.presence_stream` | `(pos: u64,)` | the server-wide presence stream: one entry per presence change (a new stamp), for appservice delivery |
//! | `hs_user.hot_positions` | `(room_id, hot_seq)` | the server-wide hot-room stream: one entry per update to a room above the fan-out threshold, its `room_pos` -- `crate::token`'s `hot_seq` indexes here |
//! | `hs_user.room_members` | `(room_id, user_id)` | each indexed room's joined members, for the user directory; the row with an empty `user_id` is the marker that says the room is indexed (`UserStore::index_room_members_if_absent`) |
//! | `hs_user.ephemeral_counters` | `receipt_stream` / `presence_stream` / `hot_positions` (raw `atomic_add` keys) | the three streams' position counters |
//!
//! # The coalescing invariant, precisely
//!
//! [`TablesUserStore::append_feed_entry`] is the one place `crate::store`'s "coalesced so an
//! unsynced room appears once" claim (`PLAN.md` section 6.6) is actually enforced, and it is
//! worth stating the invariant it maintains explicitly, because getting this wrong either loses
//! events (unacceptable) or lets the feed grow without bound (defeats the point):
//!
//! **An existing feed entry for `(user, room)` at `feed_seq = S` is merged in place (its stored
//! `room_pos` overwritten, no new row created) if and only if `S` is strictly greater than
//! [`super::UserStore::max_device_cursor`] for that user at the moment of the merge.**
//!
//! Why the *maximum* device cursor, not the minimum (the more obvious reading of "the oldest live
//! device's token"): a device's cursor is set ([`super::UserStore::record_device_cursor`]) to the
//! `feed_seq` of the `next_batch` token *just issued* to it -- i.e. "this device has now been told
//! the feed reaches at least this far". The instant any device has been issued a token with
//! `feed_seq >= S`, that specific entry `S` becomes load-bearing forever: some client, now or
//! later, may present exactly that token (or an even older one) and ask this store to reconstruct
//! "what was room `X`'s position as of `feed_seq <= S`"
//! ([`super::UserStore::room_pos_as_of`]), and if `S`'s row has been overwritten by a later
//! update, that reconstruction would silently resolve to the wrong (too new) baseline and skip
//! real events. Using the *maximum* over all devices, rather than the minimum, is what protects
//! against this for every device, not just the slowest one -- a fast device's freshly issued
//! token pins the entry just as much as a slow device's stale one does. The trade a *minimum*
//! bound would buy (deleting entries no device *needs* even if some device has already moved
//! past them) is retention/pruning, not correctness, and is out of scope for this pass -- see
//! `docs/status/05-sync.md`'s "Next".
//!
//! A room with no device having synced yet (`max_device_cursor == 0`) coalesces aggressively,
//! which is correct: nobody has been told anything about the feed yet, so nothing needs
//! preserving.

use bytes::Bytes;
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use rand::Rng;
use rand::distr::Alphanumeric;
use ruma::{DeviceId, RoomId, UserId};
use serde::{Deserialize, Serialize};

use super::{
    AccountDataRecord, FeedEntry, MembershipRecord, PresenceStreamEntry, PublicRoomEntry,
    ReceiptStreamEntry, StoreError, StoredPresence, StoredReceipt, UserStore,
};

fn to_kv<E: std::error::Error + Send + Sync + 'static>(e: E) -> hs_kv::KvError {
    hs_kv::KvError::backend(e)
}

fn decode_u64(bytes: &[u8]) -> Result<u64, StoreError> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| StoreError::Codec("expected an 8-byte counter".to_owned()))?;
    Ok(u64::from_be_bytes(arr))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeedValue {
    room_id: String,
    room_pos: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountDataValue {
    content: serde_json::Value,
    changed_seq: u64,
}

fn decode_i64(bytes: &[u8]) -> Result<i64, StoreError> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| StoreError::Codec("expected an 8-byte room position".to_owned()))?;
    Ok(i64::from_be_bytes(arr))
}

fn json_decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::Codec(e.to_string()))
}

fn json_encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(value).map_err(|e| StoreError::Codec(e.to_string()))
}

/// The `hs-kv`/`hs-tables`-backed [`UserStore`]. See the module docs for its keyspace layout and
/// the coalescing invariant.
pub struct TablesUserStore<B: KvBackend> {
    backend: B,
    feed: TypedKeyspace<B::Keyspace, (String, u64)>,
    feed_by_room: TypedKeyspace<B::Keyspace, (String, String)>,
    device_cursors: TypedKeyspace<B::Keyspace, (String, String)>,
    memberships: TypedKeyspace<B::Keyspace, (String, String)>,
    account_data_global: TypedKeyspace<B::Keyspace, (String, String)>,
    account_data_room: TypedKeyspace<B::Keyspace, (String, String, String)>,
    account_data_counter: B::Keyspace,
    filters: TypedKeyspace<B::Keyspace, (String, String)>,
    public_rooms: TypedKeyspace<B::Keyspace, (String,)>,
    receipts: TypedKeyspace<B::Keyspace, (String, String, String)>,
    presence: TypedKeyspace<B::Keyspace, (String,)>,
    receipt_stream: TypedKeyspace<B::Keyspace, (u64,)>,
    presence_stream: TypedKeyspace<B::Keyspace, (u64,)>,
    hot_positions: TypedKeyspace<B::Keyspace, (String, u64)>,
    room_members: TypedKeyspace<B::Keyspace, (String, String)>,
    ephemeral_counters: B::Keyspace,
}

/// The `user_id` of a room's marker row in `hs_user.room_members`: no user id is empty, and it
/// sorts before every real one.
const INDEXED_MARKER: &str = "";

/// The `hs_user.ephemeral_counters` key of the hot-room stream's position counter.
const HOT_POSITIONS_COUNTER: &[u8] = b"hot_positions";

/// A `hs_user.receipt_stream` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiptStreamValue {
    room_id: String,
    receipt: StoredReceipt,
}

/// Reads a raw `atomic_add` counter, `0` when it has never been bumped.
fn read_counter<R: KvRead>(
    snap: &R,
    keyspace: &R::Keyspace,
    key: &[u8],
) -> Result<u64, StoreError> {
    match snap.get(keyspace, key).map_err(StoreError::Kv)? {
        None => Ok(0),
        Some(bytes) => {
            let arr: [u8; 8] = bytes
                .as_ref()
                .try_into()
                .map_err(|_| StoreError::Codec("expected an 8-byte counter".to_owned()))?;
            Ok(u64::try_from(i64::from_be_bytes(arr)).unwrap_or(0))
        }
    }
}

impl<B: KvBackend> TablesUserStore<B> {
    /// Opens (creating if necessary) every keyspace this store needs.
    ///
    /// # Errors
    /// Returns [`StoreError::Kv`] if a keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let open = |name: &str| -> Result<B::Keyspace, StoreError> {
            backend.keyspace(name).map_err(StoreError::Kv)
        };
        Ok(Self {
            feed: TypedKeyspace::new(open("hs_user.feed")?),
            feed_by_room: TypedKeyspace::new(open("hs_user.feed_by_room")?),
            device_cursors: TypedKeyspace::new(open("hs_user.device_cursors")?),
            memberships: TypedKeyspace::new(open("hs_user.memberships")?),
            account_data_global: TypedKeyspace::new(open("hs_user.account_data_global")?),
            account_data_room: TypedKeyspace::new(open("hs_user.account_data_room")?),
            account_data_counter: open("hs_user.account_data_counter")?,
            filters: TypedKeyspace::new(open("hs_user.filters")?),
            public_rooms: TypedKeyspace::new(open("hs_user.public_rooms")?),
            receipts: TypedKeyspace::new(open("hs_user.receipts")?),
            presence: TypedKeyspace::new(open("hs_user.presence")?),
            receipt_stream: TypedKeyspace::new(open("hs_user.receipt_stream")?),
            presence_stream: TypedKeyspace::new(open("hs_user.presence_stream")?),
            hot_positions: TypedKeyspace::new(open("hs_user.hot_positions")?),
            room_members: TypedKeyspace::new(open("hs_user.room_members")?),
            ephemeral_counters: open("hs_user.ephemeral_counters")?,
            backend,
        })
    }

    /// The underlying backend, for a caller that needs its own transaction spanning this store
    /// and another table.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    fn latest_feed_seq_txn<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &R,
        uid: &str,
    ) -> Result<u64, StoreError> {
        let mut spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(uid.to_owned(),));
        spec.reverse = true;
        spec.limit = Some(1);
        // The spec above is reversed and limited to one row, so this is "the highest sequence
        // this user's feed has", or 0 for a user with no feed rows at all.
        match self.feed.range(txn, spec).next() {
            Some(item) => {
                let ((_, seq), _) = item.map_err(StoreError::Table)?;
                Ok(seq)
            }
            None => Ok(0),
        }
    }

    fn max_device_cursor_txn<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &R,
        uid: &str,
    ) -> Result<u64, StoreError> {
        let spec = TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(uid.to_owned(),));
        let mut max = 0u64;
        for item in self.device_cursors.range(txn, spec) {
            let (_, value) = item.map_err(StoreError::Table)?;
            max = max.max(decode_u64(&value)?);
        }
        Ok(max)
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> UserStore for TablesUserStore<B> {
    async fn append_feed_entry(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        room_pos: i64,
    ) -> Result<u64, StoreError> {
        let uid = user_id.to_string();
        let rid = room_id.to_string();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let max_cursor = self.max_device_cursor_txn(txn, &uid).map_err(to_kv)?;
            let existing_seq = self
                .feed_by_room
                .get(txn, &(uid.clone(), rid.clone()))
                .map_err(to_kv)?
                .map(|b| decode_u64(&b))
                .transpose()
                .map_err(to_kv)?;

            let value = json_encode(&FeedValue {
                room_id: rid.clone(),
                room_pos,
            })
            .map_err(to_kv)?;

            if let Some(seq) = existing_seq
                && seq > max_cursor
            {
                // Coalesce: overwrite the still-unconsumed entry in place. No new row, no
                // `feed_by_room` update needed (the pointer is unchanged).
                self.feed
                    .put(txn, &(uid.clone(), seq), &value)
                    .map_err(to_kv)?;
                return Ok(seq);
            }

            let latest = self.latest_feed_seq_txn(txn, &uid).map_err(to_kv)?;
            let new_seq = latest + 1;
            self.feed
                .put(txn, &(uid.clone(), new_seq), &value)
                .map_err(to_kv)?;
            self.feed_by_room
                .put(txn, &(uid.clone(), rid.clone()), &new_seq.to_be_bytes())
                .map_err(to_kv)?;
            Ok(new_seq)
        })
        .map_err(StoreError::Kv)
    }

    async fn feed_since(
        &self,
        user_id: &UserId,
        since_feed_seq: u64,
    ) -> Result<Vec<FeedEntry>, StoreError> {
        let uid = user_id.to_string();
        let snap = self.backend.snapshot();
        let mut spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(uid.clone(),));
        spec.start = std::ops::Bound::Excluded(Bytes::from(hs_tables::key::encode(&(
            uid.clone(),
            since_feed_seq,
        ))));
        let mut out = Vec::new();
        for item in self.feed.range(&snap, spec) {
            let ((_, feed_seq), value) = item.map_err(StoreError::Table)?;
            let decoded: FeedValue = json_decode(&value)?;
            let room_id = ruma::RoomId::parse(&decoded.room_id)
                .map_err(|e| StoreError::Codec(e.to_string()))?
                .to_owned();
            out.push(FeedEntry {
                feed_seq,
                room_id,
                room_pos: decoded.room_pos,
            });
        }
        Ok(out)
    }

    async fn latest_feed_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        self.latest_feed_seq_txn(&snap, user_id.as_ref())
    }

    async fn room_pos_as_of(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        as_of_feed_seq: u64,
    ) -> Result<Option<i64>, StoreError> {
        let uid = user_id.to_string();
        let rid = room_id.as_str();
        let snap = self.backend.snapshot();
        let lower = Bytes::from(hs_tables::key::encode(&(uid.clone(), 0u64)));
        let upper = Bytes::from(hs_tables::key::encode(&(uid.clone(), as_of_feed_seq)));
        let mut spec = RangeSpec::new(
            std::ops::Bound::Included(lower),
            std::ops::Bound::Included(upper),
        );
        spec.reverse = true;
        for item in self.feed.range(&snap, spec) {
            let (_, value) = item.map_err(StoreError::Table)?;
            let decoded: FeedValue = json_decode(&value)?;
            if decoded.room_id == rid {
                return Ok(Some(decoded.room_pos));
            }
        }
        Ok(None)
    }

    async fn current_feed_entry(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<FeedEntry>, StoreError> {
        let uid = user_id.to_string();
        let snap = self.backend.snapshot();
        let Some(seq) = self
            .feed_by_room
            .get(&snap, &(uid.clone(), room_id.to_string()))
            .map_err(StoreError::Table)?
        else {
            return Ok(None);
        };
        let feed_seq = decode_u64(&seq)?;
        // The pointer and the row it points at are read from one snapshot, so a coalescing
        // write landing in between cannot leave this looking at a row that has moved on.
        match self
            .feed
            .get(&snap, &(uid, feed_seq))
            .map_err(StoreError::Table)?
        {
            Some(value) => {
                let decoded: FeedValue = json_decode(&value)?;
                Ok(Some(FeedEntry {
                    feed_seq,
                    room_id: room_id.to_owned(),
                    room_pos: decoded.room_pos,
                }))
            }
            None => Err(StoreError::Codec(format!(
                "feed_by_room points at a feed row that does not exist: ({user_id}, {feed_seq})"
            ))),
        }
    }

    async fn record_device_cursor(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        feed_seq: u64,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), device_id.to_string());
        transact(&self.backend, TransactConfig::default(), |txn| {
            // Monotonic: never move a device's recorded cursor backwards (a `record_device_cursor`
            // call racing an earlier one, or an out-of-order retry, should not un-pin an entry
            // this device's own earlier token already pinned).
            let current = self
                .device_cursors
                .get(txn, &key)
                .map_err(to_kv)?
                .map(|b| decode_u64(&b))
                .transpose()
                .map_err(to_kv)?
                .unwrap_or(0);
            let next = current.max(feed_seq);
            self.device_cursors
                .put(txn, &key, &next.to_be_bytes())
                .map_err(to_kv)
        })
        .map_err(StoreError::Kv)
    }

    async fn max_device_cursor(&self, user_id: &UserId) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        self.max_device_cursor_txn(&snap, user_id.as_ref())
    }

    async fn append_hot_position(
        &self,
        room_id: &RoomId,
        room_pos: i64,
    ) -> Result<u64, StoreError> {
        let rid = room_id.to_string();
        transact(&self.backend, TransactConfig::default(), |txn| {
            // The counter and the row in one serializable transaction: a reader that sees the
            // counter at `n` sees every row up to `n` (`atomic_add` conflicts rather than
            // interleaves), which is what lets a token's `hot_seq` bound a batch.
            let seq = txn
                .atomic_add(&self.ephemeral_counters, HOT_POSITIONS_COUNTER, 1)
                .map_err(to_kv)?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            let seq = seq as u64;
            self.hot_positions
                .put(txn, &(rid.clone(), seq), &room_pos.to_be_bytes())
                .map_err(to_kv)?;
            Ok(seq)
        })
        .map_err(StoreError::Kv)
    }

    async fn index_room_members_if_absent(
        &self,
        room_id: &RoomId,
        members: &[ruma::OwnedUserId],
    ) -> Result<bool, StoreError> {
        let rid = room_id.to_string();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let marker = (rid.clone(), INDEXED_MARKER.to_owned());
            if self
                .room_members
                .get(txn, &marker)
                .map_err(to_kv)?
                .is_some()
            {
                return Ok(false);
            }
            self.room_members.put(txn, &marker, &[]).map_err(to_kv)?;
            for member in members {
                self.room_members
                    .put(txn, &(rid.clone(), member.to_string()), &[])
                    .map_err(to_kv)?;
            }
            Ok(true)
        })
        .map_err(StoreError::Kv)
    }

    async fn apply_room_member_changes(
        &self,
        room_id: &RoomId,
        changes: &[(ruma::OwnedUserId, bool)],
    ) -> Result<bool, StoreError> {
        let rid = room_id.to_string();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let marker = (rid.clone(), INDEXED_MARKER.to_owned());
            if self
                .room_members
                .get(txn, &marker)
                .map_err(to_kv)?
                .is_none()
            {
                return Ok(false);
            }
            for (member, joined) in changes {
                let key = (rid.clone(), member.to_string());
                if *joined {
                    self.room_members.put(txn, &key, &[]).map_err(to_kv)?;
                } else {
                    self.room_members.delete(txn, &key).map_err(to_kv)?;
                }
            }
            Ok(true)
        })
        .map_err(StoreError::Kv)
    }

    async fn room_member_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<Vec<ruma::OwnedUserId>>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(room_id.to_string(),));
        let mut indexed = false;
        let mut members = Vec::new();
        for item in self.room_members.range(&snap, spec) {
            let ((_, user), _) = item.map_err(StoreError::Table)?;
            if user == INDEXED_MARKER {
                indexed = true;
                continue;
            }
            members.push(ruma::UserId::parse(&user).map_err(|e| StoreError::Codec(e.to_string()))?);
        }
        Ok(indexed.then_some(members))
    }

    async fn forget_room_members(&self, room_id: &RoomId) -> Result<(), StoreError> {
        let rid = room_id.to_string();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let spec = TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(rid.clone(),));
            let mut keys = Vec::new();
            for item in self.room_members.range(&*txn, spec) {
                let (key, _) = item.map_err(to_kv)?;
                keys.push(key);
            }
            for key in keys {
                self.room_members.delete(txn, &key).map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(StoreError::Kv)
    }

    async fn latest_hot_seq(&self) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        read_counter(&snap, &self.ephemeral_counters, HOT_POSITIONS_COUNTER)
    }

    async fn hot_room_pos_as_of(
        &self,
        room_id: &RoomId,
        as_of_hot_seq: u64,
    ) -> Result<Option<i64>, StoreError> {
        let rid = room_id.to_string();
        let snap = self.backend.snapshot();
        let lower = Bytes::from(hs_tables::key::encode(&(rid.clone(), 0u64)));
        let upper = Bytes::from(hs_tables::key::encode(&(rid, as_of_hot_seq)));
        let mut spec = RangeSpec::new(
            std::ops::Bound::Included(lower),
            std::ops::Bound::Included(upper),
        );
        spec.reverse = true;
        spec.limit = Some(1);
        match self.hot_positions.range(&snap, spec).next() {
            Some(item) => {
                let (_, value) = item.map_err(StoreError::Table)?;
                Ok(Some(decode_i64(&value)?))
            }
            None => Ok(None),
        }
    }

    async fn latest_hot_seq_of_room(&self, room_id: &RoomId) -> Result<Option<u64>, StoreError> {
        let snap = self.backend.snapshot();
        let mut spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(room_id.to_string(),));
        spec.reverse = true;
        spec.limit = Some(1);
        match self.hot_positions.range(&snap, spec).next() {
            Some(item) => {
                let ((_, seq), _) = item.map_err(StoreError::Table)?;
                Ok(Some(seq))
            }
            None => Ok(None),
        }
    }

    async fn latest_account_data_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        match snap
            .get(&self.account_data_counter, user_id.as_bytes())
            .map_err(StoreError::Kv)?
        {
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_ref()
                    .try_into()
                    .map_err(|_| StoreError::Codec("expected an 8-byte counter".to_owned()))?;
                #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
                Ok(i64::from_be_bytes(arr) as u64)
            }
            None => Ok(0),
        }
    }

    async fn set_membership(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        membership: &str,
        room_pos: i64,
        hot_room: bool,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), room_id.to_string());
        let value = json_encode(&MembershipRecord {
            room_id: room_id.to_owned(),
            membership: membership.to_owned(),
            room_pos,
            hot_room,
        })?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.memberships.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(StoreError::Kv)
    }

    async fn get_membership(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<MembershipRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (user_id.to_string(), room_id.to_string());
        match self
            .memberships
            .get(&snap, &key)
            .map_err(StoreError::Table)?
        {
            Some(bytes) => Ok(Some(json_decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn list_memberships(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<MembershipRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(user_id.to_string(),));
        let mut out = Vec::new();
        for item in self.memberships.range(&snap, spec) {
            let (_, value) = item.map_err(StoreError::Table)?;
            out.push(json_decode(&value)?);
        }
        Ok(out)
    }

    async fn put_global_account_data(
        &self,
        user_id: &UserId,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<u64, StoreError> {
        let key = (user_id.to_string(), event_type.to_owned());
        let uid_bytes = user_id.as_bytes().to_vec();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let seq = txn
                .atomic_add(&self.account_data_counter, &uid_bytes, 1)
                .map_err(to_kv)?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            let seq = seq as u64;
            let value = json_encode(&AccountDataValue {
                content: content.clone(),
                changed_seq: seq,
            })
            .map_err(to_kv)?;
            self.account_data_global
                .put(txn, &key, &value)
                .map_err(to_kv)?;
            Ok(seq)
        })
        .map_err(StoreError::Kv)
    }

    async fn list_global_account_data(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<AccountDataRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(user_id.to_string(),));
        let mut out = Vec::new();
        for item in self.account_data_global.range(&snap, spec) {
            let ((_, event_type), value) = item.map_err(StoreError::Table)?;
            let decoded: AccountDataValue = json_decode(&value)?;
            out.push(AccountDataRecord {
                event_type,
                content: decoded.content,
                changed_seq: decoded.changed_seq,
            });
        }
        Ok(out)
    }

    async fn get_global_account_data(
        &self,
        user_id: &UserId,
        event_type: &str,
    ) -> Result<Option<AccountDataRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (user_id.to_string(), event_type.to_owned());
        match self
            .account_data_global
            .get(&snap, &key)
            .map_err(StoreError::Table)?
        {
            Some(bytes) => {
                let decoded: AccountDataValue = json_decode(&bytes)?;
                Ok(Some(AccountDataRecord {
                    event_type: event_type.to_owned(),
                    content: decoded.content,
                    changed_seq: decoded.changed_seq,
                }))
            }
            None => Ok(None),
        }
    }

    async fn put_room_account_data(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<u64, StoreError> {
        let key = (
            user_id.to_string(),
            room_id.to_string(),
            event_type.to_owned(),
        );
        let uid_bytes = user_id.as_bytes().to_vec();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let seq = txn
                .atomic_add(&self.account_data_counter, &uid_bytes, 1)
                .map_err(to_kv)?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            let seq = seq as u64;
            let value = json_encode(&AccountDataValue {
                content: content.clone(),
                changed_seq: seq,
            })
            .map_err(to_kv)?;
            self.account_data_room
                .put(txn, &key, &value)
                .map_err(to_kv)?;
            Ok(seq)
        })
        .map_err(StoreError::Kv)
    }

    async fn list_room_account_data(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Vec<AccountDataRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(
            user_id.to_string(),
            room_id.to_string(),
        ));
        let mut out = Vec::new();
        for item in self.account_data_room.range(&snap, spec) {
            let ((_, _, event_type), value) = item.map_err(StoreError::Table)?;
            let decoded: AccountDataValue = json_decode(&value)?;
            out.push(AccountDataRecord {
                event_type,
                content: decoded.content,
                changed_seq: decoded.changed_seq,
            });
        }
        Ok(out)
    }

    async fn put_filter(
        &self,
        user_id: &UserId,
        filter_json: serde_json::Value,
    ) -> Result<String, StoreError> {
        let filter_id: String = std::iter::repeat_with(|| rand::rng().sample(Alphanumeric) as char)
            .take(16)
            .collect();
        let key = (user_id.to_string(), filter_id.clone());
        let value = json_encode(&filter_json)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.filters.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(StoreError::Kv)?;
        Ok(filter_id)
    }

    async fn get_filter(
        &self,
        user_id: &UserId,
        filter_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let snap = self.backend.snapshot();
        let key = (user_id.to_string(), filter_id.to_owned());
        match self.filters.get(&snap, &key).map_err(StoreError::Table)? {
            Some(bytes) => Ok(Some(json_decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn upsert_public_room(&self, entry: PublicRoomEntry) -> Result<(), StoreError> {
        let key = (entry.room_id.to_string(),);
        let value = json_encode(&entry)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.public_rooms.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(StoreError::Kv)
    }

    async fn remove_public_room(&self, room_id: &RoomId) -> Result<(), StoreError> {
        let key = (room_id.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.public_rooms.delete(txn, &key).map_err(to_kv)
        })
        .map_err(StoreError::Kv)
    }

    async fn list_public_rooms(&self) -> Result<Vec<PublicRoomEntry>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = RangeSpec::full();
        let mut out = Vec::new();
        for item in self.public_rooms.range(&snap, spec) {
            let (_, value) = item.map_err(StoreError::Table)?;
            out.push(json_decode(&value)?);
        }
        Ok(out)
    }

    async fn put_receipt(
        &self,
        room_id: &RoomId,
        receipt: &StoredReceipt,
    ) -> Result<(), StoreError> {
        let key = (
            room_id.to_string(),
            receipt.user_id.clone(),
            receipt.kind.clone(),
        );
        let value = json_encode(receipt)?;
        let stream_value = json_encode(&ReceiptStreamValue {
            room_id: room_id.to_string(),
            receipt: receipt.clone(),
        })?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.receipts.put(txn, &key, &value).map_err(to_kv)?;
            // The server-wide stream (`UserStore::receipt_stream_since`), in the same
            // transaction: an entry exists exactly when the receipt does.
            let pos = txn
                .atomic_add(&self.ephemeral_counters, b"receipt_stream", 1)
                .map_err(to_kv)?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            let pos = pos as u64;
            self.receipt_stream
                .put(txn, &(pos,), &stream_value)
                .map_err(to_kv)
        })
        .map_err(StoreError::Kv)
    }

    async fn list_room_receipts(&self, room_id: &RoomId) -> Result<Vec<StoredReceipt>, StoreError> {
        let snap = self.backend.snapshot();
        let spec =
            TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(room_id.to_string(),));
        let mut out = Vec::new();
        for item in self.receipts.range(&snap, spec) {
            let (_, value) = item.map_err(StoreError::Table)?;
            out.push(json_decode(&value)?);
        }
        Ok(out)
    }

    async fn put_presence(
        &self,
        user_id: &UserId,
        presence: &StoredPresence,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(),);
        let value = json_encode(presence)?;
        let stream_value = user_id.to_string().into_bytes();
        transact(&self.backend, TransactConfig::default(), |txn| {
            // A stream entry (`UserStore::presence_stream_since`) only for a change: a
            // `last_active` refresh keeps the record's stamp and is nobody's news.
            let changed = match self.presence.get(txn, &key).map_err(to_kv)? {
                Some(old) => {
                    json_decode::<StoredPresence>(&old).is_ok_and(|old| old.seq != presence.seq)
                }
                None => true,
            };
            self.presence.put(txn, &key, &value).map_err(to_kv)?;
            if changed {
                let pos = txn
                    .atomic_add(&self.ephemeral_counters, b"presence_stream", 1)
                    .map_err(to_kv)?;
                #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
                let pos = pos as u64;
                self.presence_stream
                    .put(txn, &(pos,), &stream_value)
                    .map_err(to_kv)?;
            }
            Ok(())
        })
        .map_err(StoreError::Kv)
    }

    async fn get_presence(&self, user_id: &UserId) -> Result<Option<StoredPresence>, StoreError> {
        let snap = self.backend.snapshot();
        match self
            .presence
            .get(&snap, &(user_id.to_string(),))
            .map_err(StoreError::Table)?
        {
            Some(bytes) => Ok(Some(json_decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn receipt_stream_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<ReceiptStreamEntry>, StoreError> {
        let snap = self.backend.snapshot();
        let start = std::ops::Bound::Excluded(Bytes::from(hs_tables::key::encode(&(since,))));
        let spec = RangeSpec::new(start, std::ops::Bound::Unbounded).limit(limit.max(1));
        let mut out = Vec::new();
        for item in self.receipt_stream.range(&snap, spec) {
            let ((pos,), value) = item.map_err(StoreError::Table)?;
            let row: ReceiptStreamValue = json_decode(&value)?;
            out.push(ReceiptStreamEntry {
                pos,
                room_id: row.room_id,
                receipt: row.receipt,
            });
        }
        Ok(out)
    }

    async fn receipt_stream_head(&self) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        read_counter(&snap, &self.ephemeral_counters, b"receipt_stream")
    }

    async fn prune_receipt_stream(&self, below: u64) -> Result<usize, StoreError> {
        prune_stream(&self.backend, &self.receipt_stream, below)
    }

    async fn presence_stream_since(
        &self,
        since: u64,
        limit: usize,
    ) -> Result<Vec<PresenceStreamEntry>, StoreError> {
        let snap = self.backend.snapshot();
        let start = std::ops::Bound::Excluded(Bytes::from(hs_tables::key::encode(&(since,))));
        let spec = RangeSpec::new(start, std::ops::Bound::Unbounded).limit(limit.max(1));
        let mut out = Vec::new();
        for item in self.presence_stream.range(&snap, spec) {
            let ((pos,), value) = item.map_err(StoreError::Table)?;
            let user_id = String::from_utf8(value.to_vec())
                .map_err(|e| StoreError::Codec(format!("non-utf8 user id in stream: {e}")))?;
            out.push(PresenceStreamEntry { pos, user_id });
        }
        Ok(out)
    }

    async fn presence_stream_head(&self) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        read_counter(&snap, &self.ephemeral_counters, b"presence_stream")
    }

    async fn prune_presence_stream(&self, below: u64) -> Result<usize, StoreError> {
        prune_stream(&self.backend, &self.presence_stream, below)
    }
}

/// Deletes every entry of a `(pos,)`-keyed stream below `below`, returning how many.
fn prune_stream<B: KvBackend>(
    backend: &B,
    stream: &TypedKeyspace<B::Keyspace, (u64,)>,
    below: u64,
) -> Result<usize, StoreError> {
    transact(backend, TransactConfig::default(), |txn| {
        let end = std::ops::Bound::Excluded(Bytes::from(hs_tables::key::encode(&(below,))));
        let spec = RangeSpec::new(std::ops::Bound::Unbounded, end);
        let mut keys = Vec::new();
        for item in stream.range(&*txn, spec) {
            let (k, _v) = item.map_err(to_kv)?;
            keys.push(k);
        }
        let count = keys.len();
        for k in keys {
            stream.delete(txn, &k).map_err(to_kv)?;
        }
        Ok(count)
    })
    .map_err(StoreError::Kv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use ruma::{room_id, user_id};

    fn store() -> TablesUserStore<MemoryBackend> {
        TablesUserStore::open(MemoryBackend::new()).unwrap()
    }

    /// Every receipt written is one entry of the receipt stream, carrying the receipt; a
    /// presence write is an entry only when its stamp changed. Both are read from a position
    /// and pruned below one.
    #[tokio::test]
    async fn receipt_and_presence_writes_append_to_the_server_wide_streams() {
        let s = store();
        let room = room_id!("!a:example.org");
        let alice = user_id!("@alice:example.org");
        assert_eq!(s.receipt_stream_head().await.unwrap(), 0);
        assert_eq!(s.presence_stream_head().await.unwrap(), 0);

        let receipt = |event: &str, seq: u64| StoredReceipt {
            user_id: alice.to_string(),
            kind: "m.read".to_owned(),
            event_id: event.to_owned(),
            ts: 1000,
            seq,
        };
        s.put_receipt(room, &receipt("$one", 10)).await.unwrap();
        s.put_receipt(room, &receipt("$two", 11)).await.unwrap();
        assert_eq!(s.receipt_stream_head().await.unwrap(), 2);
        let entries = s.receipt_stream_since(0, 10).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pos, 1);
        assert_eq!(entries[0].room_id, room.as_str());
        assert_eq!(entries[0].receipt.event_id, "$one");
        assert_eq!(entries[1].receipt.event_id, "$two");
        assert_eq!(s.receipt_stream_since(1, 10).await.unwrap().len(), 1);
        assert_eq!(s.receipt_stream_since(0, 1).await.unwrap().len(), 1);
        assert_eq!(s.prune_receipt_stream(2).await.unwrap(), 1);
        assert_eq!(s.receipt_stream_since(0, 10).await.unwrap()[0].pos, 2);
        // The receipt is still there.
        assert_eq!(s.list_room_receipts(room).await.unwrap().len(), 1);

        let presence = |seq: u64, active: u64| StoredPresence {
            presence: "online".to_owned(),
            status_msg: None,
            last_active_ms: active,
            seq,
            currently_active: None,
        };
        s.put_presence(alice, &presence(5, 1)).await.unwrap();
        // A `last_active` refresh under the same stamp is not a change.
        s.put_presence(alice, &presence(5, 2)).await.unwrap();
        s.put_presence(alice, &presence(6, 3)).await.unwrap();
        assert_eq!(s.presence_stream_head().await.unwrap(), 2);
        let entries = s.presence_stream_since(0, 10).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].user_id, alice.as_str());
        assert_eq!(entries[1].pos, 2);
        assert_eq!(s.prune_presence_stream(u64::MAX).await.unwrap(), 2);
        assert!(s.presence_stream_since(0, 10).await.unwrap().is_empty());
        assert_eq!(
            s.get_presence(alice).await.unwrap().unwrap().last_active_ms,
            3
        );
    }

    #[tokio::test]
    async fn feed_entries_start_at_one_and_increment() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let room = room_id!("!a:example.org");
        let seq1 = s.append_feed_entry(uid, room, 1).await.unwrap();
        assert_eq!(seq1, 1);
        // Different room -> a fresh feed_seq, not coalesced.
        let room2 = room_id!("!b:example.org");
        let seq2 = s.append_feed_entry(uid, room2, 1).await.unwrap();
        assert_eq!(seq2, 2);
        assert_eq!(s.latest_feed_seq(uid).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn unconsumed_updates_to_the_same_room_coalesce() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let room = room_id!("!a:example.org");
        let seq1 = s.append_feed_entry(uid, room, 1).await.unwrap();
        // Nobody has synced yet (max_device_cursor == 0 < seq1), so this must merge into seq1,
        // not create seq2.
        let seq2 = s.append_feed_entry(uid, room, 2).await.unwrap();
        assert_eq!(seq1, seq2, "coalesced entries share one feed_seq");
        assert_eq!(s.latest_feed_seq(uid).await.unwrap(), 1);

        let entries = s.feed_since(uid, 0).await.unwrap();
        assert_eq!(entries.len(), 1, "coalesced: one entry, not two");
        assert_eq!(
            entries[0].room_pos, 2,
            "the merged entry carries the latest position"
        );
    }

    #[tokio::test]
    async fn once_a_device_cursor_crosses_an_entry_it_is_never_coalesced_away() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let did: &ruma::DeviceId = "DEV1".into();
        let room = room_id!("!a:example.org");

        let seq1 = s.append_feed_entry(uid, room, 1).await.unwrap();
        // Device syncs and is handed a token covering seq1.
        s.record_device_cursor(uid, did, seq1).await.unwrap();

        // A second update to the same room must now get its own, new feed_seq: seq1 is pinned.
        let seq2 = s.append_feed_entry(uid, room, 2).await.unwrap();
        assert_ne!(seq1, seq2);
        assert!(seq2 > seq1);

        // Both entries are still independently visible.
        let entries = s.feed_since(uid, 0).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].room_pos, 1);
        assert_eq!(entries[1].room_pos, 2);
    }

    #[tokio::test]
    async fn room_pos_as_of_finds_the_nearest_entry_at_or_before_the_token() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let did: &ruma::DeviceId = "DEV1".into();
        let room_a = room_id!("!a:example.org");
        let room_b = room_id!("!b:example.org");

        let seq_a1 = s.append_feed_entry(uid, room_a, 10).await.unwrap();
        s.record_device_cursor(uid, did, seq_a1).await.unwrap();
        let _seq_b1 = s.append_feed_entry(uid, room_b, 20).await.unwrap();
        let seq_a2 = s.append_feed_entry(uid, room_a, 30).await.unwrap();

        // As of the token issued right after the first room_a update, room_a's baseline is 10.
        assert_eq!(
            s.room_pos_as_of(uid, room_a, seq_a1).await.unwrap(),
            Some(10)
        );
        // As of "now" (after the second room_a update), the baseline is the latest, 30.
        assert_eq!(
            s.room_pos_as_of(uid, room_a, seq_a2).await.unwrap(),
            Some(30)
        );
        // room_b never had activity at or before seq_a1.
        assert_eq!(s.room_pos_as_of(uid, room_b, seq_a1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn room_pos_at_token_is_the_rooms_newest_entry_unless_that_is_past_the_token() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let did: &ruma::DeviceId = "DEV1".into();
        let room = room_id!("!a:example.org");
        let other = room_id!("!b:example.org");

        assert_eq!(s.current_feed_entry(uid, room).await.unwrap(), None);
        assert_eq!(s.room_pos_at_token(uid, room, 5).await.unwrap(), None);

        let seq1 = s.append_feed_entry(uid, room, 10).await.unwrap();
        // Coalesced: the newest entry is still seq1, now at 11.
        s.append_feed_entry(uid, room, 11).await.unwrap();
        let current = s.current_feed_entry(uid, room).await.unwrap().unwrap();
        assert_eq!((current.feed_seq, current.room_pos), (seq1, 11));
        assert_eq!(
            s.room_pos_at_token(uid, room, seq1).await.unwrap(),
            Some(11)
        );

        // Pinned, then moved on: a token at seq1 still resolves to seq1's position, a token at
        // the newer entry to the newer one, and a token before either to nothing.
        s.record_device_cursor(uid, did, seq1).await.unwrap();
        s.append_feed_entry(uid, other, 1).await.unwrap();
        let seq3 = s.append_feed_entry(uid, room, 12).await.unwrap();
        assert!(seq3 > seq1);
        assert_eq!(
            s.room_pos_at_token(uid, room, seq1).await.unwrap(),
            Some(11)
        );
        assert_eq!(
            s.room_pos_at_token(uid, room, seq3).await.unwrap(),
            Some(12)
        );
        assert_eq!(
            s.room_pos_at_token(uid, room, seq1 - 1).await.unwrap(),
            None
        );
        assert_eq!(
            s.current_feed_entry(uid, room)
                .await
                .unwrap()
                .unwrap()
                .feed_seq,
            seq3
        );
    }

    /// The hot-room stream is one server-wide sequence across rooms, and a room's position as
    /// of a stream position is its newest entry at or before it.
    #[tokio::test]
    async fn the_hot_room_stream_answers_a_rooms_position_as_of_any_point() {
        let s = store();
        let a = room_id!("!a:example.org");
        let b = room_id!("!b:example.org");
        assert_eq!(s.latest_hot_seq().await.unwrap(), 0);
        assert_eq!(s.latest_hot_seq_of_room(a).await.unwrap(), None);
        assert_eq!(s.hot_room_pos_as_of(a, 10).await.unwrap(), None);

        let a1 = s.append_hot_position(a, 5).await.unwrap();
        let b1 = s.append_hot_position(b, 40).await.unwrap();
        let a2 = s.append_hot_position(a, 7).await.unwrap();
        assert_eq!((a1, b1, a2), (1, 2, 3));
        assert_eq!(s.latest_hot_seq().await.unwrap(), 3);
        assert_eq!(s.latest_hot_seq_of_room(a).await.unwrap(), Some(3));
        assert_eq!(s.latest_hot_seq_of_room(b).await.unwrap(), Some(2));

        assert_eq!(s.hot_room_pos_as_of(a, 0).await.unwrap(), None);
        assert_eq!(s.hot_room_pos_as_of(a, 1).await.unwrap(), Some(5));
        assert_eq!(s.hot_room_pos_as_of(a, 2).await.unwrap(), Some(5));
        assert_eq!(s.hot_room_pos_as_of(a, 3).await.unwrap(), Some(7));
        assert_eq!(s.hot_room_pos_as_of(a, u64::MAX).await.unwrap(), Some(7));
        assert_eq!(s.hot_room_pos_as_of(b, 1).await.unwrap(), None);
        assert_eq!(s.hot_room_pos_as_of(b, 2).await.unwrap(), Some(40));
    }

    /// A room is indexed whole once; after that only changes apply, and an index of a room
    /// that is not indexed is never written by a change. A deleted room's rows go entirely.
    #[tokio::test]
    async fn the_directory_index_is_written_whole_once_then_changed() {
        let s = store();
        let room = room_id!("!r:example.org");
        let alice = user_id!("@alice:example.org").to_owned();
        let bob = user_id!("@bob:example.org").to_owned();
        let carol = user_id!("@carol:example.org").to_owned();
        assert_eq!(s.room_member_ids(room).await.unwrap(), None);
        assert!(
            !s.apply_room_member_changes(room, &[(bob.clone(), true)])
                .await
                .unwrap(),
            "a change to a room that is not indexed writes nothing"
        );
        assert_eq!(s.room_member_ids(room).await.unwrap(), None);

        assert!(
            s.index_room_members_if_absent(room, &[alice.clone(), bob.clone()])
                .await
                .unwrap()
        );
        assert!(
            !s.index_room_members_if_absent(room, std::slice::from_ref(&carol))
                .await
                .unwrap(),
            "a second whole index leaves the first alone"
        );
        assert_eq!(
            s.room_member_ids(room).await.unwrap(),
            Some(vec![alice.clone(), bob.clone()])
        );
        assert!(
            s.apply_room_member_changes(room, &[(bob.clone(), false), (carol.clone(), true)])
                .await
                .unwrap()
        );
        assert_eq!(
            s.room_member_ids(room).await.unwrap(),
            Some(vec![alice, carol])
        );
        // An indexed room with nobody in it is still indexed: an empty list, not `None`.
        let empty = room_id!("!empty:example.org");
        s.index_room_members_if_absent(empty, &[]).await.unwrap();
        assert_eq!(s.room_member_ids(empty).await.unwrap(), Some(Vec::new()));

        s.forget_room_members(room).await.unwrap();
        assert_eq!(s.room_member_ids(room).await.unwrap(), None);
    }

    #[tokio::test]
    async fn feed_since_is_exclusive_of_the_given_token() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let room = room_id!("!a:example.org");
        let seq1 = s.append_feed_entry(uid, room, 1).await.unwrap();
        assert!(s.feed_since(uid, seq1).await.unwrap().is_empty());
        assert_eq!(s.feed_since(uid, seq1 - 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn memberships_round_trip() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let room = room_id!("!a:example.org");
        assert!(s.get_membership(uid, room).await.unwrap().is_none());
        s.set_membership(uid, room, "invite", 1, false)
            .await
            .unwrap();
        let m = s.get_membership(uid, room).await.unwrap().unwrap();
        assert_eq!(m.membership, "invite");
        s.set_membership(uid, room, "join", 2, false).await.unwrap();
        let m = s.get_membership(uid, room).await.unwrap().unwrap();
        assert_eq!(m.membership, "join");
        assert_eq!(s.list_memberships(uid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn account_data_counter_is_shared_across_global_and_room_scope() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let room = room_id!("!a:example.org");
        let g1 = s
            .put_global_account_data(uid, "m.push_rules", serde_json::json!({}))
            .await
            .unwrap();
        let r1 = s
            .put_room_account_data(uid, room, "m.tag", serde_json::json!({"tags": {}}))
            .await
            .unwrap();
        assert!(r1 > g1, "the shared counter keeps advancing across scopes");
        let list = s.list_global_account_data(uid).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].changed_seq, g1);
    }

    #[tokio::test]
    async fn filters_round_trip_by_generated_id() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let body = serde_json::json!({"room": {"timeline": {"limit": 5}}});
        let id = s.put_filter(uid, body.clone()).await.unwrap();
        assert_eq!(s.get_filter(uid, &id).await.unwrap(), Some(body));
        assert_eq!(s.get_filter(uid, "nonexistent").await.unwrap(), None);
    }

    // `prop_assert_eq!` inside the block below comes from the prelude, like every other
    // proptest-using module in the workspace.
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Property: whatever sequence of appends and device-cursor advances happens, every
        /// feed entry `feed_since` ever returns for a room, read back through `room_pos_as_of` at
        /// that exact `feed_seq`, matches what was actually recorded -- i.e. coalescing never
        /// corrupts an entry a live (already-issued) cursor still depends on. `ops`: `0` = append
        /// an update to room A, `1` = append to room B, `2` = advance the (single, simulated)
        /// device's cursor to the feed's current latest.
        #[test]
        fn coalescing_never_loses_a_pinned_entry(
            ops in proptest::collection::vec(0u8..3, 1..40),
            room_pos_seed in 1i64..1000,
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let s = store();
                let uid = user_id!("@alice:example.org");
                let did: &ruma::DeviceId = "DEV1".into();
                let room_a = room_id!("!a:example.org");
                let room_b = room_id!("!b:example.org");

                // Every feed_seq this test's own bookkeeping believes was pinned by an issued
                // device cursor, paired with the room_pos it must still resolve to.
                let mut pinned: Vec<(u64, ruma::OwnedRoomId, i64)> = Vec::new();
                let mut pos = room_pos_seed;

                for op in ops {
                    pos += 1;
                    match op {
                        0 => {
                            s.append_feed_entry(uid, room_a, pos).await.unwrap();
                        }
                        1 => {
                            s.append_feed_entry(uid, room_b, pos).await.unwrap();
                        }
                        _ => {
                            let latest = s.latest_feed_seq(uid).await.unwrap();
                            if latest > 0 {
                                s.record_device_cursor(uid, did, latest).await.unwrap();
                                // Every feed entry that exists right now, for either room, is
                                // pinned as of this cursor advance: record its current value.
                                for room in [room_a, room_b] {
                                    if let Some(p) = s.room_pos_as_of(uid, room, latest).await.unwrap() {
                                        pinned.push((latest, room.to_owned(), p));
                                    }
                                }
                            }
                        }
                    }
                }

                for (as_of, room, expected_pos) in pinned {
                    let found = s.room_pos_as_of(uid, &room, as_of).await.unwrap();
                    prop_assert_eq!(found, Some(expected_pos));
                }
                Ok(())
            })?;
        }
    }
}
