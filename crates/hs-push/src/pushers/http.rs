//! HTTP pushers: posting to a Push Gateway API-compatible gateway (Sygnal, or any other), with
//! retry and backoff. Per `docs/decisions/0007-build-less-reuse-more.md`, this homeserver does
//! not build a gateway — this module is the client side only, and its notification payload is
//! built from `ruma::api::push_gateway::send_event_notification::v1`'s types
//! (`Notification`, `Device`, `NotificationCounts`, `PusherData`), not a hand-rolled JSON shape.
//!
//! The envelope around those types (the actual `reqwest` POST, retry loop and response parsing)
//! is hand-written rather than routed through Ruma's `OutgoingRequest`/HTTP-client machinery:
//! that machinery exists to plug into `ruma_client`'s own HTTP layer and its access-token /
//! Matrix-version negotiation, neither of which applies to an unauthenticated, unversioned push
//! gateway POST — adopting it here would be an adapter layer with no behavioral benefit, the kind
//! of "heavy framework to avoid writing forty lines" decision 0007 itself warns against.
//!
//! Tested in this module against a fake gateway (`hs_testkit::FakePushGateway`, and small local
//! axum servers for the retry/backoff paths a fixed fake cannot express), never a real one, per
//! this track's brief.

use std::time::Duration;

use http::StatusCode;
use ruma::api::push_gateway::send_event_notification::v1::Notification;
use serde::{Deserialize, Serialize};

/// Retry and backoff tuning for [`HttpPusherClient::notify`]. Mirrors the shape of
/// `hs_kv::TransactConfig`: exponential backoff from `base_backoff`, capped at `max_backoff`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum number of attempts (the first try plus retries). Must be at least 1.
    pub max_attempts: u32,
    /// Backoff before the second attempt; doubles each subsequent retry, capped at
    /// `max_backoff`.
    pub base_backoff: Duration,
    /// Backoff never grows past this.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

fn backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    let scale = 1u32
        .checked_shl(attempt.saturating_sub(1).min(20))
        .unwrap_or(u32::MAX);
    policy
        .base_backoff
        .saturating_mul(scale)
        .min(policy.max_backoff)
}

/// Why [`HttpPusherClient::notify`] failed to deliver a notification.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// Every attempt failed (a transport error, or a `5xx`/other non-2xx status the gateway
    /// itself didn't attribute to a specific rejected pushkey).
    #[error("push gateway request failed after {attempts} attempt(s): {reason}")]
    Exhausted {
        /// How many attempts were made.
        attempts: u32,
        /// The last failure's description.
        reason: String,
    },
    /// The gateway responded with a `4xx` the spec says is not worth retrying (a malformed
    /// request on our side — retrying it would just fail the same way every time).
    #[error("push gateway rejected the request outright ({status}): {body}")]
    ClientError {
        /// The HTTP status the gateway returned.
        status: StatusCode,
        /// Its response body, for diagnostics.
        body: String,
    },
}

#[derive(Serialize)]
struct NotifyRequestBody<'a> {
    notification: &'a Notification,
}

#[derive(Deserialize, Default)]
struct NotifyResponseBody {
    #[serde(default)]
    rejected: Vec<String>,
}

/// Posts event notifications to a Push Gateway API-compatible gateway (`POST
/// /_matrix/push/v1/notify`), retrying transient failures with exponential backoff.
pub struct HttpPusherClient {
    http: reqwest::Client,
    retry: RetryPolicy,
}

impl HttpPusherClient {
    /// A client with the given retry policy, using a fresh `reqwest::Client`.
    #[must_use]
    pub fn new(retry: RetryPolicy) -> Self {
        Self {
            http: hs_http::client::builder().build().unwrap_or_default(),
            retry,
        }
    }

