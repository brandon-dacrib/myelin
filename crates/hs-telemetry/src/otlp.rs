//! OpenTelemetry OTLP trace export, behind the `otlp` feature.
//!
//! # What a request can cost, and what it cannot
//!
//! Spans are handed to the SDK's batch span processor, which runs on a thread of its own. A
//! finished span goes into a bounded queue ([`MAX_QUEUE_SIZE`]) with a non-blocking `try_send`;
//! when the queue is full (a collector that is down, slow or unreachable) the span is dropped and
//! counted, never waited for. Export happens on that thread, in batches of at most
//! [`MAX_EXPORT_BATCH_SIZE`], every [`SCHEDULED_DELAY`] or sooner when a batch fills, and each
//! export gives up after [`EXPORT_TIMEOUT`]. So a request thread's whole cost is building its
//! spans and one channel send per sampled span; nothing a collector does can make it wait.
//!
//! The sampler is parent-based over a trace-id ratio: a request that arrives with a sampled W3C
//! `traceparent` is traced (the caller decided), one with an unsampled `traceparent` is not, and
//! a request with none is traced with probability [`crate::Options::otlp_sample_ratio`]. An
//! unsampled span is never recorded or exported.

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider,
};

use crate::error::TelemetryError;
use crate::init::Options;

/// Spans waiting for export, at most. Past this, new spans are dropped rather than queued.
pub const MAX_QUEUE_SIZE: usize = 4096;
/// Spans per export call, at most.
pub const MAX_EXPORT_BATCH_SIZE: usize = 512;
/// How long a partial batch waits before it is sent anyway.
pub const SCHEDULED_DELAY: Duration = Duration::from_secs(2);
/// How long one export may take before it is abandoned (the gRPC call's deadline).
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// The sampler [`build_provider`] installs: parent-based, over a trace-id ratio clamped to
/// `0.0..=1.0` (a non-finite ratio means none).
#[must_use]
pub fn sampler(ratio: f64) -> Sampler {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
}

/// Builds an OTLP (gRPC/tonic) span exporter and a batching tracer provider tagged with this
/// service's name, version and instance as OTLP resource attributes.
///
/// Must be called inside a Tokio runtime: the gRPC channel's connection task is spawned on it.
/// The channel connects lazily, so an unreachable collector is not an error here; it is a
/// dropped batch later.
///
/// # Errors
/// Returns [`TelemetryError::Otlp`] if the exporter could not be built (for example, an
/// unparseable endpoint URL).
pub fn build_provider(
    options: &Options,
    endpoint: &str,
) -> Result<SdkTracerProvider, TelemetryError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()?;

    let mut resource = Resource::builder()
        .with_service_name(options.service_name.clone())
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            options.service_version.clone(),
        ));
    // In a pod, `HOSTNAME` is the pod's name: which replica a span came from.
    if let Some(instance) = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()) {
        resource = resource.with_attribute(opentelemetry::KeyValue::new(
            "service.instance.id",
            instance,
        ));
    }

    let processor = BatchSpanProcessor::builder(Watched::new(exporter, endpoint))
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(MAX_QUEUE_SIZE)
                .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
                .with_scheduled_delay(SCHEDULED_DELAY)
                .build(),
        )
        .build();

    let provider = SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_sampler(sampler(options.otlp_sample_ratio))
        .with_resource(resource.build())
        .build();

    Ok(provider)
}

/// How often, at most, a failing export is logged while it keeps failing.
pub const EXPORT_FAILURE_WARN_EVERY: Duration = Duration::from_secs(300);

/// A span exporter that tells the operator when export fails, and when it recovers.
///
/// The SDK's own diagnostics are compiled out (its `internal-logs` feature would log every
/// failed batch, every few seconds while a collector is down). This logs the first failure at
/// `warn`, then at most once per [`EXPORT_FAILURE_WARN_EVERY`] with how many batches and spans
/// were lost, and an `info` line when export works again. The lines carry no span, so they are
/// log lines only, never exported spans of their own.
#[derive(Debug)]
struct Watched<E> {
    inner: E,
    endpoint: String,
    state: std::sync::Mutex<WatchState>,
}

#[derive(Debug, Default)]
struct WatchState {
    failed_batches: u64,
    failed_spans: u64,
    last_warned: Option<std::time::Instant>,
}

impl<E> Watched<E> {
    fn new(inner: E, endpoint: &str) -> Self {
        Self {
            inner,
            endpoint: endpoint.to_owned(),
            state: std::sync::Mutex::new(WatchState::default()),
        }
    }

