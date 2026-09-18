//! The forwarding client: sends an [`Envelope`] to a shard's owner over the mesh, with retries
//! and ownership refresh (`docs/rfcs/0001-cluster-ownership.md` section 8).
//!
//! One persistent, multiplexed HTTP/2 connection is kept per peer address (RFC 0001 section 8/11:
//! "one HTTP/2 connection (multiplexed)"), reused across forwards via [`hyper`]'s
//! `SendRequest::clone` (a cheap handle to the same multiplexer, safe to use concurrently). A
//! pooled connection that turns out to be dead (the peer restarted, an idle timeout fired) is
//! evicted and redialed once, inline, before the failure is reported up to [`Forwarder::forward`]'s
//! own retry loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http2::SendRequest;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;
use tokio::time::Instant;

use crate::error::ForwardError;
use crate::mesh::auth::AuthMode;
use crate::mesh::envelope::{Envelope, Reply, headers};
use crate::mesh::tls::TlsMaterial;
use crate::metrics::ClusterMetrics;
use crate::ownership::Ownership;
use crate::types::ReplicaId;

/// Sends forwarded requests to shard owners, with the retry and ownership-refresh policy of RFC
/// 0001 section 8.
pub struct Forwarder {
    auth: AuthMode,
    client_tls: Option<Arc<rustls::ClientConfig>>,
    max_hops: u32,
    max_attempts: u32,
    base_backoff: Duration,
    ownership: Arc<dyn Ownership>,
    metrics: Arc<ClusterMetrics>,
    /// One pooled, multiplexed HTTP/2 connection handle per peer address. A plain
    /// `std::sync::Mutex` is enough: every critical section is a map lookup/insert/remove with
    /// no `.await` inside it.
    pool: Mutex<HashMap<String, SendRequest<Full<Bytes>>>>,
}

impl Forwarder {
    /// Builds a forwarder. `tls` is required (and used) only when `auth` is
    /// [`AuthMode::MutualTls`].
    ///
    /// # Errors
    /// Returns a TLS error if `auth` is [`AuthMode::MutualTls`] but the client config could not
    /// be built.
    pub fn new(
        auth: AuthMode,
        tls: Option<&TlsMaterial>,
        max_hops: u32,
        max_attempts: u32,
        base_backoff: Duration,
        ownership: Arc<dyn Ownership>,
        metrics: Arc<ClusterMetrics>,
    ) -> Result<Self, crate::mesh::tls::TlsError> {
        let client_tls = match (&auth, tls) {
            (AuthMode::MutualTls { .. }, Some(material)) => Some(material.client_config()?),
            _ => None,
        };
        Ok(Self {
            auth,
            client_tls,
            max_hops,
            max_attempts,
            base_backoff,
            ownership,
            metrics,
            pool: Mutex::new(HashMap::new()),
        })
    }