    /// Posts `notification` to `gateway_url` (the pusher's configured `data.url`, per the spec:
    /// the full URL including `/_matrix/push/v1/notify`), retrying on transport errors and `5xx`
    /// responses.
    ///
    /// Returns the gateway's `rejected` pushkey list on any `2xx` response (the spec's mechanism
    /// for "this pushkey is stale, delete the pusher" — the caller, not this function, is
    /// responsible for deleting pushers named there, since this function has no store access).
    ///
    /// # Errors
    /// [`NotifyError::ClientError`] on a `4xx` (not retried); [`NotifyError::Exhausted`] if every
    /// attempt up to `RetryPolicy::max_attempts` failed.
    pub async fn notify(
        &self,
        gateway_url: &str,
        notification: &Notification,
    ) -> Result<Vec<String>, NotifyError> {
        let body = NotifyRequestBody { notification };
        let mut attempt = 0u32;
        let mut last_reason: String;
        loop {
            attempt += 1;
            match self.http.post(gateway_url).json(&body).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        let parsed: NotifyResponseBody = response.json().await.unwrap_or_default();
                        return Ok(parsed.rejected);
                    }
                    if status.is_client_error() {
                        let body_text = response.text().await.unwrap_or_default();
                        return Err(NotifyError::ClientError {
                            status,
                            body: body_text,
                        });
                    }
                    last_reason = format!("gateway responded with status {status}");
                }
                Err(e) => {
                    last_reason = e.to_string();
                }
            }
            if attempt >= self.retry.max_attempts {
                return Err(NotifyError::Exhausted {
                    attempts: attempt,
                    reason: last_reason,
                });
            }
            tokio::time::sleep(backoff(&self.retry, attempt)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use ruma::api::push_gateway::send_event_notification::v1::Device;

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}/_matrix/push/v1/notify")
    }

    fn sample_notification() -> Notification {
        Notification::new(vec![Device::new(
            "com.example.app".to_owned(),
            "abc123".to_owned(),
        )])
    }

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
        }
    }

    #[tokio::test]
    async fn delivers_and_surfaces_rejected_pushkeys_from_a_fake_gateway() {
        let gateway = hs_testkit::FakePushGateway::new();
        gateway.reject_pushkey("abc123");
        let url = serve(gateway.router()).await;

        let client = HttpPusherClient::new(fast_retry());
        let rejected = client.notify(&url, &sample_notification()).await.unwrap();
        assert_eq!(rejected, vec!["abc123".to_owned()]);
        assert_eq!(gateway.notifications().len(), 1);
    }

    #[tokio::test]
    async fn retries_server_errors_and_eventually_succeeds() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let router = Router::new().route(
            "/_matrix/push/v1/notify",
            post(move || {
                let counted = counted.clone();
                async move {
                    let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 {
                        (StatusCode::INTERNAL_SERVER_ERROR, "try again").into_response()
                    } else {
                        axum::Json(serde_json::json!({"rejected": []})).into_response()
                    }
                }
            }),
        );
        let url = serve(router).await;

        let client = HttpPusherClient::new(fast_retry());
        let rejected = client.notify(&url, &sample_notification()).await.unwrap();
        assert!(rejected.is_empty());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts_on_persistent_server_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let router = Router::new().route(
            "/_matrix/push/v1/notify",
            post(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        );
        let url = serve(router).await;

        let client = HttpPusherClient::new(fast_retry());
        let err = client
            .notify(&url, &sample_notification())
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::Exhausted { attempts: 4, .. }));
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn does_not_retry_a_client_error() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let router = Router::new().route(
            "/_matrix/push/v1/notify",
            post(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::BAD_REQUEST, "malformed").into_response()
                }
            }),
        );
        let url = serve(router).await;

        let client = HttpPusherClient::new(fast_retry());
        let err = client
            .notify(&url, &sample_notification())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            NotifyError::ClientError {
                status: StatusCode::BAD_REQUEST,
                ..
            }
        ));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a 4xx must not be retried"
        );
    }
}
