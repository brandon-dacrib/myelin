//! What a [`Homeserver`] becomes: pure builders for the objects `deploy/helm/hs` renders for
//! equivalent values (a `ConfigMap`, the client `Service`, the headless `Service`, a
//! `ServiceAccount`, the `StatefulSet` with its probes, and in cluster mode a
//! `PodDisruptionBudget`), and [`chart_values`], the values that make the chart render the same
//! thing. No I/O here.
//!
//! # One source of truth, checked
//!
//! The chart is Go templates and this is Rust, so the two cannot literally share a template.
//! What keeps them one design is `tests/helm_equivalence.rs`: it renders the chart with
//! [`chart_values`] for several resources (single-node, cluster on PostgreSQL with mesh TLS and
//! S3 media, cluster on CloudNativePG with a shared mesh secret) and compares every object with
//! what these builders produce, field by field. A change to either side that is not made to the
//! other fails that test. The differences it tolerates, on purpose, are listed in
//! [`chart_differences`].

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{
    RollingUpdateStatefulSetStrategy, StatefulSet, StatefulSetSpec, StatefulSetUpdateStrategy,
};
use k8s_openapi::api::core::v1::{
    Affinity, Capabilities, ConfigMap, ConfigMapVolumeSource, Container, ContainerPort,
    EmptyDirVolumeSource, EnvVar, EnvVarSource, HTTPGetAction, ObjectFieldSelector,
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource,
    PodAffinityTerm, PodAntiAffinity, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    ResourceRequirements, SeccompProfile, SecretKeySelector, SecretVolumeSource, SecurityContext,
    Service, ServiceAccount, ServicePort, ServiceSpec, Volume, VolumeMount,
    VolumeResourceRequirements, WeightedPodAffinityTerm,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{Resource as _, ResourceExt as _};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::bridge::MANAGER;
use crate::crds::{
    AntiAffinity, Homeserver, HomeserverSpec, MediaBackend, SecretKeyRef, StorageBackend,
};

/// `app.kubernetes.io/name` of a homeserver's objects: the chart's name, so the chart and the
/// operator select pods identically.
pub const APP_NAME: &str = "hs";
/// StatefulSet annotation holding a hash of the pod template the operator applied, so it can
/// tell a template change (which it must roll itself, draining first) from a no-op.
pub const ANNOTATION_TEMPLATE_HASH: &str = "hs.matrix.org/pod-template-hash";
/// Pod-template annotation holding the config's hash, so a config change rolls the pods (the
/// chart's `checksum/config`).
pub const ANNOTATION_CONFIG_CHECKSUM: &str = "checksum/config";
/// The client port (the chart's `service.clientPort`).
pub const CLIENT_PORT: i32 = 8008;
/// The federation port (the chart's `service.federationPort`).
pub const FEDERATION_PORT: i32 = 8448;
/// The metrics port (the chart's `service.metricsPort`).
pub const METRICS_PORT: i32 = 9090;
/// The container's name.
pub const CONTAINER_NAME: &str = "hs";

const DATA_DIR: &str = "/var/lib/hs/data";
const CONFIG_FILE: &str = "/etc/hs/config/homeserver.yaml";

/// The ways the operator's objects differ from the chart's on purpose. The equivalence test
/// normalises exactly these away and nothing else.
///
/// - Labels: the chart adds `helm.sh/chart`, `app.kubernetes.io/version` and
///   `app.kubernetes.io/managed-by: Helm`; the operator adds `app.kubernetes.io/managed-by:
///   myelin-operator` and neither of the others. Selector labels are identical.
/// - `checksum/config`: both roll the pods on a config change, but the chart hashes its rendered
///   template file and the operator the config text.
/// - The StatefulSet's `updateStrategy`: the chart leaves the default (`RollingUpdate`, the
///   StatefulSet controller replaces pods itself); the operator sets a `partition` and lowers it
///   one pod at a time after draining that pod, so no pod is replaced before it has handed off
///   its shards. The operator also stamps [`ANNOTATION_TEMPLATE_HASH`] on the StatefulSet.
/// - Owner references: the operator's objects are owned by the `Homeserver`.
/// - The embedded data claim template: the chart's carries `helm.sh/resource-policy: keep`,
///   which means nothing outside Helm (a StatefulSet's claims outlive it anyway).
#[must_use]
pub fn chart_differences() -> &'static [&'static str] {
    &[
        "labels helm.sh/chart, app.kubernetes.io/version, app.kubernetes.io/managed-by",
        "pod template annotation checksum/config",
        "StatefulSet spec.updateStrategy and metadata.annotations",
        "metadata.ownerReferences",
        "volumeClaimTemplates[].metadata.annotations",
    ]
}

/// Whether the spec runs the chart's `cluster` mode (any backend but embedded).
#[must_use]
pub fn is_cluster(spec: &HomeserverSpec) -> bool {
    spec.storage.backend != StorageBackend::Embedded
}

/// The replica count the spec asks for: always 1 with embedded storage, never negative.
#[must_use]
pub fn desired_replicas(spec: &HomeserverSpec) -> i32 {
    if is_cluster(spec) {
        spec.replicas.max(0)
    } else {
        1
    }
}

