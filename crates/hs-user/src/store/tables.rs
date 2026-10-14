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
//! | `hs_user.device_cursor_max` | `(user_id,)` | the maximum over `hs_user.device_cursors` for the user, kept by [`UserStore::record_device_cursor`] so a fan-out reads it with one multi-get rather than a range per member (absent for a user who has not synced since it was added: computed from the range then) |
//! | `hs_user.feed_heads` | `(user_id,)` | the feed's newest `feed_seq` and its *floor* (the retention section below), kept by every append; absent for a user whose feed was last written before it was added (the newest is then the reverse range it always was) |
//! | `hs_user.memberships` | `(user_id, room_id)` | current (not historical) membership snapshot -- the room set for an initial sync |
//! | `hs_user.account_data_global` | `(user_id, event_type)` | global account data |
//! | `hs_user.account_data_room` | `(user_id, room_id, event_type)` | room-scoped account data (`m.tag` and friends) |
//! | `hs_user.account_data_counter` | `user_id` (raw `atomic_add` key, not a [`hs_tables::keyspace::TypedKeyspace`]) | the shared global/room account-data change counter |
//! | `hs_user.filters` | `(user_id, filter_id)` | uploaded named filters (`POST /user/{userId}/filter`) |
//! | `hs_user.receipts` | `(room_id, user_id, kind)`, or `(room_id, user_id, "{kind} {thread_id}")` for a threaded receipt | the latest read receipt of each kind per user per thread per room |
//! | `hs_user.presence` | `user_id` | each user's latest presence |
//! | `hs_user.receipt_stream` | `(pos: u64,)` | the server-wide receipt stream: one entry per receipt written, for appservice delivery |
//! | `hs_user.presence_stream` | `(pos: u64,)` | the server-wide presence stream: one entry per presence change (a new stamp), for appservice delivery |
//! | `hs_user.hot_positions` | `(room_id, hot_seq)` | the server-wide hot-room stream: one entry per update to a room above the fan-out threshold, its `room_pos` -- `crate::token`'s `hot_seq` indexes here |
//! | `hs_user.room_members` | `(room_id, user_id)` | each indexed room's joined members, for the user directory; the row with an empty `user_id` is the marker that says the room is indexed (`UserStore::index_room_members_if_absent`) |
//! | `hs_user.peeks` | `(user_id, device_id, room_id)` | each device's peeks into world-readable rooms (MSC2753), with the feed position each began at |
//! | `hs_user.room_peekers` | `(room_id, user_id, device_id)` | the same peeks by room, for a room update's fan-out |
//! | `hs_user.ephemeral_counters` | `receipt_stream` / `presence_stream` / `hot_positions` (raw `atomic_add` keys) | the three streams' position counters; `hot_positions_floor` (a plain 8-byte key) is the hot-room stream's floor |
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
//!
//! # Retention: the floor
//!
//! Coalescing bounds how fast a feed grows between two syncs, not how long it is kept: every
//! entry a device was ever handed a token at or past stayed for ever. [`UserStore::compact_feed`]
//! is the retention. It moves the user's *floor* (`hs_user.feed_heads`) up so that at most
//! `keep` entries lie above it, and below the floor keeps exactly one entry per room, the newest,
//! deleting the rest. Three properties make that safe for every token:
//!
//! - **A kept entry is never rewritten.** [`TablesUserStore::append_feed_entry`] coalesces only
//!   into an entry above the floor (as well as above the maximum device cursor), so a kept
//!   entry's `room_pos` stays what it was when it was written.
//! - **A token at or above the floor sees exactly what it did.** Entries above the floor are
//!   untouched, and a room's position as of a token (`room_pos_as_of`) is its newest entry at or
//!   below the token: that entry is either above the floor or the one kept per room, which is
//!   the newest at or below the floor.
//! - **A token below the floor can only repeat, never skip.** [`UserStore::feed_since`] from
//!   such a token still names every room that changed after it: a room with a deleted entry
//!   after the token has its newest entry at or below the floor kept, and that one is at or
//!   after the deleted one, so after the token too. What may be gone is the room's position as
//!   of the token (its entries at or below the token): `room_pos_as_of` then answers a kept
//!   entry no newer than the token, or nothing, and `crate::sync` resumes from there or sends
//!   the room whole.
//!
//! The hot-room stream is compacted the same way ([`UserStore::compact_hot_stream`], the floor in
//! `hs_user.ephemeral_counters`): its entries are never rewritten at all, and a hot room is a
//! candidate on every incremental sync, so the same holds there.

use bytes::Bytes;
use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use rand::Rng;
use rand::distr::Alphanumeric;
use ruma::{DeviceId, OwnedUserId, RoomId, UserId};
use serde::{Deserialize, Serialize};

use super::{
    AccountDataRecord, FanOutReport, FanOutWrite, FeedEntry, MembershipRecord, PresenceStreamEntry,
    PublicRoomEntry, ReceiptStreamEntry, StoreError, StoredPresence, StoredReceipt, UserStore,
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

/// A user's `hs_user.feed_heads` row: where the feed ends and where its compacted part does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FeedHead {
    /// The newest `feed_seq` in the feed, `0` for an empty one.
    latest: u64,
    /// The `feed_seq` at or below which the feed is compacted to one entry per room.
    #[serde(default)]
    floor: u64,
}

/// What [`TablesUserStore::append_feed_entry_txn`]'s coalescing decision reads: the room's
/// current feed entry (the `feed_by_room` pointer) and the user's maximum device cursor.
#[derive(Debug, Clone, Copy)]
struct CoalesceBound {
    existing_seq: Option<u64>,
    max_cursor: u64,
}

/// How many members one fan-out transaction writes at most (`UserStore::apply_fan_out`). A
/// transaction's conflict window grows with its size; a batch that keeps conflicting is retried
/// member by member, so a bound keeps that fallback small too.
const FAN_OUT_BATCH: usize = 100;

/// How many feed or stream rows one compaction call scans at most: a feed that grew for months
/// before retention existed is compacted in pieces, over several calls.
const COMPACTION_SCAN_LIMIT: usize = 50_000;

/// How many feed or stream rows one compaction transaction scans, and so deletes, at most: well
/// under the store's 10,000-mutation transaction limit (`hs_kv::MAX_TXN_MUTATIONS`). Until
/// 2026-10-14 a compaction was one transaction of up to [`COMPACTION_SCAN_LIMIT`] deletes,
/// which the store refused once a window held more than 10,000 rows to delete: the floor never
/// moved, and a feed's compaction was tried again, and refused again, on every fan-out to it.
const COMPACTION_BATCH: usize = 1_000;

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
    feed_heads: TypedKeyspace<B::Keyspace, (String,)>,
    device_cursors: TypedKeyspace<B::Keyspace, (String, String)>,
    device_cursor_max: TypedKeyspace<B::Keyspace, (String,)>,
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
    peeks: TypedKeyspace<B::Keyspace, (String, String, String)>,
    room_peekers: TypedKeyspace<B::Keyspace, (String, String, String)>,
    ephemeral_counters: B::Keyspace,
}

