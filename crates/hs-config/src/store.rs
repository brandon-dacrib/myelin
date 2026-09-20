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
//! [`BOOTSTRAP_SECTIONS`] — `storage` alone today — says where the database *is*, so it is read
//! before there is a database to read it from. It comes from the command line, an `HS__` variable
//! or the bootstrap file, and writing it here is refused with an error that says so rather than
//! accepted and ignored.
//!
//! # Layout
//!
//! One `hs-kv` keyspace, [`KEYSPACE`], with three kinds of key:
//!
//! - `section/<name>` — that section's sparse document, as JSON. Absent means the section sets
//!   nothing and everything in it falls through to the file or the schema default.
//! - `meta` — [`ConfigMeta`]: the revision counter, who last wrote and when.
//! - `history/<revision>` — [`ChangeRecord`], one per write, so the web interface can show what
//!   changed and when without a separate audit store.
//!
//! Keys are fixed ASCII with a zero-padded decimal revision, so `history/` scans in revision
//! order. There is no tuple encoding and no index to maintain, which is why this does not go
//! through `hs-tables`.

use std::collections::BTreeMap;

use hs_kv::{KvBackend, KvError, KvRead, KvWrite, RangeSpec, TransactConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::document::merge_patch;
use crate::reload::SECTION_NAMES;

/// The `hs-kv` keyspace this store owns.
pub const KEYSPACE: &str = "config";

/// Sections that are read before the database is open, and so cannot be stored in it.
pub const BOOTSTRAP_SECTIONS: &[&str] = &["storage"];

/// True when `section` must come from the bootstrap layer rather than the database.
#[must_use]
pub fn is_bootstrap_section(section: &str) -> bool {
    BOOTSTRAP_SECTIONS.contains(&section)
}

const META_KEY: &[u8] = b"meta";
const SECTION_PREFIX: &str = "section/";
const HISTORY_PREFIX: &str = "history/";

fn section_key(name: &str) -> Vec<u8> {
    format!("{SECTION_PREFIX}{name}").into_bytes()
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
        "{section:?} says where this server's database is, so it is read before the database is \
         open and cannot be stored in it -- set it on the command line, in an HS__ environment \
         variable, or in the bootstrap file"
    )]
    BootstrapSection {
        /// The section the caller tried to write.
        section: String,
    },
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

    /// Writes `document`'s sections as the initial contents, but only if nothing has been written
    /// yet. Returns `true` when it seeded and `false` when the store already held configuration
    /// and was left untouched.
    ///
    /// This is how an existing `homeserver.yaml` deployment moves into the database: the first
    /// boot copies the file in, and from then on the database is what the server reads and the
    /// web interface writes. Re-running it is a no-op, so a file left mounted after the move
    /// cannot quietly revert a change made in the UI.
    ///
    /// # Errors
    /// Returns [`StoreError::Kv`] on a backend failure.
    pub fn seed(&self, document: &Value, source: &str, now_ms: i64) -> Result<bool, StoreError> {
        let sections: Vec<(String, Value)> = document
            .as_object()
            .map(|map| {
                map.iter()
                    .filter(|(name, _)| !is_bootstrap_section(name))
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
    /// [`StoreError::RevisionMismatch`], [`StoreError::Corrupt`] or [`StoreError::Kv`].
    pub fn patch_section(
        &self,
        section: &str,
        patch: &Value,
        actor: Option<&str>,
        now_ms: i64,
        expected_revision: Option<u64>,
    ) -> Result<Stored, StoreError> {
        check_section_name(section)?;
        if is_bootstrap_section(section) {
            return Err(StoreError::BootstrapSection {
                section: section.to_owned(),
            });
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
                }),
            )?;
            Ok(Ok(()))
        })?;
        outcome?;
        self.load()
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
            "server": {"server_name": "example.org"},
            "storage": {"backend": "embedded", "data_dir": "/var/lib/myelin"},
        });
        assert!(store.seed(&document, "homeserver.yaml", 10).unwrap());

        let stored = store.load().unwrap();
        assert_eq!(
            stored.document.pointer("/server/server_name"),
            Some(&json!("example.org"))
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
                &json!({"server_name": "changed.example"}),
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
                .pointer("/server/server_name"),
            Some(&json!("changed.example"))
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

    #[test]
    fn sections_of_names_every_section_even_when_the_store_is_empty() {
        let sections = sections_of(&Value::Object(Map::new()));
        assert_eq!(sections.len(), SECTION_NAMES.len());
        assert_eq!(sections["auth"], json!({}));
    }
}