/// Why a spec cannot be turned into a workload: the same checks as the chart's `hs.validate`,
/// with the same wording where there is an equivalent.
///
/// # Errors
/// A message for the `SpecValid` condition when the spec is not usable.
pub fn validate(spec: &HomeserverSpec) -> Result<(), String> {
    if spec.server_name.trim().is_empty() {
        return Err("serverName is required".to_owned());
    }
    if spec.replicas < 0 {
        return Err("replicas cannot be negative".to_owned());
    }
    match spec.media.backend {
        MediaBackend::S3 => {
            if spec.media.s3.as_ref().is_none_or(|s3| s3.bucket.is_empty()) {
                return Err("media.backend is s3 but media.s3.bucket is not set".to_owned());
            }
        }
        MediaBackend::Local => {
            if is_cluster(spec) && spec.media.local_claim.is_none() {
                return Err(
                    "media.backend is local, but this deployment has no data volume to \
                            keep media on (storage.backend is not embedded) and its replicas \
                            would not share one anyway: set media.backend to s3, or point \
                            media.localClaim at a ReadWriteMany claim"
                        .to_owned(),
                );
            }
        }
    }
    match spec.storage.backend {
        StorageBackend::Embedded => {
            if spec.storage.embedded.is_none() {
                return Err(
                    "storage.backend is embedded but storage.embedded is not set".to_owned(),
                );
            }
        }
        StorageBackend::Postgres => {
            let ok = spec.storage.postgres.as_ref().is_some_and(|pg| {
                pg.cloud_native_pg_cluster
                    .as_deref()
                    .is_some_and(|c| !c.is_empty())
                    || pg.host.as_deref().is_some_and(|h| !h.is_empty())
            });
            if !ok {
                return Err("storage.backend is postgres but neither \
                            storage.postgres.cloudNativePgCluster nor storage.postgres.host is set"
                    .to_owned());
            }
        }
        StorageBackend::Slatedb => {
            if spec.storage.slatedb.is_none() {
                return Err("storage.backend is slatedb but storage.slatedb is not set".to_owned());
            }
        }
    }
    if is_cluster(spec)
        && spec.cluster.mesh_tls_secret.is_none()
        && spec.cluster.mesh_shared_secret_ref.is_none()
    {
        return Err(
            "storage is shared (cluster mode) but the mesh has no authentication: set \
                    cluster.meshTlsSecret (a kubernetes.io/tls Secret with tls.crt, tls.key and \
                    ca.crt for *.<name>-headless.<namespace>.svc.<clusterDomain>) or, on a \
                    trusted pod network only, cluster.meshSharedSecretRef"
                .to_owned(),
        );
    }
    Ok(())
}

/// The labels that select a homeserver's pods: the chart's `hs.selectorLabels`.
#[must_use]
pub fn selector_labels(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_owned(), APP_NAME.to_owned()),
        ("app.kubernetes.io/instance".to_owned(), name.to_owned()),
    ])
}

/// Every label the operator puts on a homeserver's objects and pods.
#[must_use]
pub fn labels(name: &str) -> BTreeMap<String, String> {
    let mut labels = selector_labels(name);
    labels.insert(
        "app.kubernetes.io/managed-by".to_owned(),
        MANAGER.to_owned(),
    );
    labels
}

