# 0004. Telemetry conventions: metric names, span names, request ids

Status: accepted, 2026-09-18. Owner: track 12 (platform and Kubernetes).
Consumers: every track that emits metrics or spans, in particular tracks 03
(cluster) and 15 (admin API and modules), who asked for this to be recorded
here rather than only in `crates/hs-telemetry` rustdoc.

This document is a mirror of the binding rules documented at
`crates/hs-telemetry/src/metrics.rs` (module-level rustdoc). If the two
ever disagree, the rustdoc is normative (it is what `cargo doc` publishes
and what compiles), and this file should be corrected to match.

## Metric naming

- **Prefix.** Every native metric is named `hs_<subsystem>_<noun>_<unit or
  "total">`, no exceptions. `<subsystem>` is the owning crate's short name
  without the `hs-` prefix (`room`, `federation`, `cluster`, `media`,
  `auth`, ...). A metric with no natural subsystem (process-wide ones
  `hs-telemetry` itself registers) uses `hs_process_*` or `hs_http_*`.
- **Suffixes.** Counters end in `_total` (`hs_http_requests_total`) **on
  the wire** — but call `registry.register(name, help, counter)` with
  `name` **without** that suffix (`"hs_http_requests"`, not
  `"hs_http_requests_total"`). `prometheus_client`'s text encoder appends
  a literal `_total` to every counter it renders unconditionally, so a
  name that already ends in `_total` renders doubled
  (`hs_http_requests_total_total`) — a real bug this project shipped once,
  caught by curling a running server rather than by a unit test that only
  checked `.contains("hs_http_requests_total")` (still true of the doubled
  name, as a prefix). Values with a unit end in that unit, spelled out and
  always base-SI (`_seconds`, not `_ms`; `_bytes`, not `_kb`):
  `hs_http_request_duration_seconds`, registered exactly as written
  (histograms get `_bucket`/`_sum`/`_count` from the metric type, not from
  a name convention, so they have no equivalent doubling risk). Gauges
  have no mandated suffix beyond the noun itself (`hs_cluster_shards_owned`).
- **Labels.** Keep label cardinality bounded: `method`, `route` (the
  *templated* path, e.g. `/rooms/{roomId}/state`, never a raw path with
  real room IDs interpolated), `status_class` (`"2xx"`, `"4xx"`, ...), and
  subsystem-specific low-cardinality labels only. Never label with a user
  ID, room ID, event ID, device ID or any other value with unbounded
  cardinality — that turns a metric into an accidental unbounded time
  series generator and can take down the Prometheus server scraping it.
- **Histograms, not summaries**, for latency: histograms aggregate
  correctly across replicas (`histogram_quantile` over a `sum by ()`),
  which Prometheus summaries cannot do. Seed bucket boundaries from the
  subsystem's own expected latency floor with
  `prometheus_client::metrics::histogram::exponential_buckets`, not the
  library's HTTP-tuned defaults — a storage commit measured in
  microseconds needs different buckets than an HTTP request measured in
  tens of milliseconds.
- **Synapse-compatible names.** When
  `hs_config::telemetry::MetricsConfig::synapse_compat_names` is set
  (default `true`), the same sample is additionally exported once more
  under its Synapse-shaped name (`synapse_*`) beside the native `hs_*`
  one, so existing Synapse Grafana dashboards keep working unmodified
  during a migration. `hs-telemetry` does not maintain that duplicate
  name table itself — each subsystem with a Synapse analog registers both
  names for its own metrics into the shared
  `hs_telemetry::metrics::Metrics` registry.

## Span naming

Spans use `<subsystem>.<operation>` in `snake_case` (`room.apply_event`,
`federation.send_transaction`, `auth.login`), matching the metric
subsystem vocabulary above so a trace and a metric for the same operation
are easy to correlate by name alone.

The outermost span for one inbound HTTP request is always named
`request` and carries the `request_id` field (see below); nested spans
opened while handling it do not repeat that field — it is already on the
parent span and `tracing`'s structured output includes ancestor fields.

## Request-id propagation

`hs_telemetry::request_id::RequestIdLayer` is a `tower::Layer` (not an
axum-specific extractor, so it wraps any `http::Request`/`http::Response`
service — the client router, the federation router, the metrics listener
alike). It:

1. Reads `x-request-id` from the incoming request. If present, that value
   is trusted and reused verbatim (a reverse proxy or an upstream
   federation peer may already have minted one).
2. Otherwise generates a fresh one: 16 bytes of randomness, hex-encoded.
3. Opens an `info_span!("request", request_id = %id)` around the rest of
   the request's handling.
4. Stamps the same value onto the `x-request-id` response header.

Mount it outermost in the service stack so every span opened further in
inherits `request_id` as an ancestor field.

## Where this is implemented

- `crates/hs-telemetry/src/metrics.rs` — the `Metrics` registry, the
  naming conventions (normative copy), `hs_http_requests_total` and
  `hs_http_request_duration_seconds`.
- `crates/hs-telemetry/src/request_id.rs` — `RequestIdLayer`.
- `crates/hs-telemetry/src/init.rs` — tracing subscriber initialization
  (JSON or Synapse-like text format), OTLP export behind the `otlp`
  feature, Sentry reporting behind the `sentry` feature.
