//! A durable [`hs_admin::audit::AuditSink`] over `hs-kv`, replacing the in-memory one `hs serve`
//! used to wire.
//!
//! `hs-admin` deliberately depends on no storage crate: it defines the sink as a trait and lets
//! whoever composes the server supply one (the same shape as `hs_admin::sources::UserDirectory`,
//! which `hs-auth` implements). Its own [`hs_admin::audit::InMemoryAuditSink`] says so in its doc
//! comment — "not durable: a process restart loses everything, which is fine for a mock and wrong
//! for production". `hs serve` wired exactly that one, so until now every record of who locked an
//! account or promoted an admin vanished on restart. An audit log that does not survive the event
//! it is meant to explain is not an audit log.
//!
//! # Keyspace and ordering
//!
//! Two keyspaces: `hs_admin.audit`, keyed by `(sequence, entry_id)`, and `hs_admin.audit_by_id`,
//! mapping an entry id to its sequence so a point `get` and a cursor stay one lookup.
//!
//! The sequence exists because entry ids do **not** order reliably. They are ULIDs from
//! `hs_admin::model::AuditEntry::new`, and two ULIDs minted in the same millisecond have no
//! guaranteed order relative to each other — `hs-admin`'s own event bus hit this and fixed it by
//! assigning ids at publish time (see `crates/hs-admin/src/events.rs`). Keying this log by id
//! would have meant "newest first" was only approximately newest first, which for an audit log is
//! not a detail: the order in which an operator did things is most of what the log is for. The
//! first version of this module did key by id, and its own pagination test caught it — a second
//! page started three entries further back than it should have, because insertion order and key
//! order disagreed.
//!
//! The sequence is assigned inside the same transaction as the write, by reading the current
//! highest key, so two concurrent appends cannot take the same number: the loser of the conflict
//! retries and reads the winner's sequence. That is one extra read per append, which an audit
//! log's write volume can afford.
//!
//! # Querying
//!
//! `hs_admin::audit::AuditFilter` already knows how to decide whether an entry matches
//! (`AuditFilter::matches`), so this store scans newest-first and applies that predicate rather
//! than reimplementing the rules, which keeps the in-memory sink and this one answering
//! identically by construction. The scan is bounded two ways: it stops once `filter.limit`
//! matches have been collected, and it never reads more than [`MAX_SCANNED`] rows even if fewer
//! match, so a narrow filter over a large log cannot turn one request into a full table scan.
//! A caller that hits that ceiling gets fewer rows than it asked for; the cursor it already
//! carries lets it continue. Indexes per filterable field are the fix if that ever becomes a real
//! query pattern rather than an operator paging through recent activity.

use std::sync::Arc;

use hs_admin::audit::{AuditError, AuditFilter, AuditSink};
use hs_admin::model::AuditEntry;
use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_tables::key::TupleKey;
use hs_tables::keyspace::TypedKeyspace;
use std::ops::Bound;

/// The most rows one [`AuditSink::query`] will read before giving up, however few of them matched.
/// Chosen so a filtered page over a long log stays a bounded amount of work; see the module doc.
const MAX_SCANNED: usize = 10_000;

/// A durable audit sink over any `hs-kv` backend.
pub struct TablesAuditSink<B: KvBackend> {
    backend: B,
    /// `(sequence, entry_id) -> entry JSON`, in insertion order.
    entries: Arc<TypedKeyspace<B::Keyspace, (u64, String)>>,
    /// `entry_id -> sequence`, so a point read or a cursor does not scan.
    by_id: Arc<TypedKeyspace<B::Keyspace, (String,)>>,
}

impl<B: KvBackend> TablesAuditSink<B> {
    /// Opens (or creates) the two audit keyspaces on `backend`.
    ///
    /// # Errors
    /// Returns the backend's own error if either keyspace cannot be opened.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        let entries = backend.keyspace("hs_admin.audit")?;
        let by_id = backend.keyspace("hs_admin.audit_by_id")?;
        Ok(Self {
            backend,
            entries: Arc::new(TypedKeyspace::new(entries)),
            by_id: Arc::new(TypedKeyspace::new(by_id)),
        })
    }

    /// The highest sequence stored, or `None` for an empty log. Read inside the caller's
    /// transaction so the value cannot go stale between reading it and writing the next one.
    fn highest_sequence<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        read: &R,
    ) -> Result<Option<u64>, hs_kv::KvError> {
        let spec = RangeSpec {
            start: Bound::Unbounded,
            end: Bound::Unbounded,
            reverse: true,
            limit: Some(1),
        };
        match self.entries.range(read, spec).next() {
            Some(item) => {
                // `TypedKeyspace::range` yields the key already decoded into its tuple type.
                let ((seq, _id), _value) = item.map_err(hs_kv::KvError::backend)?;
                Ok(Some(seq))
            }
            None => Ok(None),
        }
    }
}