/// The `user_id` of a room's marker row in `hs_user.room_members`: no user id is empty, and it
/// sorts before every real one.
const INDEXED_MARKER: &str = "";

/// The `hs_user.ephemeral_counters` key of the hot-room stream's position counter.
const HOT_POSITIONS_COUNTER: &[u8] = b"hot_positions";

/// The `hs_user.ephemeral_counters` key of the hot-room stream's floor
/// (`UserStore::compact_hot_stream`): a plain 8-byte big-endian value, not an `atomic_add` key.
const HOT_POSITIONS_FLOOR: &[u8] = b"hot_positions_floor";

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
            feed_heads: TypedKeyspace::new(open("hs_user.feed_heads")?),
            device_cursors: TypedKeyspace::new(open("hs_user.device_cursors")?),
            device_cursor_max: TypedKeyspace::new(open("hs_user.device_cursor_max")?),
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
            peeks: TypedKeyspace::new(open("hs_user.peeks")?),
            room_peekers: TypedKeyspace::new(open("hs_user.room_peekers")?),
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

    /// The newest `feed_seq` of `uid`'s feed as its `hs_user.feed_heads` row says, or, for a feed
    /// last written before that row existed, as the feed's last row says (one reverse range).
    fn feed_head_txn<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &R,
        uid: &str,
    ) -> Result<FeedHead, StoreError> {
        match self
            .feed_heads
            .get(txn, &(uid.to_owned(),))
            .map_err(StoreError::Table)?
        {
            Some(bytes) => json_decode(&bytes),
            None => self.feed_head_from_rows_txn(txn, uid),
        }
    }

    /// [`TablesUserStore::feed_head_txn`]'s fallback: the feed's last row, floor `0`.
    fn feed_head_from_rows_txn<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &R,
        uid: &str,
    ) -> Result<FeedHead, StoreError> {
        let mut spec = TypedKeyspace::<B::Keyspace, (String, u64)>::prefix(&(uid.to_owned(),));
        spec.reverse = true;
        spec.limit = Some(1);
        // The spec above is reversed and limited to one row, so this is "the highest sequence
        // this user's feed has", or 0 for a user with no feed rows at all.
        let latest = match self.feed.range(txn, spec).next() {
            Some(item) => {
                let ((_, seq), _) = item.map_err(StoreError::Table)?;
                seq
            }
            None => 0,
        };
        Ok(FeedHead { latest, floor: 0 })
    }

    /// The maximum device cursor of `uid` as its `hs_user.device_cursor_max` row says, or, for a
    /// user who has not synced since that row existed, as the cursors themselves say (one
    /// range).
    fn max_device_cursor_txn<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        txn: &R,
        uid: &str,
    ) -> Result<u64, StoreError> {
        match self
            .device_cursor_max
            .get(txn, &(uid.to_owned(),))
            .map_err(StoreError::Table)?
        {
            Some(bytes) => decode_u64(&bytes),
            None => self.max_device_cursor_from_rows_txn(txn, uid),
        }
    }

    /// [`TablesUserStore::max_device_cursor_txn`]'s fallback: the maximum over the cursors.
    fn max_device_cursor_from_rows_txn<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
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

    /// The one feed append, shared by [`UserStore::append_feed_entry`] (one user) and
    /// [`UserStore::apply_fan_out`] (a batch): the coalescing rule of the module docs, over a
    /// head and pointer the caller has already read. Writes the feed row, the `feed_by_room`
    /// pointer when a row is added, and the head; `head` is updated in place for the next
    /// append in the same transaction. Returns the `feed_seq` the write landed at.
    fn append_feed_entry_txn<T: KvRead<Keyspace = B::Keyspace> + KvWrite>(
        &self,
        txn: &mut T,
        uid: &str,
        rid: &str,
        room_pos: i64,
        bound: CoalesceBound,
        head: &mut FeedHead,
    ) -> Result<u64, StoreError> {
        let value = json_encode(&FeedValue {
            room_id: rid.to_owned(),
            room_pos,
        })?;
        if let Some(seq) = bound.existing_seq
            && seq > bound.max_cursor
            && seq > head.floor
        {
            // Coalesce: overwrite the still-unconsumed entry in place. No new row, no
            // `feed_by_room` update needed (the pointer is unchanged). An entry at or below the
            // floor is one `compact_feed` kept as the room's position as of the floor, and it
            // stays what it was (the module docs, "Retention").
            self.feed
                .put(txn, &(uid.to_owned(), seq), &value)
                .map_err(StoreError::Table)?;
            return Ok(seq);
        }
        let new_seq = head.latest + 1;
        self.feed
            .put(txn, &(uid.to_owned(), new_seq), &value)
            .map_err(StoreError::Table)?;
        self.feed_by_room
            .put(
                txn,
                &(uid.to_owned(), rid.to_owned()),
                &new_seq.to_be_bytes(),
            )
            .map_err(StoreError::Table)?;
        head.latest = new_seq;
        self.feed_heads
            .put(txn, &(uid.to_owned(),), &json_encode(head)?)
            .map_err(StoreError::Table)?;
        Ok(new_seq)
    }

    /// One batch of [`UserStore::apply_fan_out`] in one transaction: the memberships, feed
    /// pointers, heads and cursor maxima of the batch's users are read with one multi-get each,
    /// then every write is buffered and committed together.
    fn fan_out_batch_txn(
        &self,
        rid: &str,
        room_pos: i64,
        writes: &[FanOutWrite],
        feed_retention: u64,
        report: &mut FanOutReport,
    ) -> Result<(), hs_kv::KvError> {
        transact(&self.backend, TransactConfig::default(), |txn| {
            let feed_users: Vec<String> = writes
                .iter()
                .filter(|w| w.feed_entry)
                .map(|w| w.user_id.to_string())
                .collect();
            let pointer_keys: Vec<(String, String)> = feed_users
                .iter()
                .map(|uid| (uid.clone(), rid.to_owned()))
                .collect();
            let user_keys: Vec<(String,)> = feed_users.iter().map(|uid| (uid.clone(),)).collect();
            let pointers = self
                .feed_by_room
                .multi_get(txn, &pointer_keys)
                .map_err(to_kv)?;
            let heads = self.feed_heads.multi_get(txn, &user_keys).map_err(to_kv)?;
            let maxima = self
                .device_cursor_max
                .multi_get(txn, &user_keys)
                .map_err(to_kv)?;

            let mut records = 0usize;
            let mut entries = 0usize;
            let mut to_compact = Vec::new();
            let mut feed_index = 0usize;
            for write in writes {
                let uid = write.user_id.to_string();
                if let Some((membership, baseline_pos)) = &write.record {
                    let value = json_encode(&MembershipRecord {
                        room_id: ruma::RoomId::parse(rid)
                            .map_err(|e| to_kv(StoreError::Codec(e.to_string())))?
                            .to_owned(),
                        membership: membership.clone(),
                        room_pos: *baseline_pos,
                        hot_room: write.hot_room,
                    })
                    .map_err(to_kv)?;
                    self.memberships
                        .put(txn, &(uid.clone(), rid.to_owned()), &value)
                        .map_err(to_kv)?;
                    records += 1;
                }
                if !write.feed_entry {
                    continue;
                }
                let i = feed_index;
                feed_index += 1;
                let existing_seq = pointers[i]
                    .as_ref()
                    .map(|b| decode_u64(b))
                    .transpose()
                    .map_err(to_kv)?;
                let mut head = match &heads[i] {
                    Some(bytes) => json_decode(bytes).map_err(to_kv)?,
                    None => self.feed_head_from_rows_txn(txn, &uid).map_err(to_kv)?,
                };
                let max_cursor = match &maxima[i] {
                    Some(bytes) => decode_u64(bytes).map_err(to_kv)?,
                    None => {
                        // A user who has not synced since the maximum was kept: computed from
                        // the cursors once, and written so the next fan-out has it to multi-get.
                        let max = self
                            .max_device_cursor_from_rows_txn(txn, &uid)
                            .map_err(to_kv)?;
                        self.device_cursor_max
                            .put(txn, &(uid.clone(),), &max.to_be_bytes())
                            .map_err(to_kv)?;
                        max
                    }
                };
                self.append_feed_entry_txn(
                    txn,
                    &uid,
                    rid,
                    room_pos,
                    CoalesceBound {
                        existing_seq,
                        max_cursor,
                    },
                    &mut head,
                )
                .map_err(to_kv)?;
                entries += 1;
                if feed_retention > 0
                    && head.latest.saturating_sub(head.floor) > feed_retention.saturating_mul(2)
                {
                    to_compact.push(write.user_id.clone());
                }
            }
            Ok((records, entries, to_compact))
        })
        .map(|(records, entries, to_compact)| {
            report.records += records;
            report.feed_entries += entries;
            report.transactions += 1;
            report.feeds_to_compact.extend(to_compact);
        })
    }
}

