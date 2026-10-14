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

    let queue = crate::export_stats::Queue::default();
    let mut watched = Watched::new(exporter, endpoint);
    watched.queue = Some(queue.clone());
    let processor = BatchSpanProcessor::builder(watched)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(MAX_QUEUE_SIZE)
                .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
                .with_scheduled_delay(SCHEDULED_DELAY)
                .build(),
        )
        .build();

    let provider = SdkTracerProvider::builder()
        .with_span_processor(Counted {
            inner: processor,
            queue,
        })
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
    /// The pipeline's queue, given back each export's places and counted on `/metrics`.
    queue: Option<crate::export_stats::Queue>,
}

/// The span processor in front of the batch processor: counts each finished span into the
/// queue ([`crate::export_stats`]), and drops it (counted) rather than hand it on when the
/// queue is full. So `/metrics` says how many spans were dropped, which the batch processor
/// keeps to itself.
#[derive(Debug)]
struct Counted<P> {
    inner: P,
    queue: crate::export_stats::Queue,
}

impl<P: opentelemetry_sdk::trace::SpanProcessor> opentelemetry_sdk::trace::SpanProcessor
    for Counted<P>
{
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: opentelemetry_sdk::trace::SpanData) {
        if self.queue.try_enqueue() {
            self.inner.on_end(span);
        }
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
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
            queue: None,
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
        if let Some(queue) = &self.queue {
            queue.exported(spans, result.is_ok());
        }
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

/// The OpenTelemetry span context of `span`: invalid when no OTLP layer is installed, when the
/// span is disabled, or when its target is not exported. Starts the span if it had not started.
pub(crate) fn span_context(span: &tracing::Span) -> opentelemetry::trace::SpanContext {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    if span.is_none() {
        return opentelemetry::trace::SpanContext::empty_context();
    }
    span.context().span().span_context().clone()
}

/// A W3C `traceparent` for `cx`, sampled flag included.
pub(crate) fn traceparent(cx: &opentelemetry::trace::SpanContext) -> Option<String> {
    cx.is_valid().then(|| {
        format!(
            "00-{}-{}-{:02x}",
            cx.trace_id(),
            cx.span_id(),
            cx.trace_flags().to_u8()
        )
    })
}

/// Parses a W3C `traceparent` into the remote span context it names; invalid when it is not one.
pub(crate) fn parse_traceparent(value: &str) -> opentelemetry::trace::SpanContext {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;

    let propagator = opentelemetry_sdk::propagation::TraceContextPropagator::new();
    let cx = propagator.extract(&OneHeader(value));
    cx.span().span_context().clone()
}

/// Makes `span`, which must not have started, a child of the remote span `remote`.
pub(crate) fn set_remote_parent(span: &tracing::Span, remote: opentelemetry::trace::SpanContext) {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    if remote.is_valid() && !span.is_none() {
        // Fails only when the span already started or has no OpenTelemetry data; it is then
        // its own trace's root, which is what it would have been anyway.
        let _ = span.set_parent(opentelemetry::Context::new().with_remote_span_context(remote));
    }
}

/// Links `span` to `cx` (a span that caused it, in another trace).
pub(crate) fn add_link(span: &tracing::Span, cx: opentelemetry::trace::SpanContext) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    if cx.is_valid() && !span.is_none() {
        span.add_link(cx);
    }
}

/// [`opentelemetry::propagation::Extractor`] over a lone `traceparent` value.
struct OneHeader<'a>(&'a str);

