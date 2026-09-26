//! The client the homeserver's bridge manager (`crates/hs-bridges`) deploys bridges with: it
//! writes a `Bridge` and its files `Secret`, reads the `Bridge`'s status back, and deletes it.
//! The operator ([`crate::controller`]) does the rest. `docs/rfcs/0017-the-server-deploys-its-own-bridges.md`
//! sections 4.3 to 4.5.
//!
//! [`KubeBridgeClient::manifest_yaml`] renders the same two objects as YAML, for a bridge that
//! is to run on another cluster running the operator.

use std::collections::BTreeMap;

use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::{Api, Resource as _};
use serde::Serialize;

use crate::bridge::{
    ANNOTATION_APPSERVICE_ID, CONDITION_AVAILABLE, LABEL_APPSERVICE_ID, LABEL_BRIDGE_TYPE,
    label_value,
};
use crate::crds::{Bridge, BridgeSpec, BridgeStorage, ImageSpec, Phase};

/// The field manager the homeserver applies `Bridge`s and Secrets with.
pub const FIELD_MANAGER: &str = "myelin-homeserver";

/// One bridge process to run: what the manager asks for, and what becomes a `Bridge` named
/// [`name`](Self::name) and a Secret named `<name>-files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInstanceSpec {
    /// The name of the `Bridge`, and so of its Deployment and Service: a DNS-1035 label
    /// (lowercase letters, digits and `-`, starting with a letter, at most 63 characters), such
    /// as `bridge-1a2b3c4d`.
    pub name: String,
    /// Extra labels for the `Bridge` and its Secret (the manager's own bookkeeping).
    pub labels: BTreeMap<String, String>,
    /// The bridge type from the catalogue (`mautrix-whatsapp`).
    pub bridge_type: String,
    /// The appservice registration id this process answers for (`whatsapp-alice`).
    pub appservice_id: String,
    /// The bridge's image.
    pub image: ImageSpec,
    /// The port the bridge listens on for appservice transactions.
    pub port: i32,
    /// Arguments to the image's entrypoint; empty for its default command.
    pub args: Vec<String>,
    /// File name to contents. Each becomes a file in the bridge's `/data` on its first start
    /// (and is never overwritten after), through the Secret `<name>-files`.
    pub files: BTreeMap<String, String>,
    /// The volume size, a Kubernetes quantity; `1Gi` when unset.
    pub storage_size: Option<String>,
}

/// What the cluster says about one `Bridge`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BridgeInstanceStatus {
    /// The `Bridge`'s name.
    pub name: String,
    /// Its namespace.
    pub namespace: String,
    /// The operator's phase: `Pending` until it has run it, `Ready`, or `Degraded`.
    pub phase: Phase,
    /// Whether the bridge's pod is up and accepting connections on its port.
    pub ready: bool,
    /// The operator's explanation (the `Available` condition's message), when it has written one.
    pub message: Option<String>,
    /// The image reference it runs.
    pub image: String,
    /// Where the homeserver reaches it: `http://<name>.<namespace>.svc:<port>`, the registration's
    /// `url`.
    pub service_url: String,
}

/// What [`KubeBridgeClient`] can fail with.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    /// A call to the Kubernetes API failed (including a missing permission: the chart's
    /// `bridges-rbac.yaml` grants what is needed).
    #[error("kubernetes API: {0}")]
    Kube(#[from] kube::Error),
    /// No Kubernetes client configuration could be found (not in a pod, no kubeconfig).
    #[error("no kubernetes configuration: {0}")]
    Config(String),
    /// The spec cannot be deployed as given.
    #[error("invalid bridge instance: {0}")]
    Invalid(String),
}

/// Deploys, inspects and removes bridges in one namespace through the Kubernetes API.
#[derive(Clone)]
pub struct KubeBridgeClient {
    client: kube::Client,
    namespace: String,
}

