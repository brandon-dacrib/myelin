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

        let span = tracing::info_span!("request", request_id = %request_id);
        let response_id = request_id.clone();
        let fut = {
            let _entered = span.clone();
            self.inner.call(req)
        };

        Box::pin(tracing::Instrument::instrument(
            async move {
                let mut response = fut.await?;
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
