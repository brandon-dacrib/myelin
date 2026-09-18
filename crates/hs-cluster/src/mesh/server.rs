//! The mesh HTTP/2 server: accepts `POST /mesh/v1/forward` and `POST /mesh/v1/released`,
//! authenticates the peer, checks ownership and fencing, and dispatches to a [`ShardHandler`].
//! `docs/rfcs/0001-cluster-ownership.md` sections 8, 9 and 11.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};

use crate::mesh::auth::{Authenticator, TlsPeerInfo};
use crate::mesh::envelope::{
    Envelope, IdempotencyKey, Reply, RequesterContext, ShardHandler, headers,
};
use crate::mesh::idempotency::IdempotencyCache;
use crate::mesh::tls;
use crate::ownership::Ownership;
use crate::types::{Generation, ReplicaId, ShardId};

/// Everything a mesh connection needs to answer a forwarded request. Not generic over the
/// `hs-kv` backend: the ownership and handler traits are already object-safe, so one `MeshServer`
/// serves whichever backend the process was started with.
pub struct MeshDeps {
    /// Authenticates each connection or request.
    pub authenticator: Arc<dyn Authenticator>,
    /// Answers `is_mine` / `owner_of` / `fence`.
    pub ownership: Arc<dyn Ownership>,
    /// Dispatches an authorized, fenced request to the actor that owns the route.
    pub handler: Arc<dyn ShardHandler>,
    /// The owner-side reply cache (RFC 0001 section 8).
    pub idempotency: Arc<IdempotencyCache>,
    /// Bounded in-flight forwards, shared across every connection this server accepts (RFC 0001
    /// section 9: "a bounded number of in-flight forwards ... enforced by a semaphore").
    pub in_flight: Arc<Semaphore>,
    /// Notified when a `/mesh/v1/released` nudge arrives, so the local convergence loop
    /// re-evaluates immediately instead of waiting for its next tick.
    pub nudge: Option<Arc<tokio::sync::Notify>>,
}

/// The mesh's HTTP/2 listener.
pub struct MeshServer {
    listen_addr: String,
    tls_server_config: Option<Arc<rustls::ServerConfig>>,
}

impl MeshServer {
    /// Builds a server that binds `listen_addr`. `tls` is required (and used to build the
    /// server's `rustls::ServerConfig`, requiring client certificates) only when serving in
    /// mutual-TLS mode; a plaintext server still requires callers to configure a
    /// [`crate::mesh::auth::SharedSecretAuthenticator`] in [`MeshDeps`], since the mesh must
    /// never be reachable unauthenticated.
    ///
    /// # Errors
    /// Returns a TLS error if the server config could not be built.
    pub fn new(
        listen_addr: impl Into<String>,
        tls_material: Option<&tls::TlsMaterial>,
    ) -> Result<Self, tls::TlsError> {
        let tls_server_config = tls_material
            .map(tls::TlsMaterial::server_config)
            .transpose()?;
        Ok(Self {
            listen_addr: listen_addr.into(),
            tls_server_config,
        })
    }

    /// Serves until `shutdown` carries `true`. Each accepted connection is handled on its own
    /// task; connection errors (a peer resetting, a failed TLS handshake) are logged and do not
    /// stop the listener.
    ///
    /// # Errors
    /// Returns an I/O error only if the listener itself could not be bound.
    pub async fn serve(
        self,
        deps: Arc<MeshDeps>,
        mut shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _peer_addr) = accepted?;
                    stream.set_nodelay(true).ok();
                    let deps = deps.clone();
                    match self.tls_server_config.clone() {
                        Some(cfg) => {
                            let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
                            tokio::spawn(async move {
                                if let Ok(tls_stream) = acceptor.accept(stream).await {
                                    let tls_info = peer_info(&tls_stream);
                                    serve_connection(tls_stream, tls_info, deps).await;
                                }
                            });
                        }
                        None => {
                            tokio::spawn(serve_connection(stream, None, deps));
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn peer_info<IO>(stream: &tokio_rustls::server::TlsStream<IO>) -> Option<TlsPeerInfo> {
    let certs = stream.get_ref().1.peer_certificates()?;
    let leaf = certs.first()?;
    Some(TlsPeerInfo {
        fingerprint_sha256_hex: tls::fingerprint_sha256_hex(leaf),
        sans: tls::extract_dns_sans(leaf),
    })
}

async fn serve_connection<IO>(io: IO, tls_info: Option<TlsPeerInfo>, deps: Arc<MeshDeps>)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let tls_info = Arc::new(tls_info);
    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
        let deps = deps.clone();
        let tls_info = tls_info.clone();
        async move { Ok::<_, Infallible>(handle_request(req, tls_info, deps).await) }
    });
    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(io), service)
        .await;
}

