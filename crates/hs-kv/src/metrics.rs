//! Process-wide metrics of this crate: what the PostgreSQL backend's commit flushes (RFC 0021).
//!
//! Process-wide statics, like `hs_user::metrics` and `hs_room::metrics`: a backend is opened far
//! from any registry, and a metric is only atomics. [`register_metrics`] puts them on
//! `/metrics`; `hs-cli` calls it once at startup. The same counts are available per backend,
//! without a registry, from `PostgresBackend::flush_stats`.

use std::sync::LazyLock;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::histogram::Histogram;

/// `hs_kv_postgres_flush_writes`: buffered writes (puts and deletes) one commit flushed. A
/// room update's fan-out writes about three per member (decision 0026), so a batch of 100
/// members is one observation near 300.
static FLUSH_WRITES: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::new([
        1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0,
    ])
});

/// `hs_kv_postgres_flush_duration_seconds`: how long flushing one commit's writes took, from
/// the first statement to the last, before `COMMIT`.
static FLUSH_DURATION: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::new([
        0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
        5.0,
    ])
});

/// `hs_kv_postgres_flush_statements_total`: write statements sent by flushes. With bulk flushes
/// this grows by one per table and kind (upsert, delete) per commit, plus one per extra chunk
/// of `FLUSH_CHUNK_ROWS` rows; before RFC 0021 it grew by one per write.
static FLUSH_STATEMENTS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Records one flush: `writes` buffered writes sent as `statements` statements in `elapsed`.
pub(crate) fn observe_flush(writes: usize, statements: usize, elapsed: Duration) {
    FLUSH_WRITES.observe(writes as f64);
    FLUSH_DURATION.observe(elapsed.as_secs_f64());
    FLUSH_STATEMENTS.inc_by(statements as u64);
}

/// Registers this crate's metrics into `registry`: `hs_kv_postgres_flush_writes`,
/// `hs_kv_postgres_flush_duration_seconds` and `hs_kv_postgres_flush_statements_total`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_kv_postgres_flush_writes",
        "Buffered writes (puts and deletes) one PostgreSQL commit flushed in bulk",
        FLUSH_WRITES.clone(),
    );
    registry.register(
        "hs_kv_postgres_flush_duration_seconds",
        "Time one PostgreSQL commit took to flush its buffered writes, before COMMIT",
        FLUSH_DURATION.clone(),
    );
    // Counters are registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_kv_postgres_flush_statements",
        "Write statements PostgreSQL commits sent while flushing buffered writes",
        FLUSH_STATEMENTS.clone(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metrics_register_and_encode() {
        let mut registry = prometheus_client::registry::Registry::default();
        register_metrics(&mut registry);
        observe_flush(300, 3, Duration::from_millis(2));
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).expect("encode");
        assert!(out.contains("hs_kv_postgres_flush_writes_bucket"), "{out}");
        assert!(
            out.contains("hs_kv_postgres_flush_duration_seconds_bucket"),
            "{out}"
        );
        assert!(
            out.contains("hs_kv_postgres_flush_statements_total"),
            "{out}"
        );
    }
}
