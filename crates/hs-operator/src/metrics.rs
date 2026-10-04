//! The operator's Prometheus metrics, registered into an [`hs_telemetry::Metrics`] registry
//! (the naming conventions there apply: `hs_operator_*`, counters registered without `_total`),
//! and [`serve`], the `/metrics` listener `hs operator --metrics-address` runs.
//!
//! | Metric | Type | Labels |
//! | --- | --- | --- |
//! | `hs_operator_reconcile_duration_seconds` | histogram | `kind`, `result` (`ok`, `error`) |
//! | `hs_operator_reconcile_errors_total` | counter | `kind`, `reason` |
//! | `hs_operator_drains_in_flight` | gauge | `kind` |
//! | `hs_operator_drains_total` | counter | `outcome` (`started`, `completed`, `timed_out`, `aborted`, `refused`, `undrained`) |

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};

/// `kind` and `result` of a reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct ReconcileLabels {
    /// `Homeserver` or `Bridge`.
    pub kind: String,
    /// `ok` or `error`.
    pub result: String,
}

/// `kind` and `reason` of a failed reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct ErrorLabels {
    /// `Homeserver` or `Bridge`.
    pub kind: String,
    /// A short, bounded reason (`kube`, `admin_api`, `invalid`, ...).
    pub reason: String,
}

/// `kind` of the drains in flight.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct KindLabels {
    /// `Homeserver`.
    pub kind: String,
}

/// How a drain step ended.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct DrainLabels {
    /// `started`, `completed`, `timed_out`, `aborted`, `refused` or `undrained`.
    pub outcome: String,
}

/// The operator's metrics. Cheap to clone.
#[derive(Clone)]
pub struct OperatorMetrics {
    registry: hs_telemetry::Metrics,
    /// `hs_operator_reconcile_duration_seconds{kind,result}`.
    pub reconcile_duration: Family<ReconcileLabels, Histogram>,
    /// `hs_operator_reconcile_errors_total{kind,reason}`.
    pub reconcile_errors: Family<ErrorLabels, Counter>,
    /// `hs_operator_drains_in_flight{kind}`.
    pub drains_in_flight: Family<KindLabels, Gauge>,
    /// `hs_operator_drains_total{outcome}`.
    pub drains: Family<DrainLabels, Counter>,
    draining: Arc<Mutex<BTreeSet<String>>>,
}

impl Default for OperatorMetrics {
    fn default() -> Self {
        Self::new(hs_telemetry::Metrics::new())
    }
}

impl std::fmt::Debug for OperatorMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorMetrics").finish_non_exhaustive()
    }
}

