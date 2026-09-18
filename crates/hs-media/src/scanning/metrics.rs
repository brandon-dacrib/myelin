//! Metrics for content scanning (RFC section 8), registered into the shared
//! `hs_telemetry::metrics::Metrics` registry per `docs/decisions/0004-telemetry-conventions.md`
//! (mirrored in `crates/hs-telemetry/src/metrics.rs`'s module doc, which is normative).
//!
//! - `hs_media_scans_total{provider,verdict,source}` — **a cache hit is never counted here**
//!   (RFC section 6: "a cache hit must never be reported as a scan in metrics, or the metrics
//!   lie about scanner load"). Only [`ScanMetrics::record_scan`] increments it;
//!   [`ScanMetrics::record_cache_hit`] is a separate counter.
//! - `hs_media_scan_duration_seconds{provider,verdict,source}`.
//! - `hs_media_scan_cache_hits_total{provider}`.
//! - `hs_media_scan_errors_total{provider,kind}`.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

/// Labels shared by the scan counters and the duration histogram.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct ScanLabels {
    /// The provider id (`ContentScanner::id`, e.g. `"icap"`, `"http"`, `"none"`).
    pub provider: String,
    /// The verdict, as a low-cardinality label: `"clean"`, `"infected"`, `"unscannable"`,
    /// `"replaced"`, `"error"`.
    pub verdict: String,
    /// Where the content came from (`ScanSourceKind::as_str`): `"local"`, `"appservice"`,
    /// `"federation"`.
    pub source: String,
}

/// Labels for the cache-hit counter (no `verdict`/`source`: a cache hit's verdict and source
/// are already implied by what was cached, and keeping this counter's cardinality minimal matters
/// more than that detail).
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct CacheHitLabels {
    /// The provider id.
    pub provider: String,
}

/// Labels for the error counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct ScanErrorLabels {
    /// The provider id.
    pub provider: String,
    /// A low-cardinality error kind: `"timeout"`, `"unavailable"`, `"protocol"`, `"other"`.
    pub kind: String,
}

/// The content-scanning metric families.
#[derive(Clone)]
pub struct ScanMetrics {
    /// `hs_media_scans_total{provider,verdict,source}`.
    pub scans_total: Family<ScanLabels, Counter>,
    /// `hs_media_scan_duration_seconds{provider,verdict,source}`.
    pub scan_duration_seconds: Family<ScanLabels, Histogram>,
    /// `hs_media_scan_cache_hits_total{provider}`.
    pub cache_hits_total: Family<CacheHitLabels, Counter>,
    /// `hs_media_scan_errors_total{provider,kind}`.
    pub errors_total: Family<ScanErrorLabels, Counter>,
}

impl ScanMetrics {
    /// Registers this crate's scanning metric families into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let scans_total = Family::<ScanLabels, Counter>::default();
        let scan_duration_seconds = Family::<ScanLabels, Histogram>::new_with_constructor(|| {
            // Scans range from single-digit milliseconds (a cached/skip path) to tens of seconds
            // (a slow cloud API); seed accordingly rather than the crate's web-latency default.
            Histogram::new(exponential_buckets(0.005, 2.0, 14))
        });
        let cache_hits_total = Family::<CacheHitLabels, Counter>::default();
        let errors_total = Family::<ScanErrorLabels, Counter>::default();

        metrics.with_registry(|registry: &mut Registry| {
            // Counters are registered without the `_total` suffix; `prometheus_client`'s text
            // encoder appends it (see `hs_telemetry::metrics`'s module doc — this bug shipped
            // once already for `hs_http_requests_total` and is worth re-stating at every call
            // site).
            registry.register(
                "hs_media_scans",
                "Content scans performed, by provider, verdict and source. Excludes cache hits.",
                scans_total.clone(),
            );
            registry.register(
                "hs_media_scan_duration_seconds",
                "Content scan duration in seconds, by provider, verdict and source.",
                scan_duration_seconds.clone(),
            );
            registry.register(
                "hs_media_scan_cache_hits",
                "Verdict cache hits, by provider. Never counted in hs_media_scans_total.",
                cache_hits_total.clone(),
            );
            registry.register(
                "hs_media_scan_errors",
                "Scan failures (timeout, connection error, malformed response), by provider and kind.",
                errors_total.clone(),
            );
        });

        Self {
            scans_total,
            scan_duration_seconds,
            cache_hits_total,
            errors_total,
        }
    }

    /// A standalone instance with its own private registry, for tests and for any caller that
    /// does not yet have a shared `hs_telemetry::metrics::Metrics` to register into.
    #[must_use]
    pub fn standalone() -> Self {
        Self::register(&hs_telemetry::metrics::Metrics::new())
    }

    /// Records one completed scan (never a cache hit — see [`ScanMetrics::record_cache_hit`]).
    pub fn record_scan(&self, provider: &str, verdict: &str, source: &str, duration_secs: f64) {
        let labels = ScanLabels {
            provider: provider.to_string(),
            verdict: verdict.to_string(),
            source: source.to_string(),
        };
        self.scans_total.get_or_create(&labels).inc();
        self.scan_duration_seconds
            .get_or_create(&labels)
            .observe(duration_secs);
    }

    /// Records a verdict cache hit. Deliberately not routed through [`ScanMetrics::record_scan`].
    pub fn record_cache_hit(&self, provider: &str) {
        self.cache_hits_total
            .get_or_create(&CacheHitLabels {
                provider: provider.to_string(),
            })
            .inc();
    }

    /// Records a scan failure.
    pub fn record_error(&self, provider: &str, kind: &str) {
        self.errors_total
            .get_or_create(&ScanErrorLabels {
                provider: provider.to_string(),
                kind: kind.to_string(),
            })
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_and_cache_hit_are_separate_counters() {
        let metrics = ScanMetrics::standalone();
        metrics.record_scan("icap", "clean", "local", 0.01);
        metrics.record_cache_hit("icap");

        let text = hs_telemetry::metrics::Metrics::new().encode_to_string().unwrap();
        // (This standalone registry is separate from the one above; the real assertion is that
        // the two families are independent counters, checked directly below.)
        let _ = text;

        let scan_labels = ScanLabels {
            provider: "icap".into(),
            verdict: "clean".into(),
            source: "local".into(),
        };
        assert_eq!(metrics.scans_total.get_or_create(&scan_labels).get(), 1);
        assert_eq!(
            metrics
                .cache_hits_total
                .get_or_create(&CacheHitLabels {
                    provider: "icap".into()
                })
                .get(),
            1
        );
    }

    #[test]
    fn metric_names_render_without_doubled_total_suffix() {
        let telemetry = hs_telemetry::metrics::Metrics::new();
        let scan_metrics = ScanMetrics::register(&telemetry);
        scan_metrics.record_scan("icap", "clean", "local", 0.01);
        scan_metrics.record_cache_hit("icap");
        scan_metrics.record_error("icap", "timeout");

        let text = telemetry.encode_to_string().unwrap();
        assert!(text.contains("hs_media_scans_total{"));
        assert!(!text.contains("hs_media_scans_total_total"));
        assert!(text.contains("hs_media_scan_cache_hits_total{"));
        assert!(text.contains("hs_media_scan_errors_total{"));
        assert!(text.contains("hs_media_scan_duration_seconds"));
    }
}
