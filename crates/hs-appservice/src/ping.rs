//! Ping, in both directions (`PLAN.md` section 8.1 point 3; spec MSC2659, stable since v1.7):
//!
//! - **Inbound**: an appservice calls `POST /_matrix/client/v1/appservice/{appserviceId}/ping` on
//!   us, authenticated with its own `as_token`, asking us to prove we can reach it.
//! - **Outbound**: in response, we call `POST {url}/_matrix/app/v1/ping` on the appservice,
//!   authenticated with its `hs_token` — this is the actual connectivity test; the inbound call is
//!   just what triggers it and reports the result back.
//!
//! [`PingService`] owns the outbound call and the health bookkeeping; [`routes::router`] is the
//! inbound axum route (mounted with `State<hs_auth::state::AuthState>` — see that module's docs
//! for why, and how a caller composes it with this crate's own registry state).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::error::AppserviceError;
use crate::registry::Registry;
use hs_kv::KvBackend;

/// Why an outbound ping call did not get a successful response, distinguished the way the spec's
/// `POST /_matrix/client/v1/appservice/{appserviceId}/ping` response requires
/// (`refs/matrix-spec/data/api/client-server/appservice_ping.yaml`): a timeout, a connection-level
/// failure (refused, DNS, TLS), or a response that came back but was not 2xx (`M_BAD_STATUS`,
/// which the spec says should carry the appservice's HTTP status and body back for debugging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingTransportError {
    /// The request timed out.
    Timeout,
    /// The request could not even be sent/completed (refused, DNS, TLS, ...).
    ConnectionFailed(String),
    /// The appservice responded, but not with 2xx.
    BadStatus {
        /// The HTTP status the appservice returned.
        status: u16,
        /// The response body, for debugging (spec: "may include `status` and `body` fields").
        body: String,
    },
}

/// Performs the outbound `POST {url}/_matrix/app/v1/ping` call. A trait so tests and
/// `hs-bridge-conformance` can substitute an in-process double, exactly like
/// [`crate::scheduler::TransactionSender`] (kept separate from that trait because ping and
/// transaction delivery are different spec endpoints with different bodies, even though both are
/// "call the appservice over HTTP").
#[async_trait]
pub trait PingTransport: Send + Sync {
    /// # Errors
    /// Returns [`PingTransportError`] on anything but a 2xx response.
    async fn ping(
        &self,
        url: &str,
        hs_token: &str,
        transaction_id: Option<&str>,
    ) -> Result<(), PingTransportError>;
}

/// The real HTTP ping transport.
pub struct HttpPingTransport {
    client: reqwest::Client,
}

impl Default for HttpPingTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpPingTransport {
    /// A transport with a 10-second timeout — pings should be fast; a bridge that takes longer
    /// than that to answer a ping is indistinguishable from unreachable for this purpose.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: hs_http::client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl PingTransport for HttpPingTransport {
    async fn ping(
        &self,
        url: &str,
        hs_token: &str,
        transaction_id: Option<&str>,
    ) -> Result<(), PingTransportError> {
        let base = url.trim_end_matches('/');
        let full_url = format!("{base}/_matrix/app/v1/ping");
        let mut body = serde_json::Map::new();
        if let Some(id) = transaction_id {
            body.insert(
                "transaction_id".to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }
        let response = self
            .client
            .post(&full_url)
            .bearer_auth(hs_token)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PingTransportError::Timeout
                } else {
                    PingTransportError::ConnectionFailed(e.to_string())
                }
            })?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            Err(PingTransportError::BadStatus { status, body })
        }
    }
}

/// Why a ping did not succeed, distinguished the way the spec's `POST
/// /_matrix/client/v1/appservice/{appserviceId}/ping` response requires: `M_URL_NOT_SET` (400),
/// `M_CONNECTION_TIMEOUT` (504), or `M_BAD_STATUS`/`M_CONNECTION_FAILED` (502).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PingFailure {
    /// The appservice's registration has `url: null` — there is nothing to ping. `400`.
    #[error("this appservice has no url set")]
    NoUrlSet,
    /// The outbound call timed out. `504`.
    #[error("timed out waiting for the appservice to respond")]
    Timeout,
    /// The outbound call could not complete (connection refused, DNS, TLS). `502`.
    #[error("failed to connect to the appservice: {0}")]
    ConnectionFailed(String),
    /// The appservice responded, but not with 2xx. `502`.
    #[error("ping returned status {status}")]
    BadStatus {
        /// The HTTP status the appservice returned.
        status: u16,
        /// The response body, for debugging.
        body: String,
    },
}

