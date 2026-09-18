//! The `PushGateway` custom resource: runs a push-gateway workload (translating Matrix
//! `m.push_gateway`/`sygnal`-shaped push notifications to APNs/FCM) for a `Homeserver`. See
//! `ruma`'s `push-gateway-api` feature (already a workspace dependency, `Cargo.toml`
//! `[workspace.dependencies]`) for the wire protocol this workload speaks; this crate only
//! describes and reconciles the Kubernetes side of running it.

use k8s_openapi::api::core::v1::ResourceRequirements;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{ImageSpec, OperatorStatus};

/// `spec` of a `PushGateway`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, CustomResource)]
#[kube(
    group = "hs.matrix.org",
    version = "v1alpha1",
    kind = "PushGateway",
    namespaced,
    shortname = "pgw",
    status = "OperatorStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct PushGatewaySpec {
    /// The push-gateway image.
    pub image: ImageSpec,
    /// Number of replicas. Push gateways are stateless request/response services, so this scales
    /// freely (unlike most `Bridge` workloads).
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    /// Provider-specific config (APNs certificate reference, FCM service-account key reference,
    /// ...), inline as opaque YAML/JSON the operator passes through without interpreting.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub provider_config: serde_json::Map<String, serde_json::Value>,
    /// Kubernetes resource requests/limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

fn default_replicas() -> i32 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PushGatewaySpec {
        PushGatewaySpec {
            image: ImageSpec {
                repository: "ghcr.io/matrix-org/hs-pushgw".to_owned(),
                tag: Some("0.0.1".to_owned()),
                digest: None,
                pull_policy: None,
            },
            replicas: 2,
            provider_config: serde_json::Map::new(),
            resources: None,
        }
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        let back: PushGatewaySpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.replicas, spec.replicas);
    }

    #[test]
    fn spec_round_trips_through_yaml() {
        let spec = sample();
        let yaml = serde_yaml_ng::to_string(&spec).unwrap();
        let back: PushGatewaySpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back.image.repository, spec.image.repository);
    }
}
