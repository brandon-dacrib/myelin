//! Assembling the effective configuration out of its layers.
//!
//! The server's settings live in its database ([`crate::store`]). A bootstrap file may still
//! supply them -- that is how an existing `homeserver.yaml` deployment keeps working, and how the
//! `storage` section, which says where the database is, gets read at all -- and `HS__`
//! environment variables still override everything, because a deployment that pins a setting in
//! its manifest has said something this server must not quietly undo.
//!
//! [`Layers::resolve`] merges the three into one document and deserializes it, and reports which
//! layer each setting came from so the web interface can show an operator why a value is what it
//! is, and refuse to offer an edit that the environment would override anyway.
//!
//! The database layer never supplies a bootstrap setting ([`crate::bootstrap`]): whatever it
//! holds under one of those pointers -- only a store written before decision 0010 can -- is
//! ignored when the layers are merged, so a replica's listeners and mesh identity always come
//! from its own file and environment.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::document::{self, Origin};
use crate::error::ConfigError;
use crate::{Config, store};

/// The bootstrap file, if one was given: where it came from, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLayer {
    /// The path, for error messages and for the admin API's provenance reporting.
    pub path: PathBuf,
    /// The document it parsed to.
    pub document: Value,
}

/// The layers the effective configuration is built from, lowest precedence first.
#[derive(Debug, Clone)]
pub struct Layers {
    /// The bootstrap file, if any.
    pub file: Option<FileLayer>,
    /// What the database holds ([`crate::store::ConfigStore::load`]).
    pub database: Value,
    /// The `HS__` environment overrides, as a document
    /// ([`crate::env::override_document`]).
    pub environment: Value,
}

/// A resolved configuration: the typed value, the document it came from, and where each setting
/// was set.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The validated configuration, with file-backed secrets resolved.
    pub config: Config,
    /// The merged document, before deserialization.
    pub document: Value,
    /// The layer that set each setting, by JSON Pointer. A setting that is absent here is at its
    /// schema default.
    pub origins: BTreeMap<String, Origin>,
}

impl Resolved {
    /// Where one setting came from, by JSON Pointer (`/auth/enable_registration`).
    #[must_use]
    pub fn origin(&self, pointer: &str) -> Origin {
        self.origins
            .get(pointer)
            .copied()
            .unwrap_or(Origin::Default)
    }
}

/// An empty set of layers: no file, nothing in the database, nothing in the environment -- the
/// schema's own defaults.
///
/// Both documents are empty *objects*, not `Value::Null`. A derived `Default` would give null, and
/// RFC 7396 says a non-object patch replaces its target wholesale -- so merging a null layer
/// collapses the entire configuration to null and every setting silently reads back at its
/// default, which looks exactly like a configuration that was never set rather than like a bug.
impl Default for Layers {
    fn default() -> Self {
        Self {
            file: None,
            database: Value::Object(Map::new()),
            environment: Value::Object(Map::new()),
        }
    }
}

