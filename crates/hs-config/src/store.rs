//! The configuration store: the homeserver's settings, kept in its own database rather than in a
//! file an operator has to find, edit and redeploy.
//!
//! This is the layer the admin API and the management web interface write through, and the reason
//! [`Origin::Database`](crate::document::Origin::Database) outranks the bootstrap file: a setting
//! changed in the web interface must not be silently undone on the next restart by a
//! `homeserver.yaml` nobody remembers is mounted.
//!
//! # What cannot live here
//!
//! The bootstrap settings ([`crate::bootstrap`], decision 0010): where the database is, what this
//! process listens on, this replica's identity in the cluster, paths on this process's own
//! filesystem, the server's name, and the registration files imported once into the appservice
//! registry. They come from the command line, an `HS__` variable or the bootstrap file. Seeding
//! skips them, writing one is refused with an error that says so rather than accepted and
//! ignored, and [`ConfigStore::purge_bootstrap`] removes any an earlier version stored.
//! [`BOOTSTRAP_SECTIONS`] are the sections that are bootstrap as a whole.
//!
//! The server's name is the one bootstrap value the store does remember, as the database's
//! *identity* ([`ConfigMeta::server_name`]) rather than as a setting: a second start with only a
//! data directory must still know who it is, and a start that declares a different name must be
//! told it cannot have it.
//!
//! # Layout
//!
//! One `hs-kv` keyspace, [`KEYSPACE`], with four kinds of key:
//!
//! - `section/<name>` — that section's sparse document, as JSON. Absent means the section sets
//!   nothing and everything in it falls through to the file or the schema default.
//! - `meta` — [`ConfigMeta`]: the revision counter, who last wrote and when.
//! - `history/<revision>` — [`ChangeRecord`], one per write, so the web interface can show what
//!   changed and when without a separate audit store. Since the per-setting history each record
//!   also carries what the database held at every setting it touched ([`ChangeRecord::before`]),
//!   which is what [`ConfigStore::revert_plan`] undoes it with. That includes a secret's earlier
//!   value: restoring a rotated secret is what a revert is for, and it never leaves the server --
//!   the admin API redacts history exactly as it redacts values.
//! - `import/<kind>/<key>` — [`ImportRecord`], one per bootstrap-file item imported into the
//!   database once (an appservice registration file, today), so that a file still listed after
//!   the import is not imported again over what an operator has since changed.
//!
//! Keys are fixed ASCII with a zero-padded decimal revision, so `history/` scans in revision
//! order. There is no tuple encoding and no index to maintain, which is why this does not go
//! through `hs-tables`.

use std::collections::BTreeMap;
use std::ops::Bound;

use hs_kv::{KvBackend, KvError, KvRead, KvWrite, RangeSpec, TransactConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::bootstrap::{self, strip_bootstrap};
use crate::document::merge_patch;
use crate::reload::SECTION_NAMES;

/// The `hs-kv` keyspace this store owns.
pub const KEYSPACE: &str = "config";

/// Sections that are bootstrap as a whole ([`crate::bootstrap`]), and so cannot be stored at all.
/// Other sections hold some bootstrap settings among administered ones; see
/// [`crate::bootstrap::BOOTSTRAP_SETTINGS`] for the full list.
pub const BOOTSTRAP_SECTIONS: &[&str] = &["storage", "listeners"];

/// True when the whole of `section` must come from the bootstrap layer rather than the database.
#[must_use]
pub fn is_bootstrap_section(section: &str) -> bool {
    BOOTSTRAP_SECTIONS.contains(&section)
}

/// The actor recorded against the history entries [`ConfigStore::purge_bootstrap`] writes.
pub const PURGE_ACTOR: &str = "system: bootstrap settings are not stored (decision 0010)";

const META_KEY: &[u8] = b"meta";
const SECTION_PREFIX: &str = "section/";
const HISTORY_PREFIX: &str = "history/";
const IMPORT_PREFIX: &str = "import/";

fn section_key(name: &str) -> Vec<u8> {
    format!("{SECTION_PREFIX}{name}").into_bytes()
}

fn import_key(kind: &str, key: &str) -> Vec<u8> {
    format!("{IMPORT_PREFIX}{kind}/{key}").into_bytes()
}

/// One past every `history/<revision>` key: the exclusive upper bound of a history scan.
fn history_upper_bound() -> Vec<u8> {
    // `0` is the successor of `/` in ASCII, so `history0` sorts after every `history/...`.
    b"history0".to_vec()
}

fn history_key(revision: u64) -> Vec<u8> {
    // Zero-padded so the lexicographic key order `hs-kv` guarantees is also revision order.
    format!("{HISTORY_PREFIX}{revision:020}").into_bytes()
}

/// Why a configuration store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The storage backend failed.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// A stored value is not the JSON this module wrote. Only reachable if something else wrote
    /// to the keyspace, or a write was truncated.
    #[error("the stored configuration at {key} is unreadable: {detail}")]
    Corrupt {
        /// The key whose value could not be parsed.
        key: String,
        /// The parse error.
        detail: String,
    },
    /// An `If-Match` revision did not match what is stored: somebody else changed the
    /// configuration in between, and the caller's patch was computed against a stale view.
    #[error(
        "the configuration has changed since revision {expected} (it is now at {actual}); \
         re-read it and apply the change again"
    )]
    RevisionMismatch {
        /// What the caller expected to be current.
        expected: u64,
        /// What is actually current.
        actual: u64,
    },
    /// No such top-level section.
    #[error("{section:?} is not a configuration section (expected one of {SECTION_NAMES:?})")]
    UnknownSection {
        /// The name the caller asked for.
        section: String,
    },
    /// A section that cannot be stored in the database was written to it.
    #[error(
        "{section:?} is a bootstrap section: it is read before the database is open, or belongs \
         to one process rather than to the whole server, so it cannot be stored in the database \
         -- set it on the command line, in an HS__ environment variable, or in the bootstrap file"
    )]
    BootstrapSection {
        /// The section the caller tried to write.
        section: String,
    },
    /// A patch would write one or more bootstrap settings inside an otherwise administered
    /// section (`cluster.mesh.port`, `server.server_name`, ...).
    #[error(
        "{} cannot be stored in the database: it {} -- set it on the command line, in an HS__ \
         environment variable, or in the bootstrap file",
        pointers.join(", "),
        explanation
    )]
    BootstrapSetting {
        /// The whole-configuration JSON Pointers the patch would have written.
        pointers: Vec<String>,
        /// Why the first of them is bootstrap ([`crate::bootstrap::BootstrapReason`]).
        explanation: &'static str,
    },
    /// No change with this revision was recorded against this section.
    #[error("no change to {section:?} was recorded at revision {revision}")]
    NoSuchChange {
        /// The section asked about.
        section: String,
        /// The revision asked for.
        revision: u64,
    },
    /// The change exists but cannot be undone automatically.
    #[error("revision {revision} cannot be reverted: {reason}")]
    NotRevertible {
        /// The revision asked for.
        revision: u64,
        /// Why not.
        reason: String,
    },
}

