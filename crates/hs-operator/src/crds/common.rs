//! Types shared across every CRD in [`crate::crds`]: image references, resource requirements
//! (kept as loosely typed as `k8s-openapi`'s own `ResourceRequirements` rather than reinvented),
//! and the status shape every one of this operator's kinds reports through.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A container image reference, split into repository and tag/digest so a `Kustomize`-style
/// image override (`newTag`, `newName`) can target just the field it needs.
///
/// camelCase on the wire (`pullPolicy`), like a container's own `imagePullPolicy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageSpec {
    /// The image repository, e.g. `ghcr.io/matrix-org/hs`.
    pub repository: String,
    /// The image tag. Mutually exclusive with `digest` in principle; both are accepted and
    /// `digest` wins if both are set (the same convention `deploy/helm/hs`'s
    /// `hs.image` template helper uses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// An image digest (`sha256:...`), for a pinned, content-addressed reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Pull policy. Defaults to the cluster/Kubernetes default (`IfNotPresent` for a tag,
    /// `Always` otherwise) when omitted, same as `deploy/helm/hs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_policy: Option<String>,
}

impl ImageSpec {
    /// The image reference a container's `image` field takes: `repository@digest` when a digest
    /// is set (it wins over a tag), else `repository:tag`, else the bare repository (which the
    /// container runtime reads as `:latest`).
    #[must_use]
    pub fn reference(&self) -> String {
        match (&self.digest, &self.tag) {
            (Some(digest), _) if !digest.is_empty() => format!("{}@{digest}", self.repository),
            (_, Some(tag)) if !tag.is_empty() => format!("{}:{tag}", self.repository),
            _ => self.repository.clone(),
        }
    }
}

/// A reference to a key within a `Secret` in the same namespace as the CRD instance — the same
/// shape `deploy/helm/hs/values.yaml`'s `secrets.*.existingSecret`/`key` pairs use, so a Helm
/// deployment and an operator-managed one describe secrets identically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretKeyRef {
    /// The `Secret` name.
    pub name: String,
    /// The key within that `Secret`'s `data`/`stringData`.
    pub key: String,
}

/// The lifecycle phase every kind in this operator reports. Coarse by design — fine-grained
/// state belongs in `conditions`, matching the Kubernetes API convention (a `phase` field is a
/// human-readable summary, `status.conditions` is the machine-readable detail other controllers
/// and `kubectl wait --for=condition=...` actually key off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum Phase {
    /// The resource was just created or its spec just changed; reconciliation has not converged
    /// yet.
    #[default]
    Pending,
    /// The workload this resource manages is running and passing its readiness probe(s).
    Ready,
    /// Reconciliation converged onto a broken state (an image pull failure, a referenced Secret
    /// missing, ...) and needs operator attention. `status.conditions` carries the detail.
    Degraded,
}

/// The status sub-resource every kind in this operator uses. Identical shape across all five
/// kinds by design: one dashboard query, one `kubectl get -o jsonpath='{.status.phase}'` habit,
/// works against any of them.
///
/// camelCase on the wire (`observedGeneration`, `readyReplicas`), like every Kubernetes status;
/// until 2026-09-26 it was snake_case, which the `Ready` print column (`.status.readyReplicas`)
/// never matched. Nothing wrote a status before then, so nothing stored the old spelling.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorStatus {
    /// Coarse lifecycle phase.
    #[serde(default)]
    pub phase: Phase,
    /// The `.metadata.generation` this status was computed from, for a caller to tell whether
    /// `status` reflects the current `spec` or a stale one the controller has not caught up to
    /// yet (the standard Kubernetes "observedGeneration" convention).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// How many replicas of the managed workload are currently ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_replicas: Option<i32>,
    /// Standard Kubernetes conditions (`type`, `status`, `reason`, `message`,
    /// `lastTransitionTime`). Empty until the first reconcile.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_serializes_as_pascal_case() {
        assert_eq!(serde_json::to_value(Phase::Ready).unwrap(), "Ready");
        assert_eq!(serde_json::to_value(Phase::Pending).unwrap(), "Pending");
    }

    #[test]
    fn operator_status_default_is_pending_with_no_conditions() {
        let status = OperatorStatus::default();
        assert_eq!(status.phase, Phase::Pending);
        assert!(status.conditions.is_empty());
    }

    #[test]
    fn operator_status_is_camel_case_on_the_wire() {
        let status = OperatorStatus {
            ready_replicas: Some(1),
            observed_generation: Some(2),
            ..OperatorStatus::default()
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["readyReplicas"], 1);
        assert_eq!(json["observedGeneration"], 2);
    }

    #[test]
    fn image_reference_prefers_digest_then_tag() {
        let mut image = ImageSpec {
            repository: "dock.mau.dev/mautrix/whatsapp".to_owned(),
            tag: Some("v0.12.0".to_owned()),
            digest: None,
            pull_policy: None,
        };
        assert_eq!(image.reference(), "dock.mau.dev/mautrix/whatsapp:v0.12.0");
        image.digest = Some("sha256:abc".to_owned());
        assert_eq!(
            image.reference(),
            "dock.mau.dev/mautrix/whatsapp@sha256:abc"
        );
        image.digest = None;
        image.tag = None;
        assert_eq!(image.reference(), "dock.mau.dev/mautrix/whatsapp");
    }
}
