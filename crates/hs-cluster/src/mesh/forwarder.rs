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

/// The longest wait between two forward attempts. A shard handoff completes within a few
/// heartbeats; polling the new owner four times a second keeps the added latency after it
/// settles small without flooding a peer that is still starting.
pub const MAX_BACKOFF: Duration = Duration::from_millis(250);

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
    /// ownership refresh) and `503` (the peer's `Retry-After`, else backoff), up to
    /// `max_attempts` within `env.deadline`. Application errors (any other status) are returned
    /// as-is, never retried.
    ///
    /// The backoff doubles from `base_backoff` up to [`MAX_BACKOFF`], so the default settings
    /// (40 attempts from 10 ms, 10 s deadline) keep retrying for about nine seconds: long
    /// enough to ride out a shard handoff, which is what a `421` or a refused connection
    /// almost always is. Measured on two pods on a real cluster (2026-09-28): a graceful
    /// drain left a 0.4 s window and a replica rejoining a 1.6 s one in which the believed
    /// owner answered `421`; four attempts 10 ms apart turned both into client-visible `503`s.
    ///
    /// Counted under `kind="client"` in `hs_cluster_forward_latency_seconds`; a caller
    /// forwarding something else says what with [`Forwarder::forward_as`].
    ///
    /// # Errors
    /// Returns [`ForwardError`] if no owner is known, the hop limit or deadline is exceeded, or
    /// every retry attempt fails.
    pub async fn forward(&self, env: Envelope) -> Result<Reply, ForwardError> {
        self.forward_as("client", env).await
    }

    /// [`Forwarder::forward`], counted under `kind` in `hs_cluster_forward_latency_seconds`
    /// (`federation` for a request from another server, `federation_pdu` for one PDU of a
    /// `/send`): what the caller forwarded, which the mesh itself never interprets.
    ///
    /// # Errors
    /// As [`Forwarder::forward`].
    pub async fn forward_as(
        &self,
        kind: &'static str,
        mut env: Envelope,
    ) -> Result<Reply, ForwardError> {
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
        // Lookups that found no owner at all: the shard was released and nobody has acquired it
        // yet, which during a rolling update lasts about a second (measured 2026-09-28). Waited
        // out on the same backoff and deadline as a `421`.
        let mut ownerless = 0u32;
        loop {
            if Instant::now() >= deadline_at {
                return Err(ForwardError::DeadlineExceeded(env.shard));
            }
            let Some(owner) = self.ownership.owner_of(env.shard) else {
                ownerless += 1;
                let wait = self.backoff(ownerless);
                if self.out_of_retries(ownerless, wait, deadline_at) {
                    return Err(ForwardError::NoOwner(env.shard));
                }
                self.metrics.record_forward_retry("no_owner");
                tracing::debug!(shard = %env.shard, lookup = ownerless, "no owner known for the shard yet, waiting");
                tokio::time::sleep(wait).await;
                continue;
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
                    let wait = self.backoff(attempts);
                    if self.out_of_retries(attempts, wait, deadline_at) {
                        self.metrics.record_forward(
                            "forward",
                            kind,
                            "misdirected",
                            attempt_start.elapsed(),
                        );
                        return Ok(Reply {
                            status: status.as_u16(),
                            payload: body,
                        });
                    }
                    tracing::debug!(shard = %env.shard, %owner, attempt = attempts, "mesh forward misdirected (421), waiting for ownership to settle");
                    tokio::time::sleep(wait).await;
                }
                Ok((status, hdrs, body)) if status == 503 => {
                    self.metrics.record_forward_retry("503");
                    let wait = hdrs
                        .get(headers::RETRY_AFTER_MS)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_millis)
                        .unwrap_or_else(|| self.backoff(attempts));
                    if self.out_of_retries(attempts, wait, deadline_at) {
                        self.metrics.record_forward(
                            "forward",
                            kind,
                            "unavailable",
                            attempt_start.elapsed(),
                        );
                        return Ok(Reply {
                            status: status.as_u16(),
                            payload: body,
                        });
                    }
                    tracing::debug!(shard = %env.shard, %owner, attempt = attempts, "mesh forward unavailable (503), retrying");
                    tokio::time::sleep(wait).await;
                }
                Ok((status, _hdrs, body)) => {
                    self.metrics
                        .record_forward("forward", kind, "ok", attempt_start.elapsed());
                    return Ok(Reply {
                        status: status.as_u16(),
                        payload: body,
                    });
                }
                Err(e) => {
                    self.metrics.record_forward_retry("connect");
                    tracing::debug!(shard = %env.shard, attempt = attempts, error = %e, "mesh forward attempt failed");
                    let wait = self.backoff(attempts);
                    if self.out_of_retries(attempts, wait, deadline_at) {
                        self.metrics.record_forward(
                            "forward",
                            kind,
                            "error",
                            attempt_start.elapsed(),
                        );
                        return Err(ForwardError::RetriesExhausted {
                            shard: env.shard,
                            attempts,
                        });
                    }
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// Whether to stop after `attempts` attempts rather than wait `wait` and try again: the
    /// attempt budget is spent, or the next attempt could not start before the deadline. The
    /// caller then passes back the last refusal as it is, which says more than a bare
    /// "deadline exceeded" would.
    fn out_of_retries(&self, attempts: u32, wait: Duration, deadline_at: Instant) -> bool {
        attempts >= self.max_attempts || Instant::now() + wait >= deadline_at
    }

    /// The wait before retry number `attempt + 1`: `base_backoff` doubled per attempt already
    /// made, capped at [`MAX_BACKOFF`].
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.base_backoff
            .checked_mul(factor)
            .unwrap_or(MAX_BACKOFF)
            .min(MAX_BACKOFF)
    }

    /// Sends one replica-to-replica message (`POST /mesh/v1/peer`, see
    /// [`crate::mesh::PeerHandler`]) to `peer` and returns its reply. Unlike
    /// [`Forwarder::forward`] there is no owner lookup (the message is for this exact replica),
    /// no `421`/`503` retry loop (nothing to redirect to) and no idempotency key: one attempt
    /// on the pooled connection, one inline redial if that connection turned out dead, all
    /// within `deadline`. The reply's status is the handler's own; a `501` means the peer runs
    /// without a peer handler and is reported as unreachable, so a caller fanning out to every
    /// replica treats a peer from before this route existed like one that is down.
    ///
    /// # Errors
    /// Returns [`ForwardError::PeerUnreachable`] if the peer could not be dialed, did not answer
    /// within `deadline`, or has no peer handler; [`ForwardError::Transport`] for a malformed
    /// request.
    pub async fn send_to_peer(
        &self,
        peer: &ReplicaId,
        route: &str,
        payload: Bytes,
        deadline: Duration,
    ) -> Result<Reply, ForwardError> {
        let start = Instant::now();
        let unreachable = |reason: String| ForwardError::PeerUnreachable {
            peer: peer.clone(),
            reason,
        };
        let sent = tokio::time::timeout(deadline, self.send_peer_once(peer, route, &payload)).await;
        let result = match sent {
            Ok(Ok((status, _hdrs, _body))) if status == StatusCode::NOT_IMPLEMENTED => Err(
                unreachable("the peer has no peer-message handler".to_owned()),
            ),
            Ok(Ok((status, _hdrs, body))) => Ok(Reply {
                status: status.as_u16(),
                payload: body,
            }),
            Ok(Err(e)) => Err(unreachable(e.to_string())),
            Err(_elapsed) => Err(unreachable(format!(
                "no answer within {}ms",
                deadline.as_millis()
            ))),
        };
        self.metrics.record_forward(
            "peer",
            "peer",
            if result.is_ok() { "ok" } else { "error" },
            start.elapsed(),
        );
        result
    }

    async fn send_peer_once(
        &self,
        peer: &ReplicaId,
        route: &str,
        payload: &Bytes,
    ) -> Result<(StatusCode, HeaderMap, Bytes), ForwardError> {
        let addr = self.resolve_addr(peer)?;
        let mut send_request = match self.pooled(&addr) {
            Some(sr) => sr,
            None => self.connect(&addr).await?,
        };
        let request = self.build_peer_request(route, payload)?;
        match send_request.send_request(request).await {
            Ok(response) => Self::read_response(response).await,
            Err(e) => {
                // The same single inline redial `send_once` makes, for the same reason.
                self.evict_pooled(&addr);
                tracing::debug!(%addr, error = %e, "pooled mesh connection failed, redialing");
                let mut fresh = self.connect(&addr).await?;
                let request = self.build_peer_request(route, payload)?;
                let response = fresh
                    .send_request(request)
                    .await
                    .map_err(|e| ForwardError::Transport(format!("send request: {e}")))?;
                Self::read_response(response).await
            }
        }
    }

    fn build_peer_request(
        &self,
        route: &str,
        payload: &Bytes,
    ) -> Result<Request<Full<Bytes>>, ForwardError> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mesh/v1/peer")
            .header(headers::ROUTE, route)
            .header(headers::ORIGIN, self.ownership.me().as_str());
        if let AuthMode::SharedSecret { secret } = &self.auth {
            builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {secret}"));
        }
        builder
            .body(Full::new(payload.clone()))
            .map_err(|e| ForwardError::Transport(format!("build request: {e}")))
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
