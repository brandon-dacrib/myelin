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
//! authentication (mutual TLS against a private CA when `cluster.mesh.tls` is set, a shared
//! secret otherwise) only proves the request came from a trusted peer, not who the end user is.
//!
//! `POST /createRoom` has no room id in its path, so it is gated differently: the gate mints the
//! new room's id itself, hashes it, and either handles the request locally or forwards it to the
//! shard's owner with the id in a mesh-only header, which the owner's gate turns into a
//! [`hs_cluster::PreassignedRoomId`] request extension for the handler to create the room under.
//! See [`hs_cluster::create_room`] for the invariants and `docs/rfcs/0019-create-room-shard-gate.md`
//! for the one-line change `hs-room`'s handler needs to honour the extension.

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

use hs_cluster::mesh::tls::TlsMaterial;
use hs_cluster::mesh::{
    AuthMode, Authenticator, Envelope, Forwarder, IdempotencyCache, IdempotencyKey, MeshDeps,
    MeshServer, MutualTlsAuthenticator, Reply, ShardHandler, SharedSecretAuthenticator,
};
use hs_cluster::{
    Cluster, Fence, Generation, Ownership, PREASSIGNED_ROOM_ID_HEADER, PreassignedRoomId,
    ReplicaId, ShardId, ShardLayout, ViaMesh,
};
use hs_kv::KvBackend;

/// The largest body this process will buffer while proxying a forwarded room request. Room
/// events are small (Matrix caps `m.room.message` bodies well under this); a generous cap avoids
/// rejecting a legitimate large state event while still bounding memory under a hostile or
/// misbehaving peer.
const MAX_PROXIED_BODY_BYTES: usize = 10 * 1024 * 1024;

/// The `kind` label a forwarded client request is counted under in
/// `hs_cluster_forward_latency_seconds`.
const FORWARD_KIND_CLIENT: &str = "client";

/// The `kind` label a forwarded federation request (another server's) is counted under in
/// `hs_cluster_forward_latency_seconds`.
const FORWARD_KIND_FEDERATION: &str = "federation";

/// How many times [`RoomShardGate::run_create_room`] makes a `/createRoom` that `hs-room`'s fence
/// refused here (ownership moved while it ran) before refusing it to the client. Each attempt
/// chooses afresh, so the second normally forwards to the new owner; all of them together stay
/// inside the gate's request deadline.
const MAX_CREATE_ROOM_ATTEMPTS: u32 = 4;

