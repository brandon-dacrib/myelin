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
        metrics,
    )?);

    Ok(ClusterHandles {
        cluster,
        layout,
        forwarder: Some(forwarder),
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
            server_name: None,
            mesh: None,
            peer_routes: Arc::new(PeerRoutes::default()),
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
/// of `/join` and `/knock`, whose room id is only known once the handler has resolved the alias
/// (see `docs/status/03-cluster.md`, 2026-09-27, for that gap). `/createRoom` has no room id in
/// its path either and is gated separately, by [`is_create_room`] and
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
        if is_create_room(req.method(), req.uri().path()) {
            return self.run_create_room(req, next).await;
        }
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

        let room_id = ruma::RoomId::new_v1(server_name).to_string();
        let shard = self.layout.room_shard(&room_id);
        if self.ownership.is_mine(shard) {
            req.extensions_mut()
                .insert(PreassignedRoomId(room_id.clone()));
            let response = next.run(req).await;
            return self.check_created_room(response, &room_id).await;
        }

        let Ok(value) = http::HeaderValue::from_str(&room_id) else {
            // A freshly minted `!localpart:server` is always a valid header value; this arm is
            // unreachable in practice and kept only so the gate cannot panic.
            return self.refuse(shard, "the minted room id is not a valid header value");
        };
        req.headers_mut().insert(PREASSIGNED_ROOM_ID_HEADER, value);
        tracing::debug!(%room_id, %shard, "forwarding /createRoom to the shard's owner");
        match self.forward(forwarder, shard, req).await {
            Ok(response) => response,
            Err(reason) => self.refuse(shard, &reason),
        }
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
            server_name: None,
            mesh: None,
            peer_routes: Arc::new(PeerRoutes::default()),
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
            handler: Arc::new(ProxyShardHandler { app: app_b }),
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
}
