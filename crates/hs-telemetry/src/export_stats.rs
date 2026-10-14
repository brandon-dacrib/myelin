//! How trace export is going, on `/metrics`: spans exported, dropped because the export queue
//! was full, and lost because an export failed, plus the spans waiting in the queue.
//!
//! Process-wide, like the exporter. The metrics exist (at zero) in a build without the `otlp`
//! feature or with tracing off, so a dashboard's query does not break when export is off.
//!
//! - `hs_telemetry_spans_exported_total`: spans the collector accepted.
//! - `hs_telemetry_spans_dropped_total`: spans finished while the queue already held
//!   [`MAX_QUEUED`] (a collector down, slow or unreachable), never queued.
//! - `hs_telemetry_spans_failed_total`: spans in batches the collector refused or that timed
//!   out.
//! - `hs_telemetry_span_export_queue`: spans finished and not yet exported or lost.
//!
//! `dropped` or `failed` growing while `exported` does not is a collector that is not taking
//! spans; the log says the same once every five minutes (`trace export to the OTLP collector is
//! failing`).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// Spans that may wait for export at once; past this a finished span is dropped. The batch
/// processor's own queue is this size too, so it never has to drop one itself.
pub const MAX_QUEUED: i64 = 4096;

static EXPORTED: LazyLock<Counter> = LazyLock::new(Counter::default);
static DROPPED: LazyLock<Counter> = LazyLock::new(Counter::default);
static FAILED: LazyLock<Counter> = LazyLock::new(Counter::default);
static QUEUED: LazyLock<Gauge> = LazyLock::new(Gauge::default);

/// One export pipeline's queue length: places taken by finished spans and given back by
/// exports. Shared by the processor that queues spans and the exporter that sends them.
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(feature = "otlp"), allow(dead_code))]
pub(crate) struct Queue(Arc<AtomicI64>);

#[cfg_attr(not(feature = "otlp"), allow(dead_code))]
impl Queue {
    /// Takes a place in the queue for one finished span: `false` (and the span counted as
    /// dropped) when the queue is full.
    pub(crate) fn try_enqueue(&self) -> bool {
        let previous = self.0.fetch_add(1, Ordering::AcqRel);
        if previous >= MAX_QUEUED {
            self.0.fetch_sub(1, Ordering::AcqRel);
            DROPPED.inc();
            return false;
        }
        QUEUED.set(previous + 1);
        true
    }

    /// Records one export of `spans` spans: exported, or lost when `ok` is `false`; their
    /// places in the queue are given back.
    pub(crate) fn exported(&self, spans: usize, ok: bool) {
        let n = spans as u64;
        if ok {
            EXPORTED.inc_by(n);
        } else {
            FAILED.inc_by(n);
        }
        let spans = i64::try_from(spans).unwrap_or(i64::MAX);
        let left = self.0.fetch_sub(spans, Ordering::AcqRel) - spans;
        QUEUED.set(left.max(0));
    }

    /// Spans in the queue now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> i64 {
        self.0.load(Ordering::Acquire)
    }
}

/// Spans exported so far in this process.
#[must_use]
pub fn spans_exported() -> u64 {
    EXPORTED.get()
}

/// Spans dropped because the export queue was full, so far in this process.
#[must_use]
pub fn spans_dropped() -> u64 {
    DROPPED.get()
}

/// Spans lost to failed exports, so far in this process.
#[must_use]
pub fn spans_failed() -> u64 {
    FAILED.get()
}

/// Registers the four metrics this module's docs list into `registry`.
pub(crate) fn register(registry: &mut Registry) {
    // Counters without `_total`: the encoder appends it (see `crate::metrics`).
    registry.register(
        "hs_telemetry_spans_exported",
        "Trace spans the OTLP collector accepted",
        EXPORTED.clone(),
    );
    registry.register(
        "hs_telemetry_spans_dropped",
        "Trace spans dropped because the export queue was full (a collector down, slow or \
         unreachable)",
        DROPPED.clone(),
    );
    registry.register(
        "hs_telemetry_spans_failed",
        "Trace spans lost because the export of their batch failed",
        FAILED.clone(),
    );
    registry.register(
        "hs_telemetry_span_export_queue",
        "Trace spans finished and waiting for export",
        QUEUED.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_drops_and_an_export_frees_places() {
        let queue = Queue::default();
        let dropped_before = spans_dropped();
        for _ in 0..MAX_QUEUED {
            assert!(queue.try_enqueue());
        }
        assert!(!queue.try_enqueue(), "the queue is full");
        assert!(spans_dropped() > dropped_before);
        assert_eq!(queue.len(), MAX_QUEUED);
        let (exported, failed) = (spans_exported(), spans_failed());
        queue.exported(512, true);
        queue.exported(10, false);
        assert!(spans_exported() >= exported + 512);
        assert!(spans_failed() >= failed + 10);
        assert_eq!(queue.len(), MAX_QUEUED - 522);
        assert!(queue.try_enqueue());
    }

    #[test]
    fn the_metrics_are_on_the_registry() {
        let mut registry = Registry::default();
        register(&mut registry);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        for name in [
            "hs_telemetry_spans_exported_total",
            "hs_telemetry_spans_dropped_total",
            "hs_telemetry_spans_failed_total",
            "hs_telemetry_span_export_queue",
        ] {
            assert!(out.contains(name), "{name} in {out}");
        }
    }
}