/// Errors starting the cluster.
#[derive(Debug, thiserror::Error)]
pub enum ClusterSetupError {
    /// The `hs-cluster` ownership manager could not start (the store was unreachable, or the
    /// shard layout conflicts with what is already recorded).
    #[error(transparent)]
    Cluster(#[from] hs_cluster::error::ClusterError),
    /// The mesh's mutual-TLS material (`cluster.mesh.tls.*`) could not be loaded or turned into
    /// a `rustls` configuration: a missing or unreadable file, or one with no usable PEM in it.
    #[error("cluster.mesh.tls: {0}")]
    Tls(#[from] hs_cluster::mesh::tls::TlsError),
    /// `cluster.*` in the native config does not satisfy `hs-cluster`'s own invariants (for
    /// example `lease_ttl` too close to `heartbeat_interval`).
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
    /// The ownership manager's and forwarder's counters, for `/metrics`
    /// ([`hs_cluster::metrics::ClusterCollector`]). `None` in single-node mode, which has no
    /// cluster to report on.
    pub metrics: Option<Arc<hs_cluster::metrics::ClusterMetrics>>,
    origin: ReplicaId,
    origin_generation: Generation,
    default_deadline: Duration,
    /// This server's name, for minting a `/createRoom` id ahead of the handler (see
    /// [`RoomShardGate`]). `None` in single-node mode, where nothing is pre-minted.
    server_name: Option<ruma::OwnedServerName>,
    mesh: Option<MeshStartConfig>,
    /// The handlers for replica-to-replica messages (`hs_cluster::mesh::PeerHandler`), one per
    /// route prefix, added by whoever speaks on the mesh (`crate::sync_cluster::install` for
    /// `user.`, `crate::edu_forward::install` for `federation.`) and served by
    /// [`ClusterHandles::spawn_mesh`]: the mesh takes one handler, and this is it.
    peer_routes: Arc<PeerRoutes>,
    /// The handlers for forwarded shard requests that are not an HTTP request to replay, one
    /// per route prefix (`crate::federation_forward::install` for `federation.sink`). A forward
    /// whose route no prefix matches is replayed against the router, as every client request is.
    shard_routes: Arc<ShardRoutes>,
}

/// The [`ShardHandler`]s added for routes other than the HTTP proxy, by route prefix. Consulted
/// by [`ProxyShardHandler`] before it replays a forward against the router.
#[derive(Default)]
pub struct ShardRoutes {
    routes: std::sync::RwLock<Vec<(&'static str, Arc<dyn ShardHandler>)>>,
}

impl ShardRoutes {
    /// Adds `handler` for every forwarded route starting with `prefix`. A second handler for the
    /// same prefix is ignored with a warning.
    pub fn add(&self, prefix: &'static str, handler: Arc<dyn ShardHandler>) {
        let mut routes = self
            .routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if routes.iter().any(|(known, _)| *known == prefix) {
            tracing::warn!(
                prefix,
                "a mesh shard handler was already added for this prefix; ignoring the second"
            );
            return;
        }
        routes.push((prefix, handler));
        routes.sort_by_key(|(known, _)| std::cmp::Reverse(known.len()));
    }

    fn handler_for(&self, route: &str) -> Option<Arc<dyn ShardHandler>> {
        self.routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(prefix, _)| route.starts_with(prefix))
            .map(|(_, handler)| handler.clone())
    }
}

/// The one [`hs_cluster::mesh::PeerHandler`] the mesh serves, handing each message to the
/// handler added for the longest prefix of its route. A route no prefix matches is answered
/// `404`.
#[derive(Default)]
pub struct PeerRoutes {
    routes: std::sync::RwLock<Vec<(&'static str, Arc<dyn hs_cluster::mesh::PeerHandler>)>>,
}

impl PeerRoutes {
    /// Adds `handler` for every route starting with `prefix`. A second handler for the same
    /// prefix is ignored with a warning.
    pub fn add(&self, prefix: &'static str, handler: Arc<dyn hs_cluster::mesh::PeerHandler>) {
        let mut routes = self
            .routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if routes.iter().any(|(known, _)| *known == prefix) {
            tracing::warn!(
                prefix,
                "a mesh peer handler was already added for this prefix; ignoring the second"
            );
            return;
        }
        routes.push((prefix, handler));
        // Longest first, so the most specific prefix wins.
        routes.sort_by_key(|(known, _)| std::cmp::Reverse(known.len()));
    }

    /// Whether any handler has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn handler_for(&self, route: &str) -> Option<Arc<dyn hs_cluster::mesh::PeerHandler>> {
        self.routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(prefix, _)| route.starts_with(prefix))
            .map(|(_, handler)| handler.clone())
    }
}

#[async_trait::async_trait]
impl hs_cluster::mesh::PeerHandler for PeerRoutes {
    async fn handle(
        &self,
        from: ReplicaId,
        route: &str,
        payload: bytes::Bytes,
    ) -> hs_cluster::mesh::Reply {
        match self.handler_for(route) {
            Some(handler) => handler.handle(from, route, payload).await,
            None => hs_cluster::mesh::Reply {
                status: 404,
                payload: bytes::Bytes::from(format!("no such peer route: {route}")),
            },
        }
    }
}

/// How the mesh listener authenticates peers, decided once in [`start`] from
/// `cluster.mesh.tls`.
enum MeshAuth {
    /// `Authorization: Bearer <secret>` over plaintext HTTP/2.
    SharedSecret(String),
    /// Client certificates chained to the private CA in `material`, optionally restricted to a
    /// DNS SAN suffix.
    MutualTls {
        material: TlsMaterial,
        peer_san_suffix: Option<String>,
    },
}

struct MeshStartConfig {
    listen_addr: String,
    auth: MeshAuth,
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
    if config.cluster.mesh.advertise_address.is_none() {
        tracing::warn!(
            advertised = %cluster_config.mesh_advertise_addr,
            "cluster.mesh.advertise_address is unset; advertising the first listener's bind \
             address to peers, which is only right when every replica shares this host (set \
             HS__CLUSTER__MESH__ADVERTISE_ADDRESS per pod in Kubernetes)"
        );
    }
    let auth_mode = cluster_config.mesh.auth.clone();
    let (mesh_auth, tls_for_forwarder) = match &auth_mode {
        AuthMode::SharedSecret { secret } => (MeshAuth::SharedSecret(secret.clone()), None),
        AuthMode::MutualTls {
            ca_file,
            cert_file,
            key_file,
            peer_san_suffix,
        } => {
            let material = TlsMaterial::load(ca_file, cert_file, key_file)?;
            // The forwarder builds its client config from a borrowed `TlsMaterial` and keeps
            // only the resulting `rustls::ClientConfig`; the listener needs the material itself
            // later, in `spawn_mesh`, so it is loaded twice rather than cloned (`TlsMaterial`
            // holds a private key and deliberately is not `Clone`).
            let for_forwarder = TlsMaterial::load(ca_file, cert_file, key_file)?;
            tracing::info!(
                ca = %ca_file.display(),
                certificate = %cert_file.display(),
                peer_san_suffix = ?peer_san_suffix,
                "mesh authentication is mutual TLS"
            );
            (
                MeshAuth::MutualTls {
                    material,
                    peer_san_suffix: peer_san_suffix.clone(),
                },
                Some(for_forwarder),
            )
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
    let server_name = ruma::ServerName::parse(&config.server.server_name)
        .map(|name| name.to_owned())
        .map_err(|e| {
            ClusterSetupError::Invalid(format!(
                "server.server_name {:?} is not a valid Matrix server name: {e}",
                config.server.server_name
            ))
        })?;

    tracing::info!(
        replica = %me,
        mesh_listen = %listen_addr,
        "starting the cluster ownership manager"
    );
    {
        let store = hs_cluster::store::ClusterStore::open(backend.clone())?;
        let lease_ttl = cluster_config.lease_ttl;
        let me = me.clone();
        tokio::task::spawn_blocking(move || {
            refuse_live_duplicate(&store, &me, lease_ttl, unix_now_ms())
        })
        .await
        .map_err(|e| ClusterSetupError::Invalid(format!("checking the replica registry: {e}")))??;
    }
    let (cluster, manager) = Cluster::start(cluster_config, backend).await?;
    let ownership = cluster.ownership().clone();
    let metrics = manager.metrics();

    let forwarder = Arc::new(Forwarder::new(
        auth_mode,
        tls_for_forwarder.as_ref(),
        max_hops,
        max_attempts,
        retry_base_backoff,
        ownership.clone(),
        metrics.clone(),
    )?);

    Ok(ClusterHandles {
        cluster,
        layout,
        forwarder: Some(forwarder),
        metrics: Some(metrics),
        origin: me,
        origin_generation: Generation::fresh(None),
        default_deadline,
        server_name: Some(server_name),
        mesh: Some(MeshStartConfig {
            listen_addr,
            auth: mesh_auth,
            idempotency_ttl,
            max_in_flight_per_peer,
            ownership,
        }),
        peer_routes: Arc::new(PeerRoutes::default()),
        shard_routes: Arc::new(ShardRoutes::default()),
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
            metrics: None,
            origin: me,
            origin_generation: Generation::fresh(None),
            default_deadline: Duration::from_secs(10),
            server_name: None,
            mesh: None,
            peer_routes: Arc::new(PeerRoutes::default()),
            shard_routes: Arc::new(ShardRoutes::default()),
        }
    }

    /// This replica's identity on the mesh (`host:port` of its listener).
    #[must_use]
    pub fn origin(&self) -> &ReplicaId {
        &self.origin
    }

    /// This process's generation: what tells a peer that a replica it knew has restarted.
    #[must_use]
    pub fn origin_generation(&self) -> Generation {
        self.origin_generation
    }

    /// How long a forward from this replica may take, retries included
    /// (`cluster.mesh.default_deadline`).
    #[must_use]
    pub fn default_deadline(&self) -> Duration {
        self.default_deadline
    }

    /// Adds a handler for forwarded shard requests whose route starts with `prefix`, served on
    /// this replica when it owns the shard (see [`ShardRoutes`]). Must be called before
    /// [`ClusterHandles::spawn_mesh`].
    pub fn add_shard_handler(&self, prefix: &'static str, handler: Arc<dyn ShardHandler>) {
        self.shard_routes.add(prefix, handler);
    }

    /// Adds a handler for the `POST /mesh/v1/peer` messages whose route starts with `prefix`
    /// (see [`PeerRoutes`]). Must be called before [`ClusterHandles::spawn_mesh`].
    pub fn add_peer_handler(
        &self,
        prefix: &'static str,
        handler: Arc<dyn hs_cluster::mesh::PeerHandler>,
    ) {
        self.peer_routes.add(prefix, handler);
    }

    /// Starts the mesh HTTP/2 listener, if this replica is clustered (`None` in single-node
    /// mode, where nothing should ever dial in). `app` must be the exact same router serving this
    /// process's own client listeners: a forwarded request re-enters it in-process on the owner,
    /// so the owner's own auth middleware, room actor and persistence run exactly as they would
    /// for a request that arrived directly (see this module's docs).
    #[must_use]
    pub fn spawn_mesh(&self, app: axum::Router) -> Option<MeshRuntime> {
        let mesh = self.mesh.as_ref()?;
        let (authenticator, tls_material): (Arc<dyn Authenticator>, Option<&TlsMaterial>) =
            match &mesh.auth {
                MeshAuth::SharedSecret(secret) => (
                    Arc::new(SharedSecretAuthenticator::new(secret.clone())),
                    None,
                ),
                MeshAuth::MutualTls {
                    material,
                    peer_san_suffix,
                } => (
                    Arc::new(MutualTlsAuthenticator::new(peer_san_suffix.clone())),
                    Some(material),
                ),
            };
        let handler: Arc<dyn ShardHandler> = Arc::new(ProxyShardHandler {
            app,
            routes: self.shard_routes.clone(),
        });
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
            // No handler at all is the mesh's own `501`, which a sender reads as "this peer
            // speaks no peer messages" (a replica from before they existed).
            peers: (!self.peer_routes.is_empty())
                .then(|| self.peer_routes.clone() as Arc<dyn hs_cluster::mesh::PeerHandler>),
        });
        let server = match MeshServer::new(mesh.listen_addr.clone(), tls_material) {
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

/// The `host:port` this replica advertises to its peers as its mesh address, and uses as its
/// replica identity (see [`to_hs_cluster_config`]).
///
/// `cluster.mesh.advertise_address` wins when set: a bare host gets `cluster.mesh.port`
/// appended, a `host:port` is used as given (an IPv6 address must be bracketed, `[fd00::1]`,
/// for its port to be told apart from its own colons). Unset, the first configured listener's
/// bind address is used, with `0.0.0.0`/`::` (a wildcard bind, not a dialable address) and an
/// empty value falling back to `127.0.0.1` -- right only when every replica shares one host, as
/// in the two-process experiment in `docs/status/03-cluster.md`; [`start`] logs a warning in
/// that case.
/// The name this process runs admin tasks under (`hs_admin::tasks::TaskRegistry`): a fixed
/// `single-node` when there is one process, so a restart recognises the tasks it left behind;
/// in cluster mode, this replica's identity, so one replica's restart never touches another's.
#[must_use]
pub fn task_runner_name(config: &hs_config::Config) -> String {
    if config.cluster.single_node {
        "single-node".to_owned()
    } else {
        advertise_addr(config)
    }
}

fn advertise_addr(config: &hs_config::Config) -> String {
    let port = config.cluster.mesh.port;
    if let Some(configured) = config
        .cluster
        .mesh
        .advertise_address
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return match split_host_port(configured) {
            Some(_) => configured.to_owned(),
            None => format!("{configured}:{port}"),
        };
    }
    let host = config
        .listeners
        .listeners
        .first()
        .and_then(|listener| listener.bind_addresses.first())
        .map(String::as_str)
        .filter(|addr| !addr.is_empty() && *addr != "0.0.0.0" && *addr != "::")
        .unwrap_or("127.0.0.1");
    format!("{host}:{port}")
}

/// Refuses to start when the replica registry already holds a *live* row under this replica's
/// identity: another process is advertising the same mesh address right now. Two replicas with
/// one identity would take turns overwriting each other's registry row and each other's shard
/// ownership, a split-brain by configuration. The usual cause is the configuration store: a
/// per-replica `cluster.mesh.advertise_address` written into the shared database by the first
/// replica to start is what every later replica reads unless it sets its own through the
/// environment (`HS__CLUSTER__MESH__ADVERTISE_ADDRESS`, which outranks the database; the chart
/// does this per pod). A *stale* row under this identity (a restart of the same pod, whose old
/// row is past `lease_ttl`) is fine and is taken over by the normal generation rule.
///
/// # Errors
/// [`ClusterSetupError::Invalid`] naming the live row and the environment variable to set.
fn refuse_live_duplicate<B: KvBackend>(
    store: &hs_cluster::store::ClusterStore<B>,
    me: &ReplicaId,
    lease_ttl: Duration,
    now_unix_ms: u64,
) -> Result<(), ClusterSetupError> {
    let lease_ms = u64::try_from(lease_ttl.as_millis()).unwrap_or(u64::MAX);
    let live_duplicate = store
        .list_replicas()?
        .into_iter()
        .find(|row| &row.id == me && now_unix_ms.saturating_sub(row.heartbeat_unix_ms) < lease_ms);
    match live_duplicate {
        Some(row) => Err(ClusterSetupError::Invalid(format!(
            "another replica is already live under this identity ({me}, last heartbeat {} ms \
             ago, generation {}): every replica needs its own cluster.mesh.advertise_address. \
             A value in the shared configuration database is read by every replica; set it per \
             replica through the environment (HS__CLUSTER__MESH__ADVERTISE_ADDRESS), which \
             outranks the database, or unset it there (hs config unset \
             /cluster/mesh/advertise_address)",
            now_unix_ms.saturating_sub(row.heartbeat_unix_ms),
            row.generation.0
        ))),
        None => Ok(()),
    }
}

fn unix_now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Splits `host:port` into its parts, or `None` if `addr` carries no port: a bare host name, a
/// bare IPv4 address, or an unbracketed IPv6 address (whose colons are not a port separator).
fn split_host_port(addr: &str) -> Option<(&str, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let is_bracketed_v6 = host.starts_with('[') && host.ends_with(']');
    if host.contains(':') && !is_bracketed_v6 {
        return None;
    }
    Some((host, port))
}

/// Converts `hs-config`'s `cluster` section into `hs-cluster`'s own config type. The two shapes do
/// not line up one-to-one (see this crate's module docs and `docs/status/03-cluster.md`): this
/// replica's identity is its mesh address ([`advertise_addr`]), and federation/appservice shard
/// counts have no `hs-config` field yet (`layout` is filled in by the caller from `hs-cluster`'s
/// own defaults for those two kinds).
fn to_hs_cluster_config(
    config: &hs_config::Config,
    layout: ShardLayout,
) -> hs_cluster::ClusterConfig {
    let cluster_cfg = &config.cluster;
    let mesh_advertise_addr = advertise_addr(config);
    // The forwarder dials `owner.as_str()` directly as a `host:port` (see
    // `hs_cluster::mesh::forwarder::Forwarder::resolve_addr`'s doc comment on this exact
    // convention), so this replica's identity *is* its dialable mesh address rather than a
    // separate name that would need a lookup table.
    let me = ReplicaId::new(mesh_advertise_addr.clone());

    let mut cfg = hs_cluster::ClusterConfig::new(me, mesh_advertise_addr, layout);
    cfg.heartbeat_interval = cluster_cfg.heartbeat_interval.as_std();
    cfg.lease_ttl = cluster_cfg.lease_ttl.as_std();
    cfg.mesh.listen_addr = format!("0.0.0.0:{}", cluster_cfg.mesh.port);
    cfg.mesh.auth = match &cluster_cfg.mesh.tls {
        Some(tls) => AuthMode::MutualTls {
            ca_file: tls.ca_certificate_path.clone(),
            cert_file: tls.certificate_path.clone(),
            key_file: tls.private_key_path.clone(),
            peer_san_suffix: tls.peer_san_suffix.clone(),
        },
        None => AuthMode::SharedSecret {
            secret: cluster_cfg
                .mesh
                .shared_secret
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("dev-only-shared-secret")
                .to_owned(),
        },
    };
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
/// `.../rooms/{roomId}/...`, or `.../join/{roomId}` and `.../knock/{roomId}` when what follows
/// is a room id (`!...`) rather than an alias. Returns `None` for any other path — every route
/// this module does not need to gate (`/sync`, `/login`, `/media/...`, ...) and the alias forms
/// of `/join` and `/knock`, whose room id is only known once the alias is resolved: the gate
/// does that itself first ([`extract_alias`], [`AliasResolver`]). `/createRoom` has no room id
/// in its path either and is gated separately, by [`is_create_room`] and
/// [`RoomShardGate::run_create_room`].
fn extract_room_id(path: &str) -> Option<String> {
    let mut segments = path.split('/');
    while let Some(segment) = segments.next() {
        match segment {
            "rooms" => {
                let raw = segments.next()?;
                if raw.is_empty() {
                    return None;
                }
                return Some(percent_decode(raw));
            }
            "join" | "knock" => {
                let decoded = percent_decode(segments.next()?);
                return decoded.starts_with('!').then_some(decoded);
            }
            _ => {}
        }
    }
    None
}

/// The federation endpoints whose first path parameter is a room this server answers for, so
/// only the room's owner may answer them: the membership handshakes (`make_join`, `send_join`,
/// `make_leave`, `send_leave`, `make_knock`, `send_knock`), `invite` and
/// `exchange_third_party_invite`, which write to the room; and the room reads another server
/// walks the room's history with (`state`, `state_ids`, `backfill`, `get_missing_events`,
/// `event_auth`, `timestamp_to_event`, `hierarchy`, `extremities`), which a non-owner's copy of
/// the room could answer stale -- a server fetching an event the owner has just sent it would be
/// told it does not exist. `/rooms/{roomId}/...` (the room complexity) is matched like any other
/// `/rooms/` path by [`extract_room_id`]. `/send` carries PDUs of any number of rooms and is
/// handled per PDU instead (`crate::federation_forward`).
const FEDERATION_ROOM_ENDPOINTS: &[&str] = &[
    "make_join",
    "send_join",
    "make_leave",
    "send_leave",
    "make_knock",
    "send_knock",
    "invite",
    "exchange_third_party_invite",
    "state",
    "state_ids",
    "backfill",
    "get_missing_events",
    "event_auth",
    "timestamp_to_event",
    "hierarchy",
    "extremities",
];

/// The percent-decoded room id of a federation request for one room
/// (`/_matrix/federation/{version}/{endpoint}/{roomId}/...`, the endpoint one of
/// [`FEDERATION_ROOM_ENDPOINTS`]), or `None` for every other path.
fn extract_federation_room_id(path: &str) -> Option<String> {
    let path = path.split_once('?').map_or(path, |(path, _query)| path);
    let rest = path.strip_prefix("/_matrix/federation/")?;
    let mut segments = rest.split('/');
    let _version = segments.next()?;
    let endpoint = segments.next()?;
    if !FEDERATION_ROOM_ENDPOINTS.contains(&endpoint) {
        return None;
    }
    let room_id = percent_decode(segments.next()?);
    room_id.starts_with('!').then_some(room_id)
}

/// The alias in `.../join/{alias}` or `.../knock/{alias}`, percent-decoded, when the segment is an
/// alias (`#...`) rather than a room id. `None` for every other path.
fn extract_alias(path: &str) -> Option<String> {
    let mut segments = path.split('/');
    while let Some(segment) = segments.next() {
        if matches!(segment, "join" | "knock") {
            let decoded = percent_decode(segments.next()?);
            return decoded.starts_with('#').then_some(decoded);
        }
        if segment == "rooms" {
            return None;
        }
    }
    None
}

/// `path` and `query` with the alias segment after `join`/`knock` replaced by `room_id`, and
/// each of `via` appended as a `server_name` parameter after whatever the client sent -- the
/// order `hs-room`'s join handler would have tried them in had it resolved the alias itself.
fn alias_rewritten_uri(path: &str, query: Option<&str>, room_id: &str, via: &[String]) -> String {
    let mut out = String::with_capacity(path.len() + room_id.len());
    let mut replace_next = false;
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if replace_next {
            out.push_str(&percent_encode(room_id));
            replace_next = false;
            continue;
        }
        out.push_str(segment);
        replace_next = matches!(segment, "join" | "knock");
    }
    let mut params: Vec<String> = query
        .filter(|q| !q.is_empty())
        .map(|q| vec![q.to_owned()])
        .unwrap_or_default();
    params.extend(
        via.iter()
            .map(|server| format!("server_name={}", percent_encode(server))),
    );
    if !params.is_empty() {
        out.push('?');
        out.push_str(&params.join("&"));
    }
    out
}

/// Percent-encodes everything but RFC 3986's unreserved characters, for one path segment or
/// query value.
fn percent_encode(raw: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// What a room alias in `/join/{alias}` or `/knock/{alias}` resolved to, for [`RoomShardGate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAlias {
    /// The room the alias names.
    pub room_id: String,
    /// Servers to try the join through after any the client named: for an alias on another
    /// server, the servers its directory named and the alias's own server, as `hs-room`'s join
    /// handler would add them; empty for a local alias.
    pub via: Vec<String>,
}

/// Resolves a room alias for [`RoomShardGate`] before it decides where a `/join/{alias}` or
/// `/knock/{alias}` runs, so that a join by alias through a replica that does not own the room
/// is forwarded to the owner exactly as a join by id is.
#[async_trait::async_trait]
pub trait AliasResolver: Send + Sync {
    /// The room `alias` names, or `None` when it names none (or could not be asked); the request
    /// then goes to the handler as it came, which answers the client's error itself.
    async fn resolve(&self, alias: &str) -> Option<ResolvedAlias>;
}

/// [`AliasResolver`] over `hs-room`'s own directory: a local alias is read from the store
/// (`RoomRegistry::resolve_alias`, which loads no room), an alias on another server is asked of
/// that server's directory through the same [`hs_room::remote_join::RemoteJoin`] the join
/// handler uses.
pub struct RoomAliasResolver<B: KvBackend> {
    rooms: Arc<hs_room::registry::RoomRegistry<B>>,
    server_name: ruma::OwnedServerName,
    remote_join: Option<Arc<dyn hs_room::remote_join::RemoteJoin>>,
}

impl<B: KvBackend> RoomAliasResolver<B> {
    /// Resolves through the same registry, identity and federation the room routes use.
    #[must_use]
    pub fn new(room: &hs_room::state::RoomState<B>) -> Self {
        Self {
            rooms: room.rooms.clone(),
            server_name: room.identity.server_name.clone(),
            remote_join: room.remote_join.clone(),
        }
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> AliasResolver for RoomAliasResolver<B> {
    async fn resolve(&self, alias: &str) -> Option<ResolvedAlias> {
        let alias = ruma::RoomAliasId::parse(alias).ok()?;
        match self.rooms.resolve_alias(&alias) {
            Ok(Some(room_id)) => {
                return Some(ResolvedAlias {
                    room_id: room_id.to_string(),
                    via: Vec::new(),
                });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%alias, %error, "could not read the alias directory ahead of the shard gate");
                return None;
            }
        }
        if alias.server_name() == self.server_name {
            return None;
        }
        let remote = self.remote_join.as_ref()?;
        match remote.resolve_alias(&alias).await {
            Ok((room_id, servers)) => {
                let mut via: Vec<String> = Vec::new();
                for server in servers
                    .into_iter()
                    .chain(std::iter::once(alias.server_name().to_string()))
                {
                    if !via.contains(&server) {
                        via.push(server);
                    }
                }
                Some(ResolvedAlias {
                    room_id: room_id.to_string(),
                    via,
                })
            }
            Err(error) => {
                tracing::debug!(%alias, %error, "the alias's server did not resolve it ahead of the shard gate");
                None
            }
        }
    }
}

/// Whether a request is `POST /_matrix/client/{version}/createRoom`, the one room request whose
/// room id is not in its path.
fn is_create_room(method: &http::Method, path: &str) -> bool {
    method == http::Method::POST && path.ends_with("/createRoom")
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
    /// For minting a `/createRoom` id ahead of the handler; `None` in single-node mode, where
    /// `/createRoom` passes through untouched.
    server_name: Option<ruma::OwnedServerName>,
    /// Resolves the alias of `/join/{alias}` and `/knock/{alias}` before the gate decides; `None`
    /// leaves those to the handler wherever they land (and in single-node mode it is never asked:
    /// every shard is this replica's).
    alias_resolver: Option<Arc<dyn AliasResolver>>,
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
            server_name: handles.server_name.clone(),
            alias_resolver: None,
        })
    }

