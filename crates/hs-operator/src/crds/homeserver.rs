//! The `Homeserver` custom resource: one `hs` replica set (single-node or clustered), the
//! operator's primary kind. Mirrors `deploy/helm/hs/values.yaml`'s shape closely — the chart and
//! this CRD are two ways to describe the same workload, and [`crate::homeserver`] builds the same
//! objects the chart renders (a test renders the chart and compares). A deployment uses one or
//! the other: the operator does not manage resources the chart also owns, and vice versa, to
//! avoid two controllers fighting over the same StatefulSet.

use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{ImageSpec, Phase, SecretKeyRef};

/// Which storage backend a `Homeserver` uses. See `hs_config::storage::StorageConfig` (owned by
/// track 13) for the native config counterpart this maps onto.
///
/// Modeled as `backend` plus one `Option<...>` block per backend — not a Rust enum with
/// `#[serde(tag = "backend")]`, even though that would be the more natural Rust shape and is
/// exactly what `hs_config::storage::StorageConfig` itself uses — because the Kubernetes
/// structural-schema OpenAPI v3 dialect a CRD's schema must satisfy cannot express "the schema of
/// property X differs per `oneOf` branch" for a *required* property shared across branches;
/// `kube`'s schema conversion rejects it outright (verified empirically: `backend`'s per-variant
/// `enum: [embedded]` / `enum: [postgres]` / `enum: [slatedb]` collide). A flat struct with one
/// discriminator field and a block per backend (only the block matching `backend` is meaningful;
/// the reconciler validates that, not the schema) is the standard workaround every kube-rs CRD
/// with this shape uses.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// Which of `embedded`/`postgres`/`slatedb` below is meaningful.
    pub backend: StorageBackend,
    /// Meaningful when `backend: embedded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<EmbeddedStorageSpec>,
    /// Meaningful when `backend: postgres`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres: Option<PostgresStorageSpec>,
    /// Meaningful when `backend: slatedb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slatedb: Option<SlatedbStorageSpec>,
}

/// The storage backend discriminator. See [`StorageSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum StorageBackend {
    /// Embedded Fjall storage on a `PersistentVolumeClaim`. Forces `spec.replicas` to `1`: the
    /// chart's `singleNode` mode.
    Embedded,
    /// PostgreSQL, either CloudNativePG-managed or external: the chart's `cluster` mode.
    Postgres,
    /// SlateDB on object storage. Diskless clusters: the chart's `cluster` mode.
    Slatedb,
}

/// Embedded (Fjall) storage settings. See [`StorageSpec::embedded`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedStorageSpec {
    /// Requested PVC size, e.g. `"10Gi"`.
    pub size: String,
    /// `StorageClass` name; the cluster default is used if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
}

/// PostgreSQL storage settings. See [`StorageSpec::postgres`]. With `cloudNativePgCluster` the
/// connection comes from that cluster's `<name>-app` Secret; otherwise from `host`, `port`,
/// `database`, `user` and `passwordSecretRef` (the chart's `storage.postgres`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PostgresStorageSpec {
    /// A CloudNativePG `Cluster` name in the same namespace, to read connection details from its
    /// generated `<name>-app` Secret. Mutually exclusive with `host` in practice (the reconciler
    /// prefers `cloud_native_pg_cluster` when both are set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_native_pg_cluster: Option<String>,
    /// An externally managed PostgreSQL host, for a non-CloudNativePG database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The port; 5432 when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,
    /// Database name; `hs` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Connecting role; `hs` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The Secret key holding the connection password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret_ref: Option<SecretKeyRef>,
    /// libpq's `sslmode` for the connection (`disable`, `prefer`, `require`, `verify-ca` or
    /// `verify-full`; the chart's `storage.postgres.sslMode`); `prefer` when omitted. The server
    /// refuses to start when `require` or a verify mode cannot be met.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_mode: Option<String>,
    /// A path inside the pod to the PEM the server's certificate chains to, used by the verify
    /// modes only (the chart's `storage.postgres.sslRootCert`); ignored with any other mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_root_cert: Option<String>,
}

/// SlateDB-on-object-storage settings. See [`StorageSpec::slatedb`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SlatedbStorageSpec {
    /// The object store URL (`s3://bucket/prefix`, ...).
    pub bucket_url: String,
    /// Number of virtual storage shards.
    #[serde(default = "default_shard_count")]
    pub shard_count: u32,
}

fn default_shard_count() -> u32 {
    256
}

