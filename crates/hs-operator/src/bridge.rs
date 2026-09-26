//! What a [`Bridge`] becomes: pure builders for the `PersistentVolumeClaim`, `Deployment` and
//! `Service` the operator applies for it, and [`status_from`], which reads the Deployment and its
//! pods back into the `Bridge`'s status. No I/O here; [`crate::controller`] does the applying.
//! `docs/rfcs/0017-the-server-deploys-its-own-bridges.md` section 4.4 is the specification.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, ContainerStatus, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PersistentVolumeClaimVolumeSource, Pod, PodSpec, PodTemplateSpec, Probe, ResourceRequirements,
    SecretVolumeSource, Service, ServicePort, ServiceSpec, TCPSocketAction, Volume, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    Condition, LabelSelector, ObjectMeta, OwnerReference, Time,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{Resource as _, ResourceExt as _};
use sha2::{Digest as _, Sha256};

use crate::crds::{Bridge, OperatorStatus, Phase};

/// The field manager (and the `app.kubernetes.io/managed-by` value) of everything the operator
/// applies.
pub const MANAGER: &str = "myelin-operator";
/// `app.kubernetes.io/name` of every bridge's objects.
pub const APP_NAME: &str = "myelin-bridge";
/// Label carrying the bridge type (`mautrix-whatsapp`).
pub const LABEL_BRIDGE_TYPE: &str = "myelin.dev/bridge-type";
/// Label carrying the appservice id, made label-safe by [`label_value`].
pub const LABEL_APPSERVICE_ID: &str = "myelin.dev/appservice-id";
/// Annotation carrying the exact appservice id (a label value cannot hold every id).
pub const ANNOTATION_APPSERVICE_ID: &str = "myelin.dev/appservice-id";
/// Pod-template annotation holding a hash of the `Bridge`'s spec, so a spec change rolls the pod.
pub const ANNOTATION_SPEC_HASH: &str = "myelin.dev/spec-hash";
/// The `condition.type` [`status_from`] reports.
pub const CONDITION_AVAILABLE: &str = "Available";
/// Name of the bridge's container port, which the Service targets.
pub const PORT_NAME: &str = "appservice";

/// Where the bridge's volume is mounted, and where its files land.
const DATA_DIR: &str = "/data";
/// Where the files Secret is mounted, read-only, for the init container.
const FILES_DIR: &str = "/files";

