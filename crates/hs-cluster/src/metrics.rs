//! Cluster metrics: ownership churn, forward latency and lease age
//! (`docs/rfcs/0001-cluster-ownership.md` section 15).
//!
//! Track 12 has not published `hs-telemetry`'s registry yet, so this module keeps counters in a
//! plain `ClusterMetrics` snapshot (atomics plus a small histogram) that any exporter can read;
//! once 12 lands, an `hs-telemetry` adapter reads these same fields and registers them as
//! `hs_cluster_*` series under the names in the RFC.

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
const BUCKETS_MS: [u64; 10] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000];

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
    /// `hs_cluster_lease_age_seconds`.
    pub lease_age: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

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
