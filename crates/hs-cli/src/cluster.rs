//! Cluster wiring for `hs serve`.
//!
//! Two replicas that both point at the same PostgreSQL database and both run `hs-room`'s ordinary
//! per-process `RoomActor` registry will silently fork a room's event DAG the moment they both
//! accept writes for it concurrently: each actor computes `prev_events` from its own in-memory
//! extremities, and nothing before this module ever consulted `hs-cluster`'s ownership API to
//! stop that (see `docs/status/03-cluster.md`'s two-replica experiment for the full reproduction).
//! `hs-cluster` itself (`Cluster`, `Ownership`, `Fence`, the mesh) was fully built and tested in
//! isolation, but nothing called it — `hs-cluster` was not even a dependency of `hs-cli`.
//!
//! This module closes that gap without touching `hs-room` (owned by track 04, out of scope for
//! this crate): [`start`] builds the ownership manager (inert in single-node mode, a real
//! `hs-kv`-backed `KvOwnership` otherwise) and, when clustered, a [`hs_cluster::mesh::Forwarder`];
//! [`RoomShardGate`] is an `axum` middleware layered over the *entire* built router in
//! `crate::serve` that inspects every request's path for a `/rooms/{roomId}/...` segment and, if
//! this replica does not own that room's shard, either forwards the request verbatim to the owner
//! over the mesh or refuses it with a clear error — it never lets the request reach `hs-room`'s
//! registry, so a non-owner replica never constructs a local `RoomActor` for a room it does not
//! own, which is the actual fix (fencing alone would not have been: see the module's docs and
//! `docs/status/03-cluster.md`'s root-cause writeup for why two simultaneously-live owners never
//! trip a fence at all).
//!
//! The forward path is a raw HTTP reverse proxy over the mesh, not a structured RPC: the gate
//! serializes the incoming method, path, headers (including `Authorization`) and body into a
//! [`ProxiedRequest`] and hands it to [`hs_cluster::mesh::Forwarder::forward`]; the owner's
//! [`ProxyShardHandler`] deserializes it, replays it against its own copy of the exact same
//! `axum::Router` `hs-cli` built for its own listeners (`tower::Service::oneshot`), and ships the
//! response back the same way. This means a forwarded request is authenticated by the *owner's*
//! own auth middleware exactly as if it had arrived directly — the mesh transport's own
//! authentication (a shared secret today; `hs-cluster` also supports mutual TLS, not wired here,
//! see "Decisions made" in the status file) only proves the request came from a trusted peer, not
//! who the end user is.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tower::ServiceExt;

use hs_cluster::mesh::{
    Authenticator, Envelope, Forwarder, IdempotencyCache, IdempotencyKey, MeshDeps, MeshServer,
    Reply, ShardHandler, SharedSecretAuthenticator,
};
use hs_cluster::{Cluster, Fence, Generation, Ownership, ReplicaId, ShardId, ShardLayout};
use hs_kv::KvBackend;

