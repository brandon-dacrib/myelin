//! The `Bridge` custom resource: one bridge process (a mautrix bridge, heisenbridge, or any other
//! appservice-shaped workload), deployed by the operator as a one-replica `Deployment`, a
//! `Service` and a `PersistentVolumeClaim` (`docs/rfcs/0017-the-server-deploys-its-own-bridges.md`
//! section 4.4). The operator knows nothing about users: one `Bridge` is one process. The
//! homeserver's bridge manager (`crates/hs-bridges`) creates them through
//! [`crate::deploy::KubeBridgeClient`]; [`crate::bridge`] builds the objects each one becomes and
//! [`crate::controller`] keeps them converged.
//!
//! The draft schema this replaces (`appServiceRef`, `replicas`, inline `config`) was never
//! reconciled by anything; `v1alpha1` allows the break (RFC 0017, 4.4).

use k8s_openapi::api::core::v1::ResourceRequirements;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{ImageSpec, OperatorStatus};

/// `spec` of a `Bridge`.
///
/// There is deliberately no `replicas` field. A bridge owns its remote-network sessions (a
/// WhatsApp or Signal login is one client) and its SQLite database, so two copies running at once
/// would be two clients on one session. The operator always runs exactly one replica and rolls it
/// out with the `Recreate` strategy, so an old and a new pod never overlap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, CustomResource)]
#[kube(
    group = "hs.matrix.org",
    version = "v1alpha1",
    kind = "Bridge",
    namespaced,
    shortname = "br",
    status = "OperatorStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"BridgeType","type":"string","jsonPath":".spec.bridgeType"}"#,
    printcolumn = r#"{"name":"Appservice","type":"string","jsonPath":".spec.appserviceId"}"#,
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct BridgeSpec {
    /// Which bridge implementation this is (`"mautrix-whatsapp"`, `"heisenbridge"`, ...), from the
    /// homeserver's bridge catalogue. Free-form: a new bridge type must not need a CRD change.
    pub bridge_type: String,
    /// The appservice registration id this process answers for (`whatsapp-alice`). Informational
    /// for the operator (it labels the objects with it); the homeserver's registry is where the
    /// registration itself lives.
    pub appservice_id: String,
    /// The Matrix ID of the person this bridge is for (`@alice:example.org`); unset for a shared
    /// instance that serves everyone on the server. Informational for the operator: it labels
    /// the objects with it (`myelin.dev/owner`, made label-safe) so that
    /// `kubectl get pods -l myelin.dev/owner=alice-example.org` finds a person's bridges, and
    /// keeps the exact value in an annotation of the same name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The bridge's own image. The container runs the image's own entrypoint.
    pub image: ImageSpec,
    /// The port the bridge listens on for the homeserver's appservice transactions. The Service
    /// exposes it under the same number, and the readiness probe is a TCP connect to it.
    pub port: i32,
    /// A `Secret` in the same namespace whose every key is written into `/data` as a file on
    /// every start. A mautrix bridge completes and rewrites its `config.yaml` and generates
    /// secrets it was not given (`encryption.pickle_key`), so the values of those keys are
    /// carried from the file already there into the new copy, which keeps its crypto store
    /// readable. The Secret's contents are not part of this spec: whoever changes them
    /// annotates the `Bridge` with `myelin.dev/files-hash` so that the change rolls the pod.
    pub files_secret: String,
    /// Arguments passed to the image's entrypoint (heisenbridge takes its flags here). Empty
    /// means the image's default command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// The bridge's volume, mounted at `/data`: its configuration, registration and SQLite
    /// database.
    #[serde(default)]
    pub storage: BridgeStorage,
    /// Resource requests and limits for the bridge container. When unset the operator requests
    /// 32Mi and 10m and limits memory to 512Mi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

/// The volume a [`BridgeSpec`] keeps its state on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStorage {
    /// Requested size, a Kubernetes quantity (`1Gi`).
    #[serde(default = "default_storage_size")]
    pub size: String,
    /// The StorageClass to request; the cluster's default when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
}

impl Default for BridgeStorage {
    fn default() -> Self {
        Self {
            size: default_storage_size(),
            storage_class_name: None,
        }
    }
}

fn default_storage_size() -> String {
    "1Gi".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BridgeSpec {
        BridgeSpec {
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
            files_secret: "bridge-1a2b3c4d-files".to_owned(),
            args: Vec::new(),
            storage: BridgeStorage::default(),
            resources: None,
        }
    }

    #[test]
    fn spec_round_trips_through_json_in_camel_case() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["bridgeType"], "mautrix-whatsapp");
        assert_eq!(json["appserviceId"], "whatsapp-alice");
        assert_eq!(json["filesSecret"], "bridge-1a2b3c4d-files");
        assert_eq!(json["storage"]["size"], "1Gi");
        let back: BridgeSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn the_rfc_example_parses_with_defaults() {
        let yaml = r"
bridgeType: mautrix-whatsapp
appserviceId: whatsapp-alice
image: { repository: dock.mau.dev/mautrix/whatsapp, tag: latest }
port: 29318
filesSecret: bridge-1a2b3c4d-files
";
        let spec: BridgeSpec = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(spec.args.is_empty());
        assert_eq!(spec.storage, BridgeStorage::default());
        assert!(spec.resources.is_none());
    }

    #[test]
    fn the_schema_has_no_replicas_field() {
        use kube::CustomResourceExt as _;
        let crd = serde_json::to_value(Bridge::crd()).unwrap();
        let props = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"];
        assert!(props.get("replicas").is_none());
        assert!(props.get("appserviceId").is_some());
    }
}