/// The selector for a homeserver's pods, as a `labelSelector` query string.
#[must_use]
pub fn pod_selector(name: &str) -> String {
    selector_labels(name)
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The headless Service's name, under which every pod has a stable DNS name.
#[must_use]
pub fn headless_name(name: &str) -> String {
    format!("{name}-headless")
}

/// The ConfigMap's name.
#[must_use]
pub fn config_map_name(name: &str) -> String {
    format!("{name}-config")
}

/// The DNS domain the mesh advertises under: `<name>-headless.<namespace>.svc.<clusterDomain>`
/// (the chart's `hs.meshDomain`).
#[must_use]
pub fn mesh_domain(name: &str, namespace: &str, spec: &HomeserverSpec) -> String {
    format!(
        "{}.{namespace}.svc.{}",
        headless_name(name),
        spec.cluster.cluster_domain
    )
}

/// The replica id a pod registers under in cluster mode: its mesh address,
/// `<pod>.<mesh domain>:<mesh port>` (`crates/hs-cli/src/cluster.rs`).
#[must_use]
pub fn replica_id(pod: &str, name: &str, namespace: &str, spec: &HomeserverSpec) -> String {
    format!(
        "{pod}.{}:{}",
        mesh_domain(name, namespace, spec),
        spec.cluster.mesh_port
    )
}

/// The pod name of a StatefulSet ordinal.
#[must_use]
pub fn pod_name(name: &str, ordinal: i32) -> String {
    format!("{name}-{ordinal}")
}

/// The ordinal of a pod of this StatefulSet, from its name.
#[must_use]
pub fn pod_ordinal(name: &str, pod: &str) -> Option<i32> {
    pod.strip_prefix(name)?.strip_prefix('-')?.parse().ok()
}

/// Everything the operator applies for one `Homeserver`, except the StatefulSet's replica count
/// and partition, which [`crate::homeserver::reconciler`] decides.
#[derive(Debug, Clone)]
pub struct DesiredObjects {
    /// The `hs` configuration file.
    pub config_map: ConfigMap,
    /// The client Service.
    pub service: Service,
    /// The headless Service.
    pub headless_service: Service,
    /// The pods' service account.
    pub service_account: ServiceAccount,
    /// The StatefulSet; `spec.replicas` and the update partition are placeholders.
    pub stateful_set: StatefulSet,
    /// The disruption budget, in cluster mode with more than one replica.
    pub pod_disruption_budget: Option<PodDisruptionBudget>,
    /// [`ANNOTATION_TEMPLATE_HASH`]'s value.
    pub template_hash: String,
}

/// Why objects could not be built.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The resource has no name or namespace.
    #[error("the Homeserver has no {0}")]
    Missing(&'static str),
    /// The spec failed [`validate`].
    #[error("{0}")]
    Invalid(String),
    /// The configuration could not be rendered.
    #[error("rendering the configuration: {0}")]
    Render(#[from] serde_yaml_ng::Error),
}

fn owner_reference(hs: &Homeserver) -> Option<OwnerReference> {
    hs.controller_owner_ref(&()).map(|r| OwnerReference {
        block_owner_deletion: Some(true),
        ..r
    })
}

fn metadata(hs: &Homeserver, name: String) -> ObjectMeta {
    ObjectMeta {
        name: Some(name),
        namespace: hs.namespace(),
        labels: Some(labels(&hs.name_any())),
        owner_references: owner_reference(hs).map(|r| vec![r]),
        ..ObjectMeta::default()
    }
}

/// Builds every object for `hs`.
///
/// # Errors
/// When the resource has no name or namespace, or its spec fails [`validate`].
pub fn build(hs: &Homeserver) -> Result<DesiredObjects, BuildError> {
    let name = hs
        .metadata
        .name
        .clone()
        .ok_or(BuildError::Missing("name"))?;
    let namespace = hs
        .metadata
        .namespace
        .clone()
        .ok_or(BuildError::Missing("namespace"))?;
    validate(&hs.spec).map_err(BuildError::Invalid)?;
    let config_text = config_yaml(&hs.spec)?;
    let config_checksum = hex::encode(Sha256::digest(config_text.as_bytes()));
    let config_map = ConfigMap {
        metadata: metadata(hs, config_map_name(&name)),
        data: Some(BTreeMap::from([(
            "homeserver.yaml".to_owned(),
            config_text,
        )])),
        ..ConfigMap::default()
    };
    let mut stateful_set = stateful_set(hs, &name, &namespace, &config_checksum);
    let template_hash = template_hash(&stateful_set);
    stateful_set.metadata.annotations = Some(BTreeMap::from([(
        ANNOTATION_TEMPLATE_HASH.to_owned(),
        template_hash.clone(),
    )]));
    Ok(DesiredObjects {
        config_map,
        service: client_service(hs, &name),
        headless_service: headless_service(hs, &name),
        service_account: ServiceAccount {
            metadata: metadata(hs, name.clone()),
            ..ServiceAccount::default()
        },
        pod_disruption_budget: pod_disruption_budget(hs, &name),
        stateful_set,
        template_hash,
    })
}

/// A short stable hash of the StatefulSet's pod template and claim templates.
fn template_hash(sts: &StatefulSet) -> String {
    let spec = sts.spec.as_ref();
    let json = serde_json::to_vec(&(
        spec.map(|s| &s.template),
        spec.and_then(|s| s.volume_claim_templates.as_ref()),
    ))
    .unwrap_or_default();
    hex::encode(&Sha256::digest(&json)[..8])
}

/// The `homeserver.yaml` the chart's `configmap.yaml` renders for the same values.
///
/// # Errors
/// When the YAML cannot be rendered (it always can).
pub fn config_yaml(spec: &HomeserverSpec) -> Result<String, serde_yaml_ng::Error> {
    serde_yaml_ng::to_string(&config_value(spec))
}

/// [`config_yaml`] as a value.
#[must_use]
pub fn config_value(spec: &HomeserverSpec) -> Value {
    let cluster = is_cluster(spec);
    let mut config = Map::new();
    config.insert(
        "listeners".to_owned(),
        json!({"listeners": [
            {"port": CLIENT_PORT, "bind_addresses": ["::"], "resources": ["client", "federation", "health"]},
            {"port": METRICS_PORT, "bind_addresses": ["::"], "resources": ["metrics"]},
        ]}),
    );
    match spec.storage.backend {
        StorageBackend::Embedded => {}
        StorageBackend::Postgres => {
            let pg = spec.storage.postgres.clone();
            let cnpg = pg
                .as_ref()
                .and_then(|p| p.cloud_native_pg_cluster.as_ref())
                .is_some_and(|c| !c.is_empty());
            let storage = if cnpg {
                json!({
                    "backend": "postgres",
                    "host": "PLACEHOLDER_SET_VIA_HS__STORAGE__HOST_ENV",
                    "database": "PLACEHOLDER_SET_VIA_HS__STORAGE__DATABASE_ENV",
                    "user": "PLACEHOLDER_SET_VIA_HS__STORAGE__USER_ENV",
                })
            } else {
                let pg = pg.unwrap_or_default();
                json!({
                    "backend": "postgres",
                    "host": pg.host.unwrap_or_default(),
                    "port": pg.port.unwrap_or(5432),
                    "database": pg.database.unwrap_or_else(|| "hs".to_owned()),
                    "user": pg.user.unwrap_or_else(|| "hs".to_owned()),
                })
            };
            config.insert("storage".to_owned(), storage);
        }
        StorageBackend::Slatedb => {
            let (url, shards) = spec
                .storage
                .slatedb
                .as_ref()
                .map_or((String::new(), 256), |s| {
                    (s.bucket_url.clone(), s.shard_count)
                });
            config.insert(
                "storage".to_owned(),
                json!({"backend": "slatedb", "bucket_url": url, "shard_count": shards}),
            );
        }
    }
    let s3 = spec.media.backend == MediaBackend::S3;
    if s3 || spec.media.local_claim.is_some() || cluster {
        let storage = if s3 {
            let mut m = Map::new();
            m.insert("backend".to_owned(), json!("s3"));
            if let Some(s3) = &spec.media.s3 {
                m.insert("bucket".to_owned(), json!(s3.bucket));
                if let Some(v) = s3.region.as_deref().filter(|v| !v.is_empty()) {
                    m.insert("region".to_owned(), json!(v));
                }
                if let Some(v) = s3.endpoint.as_deref().filter(|v| !v.is_empty()) {
                    m.insert("endpoint".to_owned(), json!(v));
                }
                if let Some(v) = s3.access_key_id.as_deref().filter(|v| !v.is_empty()) {
                    m.insert("access_key_id".to_owned(), json!(v));
                }
                if let Some(secret) = &s3.secret_access_key_ref {
                    m.insert(
                        "secret_access_key_file".to_owned(),
                        json!(format!("/etc/hs/secrets/media-s3/{}", secret.key)),
                    );
                }
            }
            Value::Object(m)
        } else {
            json!({"backend": "local", "path": media_path(spec)})
        };
        config.insert("media".to_owned(), json!({ "storage": storage }));
    }
    if cluster {
        config.insert(
            "cluster".to_owned(),
            json!({
                "room_shards": spec.cluster.room_shards,
                "user_shards": spec.cluster.user_shards,
                "heartbeat_interval": spec.cluster.heartbeat_interval,
                "lease_ttl": spec.cluster.lease_ttl,
            }),
        );
    }
    config.insert(
        "telemetry".to_owned(),
        json!({
            "metrics": {"enabled": true, "synapse_compat_names": true},
            "logging": {"level": "info", "json": true},
            "tracing": {"enabled": false, "sample_ratio": 0.1},
        }),
    );
    // The chart appends `extraConfig` after everything, so a key there wins.
    for (k, v) in &spec.extra_config {
        config.insert(k.clone(), v.clone());
    }
    Value::Object(config)
}

fn media_path(spec: &HomeserverSpec) -> &'static str {
    if spec.media.local_claim.is_some() {
        "/var/lib/hs/media"
    } else {
        "/var/lib/hs/data/media"
    }
}

fn client_service(hs: &Homeserver, name: &str) -> Service {
    let port = |n: &str, p: i32| ServicePort {
        name: Some(n.to_owned()),
        port: p,
        target_port: Some(IntOrString::String(n.to_owned())),
        protocol: Some("TCP".to_owned()),
        ..ServicePort::default()
    };
    Service {
        metadata: metadata(hs, name.to_owned()),
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_owned()),
            selector: Some(selector_labels(name)),
            ports: Some(vec![
                port("client", CLIENT_PORT),
                port("federation", FEDERATION_PORT),
                port("metrics", METRICS_PORT),
            ]),
            ..ServiceSpec::default()
        }),
        status: None,
    }
}