impl std::fmt::Debug for KubeBridgeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubeBridgeClient")
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl KubeBridgeClient {
    /// A client for `namespace` over an existing Kubernetes client.
    pub fn new(client: kube::Client, namespace: impl Into<String>) -> Self {
        Self {
            client,
            namespace: namespace.into(),
        }
    }

    /// A client for `namespace` from the ambient configuration: the pod's service account inside
    /// a cluster, else the local kubeconfig.
    ///
    /// # Errors
    /// [`DeployError::Config`] when there is neither.
    pub async fn in_cluster(namespace: impl Into<String>) -> Result<Self, DeployError> {
        let client = crate::connect()
            .await
            .map_err(|e| DeployError::Config(e.to_string()))?;
        Ok(Self::new(client, namespace))
    }

    /// The namespace bridges are deployed in.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Where a bridge named `name` listening on `port` is reached from inside the cluster:
    /// `http://{name}.{namespace}.svc:{port}`.
    #[must_use]
    pub fn service_url(&self, name: &str, port: i32) -> String {
        service_url(name, &self.namespace, port)
    }

    /// Creates or updates the `Bridge` and then its files Secret `<name>-files` (owned by the
    /// `Bridge`, so deleting the `Bridge` deletes it), both by server-side apply. Idempotent.
    /// Returns the `Bridge`'s status as it stands, which for a new one is `Pending`.
    ///
    /// # Errors
    /// [`DeployError::Invalid`] for a spec that cannot be deployed (see [`validate`]), else
    /// [`DeployError::Kube`].
    pub async fn apply(
        &self,
        spec: &BridgeInstanceSpec,
    ) -> Result<BridgeInstanceStatus, DeployError> {
        validate(spec)?;
        let params = PatchParams::apply(FIELD_MANAGER).force();
        let bridges: Api<Bridge> = Api::namespaced(self.client.clone(), &self.namespace);
        let applied = bridges
            .patch(
                &spec.name,
                &params,
                &Patch::Apply(&bridge_object(spec, &self.namespace)),
            )
            .await?;

        let mut secret = files_secret(spec, &self.namespace);
        // `data`, not `stringData`: server-side apply does not track `stringData` keys, so a
        // file dropped from the spec would never leave the Secret.
        secret.data = Some(
            std::mem::take(&mut secret.string_data)
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, ByteString(v.into_bytes())))
                .collect(),
        );
        secret.metadata.owner_references = applied.controller_owner_ref(&()).map(|r| vec![r]);
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        secrets
            .patch(
                &files_secret_name(&spec.name),
                &params,
                &Patch::Apply(&secret),
            )
            .await?;

        Ok(instance_status(&applied, &self.namespace))
    }

    /// The `Bridge` named `name` and what the operator last said about it, or `None` when there
    /// is no such `Bridge`.
    ///
    /// # Errors
    /// [`DeployError::Kube`].
    pub async fn status(&self, name: &str) -> Result<Option<BridgeInstanceStatus>, DeployError> {
        let bridges: Api<Bridge> = Api::namespaced(self.client.clone(), &self.namespace);
        Ok(bridges
            .get_opt(name)
            .await?
            .map(|b| instance_status(&b, &self.namespace)))
    }

    /// Deletes the `Bridge` named `name` (its Deployment, Service, volume and Secret go with it,
    /// by owner reference) and its files Secret. A `Bridge` that does not exist is not an error.
    ///
    /// # Errors
    /// [`DeployError::Kube`].
    pub async fn delete(&self, name: &str) -> Result<(), DeployError> {
        let bridges: Api<Bridge> = Api::namespaced(self.client.clone(), &self.namespace);
        ignore_not_found(bridges.delete(name, &DeleteParams::background()).await)?;
        // Normally already on its way out by owner reference; deleted by name as well in case it
        // was written before its `Bridge` had a uid to point at.
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        ignore_not_found(
            secrets
                .delete(&files_secret_name(name), &DeleteParams::background())
                .await,
        )?;
        Ok(())
    }

    /// The Secret (with `stringData`) and the `Bridge`, as a two-document YAML stream someone
    /// can `kubectl apply -f` in `namespace` on another cluster running the operator. Pure: it
    /// talks to no cluster. The Secret carries the bridge's tokens, so the text does too.
    #[must_use]
    pub fn manifest_yaml(spec: &BridgeInstanceSpec, namespace: &str) -> String {
        let secret = files_secret(spec, namespace);
        let bridge = bridge_object(spec, namespace);
        // Serializing these plain objects cannot fail; an empty document on the impossible path
        // is better than a panic in a request handler.
        let secret_yaml = serde_yaml_ng::to_string(&secret).unwrap_or_default();
        let bridge_yaml = serde_yaml_ng::to_string(&bridge).unwrap_or_default();
        format!("---\n{secret_yaml}---\n{bridge_yaml}")
    }
}