    /// [`RoomShardGate::new`], also resolving the alias of `/join/{alias}` and `/knock/{alias}`
    /// through `resolver` so that a join by alias is gated as a join by id is.
    #[must_use]
    pub fn with_alias_resolver(
        handles: &ClusterHandles,
        resolver: Arc<dyn AliasResolver>,
    ) -> Arc<Self> {
        let mut gate = Self::new(handles);
        if let Some(gate) = Arc::get_mut(&mut gate) {
            gate.alias_resolver = Some(resolver);
        }
        gate
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
        if is_create_room(req.method(), req.uri().path()) {
            return self.run_create_room(req, next).await;
        }
        let mut req = req;
        let (room_id, kind) = if let Some(room_id) = extract_federation_room_id(req.uri().path()) {
            (room_id, FORWARD_KIND_FEDERATION)
        } else {
            let room_id = match extract_room_id(req.uri().path()) {
                Some(room_id) => room_id,
                None => match self.resolve_alias_ahead(&mut req).await {
                    Some(room_id) => room_id,
                    None => return next.run(req).await,
                },
            };
            let kind = if req.uri().path().starts_with("/_matrix/federation/") {
                FORWARD_KIND_FEDERATION
            } else {
                FORWARD_KIND_CLIENT
            };
            (room_id, kind)
        };
        let shard = self.layout.room_shard(&room_id);
        if self.ownership.is_mine(shard) {
            return self.run_owned(shard, req, next, kind).await;
        }
        match &self.forwarder {
            Some(forwarder) => match self.forward(forwarder, shard, req, kind).await {
                Ok(response) => response,
                Err(reason) => self.refuse(shard, &reason),
            },
            None => self.refuse(shard, "no mesh forwarder is configured on this replica"),
        }
    }

