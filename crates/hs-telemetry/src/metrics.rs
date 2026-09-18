//! A Prometheus [`Registry`] and the naming conventions every track registering a metric here
//! must follow.
//!
//! # Naming conventions
//!
//! These conventions are binding for every metric this project exports, mirrored in
//! `docs/decisions/0004-telemetry-conventions.md` (tracks 03 and 15 consume that copy; this one
//! is normative and the decisions doc must be kept in sync with it).
//!
//! - **Prefix.** Every native metric is named `hs_<subsystem>_<noun>_<unit or "total">`, no
//!   exceptions. `<subsystem>` is the owning crate's short name without the `hs-` prefix (`room`,
//!   `federation`, `cluster`, `media`, `auth`, ...). A metric with no natural subsystem
//!   (process-wide ones this crate itself registers) uses `hs_process_*` or `hs_http_*`.
//! - **Suffixes.** Counters end in `_total` (`hs_http_requests_total`) *on the wire* — but
//!   `registry.register(name, help, counter)` must be called with `name` **without** that
//!   suffix (`"hs_http_requests"`, not `"hs_http_requests_total"`):
//!   `prometheus_client`'s text encoder appends a literal `_total` to every [`Counter`] it
//!   renders unconditionally, so a name that already ends in `_total` comes out doubled
//!   (`hs_http_requests_total_total`) — a real bug this crate shipped once (caught by curling a
//!   running server, not by a unit test that only checked `.contains("hs_http_requests_total")`,
//!   which a doubled name still satisfies as a prefix). If you are registering a `Counter` and
//!   typing `_total` into the string you pass to `register`, stop — that suffix is the encoder's
//!   job. Values with a unit end in that unit, spelled out and always base-SI (`_seconds`, not
//!   `_ms`; `_bytes`, not `_kb`): `hs_http_request_duration_seconds`, registered exactly as
//!   written (histograms get their `_bucket`/`_sum`/`_count` suffixes from the metric *type*, not
//!   from a name convention, so there is no equivalent doubling risk there). Gauges have no
//!   mandated suffix beyond the noun itself (`hs_cluster_shards_owned`).
//! - **Labels.** Keep the label cardinality bounded: `method`, `route` (the *templated* path, for
//!   example `/rooms/{roomId}/state`, never the raw path with real room IDs interpolated —
//!   interpolating identifiers into a label defeats Prometheus's storage model), `status_class`
//!   (`"2xx"`, `"4xx"`, ...), and subsystem-specific low-cardinality labels only. Never label with
//!   a user ID, room ID, event ID, device ID or any other value with unbounded cardinality.
//! - **Histograms.** Prefer histograms (not summaries) for latency: they aggregate correctly
//!   across replicas, which Prometheus summaries cannot do. Use
//!   [`prometheus_client::metrics::histogram::Histogram::new`] with
//!   [`prometheus_client::metrics::histogram::exponential_buckets`] seeded from the
//!   subsystem's expected latency floor, not the library's default bucket set, which is tuned for
//!   web request latencies in seconds and is a poor fit for, say, a storage commit measured in
//!   microseconds.
//! - **Synapse-compatible names.** When `telemetry.metrics.synapse_compat_names` is set
//!   (`hs_config::telemetry::MetricsConfig::synapse_compat_names`, default `true`), the same
//!   sample is additionally exported once more under its Synapse-shaped name (`synapse_*`) beside
//!   the native `hs_*` one, so existing Synapse Grafana dashboards keep working unmodified. This
//!   crate does not maintain that duplicate name table itself (each subsystem that has a Synapse
//!   analog registers both names for its own metrics); [`Registry`] just provides the shared
//!   registry both go into.
//!
//! # Span naming
//!
//! Spans use the pattern `<subsystem>.<operation>` in `snake_case`
//! (`room.apply_event`, `federation.send_transaction`, `auth.login`), matching the metric
//! subsystem vocabulary above so a trace and a metric for the same operation are easy to
//! correlate by name alone. The outermost span for one inbound HTTP request is always named
//! `request` (see [`crate::request_id`]) and carries the `request_id` field; nested spans opened
//! while handling it do not repeat that field (it is already on the parent span and `tracing`
//! includes ancestor fields in structured output).

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

/// The label set for `hs_http_requests_total` and `hs_http_request_duration_seconds`: bounded,
/// low-cardinality dimensions only. See the module's naming conventions on why `route` must be
/// the templated path, not the raw one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
pub struct HttpLabels {
    /// The HTTP method, upper-case (`"GET"`, `"POST"`, ...).
    pub method: String,
    /// The templated route (`"/​_matrix/client/v3/rooms/{roomId}/state"`), never a raw path with
    /// identifiers interpolated.
    pub route: String,
    /// `"2xx"`, `"3xx"`, `"4xx"`, `"5xx"`.
    pub status_class: String,
}

