//! Errors from initializing telemetry.

use thiserror::Error;

/// Errors setting up tracing, metrics, OpenTelemetry export or Sentry.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// A global tracing subscriber was already installed (for example, by a test harness or a
    /// second call to [`crate::init::init`] in the same process).
    #[error("a global tracing subscriber is already set")]
    AlreadyInitialized,

    /// The `otlp` feature is enabled and `tracing.enabled` was requested but no OTLP endpoint was
    /// configured.
    #[error("OTLP tracing was requested but no endpoint was configured")]
    MissingOtlpEndpoint,

    /// `RUST_LOG` set the log filter at startup, and it outranks the configured level.
    #[error("RUST_LOG set this process's log filter, which outranks telemetry.logging.level")]
    LogFilterPinned,

    /// The running log filter could not be replaced.
    #[error("could not replace the log filter: {0}")]
    Reload(String),

    /// Building the OTLP exporter or tracer provider failed.
    #[cfg(feature = "otlp")]
    #[error("failed to build the OTLP exporter: {0}")]
    Otlp(#[from] opentelemetry_otlp::ExporterBuildError),

    /// Initializing the Sentry client failed (a malformed DSN, for example).
    #[cfg(feature = "sentry")]
    #[error("failed to initialize Sentry: {0}")]
    Sentry(String),
}