fn headless_service(hs: &Homeserver, name: &str) -> Service {
    let port = |n: &str, p: i32| ServicePort {
        name: Some(n.to_owned()),
        port: p,
        target_port: Some(IntOrString::String(n.to_owned())),
        protocol: Some("TCP".to_owned()),
        ..ServicePort::default()
    };
    let mut ports = vec![port("client", CLIENT_PORT)];
    if is_cluster(&hs.spec) {
        ports.push(port("mesh", hs.spec.cluster.mesh_port));
    }
    Service {
        metadata: metadata(hs, headless_name(name)),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_owned()),
            publish_not_ready_addresses: Some(true),
            selector: Some(selector_labels(name)),
            ports: Some(ports),
            ..ServiceSpec::default()
        }),
        status: None,
    }
}

fn pod_disruption_budget(hs: &Homeserver, name: &str) -> Option<PodDisruptionBudget> {
    (is_cluster(&hs.spec) && desired_replicas(&hs.spec) > 1).then(|| PodDisruptionBudget {
        metadata: metadata(hs, name.to_owned()),
        spec: Some(PodDisruptionBudgetSpec {
            selector: Some(LabelSelector {
                match_labels: Some(selector_labels(name)),
                match_expressions: None,
            }),
            min_available: Some(IntOrString::Int(1)),
            ..PodDisruptionBudgetSpec::default()
        }),
        status: None,
    })
}

fn env(name: &str, value: impl Into<String>) -> EnvVar {
    EnvVar {
        name: name.to_owned(),
        value: Some(value.into()),
        value_from: None,
    }
}

fn env_secret(name: &str, secret: &str, key: &str) -> EnvVar {
    EnvVar {
        name: name.to_owned(),
        value: None,
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret.to_owned(),
                key: key.to_owned(),
                optional: None,
            }),
            ..EnvVarSource::default()
        }),
    }
}

fn mount(name: &str, path: &str, sub_path: Option<&str>, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.to_owned(),
        mount_path: path.to_owned(),
        sub_path: sub_path.map(str::to_owned),
        read_only: read_only.then_some(true),
        ..VolumeMount::default()
    }
}

fn secret_volume(name: &str, secret: &str) -> Volume {
    Volume {
        name: name.to_owned(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(secret.to_owned()),
            ..SecretVolumeSource::default()
        }),
        ..Volume::default()
    }
}

fn claim_volume(name: &str, claim: &str) -> Volume {
    Volume {
        name: name.to_owned(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: claim.to_owned(),
            read_only: None,
        }),
        ..Volume::default()
    }
}

fn http_probe(path: &str) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.to_owned()),
            port: IntOrString::String("client".to_owned()),
            ..HTTPGetAction::default()
        }),
        ..Probe::default()
    }
}

/// The chart's default container resources.
fn default_resources() -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_owned(), Quantity("100m".to_owned())),
            ("memory".to_owned(), Quantity("256Mi".to_owned())),
        ])),
        limits: Some(BTreeMap::from([(
            "memory".to_owned(),
            Quantity("1Gi".to_owned()),
        )])),
        ..ResourceRequirements::default()
    }
}

/// The image reference the chart renders: `repository@digest`, else `repository:tag`, else
/// `repository:latest`.
fn image_reference(spec: &HomeserverSpec) -> String {
    let image = &spec.image;
    match (&image.digest, &image.tag) {
        (Some(d), _) if !d.is_empty() => format!("{}@{d}", image.repository),
        (_, Some(t)) if !t.is_empty() => format!("{}:{t}", image.repository),
        _ => format!("{}:latest", image.repository),
    }
}

/// The chart's pull policy: the spec's, else `IfNotPresent` for a digest and `Always` for a tag.
fn pull_policy(spec: &HomeserverSpec) -> String {
    let image = &spec.image;
    match (&image.pull_policy, &image.digest) {
        (Some(p), _) if !p.is_empty() => p.clone(),
        (_, Some(d)) if !d.is_empty() => "IfNotPresent".to_owned(),
        _ => "Always".to_owned(),
    }
}