impl<B: KvBackend> TablesAuditSink<B> {
    /// The sequence an entry id was stored under, if this log holds it.
    fn sequence_of<R: hs_kv::KvRead<Keyspace = B::Keyspace>>(
        &self,
        read: &R,
        id: &str,
    ) -> Result<Option<u64>, AuditError> {
        let Some(bytes) = self
            .by_id
            .get(read, &(id.to_owned(),))
            .map_err(unavailable)?
        else {
            return Ok(None);
        };
        let array: [u8; 8] = bytes
            .as_ref()
            .try_into()
            .map_err(|_| unavailable("audit index value is not an 8-byte sequence"))?;
        Ok(Some(u64::from_be_bytes(array)))
    }
}

fn decode(bytes: &[u8]) -> Result<AuditEntry, AuditError> {
    serde_json::from_slice(bytes).map_err(|e| unavailable(format!("decode: {e}")))
}

/// Every failure here is reported as [`AuditError::Unavailable`], which `hs-admin` turns into a
/// `503` that fails the mutation it was recording — deliberately, per RFC 0004 section 9: an
/// action whose audit entry could not be written must not be reported as having succeeded.
fn unavailable(e: impl std::fmt::Display) -> AuditError {
    AuditError::Unavailable(e.to_string())
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> AuditSink for TablesAuditSink<B> {
    async fn append(&self, entry: AuditEntry) -> Result<(), AuditError> {
        let id = entry.id.clone();
        let value = serde_json::to_vec(&entry).map_err(|e| unavailable(format!("encode: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let next = self.highest_sequence(txn)?.map_or(0, |seq| seq + 1);
            self.entries
                .put(txn, &(next, id.clone()), &value)
                .map_err(hs_kv::KvError::backend)?;
            self.by_id
                .put(txn, &(id.clone(),), &next.to_be_bytes())
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(unavailable)
    }

    async fn get(&self, id: &str) -> Result<Option<AuditEntry>, AuditError> {
        let snap = self.backend.snapshot();
        let Some(seq) = self.sequence_of(&snap, id)? else {
            return Ok(None);
        };
        let bytes = self
            .entries
            .get(&snap, &(seq, id.to_owned()))
            .map_err(unavailable)?;
        bytes.as_deref().map(decode).transpose()
    }

    async fn query(&self, filter: &AuditFilter) -> Result<Vec<AuditEntry>, AuditError> {
        let snap = self.backend.snapshot();

        // A cursor names the last entry the caller already saw. Resolve it to its sequence and
        // scan strictly below that, newest-first. A cursor naming an entry this log does not have
        // (an id from another server, or one since pruned) starts from the newest rather than
        // silently returning nothing.
        let end = match &filter.cursor {
            Some(cursor) => match self.sequence_of(&snap, cursor)? {
                Some(seq) => Bound::Excluded(bytes::Bytes::from((seq, cursor.clone()).encode())),
                None => Bound::Unbounded,
            },
            None => Bound::Unbounded,
        };
        let spec = RangeSpec {
            start: Bound::Unbounded,
            end,
            reverse: true,
            limit: Some(MAX_SCANNED),
        };

        let mut matched = Vec::with_capacity(filter.limit.min(256));
        for item in self.entries.range(&snap, spec) {
            let (_key, value) = item.map_err(unavailable)?;
            let entry = decode(&value)?;
            if filter.matches(&entry) {
                matched.push(entry);
                if matched.len() >= filter.limit {
                    break;
                }
            }
        }
        Ok(matched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_admin::model::{Actor, ActorKind, AuditOutcome, ResourceRef};
    use hs_kv::memory::MemoryBackend;

    fn sink() -> TablesAuditSink<MemoryBackend> {
        TablesAuditSink::open(MemoryBackend::new()).expect("opening an in-memory keyspace")
    }

    fn actor(id: &str) -> Actor {
        Actor {
            kind: ActorKind::User,
            id: id.to_owned(),
            display_name: None,
            token_id: None,
            ip: None,
            user_agent: None,
        }
    }

    fn entry(action: &str, who: &str, target_id: &str) -> AuditEntry {
        AuditEntry::new(
            action,
            actor(who),
            ResourceRef {
                r#type: "user".to_owned(),
                id: target_id.to_owned(),
            },
            AuditOutcome::success(200),
        )
    }

    fn filter(limit: usize) -> AuditFilter {
        AuditFilter {
            limit,
            ..AuditFilter::default()
        }
    }

    #[tokio::test]
    async fn an_appended_entry_comes_back_by_id() {
        let sink = sink();
        let e = entry("users.lock", "@ops:example.org", "@bad:example.org");
        let id = e.id.clone();
        sink.append(e).await.unwrap();

        let found = sink.get(&id).await.unwrap().expect("the entry is stored");
        assert_eq!(found.id, id);
        assert_eq!(found.action, "users.lock");
        assert_eq!(found.actor.id, "@ops:example.org");
        assert!(
            sink.get("01JUNKJUNKJUNKJUNKJUNKJUNK")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_reopened_backend_still_has_the_entries() {
        // The point of this sink: the same backend handle reopened (as a restart would) still
        // answers. `MemoryBackend` persists for as long as the backend value lives, so this
        // checks the keyspace is addressed by name rather than held in the sink's own memory.
        let backend = MemoryBackend::new();
        let first = TablesAuditSink::open(backend.clone()).unwrap();
        let e = entry("users.deactivate", "@ops:example.org", "@gone:example.org");
        let id = e.id.clone();
        first.append(e).await.unwrap();
        drop(first);

        let second = TablesAuditSink::open(backend).unwrap();
        assert_eq!(
            second.get(&id).await.unwrap().map(|e| e.action).as_deref(),
            Some("users.deactivate")
        );
    }

    #[tokio::test]
    async fn query_returns_newest_first() {
        let sink = sink();
        for n in 0..5 {
            sink.append(entry(
                &format!("action.{n}"),
                "@ops:example.org",
                "@t:example.org",
            ))
            .await
            .unwrap();
        }
        let page = sink.query(&filter(10)).await.unwrap();
        let actions: Vec<&str> = page.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(
            actions,
            vec!["action.4", "action.3", "action.2", "action.1", "action.0"]
        );
    }

    #[tokio::test]
    async fn query_honours_the_limit_and_the_cursor() {
        let sink = sink();
        for n in 0..6 {
            sink.append(entry(
                &format!("action.{n}"),
                "@ops:example.org",
                "@t:example.org",
            ))
            .await
            .unwrap();
        }

        let first_page = sink.query(&filter(2)).await.unwrap();
        assert_eq!(first_page.len(), 2);
        assert_eq!(first_page[0].action, "action.5");

        let next = AuditFilter {
            cursor: Some(first_page[1].id.clone()),
            ..filter(2)
        };
        let second_page = sink.query(&next).await.unwrap();
        assert_eq!(second_page.len(), 2);
        assert_eq!(second_page[0].action, "action.3");
        // The cursor is exclusive: the entry it names must not repeat across pages.
        assert!(
            second_page.iter().all(|e| e.id != first_page[1].id),
            "the cursor entry was served twice"
        );
    }

    #[tokio::test]
    async fn query_applies_the_filters_it_is_given() {
        let sink = sink();
        sink.append(entry("users.lock", "@ops:example.org", "@a:example.org"))
            .await
            .unwrap();
        sink.append(entry("users.lock", "@other:example.org", "@b:example.org"))
            .await
            .unwrap();
        sink.append(entry(
            "users.deactivate",
            "@ops:example.org",
            "@c:example.org",
        ))
        .await
        .unwrap();

        let by_actor = AuditFilter {
            actor: Some("@ops:example.org".to_owned()),
            ..filter(10)
        };
        assert_eq!(sink.query(&by_actor).await.unwrap().len(), 2);

        let by_action = AuditFilter {
            action: Some("users.lock".to_owned()),
            ..filter(10)
        };
        assert_eq!(sink.query(&by_action).await.unwrap().len(), 2);

        let both = AuditFilter {
            actor: Some("@ops:example.org".to_owned()),
            action: Some("users.lock".to_owned()),
            ..filter(10)
        };
        let matched = sink.query(&both).await.unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].target.id, "@a:example.org");
    }
}
