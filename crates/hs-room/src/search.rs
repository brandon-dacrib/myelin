//! Full-text search over room events: the index behind `POST /_matrix/client/v3/search`
//! (`crate::routes::search`), and the background task that keeps it current ([`run_indexer`]).
//! Decision 0021 says why it is shaped this way.
//!
//! # The index
//!
//! An inverted index in one keyspace of the server's own store (`room_search`), so it is as
//! durable as the events it points at, works on every backend, and -- in a cluster, where the
//! store is shared -- is one index for every replica:
//!
//! - `p <term> 0x00 <field> <room_sn> <pos>` -> `<origin_server_ts> <term frequency>`: a posting.
//!   `term` is a lower-cased word ([`tokenize`]); `field` is which of `content.body`,
//!   `content.name` or `content.topic` it was in ([`Field`]); `room_sn` and `pos` name the event
//!   by its room and room-local timeline position. A query term matches every indexed term it is
//!   a prefix of (Synapse's PostgreSQL search does the same), which is one range scan.
//! - `d <room_sn> <pos>` -> the event's postings (JSON), so that a redaction can take them out.
//! - `c <room_sn>` -> the room's cursor: the newest timeline position indexed.
//! - `n` -> how many events the index holds (`hs_room_search_index_documents`).
//!
//! A page of events, its postings and the room's cursor are written in one transaction, so a
//! crash or a restart neither indexes an event twice nor skips one: the indexer resumes from the
//! cursor. An event already present (two replicas indexing one room across a handoff) is not
//! written again.
//!
//! # What is indexed
//!
//! The timeline from position 1 on: every `m.room.message`'s `content.body`, `m.room.name`'s
//! `content.name` and `m.room.topic`'s `content.topic` that is a string -- Synapse's three
//! fields. An `m.room.redaction` removes its target's postings. History fetched from other
//! servers after the fact (negative positions, a rejoin's gap) is not indexed: it is history,
//! never news, and the cursor only moves forward. Words are split on anything that is not a
//! letter or digit; a script written without spaces is one word per run, so it is found by
//! the start of the run only.
//!
//! # Keeping it current
//!
//! The room registry's global stream is a doorbell, as for appservice delivery
//! (`hs_appservice::pump`): an update says which room to read from its cursor, and what is
//! read is decided by the cursor. At start, after a lagged stream, and every
//! [`SWEEP_INTERVAL`] the indexer compares every room's head with its cursor and reads what is
//! behind (the first start indexes everything held this way, in the background, logged and
//! counted in `hs_room_search_rooms_behind`). In a cluster each replica indexes only the rooms
//! whose shard it owns (`RoomRegistry::owns_room`); the sweep picks a room up after a handoff.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hs_kv::{KvBackend, KvRead, KvWrite, RangeSpec, TransactConfig, transact};
use hs_model::canonical::CanonicalJsonValue;
use hs_model::{Event, RoomSn};
use ruma::{OwnedEventId, OwnedRoomId, RoomId};

use crate::error::RoomError;
use crate::persist::Tables;
use crate::registry::RoomRegistry;

/// The keyspace the index lives in.
pub const KEYSPACE: &str = "room_search";

const TAG_POSTING: u8 = b'p';
const TAG_DOC: u8 = b'd';
const TAG_CURSOR: u8 = b'c';
const KEY_DOCUMENTS: &[u8] = b"n";

/// Words longer than this many characters are not indexed (a pasted key, a URL's path).
pub const MAX_TERM_CHARS: usize = 64;
/// At most this many distinct words of one event are indexed; the rest are not.
pub const MAX_TERMS_PER_EVENT: usize = 128;
/// Events read from a room and written to the index per transaction.
pub const INDEX_PAGE: usize = 32;
/// Postings read per query word. A word more common than this matches only the events whose
/// postings come first in the scan (by word, then room); the response's `count` is then a lower
/// bound.
pub const MAX_POSTINGS_PER_TERM: usize = 50_000;
/// How often the indexer compares every room's head with its cursor, besides at start and
/// after missing updates.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Which part of an event a word was found in: one of the spec's three `keys`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    /// `content.body` of an `m.room.message`.
    Body,
    /// `content.name` of an `m.room.name`.
    Name,
    /// `content.topic` of an `m.room.topic`.
    Topic,
}

