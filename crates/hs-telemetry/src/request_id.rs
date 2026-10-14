//! Request-id propagation: a [`tower::Layer`] that ensures every request carries an
//! `X-Request-Id` header, echoes it back on the response, and attaches it to the
//! [`tracing::Span`] every downstream handler and log line runs inside.
//!
//! Mount [`RequestIdLayer`] outermost (or as close to outermost as the listener setup allows) so
//! every span opened further in — including ones opened by `tower-http`'s own `TraceLayer`, if a
//! consumer stacks one alongside this — can pick up `request_id` as a field. A caller-supplied
//! `X-Request-Id` is trusted and reused verbatim (matching how a reverse proxy or an upstream
//! federation peer is expected to have already minted one); a request with no header gets a fresh
//! random one.
//!
//! This is deliberately a plain [`tower::Layer`], not an axum-specific extractor: it works
//! unmodified on any `http::Request`/`http::Response` service, so the same layer wraps the
//! `/_matrix/client` router, the federation router and the metrics listener alike.

use std::task::{Context, Poll};

use http::{HeaderName, HeaderValue, Request, Response};
use tower::{Layer, Service};

/// The header name this layer reads and writes: `x-request-id`, the de facto standard used by
/// most reverse proxies (nginx, Envoy, Traefik) and by Synapse's own request logging.
pub static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Generates a request id: 16 bytes of randomness, hex-encoded. Not a UUID (no need for the
/// dashes or the version/variant bits) but the same 128 bits of entropy.
fn generate_request_id() -> String {
    let mut bytes = [0u8; 16];
    rand::Rng::fill(&mut rand::rng(), &mut bytes);
    hex::encode(bytes)
}

/// A [`tower::Layer`] that wraps a service with [`RequestIdService`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdLayer;

impl RequestIdLayer {
    /// A new layer.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService { inner }
    }
}

/// The [`tower::Service`] [`RequestIdLayer`] produces. Reads or generates a request id, attaches
/// it to a tracing span around the inner call, and stamps it onto the response headers.
#[derive(Debug, Clone)]
pub struct RequestIdService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RequestIdService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let request_id = req
            .headers()
            .get(&REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(generate_request_id);

        if let Ok(value) = HeaderValue::from_str(&request_id) {
            req.headers_mut().insert(REQUEST_ID_HEADER.clone(), value);
        }

        let span = request_span(&req, &request_id);
        let response_id = request_id.clone();
        let fut = {
            let _entered = span.enter();
            self.inner.call(req)
        };

        let record_on = span.clone();
        Box::pin(tracing::Instrument::instrument(
            async move {
                let mut response = fut.await?;
                record_status(&record_on, response.status());
                if let Ok(value) = HeaderValue::from_str(&response_id) {
                    response
                        .headers_mut()
                        .insert(REQUEST_ID_HEADER.clone(), value);
                }
                Ok(response)
            },
            span,
        ))
    }
}

/// The fields of the span every request runs in, by name. A span's field must be declared when
/// the span is made for a later `record` to land, so these are all declared (most of them empty)
/// by [`RequestIdService`]; code further in fills them with `tracing::Span::current().record(..)`.
/// Named after OpenTelemetry's HTTP semantic conventions where one exists.
pub mod fields {
    /// The request id ([`super::REQUEST_ID_HEADER`]).
    pub const REQUEST_ID: &str = "request_id";
    /// `GET`, `PUT`, ...
    pub const METHOD: &str = "http.request.method";
    /// The matched route template (`/_matrix/client/v3/rooms/{roomId}/send/...`), never the raw
    /// path. Recorded by whatever routes the request (`hs-cli`'s metrics middleware).
    pub const ROUTE: &str = "http.route";
    /// The response's status code.
    pub const STATUS: &str = "http.response.status_code";
    /// The authenticated Matrix user, for a client or admin request.
    pub const USER_ID: &str = "matrix.user_id";
    /// The authenticated origin server, for a federation request.
    pub const ORIGIN: &str = "matrix.origin";
    /// Who called the admin API: the principal's id (a user id, a client's or a service
    /// account's id). Recorded by `hs-admin`'s scope check.
    pub const ADMIN_PRINCIPAL: &str = "admin.principal";
    /// The admin API principal's kind: `user`, `client`, `service_account` or `legacy`.
    pub const ADMIN_PRINCIPAL_KIND: &str = "admin.principal_kind";
    /// The OpenTelemetry span name (`GET /_matrix/client/v3/sync`), `tracing-opentelemetry`'s
    /// special field.
    pub const OTEL_NAME: &str = "otel.name";
}