impl OperatorMetrics {
    /// Registers the operator's families into `registry`.
    #[must_use]
    pub fn new(registry: hs_telemetry::Metrics) -> Self {
        let reconcile_duration = Family::<ReconcileLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(exponential_buckets(0.005, 2.0, 14))
        });
        let reconcile_errors = Family::<ErrorLabels, Counter>::default();
        let drains_in_flight = Family::<KindLabels, Gauge>::default();
        let drains = Family::<DrainLabels, Counter>::default();
        registry.with_registry(|r| {
            r.register(
                "hs_operator_reconcile_duration_seconds",
                "How long one reconcile took, by kind and result",
                reconcile_duration.clone(),
            );
            r.register(
                "hs_operator_reconcile_errors",
                "Reconciles that failed, by kind and reason",
                reconcile_errors.clone(),
            );
            r.register(
                "hs_operator_drains_in_flight",
                "Replicas the operator is waiting on to hand off their shards",
                drains_in_flight.clone(),
            );
            r.register(
                "hs_operator_drains",
                "Drain steps the operator took, by outcome",
                drains.clone(),
            );
            // The admin-API client (`homeserver::admin::HttpAdminApi`) is built by
            // `hs_http::client::builder()`, so its connections are counted by the
            // process-wide `hs_outbound_*` counters; they are served here, as `hs serve`
            // serves them.
            hs_http::outbound::register_metrics(r);
        });
        Self {
            registry,
            reconcile_duration,
            reconcile_errors,
            drains_in_flight,
            drains,
            draining: Arc::default(),
        }
    }

    /// The registry, for rendering.
    #[must_use]
    pub fn registry(&self) -> &hs_telemetry::Metrics {
        &self.registry
    }

    /// Records one reconcile.
    pub fn record_reconcile(&self, kind: &str, duration_secs: f64, error_reason: Option<&str>) {
        let result = if error_reason.is_some() {
            "error"
        } else {
            "ok"
        };
        self.reconcile_duration
            .get_or_create(&ReconcileLabels {
                kind: kind.to_owned(),
                result: result.to_owned(),
            })
            .observe(duration_secs);
        if let Some(reason) = error_reason {
            self.reconcile_errors
                .get_or_create(&ErrorLabels {
                    kind: kind.to_owned(),
                    reason: reason.to_owned(),
                })
                .inc();
        }
    }

    /// Counts one drain step.
    pub fn drain_event(&self, outcome: &str) {
        self.drains
            .get_or_create(&DrainLabels {
                outcome: outcome.to_owned(),
            })
            .inc();
    }

    /// Records whether the `Homeserver` `key` (`namespace/name`) has a drain in flight, and
    /// sets the gauge to how many do.
    pub fn set_draining(&self, key: &str, draining: bool) {
        let mut set = self.draining.lock().unwrap_or_else(PoisonError::into_inner);
        if draining {
            set.insert(key.to_owned());
        } else {
            set.remove(key);
        }
        let count = i64::try_from(set.len()).unwrap_or(i64::MAX);
        self.drains_in_flight
            .get_or_create(&KindLabels {
                kind: "Homeserver".to_owned(),
            })
            .set(count);
    }

    /// The number of drains in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.draining
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The current count of drain steps with `outcome`.
    #[must_use]
    pub fn drain_count(&self, outcome: &str) -> u64 {
        self.drains
            .get_or_create(&DrainLabels {
                outcome: outcome.to_owned(),
            })
            .get()
    }
}

/// Serves `GET /metrics` (and `GET /healthz`) on `address` until the future is dropped.
///
/// # Errors
/// When the address cannot be bound.
pub async fn serve(metrics: OperatorMetrics, address: SocketAddr) -> std::io::Result<()> {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;

    let app = axum::Router::new()
        .route(
            "/metrics",
            get(move || {
                let metrics = metrics.clone();
                async move {
                    match metrics.registry().encode_to_string() {
                        Ok(text) => (
                            StatusCode::OK,
                            [(
                                header::CONTENT_TYPE,
                                "application/openmetrics-text; version=1.0.0; charset=utf-8",
                            )],
                            text,
                        )
                            .into_response(),
                        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                }
            }),
        )
        .route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "operator metrics listening");
    axum::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The outbound connection counters are served beside the operator's own, since its
    /// admin-API client is counted by them.
    #[test]
    fn the_outbound_counters_are_served_too() {
        let metrics = OperatorMetrics::default();
        let text = metrics.registry().encode_to_string().unwrap();
        assert!(text.contains("hs_outbound_connections"), "{text}");
        assert!(text.contains("hs_outbound_connect_failures"), "{text}");
    }

    #[test]
    fn families_render_under_their_documented_names() {
        let metrics = OperatorMetrics::default();
        metrics.record_reconcile("Homeserver", 0.01, None);
        metrics.record_reconcile("Homeserver", 0.02, Some("kube"));
        metrics.drain_event("started");
        metrics.set_draining("ns/a", true);
        metrics.set_draining("ns/b", true);
        metrics.set_draining("ns/a", false);
        let text = metrics.registry().encode_to_string().unwrap();
        assert!(text.contains(
            "hs_operator_reconcile_duration_seconds_count{kind=\"Homeserver\",result=\"ok\"} 1"
        ));
        assert!(
            text.contains(
                "hs_operator_reconcile_errors_total{kind=\"Homeserver\",reason=\"kube\"} 1"
            )
        );
        assert!(text.contains("hs_operator_drains_in_flight{kind=\"Homeserver\"} 1"));
        assert!(text.contains("hs_operator_drains_total{outcome=\"started\"} 1"));
        assert!(!text.contains("_total_total"));
        assert_eq!(metrics.in_flight(), 1);
    }
}