impl Field {
    /// Every field, in the order the spec lists them.
    pub const ALL: [Field; 3] = [Field::Body, Field::Name, Field::Topic];

    fn byte(self) -> u8 {
        match self {
            Self::Body => b'b',
            Self::Name => b'n',
            Self::Topic => b't',
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'b' => Some(Self::Body),
            b'n' => Some(Self::Name),
            b't' => Some(Self::Topic),
            _ => None,
        }
    }

    /// The field a request's `keys` entry names (`content.body`, `content.name`,
    /// `content.topic`), if it is one of them.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "content.body" => Some(Self::Body),
            "content.name" => Some(Self::Name),
            "content.topic" => Some(Self::Topic),
            _ => None,
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Body => "m.room.message",
            Self::Name => "m.room.name",
            Self::Topic => "m.room.topic",
        }
    }

    fn content_key(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Name => "name",
            Self::Topic => "topic",
        }
    }
}

/// Splits `text` into the words the index holds: runs of letters and digits, lower-cased,
/// at most [`MAX_TERM_CHARS`] characters each (longer runs are dropped). Queries are split the
/// same way.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty() && word.chars().count() <= MAX_TERM_CHARS)
        .map(str::to_lowercase)
        .collect()
}

/// The searchable text of `event`: each field it carries, with the text in it. Empty for an
/// event of any other type, or whose field is missing or not a string (a redacted message).
#[must_use]
pub fn fields_of(event: &Event) -> Vec<(Field, String)> {
    let event_type = event.header().event_type.as_str();
    let Some(content) = event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
    else {
        return Vec::new();
    };
    Field::ALL
        .into_iter()
        .filter(|field| field.event_type() == event_type)
        .filter_map(|field| {
            let text = content
                .get(field.content_key())
                .and_then(CanonicalJsonValue::as_str)?;
            Some((field, text.to_owned()))
        })
        .collect()
}

/// Whether every one of `terms` is a prefix of some word of `event` in one of `fields`: what a
/// hit is checked against when it is read back, so that an index that has not yet seen a
/// redaction (or an edit of the event's stored body) never shows an event that no longer says
/// what was searched for.
#[must_use]
pub fn event_matches(event: &Event, terms: &[String], fields: &[Field]) -> bool {
    let words: Vec<String> = fields_of(event)
        .into_iter()
        .filter(|(field, _)| fields.contains(field))
        .flat_map(|(_, text)| tokenize(&text))
        .collect();
    terms
        .iter()
        .all(|term| words.iter().any(|word| word.starts_with(term.as_str())))
}

/// One timeline event as the indexer hands it to [`SearchIndex::index_page`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDoc {
    /// The event's room-local timeline position.
    pub pos: i64,
    /// Its `origin_server_ts`, kept with each posting for `order_by: recent`.
    pub ts: i64,
    /// Its words: `(field, word) -> how many times`.
    pub terms: BTreeMap<(Field, String), u32>,
    /// For an `m.room.redaction`, the event it redacts, whose postings are removed.
    pub redacts: Option<OwnedEventId>,
}

impl IndexDoc {
    /// What `event`, at timeline position `pos`, contributes to the index.
    #[must_use]
    pub fn of(pos: i64, event: &Event) -> Self {
        let mut terms = BTreeMap::new();
        for (field, text) in fields_of(event) {
            for word in tokenize(&text) {
                if terms.len() >= MAX_TERMS_PER_EVENT && !terms.contains_key(&(field, word.clone()))
                {
                    continue;
                }
                *terms.entry((field, word)).or_insert(0u32) += 1;
            }
        }
        let redacts = (event.header().event_type == "m.room.redaction")
            .then(|| crate::actor::extract_redacts(event))
            .flatten();
        Self {
            pos,
            ts: event.header().origin_server_ts,
            terms,
            redacts,
        }
    }
}

/// One event a query matched, before it is checked against the room.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// The event's room.
    pub room_sn: RoomSn,
    /// The event's room-local timeline position.
    pub pos: i64,
    /// Its `origin_server_ts`.
    pub ts: i64,
    /// Its relevance: for each query word, `(1 + ln tf) * ln(1 + N / df)`, summed (`tf` the
    /// word's occurrences in the event, `df` the events holding it, `N` the events indexed).
    pub score: f64,
}