/// The largest body this process will buffer while proxying a forwarded room request. Room
/// events are small (Matrix caps `m.room.message` bodies well under this); a generous cap avoids
/// rejecting a legitimate large state event while still bounding memory under a hostile or
/// misbehaving peer.
const MAX_PROXIED_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Errors starting the cluster.
#[derive(Debug, thiserror::Error)]
pub enum ClusterSetupError {
    /// The `hs-cluster` ownership manager could not start (the store was unreachable, or the
    /// shard layout conflicts with what is already recorded).
    #[error(transparent)]
    Cluster(#[from] hs_cluster::error::ClusterError),
    /// The mesh forwarder's TLS material could not be built. Unreachable in the shared-secret
    /// mode this module wires today, kept for when mutual TLS is added.
    #[error(transparent)]
    Tls(#[from] hs_cluster::mesh::tls::TlsError),
    /// `cluster.*` in the native config does not satisfy `hs-cluster`'s own invariants (for
    /// example `lease_ttl` too close to `heartbeat_interval`), or asks for something this module
    /// does not implement yet (mutual TLS).
    #[error("cluster config is invalid: {0}")]
    Invalid(String),
}

/// Everything a running `hs serve` process needs from `hs-cluster`: the ownership handle every
/// request the [`RoomShardGate`] gates consults, and (once clustered) the forwarder and the mesh
/// listener's own startup parameters.
pub struct ClusterHandles {
    /// The ownership/lifecycle facade. `hs_cluster::Cluster::single_node` in single-node mode
    /// (the default; behaves exactly as before this module existed — `is_mine` is always `true`,
    /// so [`RoomShardGate`] never forwards or refuses anything), a real clustered manager
    /// otherwise.
    pub cluster: Cluster,
    /// The shard layout this process computes `/rooms/{roomId}` shards against. Meaningless in
    /// single-node mode (every shard is "mine" regardless), kept anyway so the same gate code
    /// runs unconditionally.
    pub layout: ShardLayout,
    /// The mesh forwarder, once this replica is clustered and the mesh could be built. `None` in
    /// single-node mode, where [`RoomShardGate`] must never need it (`is_mine` never returns
    /// `false`).
    pub forwarder: Option<Arc<Forwarder>>,
    origin: ReplicaId,
    origin_generation: Generation,
    default_deadline: Duration,
    mesh: Option<MeshStartConfig>,
}

struct MeshStartConfig {
    listen_addr: String,
    secret: String,
    idempotency_ttl: Duration,
    max_in_flight_per_peer: usize,
    ownership: Arc<dyn Ownership>,
}

/// A running mesh listener, returned by [`ClusterHandles::spawn_mesh`]. Dropping this without
/// calling [`MeshRuntime::shutdown`] leaks the listener task (it keeps running); `crate::serve`
/// always shuts it down as part of [`crate::serve::ServeHandle::shutdown`].
pub struct MeshRuntime {
    shutdown_tx: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl MeshRuntime {
    /// Stops accepting new mesh connections and waits for the listener task to exit.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join.await;
    }
}

/// Builds this process's [`ClusterHandles`] from the native config: [`Cluster::single_node`] when
/// `config.cluster.single_node` (the default, matching today's behavior exactly — this is the
/// change the brief calls out as "safe to land first with no functional change"), otherwise a real
/// [`hs_cluster::Cluster::start`] over the same already-open storage `backend` every other
/// subsystem uses, plus a [`Forwarder`] for forwarding to whichever peer owns a shard this replica
/// does not.
///
/// # Errors
/// See [`ClusterSetupError`].
pub async fn start<B: KvBackend + 'static>(
    config: &hs_config::Config,
    backend: B,
) -> Result<ClusterHandles, ClusterSetupError> {
    let layout_from_config = ShardLayout {
        rooms: config.cluster.room_shards,
        users: config.cluster.user_shards,
        // hs-config's ClusterConfig has no separate federation/appservice shard-count fields yet
        // (see docs/status/03-cluster.md, "Interfaces needed" / this update's decisions); reuse
        // hs-cluster's own crate-wide defaults until track 13 adds them.
        federation: ShardLayout::default().federation,
        appservice: ShardLayout::default().appservice,
    };

    if config.cluster.single_node {
        return Ok(ClusterHandles::single_node(layout_from_config));
    }

    let cluster_config = to_hs_cluster_config(config, layout_from_config);
    cluster_config
        .validate()
        .map_err(ClusterSetupError::Invalid)?;
    let secret = match &cluster_config.mesh.auth {
        hs_cluster::mesh::AuthMode::SharedSecret { secret } => secret.clone(),
        hs_cluster::mesh::AuthMode::MutualTls { .. } => {
            return Err(ClusterSetupError::Invalid(
                "cluster.mesh.tls is set, but hs-cli only wires the shared-secret mesh auth mode \
                 today (see docs/status/03-cluster.md, \"Decisions made\"); unset it or run \
                 without TLS material for now"
                    .to_owned(),
            ));
        }
    };
    let layout = cluster_config.layout;
    let me = cluster_config.me.clone();
    let listen_addr = cluster_config.mesh.listen_addr.clone();
    let idempotency_ttl = cluster_config.mesh.idempotency_ttl;
    let max_in_flight_per_peer = cluster_config.mesh.max_in_flight_per_peer;
    let max_hops = cluster_config.mesh.max_hops;
    let max_attempts = cluster_config.mesh.max_attempts;
    let retry_base_backoff = cluster_config.mesh.retry_base_backoff;
    let default_deadline = cluster_config.mesh.default_deadline;

    let (cluster, manager) = Cluster::start(cluster_config, backend).await?;
    let ownership = cluster.ownership().clone();
    let metrics = manager.metrics();

    let forwarder = Arc::new(Forwarder::new(
        hs_cluster::mesh::AuthMode::SharedSecret {
            secret: secret.clone(),
        },
        None,
        max_hops,
        max_attempts,
        retry_base_backoff,
        ownership.clone(),
        metrics,
    )?);

    Ok(ClusterHandles {
        cluster,
        layout,
        forwarder: Some(forwarder),
        origin: me,
        origin_generation: Generation::fresh(None),
        default_deadline,
        mesh: Some(MeshStartConfig {
            listen_addr,
            secret,
            idempotency_ttl,
            max_in_flight_per_peer,
            ownership,
        }),
    })
}

impl ClusterHandles {
    /// The inert single-node handles: `is_mine` is always `true`, so [`RoomShardGate`] never
    /// forwards or refuses anything and no mesh listener is started. Used both by [`start`] and by
    /// [`crate::serve::route_manifest`], which needs a [`ClusterHandles`] to build the gate layer
    /// without a real config or backend.
    #[must_use]
    pub fn single_node(layout: ShardLayout) -> Self {
        let me = ReplicaId::new(single_node_replica_id());
        Self {
            cluster: Cluster::single_node(me.clone()),
            layout,
            forwarder: None,
            origin: me,
            origin_generation: Generation::fresh(None),
            default_deadline: Duration::from_secs(10),
            mesh: None,
        }
    }

