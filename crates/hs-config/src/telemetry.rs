//! Metrics, tracing, logging and error reporting. Hot-reloadable (see
//! [`crate::reload`]).

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::error::{Validate, ValidationErrors};
use crate::secret::{SecretString, resolve_secret_pair};

const fn default_true() -> bool {
    true
}

fn default_sample_ratio() -> f64 {
    0.1
}

fn default_environment() -> String {
    "production".to_owned()
}

/// Prometheus metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Serve `/metrics` on a listener with the `metrics` resource.
    #[serde(default)]
    pub enabled: bool,
    /// Additionally export Synapse-named metrics (`synapse_*`) alongside
    /// the native `hs_*` ones, for dashboards built against Synapse.
    #[serde(default = "default_true")]
    pub synapse_compat_names: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            synapse_compat_names: true,
        }
    }
}

/// OpenTelemetry distributed tracing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TracingConfig {
    /// Emit spans at all.
    #[serde(default)]
    pub enabled: bool,
    /// OTLP collector endpoint. Required when `enabled` is true.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// Fraction of traces sampled, `0.0` to `1.0`.
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: None,
            sample_ratio: default_sample_ratio(),
        }
    }
}

/// Log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Everything, including per-request tracing detail.
    Trace,
    /// Verbose diagnostic output.
    Debug,
    /// Normal operational messages.
    Info,
    /// Recoverable problems worth an operator's attention.
    Warn,
    /// Failures.
    Error,
}

fn default_log_level() -> LogLevel {
    LogLevel::Info
}

/// Structured logging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// Minimum level emitted.
    #[serde(default = "default_log_level")]
    pub level: LogLevel,
    /// Emit JSON lines instead of human-readable text. Corresponds to
    /// Synapse's `log_config` handler choice, simplified to a boolean since
    /// this server has one structured schema rather than arbitrary
    /// `logging.config.dictConfig` handlers.
    #[serde(default)]
    pub json: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            json: false,
        }
    }
}

/// Sentry error reporting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SentryConfig {
    /// Inline DSN. Prefer `dsn_file`.
    #[serde(default)]
    pub dsn: SecretString,
    /// Path to a file containing the DSN.
    #[serde(default)]
    pub dsn_file: Option<PathBuf>,
    /// Environment tag attached to events.
    #[serde(default = "default_environment")]
    pub environment: String,
}

/// Telemetry: metrics, tracing, logging and error reporting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Prometheus metrics.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// OpenTelemetry tracing.
    #[serde(default)]
    pub tracing: TracingConfig,
    /// Structured logging.
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Sentry error reporting; absent disables it.
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
}

impl Validate for TelemetryConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if !(0.0..=1.0).contains(&self.tracing.sample_ratio)
            || !self.tracing.sample_ratio.is_finite()
        {
            errors.push(
                format!("{prefix}.tracing.sample_ratio"),
                format!(
                    "must be between 0.0 and 1.0, got {}",
                    self.tracing.sample_ratio
                ),
            );
        }
        if self.tracing.enabled && self.tracing.otlp_endpoint.is_none() {
            errors.push(
                format!("{prefix}.tracing.otlp_endpoint"),
                "must be set when tracing.enabled is true",
            );
        }
    }
}

impl TelemetryConfig {
    /// Resolves any `*_file` secrets this section carries.
    pub(crate) fn resolve_secrets(&mut self, prefix: &str) -> Result<(), ConfigError> {
        if let Some(sentry) = &mut self.sentry {
            resolve_secret_pair(
                &format!("{prefix}.sentry.dsn"),
                &mut sentry.dsn,
                &sentry.dsn_file,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        TelemetryConfig::default().validate("telemetry", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_out_of_range_sample_ratio() {
        let mut cfg = TelemetryConfig::default();
        cfg.tracing.sample_ratio = 1.5;
        let mut errors = ValidationErrors::new();
        cfg.validate("telemetry", &mut errors);
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.path == "telemetry.tracing.sample_ratio")
        );
    }

    #[test]
    fn rejects_tracing_enabled_without_endpoint() {
        let mut cfg = TelemetryConfig::default();
        cfg.tracing.enabled = true;
        let mut errors = ValidationErrors::new();
        cfg.validate("telemetry", &mut errors);
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.path == "telemetry.tracing.otlp_endpoint")
        );
    }
}