/// What [`SearchIndex::query`] found.
#[derive(Debug, Clone, Default)]
pub struct Matches {
    /// Every event in the asked rooms holding every query word (as a prefix), in no order.
    pub candidates: Vec<Candidate>,
    /// The indexed words the query words matched in those rooms: the response's `highlights`.
    pub highlights: BTreeSet<String>,
    /// A query word matched more than [`MAX_POSTINGS_PER_TERM`] postings, so some events were
    /// not considered.
    pub truncated: bool,
}

/// The outcome of one [`SearchIndex::index_page`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageOutcome {
    /// Events whose postings were written.
    pub added: u64,
    /// Events whose postings a redaction removed.
    pub removed: u64,
    /// Events the index holds after the page.
    pub documents: i64,
}

fn encode_pos(pos: i64) -> [u8; 8] {
    // Sign bit flipped: big-endian bytes then order as the integers do.
    ((pos as u64) ^ (1 << 63)).to_be_bytes()
}

fn decode_pos(bytes: [u8; 8]) -> i64 {
    (u64::from_be_bytes(bytes) ^ (1 << 63)) as i64
}

fn posting_key(term: &str, field: Field, room_sn: RoomSn, pos: i64) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + term.len() + 1 + 1 + 4 + 8);
    key.push(TAG_POSTING);
    key.extend_from_slice(term.as_bytes());
    key.push(0);
    key.push(field.byte());
    key.extend_from_slice(&room_sn.to_be_bytes());
    key.extend_from_slice(&encode_pos(pos));
    key
}

/// `(term, field, room_sn, pos)` from a posting key; `None` for a key of another shape.
fn parse_posting_key(key: &[u8]) -> Option<(&str, Field, RoomSn, i64)> {
    let rest = key.strip_prefix(&[TAG_POSTING])?;
    let split = rest.iter().position(|b| *b == 0)?;
    let (term, tail) = rest.split_at(split);
    let tail = tail.get(1..)?;
    if tail.len() != 1 + 4 + 8 {
        return None;
    }
    let field = Field::from_byte(tail[0])?;
    let room_sn = RoomSn::from_be_bytes(tail[1..5].try_into().ok()?);
    let pos = decode_pos(tail[5..13].try_into().ok()?);
    Some((std::str::from_utf8(term).ok()?, field, room_sn, pos))
}

fn doc_key(room_sn: RoomSn, pos: i64) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 4 + 8);
    key.push(TAG_DOC);
    key.extend_from_slice(&room_sn.to_be_bytes());
    key.extend_from_slice(&encode_pos(pos));
    key
}

fn cursor_key(room_sn: RoomSn) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 4);
    key.push(TAG_CURSOR);
    key.extend_from_slice(&room_sn.to_be_bytes());
    key
}

fn read_i64<R: KvRead>(
    txn: &R,
    keyspace: &R::Keyspace,
    key: &[u8],
) -> Result<Option<i64>, hs_kv::KvError> {
    Ok(txn
        .get(keyspace, key)?
        .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_ref()).ok())
        .map(i64::from_be_bytes))
}

fn table_error(e: hs_tables::keyspace::TableError) -> hs_kv::KvError {
    match e {
        hs_tables::keyspace::TableError::Kv(kv) => kv,
        other => hs_kv::KvError::backend(other),
    }
}

/// A posting's value: `origin_server_ts` and term frequency.
fn posting_value(ts: i64, tf: u32) -> [u8; 12] {
    let mut value = [0u8; 12];
    value[..8].copy_from_slice(&ts.to_be_bytes());
    value[8..].copy_from_slice(&tf.to_be_bytes());
    value
}

fn parse_posting_value(value: &[u8]) -> Option<(i64, u32)> {
    let ts = i64::from_be_bytes(value.get(..8)?.try_into().ok()?);
    let tf = u32::from_be_bytes(value.get(8..12)?.try_into().ok()?);
    Some((ts, tf))
}

/// The postings a `d` row records: `(field byte, word)`.
type DocRecord = Vec<(u8, String)>;

/// The search index. Cheap to clone. See the module docs.
#[derive(Clone)]
pub struct SearchIndex<B: KvBackend> {
    backend: B,
    keyspace: B::Keyspace,
    tables: Tables<B>,
}

