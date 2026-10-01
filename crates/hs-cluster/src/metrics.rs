//! Cluster metrics: ownership churn, forward latency and lease age
//! (`docs/rfcs/0001-cluster-ownership.md` section 15).
//!
//! The counters live in a plain [`ClusterMetrics`] (atomics plus a small histogram) that the
//! ownership manager and the forwarder write to; [`ClusterCollector`] reads them at scrape time
//! and renders them as the `hs_cluster_*` series of the RFC, registered on the server's shared
//! Prometheus registry by `hs-cli` (`registry.register_collector`). Until 2026-09-28 nothing
//! registered them, so a clustered replica's `/metrics` had no `hs_cluster_*` series at all;
//! found on the first two-pod run on a real cluster.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Why an ownership change happened, for the `reason` label on
/// `hs_cluster_ownership_changes_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChurnReason {
    /// This replica acquired a shard.
    Acquire,
    /// This replica released a shard because it was no longer the desired owner.
    Release,
    /// This replica lost a shard's fence (a transaction was rejected as stale).
    Lost,
}

impl ChurnReason {
    /// The metric label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ChurnReason::Acquire => "acquire",
            ChurnReason::Release => "release",
            ChurnReason::Lost => "lost",
        }
    }
}

/// A fixed set of latency buckets (milliseconds) for forward-latency and lease-age observations.
/// Coarse on purpose: this is a lightweight stand-in for a real histogram type until 12's
/// telemetry registry lands.
/// The top buckets cover a forward that waited out a shard handoff (seconds, not milliseconds).
const BUCKETS_MS: [u64; 13] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];

#[derive(Default)]
struct Histogram {
    counts: [AtomicU64; BUCKETS_MS.len() + 1],
    sum_ms: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn observe(&self, d: Duration) {
        let ms = d.as_millis().min(u128::from(u64::MAX)) as u64;
        let bucket = BUCKETS_MS
            .iter()
            .position(|b| ms <= *b)
            .unwrap_or(BUCKETS_MS.len());
        self.counts[bucket].fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(ms, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Per-bucket counts in seconds for a Prometheus histogram, the last bucket `f64::MAX`
    /// (which the text encoder writes as `+Inf`, as for its own histograms); the
    /// text encoder accumulates them.
    fn prometheus_buckets(&self) -> Vec<(f64, u64)> {
        BUCKETS_MS
            .iter()
            .map(|ms| *ms as f64 / 1000.0)
            .chain(std::iter::once(f64::MAX))
            .zip(self.counts.iter().map(|c| c.load(Ordering::Relaxed)))
            .collect()
    }

    fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            count: self.count.load(Ordering::Relaxed),
            sum_ms: self.sum_ms.load(Ordering::Relaxed),
        }
    }
}

/// A cheap summary of a [`Histogram`]: enough to compute a mean; the per-bucket counts are
/// exposed by an exporter directly against the live counters, not through this snapshot type.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistogramSnapshot {
    /// Number of observations.
    pub count: u64,
    /// Sum of observed milliseconds.
    pub sum_ms: u64,
}

