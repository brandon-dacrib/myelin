//! Metrics, tracing, logging and error reporting. Read at startup, but for the log level (see
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

/// Prometheus metrics: counters and timings a monitoring system scrapes from `/metrics`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Whether to serve `/metrics` (on a listener that has the `metrics` resource) for Prometheus
    /// or a compatible scraper. Costs nearly nothing; leave it on. Off, there is no way to see
    /// the server's load, queues or errors over time.
    #[serde(default)]
    pub enabled: bool,
    /// Also export metrics under Synapse's names (`synapse_*`) beside this server's own `hs_*`
    /// ones, so Grafana dashboards built for Synapse keep working after a migration. Doubles
    /// the series scraped for those metrics.
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

/// OpenTelemetry tracing: a timeline of each request across this server's parts and replicas,
/// sent to a collector such as Jaeger or Tempo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TracingConfig {
    /// Whether to send traces. Off by default; turn it on with a collector to send them to, to
    /// find where slow requests spend their time. Sending costs a little CPU and network per
    /// sampled request.
    #[serde(default)]
    pub enabled: bool,
    /// The OTLP collector traces are sent to (`http://otel-collector:4317`). Required when
    /// tracing is on.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// The share of requests traced, from `0.0` (none) to `1.0` (every one). A small share is
    /// enough to find slow paths on a busy server and keeps the cost down.
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

/// Logging: what the server writes to its standard output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// Minimum level emitted. A change applies to the running server at
    /// once, unless `RUST_LOG` set the filter when it started.
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

/// Sentry error reporting: errors and panics sent to a Sentry project, with their context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SentryConfig {
    /// The Sentry project's DSN (the URL Sentry gives a project), which turns reporting on.
    /// Prefer `dsn_file`.
    #[serde(default)]
    pub dsn: SecretString,
    /// Path to a file holding the DSN, read in place of `dsn`.
    #[serde(default)]
    pub dsn_file: Option<PathBuf>,
    /// The environment each report is tagged with (`production`, `staging`), to tell
    /// deployments apart in one Sentry project.
    #[serde(default = "default_environment")]
    pub environment: String,
}

/// Telemetry: what the server tells an operator about itself -- metrics, traces, logs and
/// error reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Prometheus metrics: whether `/metrics` is served, and under which names.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// OpenTelemetry tracing: whether requests are traced, how many, and where traces go.
    #[serde(default)]
    pub tracing: TracingConfig,
    /// Logging: how much is logged, and as text or JSON lines.
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Sentry error reporting. Unset by default, which sends nothing anywhere.
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