impl<B: KvBackend> SearchIndex<B> {
    /// Opens the index over `backend`, next to the room tables in `tables`.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] if the keyspace cannot be opened.
    pub fn open(backend: B, tables: Tables<B>) -> Result<Self, hs_kv::KvError> {
        let keyspace = backend.keyspace(KEYSPACE)?;
        Ok(Self {
            backend,
            keyspace,
            tables,
        })
    }

    /// The `RoomSn` `room_id` is interned to, if this server holds the room at all.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] on a storage failure.
    pub fn room_sn(&self, room_id: &RoomId) -> Result<Option<RoomSn>, hs_kv::KvError> {
        let snapshot = self.backend.snapshot();
        self.tables.room_sn.lookup(&snapshot, room_id.as_bytes())
    }

    /// The newest timeline position indexed in `room_sn`, `0` when none is.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] on a storage failure.
    pub fn cursor(&self, room_sn: RoomSn) -> Result<i64, hs_kv::KvError> {
        let snapshot = self.backend.snapshot();
        Ok(read_i64(&snapshot, &self.keyspace, &cursor_key(room_sn))?.unwrap_or(0))
    }

    /// How many events the index holds.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] on a storage failure.
    pub fn documents(&self) -> Result<i64, hs_kv::KvError> {
        let snapshot = self.backend.snapshot();
        Ok(read_i64(&snapshot, &self.keyspace, KEY_DOCUMENTS)?
            .unwrap_or(0)
            .max(0))
    }