#[allow(clippy::too_many_lines)] // One pod template, in the chart's order, is easiest to compare in one place.
fn stateful_set(
    hs: &Homeserver,
    name: &str,
    namespace: &str,
    config_checksum: &str,
) -> StatefulSet {
    let spec = &hs.spec;
    let cluster = is_cluster(spec);
    let embedded = !cluster;
    let mesh_domain = mesh_domain(name, namespace, spec);

    let mut env_vars = vec![env("HS__SERVER__SERVER_NAME", spec.server_name.clone())];
    if let Some(url) = spec.public_base_url.as_deref().filter(|u| !u.is_empty()) {
        env_vars.push(env("HS__SERVER__PUBLIC_BASEURL", url));
    }
    env_vars.push(env(
        "HS__SERVER__SIGNING_KEY_PATH",
        "/etc/hs/secrets/signing-key",
    ));
    if spec.storage.backend == StorageBackend::Postgres
        && let Some(pg) = &spec.storage.postgres
    {
        if let Some(cnpg) = pg
            .cloud_native_pg_cluster
            .as_deref()
            .filter(|c| !c.is_empty())
        {
            let secret = format!("{cnpg}-app");
            env_vars.push(env("HS__STORAGE__BACKEND", "postgres"));
            for (var, key) in [
                ("HS__STORAGE__HOST", "host"),
                ("HS__STORAGE__PORT", "port"),
                ("HS__STORAGE__DATABASE", "dbname"),
                ("HS__STORAGE__USER", "user"),
                ("HS__STORAGE__PASSWORD", "password"),
            ] {
                env_vars.push(env_secret(var, &secret, key));
            }
        } else if let Some(password) = &pg.password_secret_ref {
            env_vars.push(env_secret(
                "HS__STORAGE__PASSWORD",
                &password.name,
                &password.key,
            ));
        }
    }
    if embedded {
        env_vars.push(env("HS_DATA_DIR", DATA_DIR));
    }
    if cluster {
        env_vars.push(env("HS__CLUSTER__SINGLE_NODE", "false"));
        env_vars.push(EnvVar {
            name: "POD_NAME".to_owned(),
            value: None,
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    api_version: None,
                    field_path: "metadata.name".to_owned(),
                }),
                ..EnvVarSource::default()
            }),
        });
        env_vars.push(env(
            "HS__CLUSTER__MESH__ADVERTISE_ADDRESS",
            format!("$(POD_NAME).{mesh_domain}"),
        ));
        env_vars.push(env(
            "HS__CLUSTER__MESH__PORT",
            spec.cluster.mesh_port.to_string(),
        ));
        if spec.cluster.mesh_tls_secret.is_some() {
            env_vars.push(env(
                "HS__CLUSTER__MESH__TLS__CERTIFICATE_PATH",
                "/etc/hs/secrets/mesh-tls/tls.crt",
            ));
            env_vars.push(env(
                "HS__CLUSTER__MESH__TLS__PRIVATE_KEY_PATH",
                "/etc/hs/secrets/mesh-tls/tls.key",
            ));
            env_vars.push(env(
                "HS__CLUSTER__MESH__TLS__CA_CERTIFICATE_PATH",
                "/etc/hs/secrets/mesh-tls/ca.crt",
            ));
            env_vars.push(env(
                "HS__CLUSTER__MESH__TLS__PEER_SAN_SUFFIX",
                format!(".{mesh_domain}"),
            ));
        } else {
            env_vars.push(env(
                "HS__CLUSTER__MESH__SHARED_SECRET_FILE",
                "/etc/hs/secrets/mesh-shared-secret",
            ));
        }
    }
    if spec.registration_shared_secret_ref.is_some() {
        env_vars.push(env(
            "HS__AUTH__REGISTRATION_SHARED_SECRET_FILE",
            "/etc/hs/secrets/registration-shared-secret",
        ));
    }
    if spec.session_secret_ref.is_some() {
        env_vars.push(env(
            "HS__AUTH__SESSION_SECRET_FILE",
            "/etc/hs/secrets/session-secret",
        ));
    }

    // Mounts and volumes, in the chart's order.
    let mut mounts = vec![
        mount("config", "/etc/hs/config", None, true),
        mount(
            "secrets-signing-key",
            "/etc/hs/secrets/signing-key",
            None,
            true,
        ),
    ];
    let mut volumes = vec![
        Volume {
            name: "config".to_owned(),
            config_map: Some(ConfigMapVolumeSource {
                name: config_map_name(name),
                ..ConfigMapVolumeSource::default()
            }),
            ..Volume::default()
        },
        secret_volume("secrets-signing-key", &spec.signing_key_secret_ref.name),
    ];
    if let Some(r) = &spec.registration_shared_secret_ref {
        mounts.push(mount(
            "secrets-registration-shared-secret",
            "/etc/hs/secrets/registration-shared-secret",
            Some(&r.key),
            true,
        ));
        volumes.push(secret_volume("secrets-registration-shared-secret", &r.name));
    }
    if cluster {
        if let Some(tls) = &spec.cluster.mesh_tls_secret {
            mounts.push(mount(
                "secrets-mesh-tls",
                "/etc/hs/secrets/mesh-tls",
                None,
                true,
            ));
            volumes.push(secret_volume("secrets-mesh-tls", tls));
        } else if let Some(SecretKeyRef { name: secret, key }) =
            &spec.cluster.mesh_shared_secret_ref
        {
            mounts.push(mount(
                "secrets-mesh-shared-secret",
                "/etc/hs/secrets/mesh-shared-secret",
                Some(key),
                true,
            ));
            volumes.push(secret_volume("secrets-mesh-shared-secret", secret));
        }
    }
    if let Some(s) = &spec.session_secret_ref {
        mounts.push(mount(
            "secrets-session-secret",
            "/etc/hs/secrets/session-secret",
            Some(&s.key),
            true,
        ));
        volumes.push(secret_volume("secrets-session-secret", &s.name));
    }
    if embedded {
        mounts.push(mount("data", DATA_DIR, None, false));
    }
    volumes.push(Volume {
        name: "tmp".to_owned(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    });
    if spec.media.backend == MediaBackend::Local
        && let Some(claim) = &spec.media.local_claim
    {
        mounts.push(mount("media", "/var/lib/hs/media", None, false));
        volumes.push(claim_volume("media", claim));
    }
    if spec.media.backend == MediaBackend::S3
        && let Some(secret) = spec
            .media
            .s3
            .as_ref()
            .and_then(|s| s.secret_access_key_ref.as_ref())
    {
        mounts.push(mount(
            "secrets-media-s3",
            "/etc/hs/secrets/media-s3",
            None,
            true,
        ));
        volumes.push(secret_volume("secrets-media-s3", &secret.name));
    }
    mounts.push(mount("tmp", "/tmp", None, false));

    let mut ports = vec![
        ContainerPort {
            name: Some("client".to_owned()),
            container_port: CLIENT_PORT,
            protocol: Some("TCP".to_owned()),
            ..ContainerPort::default()
        },
        ContainerPort {
            name: Some("federation".to_owned()),
            container_port: FEDERATION_PORT,
            protocol: Some("TCP".to_owned()),
            ..ContainerPort::default()
        },
        ContainerPort {
            name: Some("metrics".to_owned()),
            container_port: METRICS_PORT,
            protocol: Some("TCP".to_owned()),
            ..ContainerPort::default()
        },
    ];
    if cluster {
        ports.push(ContainerPort {
            name: Some("mesh".to_owned()),
            container_port: spec.cluster.mesh_port,
            protocol: Some("TCP".to_owned()),
            ..ContainerPort::default()
        });
    }

    let args: Vec<String> = if embedded {
        ["serve", "--data-dir", DATA_DIR, "-c", CONFIG_FILE]
            .map(str::to_owned)
            .to_vec()
    } else {
        ["serve", "-c", CONFIG_FILE].map(str::to_owned).to_vec()
    };

    let container = Container {
        name: CONTAINER_NAME.to_owned(),
        image: Some(image_reference(spec)),
        image_pull_policy: Some(pull_policy(spec)),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            privileged: Some(false),
            read_only_root_filesystem: Some(true),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".to_owned()]),
                add: None,
            }),
            ..SecurityContext::default()
        }),
        args: Some(args),
        ports: Some(ports),
        env: Some(env_vars),
        volume_mounts: Some(mounts),
        startup_probe: Some(Probe {
            period_seconds: Some(5),
            failure_threshold: Some(30),
            ..http_probe("/health/live")
        }),
        liveness_probe: Some(Probe {
            initial_delay_seconds: Some(5),
            period_seconds: Some(10),
            timeout_seconds: Some(5),
            failure_threshold: Some(3),
            ..http_probe("/health/live")
        }),
        readiness_probe: Some(Probe {
            initial_delay_seconds: Some(5),
            period_seconds: Some(5),
            timeout_seconds: Some(5),
            failure_threshold: Some(3),
            ..http_probe("/health/ready")
        }),
        resources: Some(spec.resources.clone().unwrap_or_else(default_resources)),
        ..Container::default()
    };

    let affinity = (cluster && spec.cluster.anti_affinity != AntiAffinity::None).then(|| {
        let term = PodAffinityTerm {
            label_selector: Some(LabelSelector {
                match_labels: Some(selector_labels(name)),
                match_expressions: None,
            }),
            topology_key: "kubernetes.io/hostname".to_owned(),
            ..PodAffinityTerm::default()
        };
        let anti = if spec.cluster.anti_affinity == AntiAffinity::Hard {
            PodAntiAffinity {
                required_during_scheduling_ignored_during_execution: Some(vec![term]),
                ..PodAntiAffinity::default()
            }
        } else {
            PodAntiAffinity {
                preferred_during_scheduling_ignored_during_execution: Some(vec![
                    WeightedPodAffinityTerm {
                        weight: 100,
                        pod_affinity_term: term,
                    },
                ]),
                ..PodAntiAffinity::default()
            }
        };
        Affinity {
            pod_anti_affinity: Some(anti),
            ..Affinity::default()
        }
    });

    let claim_templates = embedded.then(|| {
        let embedded = spec.storage.embedded.as_ref();
        vec![PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("data".to_owned()),
                labels: Some(selector_labels(name)),
                ..ObjectMeta::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_owned()]),
                storage_class_name: embedded
                    .and_then(|e| e.storage_class_name.clone())
                    .filter(|c| !c.is_empty()),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".to_owned(),
                        Quantity(embedded.map_or_else(|| "10Gi".to_owned(), |e| e.size.clone())),
                    )])),
                    limits: None,
                }),
                ..PersistentVolumeClaimSpec::default()
            }),
            status: None,
        }]
    });

    StatefulSet {
        metadata: metadata(hs, name.to_owned()),
        spec: Some(StatefulSetSpec {
            service_name: Some(headless_name(name)),
            replicas: Some(desired_replicas(spec)),
            selector: LabelSelector {
                match_labels: Some(selector_labels(name)),
                match_expressions: None,
            },
            update_strategy: Some(StatefulSetUpdateStrategy {
                type_: Some("RollingUpdate".to_owned()),
                rolling_update: Some(RollingUpdateStatefulSetStrategy {
                    partition: Some(0),
                    max_unavailable: None,
                }),
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels(name)),
                    annotations: Some(BTreeMap::from([(
                        ANNOTATION_CONFIG_CHECKSUM.to_owned(),
                        config_checksum.to_owned(),
                    )])),
                    ..ObjectMeta::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(name.to_owned()),
                    security_context: Some(PodSecurityContext {
                        run_as_non_root: Some(true),
                        run_as_user: Some(65532),
                        run_as_group: Some(65532),
                        fs_group: Some(65532),
                        seccomp_profile: Some(SeccompProfile {
                            type_: "RuntimeDefault".to_owned(),
                            localhost_profile: None,
                        }),
                        ..PodSecurityContext::default()
                    }),
                    affinity,
                    termination_grace_period_seconds: cluster
                        .then_some(spec.cluster.termination_grace_period_seconds),
                    containers: vec![container],
                    volumes: Some(volumes),
                    ..PodSpec::default()
                }),
            },
            volume_claim_templates: claim_templates,
            ..StatefulSetSpec::default()
        }),
        status: None,
    }
}