/// Makes the span a request runs in: a root span (or the child of the caller's span, when the
/// request carries a W3C `traceparent` and OTLP export is compiled in) of kind `server`, named
/// by its method until the router names its route ([`record_route`]).
fn request_span<B>(req: &Request<B>, request_id: &str) -> tracing::Span {
    let method = req.method().as_str();
    let span = tracing::info_span!(
        parent: None,
        "request",
        request_id = %request_id,
        "http.request.method" = method,
        "http.route" = tracing::field::Empty,
        "http.response.status_code" = tracing::field::Empty,
        "matrix.user_id" = tracing::field::Empty,
        "matrix.origin" = tracing::field::Empty,
        "admin.principal" = tracing::field::Empty,
        "admin.principal_kind" = tracing::field::Empty,
        "otel.name" = method,
        "otel.kind" = "server",
        "otel.status_code" = tracing::field::Empty,
        "error.type" = tracing::field::Empty,
    );
    #[cfg(feature = "otlp")]
    if !span.is_disabled() {
        crate::otlp::continue_remote_trace(req.headers(), &span);
        // An attribute of the exported span only, not a field: it would otherwise be repeated
        // on every log line the request writes.
        tracing_opentelemetry::OpenTelemetrySpanExt::set_attribute(
            &span,
            "url.path",
            req.uri().path().to_owned(),
        );
    }
    span
}

/// Records the response's status on the request's span; a 5xx marks the span as failed.
fn record_status(span: &tracing::Span, status: http::StatusCode) {
    span.record(fields::STATUS, status.as_u16());
    if status.is_server_error() {
        span.record("otel.status_code", "error");
        span.record("error.type", status.as_str());
    }
}

/// Names the current request's route: records [`fields::ROUTE`] and renames the span
/// `METHOD route`. Call it from inside the router, where the matched template is known, while
/// the request's span is the current one.
pub fn record_route(method: &str, route: &str) {
    let span = tracing::Span::current();
    let name = format!("{method} {route}");
    span.record(fields::ROUTE, route);
    span.record(fields::OTEL_NAME, name.as_str());
    // `tracing-opentelemetry` takes `otel.name` only until the span has started (been entered,
    // or had a child), which by the time a request is routed it has; rename the started span.
    #[cfg(feature = "otlp")]
    if !span.is_disabled() {
        use opentelemetry::trace::TraceContextExt as _;
        tracing_opentelemetry::OpenTelemetrySpanExt::context(&span)
            .span()
            .update_name(name);
    }
}

/// Reads the request id a [`RequestIdService`] attached to this request, for a handler that wants
/// to log or return it explicitly rather than relying on the ambient span field.
#[must_use]
pub fn request_id_from_headers(headers: &http::HeaderMap) -> Option<String> {
    headers
        .get(&REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use tower::ServiceExt;

    async fn ok_service(req: Request<()>) -> Result<Response<()>, std::convert::Infallible> {
        // Echo whether the inner service already sees a request id (it should).
        assert!(request_id_from_headers(req.headers()).is_some());
        Ok(Response::builder().status(StatusCode::OK).body(()).unwrap())
    }

    #[tokio::test]
    async fn generates_a_request_id_when_absent() {
        let svc = RequestIdLayer::new().layer(tower::service_fn(ok_service));
        let req = Request::builder().body(()).unwrap();
        let res = svc.oneshot(req).await.unwrap();
        assert!(request_id_from_headers(res.headers()).is_some());
    }

    #[tokio::test]
    async fn reuses_a_caller_supplied_request_id() {
        let svc = RequestIdLayer::new().layer(tower::service_fn(ok_service));
        let req = Request::builder()
            .header(REQUEST_ID_HEADER.clone(), "caller-supplied-id")
            .body(())
            .unwrap();
        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(
            request_id_from_headers(res.headers()).as_deref(),
            Some("caller-supplied-id")
        );
    }

    #[tokio::test]
    async fn two_requests_without_a_header_get_different_ids() {
        let svc = RequestIdLayer::new().layer(tower::service_fn(ok_service));
        let res1 = svc
            .clone()
            .oneshot(Request::builder().body(()).unwrap())
            .await
            .unwrap();
        let res2 = svc
            .oneshot(Request::builder().body(()).unwrap())
            .await
            .unwrap();
        assert_ne!(
            request_id_from_headers(res1.headers()),
            request_id_from_headers(res2.headers())
        );
    }
}