    /// Writes `docs` (consecutive timeline events of `room_sn`, oldest first) and moves the
    /// room's cursor to `through`, in one transaction. Events at or below the cursor as it
    /// stands are skipped, and so is an event already indexed; a redaction takes its target's
    /// postings out.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] on a storage failure.
    pub fn index_page(
        &self,
        room_sn: RoomSn,
        docs: &[IndexDoc],
        through: i64,
    ) -> Result<PageOutcome, hs_kv::KvError> {
        let ks = &self.keyspace;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let cursor = read_i64(txn, ks, &cursor_key(room_sn))?.unwrap_or(0);
            let mut outcome = PageOutcome::default();
            if cursor >= through {
                outcome.documents = read_i64(txn, ks, KEY_DOCUMENTS)?.unwrap_or(0);
                return Ok(outcome);
            }
            for doc in docs.iter().filter(|d| d.pos > cursor) {
                if let Some(target) = &doc.redacts
                    && self.remove(txn, room_sn, target)?
                {
                    outcome.removed += 1;
                }
                if doc.terms.is_empty() {
                    continue;
                }
                let dkey = doc_key(room_sn, doc.pos);
                if txn.get(ks, &dkey)?.is_some() {
                    continue;
                }
                let mut record: DocRecord = Vec::with_capacity(doc.terms.len());
                for ((field, term), tf) in &doc.terms {
                    txn.put(
                        ks,
                        &posting_key(term, *field, room_sn, doc.pos),
                        &posting_value(doc.ts, *tf),
                    )?;
                    record.push((field.byte(), term.clone()));
                }
                let value = serde_json::to_vec(&record).map_err(hs_kv::KvError::backend)?;
                txn.put(ks, &dkey, &value)?;
                outcome.added += 1;
            }
            txn.put(ks, &cursor_key(room_sn), &through.to_be_bytes())?;
            let delta = i64::try_from(outcome.added).unwrap_or(i64::MAX)
                - i64::try_from(outcome.removed).unwrap_or(i64::MAX);
            outcome.documents = txn.atomic_add(ks, KEY_DOCUMENTS, delta)?;
            Ok(outcome)
        })
    }

    /// Removes the postings of `target` (an event of `room_sn`), if it is indexed. Whether it
    /// was.
    fn remove(
        &self,
        txn: &mut B::Txn,
        room_sn: RoomSn,
        target: &ruma::EventId,
    ) -> Result<bool, hs_kv::KvError> {
        let Some(event_sn) = self.tables.event_sn.lookup(txn, target.as_bytes())? else {
            return Ok(false);
        };
        let Some(row) = self
            .tables
            .events
            .get(&*txn, &(event_sn,))
            .map_err(table_error)?
        else {
            return Ok(false);
        };
        let Ok(persisted) = serde_json::from_slice::<crate::persist::PersistedEvent>(&row) else {
            return Ok(false);
        };
        let Some(pos) = persisted.room_pos else {
            return Ok(false);
        };
        let dkey = doc_key(room_sn, pos);
        let Some(record) = txn.get(&self.keyspace, &dkey)? else {
            return Ok(false);
        };
        let record: DocRecord = serde_json::from_slice(&record).unwrap_or_default();
        for (field, term) in record {
            if let Some(field) = Field::from_byte(field) {
                txn.delete(&self.keyspace, &posting_key(&term, field, room_sn, pos))?;
            }
        }
        txn.delete(&self.keyspace, &dkey)?;
        Ok(true)
    }

    /// The events of `rooms` holding every one of `terms` (each as a prefix of an indexed word)
    /// in one of `fields`.
    ///
    /// # Errors
    /// Returns [`hs_kv::KvError`] on a storage failure.
    pub fn query(
        &self,
        terms: &[String],
        fields: &[Field],
        rooms: &HashSet<RoomSn>,
    ) -> Result<Matches, hs_kv::KvError> {
        let mut matches = Matches::default();
        if terms.is_empty() || rooms.is_empty() || fields.is_empty() {
            return Ok(matches);
        }
        let snapshot = self.backend.snapshot();
        let documents = read_i64(&snapshot, &self.keyspace, KEY_DOCUMENTS)?
            .unwrap_or(0)
            .max(1) as f64;
        let field_bytes: Vec<u8> = fields.iter().map(|f| f.byte()).collect();

        // Per query word: the events of the asked rooms holding it, with its weight there.
        let mut per_term: Vec<HashMap<(RoomSn, i64), (i64, f64)>> = Vec::new();
        let unique: BTreeSet<&String> = terms.iter().collect();
        for term in unique {
            let mut prefix = vec![TAG_POSTING];
            prefix.extend_from_slice(term.as_bytes());
            let spec = RangeSpec::prefix(prefix).limit(MAX_POSTINGS_PER_TERM + 1);
            let mut hits: HashMap<(RoomSn, i64), (i64, u32)> = HashMap::new();
            let mut df: HashSet<(RoomSn, i64)> = HashSet::new();
            let mut scanned = 0usize;
            for item in snapshot.range(&self.keyspace, spec) {
                let (key, value) = item?;
                scanned += 1;
                if scanned > MAX_POSTINGS_PER_TERM {
                    matches.truncated = true;
                    break;
                }
                let Some((word, field, room_sn, pos)) = parse_posting_key(&key) else {
                    continue;
                };
                if !field_bytes.contains(&field.byte()) {
                    continue;
                }
                df.insert((room_sn, pos));
                if !rooms.contains(&room_sn) {
                    continue;
                }
                let Some((ts, tf)) = parse_posting_value(&value) else {
                    continue;
                };
                matches.highlights.insert(word.to_owned());
                let entry = hits.entry((room_sn, pos)).or_insert((ts, 0));
                entry.1 = entry.1.saturating_add(tf);
            }
            let idf = (1.0 + documents / (df.len().max(1) as f64)).ln();
            per_term.push(
                hits.into_iter()
                    .map(|(at, (ts, tf))| (at, (ts, (1.0 + f64::from(tf).ln()) * idf)))
                    .collect(),
            );
        }

        // Every word must match: intersect, starting from the rarest.
        per_term.sort_by_key(HashMap::len);
        let mut iter = per_term.into_iter();
        let Some(first) = iter.next() else {
            return Ok(matches);
        };
        let rest: Vec<_> = iter.collect();
        for ((room_sn, pos), (ts, weight)) in first {
            let mut score = weight;
            let mut all = true;
            for other in &rest {
                match other.get(&(room_sn, pos)) {
                    Some((_, w)) => score += w,
                    None => {
                        all = false;
                        break;
                    }
                }
            }
            if all {
                matches.candidates.push(Candidate {
                    room_sn,
                    pos,
                    ts,
                    score,
                });
            }
        }
        Ok(matches)
    }
}