impl opentelemetry::propagation::Extractor for OneHeader<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        (key == "traceparent").then_some(self.0)
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent"]
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

    /// Runs `f` under a subscriber exporting to a [`Capture`] at `ratio`; returns what it
    /// returned and what was exported.
    fn under<T>(ratio: f64, f: impl FnOnce() -> T) -> (T, Vec<SpanData>) {
        let capture = Capture::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(capture.clone())
            .with_sampler(sampler(ratio))
            .build();
        let subscriber = tracing_subscriber::registry().with(layer_from_provider(&provider));
        let out = tracing::subscriber::with_default(subscriber, f);
        provider.force_flush().unwrap();
        let spans = capture.0.lock().unwrap().clone();
        (out, spans)
    }

    #[test]
    fn sampled_is_true_only_inside_a_sampled_trace() {
        let ((outside, inside), _) = under(1.0, || {
            let outside = crate::current_is_sampled();
            let _span = tracing::info_span!(target: "hs_test", "pass").entered();
            (outside, crate::current_is_sampled())
        });
        assert!(!outside, "no current span");
        assert!(inside);
        let (unsampled, spans) = under(0.0, || {
            let _span = tracing::info_span!(target: "hs_test", "pass").entered();
            crate::current_is_sampled()
        });
        assert!(!unsampled);
        assert!(spans.is_empty());
        // No OpenTelemetry layer at all: a span, but nothing records it.
        let subscriber = tracing_subscriber::registry();
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!(target: "hs_test", "pass").entered();
            assert!(!crate::current_is_sampled());
            assert!(crate::current_traceparent().is_none());
        });
    }

    #[test]
    fn a_traceparent_passed_on_continues_the_trace_on_the_other_side() {
        let ((), spans) = under(1.0, || {
            let traceparent = {
                let _forwarding = tracing::info_span!(target: "hs_test", "forwarding").entered();
                crate::current_traceparent().expect("a traceparent")
            };
            assert!(traceparent.starts_with("00-") && traceparent.ends_with("-01"));
            let handling = tracing::info_span!(target: "hs_test", parent: None, "handling");
            crate::continue_trace(&handling, &traceparent);
            drop(handling.entered());
        });
        let forwarding = spans.iter().find(|s| s.name == "forwarding").unwrap();
        let handling = spans.iter().find(|s| s.name == "handling").unwrap();
        assert_eq!(
            handling.span_context.trace_id(),
            forwarding.span_context.trace_id()
        );
        assert_eq!(handling.parent_span_id, forwarding.span_context.span_id());

        // An unsampled trace is passed on unsampled, and the other side keeps it so.
        let ((), spans) = under(0.0, || {
            let traceparent = {
                let _forwarding = tracing::info_span!(target: "hs_test", "forwarding").entered();
                crate::current_traceparent().expect("a traceparent")
            };
            assert!(traceparent.ends_with("-00"), "{traceparent}");
            let handling = tracing::info_span!(target: "hs_test", parent: None, "handling");
            crate::continue_trace(&handling, &traceparent);
            drop(handling.entered());
        });
        assert!(spans.is_empty(), "{spans:?}");
    }

    #[test]
    fn a_pass_links_to_the_requests_that_caused_it() {
        let ((), spans) = under(1.0, || {
            let links: Vec<crate::TraceLink> = (0..40)
                .map(|_| {
                    let _request = tracing::info_span!(target: "hs_test", "request").entered();
                    crate::TraceLink::current()
                })
                .collect();
            assert!(crate::TraceLink::current().is_empty(), "outside a span");
            let pass = tracing::info_span!(
                target: "hs_test",
                parent: None,
                "pass",
                links_dropped = tracing::field::Empty
            );
            crate::add_links(&pass, &links);
            drop(pass.entered());
        });
        let pass = spans.iter().find(|s| s.name == "pass").unwrap();
        assert_eq!(pass.links.links.len(), crate::spans::MAX_LINKS_PER_SPAN);
        let request = spans.iter().find(|s| s.name == "request").unwrap();
        assert!(
            pass.links
                .links
                .iter()
                .any(|l| l.span_context.trace_id() == request.span_context.trace_id())
        );
        assert_ne!(
            pass.span_context.trace_id(),
            request.span_context.trace_id()
        );
        assert_eq!(
            attr(pass, "links_dropped")
                .map(ToString::to_string)
                .as_deref(),
            Some("8")
        );
        let link = crate::TraceLink::from_traceparent(TRACEPARENT);
        assert_eq!(link.traceparent().as_deref(), Some(TRACEPARENT));
        assert!(crate::TraceLink::from_traceparent("nonsense").is_empty());
    }

    #[test]
    fn a_full_queue_drops_spans_in_front_of_the_batch_processor() {
        let capture = Capture::default();
        let queue = crate::export_stats::Queue::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(Counted {
                inner: opentelemetry_sdk::trace::SimpleSpanProcessor::new(capture.clone()),
                queue: queue.clone(),
            })
            .with_sampler(sampler(1.0))
            .build();
        // Fill the queue: nothing is exported through `Watched`, so no place is given back.
        for _ in 0..crate::export_stats::MAX_QUEUED - 1 {
            assert!(queue.try_enqueue());
        }
        let dropped = crate::export_stats::spans_dropped();
        let subscriber = tracing_subscriber::registry().with(layer_from_provider(&provider));
        tracing::subscriber::with_default(subscriber, || {
            drop(tracing::info_span!(target: "hs_test", "kept").entered());
            drop(tracing::info_span!(target: "hs_test", "dropped").entered());
        });
        provider.force_flush().unwrap();
        let names: Vec<_> = capture
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(names, vec!["kept"]);
        assert!(crate::export_stats::spans_dropped() > dropped);
    }

    /// What guarding a small span with [`crate::current_is_sampled`] costs, against opening the
    /// span unguarded, under the subscriber `hs` runs (level filter, JSON log layer, OTLP
    /// layer) inside a request span that is sampled or not. Run by hand:
    /// `cargo test -p hs-telemetry --features otlp --release --lib -- --ignored --nocapture
    /// cost_of`.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn cost_of_a_guarded_span() {
        const N: u32 = 200_000;
        for ratio in [0.0, 1.0] {
            let provider = SdkTracerProvider::builder()
                .with_span_processor(Counted {
                    inner: opentelemetry_sdk::trace::SimpleSpanProcessor::new(Capture::default()),
                    queue: crate::export_stats::Queue::default(),
                })
                .with_sampler(sampler(ratio))
                .build();
            let subscriber = tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new("info"))
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_span_list(true)
                        .with_writer(std::io::sink),
                )
                .with(layer_from_provider(&provider));
            tracing::subscriber::with_default(subscriber, || {
                let _request = tracing::info_span!(target: "hs_test", "request").entered();
                let time = |label: &str, f: &dyn Fn()| {
                    let started = std::time::Instant::now();
                    for _ in 0..N {
                        f();
                    }
                    let ns = started.elapsed().as_nanos() / u128::from(N);
                    println!("ratio {ratio}: {label}: {ns} ns");
                };
                time("nothing", &|| {
                    std::hint::black_box(());
                });
                time("current_is_sampled()", &|| {
                    std::hint::black_box(crate::current_is_sampled());
                });
                time("a span, unguarded", &|| {
                    let span = tracing::info_span!(target: "hs_test", "kv.transaction", writes = 3);
                    drop(std::hint::black_box(span.entered()));
                });
                time("a span, guarded", &|| {
                    let span = if crate::current_is_sampled() {
                        tracing::info_span!(target: "hs_test", "kv.transaction", writes = 3)
                    } else {
                        tracing::Span::none()
                    };
                    drop(std::hint::black_box(span.entered()));
                });
            });
            // The Capture held every sampled span; drop them before the next round.
            drop(provider);
        }
    }

    #[test]
    fn the_sampler_clamps_its_ratio() {
        assert!(format!("{:?}", sampler(2.0)).contains("TraceIdRatioBased(1.0)"));
        assert!(format!("{:?}", sampler(-1.0)).contains("TraceIdRatioBased(0.0)"));
        assert!(format!("{:?}", sampler(f64::NAN)).contains("TraceIdRatioBased(0.0)"));
        assert!(format!("{:?}", sampler(0.2)).contains("ParentBased"));
    }
}