/// Process-wide cluster metrics for one replica.
#[derive(Default)]
pub struct ClusterMetrics {
    ownership_changes: Mutex<HashMap<(&'static str, ChurnReason), u64>>,
    owned_shards: Mutex<HashMap<&'static str, i64>>,
    forward_latency: Mutex<HashMap<(String, &'static str), Histogram>>,
    forward_retries: Mutex<HashMap<&'static str, u64>>,
    fenced_total: Mutex<HashMap<&'static str, u64>>,
    live_replicas: AtomicU64,
    heartbeat_seq: AtomicU64,
    drain_released_at_once: AtomicU64,
    lease_age: Mutex<Duration>,
    peer_lease_age: Mutex<HashMap<String, Duration>>,
}

impl ClusterMetrics {
    /// A fresh, zeroed metrics set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an ownership change for a shard kind.
    pub fn record_ownership_change(&self, kind: &'static str, reason: ChurnReason) {
        let mut m = self
            .ownership_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *m.entry((kind, reason)).or_default() += 1;
        drop(m);
        let mut owned = self
            .owned_shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = owned.entry(kind).or_default();
        match reason {
            ChurnReason::Acquire => *entry += 1,
            ChurnReason::Release | ChurnReason::Lost => *entry -= 1,
        }
    }

    /// Records the outcome and latency of one forward.
    pub fn record_forward(&self, route: &str, outcome: &'static str, latency: Duration) {
        let mut m = self
            .forward_latency
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        m.entry((route.to_owned(), outcome))
            .or_default()
            .observe(latency);
    }

    /// Records one forward retry, by reason (`"connect"`, `"421"`, `"503"`).
    pub fn record_forward_retry(&self, reason: &'static str) {
        let mut m = self
            .forward_retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *m.entry(reason).or_default() += 1;
    }

    /// Records a fenced (stale-owner) transaction rejection for a shard kind.
    pub fn record_fenced(&self, kind: &'static str) {
        let mut m = self
            .fenced_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *m.entry(kind).or_default() += 1;
    }

    /// Sets the number of replicas currently judged live.
    pub fn set_live_replicas(&self, n: u64) {
        self.live_replicas.store(n, Ordering::Relaxed);
    }

    /// Sets the `heartbeat_seq` of this replica's last heartbeat that reached the store.
    pub fn set_heartbeat_seq(&self, seq: u64) {
        self.heartbeat_seq.store(seq, Ordering::Relaxed);
    }

    /// Counts `n` shards a drain released without waiting for a new owner, because no other
    /// replica was live and hashable to claim them.
    pub fn record_drain_released_at_once(&self, n: u64) {
        self.drain_released_at_once.fetch_add(n, Ordering::Relaxed);
    }

    /// Sets this replica's own lease age (time since its last successful heartbeat).
    pub fn set_lease_age(&self, age: Duration) {
        *self
            .lease_age
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = age;
    }

    /// Sets a peer's observed lease age.
    pub fn set_peer_lease_age(&self, peer: &str, age: Duration) {
        self.peer_lease_age
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(peer.to_owned(), age);
    }

    /// A point-in-time snapshot suitable for an exporter or a test assertion.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            ownership_changes: self
                .ownership_changes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            owned_shards: self
                .owned_shards
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            forward_latency: self
                .forward_latency
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|(k, v)| (k.clone(), v.snapshot()))
                .collect(),
            forward_retries: self
                .forward_retries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            fenced_total: self
                .fenced_total
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            live_replicas: self.live_replicas.load(Ordering::Relaxed),
            heartbeat_seq: self.heartbeat_seq.load(Ordering::Relaxed),
            drain_released_at_once: self.drain_released_at_once.load(Ordering::Relaxed),
            lease_age: *self
                .lease_age
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }
}

/// A snapshot of [`ClusterMetrics`] at one instant.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    /// `hs_cluster_ownership_changes_total{kind, reason}`.
    pub ownership_changes: HashMap<(&'static str, ChurnReason), u64>,
    /// `hs_cluster_owned_shards{kind}`.
    pub owned_shards: HashMap<&'static str, i64>,
    /// `hs_cluster_forward_latency_seconds{route, outcome}`.
    pub forward_latency: HashMap<(String, &'static str), HistogramSnapshot>,
    /// `hs_cluster_forward_retries_total{reason}`.
    pub forward_retries: HashMap<&'static str, u64>,
    /// `hs_cluster_fenced_total{kind}`.
    pub fenced_total: HashMap<&'static str, u64>,
    /// `hs_cluster_live_replicas`.
    pub live_replicas: u64,
    /// `hs_cluster_heartbeat_seq`.
    pub heartbeat_seq: u64,
    /// `hs_cluster_drain_released_at_once_total`.
    pub drain_released_at_once: u64,
    /// `hs_cluster_lease_age_seconds`.
    pub lease_age: Duration,
}

/// Renders a replica's [`ClusterMetrics`] as Prometheus series on every scrape. Register it with
/// `registry.register_collector(Box::new(ClusterCollector::new(metrics)))`.
///
/// Series (RFC 0001 section 15): `hs_cluster_owned_shards{kind}`,
/// `hs_cluster_ownership_changes_total{kind,reason}`, `hs_cluster_forward_latency_seconds{route,
/// outcome}` (histogram), `hs_cluster_forward_retries_total{reason}`,
/// `hs_cluster_fenced_total{kind}`, `hs_cluster_live_replicas` and
/// `hs_cluster_lease_age_seconds`; and, beyond the RFC, `hs_cluster_heartbeat_seq` and
/// `hs_cluster_drain_released_at_once_total`.
#[derive(Clone)]
pub struct ClusterCollector {
    metrics: std::sync::Arc<ClusterMetrics>,
}

impl ClusterCollector {
    /// A collector reading `metrics`.
    #[must_use]
    pub fn new(metrics: std::sync::Arc<ClusterMetrics>) -> Self {
        Self { metrics }
    }
}

impl std::fmt::Debug for ClusterCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterCollector").finish_non_exhaustive()
    }
}