/// Reads `room_id` from its cursor to its head into the index, a page at a time. The number of
/// events indexed. What the indexer does for each room it is told about, and what a search does
/// first for each room it reads, so that a message sent a moment ago is found.
///
/// # Errors
/// [`RoomError`] if the room cannot be loaded or the index cannot be written.
pub(crate) async fn index_room<B: KvBackend + 'static>(
    rooms: &RoomRegistry<B>,
    room_id: &RoomId,
) -> Result<u64, RoomError> {
    let index = rooms.search_index().clone();
    let Some(room_sn) = index.room_sn(room_id)? else {
        return Ok(0);
    };
    let handle = rooms.get_or_load(room_id).await?;
    let mut cursor = index.cursor(room_sn)?;
    let mut added = 0u64;
    loop {
        let docs: Vec<IndexDoc> = handle
            .query(move |actor| {
                actor
                    .events_after(cursor, INDEX_PAGE)
                    .into_iter()
                    .map(|(pos, event)| IndexDoc::of(pos, event))
                    .collect()
            })
            .await;
        let Some(through) = docs.last().map(|d| d.pos) else {
            return Ok(added);
        };
        let page_index = index.clone();
        let page_docs = docs.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            page_index.index_page(room_sn, &page_docs, through)
        })
        .await
        .map_err(|e| RoomError::Internal(format!("search indexing task failed: {e}")))??;
        let now_ms = crate::metrics::now_ms();
        for doc in docs.iter().filter(|d| !d.terms.is_empty()) {
            crate::metrics::observe_search_index_delay(now_ms - doc.ts);
        }
        crate::metrics::search_indexed(outcome.added, outcome.documents);
        added += outcome.added;
        cursor = through;
    }
}

/// Compares every room this replica owns with its cursor and indexes what is behind. Logs at
/// `info` when anything was (the first start indexes everything held this way).
async fn catch_up<B: KvBackend + 'static>(rooms: &RoomRegistry<B>, why: &str) {
    let started = Instant::now();
    let heads = match rooms.room_heads() {
        Ok(heads) => heads,
        Err(error) => {
            tracing::warn!(%error, "search index: could not list the rooms to catch up on");
            return;
        }
    };
    let index = rooms.search_index();
    let mut behind: Vec<(OwnedRoomId, i64)> = Vec::new();
    for (room_id, head) in heads {
        if head <= 0 || !rooms.owns_room(&room_id) {
            continue;
        }
        let cursor = match index.room_sn(&room_id) {
            Ok(Some(sn)) => index.cursor(sn).unwrap_or(0),
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, %room_id, "search index: could not read a room's cursor");
                continue;
            }
        };
        if cursor < head {
            behind.push((room_id, head - cursor));
        }
    }
    if behind.is_empty() {
        crate::metrics::set_search_rooms_behind(0);
        return;
    }
    let events_behind: i64 = behind.iter().map(|(_, n)| *n).sum();
    tracing::info!(
        rooms = behind.len(),
        positions_behind = events_behind,
        why,
        "search index: indexing rooms that are behind"
    );
    let mut remaining = behind.len();
    crate::metrics::set_search_rooms_behind(remaining);
    let mut indexed = 0u64;
    for (room_id, _) in behind {
        match index_room(rooms, &room_id).await {
            Ok(n) => indexed += n,
            Err(error) => tracing::warn!(%error, %room_id, "search index: could not index a room"),
        }
        remaining -= 1;
        crate::metrics::set_search_rooms_behind(remaining);
    }
    tracing::info!(
        events = indexed,
        documents = index.documents().unwrap_or(0),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "search index: caught up"
    );
}

