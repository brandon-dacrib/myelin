//! Appservice (bridge) registry bootstrap. Corresponds to Synapse's
//! `app_service_config_files`. Hot-reloadable (see [`crate::reload`]): the
//! registry itself supports hot registration through the admin API
//! (`PLAN.md` D7); this section only lists the static registration files
//! read at startup and re-scanned on reload.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

const fn default_true() -> bool {
    true
}

fn default_tracking_failure_threshold() -> u32 {
    50
}

/// Appservice registry bootstrap settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppservicesConfig {
    /// Master switch for appservice transaction delivery.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Static registration YAML files loaded at startup (and on reload).
    /// Corresponds to Synapse's `app_service_config_files`. Appservices
    /// registered later through the admin API do not need an entry here.
    #[serde(default)]
    pub registration_files: Vec<PathBuf>,
    /// Consecutive delivery failures to one appservice before it is marked
    /// unhealthy and moved to backlog-only delivery.
    #[serde(default = "default_tracking_failure_threshold")]
    pub tracking_failure_threshold: u32,
}

impl Default for AppservicesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
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