    /// Starts the mesh HTTP/2 listener, if this replica is clustered (`None` in single-node
    /// mode, where nothing should ever dial in). `app` must be the exact same router serving this
    /// process's own client listeners: a forwarded request re-enters it in-process on the owner,
    /// so the owner's own auth middleware, room actor and persistence run exactly as they would
    /// for a request that arrived directly (see this module's docs).
    #[must_use]
    pub fn spawn_mesh(&self, app: axum::Router) -> Option<MeshRuntime> {
        let mesh = self.mesh.as_ref()?;
        let authenticator: Arc<dyn Authenticator> =
            Arc::new(SharedSecretAuthenticator::new(mesh.secret.clone()));
        let handler: Arc<dyn ShardHandler> = Arc::new(ProxyShardHandler { app });
        let deps = Arc::new(MeshDeps {
            authenticator,
            ownership: mesh.ownership.clone(),
            handler,
            idempotency: Arc::new(IdempotencyCache::new(mesh.idempotency_ttl, 4096)),
            in_flight: Arc::new(tokio::sync::Semaphore::new(mesh.max_in_flight_per_peer)),
            // No nudge wiring yet: `/mesh/v1/released` just answers `200` without waking the
            // local convergence loop early, so a peer's release is only noticed on this
            // replica's next heartbeat tick rather than immediately. Correctness is unaffected
            // (the same loop runs unconditionally on its own interval); only failover latency
            // is, and only by up to one `heartbeat_interval`. Documented in the status file.
            nudge: None,
        });
        let server = match MeshServer::new(mesh.listen_addr.clone(), None) {
            Ok(server) => server,
            Err(error) => {
                tracing::error!(
                    %error,
                    listen_addr = %mesh.listen_addr,
                    "failed to build the mesh listener; this replica will not accept forwards \
                     from peers"
                );
                return None;
            }
        };
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let listen_addr = mesh.listen_addr.clone();
        let join = tokio::spawn(async move {
            if let Err(error) = server.serve(deps, shutdown_rx).await {
                tracing::error!(%listen_addr, %error, "mesh listener exited with an error");
            }
        });
        Some(MeshRuntime { shutdown_tx, join })
    }
}

/// This replica's identity in single-node mode: a host/pid pair, since single-node mode has no
/// registry row and nothing else ever reads it back (`Cluster::single_node`'s `owner_of` always
/// answers "me" regardless of its value) — it exists purely so log lines and the mesh-disabled
/// gate have something stable to print.
fn single_node_replica_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_owned());
    format!("{host}:{}", std::process::id())
}