impl StoreError {
    /// The error for a patch against `section` that touches the bootstrap settings `pointers`.
    #[must_use]
    pub fn bootstrap(section: &str, pointers: Vec<String>) -> Self {
        if is_bootstrap_section(section) {
            return StoreError::BootstrapSection {
                section: section.to_owned(),
            };
        }
        let explanation = pointers
            .first()
            .and_then(|p| bootstrap::bootstrap_setting(p))
            .map_or("is a bootstrap setting", |s| s.reason.explanation());
        StoreError::BootstrapSetting {
            pointers,
            explanation,
        }
    }
}

/// Bookkeeping about the stored configuration as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigMeta {
    /// Increments on every write. Used as an optimistic-concurrency token by the admin API's
    /// `If-Match`, so two operators editing at once cannot silently overwrite each other.
    pub revision: u64,
    /// When the last write happened, in milliseconds since the Unix epoch. Zero when nothing has
    /// been written.
    pub updated_at_ms: i64,
    /// Who made the last write, as the admin API's principal. `None` for a seed.
    pub updated_by: Option<String>,
    /// Where the initial contents came from, if this store was seeded from a file.
    pub seeded_from: Option<String>,
    /// The server name this database was created for: its identity, not a setting. Recorded by
    /// the first seed (or, for a store written before decision 0010, by
    /// [`ConfigStore::purge_bootstrap`] from the `server.server_name` it used to hold) and never
    /// changed after, because every identifier the server has issued carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

/// One bootstrap-file item imported into the database once (`import/<kind>/<key>`).
///
/// The record is what makes an import one-time: a file still listed in the bootstrap layer after
/// it has been imported is skipped on every later start, so a change made through the admin API
/// to what it imported is not overwritten by the file on the next restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImportRecord {
    /// What kind of thing was imported (`appservice_registration`).
    pub kind: String,
    /// What identifies the source within its kind (a registration file's path).
    pub key: String,
    /// What it became (an appservice id), or why nothing was imported.
    pub outcome: String,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: i64,
}

/// One configuration change, kept so the web interface can show a history without a separate
/// audit store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeRecord {
    /// The revision this change produced.
    pub revision: u64,
    /// The section it touched.
    pub section: String,
    /// The merge patch that was applied (`null` values are removals -- see
    /// [`crate::document::merge_patch`]).
    pub patch: Value,
    /// Who applied it.
    pub actor: Option<String>,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: i64,
    /// What the database held at each setting the patch touched, just before it was applied
    /// ([`crate::history::before_values`]): section-relative JSON Pointer to value, `null` where
    /// it held nothing. `None` for a record written before this was kept, which can be shown
    /// but not reverted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<BTreeMap<String, Value>>,
    /// The revision this change undid, when it was a revert ([`ConfigStore::apply_revert`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverts: Option<u64>,
}

/// A later change to the same settings as the one being reverted: reverting over it would undo
/// it too, so [`ConfigStore::revert_plan`] reports it and the caller decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaterChange {
    /// The later change's revision.
    pub revision: u64,
    /// Who made it.
    pub actor: Option<String>,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: i64,
    /// The section-relative pointers it wrote that overlap the change being reverted.
    pub pointers: Vec<String>,
}

/// How to undo one recorded change, computed against what is stored now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertPlan {
    /// The change being undone.
    pub record: ChangeRecord,
    /// The merge patch that undoes it against the section as it is stored now. An empty object
    /// when the settings already hold their earlier values.
    pub patch: Value,
    /// Later changes to the same settings, oldest first. Empty means the revert undoes only this
    /// change.
    pub conflicts: Vec<LaterChange>,
    /// The store's revision this plan was computed at. [`ConfigStore::apply_revert`] refuses to
    /// write it over any other.
    pub revision: u64,
}

/// One page of history, newest first ([`ConfigStore::history_page`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    /// The changes on this page, newest first.
    pub records: Vec<ChangeRecord>,
    /// Pass as `before` for the next, older page; `None` on the oldest page.
    pub older: Option<u64>,
    /// Pass as `before` for the previous, newer page; `None` on the newest page.
    pub newer: Option<u64>,
}

/// The stored configuration: every section merged into one sparse document, plus its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// The merged document -- the [`Origin::Database`](crate::document::Origin::Database) layer.
    pub document: Value,
    /// Revision, last writer, seed provenance.
    pub meta: ConfigMeta,
}

impl Stored {
    /// An empty store: no sections, revision zero.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            document: Value::Object(Map::new()),
            meta: ConfigMeta::default(),
        }
    }

    /// True when no section has ever been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.document
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
    }
}

/// The homeserver's settings, in the homeserver's own database.
///
/// Cheap to clone: it holds an `hs-kv` backend handle (itself an `Arc`) and a keyspace handle.
#[derive(Debug, Clone)]
pub struct ConfigStore<B: KvBackend> {
    backend: B,
    keyspace: B::Keyspace,
}

