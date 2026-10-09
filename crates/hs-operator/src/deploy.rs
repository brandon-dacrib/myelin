//! The client the homeserver's bridge manager (`crates/hs-bridges`) deploys bridges with: it
//! writes a `Bridge` and its files `Secret`, reads the `Bridge`'s status back, and deletes it.
//! The operator ([`crate::controller`]) does the rest. `docs/rfcs/0017-the-server-deploys-its-own-bridges.md`
//! sections 4.3 to 4.5.
//!
//! [`KubeBridgeClient::manifest_yaml`] renders the same two objects as YAML, for a bridge that
//! is to run on another cluster running the operator.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::{Api, Resource as _};
use serde::Serialize;

use crate::bridge::{
    ANNOTATION_APPSERVICE_ID, ANNOTATION_FILES_HASH, ANNOTATION_OWNER, CONDITION_AVAILABLE,
    LABEL_APPSERVICE_ID, LABEL_BRIDGE_TYPE, LABEL_OWNER, label_value,
};
use crate::crds::{Bridge, BridgeSpec, BridgeStorage, ImageSpec, Phase};

/// The field manager the homeserver applies `Bridge`s and Secrets with.
pub const FIELD_MANAGER: &str = "myelin-homeserver";

/// Where the current `Bridge` CRD is, for the sentence that says the cluster's is older.
pub const BRIDGE_CRD_PATH: &str = "deploy/crds/bridge.yaml";

/// `Bridge` fields added after the first released CRD that nothing depends on: the operator
/// only labels objects with them. When the cluster's CRD is older than this server and lacks
/// only fields from this list, [`KubeBridgeClient::apply`] writes the `Bridge` without them
/// (the bridge keeps running) and says the CRD needs applying. A field that is missing and not
/// on this list stops the apply with [`DeployError::OutdatedCrd`].
pub const INFORMATIONAL_FIELDS: &[&str] = &[".spec.owner"];

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
    /// The owner's Matrix ID; `None` for a shared instance. Labelled onto every object
    /// (`myelin.dev/owner`, [`LABEL_OWNER`]) so a person's bridges can be listed.
    pub owner: Option<String>,
    /// The bridge's image.
    pub image: ImageSpec,
    /// The port the bridge listens on for appservice transactions.
    pub port: i32,
    /// Arguments to the image's entrypoint; empty for its default command.
    pub args: Vec<String>,
    /// File name to contents. Each becomes a file in the bridge's `/data` on every start,
    /// through the Secret `<name>-files`; the `Bridge` is annotated with a hash of them
    /// ([`ANNOTATION_FILES_HASH`]) so that a change rolls the pod, and the operator's init
    /// container carries the secrets a mautrix bridge generated into the new copy.
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
    /// The `Bridge` CRD installed in the cluster does not declare fields this server writes,
    /// and at least one of them is not [informational](INFORMATIONAL_FIELDS). Its message is
    /// [`outdated_crd_message`].
    #[error("{}", outdated_crd_message(.fields))]
    OutdatedCrd {
        /// The undeclared fields, as the API server names them (`.spec.owner`).
        fields: Vec<String>,
    },
}

/// The one sentence an operator reads when the cluster's `Bridge` CRD lacks `fields`: what is
/// wrong and the command that fixes it.
#[must_use]
pub fn outdated_crd_message(fields: &[String]) -> String {
    format!(
        "the Bridge CRD in the cluster is older than this server (it does not declare {}): \
         apply {BRIDGE_CRD_PATH} (`kubectl apply --server-side -f {BRIDGE_CRD_PATH}`), or \
         `helm upgrade` the chart, which keeps it current",
        fields.join(", ")
    )
}

