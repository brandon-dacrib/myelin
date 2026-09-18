//! Tracing subscriber initialization: structured JSON logs, a Synapse-like plain text format,
//! level filtering, and (behind feature flags) OpenTelemetry OTLP export and Sentry error
//! reporting layered onto the same subscriber.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::error::TelemetryError;

/// Minimum level emitted, independent of `RUST_LOG`. [`Options::level`] sets the default filter
/// directive; `RUST_LOG`, if set in the environment, always takes precedence (the standard
/// `tracing-subscriber` behavior), so an operator can raise verbosity for one deployment without
/// editing `hs-config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// Everything, including per-request tracing detail.
    Trace,
    /// Verbose diagnostic output.
    Debug,
    /// Normal operational messages.
    #[default]
    Info,
    /// Recoverable problems worth an operator's attention.
    Warn,
    /// Failures.
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// The log line format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// One JSON object per line: `{"timestamp":..., "level":..., "target":..., "fields": {...},
    /// "span": {...}}`. The default for production and for any deployment feeding logs to a
    /// collector (Loki, an ELK stack, CloudWatch Logs).
    #[default]
    Json,
    /// A Synapse-like plain text line:
    /// `2026-09-18 12:00:00,000 - hs_room::actor - INFO - message key=value key2=value2`. Not
    /// byte-identical to Synapse's own `logging.Formatter` output (this project has one
    /// structured schema, not arbitrary handler configuration — see
    /// `hs_config::telemetry::LoggingConfig`'s doc comment), but close enough that an operator's
    /// eye, and simple `grep`/`awk` log-scraping, keeps working across a migration.
    SynapseText,
}

/// Options controlling [`init`]. Built from `hs_config::TelemetryConfig` by the caller (this
/// crate does not depend on `hs-config`, keeping the dependency direction one-way: `hs-cli`
/// bridges the two).
#[derive(Debug, Clone)]
pub struct Options {
    /// Minimum level, used to build the default filter directive.
    pub level: Level,
    /// JSON or Synapse-like text output.
    pub format: LogFormat,
    /// OTLP trace export. Only usable when this crate is built with the `otlp` feature; ignored
    /// (with a warning logged after `init` returns) otherwise.
    pub otlp_endpoint: Option<String>,
    /// Fraction of traces sampled when OTLP export is enabled, `0.0` to `1.0`.
    pub otlp_sample_ratio: f64,
    /// Sentry DSN. Only usable when this crate is built with the `sentry` feature; ignored (with
    /// a warning logged after `init` returns) otherwise.
    pub sentry_dsn: Option<String>,
    /// Sentry environment tag.
    pub sentry_environment: String,
    /// The service name attached to every span/log line and to Sentry events (`service.name` in
    /// OTLP resource attributes).
    pub service_name: String,
    /// The service version (`CARGO_PKG_VERSION` of the binary), attached the same way.
    pub service_version: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            level: Level::default(),
            format: LogFormat::default(),
            otlp_endpoint: None,
            otlp_sample_ratio: 0.1,
            sentry_dsn: None,
            sentry_environment: "production".to_owned(),
            service_name: "hs".to_owned(),
            service_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Held for the lifetime of the process. Dropping it flushes and shuts down whichever optional
/// exporters were started (OTLP's batch span processor, Sentry's transport queue); the
/// [`std::mem::forget`]-free RAII pattern the `opentelemetry`/`sentry` crates expect.
#[must_use = "dropping the guard immediately shuts telemetry back down"]
pub struct Guard {
    #[cfg(feature = "otlp")]
    otlp_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    // Held only for its `Drop` side effect (flushing Sentry's transport queue); never read.
    #[cfg(feature = "sentry")]
    #[allow(dead_code)]
    sentry_guard: Option<sentry::ClientInitGuard>,
    // Keeps the struct non-empty (and this field used) when neither optional feature is
    // compiled in, so the struct's shape does not change across feature combinations in a way
    // that would otherwise trip an "unused" warning on the whole type.
    _private: (),
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.otlp_provider.take() {
            let _ = provider.shutdown();
        }
        // sentry::ClientInitGuard flushes and shuts down its transport on drop already.
    }
}