    /// Records one export's outcome, logging as the type's docs say.
    fn observe(&self, spans: usize, result: &opentelemetry_sdk::error::OTelSdkResult) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match result {
            Ok(()) => {
                if state.failed_batches > 0 {
                    tracing::info!(
                        target: "hs_telemetry::otlp",
                        endpoint = %self.endpoint,
                        failed_batches = state.failed_batches,
                        lost_spans = state.failed_spans,
                        "trace export works again"
                    );
                }
                *state = WatchState::default();
            }
            Err(error) => {
                state.failed_batches += 1;
                state.failed_spans += spans as u64;
                let due = state
                    .last_warned
                    .is_none_or(|at| at.elapsed() >= EXPORT_FAILURE_WARN_EVERY);
                if due {
                    tracing::warn!(
                        target: "hs_telemetry::otlp",
                        endpoint = %self.endpoint,
                        %error,
                        failed_batches = state.failed_batches,
                        lost_spans = state.failed_spans,
                        "trace export to the OTLP collector is failing; spans are dropped until \
                         it answers (requests are not slowed)"
                    );
                    state.last_warned = Some(std::time::Instant::now());
                }
            }
        }
    }
}

impl<E: opentelemetry_sdk::trace::SpanExporter> opentelemetry_sdk::trace::SpanExporter
    for Watched<E>
{
    async fn export(
        &self,
        batch: Vec<opentelemetry_sdk::trace::SpanData>,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        let spans = batch.len();
        let result = self.inner.export(batch).await;
        self.observe(spans, &result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// Wraps `provider` in a `tracing-opentelemetry` layer that can be composed onto any
/// `tracing_subscriber::Registry`-based subscriber via `.with(...)`.
///
/// Thread ids and names and the source location are left off every span: they cost a handful
/// of attributes per span and say nothing a span's name and target do not.
///
/// Only this project's own spans (targets starting `hs_`) are exported: a library's (`hyper`,
/// `h2`, `tonic` -- the exporter's own transport among them --, `tantivy`, Ruma's) would be a
/// trace of its own with nothing around it, and its name means nothing to an operator. Events
/// are kept, whatever their target: they become events on the span they happened in. The span
/// [`crate::outside_spans`] runs outbound HTTP in is not exported either.
pub fn layer_from_provider<S>(provider: &SdkTracerProvider) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    use tracing_subscriber::Layer as _;

    let tracer = provider.tracer("hs-telemetry");
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_threads(false)
        .with_location(false)
        .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
            !metadata.is_span()
                || (metadata.target().starts_with("hs_")
                    && metadata.target() != crate::spans::DETACHED_TARGET)
        }))
}

/// Reads a W3C `traceparent` (and `tracestate`) from `headers` and, when it names a valid
/// remote span, makes it `span`'s parent, so the request continues its caller's trace.
pub(crate) fn continue_remote_trace(headers: &http::HeaderMap, span: &tracing::Span) {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    if !headers.contains_key("traceparent") {
        return;
    }
    let propagator = opentelemetry_sdk::propagation::TraceContextPropagator::new();
    let cx = propagator.extract(&HeaderExtractor(headers));
    if cx.span().span_context().is_valid() {
        // Fails only when the span is already started or not an OpenTelemetry span (no OTLP
        // layer installed); either way the request is simply its own trace's root.
        let _ = span.set_parent(cx);
    }
}

