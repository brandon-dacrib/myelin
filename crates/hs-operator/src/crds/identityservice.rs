//! The `IdentityService` custom resource: runs a Matrix identity service (3PID lookup/binding,
//! the `identity-service-api` ruma feature) alongside a `Homeserver`.

use k8s_openapi::api::core::v1::ResourceRequirements;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{ImageSpec, OperatorStatus};

/// `spec` of an `IdentityService`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, CustomResource)]
#[kube(
    group = "hs.matrix.org",
    version = "v1alpha1",
    kind = "IdentityService",
    namespaced,
    shortname = "is",
    status = "OperatorStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct IdentityServiceSpec {
    /// The identity-service image.
    pub image: ImageSpec,
    /// Number of replicas.
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    /// The public base URL this identity service is reachable at (advertised to clients via
    /// `.well-known/matrix/client`'s `m.identity_server` entry by whichever `Homeserver`
    /// references it).
    pub public_base_url: String,
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

    fn sample() -> IdentityServiceSpec {
        IdentityServiceSpec {
            image: ImageSpec {
                repository: "ghcr.io/matrix-org/hs-identity".to_owned(),
                tag: Some("0.0.1".to_owned()),
                digest: None,
                pull_policy: None,
            },
            replicas: 2,
            public_base_url: "https://identity.example.org".to_owned(),
            resources: None,
        }
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        let back: IdentityServiceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.public_base_url, spec.public_base_url);
    }

    #[test]
    fn spec_round_trips_through_yaml() {
        let spec = sample();
        let yaml = serde_yaml_ng::to_string(&spec).unwrap();
        let back: IdentityServiceSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back.replicas, spec.replicas);
    }
}