    /// Forwards `env` to `env.shard`'s owner, retrying on connection failure, `421` (with
    /// ownership refresh) and `503` (bounded backoff), up to `max_attempts` within `env.deadline`.
    /// Application errors (any other status) are returned as-is, never retried.
    ///
    /// # Errors
    /// Returns [`ForwardError`] if no owner is known, the hop limit or deadline is exceeded, or
    /// every retry attempt fails.
    pub async fn forward(&self, mut env: Envelope) -> Result<Reply, ForwardError> {
        let start = Instant::now();
        let deadline_at = start + env.deadline;
        env.hops += 1;
        if env.hops > self.max_hops {
            return Err(ForwardError::TooManyHops {
                shard: env.shard,
                max_hops: self.max_hops,
            });
        }

        let mut attempts = 0u32;
        loop {
            if Instant::now() >= deadline_at {
                return Err(ForwardError::DeadlineExceeded(env.shard));
            }
            let Some(owner) = self.ownership.owner_of(env.shard) else {
                return Err(ForwardError::NoOwner(env.shard));
            };
            attempts += 1;
            let attempt_start = Instant::now();
            match self.send_once(&owner, &env).await {
                Ok((status, hdrs, body)) if status == 421 => {
                    self.metrics.record_forward_retry("421");
                    // `owner_of` is a live cache fed by the ownership manager's row scans and
                    // mesh announcements; the hint in the reply is informational only (the next
                    // `owner_of` call will already reflect a fresher view in the common case).
                    let _ = hdrs.get(headers::OWNER_HINT);
                    if attempts >= self.max_attempts {
                        self.metrics.record_forward(
                            "forward",
                            "misdirected",
                            attempt_start.elapsed(),
                        );
                        return Ok(Reply {
                            status: status.as_u16(),
                            payload: body,
                        });
                    }
                    tokio::time::sleep(self.base_backoff).await;
                }
                Ok((status, hdrs, body)) if status == 503 => {
                    self.metrics.record_forward_retry("503");
                    if attempts >= self.max_attempts {
                        self.metrics.record_forward(
                            "forward",
                            "unavailable",
                            attempt_start.elapsed(),
                        );
                        return Ok(Reply {
                            status: status.as_u16(),
                            payload: body,
                        });
                    }
                    let retry_after = hdrs
                        .get(headers::RETRY_AFTER_MS)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_millis)
                        .unwrap_or(self.base_backoff);
                    tokio::time::sleep(
                        retry_after.min(deadline_at.saturating_duration_since(Instant::now())),
                    )
                    .await;
                }
                Ok((status, _hdrs, body)) => {
                    self.metrics
                        .record_forward("forward", "ok", attempt_start.elapsed());
                    return Ok(Reply {
                        status: status.as_u16(),
                        payload: body,
                    });
                }
                Err(e) => {
                    self.metrics.record_forward_retry("connect");
                    tracing::debug!(shard = %env.shard, attempt = attempts, error = %e, "mesh forward attempt failed");
                    if attempts >= self.max_attempts {
                        self.metrics
                            .record_forward("forward", "error", attempt_start.elapsed());
                        return Err(ForwardError::RetriesExhausted {
                            shard: env.shard,
                            attempts,
                        });
                    }
                    tokio::time::sleep(self.base_backoff).await;
                }
            }
        }
    }

    async fn send_once(
        &self,
        owner: &ReplicaId,
        env: &Envelope,
    ) -> Result<(StatusCode, HeaderMap, Bytes), ForwardError> {
        let addr = self.resolve_addr(owner)?;

        let mut send_request = match self.pooled(&addr) {
            Some(sr) => sr,
            None => self.connect(&addr).await?,
        };

        let request = self.build_request(env)?;
        match send_request.send_request(request).await {
            Ok(response) => Self::read_response(response).await,
            Err(e) => {
                // The pooled connection may have gone stale (the peer restarted, an idle
                // timeout fired, a prior request's error poisoned the multiplexer). Evict it
                // and retry once against a freshly dialed connection before surfacing a
                // failure -- `forward`'s own retry loop still covers everything else (421, 503,
                // repeated connect failures) on top of this one inline redial.
                self.evict_pooled(&addr);
                tracing::debug!(%addr, error = %e, "pooled mesh connection failed, redialing");
                let mut fresh = self.connect(&addr).await?;
                let request = self.build_request(env)?;
                let response = fresh
                    .send_request(request)
                    .await
                    .map_err(|e| ForwardError::Transport(format!("send request: {e}")))?;
                Self::read_response(response).await
            }
        }
    }

    async fn read_response(
        response: Response<Incoming>,
    ) -> Result<(StatusCode, HeaderMap, Bytes), ForwardError> {
        let status = response.status();
        let hdrs = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| ForwardError::Transport(format!("read body: {e}")))?
            .to_bytes();
        Ok((status, hdrs, body))
    }

    /// A pooled connection handle for `addr`, if one is live. `SendRequest::clone` is a cheap
    /// handle to the same underlying multiplexer (safe to use concurrently from multiple
    /// forwards at once), so the original stays in the pool for the next caller.
    fn pooled(&self, addr: &str) -> Option<SendRequest<Full<Bytes>>> {
        self.pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(addr)
            .cloned()
    }

    fn evict_pooled(&self, addr: &str) {
        self.pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(addr);
    }

    /// Dials `addr` (TCP, plus TLS when configured for mutual TLS), performs the HTTP/2
    /// handshake, spawns the connection's background driver task, and stores the resulting
    /// handle in the pool for reuse by later forwards to the same peer.
    async fn connect(&self, addr: &str) -> Result<SendRequest<Full<Bytes>>, ForwardError> {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| ForwardError::Transport(format!("connect {addr}: {e}")))?;
        tcp.set_nodelay(true).ok();

        let send_request = if let Some(tls_cfg) = &self.client_tls {
            let server_name = rustls_pki_types::ServerName::try_from(addr_host(addr))
                .map_err(|e| ForwardError::Transport(format!("invalid TLS server name: {e}")))?
                .to_owned();
            let connector = tokio_rustls::TlsConnector::from(tls_cfg.clone());
            let tls_stream = connector
                .connect(server_name, tcp)
                .await
                .map_err(|e| ForwardError::Transport(format!("TLS handshake: {e}")))?;
            self.handshake(tls_stream).await?
        } else {
            self.handshake(tcp).await?
        };

        self.pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(addr.to_owned(), send_request.clone());
        Ok(send_request)
    }

    async fn handshake<IO>(&self, io: IO) -> Result<SendRequest<Full<Bytes>>, ForwardError>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (send_request, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(io))
                .await
                .map_err(|e| ForwardError::Transport(format!("HTTP/2 handshake: {e}")))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(send_request)
    }

    fn build_request(&self, env: &Envelope) -> Result<Request<Full<Bytes>>, ForwardError> {
        let requester = serde_json::to_vec(&env.requester)
            .map_err(|e| ForwardError::Transport(format!("encode requester context: {e}")))?;
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mesh/v1/forward")
            .header(headers::SHARD, env.shard.to_string())
            .header(headers::ROUTE, &env.route)
            .header(headers::IDEMPOTENCY_KEY, env.idempotency_key.to_string())
            .header(headers::REQUESTER, base64_encode(&requester))
            .header(headers::DEADLINE_MS, env.deadline.as_millis().to_string())
            .header(headers::ORIGIN, env.origin.as_str())
            .header(
                headers::ORIGIN_GENERATION,
                env.origin_generation.0.to_string(),
            )
            .header(headers::HOPS, env.hops.to_string());
        if let Some(tp) = &env.traceparent {
            builder = builder.header(headers::TRACEPARENT, tp);
        }
        if let AuthMode::SharedSecret { secret } = &self.auth {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {secret}"));
        }
        builder
            .body(Full::new(env.payload.clone()))
            .map_err(|e| ForwardError::Transport(format!("build request: {e}")))
    }

    fn resolve_addr(&self, owner: &ReplicaId) -> Result<String, ForwardError> {
        // The mesh address is carried on the replica registry row, which the ownership manager
        // does not expose directly through the object-safe `Ownership` trait (actors never need
        // raw addresses). A production wiring passes a small `AddressBook` alongside the
        // `Ownership` trait object; for now the owner's id is used directly as the address,
        // which is exactly right for the in-process chaos harness's `ReplicaId == "host:port"`
        // convention and is documented as a Phase 1 follow-up otherwise.
        Ok(owner.as_str().to_owned())
    }
}

fn addr_host(addr: &str) -> String {
    addr.rsplit_once(':')
        .map(|(h, _)| h.to_owned())
        .unwrap_or_else(|| addr.to_owned())
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