/// Checks a spec can become Kubernetes objects: the name is a DNS-1035 label, the port is a
/// port, every file name is a valid Secret key, and there is an appservice id and a bridge type.
///
/// # Errors
/// [`DeployError::Invalid`] saying which rule is broken.
pub fn validate(spec: &BridgeInstanceSpec) -> Result<(), DeployError> {
    let name = &spec.name;
    let dns_label = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !dns_label {
        return Err(DeployError::Invalid(format!(
            "name {name:?} is not a DNS-1035 label (lowercase letters, digits and '-', starting \
             with a letter, at most 63 characters)"
        )));
    }
    if !(1..=65535).contains(&spec.port) {
        return Err(DeployError::Invalid(format!(
            "port {} is out of range",
            spec.port
        )));
    }
    if spec.image.repository.is_empty() {
        return Err(DeployError::Invalid(
            "the image has no repository".to_owned(),
        ));
    }
    if spec.bridge_type.is_empty() || spec.appservice_id.is_empty() {
        return Err(DeployError::Invalid(
            "bridge type and appservice id are required".to_owned(),
        ));
    }
    for file in spec.files.keys() {
        let valid = !file.is_empty()
            && file.len() <= 253
            && file != "."
            && file != ".."
            && file
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !valid {
            return Err(DeployError::Invalid(format!(
                "file name {file:?} is not a valid Secret key"
            )));
        }
    }
    Ok(())
}

/// The name of a bridge's files Secret: `<name>-files`.
#[must_use]
pub fn files_secret_name(name: &str) -> String {
    format!("{name}-files")
}

fn service_url(name: &str, namespace: &str, port: i32) -> String {
    format!("http://{name}.{namespace}.svc:{port}")
}

fn object_labels(spec: &BridgeInstanceSpec) -> BTreeMap<String, String> {
    let mut labels = spec.labels.clone();
    labels.insert(LABEL_BRIDGE_TYPE.to_owned(), label_value(&spec.bridge_type));
    labels.insert(
        LABEL_APPSERVICE_ID.to_owned(),
        label_value(&spec.appservice_id),
    );
    labels
}

fn object_meta(name: String, spec: &BridgeInstanceSpec, namespace: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name),
        namespace: Some(namespace.to_owned()),
        labels: Some(object_labels(spec)),
        annotations: Some(BTreeMap::from([(
            ANNOTATION_APPSERVICE_ID.to_owned(),
            spec.appservice_id.clone(),
        )])),
        ..ObjectMeta::default()
    }
}

fn bridge_object(spec: &BridgeInstanceSpec, namespace: &str) -> Bridge {
    let mut bridge = Bridge::new(
        &spec.name,
        BridgeSpec {
            bridge_type: spec.bridge_type.clone(),
            appservice_id: spec.appservice_id.clone(),
            image: spec.image.clone(),
            port: spec.port,
            files_secret: files_secret_name(&spec.name),
            args: spec.args.clone(),
            storage: BridgeStorage {
                size: spec
                    .storage_size
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| BridgeStorage::default().size),
                storage_class_name: None,
            },
            resources: None,
        },
    );
    bridge.metadata = object_meta(spec.name.clone(), spec, namespace);
    bridge
}

fn files_secret(spec: &BridgeInstanceSpec, namespace: &str) -> Secret {
    Secret {
        metadata: object_meta(files_secret_name(&spec.name), spec, namespace),
        type_: Some("Opaque".to_owned()),
        string_data: Some(spec.files.clone()),
        ..Secret::default()
    }
}

fn instance_status(bridge: &Bridge, namespace: &str) -> BridgeInstanceStatus {
    let name = bridge.metadata.name.clone().unwrap_or_default();
    let namespace = bridge
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| namespace.to_owned());
    let status = bridge.status.clone().unwrap_or_default();
    let message = status
        .conditions
        .iter()
        .find(|c| c.type_ == CONDITION_AVAILABLE)
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty());
    BridgeInstanceStatus {
        service_url: service_url(&name, &namespace, bridge.spec.port),
        name,
        namespace,
        phase: status.phase,
        ready: status.phase == Phase::Ready,
        message,
        image: bridge.spec.image.reference(),
    }
}

