//! How long this process took to boot, for the `listening` line and `/metrics`.
//!
//! `hs serve` measures from the start of its `serve` command to the moment every listener is
//! bound, and says whether the boot was **cold** (the store was created by this process: the very
//! first boot over a data directory or database) or warm. A cold boot used to cost about five
//! seconds on the embedded backend, nearly all of it creating Fjall keyspaces
//! (`docs/status/01-storage-engine.md`); it is now in the same range as a warm one, and
//! `hs_boot_duration_seconds{cold}` is where an operator would see it come back.

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;

/// The label of `hs_boot_duration_seconds`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct ColdLabels {
    cold: String,
}

/// `hs_boot_duration_seconds{cold="true"|"false"}`: this process's boot, from the start of
/// `hs serve` to every listener bound. Set once per process, so one series carries a value.
#[derive(Clone)]
pub struct BootMetric {
    duration: Family<ColdLabels, Gauge<f64, AtomicU64>>,
}

impl BootMetric {
    /// Registers the family into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let duration = Family::<ColdLabels, Gauge<f64, AtomicU64>>::default();
        metrics.with_registry(|registry| {
            registry.register(
                "hs_boot_duration_seconds",
                "How long this process took from the start of `hs serve` to every listener \
                 bound; cold=\"true\" when this process created the store (a first boot)",
                duration.clone(),
            );
        });
        Self { duration }
    }

    /// Records this process's boot.
    pub fn record(&self, elapsed: Duration, cold: bool) {
        self.duration
            .get_or_create(&ColdLabels {
                cold: cold.to_string(),
            })
            .set(elapsed.as_secs_f64());
    }
}

/// What the store says about a boot: whether this process created it, and how many keyspaces of
/// the backend's own it had to create (the expensive step of a cold boot on Fjall; `None` for a
/// backend that does not count them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreBoot {
    /// The store was created by this process.
    pub cold: bool,
    /// Fjall keyspaces created by this process (`hs_kv::fjall_backend::FjallBackend::
    /// fjall_keyspaces_created`), or `None` on PostgreSQL.
    pub keyspaces_created: Option<usize>,
    /// `hs-kv` keyspaces this process opened, where the backend counts them.
    pub keyspaces_opened: Option<usize>,
}

impl StoreBoot {
    /// What the opened store says so far. Call it once the server has opened every store
    /// (after `crate::serve::spawn_serve_with_storage`), from a handle taken before the storage
    /// was handed over; `cold` is [`crate::bootstrap::Booted::cold`].
    #[must_use]
    pub fn read(storage: Option<&hs_kv::fjall_backend::FjallBackend>, cold: bool) -> Self {
        Self {
            cold,
            keyspaces_created: storage
                .map(hs_kv::fjall_backend::FjallBackend::fjall_keyspaces_created),
            keyspaces_opened: storage.map(hs_kv::fjall_backend::FjallBackend::keyspaces_opened),
        }
    }
}

/// The `listening` line, once per bound address, with what the boot cost:
/// `listening addr=... boot_ms=... cold=... keyspaces_created=... keyspaces_opened=...` (the
/// last two on the embedded backend only).
pub fn log_listening(addr: &std::net::SocketAddr, elapsed: Duration, store: &StoreBoot) {
    let boot_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    match (store.keyspaces_created, store.keyspaces_opened) {
        (Some(keyspaces_created), Some(keyspaces_opened)) => tracing::info!(
            %addr,
            boot_ms,
            cold = store.cold,
            keyspaces_created,
            keyspaces_opened,
            "listening"
        ),
        _ => tracing::info!(%addr, boot_ms, cold = store.cold, "listening"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_duration_is_exported_with_its_cold_label() {
        let metrics = hs_telemetry::metrics::Metrics::new();
        let boot = BootMetric::register(&metrics);
        boot.record(Duration::from_millis(1500), true);
        let text = metrics.encode_to_string().unwrap();
        assert!(
            text.contains("hs_boot_duration_seconds{cold=\"true\"} 1.5"),
            "{text}"
        );
    }
}