/// The chart values that render the same objects as [`build`] for `hs`, as JSON (a valid
/// `values.yaml`). The release name must be `hs`'s name and the namespace its namespace; the
/// values pin `fullnameOverride` to the name and turn the chart's bridge operator off (the
/// operator's `Homeserver` does not deploy bridges; a `Bridge` is its own resource).
#[must_use]
pub fn chart_values(hs: &Homeserver) -> Value {
    let spec = &hs.spec;
    let name = hs.name_any();
    let cluster = is_cluster(spec);
    let (registry, repository) = match spec.image.repository.split_once('/') {
        Some((registry, rest)) => (registry.to_owned(), rest.to_owned()),
        None => ("docker.io".to_owned(), spec.image.repository.clone()),
    };
    let mut image = json!({
        "registry": registry,
        "repository": repository,
        "tag": spec.image.tag.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "latest".to_owned()),
    });
    if let Some(d) = spec.image.digest.as_deref().filter(|d| !d.is_empty()) {
        image["digest"] = json!(d);
    }
    if let Some(p) = spec.image.pull_policy.as_deref().filter(|p| !p.is_empty()) {
        image["pullPolicy"] = json!(p);
    }

    let mut values = json!({
        "fullnameOverride": name,
        "mode": if cluster { "cluster" } else { "singleNode" },
        "replicaCount": spec.replicas,
        "image": image,
        "serverName": spec.server_name,
        "publicBaseUrl": spec.public_base_url.clone().unwrap_or_default(),
        "bridges": {"enabled": false},
        "secrets": {
            "signingKey": {"existingSecret": spec.signing_key_secret_ref.name, "key": spec.signing_key_secret_ref.key},
        },
        "cluster": {
            "roomShards": spec.cluster.room_shards,
            "userShards": spec.cluster.user_shards,
            "heartbeatInterval": spec.cluster.heartbeat_interval,
            "leaseTtl": spec.cluster.lease_ttl,
            "clusterDomain": spec.cluster.cluster_domain,
            "terminationGracePeriodSeconds": spec.cluster.termination_grace_period_seconds,
            "mesh": {"port": spec.cluster.mesh_port},
        },
        "affinity": {"podAntiAffinity": {
            "enabled": spec.cluster.anti_affinity != AntiAffinity::None,
            "style": if spec.cluster.anti_affinity == AntiAffinity::Hard { "hard" } else { "soft" },
        }},
    });
    if let Some(r) = &spec.registration_shared_secret_ref {
        values["secrets"]["registrationSharedSecret"] =
            json!({"existingSecret": r.name, "key": r.key});
    }
    if let Some(s) = &spec.session_secret_ref {
        values["secrets"]["sessionSecret"] = json!({"existingSecret": s.name, "key": s.key});
    }
    if let Some(tls) = &spec.cluster.mesh_tls_secret {
        values["cluster"]["mesh"]["tls"] = json!({ "existingSecret": tls });
    } else if let Some(s) = &spec.cluster.mesh_shared_secret_ref {
        values["cluster"]["mesh"]["sharedSecret"] = json!({"existingSecret": s.name, "key": s.key});
    }
    match spec.storage.backend {
        StorageBackend::Embedded => {
            let e = spec.storage.embedded.as_ref();
            values["storage"] = json!({"backend": "embedded", "embedded": {
                "size": e.map_or_else(|| "10Gi".to_owned(), |e| e.size.clone()),
                "storageClassName": e.and_then(|e| e.storage_class_name.clone()).unwrap_or_default(),
            }});
        }
        StorageBackend::Postgres => {
            let pg = spec.storage.postgres.as_ref();
            let cnpg = pg
                .and_then(|p| p.cloud_native_pg_cluster.clone())
                .filter(|c| !c.is_empty());
            if let Some(cluster_name) = cnpg {
                values["cloudNativePG"] = json!({"enabled": true, "clusterName": cluster_name});
                values["storage"] = json!({"backend": "postgres"});
            } else {
                let mut postgres = json!({
                    "host": pg.and_then(|p| p.host.clone()).unwrap_or_default(),
                    "port": pg.and_then(|p| p.port).unwrap_or(5432),
                    "database": pg.and_then(|p| p.database.clone()).unwrap_or_else(|| "hs".to_owned()),
                    "user": pg.and_then(|p| p.user.clone()).unwrap_or_else(|| "hs".to_owned()),
                });
                if let Some(pw) = pg.and_then(|p| p.password_secret_ref.as_ref()) {
                    postgres["password"] = json!({"secret": pw.name, "secretKey": pw.key});
                }
                values["storage"] = json!({"backend": "postgres", "postgres": postgres});
            }
        }
        StorageBackend::Slatedb => {
            let s = spec.storage.slatedb.as_ref();
            values["storage"] = json!({"backend": "slatedb", "objectStorage": {
                "bucketUrl": s.map(|s| s.bucket_url.clone()).unwrap_or_default(),
                "shardCount": s.map_or(256, |s| s.shard_count),
            }});
        }
    }
    match spec.media.backend {
        MediaBackend::Local => {
            values["media"] = json!({"storage": {"backend": "local", "local": {
                "existingClaim": spec.media.local_claim.clone().unwrap_or_default(),
            }}});
        }
        MediaBackend::S3 => {
            let mut s3 = json!({});
            if let Some(b) = &spec.media.s3 {
                s3 = json!({
                    "bucket": b.bucket,
                    "region": b.region.clone().unwrap_or_default(),
                    "endpoint": b.endpoint.clone().unwrap_or_default(),
                    "accessKeyId": b.access_key_id.clone().unwrap_or_default(),
                });
                if let Some(secret) = &b.secret_access_key_ref {
                    s3["existingSecret"] = json!(secret.name);
                    s3["secretAccessKeyKey"] = json!(secret.key);
                }
            }
            values["media"] = json!({"storage": {"backend": "s3", "s3": s3}});
        }
    }
    if let Some(resources) = &spec.resources {
        values["resources"] = serde_json::to_value(resources).unwrap_or_default();
    }
    if !spec.extra_config.is_empty() {
        values["extraConfig"] = Value::Object(spec.extra_config.clone());
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::homeserver::testing::{cluster_homeserver, single_node_homeserver};

    #[test]
    fn embedded_storage_is_one_replica_whatever_replicas_says() {
        let mut hs = single_node_homeserver("hs");
        hs.spec.replicas = 5;
        assert_eq!(desired_replicas(&hs.spec), 1);
        let objects = build(&hs).unwrap();
        assert!(objects.pod_disruption_budget.is_none());
        let sts = objects.stateful_set.spec.unwrap();
        assert!(sts.volume_claim_templates.is_some());
        let pod = sts.template.spec.unwrap();
        assert!(pod.affinity.is_none());
        assert!(pod.termination_grace_period_seconds.is_none());
    }

    #[test]
    fn cluster_mode_has_a_budget_the_mesh_and_the_advertised_name() {
        let hs = cluster_homeserver("chat", 3);
        let objects = build(&hs).unwrap();
        assert!(objects.pod_disruption_budget.is_some());
        let sts = objects.stateful_set.spec.unwrap();
        let container = &sts.template.spec.as_ref().unwrap().containers[0];
        let env: BTreeMap<_, _> = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.as_str(), e.value.clone()))
            .collect();
        assert_eq!(
            env["HS__CLUSTER__MESH__ADVERTISE_ADDRESS"].as_deref(),
            Some("$(POD_NAME).chat-headless.matrix.svc.cluster.local")
        );
        assert_eq!(env["HS__CLUSTER__SINGLE_NODE"].as_deref(), Some("false"));
        let headless = objects.headless_service.spec.unwrap();
        assert!(
            headless
                .ports
                .unwrap()
                .iter()
                .any(|p| p.name.as_deref() == Some("mesh"))
        );
    }

    #[test]
    fn a_cluster_with_one_replica_has_no_budget() {
        let hs = cluster_homeserver("chat", 1);
        assert!(build(&hs).unwrap().pod_disruption_budget.is_none());
    }

    #[test]
    fn the_replica_id_is_the_mesh_address() {
        let hs = cluster_homeserver("chat", 3);
        assert_eq!(
            replica_id("chat-2", "chat", "matrix", &hs.spec),
            "chat-2.chat-headless.matrix.svc.cluster.local:8449"
        );
        assert_eq!(pod_ordinal("chat", "chat-2"), Some(2));
        assert_eq!(pod_ordinal("chat", "chat-x"), None);
        assert_eq!(pod_ordinal("chat", "chatter-1"), None);
    }

    #[test]
    fn validation_refuses_what_the_chart_refuses() {
        let mut hs = cluster_homeserver("chat", 3);
        hs.spec.cluster.mesh_tls_secret = None;
        assert!(validate(&hs.spec).unwrap_err().contains("mesh"));

        let mut hs = cluster_homeserver("chat", 3);
        hs.spec.media = crate::crds::MediaSpec::default();
        assert!(
            validate(&hs.spec)
                .unwrap_err()
                .contains("media.backend is local")
        );

        let mut hs = cluster_homeserver("chat", 3);
        hs.spec.storage.postgres = None;
        assert!(validate(&hs.spec).unwrap_err().contains("postgres"));

        let mut hs = single_node_homeserver("hs");
        hs.spec.server_name = String::new();
        assert!(build(&hs).is_err());
    }

    #[test]
    fn a_config_change_changes_the_template_hash_and_the_checksum() {
        let a = build(&cluster_homeserver("chat", 3)).unwrap();
        let mut hs = cluster_homeserver("chat", 3);
        hs.spec.cluster.lease_ttl = "20s".to_owned();
        let b = build(&hs).unwrap();
        assert_ne!(a.template_hash, b.template_hash);
        let mut hs = cluster_homeserver("chat", 5);
        hs.spec.replicas = 5;
        let c = build(&hs).unwrap();
        assert_eq!(
            a.template_hash, c.template_hash,
            "replicas are not the template"
        );
    }

    #[test]
    fn extra_config_wins_over_the_generated_keys() {
        let mut hs = single_node_homeserver("hs");
        hs.spec.extra_config.insert(
            "telemetry".to_owned(),
            json!({"logging": {"level": "debug"}}),
        );
        let config = config_value(&hs.spec);
        assert_eq!(config["telemetry"]["logging"]["level"], "debug");
    }

    #[test]
    fn the_shipped_examples_parse_and_build() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/operator/examples");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            let mut hs: Homeserver = serde_yaml_ng::from_str(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            hs.metadata.namespace = Some("matrix".to_owned());
            build(&hs).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            seen += 1;
        }
        assert!(seen >= 2);
    }

    #[test]
    fn chart_values_pick_the_mode_from_the_backend() {
        let values = chart_values(&single_node_homeserver("hs"));
        assert_eq!(values["mode"], "singleNode");
        assert_eq!(values["fullnameOverride"], "hs");
        let values = chart_values(&cluster_homeserver("chat", 3));
        assert_eq!(values["mode"], "cluster");
        assert_eq!(values["image"]["registry"], "ghcr.io");
        assert_eq!(values["image"]["repository"], "brandon-dacrib/myelin");
        assert_eq!(values["bridges"]["enabled"], false);
    }
}