    /// For `/join/{alias}` and `/knock/{alias}` on a clustered replica: resolves the alias and
    /// rewrites the request to name the room id instead (plus the `server_name`s the handler
    /// would have added for an alias on another server), so the gate can route it like a join by
    /// id and the owner does not resolve it a second time. `None` -- the request is left as it
    /// came and passes through to the handler -- for every other path, in single-node mode, and
    /// when the alias names no room (the handler then gives the client its `404`).
    async fn resolve_alias_ahead(&self, req: &mut Request) -> Option<String> {
        let resolver = self.alias_resolver.as_ref()?;
        self.forwarder.as_ref()?;
        let alias = extract_alias(req.uri().path())?;
        let Some(resolved) = resolver.resolve(&alias).await else {
            tracing::debug!(%alias, "the alias names no room; leaving the request to the handler");
            return None;
        };
        let rewritten = alias_rewritten_uri(
            req.uri().path(),
            req.uri().query(),
            &resolved.room_id,
            &resolved.via,
        );
        match rewritten.parse::<http::Uri>() {
            Ok(uri) => *req.uri_mut() = uri,
            Err(error) => {
                tracing::warn!(%alias, room_id = %resolved.room_id, %error, "could not rewrite a request by alias to its room id; leaving it to the handler");
                return None;
            }
        }
        let shard = self.layout.room_shard(&resolved.room_id);
        tracing::info!(
            %alias,
            room_id = %resolved.room_id,
            %shard,
            owned_here = self.ownership.is_mine(shard),
            "resolved a join or knock by alias ahead of the shard gate"
        );
        Some(resolved.room_id)
    }

    /// Handles a room request whose shard this replica owned when it arrived. If the shard moves
    /// away while the request runs, `hs-room`'s fence refuses the write with a `503` and nothing
    /// is persisted; this then sends the same request on to the new owner instead of handing
    /// the client that `503`. Seen on two pods on a real cluster (2026-09-28): a replica coming
    /// back takes its shards from the survivor, and a send already past the gate on the survivor
    /// was fenced.
    ///
    /// Only at the edge: a request that came in over the mesh returns its `503` to the replica
    /// that forwarded it, whose forwarder retries against the current owner. In single-node mode
    /// the request passes straight through (no forwarder, and no buffering).
    async fn run_owned(
        &self,
        shard: ShardId,
        req: Request,
        next: Next,
        kind: &'static str,
    ) -> Response {
        let Some(forwarder) = &self.forwarder else {
            return next.run(req).await;
        };
        if req.extensions().get::<ViaMesh>().is_some() {
            return next.run(req).await;
        }
        let (parts, body) = req.into_parts();
        let body = match to_bytes(body, MAX_PROXIED_BODY_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                return hs_http::error::MatrixError::custom(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    hs_http::error::MatrixErrorCode::TooLarge,
                    format!("reading the request body: {error}"),
                )
                .into_response();
            }
        };
        let response = next
            .run(Request::from_parts(parts.clone(), Body::from(body.clone())))
            .await;
        if response.status() != StatusCode::SERVICE_UNAVAILABLE || self.ownership.is_mine(shard) {
            return response;
        }
        tracing::info!(
            %shard,
            kind,
            "the room's shard moved while a request for it ran here; sending it on to the new owner"
        );
        match self
            .forward(
                forwarder,
                shard,
                Request::from_parts(parts, Body::from(body)),
                kind,
            )
            .await
        {
            Ok(forwarded) => forwarded,
            Err(reason) => self.refuse(shard, &reason),
        }
    }

    /// Gates `POST /createRoom` (see [`hs_cluster::create_room`] for the design): mints the new
    /// room's id, and handles the request here if this replica owns the id's shard or forwards
    /// it to the owner with the id in the mesh-only header otherwise. On the receiving end of a
    /// forward the header becomes a [`PreassignedRoomId`] extension, after re-checking ownership.
    /// Any value a *client* sent under the header is dropped: only a request that re-entered the
    /// router from the mesh (marked [`ViaMesh`] by [`ProxyShardHandler`]) may carry one.
    async fn run_create_room(&self, mut req: Request, next: Next) -> Response {
        let via_mesh = req.extensions().get::<ViaMesh>().is_some();
        let header = req.headers_mut().remove(PREASSIGNED_ROOM_ID_HEADER);
        let forwarded_id = if via_mesh {
            header.and_then(|v| v.to_str().ok().map(str::to_owned))
        } else {
            None
        };

        if let Some(room_id) = forwarded_id {
            let shard = self.layout.room_shard(&room_id);
            if !self.ownership.is_mine(shard) {
                return self.refuse(
                    shard,
                    "a forwarded /createRoom named a room whose shard this replica does not \
                     own; ownership moved, and the sender retries against the current owner",
                );
            }
            req.extensions_mut()
                .insert(PreassignedRoomId(room_id.clone()));
            let response = next.run(req).await;
            return self.check_created_room(response, &room_id).await;
        }

        let (Some(server_name), Some(forwarder)) = (&self.server_name, &self.forwarder) else {
            // Single-node mode: every shard is this replica's, so the handler's own id is as
            // good as any and nothing is pre-minted.
            return next.run(req).await;
        };

        // At the edge the request is kept so that a create fenced here can be made again.
        let (parts, body) = req.into_parts();
        let body = match to_bytes(body, MAX_PROXIED_BODY_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                return hs_http::error::MatrixError::custom(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    hs_http::error::MatrixErrorCode::TooLarge,
                    format!("reading the request body: {error}"),
                )
                .into_response();
            }
        };
        let started = std::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let req = Request::from_parts(parts.clone(), Body::from(body.clone()));
            let (response, fenced_here) = self
                .create_room_once(req, next.clone(), server_name, forwarder)
                .await;
            let Some(fenced_shard) = fenced_here else {
                return response;
            };
            // The room's shard (or, for a version-12 room, every shard this replica could
            // place it on) moved away between the gate's ownership check and the handler's
            // fenced write, or this replica stopped believing it owns anything: nothing was
            // committed past the point the fence refused. Choose again from the current
            // ownership, which forwards the create when the shard is now elsewhere.
            let wait = Duration::from_millis(100 * u64::from(attempt));
            if attempt >= MAX_CREATE_ROOM_ATTEMPTS
                || started.elapsed() + wait >= self.default_deadline
            {
                tracing::warn!(
                    attempts = attempt,
                    "a /createRoom was fenced on every attempt here; refusing it so that the \
                     client retries"
                );
                return self.refuse(
                    fenced_shard,
                    "the room's shard kept moving while it was being created; ownership did \
                     not settle in time",
                );
            }
            tracing::info!(
                attempt,
                "a /createRoom was fenced here because ownership moved while it ran; creating \
                 it again against the current ownership"
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// One attempt at an edge `/createRoom` (see [`Self::run_create_room`]): mints an id, runs
    /// the handler here if this replica owns the id's shard and forwards the request otherwise.
    /// The second value names the shard when the handler ran here and answered `503` --
    /// `hs-room`'s fence refused the creation because ownership moved under it -- so the caller
    /// may try again.
    async fn create_room_once(
        &self,
        mut req: Request,
        next: Next,
        server_name: &ruma::ServerName,
        forwarder: &Forwarder,
    ) -> (Response, Option<ShardId>) {
        let room_id = ruma::RoomId::new_v1(server_name).to_string();
        let shard = self.layout.room_shard(&room_id);
        if self.ownership.is_mine(shard) {
            req.extensions_mut()
                .insert(PreassignedRoomId(room_id.clone()));
            let response = next.run(req).await;
            if response.status() == StatusCode::SERVICE_UNAVAILABLE {
                return (response, Some(shard));
            }
            return (self.check_created_room(response, &room_id).await, None);
        }

        let Ok(value) = http::HeaderValue::from_str(&room_id) else {
            // A freshly minted `!localpart:server` is always a valid header value; this arm is
            // unreachable in practice and kept only so the gate cannot panic.
            return (
                self.refuse(shard, "the minted room id is not a valid header value"),
                None,
            );
        };
        req.headers_mut().insert(PREASSIGNED_ROOM_ID_HEADER, value);
        tracing::debug!(%room_id, %shard, "forwarding /createRoom to the shard's owner");
        let response = match self
            .forward(forwarder, shard, req, FORWARD_KIND_CLIENT)
            .await
        {
            Ok(response) => response,
            Err(reason) => self.refuse(shard, &reason),
        };
        (response, None)
    }

    /// Reads the `room_id` out of a successful `/createRoom` response and warns if it is not the
    /// pre-assigned one, or hashes to a shard this replica does not own: either means the handler
    /// minted its own id (`hs-room` not yet honouring [`PreassignedRoomId`], or a room version
    /// whose id is derived from the create event) and the room's first actor may have been built
    /// on a non-owner. The response is passed on unchanged either way; this is a diagnostic, and
    /// the room is usable (every later request is routed to its true owner).
    async fn check_created_room(&self, response: Response, preassigned: &str) -> Response {
        if response.status() != StatusCode::OK {
            return response;
        }
        let (parts, body) = response.into_parts();
        let bytes = match to_bytes(body, MAX_PROXIED_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, "could not read the /createRoom response body");
                return Response::from_parts(parts, Body::empty());
            }
        };
        let created = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("room_id")?.as_str().map(str::to_owned));
        match created {
            Some(id) if id == preassigned => {
                tracing::debug!(room_id = %id, "created a room under its pre-assigned id");
            }
            Some(id) if self.ownership.is_mine(self.layout.room_shard(&id)) => {
                // A room version whose id is the create event's hash (12+): the handler rebuilt
                // the create event until its id landed on a shard this replica owns (decision
                // 0020), so the pre-assigned id was only ever the routing choice.
                tracing::debug!(
                    room_id = %id,
                    "created a room under a hash-derived id on a shard this replica owns"
                );
            }
            Some(id) => {
                let shard = self.layout.room_shard(&id);
                let mine = self.ownership.is_mine(shard);
                tracing::warn!(
                    room_id = %id,
                    %preassigned,
                    %shard,
                    owned_here = mine,
                    "the /createRoom handler minted its own room id instead of the pre-assigned \
                     one (see docs/rfcs/0019-create-room-shard-gate.md); the room's first actor \
                     was built on this replica, which owns its shard: {mine}"
                );
            }
            None => {}
        }
        Response::from_parts(parts, Body::from(bytes))
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

    /// Sends `req` to `shard`'s owner over the mesh and returns the owner's response, counted
    /// under `kind` (`client` or `federation`) in `hs_cluster_forward_latency_seconds`.
    async fn forward(
        &self,
        forwarder: &Forwarder,
        shard: ShardId,
        req: Request,
        kind: &'static str,
    ) -> Result<Response, String> {
        let started = std::time::Instant::now();
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

        let reply = forwarder.forward_as(kind, env).await.map_err(|e| {
            tracing::debug!(kind, %shard, method = %parts.method, path = parts.uri.path(), error = %e, "could not forward a request to the shard's owner");
            format!("forwarding to the shard owner: {e}")
        })?;
        tracing::debug!(
            kind,
            %shard,
            method = %parts.method,
            path = parts.uri.path(),
            status = reply.status,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "forwarded a request to the shard's owner"
        );

        // `reply.status` here is the *proxied* application's status (a `200` from a successful
        // `PUT /send/...`, a `403` from a rejected event, ...) except in the one case
        // `Forwarder::forward` itself gives up and returns the mesh-level `421`/`503` verbatim
        // (misdirected after every retry, or persistently unavailable) — those two statuses
        // collide with the ones `hs-room`'s own handlers could in principle return for
        // unrelated reasons; this workspace's client-server API does not use either today, so
        // the collision is only a latent risk, documented in `docs/status/03-cluster.md`.
        let proxied_response: ProxiedResponseBody = serde_json::from_slice(&reply.payload)
            .map_err(|e| match reply.status {
                // The forwarder's own give-up: the believed owner still refused after every
                // retry the deadline allowed, so the payload is that refusal, not a proxied
                // response.
                421 => "the believed owner answered that it does not own the shard (421) \
                        until the request's deadline; ownership did not settle in time"
                    .to_owned(),
                503 => "the owner was unavailable (503) until the request's deadline".to_owned(),
                _ => format!("decoding the forwarded response: {e}"),
            })?;
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
    /// Routes that are not an HTTP request to replay (see [`ShardRoutes`]).
    routes: Arc<ShardRoutes>,
}

