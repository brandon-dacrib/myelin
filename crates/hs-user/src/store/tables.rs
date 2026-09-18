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

use super::{AccountDataRecord, FeedEntry, MembershipRecord, PublicRoomEntry, StoreError, UserStore};

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
        for item in self.feed.range(txn, spec) {
            let ((_, seq), _) = item.map_err(StoreError::Table)?;
            return Ok(seq);
        }
        Ok(0)
    }

    fn max_device_cursor_txn<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &R,
        uid: &str,
    ) -> Result<u64, StoreError> {
        let spec =
            TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(uid.to_owned(),));
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

            let value =
                json_encode(&FeedValue { room_id: rid.clone(), room_pos }).map_err(to_kv)?;

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
        spec.start = std::ops::Bound::Excluded(Bytes::from(
            hs_tables::key::encode(&(uid.clone(), since_feed_seq)),
        ));
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
        self.latest_feed_seq_txn(&snap, &user_id.to_string())
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
        self.max_device_cursor_txn(&snap, &user_id.to_string())
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
        match self.memberships.get(&snap, &key).map_err(StoreError::Table)? {
            Some(bytes) => Ok(Some(json_decode(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn list_memberships(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<MembershipRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let spec =
            TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(user_id.to_string(),));
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
            let value = json_encode(&AccountDataValue { content: content.clone(), changed_seq: seq })
                .map_err(to_kv)?;
            self.account_data_global.put(txn, &key, &value).map_err(to_kv)?;
            Ok(seq)
        })
        .map_err(StoreError::Kv)
    }

    async fn list_global_account_data(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<AccountDataRecord>, StoreError> {
        let snap = self.backend.snapshot();
        let spec =
            TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(user_id.to_string(),));
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
        let key = (user_id.to_string(), room_id.to_string(), event_type.to_owned());
        let uid_bytes = user_id.as_bytes().to_vec();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let seq = txn
                .atomic_add(&self.account_data_counter, &uid_bytes, 1)
                .map_err(to_kv)?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            let seq = seq as u64;
            let value = json_encode(&AccountDataValue { content: content.clone(), changed_seq: seq })
                .map_err(to_kv)?;
            self.account_data_room.put(txn, &key, &value).map_err(to_kv)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use ruma::{room_id, user_id};

    fn store() -> TablesUserStore<MemoryBackend> {
        TablesUserStore::open(MemoryBackend::new()).unwrap()
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
        assert_eq!(entries[0].room_pos, 2, "the merged entry carries the latest position");
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
        s.set_membership(uid, room, "invite", 1, false).await.unwrap();
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
