//! Process-wide metrics of this crate: what the PostgreSQL backend's commit flushes (RFC 0021)
//! and the Fjall backend's write buffer against its cap (RFC 0024).
//!
//! Process-wide statics, like `hs_user::metrics` and `hs_room::metrics`: a backend is opened far
//! from any registry, and a metric is only atomics. [`register_metrics`] puts them on
//! `/metrics`; `hs-cli` calls it once at startup. The same counts are available per backend,
//! without a registry, from `PostgresBackend::flush_stats` and
//! [`FjallBackend::write_buffer_stats`](crate::fjall_backend::FjallBackend::write_buffer_stats).
//! The Fjall gauges are read live at scrape time from every backend open in the process (a
//! [`Collector`]), so they show a flush emptying the buffer, not the size at the last commit.

use std::sync::LazyLock;
use std::time::Duration;

use prometheus_client::collector::Collector;
use prometheus_client::encoding::DescriptorEncoder;
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Unit;

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

/// The Fjall write buffer, read live from every open [`crate::fjall_backend::FjallBackend`]:
/// `hs_kv_fjall_write_buffer_bytes` (every memtable together, active and awaiting a flush),
/// `hs_kv_fjall_write_buffer_cap_bytes` (the cap the backend enforces; absent when no open
/// backend has one), `hs_kv_fjall_write_buffer_rotations_total` (memtable rotations the cap
/// caused) and `hs_kv_fjall_sealed_memtables` (memtables waiting for Fjall's flush workers). A
/// process normally has one backend; with more, the gauges are sums and the cap the smallest.
#[derive(Debug)]
struct FjallWriteBuffer;

impl Collector for FjallWriteBuffer {
    fn encode(&self, mut encoder: DescriptorEncoder<'_>) -> Result<(), std::fmt::Error> {
        let stats = crate::fjall_backend::open_backend_stats();
        let bytes: u64 = stats.iter().map(|s| s.bytes).sum();
        let rotations: u64 = stats.iter().map(|s| s.rotations).sum();
        let sealed: usize = stats.iter().map(|s| s.sealed_memtables).sum();
        let cap = stats.iter().filter_map(|s| s.cap).min();

        let mut e = encoder.encode_descriptor(
            // The unit appends `_bytes`.
            "hs_kv_fjall_write_buffer",
            "Bytes in the Fjall database's memtables, active and awaiting a flush together",
            Some(&Unit::Bytes),
            MetricType::Gauge,
        )?;
        e.encode_gauge(&bytes)?;
        if let Some(cap) = cap {
            let mut e = encoder.encode_descriptor(
                "hs_kv_fjall_write_buffer_cap",
                "The write-buffer cap hs-kv enforces by rotating memtables (RFC 0024)",
                Some(&Unit::Bytes),
                MetricType::Gauge,
            )?;
            e.encode_gauge(&cap)?;
        }
        let mut e = encoder.encode_descriptor(
            "hs_kv_fjall_write_buffer_rotations",
            "Memtable rotations hs-kv requested because the Fjall write buffer passed its cap",
            None,
            MetricType::Counter,
        )?;
        e.encode_counter::<Vec<(String, String)>, u64, u64>(&rotations, None)?;
        let mut e = encoder.encode_descriptor(
            "hs_kv_fjall_sealed_memtables",
            "Fjall memtables sealed and waiting for a flush worker",
            None,
            MetricType::Gauge,
        )?;
        e.encode_gauge(&sealed)?;
        Ok(())
    }
}

/// Registers this crate's metrics into `registry`: `hs_kv_postgres_flush_writes`,
/// `hs_kv_postgres_flush_duration_seconds`, `hs_kv_postgres_flush_statements_total`, and the
/// Fjall write-buffer gauges `hs_kv_fjall_write_buffer_bytes`,
/// `hs_kv_fjall_write_buffer_cap_bytes`, `hs_kv_fjall_write_buffer_rotations_total` and
/// `hs_kv_fjall_sealed_memtables`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register_collector(Box::new(FjallWriteBuffer));
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
        // The Fjall collector encodes with no backend open: zero bytes, no cap.
        assert!(out.contains("hs_kv_fjall_write_buffer_bytes 0"), "{out}");
        assert!(!out.contains("hs_kv_fjall_write_buffer_cap_bytes"), "{out}");
        assert!(
            out.contains("hs_kv_fjall_write_buffer_rotations_total 0"),
            "{out}"
        );
    }

    #[test]
    fn the_fjall_gauges_read_an_open_backend_live() {
        use crate::fjall_backend::{FJALL_WRITE_BUFFER_CAP, FjallBackend};
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = FjallBackend::open(dir.path()).expect("open");
        let mut registry = prometheus_client::registry::Registry::default();
        register_metrics(&mut registry);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).expect("encode");
        assert!(
            out.contains(&format!(
                "hs_kv_fjall_write_buffer_cap_bytes {FJALL_WRITE_BUFFER_CAP}"
            )),
            "{out}"
        );
        drop(backend);
    }
}