/// The init container's script: copy each file from the Secret into `/data` unless it is already
/// there. Never overwrite: a mautrix bridge rewrites its `config.yaml` on first start with secrets
/// it generated (`encryption.pickle_key`); replacing it would make its crypto store unreadable.
/// `/files/*` skips the Secret volume's dot-prefixed bookkeeping entries (`..data`), and `-f`
/// follows the symlinks the kubelet puts in their place.
const COPY_FILES_SCRIPT: &str = r#"set -eu
for f in /files/*; do
  [ -f "$f" ] || continue
  t="/data/$(basename "$f")"
  if [ -e "$t" ]; then
    echo "keeping $t"
  else
    cp "$f" "$t"
    echo "wrote $t"
  fi
done
"#;

/// Container waiting reasons that mean the bridge will not come up without someone changing
/// something: [`status_from`] reports them as [`Phase::Degraded`].
const DEGRADED_REASONS: &[&str] = &[
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "CrashLoopBackOff",
    "CreateContainerConfigError",
    "CreateContainerError",
];

/// The name of a bridge's volume claim: `<name>-data`.
#[must_use]
pub fn pvc_name(bridge_name: &str) -> String {
    format!("{bridge_name}-data")
}

/// Turns any string into a valid label value: at most 63 characters of `[A-Za-z0-9-_.]`, starting
/// and ending with an alphanumeric. Characters outside that set become `-`. The exact value goes
/// in an annotation where it matters ([`ANNOTATION_APPSERVICE_ID`]).
#[must_use]
pub fn label_value(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    mapped
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_owned()
}

/// The labels that select one bridge's pods: `app.kubernetes.io/name` and `/instance`.
#[must_use]
pub fn selector_labels(bridge_name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_owned(), APP_NAME.to_owned()),
        (
            "app.kubernetes.io/instance".to_owned(),
            bridge_name.to_owned(),
        ),
    ])
}

/// Every label the operator puts on a bridge's objects and pods.
#[must_use]
pub fn labels(bridge: &Bridge) -> BTreeMap<String, String> {
    let mut labels = selector_labels(&bridge.name_any());
    labels.insert(
        "app.kubernetes.io/managed-by".to_owned(),
        MANAGER.to_owned(),
    );
    labels.insert(
        LABEL_BRIDGE_TYPE.to_owned(),
        label_value(&bridge.spec.bridge_type),
    );
    labels.insert(
        LABEL_APPSERVICE_ID.to_owned(),
        label_value(&bridge.spec.appservice_id),
    );
    labels
}

/// The label selector (`key=value,...`) of one bridge's pods, for a `list` call.
#[must_use]
pub fn pod_selector(bridge_name: &str) -> String {
    let mut parts: Vec<String> = selector_labels(bridge_name)
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    parts.push(format!("app.kubernetes.io/managed-by={MANAGER}"));
    parts.join(",")
}

/// A short, stable hash of the `Bridge`'s spec (16 hex digits of SHA-256 over its JSON).
#[must_use]
pub fn spec_hash(bridge: &Bridge) -> String {
    // Serializing a plain data struct to JSON cannot fail; an empty input on the impossible path
    // still gives a stable (if uninformative) hash rather than a panic.
    let json = serde_json::to_vec(&bridge.spec).unwrap_or_default();
    let digest = Sha256::digest(&json);
    hex::encode(&digest[..8])
}

fn metadata(bridge: &Bridge, name: String) -> ObjectMeta {
    ObjectMeta {
        name: Some(name),
        namespace: bridge.namespace(),
        labels: Some(labels(bridge)),
        annotations: Some(BTreeMap::from([(
            ANNOTATION_APPSERVICE_ID.to_owned(),
            bridge.spec.appservice_id.clone(),
        )])),
        owner_references: owner_reference(bridge).map(|r| vec![r]),
        ..ObjectMeta::default()
    }
}

/// The controller owner reference to the `Bridge`, when it has a uid (it always does once read
/// from the API server), so deleting the `Bridge` deletes everything built from it.
fn owner_reference(bridge: &Bridge) -> Option<OwnerReference> {
    bridge.controller_owner_ref(&()).map(|r| OwnerReference {
        block_owner_deletion: Some(true),
        ..r
    })
}

/// The bridge's volume claim, `<name>-data`: ReadWriteOnce, the spec's size and StorageClass.
#[must_use]
pub fn desired_pvc(bridge: &Bridge) -> PersistentVolumeClaim {
    let storage = &bridge.spec.storage;
    PersistentVolumeClaim {
        metadata: metadata(bridge, pvc_name(&bridge.name_any())),
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_owned()]),
            storage_class_name: storage.storage_class_name.clone().filter(|s| !s.is_empty()),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_owned(),
                    Quantity(storage.size.clone()),
                )])),
                limits: None,
            }),
            ..PersistentVolumeClaimSpec::default()
        }),
        status: None,
    }
}

fn default_resources() -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("memory".to_owned(), Quantity("32Mi".to_owned())),
            ("cpu".to_owned(), Quantity("10m".to_owned())),
        ])),
        limits: Some(BTreeMap::from([(
            "memory".to_owned(),
            Quantity("512Mi".to_owned()),
        )])),
        claims: None,
    }
}

fn tcp_probe(initial_delay: i32, period: i32, failure_threshold: i32) -> Probe {
    Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::String(PORT_NAME.to_owned()),
            host: None,
        }),
        initial_delay_seconds: Some(initial_delay),
        period_seconds: Some(period),
        timeout_seconds: Some(5),
        failure_threshold: Some(failure_threshold),
        ..Probe::default()
    }
}

/// The bridge's Deployment, `<name>`: one replica, `Recreate` (two copies of a bridge must never
/// overlap), an init container that seeds `/data` from the files Secret without overwriting, and
/// the bridge container running the image's own entrypoint with `/data` on the claim.
///
/// The pod does not force `runAsNonRoot`: mautrix's `docker-run.sh` starts as root to `chown
/// /data` and then drops to UID 1337. It gets no service-account token; a bridge has no business
/// with the Kubernetes API.
#[must_use]
pub fn desired_deployment(bridge: &Bridge) -> Deployment {
    let name = bridge.name_any();
    let spec = &bridge.spec;
    let image = spec.image.reference();
    let pull_policy = spec.image.pull_policy.clone().filter(|p| !p.is_empty());
    // Includes `app.kubernetes.io/managed-by`, which the controller's pod watch selects on.
    let pod_labels = labels(bridge);

    let data_mount = VolumeMount {
        name: "data".to_owned(),
        mount_path: DATA_DIR.to_owned(),
        ..VolumeMount::default()
    };
    let init = Container {
        name: "files".to_owned(),
        image: Some(image.clone()),
        image_pull_policy: pull_policy.clone(),
        command: Some(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            COPY_FILES_SCRIPT.to_owned(),
        ]),
        volume_mounts: Some(vec![
            VolumeMount {
                name: "files".to_owned(),
                mount_path: FILES_DIR.to_owned(),
                read_only: Some(true),
                ..VolumeMount::default()
            },
            data_mount.clone(),
        ]),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("memory".to_owned(), Quantity("16Mi".to_owned())),
                ("cpu".to_owned(), Quantity("10m".to_owned())),
            ])),
            limits: Some(BTreeMap::from([(
                "memory".to_owned(),
                Quantity("64Mi".to_owned()),
            )])),
            claims: None,
        }),
        ..Container::default()
    };
    let main = Container {
        name: "bridge".to_owned(),
        image: Some(image),
        image_pull_policy: pull_policy,
        args: (!spec.args.is_empty()).then(|| spec.args.clone()),
        ports: Some(vec![ContainerPort {
            name: Some(PORT_NAME.to_owned()),
            container_port: spec.port,
            protocol: Some("TCP".to_owned()),
            ..ContainerPort::default()
        }]),
        readiness_probe: Some(tcp_probe(5, 5, 3)),
        // Generous: a bridge that is slow to answer while it syncs a large account is still
        // better left alone than restarted into the same sync.
        liveness_probe: Some(tcp_probe(60, 20, 6)),
        volume_mounts: Some(vec![data_mount]),
        resources: Some(spec.resources.clone().unwrap_or_else(default_resources)),
        ..Container::default()
    };

    let mut pod_annotations = BTreeMap::new();
    pod_annotations.insert(ANNOTATION_SPEC_HASH.to_owned(), spec_hash(bridge));
    pod_annotations.insert(
        ANNOTATION_APPSERVICE_ID.to_owned(),
        spec.appservice_id.clone(),
    );

    Deployment {
        metadata: metadata(bridge, name.clone()),
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_owned()),
                rolling_update: None,
            }),
            selector: LabelSelector {
                match_labels: Some(selector_labels(&name)),
                match_expressions: None,
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    annotations: Some(pod_annotations),
                    ..ObjectMeta::default()
                }),
                spec: Some(PodSpec {
                    automount_service_account_token: Some(false),
                    enable_service_links: Some(false),
                    init_containers: Some(vec![init]),
                    containers: vec![main],
                    volumes: Some(vec![
                        Volume {
                            name: "files".to_owned(),
                            secret: Some(SecretVolumeSource {
                                secret_name: Some(spec.files_secret.clone()),
                                // The files carry the bridge's tokens; the copies inherit this.
                                default_mode: Some(0o600),
                                ..SecretVolumeSource::default()
                            }),
                            ..Volume::default()
                        },
                        Volume {
                            name: "data".to_owned(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: pvc_name(&name),
                                read_only: None,
                            }),
                            ..Volume::default()
                        },
                    ]),
                    ..PodSpec::default()
                }),
            },
            ..DeploymentSpec::default()
        }),
        status: None,
    }
}

/// The bridge's Service, `<name>`: ClusterIP, `port` to the container's `appservice` port. The
/// homeserver reaches the bridge at `http://<name>.<namespace>.svc:<port>`.
#[must_use]
pub fn desired_service(bridge: &Bridge) -> Service {
    let name = bridge.name_any();
    Service {
        metadata: metadata(bridge, name.clone()),
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_owned()),
            selector: Some(selector_labels(&name)),
            ports: Some(vec![ServicePort {
                name: Some(PORT_NAME.to_owned()),
                port: bridge.spec.port,
                target_port: Some(IntOrString::String(PORT_NAME.to_owned())),
                protocol: Some("TCP".to_owned()),
                ..ServicePort::default()
            }]),
            ..ServiceSpec::default()
        }),
        status: None,
    }
}

/// The `Bridge`'s status, from its Deployment (if it exists yet) and that Deployment's pods (the
/// caller lists them by [`pod_selector`]).
///
/// - [`Phase::Ready`] when the Deployment has a ready replica.
/// - [`Phase::Degraded`] when a container or init container of a pod waits for a reason that will
///   not fix itself (an image that cannot be pulled, a crash loop, a missing Secret).
/// - [`Phase::Pending`] otherwise, with what it is waiting for.
///
/// One `Available` condition carries the reason and message; its `lastTransitionTime` is kept
/// from the current status while its `status` does not change, so an unchanged state compares
/// equal and is not written again.
#[must_use]
pub fn status_from(
    bridge: &Bridge,
    deployment: Option<&Deployment>,
    pods: &[Pod],
) -> OperatorStatus {
    status_at(
        bridge,
        deployment,
        pods,
        Time(k8s_openapi::chrono::Utc::now()),
    )
}

/// [`status_from`] with the clock supplied, for tests.
fn status_at(
    bridge: &Bridge,
    deployment: Option<&Deployment>,
    pods: &[Pod],
    now: Time,
) -> OperatorStatus {
    let generation = bridge.metadata.generation;
    let ready_replicas = deployment
        .and_then(|d| d.status.as_ref())
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);

    let (phase, reason, message) = if deployment.is_none() {
        (
            Phase::Pending,
            "DeploymentMissing".to_owned(),
            "waiting for the Deployment to be created".to_owned(),
        )
    } else if ready_replicas >= 1 {
        (
            Phase::Ready,
            "Ready".to_owned(),
            format!(
                "the bridge is accepting connections on port {}",
                bridge.spec.port
            ),
        )
    } else if let Some((reason, message)) = degraded_reason(pods) {
        (Phase::Degraded, reason, message)
    } else {
        let (reason, message) = pending_reason(pods);
        (Phase::Pending, reason, message)
    };

    let condition_status = if phase == Phase::Ready {
        "True"
    } else {
        "False"
    };
    let previous = bridge
        .status
        .as_ref()
        .and_then(|s| s.conditions.iter().find(|c| c.type_ == CONDITION_AVAILABLE));
    let last_transition_time = match previous {
        Some(c) if c.status == condition_status => c.last_transition_time.clone(),
        _ => now,
    };

    OperatorStatus {
        phase,
        observed_generation: generation,
        ready_replicas: Some(ready_replicas),
        conditions: vec![Condition {
            type_: CONDITION_AVAILABLE.to_owned(),
            status: condition_status.to_owned(),
            reason,
            message,
            observed_generation: generation,
            last_transition_time,
        }],
    }
}

fn all_container_statuses(pod: &Pod) -> impl Iterator<Item = &ContainerStatus> {
    let status = pod.status.as_ref();
    let init = status
        .and_then(|s| s.init_container_statuses.as_deref())
        .unwrap_or_default();
    let main = status
        .and_then(|s| s.container_statuses.as_deref())
        .unwrap_or_default();
    init.iter().chain(main.iter())
}

fn waiting(status: &ContainerStatus) -> Option<(&str, Option<&str>)> {
    let waiting = status.state.as_ref()?.waiting.as_ref()?;
    let reason = waiting.reason.as_deref()?;
    Some((reason, waiting.message.as_deref()))
}

fn degraded_reason(pods: &[Pod]) -> Option<(String, String)> {
    pods.iter().flat_map(all_container_statuses).find_map(|cs| {
        let (reason, message) = waiting(cs)?;
        DEGRADED_REASONS.contains(&reason).then(|| {
            let text = match message {
                Some(m) if !m.is_empty() => format!("{}: {reason}: {m}", cs.name),
                _ => format!("{}: {reason}", cs.name),
            };
            (reason.to_owned(), text)
        })
    })
}

fn pending_reason(pods: &[Pod]) -> (String, String) {
    if let Some((reason, _)) = pods
        .iter()
        .flat_map(all_container_statuses)
        .find_map(waiting)
    {
        return (
            "WaitingForPod".to_owned(),
            format!("waiting for the pod: {reason}"),
        );
    }
    // Not scheduled yet: the scheduler's message says why (an unbound claim, no room on a node).
    let unscheduled = pods.iter().find_map(|pod| {
        pod.status
            .as_ref()?
            .conditions
            .as_ref()?
            .iter()
            .find(|c| c.type_ == "PodScheduled" && c.status == "False")
            .and_then(|c| c.message.clone())
    });
    match unscheduled {
        Some(message) => (
            "WaitingForPod".to_owned(),
            format!("waiting for the pod: {message}"),
        ),
        None => ("WaitingForPod".to_owned(), "waiting for the pod".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::{BridgeSpec, BridgeStorage, ImageSpec};
    use k8s_openapi::api::apps::v1::DeploymentStatus;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateWaiting, PodCondition, PodStatus,
    };

    fn bridge() -> Bridge {
        let mut b = Bridge::new(
            "bridge-1a2b3c4d",
            BridgeSpec {
                bridge_type: "mautrix-whatsapp".to_owned(),
                appservice_id: "whatsapp-alice=5fx".to_owned(),
                image: ImageSpec {
                    repository: "dock.mau.dev/mautrix/whatsapp".to_owned(),
                    tag: Some("v0.12.0".to_owned()),
                    digest: None,
                    pull_policy: None,
                },
                port: 29318,
                files_secret: "bridge-1a2b3c4d-files".to_owned(),
                args: Vec::new(),
                storage: BridgeStorage {
                    size: "2Gi".to_owned(),
                    storage_class_name: Some("longhorn".to_owned()),
                },
                resources: None,
            },
        );
        b.metadata.namespace = Some("myelin".to_owned());
        b.metadata.uid = Some("1234-uid".to_owned());
        b.metadata.generation = Some(3);
        b
    }

    fn assert_owned(meta: &ObjectMeta) {
        let owners = meta.owner_references.as_ref().expect("owner references");
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].kind, "Bridge");
        assert_eq!(owners[0].name, "bridge-1a2b3c4d");
        assert_eq!(owners[0].uid, "1234-uid");
        assert_eq!(owners[0].controller, Some(true));
        assert_eq!(meta.namespace.as_deref(), Some("myelin"));
    }

    #[test]
    fn label_value_makes_any_id_label_safe() {
        assert_eq!(label_value("whatsapp-alice"), "whatsapp-alice");
        assert_eq!(label_value("whatsapp-alice=5fx"), "whatsapp-alice-5fx");
        assert_eq!(label_value("=weird="), "weird");
        let long = "a".repeat(80);
        assert_eq!(label_value(&long).len(), 63);
        assert_eq!(label_value(&format!("{}-b", "a".repeat(62))).len(), 62);
    }

    #[test]
    fn pvc_is_named_sized_and_owned() {
        let pvc = desired_pvc(&bridge());
        assert_eq!(pvc.metadata.name.as_deref(), Some("bridge-1a2b3c4d-data"));
        assert_owned(&pvc.metadata);
        let spec = pvc.spec.unwrap();
        assert_eq!(spec.access_modes.unwrap(), vec!["ReadWriteOnce"]);
        assert_eq!(spec.storage_class_name.as_deref(), Some("longhorn"));
        assert_eq!(
            spec.resources.unwrap().requests.unwrap()["storage"],
            Quantity("2Gi".to_owned())
        );
    }

    #[test]
    fn pvc_without_a_storage_class_uses_the_default() {
        let mut b = bridge();
        b.spec.storage.storage_class_name = None;
        assert!(desired_pvc(&b).spec.unwrap().storage_class_name.is_none());
    }

    #[test]
    fn deployment_is_one_replica_recreate_with_the_expected_labels() {
        let d = desired_deployment(&bridge());
        assert_eq!(d.metadata.name.as_deref(), Some("bridge-1a2b3c4d"));
        assert_owned(&d.metadata);
        let labels = d.metadata.labels.clone().unwrap();
        assert_eq!(labels["app.kubernetes.io/name"], "myelin-bridge");
        assert_eq!(labels["app.kubernetes.io/instance"], "bridge-1a2b3c4d");
        assert_eq!(labels["app.kubernetes.io/managed-by"], "myelin-operator");
        assert_eq!(labels[LABEL_BRIDGE_TYPE], "mautrix-whatsapp");
        assert_eq!(labels[LABEL_APPSERVICE_ID], "whatsapp-alice-5fx");
        assert_eq!(
            d.metadata.annotations.as_ref().unwrap()[ANNOTATION_APPSERVICE_ID],
            "whatsapp-alice=5fx"
        );
        let spec = d.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.strategy.unwrap().type_.as_deref(), Some("Recreate"));
        let selector = spec.selector.match_labels.unwrap();
        let template_labels = spec.template.metadata.as_ref().unwrap().labels.clone();
        for (k, v) in &selector {
            assert_eq!(template_labels.as_ref().unwrap().get(k), Some(v));
        }
        assert_eq!(
            template_labels.unwrap()["app.kubernetes.io/managed-by"],
            "myelin-operator"
        );
    }

    #[test]
    fn init_container_copies_files_only_when_missing() {
        let d = desired_deployment(&bridge());
        let pod = d.spec.unwrap().template.spec.unwrap();
        let init = &pod.init_containers.unwrap()[0];
        assert_eq!(init.name, "files");
        assert_eq!(
            init.image.as_deref(),
            Some("dock.mau.dev/mautrix/whatsapp:v0.12.0")
        );
        let command = init.command.clone().unwrap();
        assert_eq!(command[..2], ["sh".to_owned(), "-c".to_owned()]);
        assert!(command[2].contains("if [ -e \"$t\" ]"));
        assert!(command[2].contains("cp \"$f\" \"$t\""));
        let mounts = init.volume_mounts.clone().unwrap();
        assert!(
            mounts
                .iter()
                .any(|m| m.mount_path == "/files" && m.read_only == Some(true))
        );
        assert!(mounts.iter().any(|m| m.mount_path == "/data"));
        let files = pod
            .volumes
            .unwrap()
            .into_iter()
            .find(|v| v.name == "files")
            .unwrap();
        assert_eq!(
            files.secret.unwrap().secret_name.as_deref(),
            Some("bridge-1a2b3c4d-files")
        );
    }

    #[test]
    fn the_copy_script_never_overwrites_and_skips_secret_bookkeeping() {
        // Run the real script against a real directory pair when a POSIX shell is present.
        let sh = "/bin/sh";
        if !std::path::Path::new(sh).exists() {
            return;
        }
        let root = std::env::temp_dir().join(format!("hs-operator-copy-{}", std::process::id()));
        let files = root.join("files");
        let data = root.join("data");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(files.join("config.yaml"), "fresh").unwrap();
        std::fs::write(files.join("registration.yaml"), "reg").unwrap();
        std::fs::create_dir_all(files.join("..data")).unwrap();
        std::fs::write(data.join("config.yaml"), "rewritten by the bridge").unwrap();
        let script = COPY_FILES_SCRIPT
            .replace("/files/", &format!("{}/", files.display()))
            .replace("/data/", &format!("{}/", data.display()));
        let status = std::process::Command::new(sh)
            .args(["-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(data.join("config.yaml")).unwrap(),
            "rewritten by the bridge"
        );
        assert_eq!(
            std::fs::read_to_string(data.join("registration.yaml")).unwrap(),
            "reg"
        );
        assert!(!data.join("..data").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn main_container_runs_the_images_entrypoint_with_probes_and_data() {
        let d = desired_deployment(&bridge());
        let pod = d.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert!(pod.security_context.is_none(), "no forced runAsNonRoot");
        let c = &pod.containers[0];
        assert!(c.command.is_none());
        assert!(c.args.is_none());
        let port = &c.ports.as_ref().unwrap()[0];
        assert_eq!(port.container_port, 29318);
        assert_eq!(port.name.as_deref(), Some("appservice"));
        let readiness = c.readiness_probe.as_ref().unwrap();
        assert!(readiness.tcp_socket.is_some());
        assert_eq!(readiness.period_seconds, Some(5));
        assert!(c.liveness_probe.as_ref().unwrap().tcp_socket.is_some());
        assert_eq!(c.volume_mounts.as_ref().unwrap()[0].mount_path, "/data");
        let resources = c.resources.clone().unwrap();
        assert_eq!(
            resources.requests.unwrap()["memory"],
            Quantity("32Mi".to_owned())
        );
        assert_eq!(
            resources.limits.unwrap()["memory"],
            Quantity("512Mi".to_owned())
        );
        let claim = pod
            .volumes
            .unwrap()
            .into_iter()
            .find(|v| v.name == "data")
            .unwrap();
        assert_eq!(
            claim.persistent_volume_claim.unwrap().claim_name,
            "bridge-1a2b3c4d-data"
        );
    }

    #[test]
    fn args_and_resources_come_from_the_spec() {
        let mut b = bridge();
        b.spec.args = vec!["--listen-port".to_owned(), "9898".to_owned()];
        b.spec.resources = Some(ResourceRequirements {
            requests: Some(BTreeMap::from([(
                "memory".to_owned(),
                Quantity("64Mi".to_owned()),
            )])),
            ..ResourceRequirements::default()
        });
        let d = desired_deployment(&b);
        let c = &d.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(c.args.as_deref().unwrap(), ["--listen-port", "9898"]);
        assert!(c.resources.as_ref().unwrap().limits.is_none());
    }

    #[test]
    fn a_spec_change_changes_the_pod_template_hash() {
        let a = desired_deployment(&bridge());
        let mut changed = bridge();
        changed.spec.image.tag = Some("v0.13.0".to_owned());
        let b = desired_deployment(&changed);
        let hash = |d: &Deployment| {
            d.spec
                .as_ref()
                .unwrap()
                .template
                .metadata
                .as_ref()
                .unwrap()
                .annotations
                .as_ref()
                .unwrap()[ANNOTATION_SPEC_HASH]
                .clone()
        };
        assert_ne!(hash(&a), hash(&b));
        assert_eq!(hash(&a), hash(&desired_deployment(&bridge())));
    }

    #[test]
    fn service_targets_the_named_port() {
        let s = desired_service(&bridge());
        assert_eq!(s.metadata.name.as_deref(), Some("bridge-1a2b3c4d"));
        assert_owned(&s.metadata);
        let spec = s.spec.unwrap();
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));
        assert_eq!(spec.selector.unwrap(), selector_labels("bridge-1a2b3c4d"));
        let port = &spec.ports.unwrap()[0];
        assert_eq!(port.port, 29318);
        assert_eq!(
            port.target_port,
            Some(IntOrString::String("appservice".to_owned()))
        );
    }

    #[test]
    fn a_bridge_without_a_uid_gets_no_owner_reference() {
        let mut b = bridge();
        b.metadata.uid = None;
        assert!(desired_service(&b).metadata.owner_references.is_none());
    }

    fn deployment_with_ready(ready: i32) -> Deployment {
        Deployment {
            status: Some(DeploymentStatus {
                ready_replicas: Some(ready),
                ..DeploymentStatus::default()
            }),
            ..Deployment::default()
        }
    }

    fn pod_waiting(init: bool, reason: &str, message: Option<&str>) -> Pod {
        let status = ContainerStatus {
            name: if init { "files" } else { "bridge" }.to_owned(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.to_owned()),
                    message: message.map(str::to_owned),
                }),
                ..ContainerState::default()
            }),
            ..ContainerStatus::default()
        };
        let mut pod_status = PodStatus::default();
        if init {
            pod_status.init_container_statuses = Some(vec![status]);
        } else {
            pod_status.container_statuses = Some(vec![status]);
        }
        Pod {
            status: Some(pod_status),
            ..Pod::default()
        }
    }

    fn t(secs: i64) -> Time {
        Time(k8s_openapi::chrono::DateTime::from_timestamp(secs, 0).unwrap())
    }

    #[test]
    fn ready_when_a_replica_is_ready() {
        let s = status_at(&bridge(), Some(&deployment_with_ready(1)), &[], t(10));
        assert_eq!(s.phase, Phase::Ready);
        assert_eq!(s.ready_replicas, Some(1));
        assert_eq!(s.observed_generation, Some(3));
        let c = &s.conditions[0];
        assert_eq!(c.type_, "Available");
        assert_eq!(c.status, "True");
        assert_eq!(c.reason, "Ready");
        assert_eq!(c.observed_generation, Some(3));
    }

    #[test]
    fn pending_without_a_deployment() {
        let s = status_at(&bridge(), None, &[], t(10));
        assert_eq!(s.phase, Phase::Pending);
        assert_eq!(s.conditions[0].status, "False");
        assert_eq!(s.conditions[0].reason, "DeploymentMissing");
    }

    #[test]
    fn degraded_on_an_image_pull_failure_in_the_init_container() {
        let pods = [pod_waiting(
            true,
            "ImagePullBackOff",
            Some("Back-off pulling image"),
        )];
        let s = status_at(&bridge(), Some(&deployment_with_ready(0)), &pods, t(10));
        assert_eq!(s.phase, Phase::Degraded);
        assert_eq!(s.conditions[0].reason, "ImagePullBackOff");
        assert_eq!(
            s.conditions[0].message,
            "files: ImagePullBackOff: Back-off pulling image"
        );
    }

    #[test]
    fn degraded_on_a_crash_loop_and_a_missing_secret() {
        for reason in [
            "CrashLoopBackOff",
            "CreateContainerConfigError",
            "ErrImagePull",
        ] {
            let pods = [pod_waiting(false, reason, None)];
            let s = status_at(&bridge(), Some(&deployment_with_ready(0)), &pods, t(10));
            assert_eq!(s.phase, Phase::Degraded, "{reason}");
            assert_eq!(s.conditions[0].message, format!("bridge: {reason}"));
        }
    }

    #[test]
    fn pending_with_the_waiting_reason() {
        let pods = [pod_waiting(false, "ContainerCreating", None)];
        let s = status_at(&bridge(), Some(&deployment_with_ready(0)), &pods, t(10));
        assert_eq!(s.phase, Phase::Pending);
        assert_eq!(
            s.conditions[0].message,
            "waiting for the pod: ContainerCreating"
        );
    }

    #[test]
    fn pending_with_the_schedulers_message_or_plainly() {
        let unscheduled = Pod {
            status: Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "PodScheduled".to_owned(),
                    status: "False".to_owned(),
                    message: Some("pod has unbound immediate PersistentVolumeClaims".to_owned()),
                    ..PodCondition::default()
                }]),
                ..PodStatus::default()
            }),
            ..Pod::default()
        };
        let s = status_at(
            &bridge(),
            Some(&deployment_with_ready(0)),
            &[unscheduled],
            t(10),
        );
        assert!(s.conditions[0].message.contains("unbound"));
        let s = status_at(&bridge(), Some(&deployment_with_ready(0)), &[], t(10));
        assert_eq!(s.conditions[0].message, "waiting for the pod");
    }

    #[test]
    fn transition_time_is_kept_while_the_condition_status_holds() {
        let mut b = bridge();
        b.status = Some(status_at(&b, Some(&deployment_with_ready(0)), &[], t(10)));
        // Same False status, different reason: time kept, so an unchanged state compares equal.
        let pods = [pod_waiting(false, "ContainerCreating", None)];
        let s = status_at(&b, Some(&deployment_with_ready(0)), &pods, t(20));
        assert_eq!(s.conditions[0].last_transition_time, t(10));
        let again = status_at(&b, Some(&deployment_with_ready(0)), &[], t(30));
        assert_eq!(Some(&again), b.status.as_ref());
        // False -> True: a new transition.
        let s = status_at(&b, Some(&deployment_with_ready(1)), &[], t(40));
        assert_eq!(s.conditions[0].last_transition_time, t(40));
    }

    #[test]
    fn applied_objects_carry_their_type_for_server_side_apply() {
        // Server-side apply needs apiVersion and kind in the body.
        let b = bridge();
        let objects = [
            serde_json::to_value(desired_pvc(&b)).unwrap(),
            serde_json::to_value(desired_deployment(&b)).unwrap(),
            serde_json::to_value(desired_service(&b)).unwrap(),
        ];
        let kinds: Vec<_> = objects
            .iter()
            .map(|o| (o["apiVersion"].clone(), o["kind"].clone()))
            .collect();
        assert_eq!(kinds[0], ("v1".into(), "PersistentVolumeClaim".into()));
        assert_eq!(kinds[1], ("apps/v1".into(), "Deployment".into()));
        assert_eq!(kinds[2], ("v1".into(), "Service".into()));
    }

    #[test]
    fn pod_selector_names_the_bridge_and_the_manager() {
        let sel = pod_selector("bridge-1a2b3c4d");
        assert!(sel.contains("app.kubernetes.io/instance=bridge-1a2b3c4d"));
        assert!(sel.contains("app.kubernetes.io/name=myelin-bridge"));
        assert!(sel.contains("app.kubernetes.io/managed-by=myelin-operator"));
    }
}