impl PingFailure {
    /// The spec's `errcode` for this failure.
    #[must_use]
    pub fn errcode(&self) -> &'static str {
        match self {
            Self::NoUrlSet => "M_URL_NOT_SET",
            Self::Timeout => "M_CONNECTION_TIMEOUT",
            Self::ConnectionFailed(_) => "M_CONNECTION_FAILED",
            Self::BadStatus { .. } => "M_BAD_STATUS",
        }
    }

    /// The spec's HTTP status for this failure.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NoUrlSet => 400,
            Self::Timeout => 504,
            Self::ConnectionFailed(_) | Self::BadStatus { .. } => 502,
        }
    }
}

impl From<PingTransportError> for PingFailure {
    fn from(e: PingTransportError) -> Self {
        match e {
            PingTransportError::Timeout => Self::Timeout,
            PingTransportError::ConnectionFailed(msg) => Self::ConnectionFailed(msg),
            PingTransportError::BadStatus { status, body } => Self::BadStatus { status, body },
        }
    }
}

/// A successful ping's round-trip time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingSuccess {
    /// Milliseconds elapsed between issuing the outbound call and receiving a 2xx response.
    pub duration_ms: u64,
}

/// Owns the outbound ping call and its health bookkeeping.
pub struct PingService<B: KvBackend> {
    registry: Arc<Registry<B>>,
    transport: Arc<dyn PingTransport>,
}

impl<B: KvBackend> PingService<B> {
    /// Builds a ping service over `registry`, calling out with `transport`.
    #[must_use]
    pub fn new(registry: Arc<Registry<B>>, transport: Arc<dyn PingTransport>) -> Self {
        Self {
            registry,
            transport,
        }
    }

    /// Pings `appservice_id`'s registered `url`, recording the result in its health row
    /// regardless of outcome. This is the single entry point both the inbound client route and
    /// the admin API's `POST /appservices/{id}/ping` call.
    ///
    /// # Errors
    /// Returns [`AppserviceError::NotFound`] if `appservice_id` is not registered. A failed ping
    /// itself is `Ok(Err(PingFailure))`, not an `AppserviceError` — an unreachable bridge is an
    /// expected, reportable outcome, not an operational failure of this service (the same
    /// reasoning as [`crate::scheduler::Scheduler::drain`]'s `Ok(DrainOutcome::Failed)`).
    pub async fn ping(
        &self,
        appservice_id: &str,
        transaction_id: Option<&str>,
    ) -> Result<Result<PingSuccess, PingFailure>, AppserviceError> {
        let row = self
            .registry
            .get(appservice_id)?
            .ok_or_else(|| AppserviceError::NotFound(appservice_id.to_string()))?;

        let now = self.registry.now_ms();
        let Some(url) = row.url else {
            self.record(appservice_id, now, Err(&PingFailure::NoUrlSet))?;
            return Ok(Err(PingFailure::NoUrlSet));
        };

        let start = Instant::now();
        let outcome = self
            .transport
            .ping(&url, &row.hs_token, transaction_id)
            .await;
        let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        match outcome {
            Ok(()) => {
                let success = PingSuccess {
                    duration_ms: elapsed_ms,
                };
                self.record(appservice_id, now, Ok(()))?;
                Ok(Ok(success))
            }
            Err(err) => {
                let failure = PingFailure::from(err);
                self.record(appservice_id, now, Err(&failure))?;
                Ok(Err(failure))
            }
        }
    }