/// The first configured listener's bind address, used as the mesh's advertised host — `None`,
/// `"0.0.0.0"` and `"::"` (a listener bound to every interface, which is not a dialable address
/// for a peer) fall back to `"127.0.0.1"`, correct for this session's same-host two-replica setup
/// and for any other deployment that terminates the mesh behind a loopback-reachable sidecar.
/// Getting the real advertised address right for a Kubernetes pod (the pod IP, not a config
/// value) is out of scope for this pass; see `docs/status/03-cluster.md`.
fn advertise_host(config: &hs_config::Config) -> String {
    config
        .listeners
        .listeners
        .first()
        .and_then(|listener| listener.bind_addresses.first())
        .map(String::as_str)
        .filter(|addr| !addr.is_empty() && *addr != "0.0.0.0" && *addr != "::")
        .unwrap_or("127.0.0.1")
        .to_owned()
}

/// Converts `hs-config`'s `cluster` section into `hs-cluster`'s own config type. The two shapes do
/// not line up one-to-one (see this crate's module docs and `docs/status/03-cluster.md`): this
/// replica's identity and mesh-advertised address are process-level facts with no config field
/// (derived from the first listener's bind address plus `cluster.mesh.port` — see
/// [`advertise_host`]), and federation/appservice shard counts have no `hs-config` field yet
/// (`layout` is filled in by the caller from `hs-cluster`'s own defaults for those two kinds).
fn to_hs_cluster_config(
    config: &hs_config::Config,
    layout: ShardLayout,
) -> hs_cluster::ClusterConfig {
    let cluster_cfg = &config.cluster;
    let host = advertise_host(config);
    let mesh_advertise_addr = format!("{host}:{}", cluster_cfg.mesh.port);
    // The forwarder dials `owner.as_str()` directly as a `host:port` (see
    // `hs_cluster::mesh::forwarder::Forwarder::resolve_addr`'s doc comment on this exact
    // convention), so this replica's identity *is* its dialable mesh address rather than a
    // separate name that would need a lookup table.
    let me = ReplicaId::new(mesh_advertise_addr.clone());

    let mut cfg = hs_cluster::ClusterConfig::new(me, mesh_advertise_addr, layout);
    cfg.heartbeat_interval = cluster_cfg.heartbeat_interval.as_std();
    cfg.lease_ttl = cluster_cfg.lease_ttl.as_std();
    cfg.mesh.listen_addr = format!("0.0.0.0:{}", cluster_cfg.mesh.port);
    let secret = cluster_cfg
        .mesh
        .shared_secret
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("dev-only-shared-secret")
        .to_owned();
    if cluster_cfg.mesh.tls.is_some() {
        // Recorded via the `AuthMode` value itself rather than a separate flag: `start` above
        // checks for this variant and refuses to boot rather than silently downgrading a
        // TLS-configured deployment to shared-secret auth.
        cfg.mesh.auth = hs_cluster::mesh::AuthMode::MutualTls {
            ca_file: std::path::PathBuf::new(),
            cert_file: std::path::PathBuf::new(),
            key_file: std::path::PathBuf::new(),
            peer_san_suffix: None,
        };
    } else {
        cfg.mesh.auth = hs_cluster::mesh::AuthMode::SharedSecret { secret };
    }
    cfg
}

/// A serialized HTTP request, carried as a [`Envelope::payload`] from [`RoomShardGate`] to
/// [`ProxyShardHandler`]. `headers` excludes hop-by-hop headers (see [`is_hop_by_hop`]); `body_b64`
/// is base64 rather than a raw byte field so the envelope stays valid UTF-8 JSON regardless of
/// content (a media upload would not hit this path — nothing under `/media` is room-scoped — but a
/// state event's content is arbitrary JSON and could in principle contain anything after
/// canonicalization).
#[derive(Debug, Serialize, Deserialize)]
struct ProxiedRequest {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body_b64: String,
}

