//! A fake federation peer: an axum router that accepts any method and path (`hs-federation`, the
//! real client, does not exist yet, so this cannot be a typed double over its request shapes),
//! records every request it receives, and answers with either a queued canned response or a
//! configurable default. Point `hs-federation`'s client at [`FakeFederationPeer::router`]'s
//! address once that track has an outbound client to test against; today it stands in for "some
//! other homeserver" in scenarios that only need to observe what was sent.
//!
//! Several independent fake peers can be created (`FakeFederationPeer::new("matrix.example.org")`
//! per remote server name) to test fan-out to multiple destinations.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Router, extract::Extension};
use serde_json::Value;

use crate::record_log::RecordLog;

/// One recorded inbound request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordedRequest {
    /// The HTTP method, as text (`"PUT"`, `"GET"`, ...).
    pub method: String,
    /// The request path plus query string, exactly as received.
    pub path: String,
    /// The body, parsed as JSON if possible; `null` for an empty body, or a JSON string of the
    /// raw bytes if the body was not valid JSON (federation transactions and PDUs are always
    /// JSON, but this double does not assume that).
    pub body: Value,
}

/// A response to hand back for the next matching (or next any) recorded request.
#[derive(Debug, Clone)]
pub struct CannedResponse {
    status: StatusCode,
    body: Value,
}

impl CannedResponse {
    /// A `200 OK` with the given JSON body.
    #[must_use]
    pub fn ok(body: Value) -> Self {
        Self {
            status: StatusCode::OK,
            body,
        }
    }

    /// An arbitrary status and JSON body (for example a `race`-shaped `404 M_NOT_FOUND` from a
    /// remote server).
    #[must_use]
    pub fn new(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }
}

struct Inner {
    log: RecordLog,
    queue: Mutex<VecDeque<CannedResponse>>,
    default_status: StatusCode,
}

/// A fake federation peer identified by `server_name` (informational only — nothing in this type
/// validates it against the `Host` header or `X-Matrix` signature of incoming requests; signature
/// verification is the concern of whatever test wires up a real signing peer later).
#[derive(Clone)]
pub struct FakeFederationPeer {
    server_name: Arc<str>,
    inner: Arc<Inner>,
}

impl FakeFederationPeer {
    /// A fresh fake peer that answers every request `200 {}` until responses are queued with
    /// [`FakeFederationPeer::queue_response`].
    #[must_use]
    pub fn new(server_name: impl Into<Arc<str>>) -> Self {
        Self {
            server_name: server_name.into(),
            inner: Arc::new(Inner {
                log: RecordLog::new(),
                queue: Mutex::new(VecDeque::new()),
                default_status: StatusCode::OK,
            }),
        }
    }

    /// This peer's configured server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Queues one response to be returned for the next request that does not have an
    /// already-queued response ahead of it (FIFO). Once the queue is drained, requests fall back
    /// to a `200 {}`.
    pub fn queue_response(&self, response: CannedResponse) {
        self.inner.queue.lock().unwrap().push_back(response);
    }

    /// The axum router: matches any method and path.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/{*path}", any(handle))
            .route("/", any(handle))
            .layer(Extension(self.inner.clone()))
    }

    /// Every request recorded so far, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.inner
            .log
            .all_as()
            .expect("this fake only ever writes RecordedRequest values")
    }

    /// How many requests have been recorded.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.inner.log.len()
    }
}

async fn handle(
    method: Method,
    uri: Uri,
    Extension(inner): Extension<Arc<Inner>>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed_body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    inner.log.record(&RecordedRequest {
        method: method.to_string(),
        path: uri.to_string(),
        body: parsed_body,
    });

    let next = inner.queue.lock().unwrap().pop_front();
    match next {
        Some(canned) => (canned.status, axum::Json(canned.body)).into_response(),
        None => (inner.default_status, axum::Json(serde_json::json!({}))).into_response(),
    }
}

// Silence the unused-import lint on `Body` — kept for callers building requests against this
// router directly in their own tests, re-exported for convenience.
#[allow(unused_imports)]
use Body as _FederationBody;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn records_requests_and_answers_default_ok() {
        let peer = FakeFederationPeer::new("federation.example.org");
        let router = peer.router();

        let response = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/_matrix/federation/v1/send/123")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"pdus": []}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let requests = peer.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "PUT");
        assert!(requests[0].path.contains("/send/123"));
    }

    #[tokio::test]
    async fn queued_responses_are_returned_fifo() {
        let peer = FakeFederationPeer::new("federation.example.org");
        peer.queue_response(CannedResponse::new(
            StatusCode::NOT_FOUND,
            serde_json::json!({"errcode": "M_NOT_FOUND"}),
        ));
        peer.queue_response(CannedResponse::ok(serde_json::json!({"ok": true})));

        let first = peer
            .router()
            .oneshot(Request::builder().uri("/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::NOT_FOUND);

        let second = peer
            .router()
            .oneshot(Request::builder().uri("/b").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);

        let third = peer
            .router()
            .oneshot(Request::builder().uri("/c").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(third.status(), StatusCode::OK);
        assert_eq!(peer.request_count(), 3);
    }
}