    fn record(
        &self,
        appservice_id: &str,
        now_ms: u64,
        outcome: Result<(), &PingFailure>,
    ) -> Result<(), AppserviceError> {
        let mut health = self.registry.store().health(appservice_id)?;
        health.last_ping_at_ms = Some(now_ms);
        match outcome {
            Ok(()) => {
                health.last_ping_success = Some(true);
                // A ping that works ends the last failure. The error a page shows is the
                // current one, not the one from before the retry that succeeded: a bridge
                // whose first ping raced its own listener is healthy, not "healthy, with an
                // error".
                health.last_error = None;
            }
            Err(failure) => {
                health.last_ping_success = Some(false);
                health.last_error = Some(failure.to_string());
            }
        }
        self.registry.store().put_health(appservice_id, &health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::Namespaces;
    use crate::registration::Registration;
    use hs_auth::clock::FixedClock;
    use hs_kv::memory::MemoryBackend;
    use ruma::server_name;
    use std::sync::Mutex;

    struct MockTransport {
        result: Mutex<Result<(), PingTransportError>>,
        calls: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl MockTransport {
        fn ok() -> Self {
            Self {
                result: Mutex::new(Ok(())),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn failing(msg: &str) -> Self {
            Self {
                result: Mutex::new(Err(PingTransportError::ConnectionFailed(msg.to_string()))),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl PingTransport for MockTransport {
        async fn ping(
            &self,
            url: &str,
            hs_token: &str,
            transaction_id: Option<&str>,
        ) -> Result<(), PingTransportError> {
            self.calls.lock().unwrap().push((
                url.to_string(),
                hs_token.to_string(),
                transaction_id.map(str::to_string),
            ));
            self.result.lock().unwrap().clone()
        }
    }

    fn registry_with(id: &str, url: Option<&str>) -> Arc<Registry<MemoryBackend>> {
        let registry = Arc::new(
            Registry::open(MemoryBackend::new(), server_name!("example.org"))
                .unwrap()
                .with_clock(Arc::new(FixedClock::new(5000))),
        );
        registry
            .add(&Registration {
                id: id.to_string(),
                url: url.map(str::to_string),
                as_token: format!("as_{id}"),
                hs_token: format!("hs_{id}"),
                sender_localpart: format!("{id}bot"),
                rate_limited: true,
                namespaces: Namespaces::default(),
                protocols: vec![],
                receive_ephemeral: false,
                push_ephemeral_legacy: false,
                msc3202: false,
                msc4190: false,
                extra: Default::default(),
            })
            .unwrap();
        registry
    }

    #[tokio::test]
    async fn successful_ping_records_health_and_returns_duration() {
        let registry = registry_with("a", Some("http://bridge.local"));
        let transport = Arc::new(MockTransport::ok());
        let service = PingService::new(registry.clone(), transport.clone());

        let outcome = service.ping("a", Some("txn1")).await.unwrap();
        assert!(outcome.is_ok());

        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls[0].0, "http://bridge.local");
        assert_eq!(calls[0].1, "hs_a");
        assert_eq!(calls[0].2.as_deref(), Some("txn1"));
        drop(calls);

        let health = registry.health("a").unwrap();
        assert_eq!(health.last_ping_at_ms, Some(5000));
    }

    #[tokio::test]
    async fn null_url_ping_is_a_distinct_failure() {
        let registry = registry_with("dp", None);
        let transport = Arc::new(MockTransport::ok());
        let service = PingService::new(registry, transport.clone());

        let outcome = service.ping("dp", None).await.unwrap();
        assert_eq!(outcome, Err(PingFailure::NoUrlSet));
        assert!(transport.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn connection_failure_is_reported_and_recorded() {
        let registry = registry_with("a", Some("http://bridge.local"));
        let transport = Arc::new(MockTransport::failing("connection refused"));
        let service = PingService::new(registry.clone(), transport);

        let outcome = service.ping("a", None).await.unwrap();
        assert_eq!(outcome.unwrap_err().errcode(), "M_CONNECTION_FAILED");

        let health = registry.health("a").unwrap();
        assert!(health.last_error.is_some());
    }

    /// A mautrix bridge pings the moment its listener starts, and the first one can lose that
    /// race; it retries five seconds later. The health has to say what is true now.
    #[tokio::test]
    async fn a_ping_that_works_clears_the_error_from_the_one_before() {
        let registry = registry_with("a", Some("http://bridge.local"));
        let transport = Arc::new(MockTransport::failing("connection refused"));
        let service = PingService::new(registry.clone(), transport.clone());

        service.ping("a", None).await.unwrap().unwrap_err();
        assert!(registry.health("a").unwrap().last_error.is_some());

        *transport.result.lock().unwrap() = Ok(());
        service.ping("a", None).await.unwrap().unwrap();
        let health = registry.health("a").unwrap();
        assert!(health.last_error.is_none(), "{health:?}");
    }

    #[tokio::test]
    async fn unknown_appservice_is_not_found() {
        let registry = registry_with("a", Some("http://bridge.local"));
        let transport = Arc::new(MockTransport::ok());
        let service = PingService::new(registry, transport);
        assert!(matches!(
            service.ping("nope", None).await.unwrap_err(),
            AppserviceError::NotFound(id) if id == "nope"
        ));
    }
}