/// The response side of [`ProxiedRequest`], carried in [`Reply::payload`]. `Reply::status` (not a
/// field here) is the actual HTTP status the proxied handler returned — see [`RoomShardGate`]'s
/// docs on why that is safe for every status this workspace's own client-server API returns, and
/// the one documented edge case (`421`/`503`) where it is not.
#[derive(Debug, Serialize, Deserialize)]
struct ProxiedResponseBody {
    headers: Vec<(String, String)>,
    body_b64: String,
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s)
}

/// Extracts and percent-decodes the room id from a Matrix client-server path of the shape
/// `.../rooms/{roomId}/...`. Returns `None` for any path with no `rooms` segment at all — every
/// route this module does not need to gate (`/createRoom`, `/sync`, `/login`, `/media/...`, ...).
///
/// A room *created* through `/createRoom` is not gated by this function at all (there is no room
/// id in that request's path — the id is minted inside the handler). This is a known gap, not
/// fixed here: a `/createRoom` handled by a replica that does not end up owning the new room's
/// shard would construct that room's first `RoomActor` on the wrong replica. See
/// `docs/status/03-cluster.md`.
fn extract_room_id(path: &str) -> Option<String> {
    let mut segments = path.split('/');
    while let Some(segment) = segments.next() {
        if segment == "rooms" {
            let raw = segments.next()?;
            if raw.is_empty() {
                return None;
            }
            return Some(percent_decode(raw));
        }
    }
    None
}

/// A minimal percent-decoder for one path segment. No external crate: a Matrix room id is a short
/// ASCII string (`!localpart:server`) and `%XX` is the only escape any client library uses for it
/// in a path segment (`:` is the one byte that reliably gets encoded).
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `axum` middleware guarding every `/rooms/{roomId}/...` request: forwards or refuses a
/// request for a shard this replica does not own, before it can ever reach `hs-room`'s registry.
/// See this module's docs for the full design and `docs/status/03-cluster.md` for why gating
/// reads as well as writes is necessary (a stale, already-resident `RoomActor` on a non-owner
/// would otherwise serve `/messages` from its own out-of-date view forever, even once writes
/// alone are fixed).
pub struct RoomShardGate {
    ownership: Arc<dyn Ownership>,
    layout: ShardLayout,
    forwarder: Option<Arc<Forwarder>>,
    origin: ReplicaId,
    origin_generation: Generation,
    default_deadline: Duration,
}

impl RoomShardGate {
    /// Builds the gate from a started [`ClusterHandles`].
    #[must_use]
    pub fn new(handles: &ClusterHandles) -> Arc<Self> {
        Arc::new(Self {
            ownership: handles.cluster.ownership().clone(),
            layout: handles.layout,
            forwarder: handles.forwarder.clone(),
            origin: handles.origin.clone(),
            origin_generation: handles.origin_generation,
            default_deadline: handles.default_deadline,
        })
    }

    /// Wraps `app` with this gate as an `axum` middleware layer. Every request that does not
    /// match `/rooms/{roomId}/...` passes through untouched (no body buffering, no extra
    /// allocation beyond the path scan) — this is what keeps single-node mode's behavior and
    /// cost identical to before this module existed.
    pub fn layer(self: Arc<Self>, app: axum::Router) -> axum::Router {
        app.layer(axum::middleware::from_fn(
            move |req: Request, next: Next| {
                let gate = self.clone();
                async move { gate.run(req, next).await }
            },
        ))
    }

    async fn run(&self, req: Request, next: Next) -> Response {
        let Some(room_id) = extract_room_id(req.uri().path()) else {
            return next.run(req).await;
        };
        let shard = self.layout.room_shard(&room_id);
        if self.ownership.is_mine(shard) {
            return next.run(req).await;
        }
        match &self.forwarder {
            Some(forwarder) => match self.forward(forwarder, shard, req).await {
                Ok(response) => response,
                Err(reason) => self.refuse(shard, &reason),
            },
            None => self.refuse(shard, "no mesh forwarder is configured on this replica"),
        }
    }

