//! Appservice (bridge) delivery settings, and the one-time import of registration files.
//!
//! A bridge is registered, changed and removed through the admin API (`appservices.*`) and the
//! web interface's Bridges section (decision 0010); the registry that holds it is in the
//! database. `registration_files` corresponds to Synapse's `app_service_config_files`, and is a
//! migration path only: each listed file is imported into the registry once, on the first start
//! that sees it, and recorded as imported ([`crate::store::ImportRecord`]). A file still listed
//! after that is not read again, so a bridge edited or removed in the interface stays that way
//! across restarts. It is a bootstrap setting ([`crate::bootstrap`]): it names files on this
//! process's filesystem, and is never stored in the database.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

fn default_tracking_failure_threshold() -> u32 {
    50
}

/// Appservice delivery settings, and registration files to import once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppservicesConfig {
    /// Registration YAML files to import into the appservice registry, once each. Corresponds to
    /// Synapse's `app_service_config_files`, and exists for migrating from it: the first start
    /// that sees a file imports it (unless the registry already has an appservice with that id)
    /// and records the import; every later start skips it, even if the file has changed. From
    /// then on the bridge is managed in the Bridges section of the interface. Bootstrap only: set
    /// in the bootstrap file or the environment, never stored in the database.
    #[serde(default)]
    pub registration_files: Vec<PathBuf>,
    /// How many deliveries in a row may fail before a bridge is marked unhealthy and events for
    /// it are kept in its backlog, to be replayed when it is back, instead of being retried one
    /// by one. Lower notices a dead bridge sooner; higher rides out a bridge that restarts
    /// often. At least 1.
    #[serde(default = "default_tracking_failure_threshold")]
    pub tracking_failure_threshold: u32,
}

impl Default for AppservicesConfig {
    fn default() -> Self {
        Self {
            registration_files: Vec::new(),
            tracking_failure_threshold: default_tracking_failure_threshold(),
        }
    }
}

impl Validate for AppservicesConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.tracking_failure_threshold == 0 {
            errors.push(
                format!("{prefix}.tracking_failure_threshold"),
                "must be at least 1 (0 would mark every appservice unhealthy immediately)",
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (i, f) in self.registration_files.iter().enumerate() {
            if !seen.insert(f.clone()) {
                errors.push(
                    format!("{prefix}.registration_files[{i}]"),
                    format!("{f:?} is listed more than once"),
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        AppservicesConfig::default().validate("appservices", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_zero_threshold() {
        let mut cfg = AppservicesConfig::default();
        cfg.tracking_failure_threshold = 0;
        let mut errors = ValidationErrors::new();
        cfg.validate("appservices", &mut errors);
        assert_eq!(errors.0.len(), 1);
    }

    #[test]
    fn rejects_duplicate_registration_file() {
        let mut cfg = AppservicesConfig::default();
        cfg.registration_files = vec![PathBuf::from("a.yaml"), PathBuf::from("a.yaml")];
        let mut errors = ValidationErrors::new();
        cfg.validate("appservices", &mut errors);
        assert_eq!(errors.0.len(), 1);
    }
}