fn ignore_not_found<T>(result: Result<T, kube::Error>) -> Result<(), DeployError> {
    match result {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::OperatorStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

    fn spec() -> BridgeInstanceSpec {
        BridgeInstanceSpec {
            name: "bridge-1a2b3c4d".to_owned(),
            labels: BTreeMap::from([("myelin.dev/user".to_owned(), "alice".to_owned())]),
            bridge_type: "mautrix-whatsapp".to_owned(),
            appservice_id: "whatsapp-alice".to_owned(),
            image: ImageSpec {
                repository: "dock.mau.dev/mautrix/whatsapp".to_owned(),
                tag: Some("latest".to_owned()),
                digest: None,
                pull_policy: None,
            },
            port: 29318,
            args: Vec::new(),
            files: BTreeMap::from([
                (
                    "config.yaml".to_owned(),
                    "homeserver:\n  address: http://myelin-hs.myelin.svc:8008\n".to_owned(),
                ),
                (
                    "registration.yaml".to_owned(),
                    "id: whatsapp-alice\n".to_owned(),
                ),
            ]),
            storage_size: None,
        }
    }

    #[test]
    fn manifest_yaml_is_a_secret_and_a_bridge_that_parse_back() {
        let yaml = KubeBridgeClient::manifest_yaml(&spec(), "myelin");
        let docs: Vec<serde_yaml_ng::Value> = serde_yaml_ng::Deserializer::from_str(&yaml)
            .map(|d| serde::Deserialize::deserialize(d).unwrap())
            .collect();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"].as_str(), Some("Secret"));
        assert_eq!(docs[0]["apiVersion"].as_str(), Some("v1"));
        assert_eq!(docs[1]["kind"].as_str(), Some("Bridge"));
        assert_eq!(
            docs[1]["apiVersion"].as_str(),
            Some("hs.matrix.org/v1alpha1")
        );
        assert_eq!(
            docs[0]["metadata"]["name"].as_str(),
            docs[1]["spec"]["filesSecret"].as_str()
        );
        assert_eq!(
            docs[0]["metadata"]["name"].as_str(),
            Some("bridge-1a2b3c4d-files")
        );
        assert_eq!(docs[0]["metadata"]["namespace"].as_str(), Some("myelin"));
        assert_eq!(docs[1]["metadata"]["namespace"].as_str(), Some("myelin"));
        assert_eq!(
            docs[0]["stringData"]["registration.yaml"].as_str(),
            Some("id: whatsapp-alice\n")
        );
        assert_eq!(
            docs[1]["metadata"]["labels"]["myelin.dev/user"].as_str(),
            Some("alice")
        );

        let bridge: Bridge = serde_yaml_ng::from_value(docs[1].clone()).unwrap();
        assert_eq!(bridge.spec.appservice_id, "whatsapp-alice");
        assert_eq!(bridge.spec.port, 29318);
        assert_eq!(bridge.spec.storage.size, "1Gi");
        assert!(bridge.status.is_none());
        let secret: Secret = serde_yaml_ng::from_value(docs[0].clone()).unwrap();
        assert_eq!(secret.string_data.unwrap().len(), 2);
    }

    #[test]
    fn storage_size_is_passed_through() {
        let mut s = spec();
        s.storage_size = Some("5Gi".to_owned());
        assert_eq!(bridge_object(&s, "ns").spec.storage.size, "5Gi");
    }

    #[test]
    fn validate_accepts_the_rfc_names_and_rejects_bad_ones() {
        assert!(validate(&spec()).is_ok());
        for bad in [
            "",
            "Bridge-1",
            "1bridge",
            "bridge-",
            "bridge_1",
            &"b".repeat(64),
        ] {
            let mut s = spec();
            s.name = bad.to_owned();
            assert!(
                matches!(validate(&s), Err(DeployError::Invalid(_))),
                "{bad:?}"
            );
        }
        let mut s = spec();
        s.port = 0;
        assert!(validate(&s).is_err());
        let mut s = spec();
        s.files.insert("../etc/passwd".to_owned(), String::new());
        assert!(validate(&s).is_err());
        let mut s = spec();
        s.appservice_id.clear();
        assert!(validate(&s).is_err());
    }

    #[test]
    fn service_url_is_the_cluster_dns_name() {
        assert_eq!(
            service_url("bridge-1a2b3c4d", "myelin", 29318),
            "http://bridge-1a2b3c4d.myelin.svc:29318"
        );
    }

    #[test]
    fn instance_status_reads_phase_and_message() {
        let mut b = bridge_object(&spec(), "myelin");
        let pending = instance_status(&b, "myelin");
        assert_eq!(pending.phase, Phase::Pending);
        assert!(!pending.ready);
        assert!(pending.message.is_none());
        assert_eq!(pending.image, "dock.mau.dev/mautrix/whatsapp:latest");
        assert_eq!(
            pending.service_url,
            "http://bridge-1a2b3c4d.myelin.svc:29318"
        );

        b.status = Some(OperatorStatus {
            phase: Phase::Degraded,
            observed_generation: Some(1),
            ready_replicas: Some(0),
            conditions: vec![Condition {
                type_: CONDITION_AVAILABLE.to_owned(),
                status: "False".to_owned(),
                reason: "ImagePullBackOff".to_owned(),
                message: "bridge: ImagePullBackOff".to_owned(),
                observed_generation: Some(1),
                last_transition_time: Time(k8s_openapi::chrono::Utc::now()),
            }],
        });
        let degraded = instance_status(&b, "myelin");
        assert_eq!(degraded.phase, Phase::Degraded);
        assert_eq!(
            degraded.message.as_deref(),
            Some("bridge: ImagePullBackOff")
        );
        let json = serde_json::to_value(&degraded).unwrap();
        assert_eq!(json["phase"], "Degraded");
    }
}