/// [`opentelemetry::propagation::Extractor`] over an [`http::HeaderMap`].
struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use http::{Request, Response, StatusCode};
    use opentelemetry::trace::{SpanKind, Status};
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};
    use tower::{Layer as _, ServiceExt as _};
    use tracing_subscriber::layer::SubscriberExt as _;

    use crate::request_id::{RequestIdLayer, record_route};

    /// Keeps every exported span.
    #[derive(Debug, Clone, Default)]
    struct Capture(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Capture {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().unwrap().extend(batch);
            Ok(())
        }
    }

    const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    /// Sends one request through [`RequestIdLayer`] under a subscriber exporting to a
    /// [`Capture`] at `ratio`, with the handler naming its route, its user and answering
    /// `status`; returns what was exported.
    async fn one_request(
        ratio: f64,
        traceparent: Option<&str>,
        status: StatusCode,
    ) -> Vec<SpanData> {
        let capture = Capture::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(capture.clone())
            .with_sampler(sampler(ratio))
            .build();
        let subscriber = tracing_subscriber::registry().with(layer_from_provider(&provider));
        let _default = tracing::subscriber::set_default(subscriber);

        let svc = RequestIdLayer::new().layer(tower::service_fn(move |_req: Request<()>| {
            record_route(
                "PUT",
                "/_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}",
            );
            tracing::Span::current().record("matrix.user_id", "@alice:example.org");
            async move {
                // A child span lands under the request's.
                let _child = tracing::info_span!("room.send").entered();
                Ok::<_, std::convert::Infallible>(
                    Response::builder().status(status).body(()).unwrap(),
                )
            }
        }));
        let mut req = Request::builder()
            .method("PUT")
            .uri("/_matrix/client/v3/rooms/!r:example.org/send/m.room.message/1");
        if let Some(tp) = traceparent {
            req = req.header("traceparent", tp);
        }
        let _ = svc.oneshot(req.body(()).unwrap()).await.unwrap();
        provider.force_flush().unwrap();
        capture.0.lock().unwrap().clone()
    }

    fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    #[tokio::test]
    async fn a_request_is_a_server_span_named_by_its_route_that_continues_the_callers_trace() {
        let spans = one_request(1.0, Some(TRACEPARENT), StatusCode::INTERNAL_SERVER_ERROR).await;
        let request = spans
            .iter()
            .find(|s| s.span_kind == SpanKind::Server)
            .expect("the request's span was exported");
        assert_eq!(
            request.name,
            "PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}"
        );
        assert_eq!(
            request.span_context.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(request.parent_span_id.to_string(), "b7ad6b7169203331");
        assert_eq!(
            attr(request, "http.route")
                .map(ToString::to_string)
                .as_deref(),
            Some("/_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}")
        );
        assert_eq!(
            attr(request, "http.response.status_code")
                .map(ToString::to_string)
                .as_deref(),
            Some("500")
        );
        assert_eq!(
            attr(request, "matrix.user_id")
                .map(ToString::to_string)
                .as_deref(),
            Some("@alice:example.org")
        );
        assert_eq!(
            attr(request, "url.path")
                .map(ToString::to_string)
                .as_deref(),
            Some("/_matrix/client/v3/rooms/!r:example.org/send/m.room.message/1")
        );
        assert!(attr(request, "request_id").is_some());
        assert!(
            matches!(request.status, Status::Error { .. }),
            "{:?}",
            request.status
        );

        let child = spans
            .iter()
            .find(|s| s.name == "room.send")
            .expect("the child span was exported");
        assert_eq!(child.parent_span_id, request.span_context.span_id());
        assert_eq!(
            child.span_context.trace_id(),
            request.span_context.trace_id()
        );
    }

    #[tokio::test]
    async fn a_librarys_spans_are_not_exported_and_its_events_land_on_ours() {
        let capture = Capture::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(capture.clone())
            .with_sampler(sampler(1.0))
            .build();
        let subscriber = tracing_subscriber::registry().with(layer_from_provider(&provider));
        tracing::subscriber::with_default(subscriber, || {
            let _ours = tracing::info_span!(target: "hs_room::actor", "room.actor").entered();
            let _theirs =
                tracing::info_span!(target: "ruma_common::push", "flatten_value").entered();
            tracing::warn!(target: "ruma_common::push", "a library's warning");
            let _detached =
                tracing::info_span!(target: "hs_telemetry::detached", "outbound").entered();
        });
        provider.force_flush().unwrap();
        let spans = capture.0.lock().unwrap().clone();
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(spans[0].name, "room.actor");
        assert_eq!(spans[0].events.len(), 1, "{:?}", spans[0].events);
    }

    #[test]
    fn a_failing_export_is_counted_and_a_working_one_clears_the_count() {
        let watched = Watched::new(Capture::default(), "http://collector:4317");
        let failed: OTelSdkResult = Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
            "connection refused".into(),
        ));
        watched.observe(10, &failed);
        watched.observe(5, &failed);
        {
            let state = watched.state.lock().unwrap();
            assert_eq!((state.failed_batches, state.failed_spans), (2, 15));
            assert!(
                state.last_warned.is_some(),
                "the first failure is logged at once"
            );
        }
        watched.observe(3, &Ok(()));
        let state = watched.state.lock().unwrap();
        assert_eq!((state.failed_batches, state.failed_spans), (0, 0));
        assert!(state.last_warned.is_none());
    }

    #[tokio::test]
    async fn a_client_error_is_not_a_failed_span() {
        let spans = one_request(1.0, None, StatusCode::NOT_FOUND).await;
        let request = spans
            .iter()
            .find(|s| s.span_kind == SpanKind::Server)
            .unwrap();
        assert!(
            !matches!(request.status, Status::Error { .. }),
            "{:?}",
            request.status
        );
    }

    #[tokio::test]
    async fn at_ratio_zero_nothing_is_exported_unless_the_caller_sampled_it() {
        assert!(one_request(0.0, None, StatusCode::OK).await.is_empty());
        let continued = one_request(0.0, Some(TRACEPARENT), StatusCode::OK).await;
        assert_eq!(continued.len(), 2, "{continued:?}");
        let unsampled = TRACEPARENT.replace("-01", "-00");
        assert!(
            one_request(1.0, Some(&unsampled), StatusCode::OK)
                .await
                .is_empty()
        );
    }

    #[test]
    fn the_sampler_clamps_its_ratio() {
        assert!(format!("{:?}", sampler(2.0)).contains("TraceIdRatioBased(1.0)"));
        assert!(format!("{:?}", sampler(-1.0)).contains("TraceIdRatioBased(0.0)"));
        assert!(format!("{:?}", sampler(f64::NAN)).contains("TraceIdRatioBased(0.0)"));
        assert!(format!("{:?}", sampler(0.2)).contains("ParentBased"));
    }
}