/// What one batch of [`TablesUserStore::compact_feed_batch`] did.
struct FeedCompactionBatch {
    /// Entries deleted.
    deleted: usize,
    /// Entries read.
    scanned: usize,
    /// The lowest `feed_seq` read, where the next batch continues below.
    lowest: Option<u64>,
}

impl<B: KvBackend> TablesUserStore<B> {
    /// One batch of [`UserStore::compact_feed`]: up to [`COMPACTION_BATCH`] of `uid`'s feed
    /// entries below `upper`, newest first, deleting each whose room is already in `seen` (a
    /// newer entry of the room stays) and adding the others' rooms to it.
    fn compact_feed_batch(
        &self,
        txn: &mut B::Txn,
        uid: &str,
        upper: std::ops::Bound<Bytes>,
        seen: &mut std::collections::HashSet<String>,
    ) -> Result<FeedCompactionBatch, hs_kv::KvError> {
        let lower =
            std::ops::Bound::Included(Bytes::from(hs_tables::key::encode(&(uid.to_owned(), 0u64))));
        let spec = RangeSpec::new(lower, upper)
            .reverse()
            .limit(COMPACTION_BATCH);
        let mut rows: Vec<(u64, String)> = Vec::with_capacity(COMPACTION_BATCH);
        for item in self.feed.range(&*txn, spec) {
            let ((_, seq), value) = item.map_err(to_kv)?;
            let decoded: FeedValue = json_decode(&value).map_err(to_kv)?;
            rows.push((seq, decoded.room_id));
        }
        let mut deleted = 0;
        for (seq, room) in &rows {
            if !seen.insert(room.clone()) {
                self.feed
                    .delete(txn, &(uid.to_owned(), *seq))
                    .map_err(to_kv)?;
                deleted += 1;
            }
        }
        Ok(FeedCompactionBatch {
            deleted,
            scanned: rows.len(),
            lowest: rows.last().map(|(seq, _)| *seq),
        })
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
            let mut head = self.feed_head_txn(txn, &uid).map_err(to_kv)?;
            self.append_feed_entry_txn(
                txn,
                &uid,
                &rid,
                room_pos,
                CoalesceBound {
                    existing_seq,
                    max_cursor,
                },
                &mut head,
            )
            .map_err(to_kv)
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
        Ok(self.feed_head_txn(&snap, user_id.as_ref())?.latest)
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
                .map_err(to_kv)?;
            // The maximum over the user's devices, kept beside the cursors so a fan-out reads
            // it with one multi-get (`fan_out_batch_txn`) rather than a range per member.
            // Written only when it moves (or has never been written): every write here is a
            // conflict for a fan-out that read it in the meantime.
            let row = self
                .device_cursor_max
                .get(txn, &(key.0.clone(),))
                .map_err(to_kv)?;
            let max = match &row {
                Some(bytes) => decode_u64(bytes).map_err(to_kv)?,
                None => self
                    .max_device_cursor_from_rows_txn(txn, &key.0)
                    .map_err(to_kv)?,
            };
            if row.is_none() || next > max {
                self.device_cursor_max
                    .put(txn, &(key.0.clone(),), &next.max(max).to_be_bytes())
                    .map_err(to_kv)?;
            }
            Ok(())
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

    async fn reconcile_room_members(
        &self,
        room_id: &RoomId,
        joined: &[ruma::OwnedUserId],
    ) -> Result<Option<(usize, usize)>, StoreError> {
        let rid = room_id.to_string();
        let wanted: std::collections::BTreeSet<String> =
            joined.iter().map(ToString::to_string).collect();
        transact(&self.backend, TransactConfig::default(), |txn| {
            let spec = TypedKeyspace::<B::Keyspace, (String, String)>::prefix(&(rid.clone(),));
            let mut indexed = false;
            let mut present = std::collections::BTreeSet::new();
            for item in self.room_members.range(&*txn, spec) {
                let ((_, user), _) = item.map_err(to_kv)?;
                if user == INDEXED_MARKER {
                    indexed = true;
                } else {
                    present.insert(user);
                }
            }
            if !indexed {
                return Ok(None);
            }
            let mut added = 0usize;
            for user in wanted.difference(&present) {
                self.room_members
                    .put(txn, &(rid.clone(), user.clone()), &[])
                    .map_err(to_kv)?;
                added += 1;
            }
            let mut removed = 0usize;
            for user in present.difference(&wanted) {
                self.room_members
                    .delete(txn, &(rid.clone(), user.clone()))
                    .map_err(to_kv)?;
                removed += 1;
            }
            Ok(Some((added, removed)))
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

    async fn get_memberships(
        &self,
        room_id: &RoomId,
        user_ids: &[OwnedUserId],
    ) -> Result<Vec<Option<MembershipRecord>>, StoreError> {
        let snap = self.backend.snapshot();
        let keys: Vec<(String, String)> = user_ids
            .iter()
            .map(|user_id| (user_id.to_string(), room_id.to_string()))
            .collect();
        self.memberships
            .multi_get(&snap, &keys)
            .map_err(StoreError::Table)?
            .into_iter()
            .map(|bytes| bytes.map(|b| json_decode(&b)).transpose())
            .collect()
    }

    async fn apply_fan_out(
        &self,
        room_id: &RoomId,
        room_pos: i64,
        writes: &[FanOutWrite],
        feed_retention: u64,
    ) -> Result<FanOutReport, StoreError> {
        let rid = room_id.to_string();
        let mut report = FanOutReport::default();
        for batch in writes.chunks(FAN_OUT_BATCH) {
            match self.fan_out_batch_txn(&rid, room_pos, batch, feed_retention, &mut report) {
                Ok(()) => {}
                Err(hs_kv::KvError::RetriesExhausted { .. }) => {
                    // The batch's transaction kept conflicting (its members' syncs recording
                    // their cursors while it ran, say). Each member on their own is a short
                    // transaction with a small conflict window: what every fan-out used to be.
                    tracing::warn!(
                        room_id = %room_id,
                        members = batch.len(),
                        "a fan-out batch kept conflicting; writing its members one at a time"
                    );
                    for write in batch {
                        if let Some((membership, baseline_pos)) = &write.record {
                            self.set_membership(
                                &write.user_id,
                                room_id,
                                membership,
                                *baseline_pos,
                                write.hot_room,
                            )
                            .await?;
                            report.records += 1;
                        }
                        if write.feed_entry {
                            self.append_feed_entry(&write.user_id, room_id, room_pos)
                                .await?;
                            report.feed_entries += 1;
                        }
                        report.transactions +=
                            usize::from(write.record.is_some()) + usize::from(write.feed_entry);
                        report.fallbacks += 1;
                    }
                }
                Err(e) => return Err(StoreError::Kv(e)),
            }
        }
        Ok(report)
    }

    async fn compact_feed(&self, user_id: &UserId, keep: u64) -> Result<usize, StoreError> {
        if keep == 0 {
            return Ok(0);
        }
        let uid = user_id.to_string();
        // Newest first, from the target down, a batch to a transaction: the first entry met of
        // each room is its newest at or below the target and stays, every later one goes. The
        // first transaction moves the floor with its batch, so no entry at or below the target
        // is coalesced into while the rest are deleted (the module docs, "Retention"); entries
        // left below the floor by a call that stopped at its scan bound only make a token below
        // the floor repeat more, and the next compaction deletes them.
        let first = transact(&self.backend, TransactConfig::default(), |txn| {
            let mut head = self.feed_head_txn(txn, &uid).map_err(to_kv)?;
            let target = head.latest.saturating_sub(keep);
            if target <= head.floor {
                return Ok(None);
            }
            let upper = std::ops::Bound::Included(Bytes::from(hs_tables::key::encode(&(
                uid.clone(),
                target,
            ))));
            let mut seen = std::collections::HashSet::new();
            let batch = self.compact_feed_batch(txn, &uid, upper, &mut seen)?;
            head.floor = target;
            self.feed_heads
                .put(txn, &(uid.clone(),), &json_encode(&head).map_err(to_kv)?)
                .map_err(to_kv)?;
            Ok(Some((batch, seen)))
        })
        .map_err(StoreError::Kv)?;
        let Some((mut batch, mut seen)) = first else {
            return Ok(0);
        };
        let mut pruned = batch.deleted;
        let mut scanned = batch.scanned;
        while batch.scanned == COMPACTION_BATCH && scanned < COMPACTION_SCAN_LIMIT {
            let Some(lowest) = batch.lowest else {
                break;
            };
            let upper = std::ops::Bound::Excluded(Bytes::from(hs_tables::key::encode(&(
                uid.clone(),
                lowest,
            ))));
            (batch, seen) = transact(&self.backend, TransactConfig::default(), |txn| {
                let mut next_seen = seen.clone();
                let batch = self.compact_feed_batch(txn, &uid, upper.clone(), &mut next_seen)?;
                Ok((batch, next_seen))
            })
            .map_err(StoreError::Kv)?;
            pruned += batch.deleted;
            scanned += batch.scanned;
        }
        Ok(pruned)
    }

    async fn feed_floor(&self, user_id: &UserId) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        Ok(self.feed_head_txn(&snap, user_id.as_ref())?.floor)
    }

    async fn compact_hot_stream(&self, keep: u64) -> Result<usize, StoreError> {
        if keep == 0 {
            return Ok(0);
        }
        let snap = self.backend.snapshot();
        let head = read_counter(&snap, &self.ephemeral_counters, HOT_POSITIONS_COUNTER)?;
        let floor = match snap
            .get(&self.ephemeral_counters, HOT_POSITIONS_FLOOR)
            .map_err(StoreError::Kv)?
        {
            Some(bytes) => decode_u64(&bytes)?,
            None => 0,
        };
        drop(snap);
        let target = head.saturating_sub(keep);
        if target <= floor {
            return Ok(0);
        }
        // The stream is keyed by room then position, so "everything at or below the target" is
        // the whole keyspace read room by room, a batch to a transaction. A room's rows are
        // contiguous and oldest first: a row at or below the target goes when the row after it
        // is the same room's and at or below the target too. The last row of a batch has no
        // row after it yet, so the next batch starts at it. Entries appended meanwhile are
        // above the head, so above the target. The floor moves only when the scan reached the
        // end, within [`COMPACTION_SCAN_LIMIT`] rows.
        let mut start = std::ops::Bound::Unbounded;
        let mut pruned = 0usize;
        let mut scanned = 0usize;
        loop {
            let (deleted, rows, last) = transact(&self.backend, TransactConfig::default(), |txn| {
                let spec = RangeSpec::new(start.clone(), std::ops::Bound::Unbounded)
                    .limit(COMPACTION_BATCH);
                let mut rows: Vec<(String, u64)> = Vec::with_capacity(COMPACTION_BATCH);
                for item in self.hot_positions.range(&*txn, spec) {
                    let ((room, seq), _) = item.map_err(to_kv)?;
                    rows.push((room, seq));
                }
                let mut deleted = 0usize;
                for pair in rows.windows(2) {
                    let ((room, seq), (next_room, next_seq)) = (&pair[0], &pair[1]);
                    if room == next_room && *seq <= target && *next_seq <= target {
                        self.hot_positions
                            .delete(txn, &(room.clone(), *seq))
                            .map_err(to_kv)?;
                        deleted += 1;
                    }
                }
                let complete = rows.len() < COMPACTION_BATCH;
                if complete {
                    txn.put(
                        &self.ephemeral_counters,
                        HOT_POSITIONS_FLOOR,
                        &target.to_be_bytes(),
                    )
                    .map_err(to_kv)?;
                }
                Ok((deleted, rows.len(), rows.pop()))
            })
            .map_err(StoreError::Kv)?;
            pruned += deleted;
            scanned += rows;
            match last {
                Some(last) if rows == COMPACTION_BATCH && scanned < COMPACTION_SCAN_LIMIT => {
                    start = std::ops::Bound::Included(Bytes::from(hs_tables::key::encode(&last)));
                }
                _ => break,
            }
        }
        Ok(pruned)
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

    async fn rooms_with_account_data_since(
        &self,
        user_id: &UserId,
        since: u64,
    ) -> Result<std::collections::BTreeSet<ruma::OwnedRoomId>, StoreError> {
        let snap = self.backend.snapshot();
        let spec =
            TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(user_id.to_string(),));
        let mut rooms = std::collections::BTreeSet::new();
        for item in self.account_data_room.range(&snap, spec) {
            let ((_, room_id, _), value) = item.map_err(StoreError::Table)?;
            let decoded: AccountDataValue = json_decode(&value)?;
            if decoded.changed_seq > since
                && let Ok(room_id) = ruma::RoomId::parse(room_id.as_str())
            {
                rooms.insert(room_id);
            }
        }
        Ok(rooms)
    }

    async fn put_peek(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        feed_seq: u64,
    ) -> Result<(), StoreError> {
        let (uid, did, rid) = (
            user_id.to_string(),
            device_id.to_string(),
            room_id.to_string(),
        );
        let value = json_encode(&feed_seq)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.peeks
                .put(txn, &(uid.clone(), did.clone(), rid.clone()), &value)
                .map_err(to_kv)?;
            self.room_peekers
                .put(txn, &(rid.clone(), uid.clone(), did.clone()), &[])
                .map_err(to_kv)?;
            Ok(())
        })
        .map_err(StoreError::Kv)
    }

    async fn remove_peeks(
        &self,
        user_id: &UserId,
        device_id: Option<&DeviceId>,
        room_id: &RoomId,
    ) -> Result<usize, StoreError> {
        let (uid, rid) = (user_id.to_string(), room_id.to_string());
        let did = device_id.map(ToString::to_string);
        transact(&self.backend, TransactConfig::default(), |txn| {
            let devices: Vec<String> = match &did {
                Some(did) => vec![did.clone()],
                None => {
                    let spec = TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(
                        rid.clone(),
                        uid.clone(),
                    ));
                    let mut devices = Vec::new();
                    for item in self.room_peekers.range(&*txn, spec) {
                        let ((_, _, device), _) = item.map_err(to_kv)?;
                        devices.push(device);
                    }
                    devices
                }
            };
            let mut removed = 0;
            for device in devices {
                let key = (uid.clone(), device.clone(), rid.clone());
                if self.peeks.get(&*txn, &key).map_err(to_kv)?.is_some() {
                    removed += 1;
                }
                self.peeks.delete(txn, &key).map_err(to_kv)?;
                self.room_peekers
                    .delete(txn, &(rid.clone(), uid.clone(), device))
                    .map_err(to_kv)?;
            }
            Ok(removed)
        })
        .map_err(StoreError::Kv)
    }

