//! The admin API's room long tail as `hs serve` wires it: `hs_room::admin::RoomRegistryDirectory`
//! over the running registry and accounts, with its purges and deletions counted into `/metrics`.
//!
//! - `hs_admin_room_operations_total{operation,outcome}`: purges (`purge_history`) and deletions
//!   (`delete`) that ended `succeeded` or `failed`.
//! - `hs_admin_room_operation_duration_seconds{operation}`: how long each took.

use std::sync::Arc;
use std::time::Duration;

use hs_kv::KvBackend;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};

#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct OutcomeLabels {
    operation: String,
    outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct OperationLabels {
    operation: String,
}

/// The room operation metric families (see the module docs).
#[derive(Clone)]
pub struct RoomOperationMetrics {
    operations: Family<OutcomeLabels, Counter>,
    duration: Family<OperationLabels, Histogram, fn() -> Histogram>,
}

/// From a few milliseconds (a small room) to about an hour (a very large one).
fn duration_histogram() -> Histogram {
    Histogram::new(exponential_buckets(0.005, 2.0, 20))
}

impl RoomOperationMetrics {
    /// Registers the families into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let operations = Family::<OutcomeLabels, Counter>::default();
        let duration =
            Family::<OperationLabels, Histogram, fn() -> Histogram>::new_with_constructor(
                duration_histogram,
            );
        metrics.with_registry(|registry| {
            registry.register(
                "hs_admin_room_operations",
                "Room purges (purge_history) and deletions (delete) run through the admin API, by \
                 outcome: succeeded, failed",
                operations.clone(),
            );
            registry.register(
                "hs_admin_room_operation_duration_seconds",
                "How long a room purge or deletion run through the admin API took",
                duration.clone(),
            );
        });
        Self {
            operations,
            duration,
        }
    }

    /// Counts one finished operation.
    pub fn observe(&self, operation: &str, outcome: &str, elapsed: Duration) {
        self.operations
            .get_or_create(&OutcomeLabels {
                operation: operation.to_owned(),
                outcome: outcome.to_owned(),
            })
            .inc();
        self.duration
            .get_or_create(&OperationLabels {
                operation: operation.to_owned(),
            })
            .observe(elapsed.as_secs_f64());
    }
}

/// The room content source for `hs serve`: the registry, the accounts (an administrator's join
/// checks the user and uses their profile) and the metrics above.
#[must_use]
pub fn source<B: KvBackend + 'static>(
    rooms: Arc<hs_room::registry::RoomRegistry<B>>,
    auth: hs_auth::state::AuthState,
    metrics: RoomOperationMetrics,
) -> Arc<dyn hs_admin::rooms::RoomContentSource> {
    Arc::new(
        hs_room::admin::RoomRegistryDirectory::new(rooms)
            .with_auth(auth)
            .with_observer(Arc::new(move |operation, outcome, elapsed| {
                metrics.observe(operation, outcome, elapsed);
            })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_operation_metrics_are_exported_under_their_names() {
        let metrics = hs_telemetry::metrics::Metrics::new();
        let room = RoomOperationMetrics::register(&metrics);
        room.observe("purge_history", "succeeded", Duration::from_millis(20));
        room.observe("delete", "failed", Duration::from_secs(2));
        let text = metrics.encode_to_string().unwrap();
        assert!(text.contains(
            "hs_admin_room_operations_total{operation=\"purge_history\",outcome=\"succeeded\"} 1"
        ));
        assert!(
            text.contains(
                "hs_admin_room_operations_total{operation=\"delete\",outcome=\"failed\"} 1"
            )
        );
        assert!(
            text.contains("hs_admin_room_operation_duration_seconds_count{operation=\"delete\"} 1")
        );
    }
}