impl Layers {
    /// The layers for a server booting with no database yet: file and environment only.
    #[must_use]
    pub fn bootstrap<I>(file: Option<FileLayer>, env_vars: I) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        Self {
            file,
            environment: crate::env::override_document(env_vars),
            ..Self::default()
        }
    }

    /// The same layers with `database` swapped in -- what a server does once its store is open.
    #[must_use]
    pub fn with_database(mut self, database: Value) -> Self {
        self.database = database;
        self
    }

    /// The layers in precedence order, lowest first, with the bootstrap settings taken out of the
    /// database layer.
    fn ordered(&self) -> Vec<(Origin, Cow<'_, Value>)> {
        let mut out = Vec::with_capacity(3);
        if let Some(file) = &self.file {
            out.push((Origin::File, Cow::Borrowed(&file.document)));
        }
        out.push((Origin::Database, self.administered_database()));
        out.push((Origin::Environment, Cow::Borrowed(&self.environment)));
        out
    }

    /// The database layer without any bootstrap setting in it, borrowed when there was none.
    fn administered_database(&self) -> Cow<'_, Value> {
        let holds_bootstrap = crate::bootstrap::BOOTSTRAP_SETTINGS
            .iter()
            .any(|setting| self.database.pointer(setting.pointer).is_some());
        if holds_bootstrap {
            Cow::Owned(crate::bootstrap::without_bootstrap(&self.database))
        } else {
            Cow::Borrowed(&self.database)
        }
    }

    /// Merges every layer into one document, without deserializing or validating it.
    #[must_use]
    pub fn merged(&self) -> Value {
        let ordered = self.ordered();
        document::merge_all(ordered.iter().map(|(_, doc)| doc.as_ref()))
    }

    /// Merges, deserializes, resolves file-backed secrets and validates.
    ///
    /// # Errors
    /// Returns [`ConfigError::Parse`] if the merged document does not match the schema,
    /// [`ConfigError::SecretFile`]/[`ConfigError::SecretConflict`] if a `*_file` secret could not
    /// be resolved, or [`ConfigError::Validation`] with every validation problem at once.
    pub fn resolve(&self) -> Result<Resolved, ConfigError> {
        let document = self.merged();
        let config = Config::from_json(&document)?;
        let ordered = self.ordered();
        Ok(Resolved {
            config,
            document,
            origins: document::origins(ordered.iter().map(|(origin, doc)| (*origin, doc.as_ref()))),
        })
    }

    /// Resolves as if `patch` had already been applied to `section` in the database.
    ///
    /// This is what the admin API validates a proposed change against: a patch is legal only if
    /// the configuration it *produces* is, which cannot be decided by looking at the patch alone
    /// (a value may be fine on its own and contradict another section, and a section may only
    /// become valid once something else is set).
    ///
    /// # Errors
    /// As [`Layers::resolve`].
    pub fn resolve_with_patch(
        &self,
        section: &str,
        patch: &Value,
    ) -> Result<Resolved, ConfigError> {
        let mut candidate = self.clone();
        let mut database = candidate.database.clone();
        document::merge_patch(
            &mut database,
            &Value::Object(
                [(section.to_owned(), patch.clone())]
                    .into_iter()
                    .collect::<Map<String, Value>>(),
            ),
        );
        candidate.database = database;
        candidate.resolve()
    }

    /// The settings in `patch` that the environment pins, as JSON Pointers into the whole
    /// configuration (`/federation/client_timeout`).
    ///
    /// A write to any of these would be stored faithfully and then have no effect, because the
    /// environment layer sits above the database. The admin API refuses such a write and names
    /// the settings, rather than reporting a success an operator would later find untrue.
    #[must_use]
    pub fn pinned_by_environment(&self, section: &str, patch: &Value) -> Vec<String> {
        let prefix = format!("/{section}");
        document::leaf_pointers(patch)
            .into_iter()
            .map(|leaf| format!("{prefix}{leaf}"))
            .filter(|pointer| self.environment.pointer(pointer).is_some())
            .collect()
    }

    /// True when `section` is one the database cannot hold (see
    /// [`crate::store::BOOTSTRAP_SECTIONS`]).
    #[must_use]
    pub fn is_bootstrap_section(section: &str) -> bool {
        store::is_bootstrap_section(section)
    }

    /// The bootstrap settings a patch against `section` would write, as whole-configuration JSON
    /// Pointers ([`crate::bootstrap::bootstrap_pointers_in_patch`]). The admin API refuses such a
    /// patch: the database would store it and every replica would ignore it.
    #[must_use]
    pub fn bootstrap_in_patch(section: &str, patch: &Value) -> Vec<String> {
        crate::bootstrap::bootstrap_pointers_in_patch(section, patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn layers() -> Layers {
        Layers {
            file: Some(FileLayer {
                path: PathBuf::from("/etc/myelin/homeserver.yaml"),
                document: json!({
                    "server": {"server_name": "example.org"},
                    "auth": {"enable_registration": false},
                }),
            }),
            database: json!({"auth": {"enable_registration": true}}),
            environment: json!({}),
        }
    }

    /// The point of the whole design: a setting changed in the web interface is what the server
    /// runs on, even though a file that says otherwise is still mounted.
    #[test]
    fn the_database_beats_the_file_it_was_seeded_from() {
        let resolved = layers().resolve().unwrap();
        assert!(resolved.config.auth.enable_registration);
        assert_eq!(
            resolved.origin("/auth/enable_registration"),
            Origin::Database
        );
        assert_eq!(resolved.origin("/server/server_name"), Origin::File);
        assert_eq!(
            resolved.origin("/federation/client_timeout"),
            Origin::Default
        );
    }

    #[test]
    fn the_environment_beats_the_database() {
        let mut layers = layers();
        layers.environment = json!({"auth": {"enable_registration": false}});
        let resolved = layers.resolve().unwrap();
        assert!(!resolved.config.auth.enable_registration);
        assert_eq!(
            resolved.origin("/auth/enable_registration"),
            Origin::Environment
        );
    }

    /// A write the environment would override is refused rather than accepted and ignored.
    #[test]
    fn a_setting_the_environment_pins_is_reported_before_it_is_written() {
        let mut layers = layers();
        layers.environment = json!({"auth": {"enable_registration": false}});
        assert_eq!(
            layers.pinned_by_environment("auth", &json!({"enable_registration": true})),
            vec!["/auth/enable_registration".to_owned()]
        );
        assert!(
            layers
                .pinned_by_environment("auth", &json!({"guest_access": true}))
                .is_empty(),
            "a sibling setting the environment says nothing about is still editable"
        );
    }

    #[test]
    fn a_patch_is_validated_against_the_configuration_it_would_produce() {
        let layers = layers();
        let err = layers
            .resolve_with_patch("server", &json!({"public_baseurl": "ftp://nope"}))
            .unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        // ... and the real configuration was not touched by the attempt.
        assert_eq!(layers.resolve().unwrap().config.server.public_baseurl, None);
    }

    #[test]
    fn a_patch_that_resolves_cleanly_produces_the_new_value() {
        let resolved = layers()
            .resolve_with_patch(
                "server",
                &json!({"public_baseurl": "https://matrix.example.org"}),
            )
            .unwrap();
        assert_eq!(
            resolved.config.server.public_baseurl.as_deref(),
            Some("https://matrix.example.org")
        );
    }

    /// A patch to a bootstrap setting cannot take effect through the database, so a candidate
    /// that includes one resolves exactly as if it did not.
    #[test]
    fn a_patch_to_a_bootstrap_setting_does_not_change_the_resolved_value() {
        let layers = layers();
        assert_eq!(
            Layers::bootstrap_in_patch("server", &json!({"server_name": "new.example"})),
            vec!["/server/server_name".to_owned()]
        );
        let resolved = layers
            .resolve_with_patch("server", &json!({"server_name": "new.example"}))
            .unwrap();
        assert_eq!(resolved.config.server.server_name, "example.org");
    }

    /// The `Default` impl exists because a derived one is a trap: `Value::Null` as a layer is an
    /// RFC 7396 whole-document replacement, so one null layer erases every other one.
    #[test]
    fn default_layers_resolve_to_the_schema_defaults_rather_than_erasing_everything() {
        let layers = Layers {
            file: Some(FileLayer {
                path: PathBuf::from("/etc/myelin/homeserver.yaml"),
                document: json!({"server": {"server_name": "example.org"}}),
            }),
            ..Layers::default()
        };
        assert_eq!(
            layers.resolve().unwrap().config.server.server_name,
            "example.org",
            "an empty database and environment must leave the file's values alone"
        );
    }

    /// What a reset actually means, end to end: the database stops setting the value, and
    /// whatever layer is underneath speaks again. It does *not* leave a tombstone that suppresses
    /// the file -- a layer is a document, not a patch, and the store merges the null away rather
    /// than storing it (`ConfigStore::patch_section`).
    #[test]
    fn resetting_a_setting_hands_it_back_to_the_layer_underneath() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let store = crate::store::ConfigStore::open(backend).unwrap();
        let file = FileLayer {
            path: PathBuf::from("/etc/myelin/homeserver.yaml"),
            document: json!({
                "server": {"server_name": "example.org"},
                "auth": {"enable_registration": false},
            }),
        };

        store
            .patch_section("auth", &json!({"enable_registration": true}), None, 1, None)
            .unwrap();
        let layers = Layers {
            file: Some(file.clone()),
            ..Layers::default()
        }
        .with_database(store.load().unwrap().document);
        assert!(layers.resolve().unwrap().config.auth.enable_registration);

        store
            .patch_section("auth", &json!({"enable_registration": null}), None, 2, None)
            .unwrap();
        let layers = Layers {
            file: Some(file),
            ..Layers::default()
        }
        .with_database(store.load().unwrap().document);
        let resolved = layers.resolve().unwrap();
        assert!(
            !resolved.config.auth.enable_registration,
            "the file sets this and the database no longer does, so the file's value stands"
        );
        assert_eq!(resolved.origin("/auth/enable_registration"), Origin::File);
    }

    /// A database written before decision 0010 may still hold a replica's listeners and mesh port.
    /// They are ignored: the file (or the schema default) says what this process binds.
    #[test]
    fn the_database_never_supplies_a_bootstrap_setting() {
        let layers = Layers {
            file: Some(FileLayer {
                path: PathBuf::from("/etc/myelin/homeserver.yaml"),
                document: json!({
                    "server": {"server_name": "example.org"},
                    "cluster": {"mesh": {"port": 9449}},
                }),
            }),
            database: json!({
                "server": {"server_name": "stale.example", "admin_contact": "a@b"},
                "listeners": {"listeners": [{"port": 18008, "bind_addresses": ["::"], "resources": ["client"]}]},
                "cluster": {"mesh": {"port": 18449}, "lease_ttl": "20s"},
            }),
            environment: json!({}),
        };
        let resolved = layers.resolve().unwrap();
        assert_eq!(resolved.config.server.server_name, "example.org");
        assert_eq!(resolved.config.cluster.mesh.port, 9449);
        assert_eq!(
            resolved.config.listeners,
            crate::ListenersConfig::default(),
            "the stored listeners are ignored; nothing else sets them, so the default stands"
        );
        assert_eq!(resolved.origin("/cluster/mesh/port"), Origin::File);
        assert_eq!(resolved.origin("/server/server_name"), Origin::File);
        // Administered settings in the same sections still come from the database.
        assert_eq!(resolved.origin("/server/admin_contact"), Origin::Database);
        assert_eq!(resolved.origin("/cluster/lease_ttl"), Origin::Database);
    }

    /// The bug decision 0010 fixes, end to end through the store: two replicas boot on one
    /// database, each with its own file. Whichever seeds first, and in whichever order they
    /// restart, each resolves its own listeners and mesh port, never the other's.
    #[test]
    fn two_replicas_seeding_one_database_keep_their_own_listeners_and_mesh_port() {
        let backend = hs_kv::memory::MemoryBackend::new();
        let store = crate::store::ConfigStore::open(backend).unwrap();
        let replica = |client_port: u16, mesh_port: u16, name: &str| FileLayer {
            path: PathBuf::from(format!("/etc/myelin/{name}.yaml")),
            document: json!({
                "server": {"server_name": "example.org"},
                "listeners": {"listeners": [{
                    "port": client_port,
                    "bind_addresses": ["127.0.0.1"],
                    "resources": ["client", "federation", "health"],
                }]},
                "cluster": {
                    "single_node": false,
                    "mesh": {"port": mesh_port, "advertise_address": format!("{name}.local")},
                },
                "auth": {"enable_registration": true},
            }),
        };
        let a = replica(18008, 18449, "replica-a");
        let b = replica(28008, 28449, "replica-b");

        assert!(store.seed(&a.document, "replica-a.yaml", 1).unwrap());
        assert!(
            !store.seed(&b.document, "replica-b.yaml", 2).unwrap(),
            "the second replica finds the store already seeded"
        );

        for (file, client_port, mesh_port, advertised) in [
            (b.clone(), 28008, 28449, "replica-b.local"),
            (a.clone(), 18008, 18449, "replica-a.local"),
            (b, 28008, 28449, "replica-b.local"),
        ] {
            let resolved = Layers {
                file: Some(file),
                ..Layers::default()
            }
            .with_database(store.load().unwrap().document)
            .resolve()
            .unwrap();
            assert_eq!(resolved.config.listeners.listeners[0].port, client_port);
            assert_eq!(resolved.config.cluster.mesh.port, mesh_port);
            assert_eq!(
                resolved.config.cluster.mesh.advertise_address.as_deref(),
                Some(advertised)
            );
            assert!(!resolved.config.cluster.single_node);
            assert!(
                resolved.config.auth.enable_registration,
                "the administered settings are shared"
            );
        }
    }

    #[test]
    fn with_no_layers_at_all_the_schema_defaults_stand() {
        let layers = Layers {
            file: None,
            database: json!({}),
            environment: json!({"server": {"server_name": "example.org"}}),
        };
        let resolved = layers.resolve().unwrap();
        assert_eq!(resolved.config, {
            let mut expected = Config::default();
            expected.server.server_name = "example.org".to_owned();
            expected
        });
    }
}