/// `spec` of a `Homeserver`.
///
/// Every field maps onto values of `deploy/helm/hs/values.yaml`
/// ([`crate::homeserver::chart_values`] is that mapping, and a test renders the chart with it and
/// compares the result with what the operator builds). There is no `mode` field:
/// `storage.backend: embedded` is the chart's `singleNode` mode (one replica, whatever
/// `replicas` says), and any other backend is `cluster` mode.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, CustomResource)]
#[kube(
    group = "hs.matrix.org",
    version = "v1alpha1",
    kind = "Homeserver",
    namespaced,
    shortname = "hs",
    status = "HomeserverStatus",
    scale = r#"{"specReplicasPath":".spec.replicas","statusReplicasPath":".status.replicas"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"ServerName","type":"string","jsonPath":".spec.serverName"}"#,
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".spec.replicas"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Draining","type":"string","jsonPath":".status.drain.pod"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct HomeserverSpec {
    /// The Matrix server name (`server.server_name`). Immutable after creation in practice
    /// (changing it after any room exists is not supported by any Matrix homeserver — see
    /// `hs_config::server::ServerConfig`'s doc comment); the reconciler does not enforce
    /// immutability itself (no Kubernetes CRD-level immutability marker for this yet), but the
    /// change would need a full new deployment in practice.
    pub server_name: String,
    /// The externally reachable base URL, if different from `https://{serverName}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    /// Number of replicas. Forced to `1` by the reconciler when `storage` is `Embedded`. Scaling
    /// down drains each replica that goes away through the admin API first (`drain`,
    /// `adminApi`).
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    /// The `hs` image to run.
    pub image: ImageSpec,
    /// Storage backend.
    pub storage: StorageSpec,
    /// The Ed25519 signing key (`hs generate-signing-key` format), from an existing `Secret`,
    /// mounted whole as a directory (the chart's `secrets.signingKey.existingSecret`; `key` is
    /// informational, the server reads every file in the directory). The operator never
    /// generates or stores a signing key itself, matching `deploy/helm/hs`'s same policy (a
    /// signing key must survive the CR being deleted and recreated).
    pub signing_key_secret_ref: SecretKeyRef,
    /// `auth.registration_shared_secret`, from an existing `Secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_shared_secret_ref: Option<SecretKeyRef>,
    /// `auth.session_secret`, from an existing `Secret` (the chart's `secrets.sessionSecret`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_secret_ref: Option<SecretKeyRef>,
    /// Cluster-mode settings (the chart's `cluster`). Ignored with embedded storage.
    #[serde(default)]
    pub cluster: ClusterSpec,
    /// Where media is kept (the chart's `media.storage`).
    #[serde(default)]
    pub media: MediaSpec,
    /// The server container's resources; the chart's defaults when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    /// How the operator drains a replica before its pod goes away.
    #[serde(default)]
    pub drain: DrainSpec,
    /// How the operator reaches the server's admin API to drain replicas. Without it the
    /// operator scales down and rolls pods without draining them first, and each pod's own
    /// `SIGTERM` handoff (twenty seconds) is all there is; the `DrainAvailable` condition says
    /// so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_api: Option<AdminApiSpec>,
    /// Extra native `hs-config` YAML, appended to the operator-generated `ConfigMap` the same way
    /// `deploy/helm/hs/values.yaml`'s `extraConfig` does.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_config: serde_json::Map<String, serde_json::Value>,
}

fn default_replicas() -> i32 {
    1
}

/// Cluster-mode settings: the mesh, the shard layout and the heartbeat. Every field has the
/// chart's default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSpec {
    /// A `kubernetes.io/tls` Secret (`tls.crt`, `tls.key`, `ca.crt`) whose certificate covers
    /// `*.<name>-headless.<namespace>.svc.<clusterDomain>`: the mesh's mutual TLS (the chart's
    /// `cluster.mesh.tls.existingSecret`). This or `meshSharedSecretRef` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_tls_secret: Option<String>,
    /// A shared secret authenticating the mesh instead of TLS, for a trusted pod network only
    /// (the chart's `cluster.mesh.sharedSecret`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_shared_secret_ref: Option<SecretKeyRef>,
    /// The mesh port.
    #[serde(default = "default_mesh_port")]
    pub mesh_port: i32,
    /// Room shards (`cluster.room_shards`).
    #[serde(default = "default_shards")]
    pub room_shards: u32,
    /// User shards (`cluster.user_shards`).
    #[serde(default = "default_shards")]
    pub user_shards: u32,
    /// How often a replica heartbeats (`cluster.heartbeat_interval`).
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: String,
    /// How long a lease outlives its last heartbeat (`cluster.lease_ttl`).
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl: String,
    /// The cluster's DNS domain, for the mesh names.
    #[serde(default = "default_cluster_domain")]
    pub cluster_domain: String,
    /// The pods' grace period; must outlast the server's own twenty-second shutdown handoff.
    #[serde(default = "default_grace")]
    pub termination_grace_period_seconds: i64,
    /// How replicas are spread over nodes.
    #[serde(default)]
    pub anti_affinity: AntiAffinity,
}

