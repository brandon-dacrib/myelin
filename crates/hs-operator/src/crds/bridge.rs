//! The `Bridge` custom resource: runs a bridge process (`mautrix-irc`, a `mautrix-go`-family
//! bridge, or any other appservice-shaped workload) and links it to an
//! [`super::appservice::AppService`] registration. `PLAN.md`'s "the `Bridge` flow end to end with
//! `mautrix-irc`" Phase 1/2 deliverable is what exercises this kind for real; today it is schema
//! and a stub reconciler only (`docs/status/12-platform-and-kubernetes.md`).

use k8s_openapi::api::core::v1::ResourceRequirements;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{ImageSpec, OperatorStatus};

/// `spec` of a `Bridge`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, CustomResource)]
#[kube(
    group = "hs.matrix.org",
    version = "v1alpha1",
    kind = "Bridge",
    namespaced,
    shortname = "br",
    status = "OperatorStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"BridgeType","type":"string","jsonPath":".spec.bridgeType"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct BridgeSpec {
    /// A free-form identifier for which bridge implementation this is (`"mautrix-irc"`,
    /// `"mautrix-whatsapp"`, ...). Not validated against a fixed enum: new bridges should not
    /// need a CRD schema change to be deployable.
    pub bridge_type: String,
    /// The name of the [`super::appservice::AppService`] this bridge process registers as (same
    /// namespace).
    pub app_service_ref: String,
    /// The bridge's own image.
    pub image: ImageSpec,
    /// Number of replicas. Most bridge implementations are not horizontally scalable (a single
    /// process owns the remote-network connection state), so this is expected to stay `1` for
    /// most bridge types; the field exists for the bridges that do support it.
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    /// The bridge's own configuration file contents (bridge-specific YAML/TOML/JSON — this
    /// operator does not parse or validate it, it only mounts it), inline. Prefer
    /// `config_secret_ref` when the config contains credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    /// A `Secret` key holding the bridge's config file instead of `config` inline, for
    /// bridge-specific config formats that embed a database password or an API token alongside
    /// non-secret settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_secret_ref: Option<super::common::SecretKeyRef>,
    /// Kubernetes resource requests/limits for the bridge container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

fn default_replicas() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BridgeSpec {
        BridgeSpec {
            bridge_type: "mautrix-irc".to_owned(),
            app_service_ref: "irc-bridge".to_owned(),
            image: ImageSpec {
                repository: "dock.mau.dev/mautrix/irc".to_owned(),
                tag: Some("latest".to_owned()),
                digest: None,
                pull_policy: None,
            },
            replicas: 1,
            config: Some("homeserver:\n  address: http://hs:8008\n".to_owned()),
            config_secret_ref: None,
            resources: None,
        }
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        let back: BridgeSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.bridge_type, spec.bridge_type);
    }

    #[test]
    fn spec_round_trips_through_yaml() {
        let spec = sample();
        let yaml = serde_yaml_ng::to_string(&spec).unwrap();
        let back: BridgeSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back.app_service_ref, spec.app_service_ref);
    }
}