/// The fields a server-side apply was refused for because the CRD's schema does not declare
/// them, from the API server's message (`failed to create typed patch object (...): .spec.owner:
/// field not declared in schema`, or one such line per field after `errors:`). Empty when the
/// refusal is anything else.
#[must_use]
pub fn undeclared_fields(message: &str) -> Vec<String> {
    const MARKER: &str = ": field not declared in schema";
    let mut fields = Vec::new();
    let mut rest = message;
    while let Some(at) = rest.find(MARKER) {
        let before = &rest[..at];
        let path = before
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or_default();
        let plain = path.len() > 1
            && path.starts_with('.')
            && path[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        if plain && !fields.iter().any(|f| f == path) {
            fields.push(path.to_owned());
        }
        rest = &rest[at + MARKER.len()..];
    }
    fields
}

/// Removes the field at `path` (`.spec.owner`) from `object`; a missing one is left alone.
fn remove_path(object: &mut serde_json::Value, path: &str) {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut at = object;
    for segment in parents {
        match at.get_mut(*segment) {
            Some(next) => at = next,
            None => return,
        }
    }
    if let Some(map) = at.as_object_mut() {
        map.remove(*last);
    }
}

/// Deploys, inspects and removes bridges in one namespace through the Kubernetes API.
#[derive(Clone)]
pub struct KubeBridgeClient {
    client: kube::Client,
    namespace: String,
    /// The fields the cluster's `Bridge` CRD was last found not to declare; `None` once an
    /// apply has gone through whole. Logged when it changes.
    outdated: Arc<Mutex<Option<Vec<String>>>>,
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
            outdated: Arc::new(Mutex::new(None)),
        }
    }

    /// When the cluster's `Bridge` CRD was last found older than this server: the sentence
    /// saying so and how to fix it ([`outdated_crd_message`]), for the bridge's page. `None`
    /// when the last apply went through whole, or before the first.
    #[must_use]
    pub fn outdated_crd(&self) -> Option<String> {
        self.outdated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            .map(outdated_crd_message)
    }

    /// Records what the last apply found about the CRD, logging once per change.
    fn note_crd(&self, fields: Option<Vec<String>>) {
        let mut outdated = self
            .outdated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *outdated == fields {
            return;
        }
        match &fields {
            Some(fields) => tracing::warn!(
                fields = %fields.join(", "),
                informational = fields.iter().all(|f| INFORMATIONAL_FIELDS.contains(&f.as_str())),
                "{}; until then Bridges are written without the informational fields it lacks",
                outdated_crd_message(fields)
            ),
            None => tracing::info!(
                "the Bridge CRD in the cluster declares every field this server writes again"
            ),
        }
        *outdated = fields;
    }

    /// Server-side applies `bridge`. When the cluster's CRD refuses fields it does not declare
    /// and every one of them is [informational](INFORMATIONAL_FIELDS), applies it again without
    /// them; otherwise fails with [`DeployError::OutdatedCrd`]. Either way the finding is
    /// recorded for [`Self::outdated_crd`] and logged once.
    async fn apply_bridge(
        &self,
        bridges: &Api<Bridge>,
        name: &str,
        bridge: &Bridge,
        params: &PatchParams,
    ) -> Result<Bridge, DeployError> {
        let refused = match bridges.patch(name, params, &Patch::Apply(bridge)).await {
            Ok(applied) => {
                self.note_crd(None);
                return Ok(applied);
            }
            Err(kube::Error::Api(e)) => {
                let fields = undeclared_fields(&e.message);
                if fields.is_empty() {
                    return Err(kube::Error::Api(e).into());
                }
                fields
            }
            Err(e) => return Err(e.into()),
        };
        self.note_crd(Some(refused.clone()));
        if !refused
            .iter()
            .all(|f| INFORMATIONAL_FIELDS.contains(&f.as_str()))
        {
            return Err(DeployError::OutdatedCrd { fields: refused });
        }
        let mut object = serde_json::to_value(bridge)
            .map_err(|e| DeployError::Invalid(format!("the Bridge does not serialize: {e}")))?;
        for field in &refused {
            remove_path(&mut object, field);
        }
        Ok(bridges.patch(name, params, &Patch::Apply(&object)).await?)
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

    /// Creates or updates the `Bridge` and its files Secret `<name>-files` (owned by the
    /// `Bridge`, so deleting the `Bridge` deletes it), both by server-side apply. Idempotent.
    /// Returns the `Bridge`'s status as it stands, which for a new one is `Pending`.
    ///
    /// For a `Bridge` that exists, the Secret is written first: the `Bridge` carries a hash of
    /// the files ([`ANNOTATION_FILES_HASH`]) and a change to it restarts the pod, so the files
    /// must be in place before it does, and a failure between the two leaves the pod as it was.
    /// A new one is written first, because the Secret's owner reference needs its uid.
    ///
    /// A cluster whose `Bridge` CRD is older than this server is handled as
    /// [`INFORMATIONAL_FIELDS`] says, and reported by [`Self::outdated_crd`].
    ///
    /// # Errors
    /// [`DeployError::Invalid`] for a spec that cannot be deployed (see [`validate`]),
    /// [`DeployError::OutdatedCrd`] for a CRD that lacks a field the bridge needs, else
    /// [`DeployError::Kube`].
    pub async fn apply(
        &self,
        spec: &BridgeInstanceSpec,
    ) -> Result<BridgeInstanceStatus, DeployError> {
        validate(spec)?;
        let params = PatchParams::apply(FIELD_MANAGER).force();
        let bridges: Api<Bridge> = Api::namespaced(self.client.clone(), &self.namespace);
        let bridge = bridge_object(spec, &self.namespace);

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
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let secret_name = files_secret_name(&spec.name);

        let applied = if let Some(existing) = bridges.get_opt(&spec.name).await? {
            secret.metadata.owner_references = existing.controller_owner_ref(&()).map(|r| vec![r]);
            secrets
                .patch(&secret_name, &params, &Patch::Apply(&secret))
                .await?;
            self.apply_bridge(&bridges, &spec.name, &bridge, &params)
                .await?
        } else {
            let applied = self
                .apply_bridge(&bridges, &spec.name, &bridge, &params)
                .await?;
            secret.metadata.owner_references = applied.controller_owner_ref(&()).map(|r| vec![r]);
            secrets
                .patch(&secret_name, &params, &Patch::Apply(&secret))
                .await?;
            applied
        };

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
    if let Some(owner) = spec_owner(spec) {
        labels.insert(LABEL_OWNER.to_owned(), label_value(owner));
    }
    labels
}

fn spec_owner(spec: &BridgeInstanceSpec) -> Option<&str> {
    spec.owner
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
}

fn object_meta(name: String, spec: &BridgeInstanceSpec, namespace: &str) -> ObjectMeta {
    let mut annotations = BTreeMap::from([(
        ANNOTATION_APPSERVICE_ID.to_owned(),
        spec.appservice_id.clone(),
    )]);
    if let Some(owner) = spec_owner(spec) {
        annotations.insert(ANNOTATION_OWNER.to_owned(), owner.to_owned());
    }
    ObjectMeta {
        name: Some(name),
        namespace: Some(namespace.to_owned()),
        labels: Some(object_labels(spec)),
        annotations: Some(annotations),
        ..ObjectMeta::default()
    }
}

fn bridge_object(spec: &BridgeInstanceSpec, namespace: &str) -> Bridge {
    let mut bridge = Bridge::new(
        &spec.name,
        BridgeSpec {
            bridge_type: spec.bridge_type.clone(),
            appservice_id: spec.appservice_id.clone(),
            owner: spec_owner(spec).map(str::to_owned),
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
        .metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(ANNOTATION_FILES_HASH.to_owned(), files_hash(&spec.files));
    bridge
}

/// A short, stable hash of a set of files (16 hex digits of SHA-256 over names and contents),
/// what [`ANNOTATION_FILES_HASH`] carries.
#[must_use]
pub fn files_hash(files: &BTreeMap<String, String>) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for (name, contents) in files {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(contents.as_bytes());
        hasher.update([0]);
    }
    hex::encode(&hasher.finalize()[..8])
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
            owner: Some("@alice:example.org".to_owned()),
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

        assert_eq!(
            docs[1]["metadata"]["labels"][LABEL_OWNER].as_str(),
            Some("alice-example.org")
        );
        assert_eq!(
            docs[1]["metadata"]["annotations"][ANNOTATION_OWNER].as_str(),
            Some("@alice:example.org")
        );
        assert_eq!(
            docs[0]["metadata"]["labels"][LABEL_OWNER].as_str(),
            Some("alice-example.org")
        );

        let bridge: Bridge = serde_yaml_ng::from_value(docs[1].clone()).unwrap();
        assert_eq!(bridge.spec.appservice_id, "whatsapp-alice");
        assert_eq!(bridge.spec.owner.as_deref(), Some("@alice:example.org"));
        assert_eq!(bridge.spec.port, 29318);
        assert_eq!(bridge.spec.storage.size, "1Gi");
        assert!(bridge.status.is_none());
        let secret: Secret = serde_yaml_ng::from_value(docs[0].clone()).unwrap();
        assert_eq!(secret.string_data.unwrap().len(), 2);
    }

    #[test]
    fn the_bridge_carries_a_hash_of_its_files_so_a_change_rolls_the_pod() {
        let a = bridge_object(&spec(), "ns");
        let hash =
            |b: &Bridge| b.metadata.annotations.as_ref().unwrap()[ANNOTATION_FILES_HASH].clone();
        assert_eq!(hash(&a).len(), 16);
        assert_eq!(hash(&a), hash(&bridge_object(&spec(), "ns")));
        let mut changed = spec();
        changed.files.insert(
            "config.yaml".to_owned(),
            "network:\n  os_name: renamed\n".to_owned(),
        );
        assert_ne!(hash(&a), hash(&bridge_object(&changed, "ns")));
        // The Secret is not annotated with it: it is the Bridge's pod that has to roll.
        assert!(
            !files_secret(&spec(), "ns")
                .metadata
                .annotations
                .unwrap()
                .contains_key(ANNOTATION_FILES_HASH)
        );
        let yaml = KubeBridgeClient::manifest_yaml(&spec(), "myelin");
        assert!(yaml.contains(ANNOTATION_FILES_HASH), "{yaml}");
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

    #[test]
    fn undeclared_fields_are_read_from_the_api_servers_refusal() {
        // As the demo's API server said it on 2026-10-09.
        let one = "failed to create typed patch object (myelin/bridge-d2854412; \
                   hs.matrix.org/v1alpha1, Kind=Bridge): .spec.owner: field not declared in schema";
        assert_eq!(undeclared_fields(one), vec![".spec.owner".to_owned()]);
        // Several at once come one per line after `errors:`.
        let several = "failed to create typed patch object (ns/b; hs.matrix.org/v1alpha1, \
                       Kind=Bridge): errors:\n  .spec.owner: field not declared in schema\n  \
                       .spec.storage.class: field not declared in schema";
        assert_eq!(
            undeclared_fields(several),
            vec![".spec.owner".to_owned(), ".spec.storage.class".to_owned()]
        );
        assert!(undeclared_fields("bridges.hs.matrix.org \"b\" is forbidden").is_empty());
        // A path this client cannot remove by name is not offered for removal.
        assert!(undeclared_fields(".spec.args[0]: field not declared in schema").is_empty());
        let message = outdated_crd_message(&[".spec.owner".to_owned()]);
        assert!(
            message.starts_with("the Bridge CRD in the cluster is older than this server"),
            "{message}"
        );
        assert!(message.contains("deploy/crds/bridge.yaml"), "{message}");
    }

    #[test]
    fn remove_path_drops_one_nested_field() {
        let mut v = serde_json::json!({"spec": {"owner": "@a:x", "port": 1}});
        remove_path(&mut v, ".spec.owner");
        remove_path(&mut v, ".spec.missing.deeper");
        assert_eq!(v, serde_json::json!({"spec": {"port": 1}}));
    }

    /// An API server for `Bridge`s and Secrets in memory, whose `Bridge` schema may lack
    /// `.spec.owner` (a CRD from before it was added), recording each request in order.
    #[derive(Default)]
    struct FakeApi {
        crd_declares_owner: bool,
        /// A schema that also lacks `.spec.port`, which the bridge cannot do without.
        crd_lacks_port: bool,
        refuse_secrets: bool,
        bridges: BTreeMap<String, serde_json::Value>,
        secrets: BTreeMap<String, serde_json::Value>,
        requests: Vec<String>,
    }

    type Shared = std::sync::Arc<Mutex<FakeApi>>;

    fn status(code: u16, reason: &str, message: &str) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        (
            axum::http::StatusCode::from_u16(code).unwrap(),
            axum::Json(serde_json::json!({
                "kind": "Status", "apiVersion": "v1", "metadata": {},
                "status": "Failure", "message": message, "reason": reason, "code": code
            })),
        )
            .into_response()
    }

    async fn fake_api(
        axum::extract::State(api): axum::extract::State<Shared>,
        method: axum::http::Method,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        let path = uri.path().to_owned();
        let mut api = api.lock().unwrap();
        api.requests.push(format!("{method} {path}"));
        let name = path.rsplit('/').next().unwrap_or_default().to_owned();
        let is_bridge = path.contains("/bridges/");
        let found = if is_bridge {
            api.bridges.get(&name).cloned()
        } else {
            api.secrets.get(&name).cloned()
        };
        if method == axum::http::Method::GET {
            return match found {
                Some(object) => axum::Json(object).into_response(),
                None => status(404, "NotFound", "not found"),
            };
        }
        let mut object: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if is_bridge {
            let mut undeclared = Vec::new();
            if !api.crd_declares_owner && object["spec"].get("owner").is_some() {
                undeclared.push(".spec.owner");
            }
            if api.crd_lacks_port {
                undeclared.push(".spec.port");
            }
            if !undeclared.is_empty() {
                let lines: Vec<String> = undeclared
                    .iter()
                    .map(|f| format!("{f}: field not declared in schema"))
                    .collect();
                let detail = if lines.len() == 1 {
                    lines[0].clone()
                } else {
                    format!("errors:\n  {}", lines.join("\n  "))
                };
                return status(
                    500,
                    "",
                    &format!(
                        "failed to create typed patch object (myelin/{name}; \
                         hs.matrix.org/v1alpha1, Kind=Bridge): {detail}"
                    ),
                );
            }
            object["metadata"]["uid"] = serde_json::json!(format!("uid-{name}"));
            api.bridges.insert(name, object.clone());
        } else {
            if api.refuse_secrets {
                return status(403, "Forbidden", "secrets is forbidden");
            }
            api.secrets.insert(name, object.clone());
        }
        axum::Json(object).into_response()
    }

    async fn client_over(api: FakeApi) -> (KubeBridgeClient, Shared) {
        let shared: Shared = std::sync::Arc::new(Mutex::new(api));
        let app = axum::Router::new()
            .fallback(fake_api)
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let url: axum::http::Uri = format!("http://{addr}").parse().unwrap();
        // As `connect` does: the workspace links both rustls providers, so none is the default
        // until one is installed (each test passes alone and panics in the workspace gate).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = kube::Client::try_from(kube::Config::new(url)).unwrap();
        (KubeBridgeClient::new(client, "myelin"), shared)
    }

    fn take_requests(api: &Shared) -> Vec<String> {
        std::mem::take(&mut api.lock().unwrap().requests)
    }

    const BRIDGE: &str = "/apis/hs.matrix.org/v1alpha1/namespaces/myelin/bridges/bridge-1a2b3c4d";
    const SECRET: &str = "/api/v1/namespaces/myelin/secrets/bridge-1a2b3c4d-files";

    #[tokio::test]
    async fn a_new_bridge_is_written_before_its_files_and_an_existing_one_after() {
        let (client, api) = client_over(FakeApi {
            crd_declares_owner: true,
            ..FakeApi::default()
        })
        .await;
        client.apply(&spec()).await.unwrap();
        assert_eq!(
            take_requests(&api),
            [
                format!("GET {BRIDGE}"),
                format!("PATCH {BRIDGE}"),
                format!("PATCH {SECRET}")
            ]
        );
        // The Secret is owned by the Bridge it was written for.
        let owner =
            api.lock().unwrap().secrets["bridge-1a2b3c4d-files"]["metadata"]["ownerReferences"][0]
                ["uid"]
                .clone();
        assert_eq!(owner, "uid-bridge-1a2b3c4d");

        // An update writes the files first: the Bridge's files hash restarts the pod, and the
        // pod must find the new files when it does.
        let mut changed = spec();
        changed
            .files
            .insert("config.yaml".to_owned(), "changed: true\n".to_owned());
        client.apply(&changed).await.unwrap();
        assert_eq!(
            take_requests(&api),
            [
                format!("GET {BRIDGE}"),
                format!("PATCH {SECRET}"),
                format!("PATCH {BRIDGE}")
            ]
        );
        assert!(client.outdated_crd().is_none());

        // A Secret that cannot be written leaves the Bridge, and so the pod, as it was.
        api.lock().unwrap().refuse_secrets = true;
        let before = api.lock().unwrap().bridges["bridge-1a2b3c4d"].clone();
        let mut again = changed.clone();
        again
            .files
            .insert("config.yaml".to_owned(), "changed: twice\n".to_owned());
        client.apply(&again).await.unwrap_err();
        assert_eq!(api.lock().unwrap().bridges["bridge-1a2b3c4d"], before);
    }

    #[tokio::test]
    async fn an_older_crd_without_the_owner_field_gets_the_bridge_without_it_and_says_so() {
        let (client, api) = client_over(FakeApi::default()).await;
        let status = client.apply(&spec()).await.unwrap();
        assert_eq!(status.name, "bridge-1a2b3c4d");
        let stored = api.lock().unwrap().bridges["bridge-1a2b3c4d"].clone();
        assert!(stored["spec"].get("owner").is_none(), "{stored}");
        assert_eq!(stored["spec"]["port"], 29318);
        // The owner still labels the Bridge: the label is metadata, not the CRD's schema.
        assert_eq!(
            stored["metadata"]["labels"][LABEL_OWNER],
            label_value("@alice:example.org")
        );
        assert_eq!(
            take_requests(&api),
            [
                format!("GET {BRIDGE}"),
                format!("PATCH {BRIDGE}"),
                format!("PATCH {BRIDGE}"),
                format!("PATCH {SECRET}")
            ]
        );
        let note = client.outdated_crd().unwrap();
        assert!(note.contains(".spec.owner"), "{note}");
        assert!(note.contains("deploy/crds/bridge.yaml"), "{note}");

        // Once the CRD is applied, the next write carries the owner and the note goes.
        api.lock().unwrap().crd_declares_owner = true;
        client.apply(&spec()).await.unwrap();
        let stored = api.lock().unwrap().bridges["bridge-1a2b3c4d"].clone();
        assert_eq!(stored["spec"]["owner"], "@alice:example.org");
        assert!(client.outdated_crd().is_none());
    }

    #[tokio::test]
    async fn an_older_crd_without_a_field_the_bridge_needs_stops_the_apply_with_the_fix() {
        let (client, api) = client_over(FakeApi {
            crd_lacks_port: true,
            ..FakeApi::default()
        })
        .await;
        let e = client.apply(&spec()).await.unwrap_err();
        assert!(
            matches!(&e, DeployError::OutdatedCrd { fields } if fields.len() == 2),
            "{e:?}"
        );
        let message = e.to_string();
        assert!(
            message.starts_with("the Bridge CRD in the cluster is older than this server"),
            "{message}"
        );
        assert!(api.lock().unwrap().bridges.is_empty());
        assert!(api.lock().unwrap().secrets.is_empty());
        assert!(client.outdated_crd().is_some());
    }
}