#[async_trait::async_trait]
impl ShardHandler for ProxyShardHandler {
    async fn handle(&self, env: Envelope, fence: Fence) -> Reply {
        if let Some(handler) = self.routes.handler_for(&env.route) {
            return handler.handle(env, fence).await;
        }
        // `fence` is not consulted here: `hs-room`'s `RoomActor::persist` checks the room's
        // own fence inside every write it commits (`hs_room::fencing`), which is the
        // belt-and-braces behind routing every write through the single owner.
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
        let mut request = match builder.body(Body::from(body)) {
            Ok(r) => r,
            Err(e) => return bad_request(format!("bad proxied request: {e}")),
        };
        // Marks the request as having arrived over the mesh, which is what lets the gate on this
        // side honour a pre-assigned `/createRoom` id (see `RoomShardGate::run_create_room`).
        // Extensions cannot be set from a client socket, so this is not spoofable.
        request.extensions_mut().insert(ViaMesh);

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
    fn extracts_room_id_from_join_and_knock_by_id_but_not_by_alias() {
        // `POST /join/{roomIdOrAlias}` has no `rooms` segment; on a non-owner it used to reach
        // the handler and be refused by the write fence instead of forwarded (found by track 05).
        assert_eq!(
            extract_room_id("/_matrix/client/v3/join/!abc%3Aexample.org"),
            Some("!abc:example.org".to_owned())
        );
        assert_eq!(
            extract_room_id("/_matrix/client/v3/knock/!abc:example.org"),
            Some("!abc:example.org".to_owned())
        );
        // An alias resolves to a room id only inside the handler: not gated here.
        assert_eq!(
            extract_room_id("/_matrix/client/v3/join/%23alias%3Aexample.org"),
            None
        );
        assert_eq!(extract_room_id("/_matrix/client/v3/join/"), None);
        // `/rooms/{roomId}/join` was already covered by the `rooms` segment.
        assert_eq!(
            extract_room_id("/_matrix/client/v3/rooms/!abc%3Aexample.org/join"),
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
    fn extracts_room_id_from_federation_room_endpoints() {
        for path in [
            "/_matrix/federation/v1/make_join/!abc%3Aexample.org/%40bob%3Aremote?ver=11",
            "/_matrix/federation/v2/send_join/!abc:example.org/$event",
            "/_matrix/federation/v1/send_leave/!abc%3Aexample.org/$event",
            "/_matrix/federation/v1/make_knock/!abc%3Aexample.org/%40bob%3Aremote",
            "/_matrix/federation/v1/send_knock/!abc%3Aexample.org/$event",
            "/_matrix/federation/v2/invite/!abc%3Aexample.org/$event",
            "/_matrix/federation/v1/exchange_third_party_invite/!abc%3Aexample.org",
            "/_matrix/federation/v1/get_missing_events/!abc%3Aexample.org",
            "/_matrix/federation/v1/state_ids/!abc%3Aexample.org?event_id=$e",
            "/_matrix/federation/v1/backfill/!abc%3Aexample.org?v=$e&limit=10",
        ] {
            assert_eq!(
                extract_federation_room_id(path),
                Some("!abc:example.org".to_owned()),
                "{path}"
            );
        }
        for path in [
            // `/send` is handled per PDU (`crate::federation_forward`), not by the gate.
            "/_matrix/federation/v1/send/txn1",
            "/_matrix/federation/v1/event/$event",
            "/_matrix/federation/v1/query/directory",
            "/_matrix/federation/v1/user/devices/%40bob%3Aexample.org",
            "/_matrix/federation/v1/publicRooms",
            // A client path with the same word is not a federation request.
            "/_matrix/client/v3/rooms/!abc%3Aexample.org/state",
            "/_matrix/federation/v1/make_join/",
        ] {
            assert_eq!(extract_federation_room_id(path), None, "{path}");
        }
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
            metrics: None,
            origin: ReplicaId::new("solo:1"),
            origin_generation: Generation::fresh(None),
            default_deadline: Duration::from_secs(10),
            server_name: None,
            mesh: None,
            peer_routes: Arc::new(PeerRoutes::default()),
            shard_routes: Arc::new(ShardRoutes::default()),
        };
        let gate = RoomShardGate::new(&handles);
        assert!(
            gate.ownership
                .is_mine(handles.layout.room_shard("!any:room.example.org"))
        );
        assert!(gate.forwarder.is_none());
    }

    fn config_with_cluster(cluster_yaml: &str) -> hs_config::Config {
        hs_config::Config::from_yaml(&format!(
            "server:\n  server_name: example.org\n\
             listeners:\n  listeners:\n    - port: 8008\n      bind_addresses: [\"10.0.0.7\"]\n      resources: [client]\n\
             cluster:\n  single_node: false\n{cluster_yaml}"
        ))
        .expect("test config parses")
    }

    #[test]
    fn advertise_addr_prefers_the_configured_address_over_the_bind_address() {
        let bare_host = config_with_cluster(
            "  mesh:\n    port: 8449\n    advertise_address: hs-0.hs-headless.matrix.svc.cluster.local\n",
        );
        assert_eq!(
            advertise_addr(&bare_host),
            "hs-0.hs-headless.matrix.svc.cluster.local:8449",
            "a bare host gets the mesh port appended"
        );

        let with_port =
            config_with_cluster("  mesh:\n    port: 8449\n    advertise_address: 10.1.2.3:9000\n");
        assert_eq!(advertise_addr(&with_port), "10.1.2.3:9000");

        let bracketed_v6 =
            config_with_cluster("  mesh:\n    port: 8449\n    advertise_address: \"[fd00::1]\"\n");
        assert_eq!(advertise_addr(&bracketed_v6), "[fd00::1]:8449");

        let unset = config_with_cluster("  mesh:\n    port: 8449\n");
        assert_eq!(
            advertise_addr(&unset),
            "10.0.0.7:8449",
            "unset falls back to the first listener's bind address"
        );

        let wildcard = {
            let mut cfg = unset.clone();
            cfg.listeners.listeners[0].bind_addresses = vec!["0.0.0.0".to_owned()];
            cfg
        };
        assert_eq!(advertise_addr(&wildcard), "127.0.0.1:8449");
    }

    #[test]
    fn a_live_row_under_this_identity_refuses_startup_but_a_stale_one_does_not() {
        use hs_cluster::types::{ReplicaRecord, ReplicaState};
        let backend = hs_kv::memory::MemoryBackend::new();
        let store = hs_cluster::store::ClusterStore::open(backend).unwrap();
        let me = ReplicaId::new("hs-0.hs-headless.matrix.svc.cluster.local:8449");
        let lease = Duration::from_secs(10);
        let now = 1_000_000_u64;

        // Empty registry: fine.
        refuse_live_duplicate(&store, &me, lease, now).expect("no rows");

        let row = |heartbeat_unix_ms: u64| ReplicaRecord {
            id: me.clone(),
            generation: Generation(7),
            mesh_addr: me.as_str().to_owned(),
            zone: None,
            version: "test".into(),
            state: ReplicaState::Active,
            heartbeat_seq: 1,
            heartbeat_unix_ms,
        };
        // A row heartbeated a moment ago under the same identity: another process is live.
        store.heartbeat(&row(now - 500)).unwrap();
        let err = refuse_live_duplicate(&store, &me, lease, now).expect_err("live duplicate");
        assert!(
            err.to_string()
                .contains("HS__CLUSTER__MESH__ADVERTISE_ADDRESS"),
            "{err}"
        );
        // The same row once it is older than the lease: a restart of this replica, allowed.
        refuse_live_duplicate(&store, &me, lease, now + 20_000).expect("stale row");
        // A live row under a different identity is somebody else, allowed.
        let other = ReplicaId::new("hs-1.hs-headless.matrix.svc.cluster.local:8449");
        refuse_live_duplicate(&store, &other, lease, now).expect("other identity");
    }

    #[test]
    fn split_host_port_tells_a_port_from_an_ipv6_colon() {
        assert_eq!(split_host_port("host:8449"), Some(("host", 8449)));
        assert_eq!(split_host_port("[fd00::1]:8449"), Some(("[fd00::1]", 8449)));
        assert_eq!(split_host_port("fd00::1"), None);
        assert_eq!(split_host_port("host"), None);
        assert_eq!(split_host_port("host:notaport"), None);
    }

    #[test]
    fn to_hs_cluster_config_carries_the_tls_paths_and_the_identity() {
        let cfg = config_with_cluster(
            "  mesh:\n    port: 8449\n    advertise_address: hs-1.mesh.test\n    tls:\n\
             \x20     certificate_path: /m/tls.crt\n      private_key_path: /m/tls.key\n\
             \x20     ca_certificate_path: /m/ca.crt\n      peer_san_suffix: .mesh.test\n",
        );
        let converted = to_hs_cluster_config(&cfg, ShardLayout::default());
        assert_eq!(converted.me.as_str(), "hs-1.mesh.test:8449");
        assert_eq!(converted.mesh_advertise_addr, "hs-1.mesh.test:8449");
        assert_eq!(converted.mesh.listen_addr, "0.0.0.0:8449");
        match converted.mesh.auth {
            AuthMode::MutualTls {
                ca_file,
                cert_file,
                key_file,
                peer_san_suffix,
            } => {
                assert_eq!(ca_file, std::path::PathBuf::from("/m/ca.crt"));
                assert_eq!(cert_file, std::path::PathBuf::from("/m/tls.crt"));
                assert_eq!(key_file, std::path::PathBuf::from("/m/tls.key"));
                assert_eq!(peer_san_suffix.as_deref(), Some(".mesh.test"));
            }
            AuthMode::SharedSecret { .. } => panic!("tls in the config must select mutual TLS"),
        }

        let plain = config_with_cluster("  mesh:\n    shared_secret: s3\n");
        match to_hs_cluster_config(&plain, ShardLayout::default())
            .mesh
            .auth
        {
            AuthMode::SharedSecret { secret } => assert_eq!(secret, "s3"),
            AuthMode::MutualTls { .. } => panic!("no tls in the config must select the secret"),
        }
    }

    #[test]
    fn is_create_room_matches_only_the_create_route() {
        assert!(is_create_room(
            &http::Method::POST,
            "/_matrix/client/v3/createRoom"
        ));
        assert!(is_create_room(
            &http::Method::POST,
            "/_matrix/client/r0/createRoom"
        ));
        assert!(!is_create_room(
            &http::Method::GET,
            "/_matrix/client/v3/createRoom"
        ));
        assert!(!is_create_room(
            &http::Method::POST,
            "/_matrix/client/v3/rooms/!a:b/send/m.room.message/1"
        ));
    }

    // ---- the /createRoom gate, with scripted ownership ----

    /// An ownership whose answers a test scripts: every shard is mine, or none is and `owner`
    /// has them all.
    struct Scripted {
        me: ReplicaId,
        mine: bool,
        owner: Option<ReplicaId>,
    }

    impl Ownership for Scripted {
        fn me(&self) -> &ReplicaId {
            &self.me
        }
        fn owner_of(&self, _shard: ShardId) -> Option<ReplicaId> {
            if self.mine {
                Some(self.me.clone())
            } else {
                self.owner.clone()
            }
        }
        fn is_mine(&self, _shard: ShardId) -> bool {
            self.mine
        }
        fn fence(&self, shard: ShardId) -> Option<Fence> {
            self.mine.then(|| Fence::inert(shard))
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<hs_cluster::OwnershipEvent> {
            tokio::sync::broadcast::channel(1).1
        }
        fn shard_map(&self) -> watch::Receiver<Arc<hs_cluster::ShardMap>> {
            watch::channel(Arc::new(hs_cluster::ShardMap::default())).1
        }
    }

    fn scripted(me: &str, mine: bool, owner: Option<&str>) -> Arc<Scripted> {
        Arc::new(Scripted {
            me: ReplicaId::new(me),
            mine,
            owner: owner.map(ReplicaId::new),
        })
    }

    const TEST_SECRET: &str = "gate-test-secret";

    fn gate(ownership: Arc<dyn Ownership>, clustered: bool) -> Arc<RoomShardGate> {
        let forwarder = clustered.then(|| {
            Arc::new(
                Forwarder::new(
                    AuthMode::SharedSecret {
                        secret: TEST_SECRET.into(),
                    },
                    None,
                    3,
                    2,
                    Duration::from_millis(5),
                    ownership.clone(),
                    Arc::new(hs_cluster::metrics::ClusterMetrics::new()),
                )
                .expect("forwarder"),
            )
        });
        Arc::new(RoomShardGate {
            ownership: ownership.clone(),
            layout: ShardLayout::default(),
            forwarder,
            origin: ownership.me().clone(),
            origin_generation: Generation::fresh(None),
            default_deadline: Duration::from_secs(5),
            server_name: clustered.then(|| {
                ruma::ServerName::parse("example.org")
                    .expect("valid")
                    .to_owned()
            }),
            alias_resolver: None,
        })
    }

    /// What one replica's stand-in `/createRoom` handler saw, per call.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    struct Seen {
        room_id: String,
        preassigned: bool,
        via_mesh: bool,
        header_present: bool,
    }

    /// A router whose `/createRoom` records what reached it and answers the way `hs-room` would
    /// once it honours [`PreassignedRoomId`]: the pre-assigned id if there is one, its own
    /// otherwise.
    fn create_room_router(seen: Arc<std::sync::Mutex<Vec<Seen>>>) -> axum::Router {
        axum::Router::new().route(
            "/_matrix/client/v3/createRoom",
            axum::routing::post(move |req: Request| {
                let seen = seen.clone();
                async move {
                    let pre = req.extensions().get::<PreassignedRoomId>().cloned();
                    let record = Seen {
                        room_id: pre
                            .as_ref()
                            .map(|p| p.0.clone())
                            .unwrap_or_else(|| "!minted-by-handler:example.org".to_owned()),
                        preassigned: pre.is_some(),
                        via_mesh: req.extensions().get::<ViaMesh>().is_some(),
                        header_present: req.headers().contains_key(PREASSIGNED_ROOM_ID_HEADER),
                    };
                    seen.lock().unwrap().push(record.clone());
                    axum::Json(serde_json::json!({ "room_id": record.room_id }))
                }
            }),
        )
    }

    fn create_room_request(client_header: Option<&str>) -> Request {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/_matrix/client/v3/createRoom")
            .header("content-type", "application/json");
        if let Some(value) = client_header {
            builder = builder.header(PREASSIGNED_ROOM_ID_HEADER, value);
        }
        builder
            .body(Body::from(r#"{"preset":"public_chat"}"#))
            .unwrap()
    }

    async fn room_id_of(response: Response) -> (StatusCode, String) {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes) }));
        (
            status,
            json.get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
        )
    }