/// The search indexer: catches up at start, then indexes each room an update on the global
/// stream names, and sweeps every room every [`SWEEP_INTERVAL`]. Runs until the stream closes
/// (the registry is dropped). `hs-cli` spawns it once per process.
pub async fn run_indexer<B: KvBackend + 'static>(rooms: Arc<RoomRegistry<B>>) {
    // Subscribed before catching up, so that nothing published in between is missed.
    let mut updates = rooms.subscribe_global();
    if let Ok(documents) = rooms.search_index().documents() {
        crate::metrics::search_indexed(0, documents);
    }
    catch_up(&rooms, "start").await;
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    sweep.tick().await;
    loop {
        tokio::select! {
            received = updates.recv() => {
                let mut dirty: BTreeSet<OwnedRoomId> = BTreeSet::new();
                let mut lagged = false;
                match received {
                    Ok(update) => {
                        dirty.insert(update.room_id);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "search index: fell behind the room stream");
                        lagged = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
                // Everything already waiting, as one batch: a burst in one room is one read.
                loop {
                    match updates.try_recv() {
                        Ok(update) => {
                            dirty.insert(update.room_id);
                        }
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                            lagged = true;
                        }
                        Err(_) => break,
                    }
                }
                if lagged {
                    catch_up(&rooms, "lagged").await;
                    continue;
                }
                for room_id in dirty {
                    if !rooms.owns_room(&room_id) {
                        continue;
                    }
                    if let Err(error) = index_room(&rooms, &room_id).await {
                        tracing::warn!(%error, %room_id, "search index: could not index a room");
                    }
                }
            }
            _ = sweep.tick() => catch_up(&rooms, "sweep").await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_lower_cased_runs_of_letters_and_digits() {
        assert_eq!(
            tokenize("Hello, World! It's 2026 -- Ünïcode_ok"),
            ["hello", "world", "it", "s", "2026", "ünïcode", "ok"]
        );
        let long = "x".repeat(MAX_TERM_CHARS + 1);
        assert_eq!(tokenize(&format!("short {long}")), ["short"]);
    }

    #[test]
    fn positions_order_as_integers_do() {
        let mut positions = vec![-5i64, 3, 0, -1, 1 << 40, i64::MIN, i64::MAX];
        let mut encoded: Vec<[u8; 8]> = positions.iter().map(|p| encode_pos(*p)).collect();
        encoded.sort();
        positions.sort();
        let decoded: Vec<i64> = encoded.into_iter().map(decode_pos).collect();
        assert_eq!(decoded, positions);
    }

    #[test]
    fn a_posting_key_reads_back() {
        let key = posting_key("hello", Field::Topic, RoomSn::new(7), -3);
        assert_eq!(
            parse_posting_key(&key),
            Some(("hello", Field::Topic, RoomSn::new(7), -3))
        );
    }

    fn doc(pos: i64, words: &[&str]) -> IndexDoc {
        let mut terms = BTreeMap::new();
        for w in words {
            *terms.entry((Field::Body, (*w).to_owned())).or_insert(0) += 1;
        }
        IndexDoc {
            pos,
            ts: pos * 1000,
            terms,
            redacts: None,
        }
    }

    /// A page written twice (a retry, two replicas across a handoff) counts once, and a page
    /// behind the cursor is skipped; every word must match, as a prefix.
    #[test]
    fn pages_are_idempotent_and_queries_intersect_prefixes() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let index = SearchIndex::open(backend, tables).unwrap();
        let room = RoomSn::new(3);
        let page = [doc(1, &["apple", "pie"]), doc(2, &["apple", "tart"])];
        assert_eq!(index.index_page(room, &page, 2).unwrap().added, 2);
        assert_eq!(index.index_page(room, &page, 2).unwrap().added, 0);
        assert_eq!(index.index_page(room, &page[..1], 1).unwrap().added, 0);
        assert_eq!(index.documents().unwrap(), 2);
        assert_eq!(index.cursor(room).unwrap(), 2);

        let rooms = HashSet::from([room]);
        let q = |terms: &[&str]| {
            let terms: Vec<String> = terms.iter().map(|t| (*t).to_owned()).collect();
            let mut found: Vec<i64> = index
                .query(&terms, &Field::ALL, &rooms)
                .unwrap()
                .candidates
                .iter()
                .map(|c| c.pos)
                .collect();
            found.sort_unstable();
            found
        };
        assert_eq!(q(&["app"]), [1, 2]);
        assert_eq!(q(&["apple", "ta"]), [2]);
        assert_eq!(q(&["apple", "cake"]), Vec::<i64>::new());
        let elsewhere = HashSet::from([RoomSn::new(4)]);
        let terms = vec!["apple".to_owned()];
        assert!(
            index
                .query(&terms, &Field::ALL, &elsewhere)
                .unwrap()
                .candidates
                .is_empty()
        );
        assert!(
            index
                .query(&terms, &[Field::Topic], &rooms)
                .unwrap()
                .candidates
                .is_empty()
        );
    }
}