    fn refuse(&self, shard: ShardId, reason: &str) -> Response {
        let message = match self.ownership.owner_of(shard) {
            Some(owner) => format!(
                "this replica does not own {shard} (believed owner: {owner}) and could not \
                 forward the request to it: {reason}"
            ),
            None => format!(
                "this replica does not own {shard} and no owner is currently known: {reason}"
            ),
        };
        tracing::warn!(%shard, %message, "refusing a room request this replica does not own");
        hs_http::error::MatrixError::custom(
            StatusCode::SERVICE_UNAVAILABLE,
            hs_http::error::MatrixErrorCode::Other("M_HS_NOT_SHARD_OWNER".to_owned()),
            message,
        )
        .into_response()
    }

    async fn forward(
        &self,
        forwarder: &Forwarder,
        shard: ShardId,
        req: Request,
    ) -> Result<Response, String> {
        let (parts, body) = req.into_parts();
        let body_bytes = to_bytes(body, MAX_PROXIED_BODY_BYTES)
            .await
            .map_err(|e| format!("reading the request body: {e}"))?;

        let mut headers = Vec::new();
        for (name, value) in parts.headers.iter() {
            if is_hop_by_hop(name.as_str()) {
                continue;
            }
            if let Ok(v) = value.to_str() {
                headers.push((name.as_str().to_owned(), v.to_owned()));
            }
        }
        let uri = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_owned())
            .unwrap_or_else(|| parts.uri.path().to_owned());

        let proxied = ProxiedRequest {
            method: parts.method.as_str().to_owned(),
            uri,
            headers,
            body_b64: base64_encode(&body_bytes),
        };
        let payload =
            serde_json::to_vec(&proxied).map_err(|e| format!("encoding the forward: {e}"))?;

        let env = Envelope {
            shard,
            route: "http.proxy".to_owned(),
            idempotency_key: IdempotencyKey::generate(),
            // `RequesterContext` is `hs-auth`'s `Requester`, not frozen yet (RFC 0001 section
            // 17); this proxy re-authenticates on the owner from the raw `Authorization` header
            // it forwarded above instead, so nothing needs to go here.
            requester: serde_json::Value::Null,
            deadline: self.default_deadline,
            origin: self.origin.clone(),
            origin_generation: self.origin_generation,
            hops: 0,
            traceparent: parts
                .headers
                .get(http::header::HeaderName::from_static("traceparent"))
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            payload: Bytes::from(payload),
        };

        let reply = forwarder
            .forward(env)
            .await
            .map_err(|e| format!("forwarding to the shard owner: {e}"))?;

        // `reply.status` here is the *proxied* application's status (a `200` from a successful
        // `PUT /send/...`, a `403` from a rejected event, ...) except in the one case
        // `Forwarder::forward` itself gives up and returns the mesh-level `421`/`503` verbatim
        // (misdirected after every retry, or persistently unavailable) — those two statuses
        // collide with the ones `hs-room`'s own handlers could in principle return for
        // unrelated reasons; this workspace's client-server API does not use either today, so
        // the collision is only a latent risk, documented in `docs/status/03-cluster.md`.
        let proxied_response: ProxiedResponseBody = serde_json::from_slice(&reply.payload)
            .map_err(|e| format!("decoding the forwarded response: {e}"))?;
        let body_bytes = base64_decode(&proxied_response.body_b64)
            .map_err(|e| format!("decoding the forwarded response body: {e}"))?;
        let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut builder = Response::builder().status(status);
        for (name, value) in &proxied_response.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
            .body(Body::from(body_bytes))
            .map_err(|e| format!("building the forwarded response: {e}"))
    }
}

/// The owner side of the reverse proxy: replays a [`ProxiedRequest`] against this process's own
/// `axum::Router` (the very same one serving its own listeners) and reports back the response.
/// This is what makes forwarding transparent to `hs-room`: the owner never learns the request
/// arrived over the mesh rather than a client socket.
struct ProxyShardHandler {
    app: axum::Router,
}

