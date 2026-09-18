//! Custom resource definitions: `Homeserver`, `AppService`, `Bridge`, `PushGateway` and
//! `IdentityService`, all in the `hs.matrix.org/v1alpha1` API group. Each kind's module has the
//! full rationale; [`all_crds`] is the entry point [`crate::bin::gen_crds`] (and the schema
//! round-trip tests) use to enumerate every one of them in one place.

pub mod appservice;
pub mod bridge;
pub mod common;
pub mod homeserver;
pub mod identityservice;
pub mod pushgateway;

pub use appservice::{AppService, AppServiceSpec};
pub use bridge::{Bridge, BridgeSpec};
pub use common::{ImageSpec, OperatorStatus, Phase, SecretKeyRef};
pub use homeserver::{
    EmbeddedStorageSpec, Homeserver, HomeserverSpec, PostgresStorageSpec, SlatedbStorageSpec,
    StorageBackend, StorageSpec,
};
pub use identityservice::{IdentityService, IdentityServiceSpec};
pub use pushgateway::{PushGateway, PushGatewaySpec};

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt as _;

/// Every CRD this operator defines, with a short label for use in filenames/log lines. Kept as
/// one function so a new kind is wired into codegen (`gen-crds`) and the schema round-trip test
/// below by adding one line here, not by updating several call sites that could drift out of
/// sync.
#[must_use]
pub fn all_crds() -> Vec<(&'static str, CustomResourceDefinition)> {
    vec![
        ("homeserver", Homeserver::crd()),
        ("appservice", AppService::crd()),
        ("bridge", Bridge::crd()),
        ("pushgateway", PushGateway::crd()),
        ("identityservice", IdentityService::crd()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_crd_has_the_expected_group_and_a_stored_version() {
        for (label, crd) in all_crds() {
            assert_eq!(crd.spec.group, "hs.matrix.org", "{label}");
            assert!(
                crd.spec.versions.iter().any(|v| v.served && v.storage),
                "{label} has no served+stored version"
            );
        }
    }

    #[test]
    fn every_crd_schema_round_trips_through_yaml() {
        // Proves gen-crds' actual output path (CRD -> YAML -> re-parsed) is well-formed for
        // every kind, not just that the Rust structs serialize.
        for (label, crd) in all_crds() {
            let yaml = serde_yaml_ng::to_string(&crd).unwrap_or_else(|e| {
                panic!("{label}: failed to render CRD as YAML: {e}");
            });
            let reparsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml)
                .unwrap_or_else(|e| panic!("{label}: rendered YAML did not reparse: {e}"));
            assert_eq!(
                reparsed["kind"].as_str(),
                Some("CustomResourceDefinition"),
                "{label}"
            );
        }
    }

    #[test]
    fn kind_names_are_the_five_specified_in_the_brief() {
        let kinds: Vec<String> = all_crds()
            .into_iter()
            .map(|(_, crd)| crd.spec.names.kind)
            .collect();
        for expected in [
            "Homeserver",
            "AppService",
            "Bridge",
            "PushGateway",
            "IdentityService",
        ] {
            assert!(
                kinds.iter().any(|k| k == expected),
                "missing kind {expected}"
            );
        }
    }
}
