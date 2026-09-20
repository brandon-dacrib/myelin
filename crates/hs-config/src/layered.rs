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
#[derive(Debug, Clone, Default)]
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

impl Layers {
    /// The layers for a server booting with no database yet: file and environment only.
    #[must_use]
    pub fn bootstrap<I>(file: Option<FileLayer>, env_vars: I) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        Self {
            file,
            database: Value::Object(Map::new()),
            environment: crate::env::override_document(env_vars),
        }
    }

    /// The same layers with `database` swapped in -- what a server does once its store is open.
    #[must_use]
    pub fn with_database(mut self, database: Value) -> Self {
        self.database = database;
        self
    }

    /// The layers in precedence order, lowest first.
    fn ordered(&self) -> Vec<(Origin, &Value)> {
        let mut out = Vec::with_capacity(3);
        if let Some(file) = &self.file {
            out.push((Origin::File, &file.document));
        }
        out.push((Origin::Database, &self.database));
        out.push((Origin::Environment, &self.environment));
        out
    }

    /// Merges every layer into one document, without deserializing or validating it.
    #[must_use]
    pub fn merged(&self) -> Value {
        document::merge_all(self.ordered().into_iter().map(|(_, doc)| doc))
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
        Ok(Resolved {
            config,
            document,
            origins: document::origins(self.ordered()),
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
            .resolve_with_patch("server", &json!({"server_name": ""}))
            .unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        // ... and the real configuration was not touched by the attempt.
        assert_eq!(
            layers.resolve().unwrap().config.server.server_name,
            "example.org"
        );
    }

    #[test]
    fn a_patch_that_resolves_cleanly_produces_the_new_value() {
        let resolved = layers()
            .resolve_with_patch("server", &json!({"server_name": "new.example"}))
            .unwrap();
        assert_eq!(resolved.config.server.server_name, "new.example");
    }

    #[test]
    fn with_no_layers_at_all_the_schema_defaults_stand() {
        let layers = Layers {
            file: None,
            database: json!({"server": {"server_name": "example.org"}}),
            environment: json!({}),
        };
        let resolved = layers.resolve().unwrap();
        assert_eq!(resolved.config, {
            let mut expected = Config::default();
            expected.server.server_name = "example.org".to_owned();
            expected
        });
    }
}
