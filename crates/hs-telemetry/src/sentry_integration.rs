//! Sentry error and panic reporting, behind the `sentry` feature. Fed from `tracing` events via
//! `sentry`'s own `tracing` integration (the `sentry::integrations::tracing::layer()` function
//! used from [`crate::init::init`]), so a `tracing::error!` anywhere in the workspace reaches
//! Sentry without a second instrumentation pass.

/// Initializes the global Sentry client. The returned guard must be kept alive for the life of
/// the process (see [`crate::init::Guard`]); dropping it flushes queued events and shuts the
/// transport down.
pub fn init_client(dsn: &str, environment: &str) -> sentry::ClientInitGuard {
    // `ClientOptions` is `#[non_exhaustive]`, so it cannot be built with a struct-literal even
    // with `..Default::default()`; start from the default and mutate the fields this crate cares
    // about instead.
    let mut options = sentry::ClientOptions::default();
    options.environment = Some(environment.to_owned().into());
    options.release = sentry::release_name!();
    options.attach_stacktrace = true;
    sentry::init((dsn, options))
}