fn respond(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

async fn handle_request(
    req: Request<Incoming>,
    tls_info: Arc<Option<TlsPeerInfo>>,
    deps: Arc<MeshDeps>,
) -> Response<Full<Bytes>> {
    if let Err(e) = deps
        .authenticator
        .authenticate(req.headers(), tls_info.as_ref().as_ref())
    {
        return respond(StatusCode::UNAUTHORIZED, format!("mesh auth failed: {e}"));
    }

    match req.uri().path() {
        "/mesh/v1/forward" => handle_forward(req, &deps).await,
        "/mesh/v1/released" => {
            if let Some(n) = &deps.nudge {
                n.notify_one();
            }
            respond(StatusCode::OK, Bytes::new())
        }
        _ => respond(StatusCode::NOT_FOUND, Bytes::new()),
    }
}

fn parse_envelope(h: &HeaderMap, payload: Bytes) -> Result<Envelope, String> {
    let get = |name: &str| -> Result<&str, String> {
        h.get(name)
            .ok_or_else(|| format!("missing header {name}"))?
            .to_str()
            .map_err(|e| format!("header {name} is not valid text: {e}"))
    };
    let shard =
        ShardId::parse(get(headers::SHARD)?).ok_or_else(|| "invalid x-hs-shard".to_string())?;
    let route = get(headers::ROUTE)?.to_string();
    let idempotency_key: IdempotencyKey = get(headers::IDEMPOTENCY_KEY)?
        .parse()
        .map_err(|_| "invalid x-hs-idempotency-key".to_string())?;
    let requester_b64 = get(headers::REQUESTER)?;
    let requester_bytes = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(requester_b64)
            .map_err(|e| format!("invalid x-hs-requester base64: {e}"))?
    };
    let requester: RequesterContext = serde_json::from_slice(&requester_bytes)
        .map_err(|e| format!("invalid x-hs-requester JSON: {e}"))?;
    let deadline_ms: u64 = get(headers::DEADLINE_MS)?
        .parse()
        .map_err(|_| "invalid x-hs-deadline-ms".to_string())?;
    let origin = ReplicaId::new(get(headers::ORIGIN)?);
    let origin_generation = Generation(
        get(headers::ORIGIN_GENERATION)?
            .parse()
            .map_err(|_| "invalid x-hs-origin-generation".to_string())?,
    );
    let hops: u32 = get(headers::HOPS)?
        .parse()
        .map_err(|_| "invalid x-hs-hops".to_string())?;
    let traceparent = h
        .get(headers::TRACEPARENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    Ok(Envelope {
        shard,
        route,
        idempotency_key,
        requester,
        deadline: Duration::from_millis(deadline_ms),
        origin,
        origin_generation,
        hops,
        traceparent,
        payload,
    })
}

async fn handle_forward(req: Request<Incoming>, deps: &Arc<MeshDeps>) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let payload = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return respond(StatusCode::BAD_REQUEST, format!("reading body: {e}")),
    };
    let env = match parse_envelope(&parts.headers, payload) {
        Ok(env) => env,
        Err(msg) => return respond(StatusCode::BAD_REQUEST, msg),
    };

    if env.deadline.is_zero() {
        return respond(StatusCode::GATEWAY_TIMEOUT, Bytes::new());
    }

    if !deps.ownership.is_mine(env.shard) {
        return misdirected(deps, env.shard);
    }

    if let Some(cached) = deps.idempotency.get(env.shard, env.idempotency_key) {
        return reply_response(cached);
    }

    let Some(fence) = deps.ownership.fence(env.shard) else {
        // Lost ownership between the `is_mine` check above and here (a race with the
        // convergence loop); tell the caller to retry against the (now different) owner.
        return misdirected(deps, env.shard);
    };

    let Ok(_permit) = deps.in_flight.clone().try_acquire_owned() else {
        return respond(StatusCode::SERVICE_UNAVAILABLE, Bytes::new());
    };

    let shard = env.shard;
    let key = env.idempotency_key;
    let reply = deps.handler.handle(env, fence).await;
    deps.idempotency.put(shard, key, reply.clone());
    reply_response(reply)
}

fn misdirected(deps: &Arc<MeshDeps>, shard: ShardId) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(StatusCode::MISDIRECTED_REQUEST);
    if let Some(owner) = deps.ownership.owner_of(shard) {
        builder = builder.header(headers::OWNER_HINT, owner.as_str());
    }
    builder
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

fn reply_response(reply: Reply) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    respond(status, reply.payload)
}
