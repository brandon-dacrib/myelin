//! Notification emails waiting to be sent: what the worker holds for an address until its email
//! is due (`super`'s module docs), stored so a restart sends them rather than losing them.
//!
//! Each replica holds the mail for the notifications its own pipeline evaluated, so the rows are
//! keyed by the holder (the replica's identity, or one fixed name for a single node) as well as
//! by user and address: a replica restores only what it held, and two replicas holding mail for
//! one address do not overwrite each other. A mail held by a replica that never comes back under
//! the same identity is not sent; the next notification starts another.
//!
//! Delivery is at least once: a row is removed right after its email is accepted by the SMTP
//! server, so a stop in between sends that email again after the restart.

use std::collections::BTreeMap;

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::{OwnedRoomId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};

use super::template::NotificationLine;
use crate::error::StoreError;

/// One room's part of a held email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldRoom {
    /// The room's name, if a notification carried one.
    pub name: Option<String>,
    /// The latest notifications in the room, oldest first, at most the email's per-room limit.
    pub lines: Vec<NotificationLine>,
}

/// An email held for one address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldMail {
    /// When it is due, in milliseconds since the epoch (wall time, so it means the same after a
    /// restart).
    pub due_ms: u64,
    /// How many times sending it has failed.
    pub attempts: u32,
    /// What it will say, by room.
    pub rooms: BTreeMap<OwnedRoomId, HeldRoom>,
}

/// Persistence for [`HeldMail`], keyed by `(holder, user, address)`. The worker writes every
/// change through and reads its holder's rows back once, when it starts.
#[async_trait::async_trait]
pub trait HeldMailStore: Send + Sync {
    /// Stores (or replaces) the mail `holder` holds for `(user_id, address)`.
    async fn put(
        &self,
        holder: &str,
        user_id: &UserId,
        address: &str,
        mail: &HeldMail,
    ) -> Result<(), StoreError>;

    /// Forgets the mail `holder` holds for `(user_id, address)`: it was sent, given up, or
    /// everything in it was read. Forgetting nothing is not an error.
    async fn remove(&self, holder: &str, user_id: &UserId, address: &str)
    -> Result<(), StoreError>;

    /// Every mail `holder` holds.
    async fn held_by(
        &self,
        holder: &str,
    ) -> Result<Vec<(OwnedUserId, String, HeldMail)>, StoreError>;
}

type Key = (String, String, String);

fn key(holder: &str, user_id: &UserId, address: &str) -> Key {
    (
        holder.to_owned(),
        user_id.to_string(),
        address.to_ascii_lowercase(),
    )
}

/// An in-memory [`HeldMailStore`]: what a store-less worker uses, and tests.
#[derive(Debug, Default)]
pub struct InMemoryHeldMailStore {
    rows: std::sync::RwLock<BTreeMap<Key, HeldMail>>,
}