    #[tokio::test]
    async fn on_the_owner_create_room_runs_locally_under_a_preassigned_id() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = gate(scripted("a:1", true, None), true).layer(create_room_router(seen.clone()));

        // A client trying to pick its own room id through the mesh-only header is ignored.
        let response = app
            .oneshot(create_room_request(Some("!chosen-by-client:example.org")))
            .await
            .unwrap();
        let (status, room_id) = room_id_of(response).await;
        assert_eq!(status, StatusCode::OK);
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].preassigned,
            "the gate pre-assigned an id: {calls:?}"
        );
        assert!(!calls[0].header_present, "the client's header was stripped");
        assert!(!calls[0].via_mesh);
        assert_eq!(room_id, calls[0].room_id);
        assert_ne!(room_id, "!chosen-by-client:example.org");
        assert!(
            room_id.starts_with('!') && room_id.ends_with(":example.org"),
            "{room_id}"
        );
    }

    #[tokio::test]
    async fn in_single_node_mode_create_room_passes_through_untouched() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app =
            gate(scripted("solo:1", true, None), false).layer(create_room_router(seen.clone()));
        let response = app
            .oneshot(create_room_request(Some("!chosen-by-client:example.org")))
            .await
            .unwrap();
        let (status, room_id) = room_id_of(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(room_id, "!minted-by-handler:example.org");
        let calls = seen.lock().unwrap().clone();
        assert!(!calls[0].preassigned);
        assert!(!calls[0].header_present, "the header is stripped even here");
    }

    #[tokio::test]
    async fn a_forwarded_create_room_for_a_shard_not_owned_here_is_refused() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app =
            gate(scripted("b:1", false, Some("c:1")), true).layer(create_room_router(seen.clone()));
        let mut req = create_room_request(Some("!forwarded:example.org"));
        req.extensions_mut().insert(ViaMesh);
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains("M_HS_NOT_SHARD_OWNER"),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(seen.lock().unwrap().is_empty(), "the handler must not run");
    }

    /// A port nobody is listening on right now; `MeshServer` binds only inside `serve`.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn on_a_non_owner_create_room_is_forwarded_to_the_owner_which_creates_it() {
        // Replica B owns every shard and runs a mesh listener whose handler replays forwards
        // against B's own gated router, exactly as `spawn_mesh` wires it in production.
        let b_addr = format!("127.0.0.1:{}", free_port());
        let seen_b = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ownership_b: Arc<dyn Ownership> = scripted(&b_addr, true, None);
        let app_b = gate(ownership_b.clone(), true).layer(create_room_router(seen_b.clone()));
        let deps = Arc::new(MeshDeps {
            authenticator: Arc::new(SharedSecretAuthenticator::new(TEST_SECRET)),
            ownership: ownership_b,
            handler: Arc::new(ProxyShardHandler {
                app: app_b,
                routes: Arc::default(),
            }),
            idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(5), 16)),
            in_flight: Arc::new(tokio::sync::Semaphore::new(8)),
            nudge: None,
            peers: None,
        });
        let server = MeshServer::new(b_addr.clone(), None).unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            server.serve(deps, shutdown_rx).await.unwrap();
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&b_addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Replica A owns nothing and believes B owns everything.
        let seen_a = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app_a = gate(scripted("127.0.0.1:1", false, Some(&b_addr)), true)
            .layer(create_room_router(seen_a.clone()));

        let response = app_a
            .oneshot(create_room_request(Some("!chosen-by-client:example.org")))
            .await
            .unwrap();
        let (status, room_id) = room_id_of(response).await;
        assert_eq!(status, StatusCode::OK, "{room_id}");

        assert!(
            seen_a.lock().unwrap().is_empty(),
            "the non-owner must never run the handler"
        );
        let calls_b = seen_b.lock().unwrap().clone();
        assert_eq!(calls_b.len(), 1, "{calls_b:?}");
        assert!(
            calls_b[0].via_mesh,
            "B saw the request arrive over the mesh"
        );
        assert!(
            calls_b[0].preassigned,
            "B's gate turned the header into the extension"
        );
        assert!(
            !calls_b[0].header_present,
            "the header does not reach the handler"
        );
        assert_eq!(
            room_id, calls_b[0].room_id,
            "the client got the id B created under"
        );
        assert_ne!(room_id, "!chosen-by-client:example.org");
        assert!(room_id.ends_with(":example.org"), "{room_id}");
    }

    // ---- a join or knock by alias ----

    /// Resolves exactly one alias, and counts how often it was asked.
    struct OneAlias {
        alias: &'static str,
        resolved: ResolvedAlias,
        asked: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AliasResolver for OneAlias {
        async fn resolve(&self, alias: &str) -> Option<ResolvedAlias> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (alias == self.alias).then(|| self.resolved.clone())
        }
    }

    fn one_alias(via: &[&str]) -> Arc<OneAlias> {
        Arc::new(OneAlias {
            alias: "#lobby:example.org",
            resolved: ResolvedAlias {
                room_id: "!lobby:example.org".to_owned(),
                via: via.iter().map(|s| (*s).to_owned()).collect(),
            },
            asked: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn with_resolver(gate: Arc<RoomShardGate>, resolver: Arc<OneAlias>) -> Arc<RoomShardGate> {
        let mut gate = Arc::into_inner(gate).expect("a fresh gate has one owner");
        gate.alias_resolver = Some(resolver);
        Arc::new(gate)
    }

    /// What a stand-in `/join/{roomIdOrAlias}` or `/knock/...` handler saw: the path and query
    /// it was called with, and whether the request came over the mesh.
    fn join_router(seen: Arc<std::sync::Mutex<Vec<(String, bool)>>>) -> axum::Router {
        let handler = move |req: Request| {
            let seen = seen.clone();
            async move {
                let uri = req
                    .uri()
                    .path_and_query()
                    .map(|pq| pq.as_str().to_owned())
                    .unwrap_or_default();
                seen.lock()
                    .unwrap()
                    .push((uri, req.extensions().get::<ViaMesh>().is_some()));
                axum::Json(serde_json::json!({ "room_id": "!lobby:example.org" }))
            }
        };
        axum::Router::new()
            .route(
                "/_matrix/client/v3/join/{target}",
                axum::routing::post(handler.clone()),
            )
            .route(
                "/_matrix/client/v3/knock/{target}",
                axum::routing::post(handler),
            )
    }

    fn post(uri: &str) -> Request {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap()
    }

    #[test]
    fn extracts_an_alias_from_join_and_knock_only() {
        assert_eq!(
            extract_alias("/_matrix/client/v3/join/%23lobby%3Aexample.org").as_deref(),
            Some("#lobby:example.org")
        );
        assert_eq!(
            extract_alias("/_matrix/client/r0/knock/#lobby:example.org").as_deref(),
            Some("#lobby:example.org")
        );
        assert_eq!(
            extract_alias("/_matrix/client/v3/join/!abc:example.org"),
            None
        );
        assert_eq!(extract_alias("/_matrix/client/v3/rooms/!abc:x/join"), None);
        assert_eq!(extract_alias("/_matrix/client/v3/sync"), None);
    }

    #[test]
    fn the_rewritten_uri_names_the_room_and_appends_the_servers_after_the_clients() {
        assert_eq!(
            alias_rewritten_uri(
                "/_matrix/client/v3/join/%23lobby%3Aexample.org",
                Some("server_name=mine.example"),
                "!lobby:example.org",
                &["remote.example".to_owned(), "[::1]:8448".to_owned()],
            ),
            "/_matrix/client/v3/join/%21lobby%3Aexample.org?server_name=mine.example&server_name=remote.example&server_name=%5B%3A%3A1%5D%3A8448"
        );
        assert_eq!(
            alias_rewritten_uri("/_matrix/client/v3/knock/%23a%3Ab", None, "!x:b", &[]),
            "/_matrix/client/v3/knock/%21x%3Ab"
        );
        assert_eq!(
            extract_room_id("/_matrix/client/v3/join/%21lobby%3Aexample.org").as_deref(),
            Some("!lobby:example.org")
        );
    }

    #[tokio::test]
    async fn on_a_non_owner_a_join_by_alias_is_forwarded_to_the_owner_by_room_id() {
        // B owns every shard and serves the mesh, as in the `/createRoom` test above.
        let b_addr = format!("127.0.0.1:{}", free_port());
        let seen_b = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ownership_b: Arc<dyn Ownership> = scripted(&b_addr, true, None);
        let app_b = gate(ownership_b.clone(), true).layer(join_router(seen_b.clone()));
        let deps = Arc::new(MeshDeps {
            authenticator: Arc::new(SharedSecretAuthenticator::new(TEST_SECRET)),
            ownership: ownership_b,
            handler: Arc::new(ProxyShardHandler {
                app: app_b,
                routes: Arc::default(),
            }),
            idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(5), 16)),
            in_flight: Arc::new(tokio::sync::Semaphore::new(8)),
            nudge: None,
            peers: None,
        });
        let server = MeshServer::new(b_addr.clone(), None).unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            server.serve(deps, shutdown_rx).await.unwrap();
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&b_addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // A owns nothing and resolves the alias itself.
        let seen_a = Arc::new(std::sync::Mutex::new(Vec::new()));
        let resolver = one_alias(&["remote.example"]);
        let app_a = with_resolver(
            gate(scripted("127.0.0.1:1", false, Some(&b_addr)), true),
            resolver.clone(),
        )
        .layer(join_router(seen_a.clone()));

        for verb in ["join", "knock"] {
            let response = app_a
                .clone()
                .oneshot(post(&format!(
                    "/_matrix/client/v3/{verb}/%23lobby%3Aexample.org?server_name=mine.example"
                )))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{verb}");
        }
        assert!(
            seen_a.lock().unwrap().is_empty(),
            "the non-owner must never run the handler"
        );
        let calls_b = seen_b.lock().unwrap().clone();
        assert_eq!(
            calls_b,
            ["join", "knock"]
                .map(|verb| (
                    format!(
                        "/_matrix/client/v3/{verb}/%21lobby%3Aexample.org?server_name=mine.example&server_name=remote.example"
                    ),
                    true
                ))
                .to_vec(),
            "the owner was handed the room id and the servers, over the mesh"
        );
        assert_eq!(resolver.asked.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A federation router standing in for the real one: `send_join` and `make_join` record
    /// the path, the `Authorization` header and the body they were given; `/send` records that
    /// it ran.
    fn federation_router(
        seen: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    ) -> axum::Router {
        let record = move |req: Request| {
            let seen = seen.clone();
            async move {
                let (parts, body) = req.into_parts();
                let body = to_bytes(body, 1 << 20).await.unwrap();
                seen.lock().unwrap().push((
                    parts
                        .uri
                        .path_and_query()
                        .map(|pq| pq.as_str().to_owned())
                        .unwrap_or_default(),
                    parts
                        .headers
                        .get(http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned(),
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                axum::Json(serde_json::json!({ "ok": true }))
            }
        };
        axum::Router::new()
            .route(
                "/_matrix/federation/v2/send_join/{roomId}/{eventId}",
                axum::routing::put(record.clone()),
            )
            .route(
                "/_matrix/federation/v1/make_join/{roomId}/{userId}",
                axum::routing::get(record.clone()),
            )
            .route(
                "/_matrix/federation/v1/send/{txnId}",
                axum::routing::put(record),
            )
    }

    /// A federation request for a room another replica owns is sent to it whole -- path, query,
    /// signature and body unchanged, so the owner's `X-Matrix` layer verifies it again -- and
    /// never runs on the replica it reached. Before, `send_join` ran there and its write was
    /// refused by the room's fence (`501 M_HS_INBOUND_INGESTION_UNSUPPORTED`, "fenced: ...").
    /// `/send` names no room and still runs where it lands.
    #[tokio::test]
    async fn on_a_non_owner_a_federation_request_for_a_room_is_forwarded_to_the_owner() {
        let b_addr = format!("127.0.0.1:{}", free_port());
        let seen_b = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ownership_b: Arc<dyn Ownership> = scripted(&b_addr, true, None);
        let app_b = gate(ownership_b.clone(), true).layer(federation_router(seen_b.clone()));
        let deps = Arc::new(MeshDeps {
            authenticator: Arc::new(SharedSecretAuthenticator::new(TEST_SECRET)),
            ownership: ownership_b,
            handler: Arc::new(ProxyShardHandler {
                app: app_b,
                routes: Arc::default(),
            }),
            idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(5), 16)),
            in_flight: Arc::new(tokio::sync::Semaphore::new(8)),
            nudge: None,
            peers: None,
        });
        let server = MeshServer::new(b_addr.clone(), None).unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            server.serve(deps, shutdown_rx).await.unwrap();
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&b_addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let seen_a = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app_a = gate(scripted("127.0.0.1:1", false, Some(&b_addr)), true)
            .layer(federation_router(seen_a.clone()));
        let signature = "X-Matrix origin=\"remote.example\",destination=\"example.org\",key=\"ed25519:k\",sig=\"abc\"";
        let send_join = Request::builder()
            .method("PUT")
            .uri("/_matrix/federation/v2/send_join/%21abc%3Aexample.org/%24join?omit_members=true")
            .header(http::header::AUTHORIZATION, signature)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"type":"m.room.member"}"#))
            .unwrap();
        let response = app_a.clone().oneshot(send_join).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let make_join = Request::builder()
            .method("GET")
            .uri("/_matrix/federation/v1/make_join/%21abc%3Aexample.org/%40bob%3Aremote.example?ver=11")
            .header(http::header::AUTHORIZATION, signature)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app_a.clone().oneshot(make_join).await.unwrap().status(),
            StatusCode::OK
        );
        let send = Request::builder()
            .method("PUT")
            .uri("/_matrix/federation/v1/send/txn1")
            .header(http::header::AUTHORIZATION, signature)
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(app_a.oneshot(send).await.unwrap().status(), StatusCode::OK);

        assert_eq!(
            seen_b.lock().unwrap().clone(),
            vec![
                (
                    "/_matrix/federation/v2/send_join/%21abc%3Aexample.org/%24join?omit_members=true"
                        .to_owned(),
                    signature.to_owned(),
                    r#"{"type":"m.room.member"}"#.to_owned()
                ),
                (
                    "/_matrix/federation/v1/make_join/%21abc%3Aexample.org/%40bob%3Aremote.example?ver=11"
                        .to_owned(),
                    signature.to_owned(),
                    String::new()
                ),
            ],
            "the owner got both requests exactly as they were signed"
        );
        assert_eq!(
            seen_a
                .lock()
                .unwrap()
                .iter()
                .map(|(path, ..)| path.clone())
                .collect::<Vec<_>>(),
            vec!["/_matrix/federation/v1/send/txn1".to_owned()],
            "only `/send` ran on the replica it reached"
        );
    }

    /// A forward on a route a shard handler was added for goes to that handler, not to the
    /// router.
    #[tokio::test]
    async fn a_forward_on_an_added_route_goes_to_its_handler() {
        struct Answer;
        #[async_trait::async_trait]
        impl ShardHandler for Answer {
            async fn handle(&self, env: Envelope, _fence: Fence) -> Reply {
                Reply::ok(Bytes::from(format!("handled {}", env.route)))
            }
        }
        let routes = Arc::new(ShardRoutes::default());
        routes.add("federation.sink", Arc::new(Answer));
        let handler = ProxyShardHandler {
            app: axum::Router::new(),
            routes,
        };
        let shard = ShardId::new(hs_cluster::ShardKind::Room, 0);
        let env = |route: &str| Envelope {
            shard,
            route: route.to_owned(),
            idempotency_key: IdempotencyKey::generate(),
            requester: serde_json::Value::Null,
            deadline: Duration::from_secs(1),
            origin: ReplicaId::new("a"),
            origin_generation: Generation::fresh(None),
            hops: 1,
            traceparent: None,
            payload: Bytes::from_static(b"not a proxied request"),
        };
        let reply = handler
            .handle(env("federation.sink"), Fence::inert(shard))
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(&reply.payload[..], b"handled federation.sink");
        // Anything else is a proxied HTTP request, and this one is not one.
        let reply = handler.handle(env("http.proxy"), Fence::inert(shard)).await;
        assert_eq!(reply.status, 400);
    }

    #[tokio::test]
    async fn on_the_owner_a_join_by_alias_runs_here_and_an_unknown_alias_passes_through() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = with_resolver(gate(scripted("a", true, None), true), one_alias(&[]))
            .layer(join_router(seen.clone()));
        for uri in [
            "/_matrix/client/v3/join/%23lobby%3Aexample.org",
            "/_matrix/client/v3/join/%23nowhere%3Aexample.org",
        ] {
            let response = app.clone().oneshot(post(uri)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                (
                    "/_matrix/client/v3/join/%21lobby%3Aexample.org".to_owned(),
                    false
                ),
                (
                    "/_matrix/client/v3/join/%23nowhere%3Aexample.org".to_owned(),
                    false
                ),
            ]
        );
    }

    #[tokio::test]
    async fn in_single_node_mode_no_alias_is_resolved() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let resolver = one_alias(&[]);
        let app = with_resolver(gate(scripted("a", true, None), false), resolver.clone())
            .layer(join_router(seen.clone()));
        let response = app
            .oneshot(post("/_matrix/client/v3/join/%23lobby%3Aexample.org"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(resolver.asked.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            seen.lock().unwrap()[0].0,
            "/_matrix/client/v3/join/%23lobby%3Aexample.org"
        );
    }

    // ---- a shard moving away while a request for it runs ----

    /// An ownership that owns every shard until [`Flipping::give_away`], then believes `owner`
    /// has them all: a handoff landing mid-request.
    struct Flipping {
        me: ReplicaId,
        mine: std::sync::atomic::AtomicBool,
        owner: ReplicaId,
    }

    impl Flipping {
        fn give_away(&self) {
            self.mine.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Ownership for Flipping {
        fn me(&self) -> &ReplicaId {
            &self.me
        }
        fn owner_of(&self, _shard: ShardId) -> Option<ReplicaId> {
            Some(if self.is_mine(_shard) {
                self.me.clone()
            } else {
                self.owner.clone()
            })
        }
        fn is_mine(&self, _shard: ShardId) -> bool {
            self.mine.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn fence(&self, shard: ShardId) -> Option<Fence> {
            self.is_mine(shard).then(|| Fence::inert(shard))
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<hs_cluster::OwnershipEvent> {
            tokio::sync::broadcast::channel(1).1
        }
        fn shard_map(&self) -> watch::Receiver<Arc<hs_cluster::ShardMap>> {
            watch::channel(Arc::new(hs_cluster::ShardMap::default())).1
        }
    }

    const SEND_PATH: &str = "/_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}";

    fn send_request(body: &str) -> Request {
        Request::builder()
            .method("PUT")
            .uri("/_matrix/client/v3/rooms/%21moving%3Aexample.org/send/m.room.message/t1")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    /// Starts replica B: owns every shard, serves the mesh, and answers a send with the body it
    /// received. Returns B's mesh address, the bodies B's handler saw, and the sender that keeps
    /// B's listener up while it is held.
    async fn spawn_owner_b() -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        watch::Sender<bool>,
    ) {
        let b_addr = format!("127.0.0.1:{}", free_port());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ownership_b: Arc<dyn Ownership> = scripted(&b_addr, true, None);
        let recorded = seen.clone();
        let router_b = axum::Router::new().route(
            SEND_PATH,
            axum::routing::put(move |body: String| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(body);
                    axum::Json(serde_json::json!({ "event_id": "$sent-by-b" }))
                }
            }),
        );
        let app_b = gate(ownership_b.clone(), true).layer(router_b);
        let deps = Arc::new(MeshDeps {
            authenticator: Arc::new(SharedSecretAuthenticator::new(TEST_SECRET)),
            ownership: ownership_b,
            handler: Arc::new(ProxyShardHandler {
                app: app_b,
                routes: Arc::default(),
            }),
            idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(5), 16)),
            in_flight: Arc::new(tokio::sync::Semaphore::new(8)),
            nudge: None,
            peers: None,
        });
        let server = MeshServer::new(b_addr.clone(), None).unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            server.serve(deps, shutdown_rx).await.unwrap();
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&b_addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (b_addr, seen, shutdown_tx)
    }

    #[tokio::test]
    async fn a_request_fenced_by_a_handoff_mid_flight_is_sent_on_to_the_new_owner() {
        let (b_addr, seen_b, _keep_b_up) = spawn_owner_b().await;
        let ownership_a = Arc::new(Flipping {
            me: ReplicaId::new("127.0.0.1:1"),
            mine: std::sync::atomic::AtomicBool::new(true),
            owner: ReplicaId::new(b_addr),
        });
        // A's handler is where the handoff lands: the shard moves to B while the send runs, and
        // `hs-room`'s fence refuses the write with a 503, as `RoomError::Fenced` does.
        let ran_on_a = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (flip, count) = (ownership_a.clone(), ran_on_a.clone());
        let router_a = axum::Router::new().route(
            SEND_PATH,
            axum::routing::put(move || {
                let (flip, count) = (flip.clone(), count.clone());
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    flip.give_away();
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({
                            "errcode": "M_UNKNOWN",
                            "error": "fenced: this replica no longer owns shard"
                        })),
                    )
                }
            }),
        );
        let app_a = gate(ownership_a, true).layer(router_a);

        let response = app_a
            .oneshot(send_request(r#"{"msgtype":"m.text","body":"hi"}"#))
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(String::from_utf8_lossy(&bytes).contains("$sent-by-b"));
        assert_eq!(ran_on_a.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            seen_b.lock().unwrap().clone(),
            vec![r#"{"msgtype":"m.text","body":"hi"}"#.to_owned()],
            "B got the same body A was sent"
        );
    }

    #[tokio::test]
    async fn a_503_from_a_shard_still_owned_here_is_passed_back_as_it_is() {
        let ownership_a: Arc<dyn Ownership> = scripted("127.0.0.1:1", true, None);
        let router_a = axum::Router::new().route(
            SEND_PATH,
            axum::routing::put(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(serde_json::json!({ "errcode": "M_LIMIT_EXCEEDED" })),
                )
            }),
        );
        let app_a = gate(ownership_a, true).layer(router_a);
        let response = app_a.oneshot(send_request("{}")).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("M_LIMIT_EXCEEDED"));
    }

    /// A `/createRoom` handler that answers as `hs-room` does when its fence refuses the
    /// creation, and counts its calls; `flip`, if given, gives the shard away first.
    fn fenced_create_room_router(
        calls: Arc<std::sync::atomic::AtomicUsize>,
        flip: Option<Arc<Flipping>>,
    ) -> axum::Router {
        axum::Router::new().route(
            "/_matrix/client/v3/createRoom",
            axum::routing::post(move || {
                let (calls, flip) = (calls.clone(), flip.clone());
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(flip) = flip {
                        flip.give_away();
                    }
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({
                            "errcode": "M_UNKNOWN",
                            "error": "fenced: this replica no longer owns shard ShardId(room/1)"
                        })),
                    )
                }
            }),
        )
    }

    /// Ownership of the room's shard moves away while `/createRoom` runs here, and the fence
    /// refuses the creation: the gate makes it again against the current ownership, which
    /// forwards it to the new owner, and the client gets the room. Before, the client got the
    /// fence's `503 M_UNKNOWN` (seen in `tests/cluster_create_room.rs` under load).
    #[tokio::test]
    async fn a_create_room_fenced_here_is_made_again_on_the_new_owner() {
        let b_addr = format!("127.0.0.1:{}", free_port());
        let seen_b = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ownership_b: Arc<dyn Ownership> = scripted(&b_addr, true, None);
        let app_b = gate(ownership_b.clone(), true).layer(create_room_router(seen_b.clone()));
        let deps = Arc::new(MeshDeps {
            authenticator: Arc::new(SharedSecretAuthenticator::new(TEST_SECRET)),
            ownership: ownership_b,
            handler: Arc::new(ProxyShardHandler {
                app: app_b,
                routes: Arc::default(),
            }),
            idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(5), 16)),
            in_flight: Arc::new(tokio::sync::Semaphore::new(8)),
            nudge: None,
            peers: None,
        });
        let server = MeshServer::new(b_addr.clone(), None).unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            server.serve(deps, shutdown_rx).await.unwrap();
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&b_addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let ownership_a = Arc::new(Flipping {
            me: ReplicaId::new("127.0.0.1:1"),
            mine: std::sync::atomic::AtomicBool::new(true),
            owner: ReplicaId::new(b_addr),
        });
        let calls_a = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app_a = gate(ownership_a.clone(), true).layer(fenced_create_room_router(
            calls_a.clone(),
            Some(ownership_a),
        ));
        let response = app_a.oneshot(create_room_request(None)).await.unwrap();
        let (status, room_id) = room_id_of(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(calls_a.load(std::sync::atomic::Ordering::SeqCst), 1);
        let calls_b = seen_b.lock().unwrap().clone();
        assert_eq!(calls_b.len(), 1, "{calls_b:?}");
        assert!(calls_b[0].via_mesh && calls_b[0].preassigned);
        assert_eq!(room_id, calls_b[0].room_id);
    }

    /// A creation fenced on every attempt (ownership never settles) is refused as the gate
    /// refuses any request it cannot place, `503 M_HS_NOT_SHARD_OWNER`, which a client retries,
    /// after a bounded number of attempts -- not answered with the fence's `M_UNKNOWN`.
    #[tokio::test]
    async fn a_create_room_fenced_on_every_attempt_is_refused_as_not_the_shard_owner() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = gate(scripted("127.0.0.1:1", true, None), true)
            .layer(fenced_create_room_router(calls.clone(), None));
        let response = app.oneshot(create_room_request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("M_HS_NOT_SHARD_OWNER"), "{text}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_CREATE_ROOM_ATTEMPTS as usize
        );
    }
}