/// The process-wide metrics this crate registers directly, plus the [`Registry`] every other
/// subsystem registers its own metric families into. Cheap to clone (every field is an `Arc`
/// internally, as `prometheus_client`'s types are); share one instance across the process rather
/// than constructing a second registry.
#[derive(Clone)]
pub struct Metrics {
    registry: std::sync::Arc<std::sync::Mutex<Registry>>,
    /// `hs_http_requests_total{method,route,status_class}`.
    pub http_requests_total: Family<HttpLabels, Counter>,
    /// `hs_http_request_duration_seconds{method,route,status_class}`.
    pub http_request_duration_seconds: Family<HttpLabels, Histogram>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Builds a fresh registry with this crate's own process-wide metrics pre-registered.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let http_requests_total = Family::<HttpLabels, Counter>::default();
        // Registered as `hs_http_requests`, not `hs_http_requests_total`: `prometheus_client`'s
        // text encoder always appends a literal `_total` suffix to every `Counter` it renders
        // (the OpenMetrics convention for counters), regardless of what the registered name
        // already ends in. Registering the name with `_total` already on it therefore rendered
        // as `hs_http_requests_total_total` on the wire — caught by curling a running server
        // during integration review, not by the unit test below (which only checked the
        // substring `"hs_http_requests_total"` was present, which it was, just as a *prefix* of
        // the doubled name). The histogram below has no equivalent bug: its `_bucket`/`_sum`/
        // `_count` suffixes come from the metric *type*, not from a name convention the given
        // name could already satisfy.
        registry.register(
            "hs_http_requests",
            "Total HTTP requests handled, by method, templated route and status class",
            http_requests_total.clone(),
        );

        let http_request_duration_seconds =
            Family::<HttpLabels, Histogram>::new_with_constructor(|| {
                Histogram::new(exponential_buckets(0.001, 2.0, 16))
            });
        registry.register(
            "hs_http_request_duration_seconds",
            "HTTP request duration in seconds, by method, templated route and status class",
            http_request_duration_seconds.clone(),
        );

        Self {
            registry: std::sync::Arc::new(std::sync::Mutex::new(registry)),
            http_requests_total,
            http_request_duration_seconds,
        }
    }

    /// Runs `f` with mutable access to the underlying [`Registry`], for a subsystem registering
    /// its own metric families at startup (`registry.register("hs_room_...", ..., family)`).
    pub fn with_registry<R>(&self, f: impl FnOnce(&mut Registry) -> R) -> R {
        let mut guard = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }

    /// Records one completed HTTP request.
    pub fn record_http_request(&self, method: &str, route: &str, status: u16, duration_secs: f64) {
        let labels = HttpLabels {
            method: method.to_owned(),
            route: route.to_owned(),
            status_class: status_class(status),
        };
        self.http_requests_total.get_or_create(&labels).inc();
        self.http_request_duration_seconds
            .get_or_create(&labels)
            .observe(duration_secs);
    }

    /// Renders every registered metric in Prometheus text exposition format, for `GET /metrics`.
    ///
    /// # Errors
    /// Returns an error only if encoding itself fails (an internal `prometheus_client`
    /// invariant violation); it never fails because of metric content.
    pub fn encode_to_string(&self) -> Result<String, std::fmt::Error> {
        let guard = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut buf = String::new();
        encode(&mut buf, &guard)?;
        Ok(buf)
    }
}

fn status_class(status: u16) -> String {
    match status / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "unknown",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_a_request_and_renders_it() {
        let metrics = Metrics::new();
        metrics.record_http_request("GET", "/health/live", 200, 0.005);
        let text = metrics.encode_to_string().unwrap();
        // Exact metric name, not a substring check: `prometheus_client` appends a literal
        // `_total` to every `Counter` it renders, so a name already ending in `_total` would
        // render as `..._total_total` and still (wrongly) satisfy a bare `.contains("..._total")`
        // check, since that's a prefix of the doubled name. This caught exactly that bug once
        // (see the comment on this family's `registry.register` call in `Metrics::new`).
        assert!(
            text.contains("hs_http_requests_total{"),
            "expected the exact metric name `hs_http_requests_total`, got:\n{text}"
        );
        assert!(
            !text.contains("hs_http_requests_total_total"),
            "metric name was doubled:\n{text}"
        );
        assert!(text.contains("hs_http_request_duration_seconds"));
        assert!(text.contains("method=\"GET\""));
        assert!(text.contains("status_class=\"2xx\""));
    }

    #[test]
    fn status_class_buckets_correctly() {
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(503), "5xx");
    }

    #[test]
    fn a_subsystem_can_register_its_own_family() {
        let metrics = Metrics::new();
        let family = Family::<Vec<(String, String)>, Counter>::default();
        metrics.with_registry(|registry| {
            // Registered as `hs_room_events` (no `_total`), for the same reason
            // `Metrics::new`'s own `hs_http_requests` family is: `prometheus_client` appends the
            // `_total` suffix to every `Counter` itself. A subsystem copying this test as a
            // template should copy the naming, not just the shape.
            registry.register("hs_room_events", "Room events applied", family.clone());
        });
        family
            .get_or_create(&vec![("kind".to_string(), "m.room.message".to_string())])
            .inc();
        let text = metrics.encode_to_string().unwrap();
        assert!(text.contains("hs_room_events_total{"));
        assert!(!text.contains("hs_room_events_total_total"));
    }
}