/// Installs the global [`tracing`] subscriber: an env-filter built from [`Options::level`]
/// (overridable by `RUST_LOG`), a JSON or Synapse-like text formatting layer, and — when this
/// crate is compiled with the matching feature and the option is set — an OpenTelemetry OTLP
/// export layer and/or a Sentry layer feeding from the same `tracing` events.
///
/// Call this exactly once, as early as possible in `main`. Returns a [`Guard`]; keep it alive
/// (bind it to a variable in `main`, not `let _ =`) until the process is ready to exit, so
/// buffered spans and events are flushed rather than dropped.
///
/// # Errors
/// Returns [`TelemetryError::AlreadyInitialized`] if a global subscriber is already set (for
/// example, called twice, or after a test harness installed one). Returns
/// [`TelemetryError::MissingOtlpEndpoint`] if `otlp_endpoint` is `None` while attempting OTLP
/// setup (callers should check this themselves before calling, matching how
/// `hs_config::telemetry::TelemetryConfig` validation already requires an endpoint whenever
/// tracing is enabled).
pub fn init(options: &Options) -> Result<Guard, TelemetryError> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(options.level.as_str()));

    let fmt_layer_json = matches!(options.format, LogFormat::Json).then(|| {
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
    });
    let fmt_layer_text = matches!(options.format, LogFormat::SynapseText).then(|| {
        // Synapse's own logging.Formatter default is
        // `%(asctime)s - %(name)s - %(levelname)s - %(message)s` with `asctime` formatted as
        // `YYYY-MM-DD HH:MM:SS,mmm`. `time::UtcTime` with this format description reproduces the
        // timestamp shape; the level/target/message layout is `tracing-subscriber`'s own default
        // full formatter, which is close enough for `grep`/`awk` log-scraping muscle memory to
        // transfer.
        tracing_subscriber::fmt::layer()
            .with_timer(tracing_subscriber::fmt::time::UtcTime::new(
                time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]:[second],[subsecond digits:3]"
                ),
            ))
            .with_target(true)
    });

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer_json)
        .with(fmt_layer_text);

    #[cfg(not(feature = "otlp"))]
    if options.otlp_endpoint.is_some() {
        eprintln!(
            "hs-telemetry: otlp_endpoint was set but this build does not have the `otlp` feature enabled; OTLP export is disabled"
        );
    }
    #[cfg(feature = "otlp")]
    let otlp_provider = if let Some(endpoint) = &options.otlp_endpoint {
        Some(crate::otlp::build_provider(options, endpoint)?)
    } else {
        None
    };
    #[cfg(feature = "otlp")]
    let otel_layer = otlp_provider.as_ref().map(crate::otlp::layer_from_provider);
    #[cfg(feature = "otlp")]
    let registry = registry.with(otel_layer);

    #[cfg(not(feature = "sentry"))]
    if options.sentry_dsn.is_some() {
        eprintln!(
            "hs-telemetry: sentry_dsn was set but this build does not have the `sentry` feature enabled; Sentry reporting is disabled"
        );
    }
    #[cfg(feature = "sentry")]
    let sentry_guard = options
        .sentry_dsn
        .as_ref()
        .map(|dsn| crate::sentry_integration::init_client(dsn, &options.sentry_environment));
    #[cfg(feature = "sentry")]
    let registry = registry.with(
        sentry_guard
            .is_some()
            .then(sentry::integrations::tracing::layer),
    );

    registry
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInitialized)?;

    Ok(Guard {
        #[cfg(feature = "otlp")]
        otlp_provider,
        #[cfg(feature = "sentry")]
        sentry_guard,
        _private: (),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_json_at_info() {
        let opts = Options::default();
        assert_eq!(opts.level, Level::Info);
        assert_eq!(opts.format, LogFormat::Json);
    }

    // `init` itself installs a *global* subscriber, so it is exercised by the `hs-cli`
    // integration test (one process, one `init` call) rather than here: a unit test that calls
    // it would either poison every other test in this crate's test binary (they share a
    // process) or silently no-op on the second call, neither of which is a meaningful
    // assertion. `tracing_subscriber::registry().with(...)` composition above is exercised by
    // construction (it must at least type-check and build) every time this module compiles.
}