    async fn list_peeks(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Vec<(ruma::OwnedRoomId, u64)>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(
            user_id.to_string(),
            device_id.to_string(),
        ));
        let mut out = Vec::new();
        for item in self.peeks.range(&snap, spec) {
            let ((_, _, room_id), value) = item.map_err(StoreError::Table)?;
            let feed_seq: u64 = json_decode(&value)?;
            if let Ok(room_id) = ruma::RoomId::parse(room_id.as_str()) {
                out.push((room_id, feed_seq));
            }
        }
        Ok(out)
    }

    async fn room_peekers(
        &self,
        room_id: &RoomId,
    ) -> Result<std::collections::BTreeSet<OwnedUserId>, StoreError> {
        let snap = self.backend.snapshot();
        let spec =
            TypedKeyspace::<B::Keyspace, (String, String, String)>::prefix(&(room_id.to_string(),));
        let mut out = std::collections::BTreeSet::new();
        for item in self.room_peekers.range(&snap, spec) {
            let ((_, user, _), _) = item.map_err(StoreError::Table)?;
            if let Ok(user) = UserId::parse(user.as_str()) {
                out.insert(user);
            }
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

    async fn import_filter(
        &self,
        user_id: &UserId,
        filter_id: &str,
        filter_json: serde_json::Value,
    ) -> Result<(), StoreError> {
        let key = (user_id.to_string(), filter_id.to_owned());
        let value = json_encode(&filter_json)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.filters.put(txn, &key, &value).map_err(to_kv)
        })
        .map_err(StoreError::Kv)
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
        let mut rooms = self.list_directory_public_rooms().await?;
        rooms.retain(|room| room.join_rule_public);
        Ok(rooms)
    }

    async fn list_directory_public_rooms(&self) -> Result<Vec<PublicRoomEntry>, StoreError> {
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
        // The third key part is the kind, with the thread after a space for a threaded
        // receipt: an unthreaded receipt keeps the key rows had before threads were kept.
        let slot = match &receipt.thread_id {
            Some(thread) => format!("{} {thread}", receipt.kind),
            None => receipt.kind.clone(),
        };
        let key = (room_id.to_string(), receipt.user_id.clone(), slot);
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

/// Deletes the entries of a `(pos,)`-keyed stream below `below`, returning how many: at most
/// [`super::STREAM_PRUNE_MAX_PER_CALL`], in transactions of at most
/// [`super::STREAM_PRUNE_BATCH`] deletes each, so a long backlog never makes one transaction
/// the store refuses. Each batch starts after the last key the previous one deleted, so it does
/// not walk back over what was just deleted.
fn prune_stream<B: KvBackend>(
    backend: &B,
    stream: &TypedKeyspace<B::Keyspace, (u64,)>,
    below: u64,
) -> Result<usize, StoreError> {
    let end = std::ops::Bound::Excluded(Bytes::from(hs_tables::key::encode(&(below,))));
    let mut start = std::ops::Bound::Unbounded;
    let mut total = 0;
    while total < super::STREAM_PRUNE_MAX_PER_CALL {
        let batch = super::STREAM_PRUNE_BATCH.min(super::STREAM_PRUNE_MAX_PER_CALL - total);
        let (deleted, last) = transact(backend, TransactConfig::default(), |txn| {
            let spec = RangeSpec::new(start.clone(), end.clone()).limit(batch);
            let mut keys = Vec::with_capacity(batch);
            for item in stream.range(&*txn, spec) {
                let (k, _v) = item.map_err(to_kv)?;
                keys.push(k);
            }
            let last = keys.last().copied();
            let count = keys.len();
            for k in keys {
                stream.delete(txn, &k).map_err(to_kv)?;
            }
            Ok((count, last))
        })
        .map_err(StoreError::Kv)?;
        total += deleted;
        match last {
            Some(last) if deleted == batch => {
                start = std::ops::Bound::Excluded(Bytes::from(hs_tables::key::encode(&last)));
            }
            _ => break,
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use ruma::{room_id, user_id};

    fn store() -> TablesUserStore<MemoryBackend> {
        TablesUserStore::open(MemoryBackend::new()).unwrap()
    }

    /// Writes stream entries `1..=n` straight into `stream`, in transactions the store accepts.
    fn seed_stream(
        s: &TablesUserStore<MemoryBackend>,
        stream: &TypedKeyspace<<MemoryBackend as KvBackend>::Keyspace, (u64,)>,
        n: u64,
    ) {
        let mut pos = 1;
        while pos <= n {
            let upto = (pos + 4_999).min(n);
            transact(&s.backend, TransactConfig::default(), |txn| {
                for p in pos..=upto {
                    stream.put(txn, &(p,), b"x").map_err(to_kv)?;
                }
                Ok(())
            })
            .unwrap();
            pos = upto + 1;
        }
    }

    /// The positions left in `stream`, oldest first.
    fn stream_positions(
        s: &TablesUserStore<MemoryBackend>,
        stream: &TypedKeyspace<<MemoryBackend as KvBackend>::Keyspace, (u64,)>,
    ) -> Vec<u64> {
        let snap = s.backend.snapshot();
        let spec = RangeSpec::new(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded);
        stream
            .range(&snap, spec)
            .map(|item| item.unwrap().0.0)
            .collect()
    }

    /// More entries below the floor than one transaction may delete (the demo's receipt and
    /// presence streams, 2026-10-14) are pruned across transactions, at most
    /// `STREAM_PRUNE_MAX_PER_CALL` a call, until none is left; nothing at or above the floor is
    /// touched. One transaction for the whole range was refused on every try.
    #[tokio::test]
    async fn a_backlog_over_the_transaction_limit_is_pruned_in_batches() {
        use crate::store::{STREAM_PRUNE_BATCH, STREAM_PRUNE_MAX_PER_CALL};
        const SEEDED: u64 = 12_345;
        const BELOW: u64 = 12_001;
        const { assert!(BELOW as usize - 1 > hs_kv::MAX_TXN_MUTATIONS) };
        const { assert!(STREAM_PRUNE_BATCH < hs_kv::MAX_TXN_MUTATIONS) };
        let s = store();

        seed_stream(&s, &s.receipt_stream, SEEDED);
        assert_eq!(
            s.prune_receipt_stream(BELOW).await.unwrap(),
            STREAM_PRUNE_MAX_PER_CALL,
            "the first call stops at its cap"
        );
        assert_eq!(
            s.prune_receipt_stream(BELOW).await.unwrap(),
            12_000 - STREAM_PRUNE_MAX_PER_CALL
        );
        assert_eq!(s.prune_receipt_stream(BELOW).await.unwrap(), 0);
        let left = stream_positions(&s, &s.receipt_stream);
        assert_eq!(left.len() as u64, SEEDED - BELOW + 1);
        assert_eq!(left.first(), Some(&BELOW));

        seed_stream(&s, &s.presence_stream, SEEDED);
        let mut pruned = 0;
        loop {
            let n = s.prune_presence_stream(u64::MAX).await.unwrap();
            assert!(n <= STREAM_PRUNE_MAX_PER_CALL);
            if n == 0 {
                break;
            }
            pruned += n;
        }
        assert_eq!(pruned as u64, SEEDED);
        assert!(stream_positions(&s, &s.presence_stream).is_empty());
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
            thread_id: None,
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

    #[tokio::test]
    async fn an_imported_filter_is_found_under_the_id_it_came_with() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let body = serde_json::json!({"event_fields": ["type"]});
        s.import_filter(uid, "0", body.clone()).await.unwrap();
        assert_eq!(s.get_filter(uid, "0").await.unwrap(), Some(body));
        // Importing again overwrites, and another user's `0` is their own.
        let newer = serde_json::json!({"event_fields": ["content"]});
        s.import_filter(uid, "0", newer.clone()).await.unwrap();
        assert_eq!(s.get_filter(uid, "0").await.unwrap(), Some(newer));
        assert_eq!(
            s.get_filter(user_id!("@bob:example.org"), "0")
                .await
                .unwrap(),
            None
        );
        // A generated id never looks like an imported one.
        let generated = s.put_filter(uid, serde_json::json!({})).await.unwrap();
        assert!(generated.parse::<u64>().is_err() && generated.len() == 16);
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

    /// The batched fan-out writes exactly what the one-at-a-time calls write -- the same
    /// membership records, feed entries, pointers and heads, coalesced by the same rule -- for
    /// a room of more members than one batch holds, in one transaction per batch.
    #[tokio::test]
    async fn a_batched_fan_out_writes_what_single_calls_would() {
        let batched = store();
        let single = store();
        let room = room_id!("!a:example.org");
        let other = room_id!("!b:example.org");
        let did: &DeviceId = "DEV".into();
        let users: Vec<OwnedUserId> = (0..250)
            .map(|i| {
                UserId::parse(format!("@u{i}:example.org"))
                    .unwrap()
                    .to_owned()
            })
            .collect();
        // The same history on both: an entry per room, every third user synced past them (so
        // `room`'s entry is pinned for them and the next one is a new row), every other user
        // with a record already.
        for s in [&batched, &single] {
            for (i, u) in users.iter().enumerate() {
                s.append_feed_entry(u, room, 1).await.unwrap();
                s.append_feed_entry(u, other, 1).await.unwrap();
                if i % 3 == 0 {
                    s.record_device_cursor(u, did, 2).await.unwrap();
                }
                if i % 2 == 0 {
                    s.set_membership(u, room, "join", 1, false).await.unwrap();
                }
            }
        }
        let writes: Vec<FanOutWrite> = users
            .iter()
            .enumerate()
            .map(|(i, u)| FanOutWrite {
                user_id: u.clone(),
                record: (i % 2 == 1).then(|| ("join".to_owned(), 5)),
                hot_room: false,
                feed_entry: true,
            })
            .collect();
        let report = batched.apply_fan_out(room, 7, &writes, 0).await.unwrap();
        assert_eq!(report.transactions, 3, "250 members in batches of 100");
        assert_eq!(report.fallbacks, 0);
        assert_eq!(report.feed_entries, 250);
        assert_eq!(report.records, 125);
        assert!(report.feeds_to_compact.is_empty(), "no retention asked for");
        for write in &writes {
            if let Some((membership, pos)) = &write.record {
                single
                    .set_membership(&write.user_id, room, membership, *pos, false)
                    .await
                    .unwrap();
            }
            single
                .append_feed_entry(&write.user_id, room, 7)
                .await
                .unwrap();
        }
        for (i, u) in users.iter().enumerate() {
            let (b, s) = (
                batched.feed_since(u, 0).await.unwrap(),
                single.feed_since(u, 0).await.unwrap(),
            );
            assert_eq!(b, s, "{u}'s feed");
            if i % 3 == 0 {
                assert_eq!(b.len(), 3, "{u}: a pinned entry gets a new row");
            } else {
                assert_eq!(b.len(), 2, "{u}: an unpinned entry is coalesced into");
            }
            assert_eq!(
                batched.latest_feed_seq(u).await.unwrap(),
                single.latest_feed_seq(u).await.unwrap()
            );
            assert_eq!(
                batched.current_feed_entry(u, room).await.unwrap(),
                single.current_feed_entry(u, room).await.unwrap()
            );
            assert_eq!(
                batched.list_memberships(u).await.unwrap(),
                single.list_memberships(u).await.unwrap()
            );
            assert_eq!(
                batched.max_device_cursor(u).await.unwrap(),
                single.max_device_cursor(u).await.unwrap()
            );
        }
        // A second fan-out finds the cursor maxima it wrote for the users without one.
        let report = batched.apply_fan_out(room, 8, &writes, 0).await.unwrap();
        assert_eq!(report.feed_entries, 250);
        assert_eq!(
            batched.get_memberships(room, &users).await.unwrap().len(),
            250
        );
        assert_eq!(
            batched
                .get_memberships(room, &users)
                .await
                .unwrap()
                .iter()
                .filter(|m| m.is_some())
                .count(),
            250
        );
    }

    /// Below the floor, compaction keeps each room's newest entry and deletes the rest;
    /// everything above the floor stays; a kept entry is never coalesced into, however far
    /// behind the devices are; and it is idempotent.
    #[tokio::test]
    async fn compaction_keeps_each_rooms_newest_entry_below_the_floor() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let did: &DeviceId = "DEV1".into();
        let (a, b, c) = (
            room_id!("!a:example.org"),
            room_id!("!b:example.org"),
            room_id!("!c:example.org"),
        );
        assert_eq!(s.append_feed_entry(uid, a, 10).await.unwrap(), 1);
        assert_eq!(s.append_feed_entry(uid, b, 20).await.unwrap(), 2);
        s.record_device_cursor(uid, did, 2).await.unwrap();
        assert_eq!(s.append_feed_entry(uid, a, 11).await.unwrap(), 3);
        assert_eq!(s.append_feed_entry(uid, c, 30).await.unwrap(), 4);
        assert_eq!(s.append_feed_entry(uid, b, 21).await.unwrap(), 5);
        assert_eq!(
            s.append_feed_entry(uid, a, 12).await.unwrap(),
            3,
            "coalesced"
        );
        assert_eq!(s.feed_floor(uid).await.unwrap(), 0);

        // Keep one above the floor: the floor is 4. At or below it: a@1, b@2, a@3, c@4.
        assert_eq!(s.compact_feed(uid, 1).await.unwrap(), 1, "a@1 goes");
        assert_eq!(s.feed_floor(uid).await.unwrap(), 4);
        let seqs: Vec<u64> = s
            .feed_since(uid, 0)
            .await
            .unwrap()
            .iter()
            .map(|e| e.feed_seq)
            .collect();
        assert_eq!(seqs, vec![2, 3, 4, 5]);
        assert_eq!(s.latest_feed_seq(uid).await.unwrap(), 5);
        assert_eq!(s.room_pos_as_of(uid, a, 4).await.unwrap(), Some(12));
        assert_eq!(s.room_pos_as_of(uid, a, 2).await.unwrap(), None, "gone");
        assert_eq!(s.room_pos_as_of(uid, b, 2).await.unwrap(), Some(20));
        assert_eq!(s.room_pos_at_token(uid, c, 5).await.unwrap(), Some(30));
        assert_eq!(s.compact_feed(uid, 1).await.unwrap(), 0, "nothing more");
        assert_eq!(s.compact_feed(uid, 0).await.unwrap(), 0, "no retention");

        // c's entry @4 is above the device cursor (2) and would have been coalesced into; it
        // is at the floor, so a new row is written and it stays what it was.
        assert_eq!(s.append_feed_entry(uid, c, 31).await.unwrap(), 6);
        assert_eq!(s.room_pos_as_of(uid, c, 4).await.unwrap(), Some(30));
        // b's @5 is above the floor and the cursor: coalesced as ever.
        assert_eq!(s.append_feed_entry(uid, b, 22).await.unwrap(), 5);

        // The batched fan-out names a feed past twice its retention.
        let writes = vec![FanOutWrite {
            user_id: uid.to_owned(),
            record: None,
            hot_room: false,
            feed_entry: true,
        }];
        s.record_device_cursor(uid, did, 6).await.unwrap();
        let report = s.apply_fan_out(a, 13, &writes, 1).await.unwrap();
        assert_eq!(report.feeds_to_compact, vec![uid.to_owned()], "7 - 4 > 2");
        let report = s.apply_fan_out(a, 14, &writes, 10).await.unwrap();
        assert!(report.feeds_to_compact.is_empty());
    }

    /// The hot-room stream is compacted the same way: per room, the newest entry at or below
    /// the floor stays, so a quiet hot room still has a position as of any newer token.
    #[tokio::test]
    async fn the_hot_room_stream_keeps_each_rooms_newest_entry_below_its_floor() {
        let s = store();
        let (x, y) = (room_id!("!x:example.org"), room_id!("!y:example.org"));
        assert_eq!(s.append_hot_position(x, 10).await.unwrap(), 1);
        assert_eq!(s.append_hot_position(y, 20).await.unwrap(), 2);
        assert_eq!(s.append_hot_position(x, 11).await.unwrap(), 3);
        assert_eq!(s.append_hot_position(x, 12).await.unwrap(), 4);
        assert_eq!(s.append_hot_position(y, 21).await.unwrap(), 5);
        assert_eq!(s.append_hot_position(x, 13).await.unwrap(), 6);

        // Keep two above the floor: the floor is 4. At or below it: x@1, y@2, x@3, x@4.
        assert_eq!(s.compact_hot_stream(2).await.unwrap(), 2, "x@1 and x@3 go");
        assert_eq!(s.hot_room_pos_as_of(x, 4).await.unwrap(), Some(12));
        assert_eq!(s.hot_room_pos_as_of(x, 3).await.unwrap(), None, "gone");
        assert_eq!(s.hot_room_pos_as_of(y, 3).await.unwrap(), Some(20));
        assert_eq!(s.hot_room_pos_as_of(y, 6).await.unwrap(), Some(21));
        assert_eq!(s.latest_hot_seq_of_room(x).await.unwrap(), Some(6));
        assert_eq!(s.latest_hot_seq().await.unwrap(), 6);
        assert_eq!(s.compact_hot_stream(2).await.unwrap(), 0, "nothing more");
        assert_eq!(s.compact_hot_stream(0).await.unwrap(), 0, "no retention");
        // The stream grows on; the next compaction moves the floor and keeps the new newest.
        assert_eq!(s.append_hot_position(y, 22).await.unwrap(), 7);
        assert_eq!(s.append_hot_position(y, 23).await.unwrap(), 8);
        assert_eq!(s.compact_hot_stream(2).await.unwrap(), 2, "x@4 and y@2 go");
        assert_eq!(s.hot_room_pos_as_of(x, 8).await.unwrap(), Some(13));
        assert_eq!(s.hot_room_pos_as_of(y, 6).await.unwrap(), Some(21));
    }

    /// A feed with more entries to delete than one transaction may hold (a single room, a
    /// device that synced after every event) is compacted across transactions: the newest
    /// entry per room at or below the floor stays, everything else there goes, and the floor
    /// moves. One transaction for the window was refused on every fan-out.
    #[tokio::test]
    async fn a_feed_compaction_over_the_transaction_limit_is_done_in_batches() {
        const LATEST: u64 = 25_000;
        const KEEP: u64 = 1_000;
        let s = store();
        let uid = user_id!("@alice:example.org");
        let rooms = ["!a:example.org", "!b:example.org", "!c:example.org"];
        let mut seq = 1;
        while seq <= LATEST {
            let upto = (seq + 4_999).min(LATEST);
            transact(&s.backend, TransactConfig::default(), |txn| {
                for n in seq..=upto {
                    let value = json_encode(&FeedValue {
                        room_id: rooms[(n % 3) as usize].to_owned(),
                        room_pos: i64::try_from(n).unwrap(),
                    })
                    .map_err(to_kv)?;
                    s.feed
                        .put(txn, &(uid.to_string(), n), &value)
                        .map_err(to_kv)?;
                }
                let head = FeedHead {
                    latest: upto,
                    floor: 0,
                };
                s.feed_heads
                    .put(
                        txn,
                        &(uid.to_string(),),
                        &json_encode(&head).map_err(to_kv)?,
                    )
                    .map_err(to_kv)?;
                Ok(())
            })
            .unwrap();
            seq = upto + 1;
        }

        let target = LATEST - KEEP;
        assert_eq!(
            s.compact_feed(uid, KEEP).await.unwrap() as u64,
            target - 3,
            "all but each room's newest at or below the floor"
        );
        assert_eq!(s.feed_floor(uid).await.unwrap(), target);
        let seqs: Vec<u64> = s
            .feed_since(uid, 0)
            .await
            .unwrap()
            .iter()
            .map(|e| e.feed_seq)
            .collect();
        assert_eq!(seqs.len() as u64, 3 + KEEP);
        assert_eq!(&seqs[..4], &[target - 2, target - 1, target, target + 1]);
        for room in rooms {
            let room = RoomId::parse(room).unwrap();
            assert!(
                s.room_pos_as_of(uid, &room, target)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(s.compact_feed(uid, KEEP).await.unwrap(), 0, "nothing more");
    }

    /// The hot-room stream likewise: more rows to delete than one transaction may hold are
    /// deleted across transactions, rooms that straddle a batch included, and the floor moves.
    #[tokio::test]
    async fn a_hot_stream_compaction_over_the_transaction_limit_is_done_in_batches() {
        const ROWS: u64 = 12_000;
        const KEEP: u64 = 500;
        let s = store();
        let rooms = [
            room_id!("!x:example.org"),
            room_id!("!y:example.org"),
            room_id!("!z:example.org"),
        ];
        for n in 1..=ROWS {
            s.append_hot_position(rooms[(n % 3) as usize], i64::try_from(n).unwrap())
                .await
                .unwrap();
        }
        let target = ROWS - KEEP;
        assert_eq!(
            s.compact_hot_stream(KEEP).await.unwrap() as u64,
            target - 3,
            "all but each room's newest at or below the floor"
        );
        for (offset, room) in [
            (0u64, rooms[target as usize % 3]),
            (1, rooms[(target as usize - 1) % 3]),
        ] {
            assert_eq!(
                s.hot_room_pos_as_of(room, target).await.unwrap(),
                Some(i64::try_from(target - offset).unwrap())
            );
        }
        assert_eq!(s.latest_hot_seq().await.unwrap(), ROWS);
        assert_eq!(s.compact_hot_stream(KEEP).await.unwrap(), 0, "nothing more");
    }

    /// The feed head and the cursor maximum are kept beside the rows; a store written before
    /// they existed has neither, and both are recovered from the rows.
    #[tokio::test]
    async fn the_feed_head_and_cursor_maximum_are_recovered_from_the_rows() {
        let s = store();
        let uid = user_id!("@alice:example.org");
        let (a, b) = (room_id!("!a:example.org"), room_id!("!b:example.org"));
        let (d1, d2): (&DeviceId, &DeviceId) = ("D1".into(), "D2".into());
        s.append_feed_entry(uid, a, 1).await.unwrap();
        s.append_feed_entry(uid, b, 1).await.unwrap();
        s.record_device_cursor(uid, d1, 1).await.unwrap();
        s.record_device_cursor(uid, d2, 2).await.unwrap();
        s.record_device_cursor(uid, d1, 1).await.unwrap();
        assert_eq!(s.max_device_cursor(uid).await.unwrap(), 2);

        // As an upgraded store would be: the rows without their summaries.
        let backend = s.backend().clone();
        let heads = backend.keyspace("hs_user.feed_heads").unwrap();
        let maxima = backend.keyspace("hs_user.device_cursor_max").unwrap();
        transact(&backend, TransactConfig::default(), |txn| {
            txn.delete(&heads, &hs_tables::key::encode(&(uid.to_string(),)))?;
            txn.delete(&maxima, &hs_tables::key::encode(&(uid.to_string(),)))
        })
        .unwrap();
        assert_eq!(s.latest_feed_seq(uid).await.unwrap(), 2);
        assert_eq!(s.feed_floor(uid).await.unwrap(), 0);
        assert_eq!(s.max_device_cursor(uid).await.unwrap(), 2);
        // An append continues the sequence and rewrites the head; a cursor rewrites the
        // maximum; both are read from the summaries again afterwards.
        assert_eq!(s.append_feed_entry(uid, a, 2).await.unwrap(), 3);
        s.record_device_cursor(uid, d1, 3).await.unwrap();
        assert_eq!(s.max_device_cursor(uid).await.unwrap(), 3);
        let snap = backend.snapshot();
        assert!(
            snap.get(&heads, &hs_tables::key::encode(&(uid.to_string(),)))
                .unwrap()
                .is_some()
        );
        assert!(
            snap.get(&maxima, &hs_tables::key::encode(&(uid.to_string(),)))
                .unwrap()
                .is_some()
        );
    }
}