impl Default for ClusterSpec {
    fn default() -> Self {
        Self {
            mesh_tls_secret: None,
            mesh_shared_secret_ref: None,
            mesh_port: default_mesh_port(),
            room_shards: default_shards(),
            user_shards: default_shards(),
            heartbeat_interval: default_heartbeat_interval(),
            lease_ttl: default_lease_ttl(),
            cluster_domain: default_cluster_domain(),
            termination_grace_period_seconds: default_grace(),
            anti_affinity: AntiAffinity::default(),
        }
    }
}

fn default_mesh_port() -> i32 {
    8449
}
fn default_shards() -> u32 {
    256
}
fn default_heartbeat_interval() -> String {
    "2s".to_owned()
}
fn default_lease_ttl() -> String {
    "10s".to_owned()
}
fn default_cluster_domain() -> String {
    "cluster.local".to_owned()
}
fn default_grace() -> i64 {
    40
}

/// Pod anti-affinity between replicas, by node (the chart's `affinity.podAntiAffinity`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum AntiAffinity {
    /// Preferred: spread when possible.
    #[default]
    Soft,
    /// Required: never two replicas on one node.
    Hard,
    /// No anti-affinity.
    None,
}

/// Where media is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaSpec {
    /// `local` (the data volume, or `localClaim`) or `s3`.
    #[serde(default)]
    pub backend: MediaBackend,
    /// A `ReadWriteMany` claim for local media shared by every replica. Required for local
    /// media in cluster mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_claim: Option<String>,
    /// The bucket, when `backend: s3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3MediaSpec>,
}

/// The media backend discriminator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum MediaBackend {
    /// A filesystem.
    #[default]
    Local,
    /// An S3-compatible bucket.
    S3,
}

/// An S3-compatible media bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct S3MediaSpec {
    /// The bucket.
    pub bucket: String,
    /// The region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The endpoint, for a store other than AWS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The access key id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    /// The secret access key, mounted from a Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key_ref: Option<SecretKeyRef>,
}

/// How a replica is drained before its pod is removed (scale-down) or replaced (rolling
/// update). `docs/decisions/0012-a-drain-is-a-request-in-the-shared-store.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrainSpec {
    /// How long a replica may take to hand off every shard before `onTimeout` applies.
    #[serde(default = "default_drain_timeout")]
    pub timeout_seconds: u64,
    /// What happens when a drain times out.
    #[serde(default)]
    pub on_timeout: DrainTimeoutPolicy,
}

impl Default for DrainSpec {
    fn default() -> Self {
        Self {
            timeout_seconds: default_drain_timeout(),
            on_timeout: DrainTimeoutPolicy::default(),
        }
    }
}

fn default_drain_timeout() -> u64 {
    600
}

/// What a timed-out drain does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DrainTimeoutPolicy {
    /// Let the pod go anyway: its `SIGTERM` handoff releases what it still owns, and the others
    /// take the rest when its lease expires. A Warning event and the `Draining` condition say
    /// so. The default, so a stuck drain never blocks a rollout forever.
    #[default]
    Proceed,
    /// Keep the pod and keep waiting, until the drain completes or the change is reverted.
    Hold,
}

/// How the operator reaches the admin API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminApiSpec {
    /// A bearer token with `admin:write` (an OAuth client-credentials token, or a server
    /// administrator's access token), from a Secret in the same namespace.
    pub token_secret_ref: SecretKeyRef,
    /// The server's base URL; `http://<name>.<namespace>.svc:8008` (the client Service) when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `status` of a `Homeserver`: the fields every kind reports ([`crate::crds::OperatorStatus`]'s
/// shape) plus the drain the operator is following, if any.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HomeserverStatus {
    /// Coarse lifecycle phase.
    #[serde(default)]
    pub phase: Phase,
    /// The `.metadata.generation` this status was computed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Pods the StatefulSet has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    /// Pods ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_replicas: Option<i32>,
    /// Pods running the current pod template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_replicas: Option<i32>,
    /// `Ready`, `Progressing`, `Draining`, `DrainAvailable` and `SpecValid`.
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The drain in flight: the operator is waiting for this replica to own no shards before
    /// its pod goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<DrainStatus>,
    /// Replicas the operator drained and will undrain: after their pod is gone (scale-down), or
    /// once their replacement is ready (rolling update), since a StatefulSet pod's replacement
    /// has the same name and inherits the drain request (decision 0012).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_undrains: Vec<PendingUndrain>,
}