#[async_trait::async_trait]
impl ShardHandler for ProxyShardHandler {
    async fn handle(&self, env: Envelope, _fence: Fence) -> Reply {
        // `_fence` is not consulted: `hs-room`'s `RoomActor::persist` does not call
        // `Fence::check` yet (out of this crate's reach — see this module's docs and
        // docs/status/03-cluster.md item 4). Routing every write through the single owner
        // (this handler existing at all) is the primary defense; the fence is the documented
        // belt-and-braces gap left for track 04.
        let proxied: ProxiedRequest = match serde_json::from_slice(&env.payload) {
            Ok(p) => p,
            Err(e) => {
                return bad_request(format!("bad proxied request: {e}"));
            }
        };
        let body = match base64_decode(&proxied.body_b64) {
            Ok(b) => b,
            Err(e) => return bad_request(format!("bad proxied request body: {e}")),
        };

        let mut builder = axum::http::Request::builder()
            .method(proxied.method.as_str())
            .uri(proxied.uri.as_str());
        for (name, value) in &proxied.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = match builder.body(Body::from(body)) {
            Ok(r) => r,
            Err(e) => return bad_request(format!("bad proxied request: {e}")),
        };

        let response = match self.app.clone().oneshot(request).await {
            Ok(r) => r,
            Err(infallible) => match infallible {},
        };
        let status = response.status().as_u16();
        let mut headers = Vec::new();
        for (name, value) in response.headers().iter() {
            if is_hop_by_hop(name.as_str()) {
                continue;
            }
            if let Ok(v) = value.to_str() {
                headers.push((name.as_str().to_owned(), v.to_owned()));
            }
        }
        let body_bytes = match to_bytes(response.into_body(), MAX_PROXIED_BODY_BYTES).await {
            Ok(b) => b,
            Err(e) => {
                return Reply {
                    status: 502,
                    payload: Bytes::from(format!("reading the owner's response body: {e}")),
                };
            }
        };
        let out = ProxiedResponseBody {
            headers,
            body_b64: base64_encode(&body_bytes),
        };
        match serde_json::to_vec(&out) {
            Ok(payload) => Reply {
                status,
                payload: Bytes::from(payload),
            },
            Err(e) => Reply {
                status: 502,
                payload: Bytes::from(format!("encoding the owner's response: {e}")),
            },
        }
    }
}

fn bad_request(message: String) -> Reply {
    Reply {
        status: 400,
        payload: Bytes::from(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_room_id_from_client_server_paths() {
        assert_eq!(
            extract_room_id("/_matrix/client/v3/rooms/!abc%3Aexample.org/send/m.room.message/1"),
            Some("!abc:example.org".to_owned())
        );
        assert_eq!(
            extract_room_id("/_matrix/client/r0/rooms/!abc:example.org/messages"),
            Some("!abc:example.org".to_owned())
        );
    }

    #[test]
    fn does_not_match_unrelated_paths() {
        assert_eq!(extract_room_id("/_matrix/client/v3/createRoom"), None);
        assert_eq!(extract_room_id("/_matrix/client/v3/sync"), None);
        assert_eq!(extract_room_id("/health/ready"), None);
        assert_eq!(extract_room_id("/_matrix/client/v3/rooms/"), None);
        assert_eq!(extract_room_id("/_matrix/client/v3/rooms"), None);
    }

    #[test]
    fn percent_decoding_round_trips_common_room_ids() {
        assert_eq!(percent_decode("!abc%3Aexample.org"), "!abc:example.org");
        assert_eq!(percent_decode("!nopercent"), "!nopercent");
        // A trailing/incomplete escape is passed through byte-for-byte rather than panicking.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("100%2"), "100%2");
    }

    #[test]
    fn single_node_gate_never_forwards_or_refuses() {
        let handles = ClusterHandles {
            cluster: Cluster::single_node(ReplicaId::new("solo:1")),
            layout: ShardLayout::default(),
            forwarder: None,
            origin: ReplicaId::new("solo:1"),
            origin_generation: Generation::fresh(None),
            default_deadline: Duration::from_secs(10),
            mesh: None,
        };
        let gate = RoomShardGate::new(&handles);
        assert!(
            gate.ownership
                .is_mine(handles.layout.room_shard("!any:room.example.org"))
        );
        assert!(gate.forwarder.is_none());
    }
}