/// Sorted copies of a map's entries, so every scrape lists series in the same order.
fn sorted<K: Clone + Ord, V: Clone>(map: &Mutex<HashMap<K, V>>) -> Vec<(K, V)> {
    let mut entries: Vec<(K, V)> = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Encodes one counter sample under `labels`.
fn counter_sample(
    family: &mut prometheus_client::encoding::MetricEncoder<'_>,
    labels: &[(&str, &str)],
    value: u64,
) -> Result<(), std::fmt::Error> {
    family
        .encode_family(&labels)?
        .encode_counter::<prometheus_client::encoding::NoLabelSet, _, u64>(&value, None)
}

impl prometheus_client::collector::Collector for ClusterCollector {
    fn encode(
        &self,
        mut encoder: prometheus_client::encoding::DescriptorEncoder,
    ) -> Result<(), std::fmt::Error> {
        use prometheus_client::metrics::MetricType;
        let m = &self.metrics;

        let mut family = encoder.encode_descriptor(
            "hs_cluster_owned_shards",
            "Shards this replica owns, by shard kind",
            None,
            MetricType::Gauge,
        )?;
        for (kind, n) in sorted(&m.owned_shards) {
            family.encode_family(&[("kind", kind)])?.encode_gauge(&n)?;
        }

        let mut family = encoder.encode_descriptor(
            "hs_cluster_ownership_changes",
            "Shard ownership changes on this replica, by shard kind and reason",
            None,
            MetricType::Counter,
        )?;
        let mut changes: Vec<((&'static str, &'static str), u64)> = m
            .ownership_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|((kind, reason), n)| ((*kind, reason.as_str()), *n))
            .collect();
        changes.sort();
        for ((kind, reason), n) in changes {
            counter_sample(&mut family, &[("kind", kind), ("reason", reason)], n)?;
        }

        let mut family = encoder.encode_descriptor(
            "hs_cluster_forward_latency_seconds",
            "Latency of requests forwarded over the mesh, retries included, by route and outcome",
            None,
            MetricType::Histogram,
        )?;
        {
            let latency = m
                .forward_latency
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut keys: Vec<&(String, &'static str)> = latency.keys().collect();
            keys.sort();
            for key in keys {
                let Some(histogram) = latency.get(key) else {
                    continue;
                };
                let snap = histogram.snapshot();
                family
                    .encode_family(&[("route", key.0.as_str()), ("outcome", key.1)])?
                    .encode_histogram::<prometheus_client::encoding::NoLabelSet>(
                        snap.sum_ms as f64 / 1000.0,
                        snap.count,
                        &histogram.prometheus_buckets(),
                        None,
                    )?;
            }
        }

        let mut family = encoder.encode_descriptor(
            "hs_cluster_forward_retries",
            "Forward attempts retried, by reason (connect, 421, 503)",
            None,
            MetricType::Counter,
        )?;
        for (reason, n) in sorted(&m.forward_retries) {
            counter_sample(&mut family, &[("reason", reason)], n)?;
        }

        let mut family = encoder.encode_descriptor(
            "hs_cluster_fenced",
            "Writes refused because this replica had lost the shard's fence, by shard kind",
            None,
            MetricType::Counter,
        )?;
        for (kind, n) in sorted(&m.fenced_total) {
            counter_sample(&mut family, &[("kind", kind)], n)?;
        }

        let live = i64::try_from(m.live_replicas.load(Ordering::Relaxed)).unwrap_or(i64::MAX);
        encoder
            .encode_descriptor(
                "hs_cluster_live_replicas",
                "Replicas this replica currently judges live, itself included",
                None,
                MetricType::Gauge,
            )?
            .encode_gauge(&live)?;

        let seq = i64::try_from(m.heartbeat_seq.load(Ordering::Relaxed)).unwrap_or(i64::MAX);
        encoder
            .encode_descriptor(
                "hs_cluster_heartbeat_seq",
                "Sequence number of this replica's last heartbeat to reach the store; it rises \
                 by one per heartbeat and continues above the last value after a restart",
                None,
                MetricType::Gauge,
            )?
            .encode_gauge(&seq)?;

        encoder
            .encode_descriptor(
                "hs_cluster_drain_released_at_once",
                "Shards a drain released without waiting for a new owner, because no other \
                 replica was live to claim them",
                None,
                MetricType::Counter,
            )?
            .encode_counter::<prometheus_client::encoding::NoLabelSet, _, u64>(
                &m.drain_released_at_once.load(Ordering::Relaxed),
                None,
            )?;

        let age = m
            .lease_age
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_secs_f64();
        encoder
            .encode_descriptor(
                "hs_cluster_lease_age_seconds",
                "Time since this replica's last successful heartbeat",
                None,
                MetricType::Gauge,
            )?
            .encode_gauge(&age)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_collector_renders_every_series_under_its_rfc_name() {
        let m = std::sync::Arc::new(ClusterMetrics::new());
        m.record_ownership_change("room", ChurnReason::Acquire);
        m.record_ownership_change("room", ChurnReason::Acquire);
        m.record_ownership_change("room", ChurnReason::Release);
        m.record_forward("forward", "ok", Duration::from_millis(3));
        m.record_forward("forward", "ok", Duration::from_millis(1_800));
        m.record_forward_retry("421");
        m.record_fenced("room");
        m.set_live_replicas(2);
        m.set_heartbeat_seq(17);
        m.record_drain_released_at_once(5);
        m.set_lease_age(Duration::from_millis(1_500));

        let mut registry = prometheus_client::registry::Registry::default();
        registry.register_collector(Box::new(ClusterCollector::new(m)));
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).expect("encode");

        for line in [
            "hs_cluster_owned_shards{kind=\"room\"} 1",
            "hs_cluster_ownership_changes_total{kind=\"room\",reason=\"acquire\"} 2",
            "hs_cluster_ownership_changes_total{kind=\"room\",reason=\"release\"} 1",
            "hs_cluster_forward_latency_seconds_count{route=\"forward\",outcome=\"ok\"} 2",
            "hs_cluster_forward_latency_seconds_sum{route=\"forward\",outcome=\"ok\"} 1.803",
            "hs_cluster_forward_retries_total{reason=\"421\"} 1",
            "hs_cluster_fenced_total{kind=\"room\"} 1",
            "hs_cluster_live_replicas 2",
            "hs_cluster_heartbeat_seq 17",
            "hs_cluster_drain_released_at_once_total 5",
            "hs_cluster_lease_age_seconds 1.5",
        ] {
            assert!(text.contains(line), "missing `{line}` in:\n{text}");
        }
        // Cumulative buckets: the 3 ms forward is under 5 ms, both are under 2.5 s.
        let bucket = |le: &str| {
            text.lines()
                .find(|l| {
                    l.starts_with("hs_cluster_forward_latency_seconds_bucket")
                        && l.contains(&format!("le=\"{le}\""))
                })
                .map(|l| l.rsplit(' ').next().unwrap_or_default().to_owned())
        };
        assert_eq!(bucket("0.005").as_deref(), Some("1"), "{text}");
        assert_eq!(bucket("2.5").as_deref(), Some("2"), "{text}");
        assert_eq!(bucket("+Inf").as_deref(), Some("2"), "{text}");
        assert!(!text.contains("_total_total"), "{text}");
    }

    #[test]
    fn ownership_changes_update_owned_gauge() {
        let m = ClusterMetrics::new();
        m.record_ownership_change("room", ChurnReason::Acquire);
        m.record_ownership_change("room", ChurnReason::Acquire);
        m.record_ownership_change("room", ChurnReason::Release);
        let snap = m.snapshot();
        assert_eq!(snap.owned_shards[&"room"], 1);
        assert_eq!(snap.ownership_changes[&("room", ChurnReason::Acquire)], 2);
    }

    #[test]
    fn forward_latency_is_observed() {
        let m = ClusterMetrics::new();
        m.record_forward("room.send", "ok", Duration::from_millis(3));
        let snap = m.snapshot();
        let h = snap.forward_latency[&("room.send".to_string(), "ok")];
        assert_eq!(h.count, 1);
    }
}