impl<B: KvBackend> ConfigStore<B> {
    /// Opens (creating if necessary) the configuration keyspace on `backend`.
    ///
    /// # Errors
    /// Returns [`StoreError::Kv`] if the keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let keyspace = backend.keyspace(KEYSPACE)?;
        Ok(Self { backend, keyspace })
    }

    /// Reads every section and the metadata, merged into one document.
    ///
    /// # Errors
    /// Returns [`StoreError::Kv`] on a backend failure or [`StoreError::Corrupt`] if a stored
    /// value is not the JSON this module writes.
    pub fn load(&self) -> Result<Stored, StoreError> {
        let snapshot = self.backend.snapshot();
        let mut document = Value::Object(Map::new());
        for entry in snapshot.range(
            &self.keyspace,
            RangeSpec::prefix(SECTION_PREFIX.as_bytes().to_vec()),
        ) {
            let (key, value) = entry?;
            let key = String::from_utf8_lossy(&key).into_owned();
            let name = key.strip_prefix(SECTION_PREFIX).unwrap_or(&key).to_owned();
            let section: Value = parse(&key, &value)?;
            document
                .as_object_mut()
                .expect("built as an object")
                .insert(name, section);
        }
        let meta = match snapshot.get(&self.keyspace, META_KEY)? {
            Some(bytes) => parse(&String::from_utf8_lossy(META_KEY), &bytes)?,
            None => ConfigMeta::default(),
        };
        Ok(Stored { document, meta })
    }

    /// The document one section stores, or `None` when it stores nothing.
    ///
    /// # Errors
    /// As [`ConfigStore::load`].
    pub fn section(&self, name: &str) -> Result<Option<Value>, StoreError> {
        check_section_name(name)?;
        let snapshot = self.backend.snapshot();
        match snapshot.get(&self.keyspace, &section_key(name))? {
            Some(bytes) => Ok(Some(parse(name, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Writes `document`'s administered sections as the initial contents, but only if nothing
    /// has been written yet. Returns `true` when it seeded and `false` when the store already
    /// held configuration and was left untouched.
    ///
    /// This is how an existing `homeserver.yaml` deployment moves into the database: the first
    /// boot copies the file in, and from then on the database is what the server reads and the
    /// web interface writes. Re-running it is a no-op, so a file left mounted after the move
    /// cannot quietly revert a change made in the UI.
    ///
    /// The bootstrap settings ([`crate::bootstrap`]) are not copied: they are per process, and a
    /// second replica booting on the same database must keep its own. The server name, if the
    /// document declares one, is recorded as the store's identity ([`ConfigMeta::server_name`]).
    ///
    /// # Errors
    /// Returns [`StoreError::Kv`] on a backend failure.
    pub fn seed(&self, document: &Value, source: &str, now_ms: i64) -> Result<bool, StoreError> {
        let server_name = document
            .pointer("/server/server_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let mut administered = document.clone();
        strip_bootstrap(&mut administered);
        let sections: Vec<(String, Value)> = administered
            .as_object()
            .map(|map| {
                map.iter()
                    .filter(|(name, _)| SECTION_NAMES.contains(&name.as_str()))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let seeded = hs_kv::transact(&self.backend, TransactConfig::default(), |txn| {
            // Read the metadata *inside* the transaction: two processes booting at once must not
            // both decide the store is empty and both seed it.
            let existing = txn.get(&self.keyspace, META_KEY)?;
            if existing.is_some() {
                return Ok(false);
            }
            for (name, value) in &sections {
                txn.put(&self.keyspace, &section_key(name), &to_bytes(value))?;
            }
            let meta = ConfigMeta {
                revision: 1,
                updated_at_ms: now_ms,
                updated_by: None,
                seeded_from: Some(source.to_owned()),
                server_name: server_name.clone(),
            };
            txn.put(&self.keyspace, META_KEY, &to_bytes(&meta))?;
            Ok(true)
        })?;
        Ok(seeded)
    }

    /// Applies `patch` to one section (RFC 7396 merge patch: `null` removes a key, dropping the
    /// setting back to the file or the schema default), bumps the revision and records the change.
    ///
    /// `expected_revision` is the admin API's `If-Match`: when it is `Some` and does not match
    /// what is stored, nothing is written and [`StoreError::RevisionMismatch`] comes back, so two
    /// operators editing the same section at once cannot silently overwrite one another.
    ///
    /// The caller is responsible for validating the *resulting* configuration first -- this layer
    /// knows the shape of the store, not the meaning of a setting.
    ///
    /// # Errors
    /// [`StoreError::UnknownSection`], [`StoreError::BootstrapSection`],
    /// [`StoreError::BootstrapSetting`], [`StoreError::RevisionMismatch`],
    /// [`StoreError::Corrupt`] or [`StoreError::Kv`].
    pub fn patch_section(
        &self,
        section: &str,
        patch: &Value,
        actor: Option<&str>,
        now_ms: i64,
        expected_revision: Option<u64>,
    ) -> Result<Stored, StoreError> {
        self.write_change(section, patch, actor, now_ms, expected_revision, None)
    }

    /// Writes a [`RevertPlan`] computed by [`ConfigStore::revert_plan`]: its patch, as a new
    /// revision whose history entry says which revision it reverted.
    ///
    /// The plan was computed against [`RevertPlan::revision`], and it is written only over that
    /// revision -- a store that moved on in between answers [`StoreError::RevisionMismatch`],
    /// and the caller plans again. As with [`ConfigStore::patch_section`], validating the
    /// configuration the revert would produce is the caller's job.
    ///
    /// # Errors
    /// As [`ConfigStore::patch_section`].
    pub fn apply_revert(
        &self,
        plan: &RevertPlan,
        actor: Option<&str>,
        now_ms: i64,
    ) -> Result<Stored, StoreError> {
        self.write_change(
            &plan.record.section,
            &plan.patch,
            actor,
            now_ms,
            Some(plan.revision),
            Some(plan.record.revision),
        )
    }

    fn write_change(
        &self,
        section: &str,
        patch: &Value,
        actor: Option<&str>,
        now_ms: i64,
        expected_revision: Option<u64>,
        reverts: Option<u64>,
    ) -> Result<Stored, StoreError> {
        check_section_name(section)?;
        let touched = bootstrap::bootstrap_pointers_in_patch(section, patch);
        if is_bootstrap_section(section) || !touched.is_empty() {
            return Err(StoreError::bootstrap(section, touched));
        }

        let key = section_key(section);
        let outcome = hs_kv::transact(&self.backend, TransactConfig::default(), |txn| {
            let meta: ConfigMeta = match txn.get(&self.keyspace, META_KEY)? {
                Some(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(meta) => meta,
                    Err(e) => return Ok(Err(corrupt("meta", &e))),
                },
                None => ConfigMeta::default(),
            };
            if let Some(expected) = expected_revision
                && expected != meta.revision
            {
                return Ok(Err(StoreError::RevisionMismatch {
                    expected,
                    actual: meta.revision,
                }));
            }

            let mut current = match txn.get(&self.keyspace, &key)? {
                Some(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(value) => value,
                    Err(e) => return Ok(Err(corrupt(section, &e))),
                },
                None => Value::Object(Map::new()),
            };
            let before = crate::history::before_values(&current, patch);
            merge_patch(&mut current, patch);

            let revision = meta.revision.saturating_add(1);
            // A section whose document merged away to nothing stores nothing: the key is removed
            // rather than left as `{}`, so "the database sets nothing here" is one state, not two.
            if current.as_object().is_some_and(serde_json::Map::is_empty) {
                txn.delete(&self.keyspace, &key)?;
            } else {
                txn.put(&self.keyspace, &key, &to_bytes(&current))?;
            }
            let record = ChangeRecord {
                revision,
                section: section.to_owned(),
                patch: patch.clone(),
                actor: actor.map(str::to_owned),
                at_ms: now_ms,
                before: Some(before),
                reverts,
            };
            txn.put(&self.keyspace, &history_key(revision), &to_bytes(&record))?;
            txn.put(
                &self.keyspace,
                META_KEY,
                &to_bytes(&ConfigMeta {
                    revision,
                    updated_at_ms: now_ms,
                    updated_by: actor.map(str::to_owned),
                    seeded_from: meta.seeded_from.clone(),
                    server_name: meta.server_name.clone(),
                }),
            )?;
            Ok(Ok(()))
        })?;
        outcome?;
        self.load()
    }

    /// Removes every bootstrap setting an earlier version stored, and returns the pointers it
    /// removed (empty when there was nothing to do, which is every start after the first one on a
    /// store written since decision 0010).
    ///
    /// Before that decision the store was seeded with the whole bootstrap file, `listeners` and
    /// `cluster.mesh` included, so a cluster's database could hold one replica's ports and hand
    /// them to every other. [`crate::Layers`] already ignores them when it resolves; this is
    /// what makes the store itself stop claiming them, so the web interface does not show a
    /// stored value that has no effect. Each section it changes gets a history entry under
    /// [`PURGE_ACTOR`], a `null` patch for each setting it removed, so the removal is explained
    /// where an operator looks for changes. A stored `server.server_name` becomes the store's
    /// identity ([`ConfigMeta::server_name`]) if it had none.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] or [`StoreError::Kv`].
    pub fn purge_bootstrap(&self, now_ms: i64) -> Result<Vec<String>, StoreError> {
        hs_kv::transact(&self.backend, TransactConfig::default(), |txn| {
            let mut meta: ConfigMeta = match txn.get(&self.keyspace, META_KEY)? {
                Some(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(meta) => meta,
                    Err(e) => return Ok(Err(corrupt("meta", &e))),
                },
                None => return Ok(Ok(Vec::new())),
            };
            let mut removed = Vec::new();
            let mut identity_changed = false;
            for &name in SECTION_NAMES {
                let key = section_key(name);
                let Some(bytes) = txn.get(&self.keyspace, &key)? else {
                    continue;
                };
                let stored: Value = match serde_json::from_slice(&bytes) {
                    Ok(value) => value,
                    Err(e) => return Ok(Err(corrupt(name, &e))),
                };
                if meta.server_name.is_none()
                    && name == "server"
                    && let Some(server_name) = stored.get("server_name").and_then(Value::as_str)
                {
                    meta.server_name = Some(server_name.to_owned());
                    identity_changed = true;
                }
                let mut document =
                    Value::Object(Map::from_iter([(name.to_owned(), stored.clone())]));
                let gone = strip_bootstrap(&mut document);
                if gone.is_empty() {
                    continue;
                }
                match document.get(name) {
                    Some(section) => txn.put(&self.keyspace, &key, &to_bytes(section))?,
                    None => txn.delete(&self.keyspace, &key)?,
                }
                meta.revision = meta.revision.saturating_add(1);
                let patch = null_patch(name, &gone);
                let record = ChangeRecord {
                    revision: meta.revision,
                    section: name.to_owned(),
                    before: Some(crate::history::before_values(&stored, &patch)),
                    patch,
                    actor: Some(PURGE_ACTOR.to_owned()),
                    at_ms: now_ms,
                    reverts: None,
                };
                txn.put(
                    &self.keyspace,
                    &history_key(meta.revision),
                    &to_bytes(&record),
                )?;
                removed.extend(gone);
            }
            if !removed.is_empty() {
                meta.updated_at_ms = now_ms;
                meta.updated_by = Some(PURGE_ACTOR.to_owned());
            }
            if !removed.is_empty() || identity_changed {
                txn.put(&self.keyspace, META_KEY, &to_bytes(&meta))?;
            }
            Ok(Ok(removed))
        })?
    }

    /// The record of an earlier one-time import of `key` of `kind`, if there was one.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] or [`StoreError::Kv`].
    pub fn import_record(&self, kind: &str, key: &str) -> Result<Option<ImportRecord>, StoreError> {
        let snapshot = self.backend.snapshot();
        match snapshot.get(&self.keyspace, &import_key(kind, key))? {
            Some(bytes) => Ok(Some(parse(key, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Records a one-time import, unless one is already recorded for the same kind and key.
    /// Returns `true` when this call recorded it and `false` when an earlier record stands --
    /// two replicas importing the same file at once agree on one record, the first.
    ///
    /// # Errors
    /// [`StoreError::Kv`].
    pub fn record_import(&self, record: &ImportRecord) -> Result<bool, StoreError> {
        let key = import_key(&record.kind, &record.key);
        Ok(hs_kv::transact(
            &self.backend,
            TransactConfig::default(),
            |txn| {
                if txn.get(&self.keyspace, &key)?.is_some() {
                    return Ok(false);
                }
                txn.put(&self.keyspace, &key, &to_bytes(record))?;
                Ok(true)
            },
        )?)
    }

    /// Every recorded import of `kind`, in key order.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] or [`StoreError::Kv`].
    pub fn imports(&self, kind: &str) -> Result<Vec<ImportRecord>, StoreError> {
        let snapshot = self.backend.snapshot();
        let mut out = Vec::new();
        for entry in snapshot.range(
            &self.keyspace,
            RangeSpec::prefix(format!("{IMPORT_PREFIX}{kind}/").into_bytes()),
        ) {
            let (key, value) = entry?;
            out.push(parse(&String::from_utf8_lossy(&key), &value)?);
        }
        Ok(out)
    }

    /// The most recent changes, newest first, at most `limit` of them.
    ///
    /// # Errors
    /// As [`ConfigStore::load`].
    pub fn history(&self, limit: usize) -> Result<Vec<ChangeRecord>, StoreError> {
        let snapshot = self.backend.snapshot();
        let mut out = Vec::new();
        for entry in snapshot.range(
            &self.keyspace,
            RangeSpec::prefix(HISTORY_PREFIX.as_bytes().to_vec())
                .reverse()
                .limit(limit),
        ) {
            let (key, value) = entry?;
            out.push(parse(&String::from_utf8_lossy(&key), &value)?);
        }
        Ok(out)
    }

    /// One page of history, newest first: at most `limit` changes older than revision `before`
    /// (from the newest when `None`), only `section`'s when it is set.
    ///
    /// Filtering by section happens before the limit, so a busy neighbouring section cannot
    /// crowd a quiet one's history off the page. Configuration writes are rare -- an operator
    /// saving a form -- so the scan walks the history keys rather than keeping a per-section
    /// index.
    ///
    /// # Errors
    /// As [`ConfigStore::load`].
    pub fn history_page(
        &self,
        section: Option<&str>,
        before: Option<u64>,
        limit: usize,
    ) -> Result<HistoryPage, StoreError> {
        let limit = limit.max(1);
        let snapshot = self.backend.snapshot();
        let matches = |record: &ChangeRecord| section.is_none_or(|name| record.section == name);

        let upper = match before {
            Some(revision) => history_key(revision),
            None => history_upper_bound(),
        };
        let mut records = Vec::new();
        let mut more = false;
        for entry in snapshot.range(
            &self.keyspace,
            RangeSpec::new(
                Bound::Included(HISTORY_PREFIX.as_bytes().to_vec().into()),
                Bound::Excluded(upper.into()),
            )
            .reverse(),
        ) {
            let (key, value) = entry?;
            let record: ChangeRecord = parse(&String::from_utf8_lossy(&key), &value)?;
            if !matches(&record) {
                continue;
            }
            if records.len() == limit {
                more = true;
                break;
            }
            records.push(record);
        }
        let older = if more {
            records.last().map(|r| r.revision)
        } else {
            None
        };

        // The previous page is the `limit` matching changes just newer than this page's newest.
        // Its cursor is one past the newest of those, which reads as the newest page when there
        // are fewer than `limit` of them.
        let newer = match before {
            None => None,
            Some(before) => {
                let from = records
                    .first()
                    .map_or(before, |record| record.revision.saturating_add(1));
                let mut newest = None;
                let mut seen = 0;
                for entry in snapshot.range(
                    &self.keyspace,
                    RangeSpec::new(
                        Bound::Included(history_key(from).into()),
                        Bound::Excluded(history_upper_bound().into()),
                    ),
                ) {
                    let (key, value) = entry?;
                    let record: ChangeRecord = parse(&String::from_utf8_lossy(&key), &value)?;
                    if !matches(&record) {
                        continue;
                    }
                    newest = Some(record.revision);
                    seen += 1;
                    if seen == limit {
                        break;
                    }
                }
                newest.map(|revision| revision.saturating_add(1))
            }
        };
        Ok(HistoryPage {
            records,
            older,
            newer,
        })
    }

    /// The change recorded at `revision`, if there is one.
    ///
    /// # Errors
    /// As [`ConfigStore::load`].
    pub fn change(&self, revision: u64) -> Result<Option<ChangeRecord>, StoreError> {
        let snapshot = self.backend.snapshot();
        match snapshot.get(&self.keyspace, &history_key(revision))? {
            Some(bytes) => Ok(Some(parse(&format!("history/{revision}"), &bytes)?)),
            None => Ok(None),
        }
    }

    /// How to undo the change to `section` recorded at `revision`, against what is stored now:
    /// the merge patch that puts every setting it touched back to what the database held before
    /// it ([`crate::history::revert_target`]), and the later changes to any of the same settings,
    /// which the revert would undo as well.
    ///
    /// Nothing is written. [`ConfigStore::apply_revert`] writes the plan once the caller has
    /// validated what it would produce and decided about the conflicts.
    ///
    /// # Errors
    /// [`StoreError::UnknownSection`]; [`StoreError::NoSuchChange`] when no change to `section`
    /// was recorded at `revision`; [`StoreError::NotRevertible`] for a change recorded before
    /// prior values were kept; otherwise as [`ConfigStore::load`].
    pub fn revert_plan(&self, section: &str, revision: u64) -> Result<RevertPlan, StoreError> {
        check_section_name(section)?;
        let snapshot = self.backend.snapshot();
        let no_such = || StoreError::NoSuchChange {
            section: section.to_owned(),
            revision,
        };
        let record: ChangeRecord = match snapshot.get(&self.keyspace, &history_key(revision))? {
            Some(bytes) => parse(&format!("history/{revision}"), &bytes)?,
            None => return Err(no_such()),
        };
        if record.section != section {
            return Err(no_such());
        }
        let Some(before) = record.before.as_ref() else {
            return Err(StoreError::NotRevertible {
                revision,
                reason: "it was recorded before this server kept the values a change replaced, \
                         so what to put back is not known -- set the settings by hand instead"
                    .to_owned(),
            });
        };

        let meta: ConfigMeta = match snapshot.get(&self.keyspace, META_KEY)? {
            Some(bytes) => parse("meta", &bytes)?,
            None => ConfigMeta::default(),
        };
        let current = match snapshot.get(&self.keyspace, &section_key(section))? {
            Some(bytes) => parse(section, &bytes)?,
            None => Value::Object(Map::new()),
        };

        let mut conflicts = Vec::new();
        for entry in snapshot.range(
            &self.keyspace,
            RangeSpec::new(
                Bound::Excluded(history_key(revision).into()),
                Bound::Excluded(history_upper_bound().into()),
            ),
        ) {
            let (key, value) = entry?;
            let later: ChangeRecord = parse(&String::from_utf8_lossy(&key), &value)?;
            if later.section != section {
                continue;
            }
            let shared: Vec<String> = crate::document::leaf_pointers(&later.patch)
                .into_iter()
                .filter(|p| {
                    before
                        .keys()
                        .any(|t| crate::history::pointers_overlap(t, p))
                })
                .collect();
            if !shared.is_empty() {
                conflicts.push(LaterChange {
                    revision: later.revision,
                    actor: later.actor,
                    at_ms: later.at_ms,
                    pointers: shared,
                });
            }
        }

        let target = crate::history::revert_target(&current, before);
        let patch = crate::history::diff_merge_patch(&current, &target);
        Ok(RevertPlan {
            record,
            patch,
            conflicts,
            revision: meta.revision,
        })
    }
}

/// The merge patch that removes each of `pointers` from `section`: `{"mesh": null}` for
/// `/cluster/mesh`. What [`ConfigStore::purge_bootstrap`] records, so its history entry reads the
/// same as an operator's reset would.
fn null_patch(section: &str, pointers: &[String]) -> Value {
    let prefix = format!("/{section}");
    let mut patch = Map::new();
    for pointer in pointers {
        let relative = pointer.strip_prefix(&prefix).unwrap_or("");
        let tokens: Vec<&str> = relative.split('/').filter(|t| !t.is_empty()).collect();
        let Some((last, parents)) = tokens.split_last() else {
            continue;
        };
        let mut cursor = &mut patch;
        for token in parents {
            let next = cursor
                .entry((*token).to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if !next.is_object() {
                *next = Value::Object(Map::new());
            }
            let Value::Object(map) = next else {
                unreachable!("forced to an object directly above");
            };
            cursor = map;
        }
        cursor.insert((*last).to_owned(), Value::Null);
    }
    Value::Object(patch)
}

fn check_section_name(section: &str) -> Result<(), StoreError> {
    if SECTION_NAMES.contains(&section) {
        Ok(())
    } else {
        Err(StoreError::UnknownSection {
            section: section.to_owned(),
        })
    }
}

fn parse<T: serde::de::DeserializeOwned>(key: &str, bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| corrupt(key, &e))
}

fn corrupt(key: &str, e: &serde_json::Error) -> StoreError {
    StoreError::Corrupt {
        key: key.to_owned(),
        detail: e.to_string(),
    }
}

fn to_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("configuration values are plain JSON and always serialize")
}

/// Every section's current document, keyed by name -- what the admin API's `GET /config` lists.
#[must_use]
pub fn sections_of(document: &Value) -> BTreeMap<&'static str, Value> {
    let mut out = BTreeMap::new();
    for &name in SECTION_NAMES {
        let value = document
            .get(name)
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        out.insert(name, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use serde_json::json;

    fn store() -> ConfigStore<MemoryBackend> {
        ConfigStore::open(MemoryBackend::new()).unwrap()
    }

    #[test]
    fn an_empty_store_reads_as_an_empty_document_at_revision_zero() {
        let stored = store().load().unwrap();
        assert!(stored.is_empty());
        assert_eq!(stored.meta.revision, 0);
    }

    #[test]
    fn a_patch_round_trips_and_bumps_the_revision() {
        let store = store();
        let stored = store
            .patch_section(
                "auth",
                &json!({"enable_registration": true}),
                Some("@admin:example.org"),
                1_700_000_000_000,
                None,
            )
            .unwrap();
        assert_eq!(stored.meta.revision, 1);
        assert_eq!(
            stored.document.pointer("/auth/enable_registration"),
            Some(&json!(true))
        );
        assert_eq!(
            stored.meta.updated_by.as_deref(),
            Some("@admin:example.org")
        );
    }

    /// The reset-to-default path, through the store: patching `null` removes the setting, and a
    /// section left with nothing in it stores nothing at all rather than an empty object.
    #[test]
    fn patching_null_clears_the_setting_and_then_the_section() {
        let store = store();
        store
            .patch_section("auth", &json!({"enable_registration": true}), None, 1, None)
            .unwrap();
        let stored = store
            .patch_section("auth", &json!({"enable_registration": null}), None, 2, None)
            .unwrap();
        assert_eq!(stored.document.pointer("/auth/enable_registration"), None);
        assert_eq!(store.section("auth").unwrap(), None);
    }

    /// Two operators editing the same section: the second write, computed against a view that is
    /// now stale, is refused rather than silently clobbering the first.
    #[test]
    fn a_stale_if_match_revision_is_refused() {
        let store = store();
        store
            .patch_section("auth", &json!({"enable_registration": true}), None, 1, None)
            .unwrap();
        let err = store
            .patch_section(
                "auth",
                &json!({"enable_registration": false}),
                None,
                2,
                Some(0),
            )
            .unwrap_err();
        match err {
            StoreError::RevisionMismatch { expected, actual } => {
                assert_eq!((expected, actual), (0, 1));
            }
            other => panic!("expected a revision mismatch, got {other:?}"),
        }
        // And nothing was written: the refused patch left the first operator's value alone.
        assert_eq!(
            store
                .load()
                .unwrap()
                .document
                .pointer("/auth/enable_registration"),
            Some(&json!(true))
        );
    }

    #[test]
    fn storage_cannot_be_written_to_the_database_it_points_at() {
        let err = store()
            .patch_section("storage", &json!({"backend": "postgres"}), None, 1, None)
            .unwrap_err();
        assert!(matches!(err, StoreError::BootstrapSection { .. }));
    }

    #[test]
    fn an_unknown_section_is_refused() {
        let err = store()
            .patch_section("nonsense", &json!({"x": 1}), None, 1, None)
            .unwrap_err();
        assert!(matches!(err, StoreError::UnknownSection { .. }));
    }

    #[test]
    fn seeding_happens_once_and_skips_the_bootstrap_section() {
        let store = store();
        let document = json!({
            "server": {"server_name": "example.org", "admin_contact": "mailto:a@example.org"},
            "storage": {"backend": "embedded", "data_dir": "/var/lib/myelin"},
        });
        assert!(store.seed(&document, "homeserver.yaml", 10).unwrap());

        let stored = store.load().unwrap();
        assert_eq!(
            stored.document.pointer("/server/admin_contact"),
            Some(&json!("mailto:a@example.org"))
        );
        assert_eq!(
            stored.document.get("storage"),
            None,
            "storage says where this database is; it cannot live inside it"
        );
        assert_eq!(stored.meta.seeded_from.as_deref(), Some("homeserver.yaml"));

        // A second boot with the same file does not overwrite what an operator has since changed
        // in the web interface.
        store
            .patch_section(
                "server",
                &json!({"admin_contact": "mailto:b@example.org"}),
                None,
                20,
                None,
            )
            .unwrap();
        assert!(!store.seed(&document, "homeserver.yaml", 30).unwrap());
        assert_eq!(
            store
                .load()
                .unwrap()
                .document
                .pointer("/server/admin_contact"),
            Some(&json!("mailto:b@example.org"))
        );
    }

    /// Decision 0010: nothing a process needs before it can read its database, and nothing that
    /// belongs to one replica rather than to the server, is copied into the shared store.
    #[test]
    fn bootstrap_settings_are_not_seeded_and_the_server_name_becomes_the_identity() {
        let store = store();
        let document = json!({
            "server": {
                "server_name": "example.org",
                "signing_key_path": "/data/keys",
                "public_baseurl": "https://matrix.example.org",
            },
            "listeners": {"listeners": [{"port": 8008, "bind_addresses": ["::"], "resources": ["client"]}]},
            "cluster": {
                "single_node": false,
                "lease_ttl": "20s",
                "mesh": {"port": 9449, "advertise_address": "hs-0.hs-headless"},
            },
            "appservices": {"registration_files": ["/etc/hs/irc.yaml"], "enabled": true},
            "auth": {"enable_registration": true},
        });
        assert!(store.seed(&document, "homeserver.yaml", 10).unwrap());
        let stored = store.load().unwrap();
        for pointer in [
            "/storage",
            "/listeners",
            "/server/server_name",
            "/server/signing_key_path",
            "/cluster/single_node",
            "/cluster/mesh",
            "/appservices/registration_files",
        ] {
            assert_eq!(
                stored.document.pointer(pointer),
                None,
                "{pointer} is bootstrap and must not be seeded"
            );
        }
        assert_eq!(
            stored.document,
            json!({
                "server": {"public_baseurl": "https://matrix.example.org"},
                "cluster": {"lease_ttl": "20s"},
                "appservices": {"enabled": true},
                "auth": {"enable_registration": true},
            }),
            "every administered setting is seeded"
        );
        assert_eq!(stored.meta.server_name.as_deref(), Some("example.org"));
    }

    #[test]
    fn a_patch_that_writes_a_bootstrap_setting_is_refused_and_writes_nothing() {
        let store = store();
        for (section, patch) in [
            ("cluster", json!({"mesh": {"port": 9000}})),
            ("cluster", json!({"single_node": false})),
            ("server", json!({"server_name": "other.example"})),
            ("server", json!({"signing_key_path": "/elsewhere"})),
            ("appservices", json!({"registration_files": ["/x.yaml"]})),
        ] {
            let err = store
                .patch_section(section, &patch, None, 1, None)
                .unwrap_err();
            assert!(
                matches!(err, StoreError::BootstrapSetting { .. }),
                "{section} {patch}: {err:?}"
            );
        }
        let err = store
            .patch_section("listeners", &json!({"listeners": []}), None, 1, None)
            .unwrap_err();
        assert!(matches!(err, StoreError::BootstrapSection { .. }));
        assert_eq!(
            store.load().unwrap().meta.revision,
            0,
            "nothing was written"
        );

        // A sibling administered setting in the same section is still writable.
        store
            .patch_section("cluster", &json!({"lease_ttl": "30s"}), None, 2, None)
            .unwrap();
    }

    /// A store written before decision 0010 holds the seeding replica's listeners and mesh port.
    /// The purge removes them, keeps the server name as the identity, explains itself in the
    /// history, and is a no-op the second time.
    #[test]
    fn a_legacy_store_is_purged_of_its_bootstrap_settings_once() {
        let backend = MemoryBackend::new();
        let store = ConfigStore::open(backend.clone()).unwrap();
        // Write the legacy shape directly, as a pre-0010 seed did.
        let keyspace = backend.keyspace(KEYSPACE).unwrap();
        hs_kv::transact(&backend, TransactConfig::default(), |txn| {
            txn.put(
                &keyspace,
                &section_key("server"),
                &to_bytes(&json!({"server_name": "example.org", "admin_contact": "a@b"})),
            )?;
            txn.put(
                &keyspace,
                &section_key("listeners"),
                &to_bytes(&json!({"listeners": [{"port": 18008}]})),
            )?;
            txn.put(
                &keyspace,
                &section_key("cluster"),
                &to_bytes(&json!({"mesh": {"port": 18449}, "lease_ttl": "20s"})),
            )?;
            txn.put(
                &keyspace,
                META_KEY,
                &to_bytes(&ConfigMeta {
                    revision: 1,
                    updated_at_ms: 1,
                    updated_by: None,
                    seeded_from: Some("replica-a.yaml".to_owned()),
                    server_name: None,
                }),
            )?;
            Ok(())
        })
        .unwrap();

        let mut removed = store.purge_bootstrap(50).unwrap();
        removed.sort();
        assert_eq!(
            removed,
            vec!["/cluster/mesh", "/listeners", "/server/server_name"]
        );
        let stored = store.load().unwrap();
        assert_eq!(
            stored.document,
            json!({"server": {"admin_contact": "a@b"}, "cluster": {"lease_ttl": "20s"}})
        );
        assert_eq!(stored.meta.server_name.as_deref(), Some("example.org"));
        assert_eq!(stored.meta.revision, 4, "one revision per section changed");
        let history = store.history(10).unwrap();
        assert_eq!(history.len(), 3);
        assert!(
            history
                .iter()
                .all(|h| h.actor.as_deref() == Some(PURGE_ACTOR))
        );
        let cluster = history.iter().find(|h| h.section == "cluster").unwrap();
        assert_eq!(cluster.patch, json!({"mesh": null}));

        assert!(store.purge_bootstrap(60).unwrap().is_empty());
        assert_eq!(store.load().unwrap().meta.revision, 4);
    }

    #[test]
    fn an_empty_store_has_nothing_to_purge() {
        let store = store();
        assert!(store.purge_bootstrap(1).unwrap().is_empty());
        assert_eq!(store.load().unwrap().meta.revision, 0);
    }

    #[test]
    fn an_import_is_recorded_once_and_the_first_record_stands() {
        let store = store();
        let kind = "appservice_registration";
        assert_eq!(store.import_record(kind, "/etc/hs/irc.yaml").unwrap(), None);
        let first = ImportRecord {
            kind: kind.to_owned(),
            key: "/etc/hs/irc.yaml".to_owned(),
            outcome: "imported as irc".to_owned(),
            at_ms: 1,
        };
        assert!(store.record_import(&first).unwrap());
        let second = ImportRecord {
            outcome: "something else".to_owned(),
            at_ms: 2,
            ..first.clone()
        };
        assert!(!store.record_import(&second).unwrap());
        assert_eq!(
            store.import_record(kind, "/etc/hs/irc.yaml").unwrap(),
            Some(first.clone())
        );
        assert_eq!(store.imports(kind).unwrap(), vec![first]);
        assert!(store.imports("other_kind").unwrap().is_empty());
        assert!(
            store.load().unwrap().is_empty(),
            "an import record is not configuration"
        );
    }

    #[test]
    fn history_is_newest_first_and_records_the_patch() {
        let store = store();
        store
            .patch_section(
                "auth",
                &json!({"enable_registration": true}),
                Some("a"),
                1,
                None,
            )
            .unwrap();
        store
            .patch_section(
                "federation",
                &json!({"client_timeout": "45s"}),
                Some("b"),
                2,
                None,
            )
            .unwrap();
        let history = store.history(10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].section, "federation");
        assert_eq!(history[0].revision, 2);
        assert_eq!(history[0].patch, json!({"client_timeout": "45s"}));
        assert_eq!(history[1].section, "auth");
    }

    fn set(store: &ConfigStore<MemoryBackend>, section: &str, patch: Value, actor: &str) -> u64 {
        store
            .patch_section(section, &patch, Some(actor), 1, None)
            .unwrap()
            .meta
            .revision
    }

    #[test]
    fn a_change_records_what_each_setting_held_before() {
        let store = store();
        set(
            &store,
            "rate_limits",
            json!({"login": {"per_second": 5.0}}),
            "a",
        );
        set(
            &store,
            "rate_limits",
            json!({"login": {"per_second": 10.0, "burst_count": 3}}),
            "b",
        );
        let latest = &store.history(1).unwrap()[0];
        assert_eq!(
            latest.before,
            Some(BTreeMap::from([
                ("/login/burst_count".to_owned(), Value::Null),
                ("/login/per_second".to_owned(), json!(5.0)),
            ]))
        );
        assert_eq!(latest.reverts, None);
    }

    /// A record written before `before` existed still reads, and is refused for a revert with a
    /// reason rather than reverted with a guess.
    #[test]
    fn a_legacy_record_reads_but_is_not_revertible() {
        let backend = MemoryBackend::new();
        let store = ConfigStore::open(backend.clone()).unwrap();
        let keyspace = backend.keyspace(KEYSPACE).unwrap();
        let legacy = json!({
            "revision": 1, "section": "auth", "patch": {"enable_registration": true},
            "actor": "@old:example.org", "at_ms": 5
        });
        hs_kv::transact(&backend, TransactConfig::default(), |txn| {
            txn.put(&keyspace, &history_key(1), &to_bytes(&legacy))?;
            Ok(())
        })
        .unwrap();
        let record = store.change(1).unwrap().unwrap();
        assert_eq!(record.before, None);
        assert!(matches!(
            store.revert_plan("auth", 1),
            Err(StoreError::NotRevertible { revision: 1, .. })
        ));
    }

    #[test]
    fn history_pages_newest_first_per_section_with_cursors_both_ways() {
        let store = store();
        // auth at 1, 3, 5, 7, 9; federation in between.
        for i in 0..5 {
            set(
                &store,
                "auth",
                json!({"enable_registration": i % 2 == 0}),
                "a",
            );
            set(
                &store,
                "federation",
                json!({"client_timeout": format!("{i}s")}),
                "b",
            );
        }
        let first = store.history_page(Some("auth"), None, 2).unwrap();
        let revisions =
            |page: &HistoryPage| page.records.iter().map(|r| r.revision).collect::<Vec<_>>();
        assert_eq!(revisions(&first), vec![9, 7]);
        assert_eq!(first.newer, None);
        assert_eq!(first.older, Some(7));

        let second = store.history_page(Some("auth"), first.older, 2).unwrap();
        assert_eq!(revisions(&second), vec![5, 3]);
        assert_eq!(second.older, Some(3));
        let back = store.history_page(Some("auth"), second.newer, 2).unwrap();
        assert_eq!(
            revisions(&back),
            vec![9, 7],
            "newer leads back to the first page"
        );

        let last = store.history_page(Some("auth"), second.older, 2).unwrap();
        assert_eq!(revisions(&last), vec![1]);
        assert_eq!(last.older, None);
        assert!(last.newer.is_some());

        let everything = store.history_page(None, None, 3).unwrap();
        assert_eq!(revisions(&everything), vec![10, 9, 8]);
    }

    #[test]
    fn a_revert_puts_back_what_the_change_replaced_as_a_new_revision() {
        let store = store();
        set(
            &store,
            "rate_limits",
            json!({"login": {"per_second": 5.0}}),
            "a",
        );
        let changed = set(
            &store,
            "rate_limits",
            json!({"login": {"per_second": 10.0}, "message": {"per_second": 1.0}}),
            "b",
        );
        let plan = store.revert_plan("rate_limits", changed).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(
            plan.patch,
            json!({"login": {"per_second": 5.0}, "message": null})
        );
        let stored = store.apply_revert(&plan, Some("c"), 9).unwrap();
        assert_eq!(stored.meta.revision, changed + 1);
        assert_eq!(
            store.section("rate_limits").unwrap(),
            Some(json!({"login": {"per_second": 5.0}}))
        );
        let record = store.change(changed + 1).unwrap().unwrap();
        assert_eq!(record.reverts, Some(changed));
        assert_eq!(record.actor.as_deref(), Some("c"));

        // Reverting the revert redoes the change.
        let redo = store.revert_plan("rate_limits", changed + 1).unwrap();
        assert!(redo.conflicts.is_empty());
        store.apply_revert(&redo, Some("c"), 10).unwrap();
        assert_eq!(
            store.section("rate_limits").unwrap(),
            Some(json!({"login": {"per_second": 10.0}, "message": {"per_second": 1.0}}))
        );
    }

    #[test]
    fn a_later_change_to_the_same_setting_is_reported_and_a_neighbour_is_not() {
        let store = store();
        let first = set(
            &store,
            "rate_limits",
            json!({"login": {"per_second": 10.0}}),
            "a",
        );
        set(
            &store,
            "rate_limits",
            json!({"login": {"burst_count": 7}}),
            "b",
        );
        set(&store, "auth", json!({"enable_registration": true}), "b");
        let clean = store.revert_plan("rate_limits", first).unwrap();
        assert!(clean.conflicts.is_empty(), "burst_count is a neighbour");

        let later = set(
            &store,
            "rate_limits",
            json!({"login": null}),
            "@ops:example.org",
        );
        let plan = store.revert_plan("rate_limits", first).unwrap();
        assert_eq!(
            plan.conflicts,
            vec![LaterChange {
                revision: later,
                actor: Some("@ops:example.org".to_owned()),
                at_ms: 1,
                pointers: vec!["/login".to_owned()],
            }]
        );
    }

    #[test]
    fn a_revert_planned_against_a_store_that_moved_on_is_refused() {
        let store = store();
        let first = set(&store, "auth", json!({"enable_registration": true}), "a");
        let plan = store.revert_plan("auth", first).unwrap();
        set(&store, "federation", json!({"client_timeout": "9s"}), "b");
        assert!(matches!(
            store.apply_revert(&plan, None, 2),
            Err(StoreError::RevisionMismatch { .. })
        ));
    }

    #[test]
    fn reverting_a_change_to_another_section_or_nowhere_is_no_such_change() {
        let store = store();
        let first = set(&store, "auth", json!({"enable_registration": true}), "a");
        assert!(matches!(
            store.revert_plan("federation", first),
            Err(StoreError::NoSuchChange { .. })
        ));
        assert!(matches!(
            store.revert_plan("auth", 99),
            Err(StoreError::NoSuchChange { .. })
        ));
    }

    /// Rotating a secret and reverting restores the old secret from the store's own record.
    #[test]
    fn a_revert_restores_a_rotated_secret() {
        let store = store();
        set(
            &store,
            "migration",
            json!({"synapse": {"database_url": "postgres://old"}}),
            "a",
        );
        let rotated = set(
            &store,
            "migration",
            json!({"synapse": {"database_url": "postgres://new"}}),
            "a",
        );
        let plan = store.revert_plan("migration", rotated).unwrap();
        store.apply_revert(&plan, Some("b"), 3).unwrap();
        assert_eq!(
            store
                .load()
                .unwrap()
                .document
                .pointer("/migration/synapse/database_url"),
            Some(&json!("postgres://old"))
        );
    }

    #[test]
    fn sections_of_names_every_section_even_when_the_store_is_empty() {
        let sections = sections_of(&Value::Object(Map::new()));
        assert_eq!(sections.len(), SECTION_NAMES.len());
        assert_eq!(sections["auth"], json!({}));
    }
}