/// Why a replica is being drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DrainReason {
    /// Its pod is being removed by a scale-down.
    ScaleDown,
    /// Its pod is being replaced by a rolling update.
    RollingUpdate,
}

/// One drain in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrainStatus {
    /// The pod.
    pub pod: String,
    /// The replica id the admin API knows it by (its mesh address).
    pub replica_id: String,
    /// Why.
    pub reason: DrainReason,
    /// When the operator asked (RFC 3339).
    pub started_at: String,
    /// The admin API task following the drain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// That task's status when last read (`running`, `succeeded`, `failed`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_status: Option<String>,
    /// Shards the replica still owned when last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shards_remaining: Option<u64>,
    /// Whether it outlived `drain.timeoutSeconds` (with `onTimeout: Hold`).
    #[serde(default)]
    pub timed_out: bool,
}

/// When a drained replica is undrained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum UndrainWhen {
    /// Once its pod no longer exists (a scale-down), so a later scale-up does not bring the
    /// ordinal back drained.
    AfterRemoval,
    /// Once its pod's replacement runs the current template and is ready (a rolling update).
    WhenReady,
}

/// A replica the operator drained and will undrain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PendingUndrain {
    /// The pod.
    pub pod: String,
    /// The replica id.
    pub replica_id: String,
    /// When.
    pub when: UndrainWhen,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HomeserverSpec {
        serde_json::from_value(serde_json::json!({
            "serverName": "example.org",
            "replicas": 3,
            "image": {"repository": "ghcr.io/matrix-org/hs", "tag": "0.0.1"},
            "storage": {"backend": "postgres", "postgres": {"cloudNativePgCluster": "hs-pg"}},
            "signingKeySecretRef": {"name": "hs-signing-key", "key": "signing.key"},
            "cluster": {"meshTlsSecret": "hs-mesh"}
        }))
        .unwrap()
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        let back: HomeserverSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.server_name, spec.server_name);
        assert_eq!(back.replicas, spec.replicas);
        assert_eq!(back.cluster, spec.cluster);
    }

    #[test]
    fn spec_round_trips_through_yaml() {
        let spec = sample();
        let yaml = serde_yaml_ng::to_string(&spec).unwrap();
        let back: HomeserverSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back.server_name, spec.server_name);
    }

    #[test]
    fn omitted_blocks_take_the_charts_defaults() {
        let spec = sample();
        assert_eq!(spec.cluster.mesh_port, 8449);
        assert_eq!(spec.cluster.room_shards, 256);
        assert_eq!(spec.cluster.heartbeat_interval, "2s");
        assert_eq!(spec.cluster.lease_ttl, "10s");
        assert_eq!(spec.cluster.termination_grace_period_seconds, 40);
        assert_eq!(spec.cluster.anti_affinity, AntiAffinity::Soft);
        assert_eq!(spec.media.backend, MediaBackend::Local);
        assert_eq!(spec.drain.timeout_seconds, 600);
        assert_eq!(spec.drain.on_timeout, DrainTimeoutPolicy::Proceed);
        assert!(spec.admin_api.is_none());
    }

    #[test]
    fn storage_backend_is_camel_case() {
        let embedded = StorageSpec {
            backend: StorageBackend::Embedded,
            embedded: Some(EmbeddedStorageSpec {
                size: "10Gi".to_owned(),
                storage_class_name: None,
            }),
            postgres: None,
            slatedb: None,
        };
        let json = serde_json::to_value(&embedded).unwrap();
        assert_eq!(json["backend"], "embedded");
    }

    #[test]
    fn a_status_with_a_drain_is_camel_case_on_the_wire() {
        let status = HomeserverStatus {
            drain: Some(DrainStatus {
                pod: "hs-2".to_owned(),
                replica_id: "hs-2.hs-headless.ns.svc.cluster.local:8449".to_owned(),
                reason: DrainReason::ScaleDown,
                started_at: "2026-09-28T00:00:00Z".to_owned(),
                task_id: Some("t1".to_owned()),
                task_status: Some("running".to_owned()),
                shards_remaining: Some(3),
                timed_out: false,
            }),
            ..HomeserverStatus::default()
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json["drain"]["replicaId"],
            "hs-2.hs-headless.ns.svc.cluster.local:8449"
        );
        assert_eq!(json["drain"]["reason"], "ScaleDown");
        assert_eq!(json["drain"]["shardsRemaining"], 3);
        assert!(json.get("pendingUndrains").is_none());
    }
}