impl InMemoryHeldMailStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl HeldMailStore for InMemoryHeldMailStore {
    async fn put(
        &self,
        holder: &str,
        user_id: &UserId,
        address: &str,
        mail: &HeldMail,
    ) -> Result<(), StoreError> {
        self.rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key(holder, user_id, address), mail.clone());
        Ok(())
    }

    async fn remove(
        &self,
        holder: &str,
        user_id: &UserId,
        address: &str,
    ) -> Result<(), StoreError> {
        self.rows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key(holder, user_id, address));
        Ok(())
    }

    async fn held_by(
        &self,
        holder: &str,
    ) -> Result<Vec<(OwnedUserId, String, HeldMail)>, StoreError> {
        let rows = self
            .rows
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rows.iter()
            .filter(|((h, _, _), _)| h == holder)
            .map(|((_, user, address), mail)| {
                let user = UserId::parse(user.as_str())
                    .map_err(|e| StoreError::Backend(format!("held mail user id: {e}")))?;
                Ok((user, address.clone(), mail.clone()))
            })
            .collect()
    }
}

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.email_held` | `(holder, user_id, address)` | the [`HeldMail`], JSON-encoded |
pub struct TablesHeldMailStore<B: KvBackend> {
    backend: B,
    rows: TypedKeyspace<B::Keyspace, Key>,
}

impl<B: KvBackend> TablesHeldMailStore<B> {
    /// Opens (creating if necessary) this store's keyspace.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let rows = TypedKeyspace::new(
            backend
                .keyspace("hs_push.email_held")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        Ok(Self { backend, rows })
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> HeldMailStore for TablesHeldMailStore<B> {
    async fn put(
        &self,
        holder: &str,
        user_id: &UserId,
        address: &str,
        mail: &HeldMail,
    ) -> Result<(), StoreError> {
        let k = key(holder, user_id, address);
        let value = serde_json::to_vec(mail)
            .map_err(|e| StoreError::Backend(format!("encode held mail: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.rows
                .put(txn, &k, &value)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }

    async fn remove(
        &self,
        holder: &str,
        user_id: &UserId,
        address: &str,
    ) -> Result<(), StoreError> {
        let k = key(holder, user_id, address);
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.rows.delete(txn, &k).map_err(hs_kv::KvError::backend)
        })
        .map_err(StoreError::from)
    }

    async fn held_by(
        &self,
        holder: &str,
    ) -> Result<Vec<(OwnedUserId, String, HeldMail)>, StoreError> {
        let snap = self.backend.snapshot();
        let spec = TypedKeyspace::<B::Keyspace, Key>::prefix(&(holder.to_owned(),));
        let mut out = Vec::new();
        for item in self.rows.range(&snap, spec) {
            let ((_, user, address), bytes) = item?;
            let user = UserId::parse(user.as_str())
                .map_err(|e| StoreError::Backend(format!("held mail user id: {e}")))?;
            let mail: HeldMail = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Backend(format!("decode held mail: {e}")))?;
            out.push((user, address, mail));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::template::LineText;
    use hs_kv::memory::MemoryBackend;

    fn mail(due_ms: u64) -> HeldMail {
        let mut rooms = BTreeMap::new();
        rooms.insert(
            ruma::owned_room_id!("!room:example.org"),
            HeldRoom {
                name: Some("Lunch".to_owned()),
                lines: vec![
                    NotificationLine {
                        sender: "Bob".to_owned(),
                        ts_ms: 1_000,
                        text: LineText::Snippet("hello".to_owned()),
                        pos: Some(7),
                        thread: Some(ruma::owned_event_id!("$root:example.org")),
                    },
                    NotificationLine {
                        sender: "Bob".to_owned(),
                        ts_ms: 1_001,
                        text: LineText::Encrypted,
                        pos: None,
                        thread: None,
                    },
                    NotificationLine {
                        sender: "Bob".to_owned(),
                        ts_ms: 1_002,
                        text: LineText::Invite,
                        pos: None,
                        thread: None,
                    },
                    NotificationLine {
                        sender: "Bob".to_owned(),
                        ts_ms: 1_003,
                        text: LineText::Activity("sent a file".to_owned()),
                        pos: None,
                        thread: None,
                    },
                ],
            },
        );
        HeldMail {
            due_ms,
            attempts: 1,
            rooms,
        }
    }

    async fn behaves(store: &dyn HeldMailStore) {
        let alice = ruma::user_id!("@alice:example.org");
        let bob = ruma::user_id!("@bob:example.org");
        assert!(store.held_by("hs-0").await.unwrap().is_empty());
        store.put("hs-0", alice, "A@x.org", &mail(5)).await.unwrap();
        store.put("hs-0", bob, "b@x.org", &mail(6)).await.unwrap();
        store.put("hs-1", alice, "a@x.org", &mail(7)).await.unwrap();
        let mut held = store.held_by("hs-0").await.unwrap();
        held.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(held.len(), 2, "only the holder's own rows");
        assert_eq!(held[0].0, alice);
        assert_eq!(held[0].1, "a@x.org", "addresses are kept lower-cased");
        assert_eq!(held[0].2, mail(5), "every line kind round-trips");
        store.put("hs-0", alice, "a@x.org", &mail(9)).await.unwrap();
        store.remove("hs-0", bob, "b@x.org").await.unwrap();
        store.remove("hs-0", bob, "b@x.org").await.unwrap();
        let held = store.held_by("hs-0").await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].2.due_ms, 9, "a put replaces");
        assert_eq!(store.held_by("hs-1").await.unwrap()[0].2.due_ms, 7);
    }

    #[tokio::test]
    async fn memory_store_keeps_held_mail() {
        behaves(&InMemoryHeldMailStore::new()).await;
    }

    #[tokio::test]
    async fn tables_store_keeps_held_mail() {
        behaves(&TablesHeldMailStore::open(MemoryBackend::new()).unwrap()).await;
    }
}
