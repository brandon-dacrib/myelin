//! The `Homeserver` custom resource: one `hs` replica set (single-node or clustered), the
//! operator's primary kind. Mirrors `deploy/helm/hs/values.yaml`'s shape closely — the chart and
//! this CRD are two ways to describe the same workload (`docs/status/12-platform-and-kubernetes.md`
//! records which one a given deployment should use; the operator does not manage resources the
//! chart also owns, and vice versa, to avoid two controllers fighting over the same StatefulSet).

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{ImageSpec, OperatorStatus, SecretKeyRef};

/// Which storage backend a `Homeserver` uses. See `hs_config::storage::StorageConfig` (owned by
/// track 13) for the native config counterpart this maps onto.
///
/// Modeled as `backend` plus one `Option<...>` block per backend — not a Rust enum with
/// `#[serde(tag = "backend")]`, even though that would be the more natural Rust shape and is
/// exactly what `hs_config::storage::StorageConfig` itself uses — because the Kubernetes
/// structural-schema OpenAPI v3 dialect a CRD's schema must satisfy cannot express "the schema of
/// property X differs per `oneOf` branch" for a *required* property shared across branches;
/// `kube`'s schema conversion rejects it outright (verified empirically: `backend`'s per-variant
/// `enum: [embedded]` / `enum: [postgres]` / `enum: [slatedb]` collide). A flat struct with one
/// discriminator field and a block per backend (only the block matching `backend` is meaningful;
/// the reconciler validates that, not the schema) is the standard workaround every kube-rs CRD
/// with this shape uses.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// Which of `embedded`/`postgres`/`slatedb` below is meaningful.
    pub backend: StorageBackend,
    /// Meaningful when `backend: embedded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<EmbeddedStorageSpec>,
    /// Meaningful when `backend: postgres`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres: Option<PostgresStorageSpec>,
    /// Meaningful when `backend: slatedb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slatedb: Option<SlatedbStorageSpec>,
}

/// The storage backend discriminator. See [`StorageSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum StorageBackend {
    /// Embedded Fjall storage on a `PersistentVolumeClaim`. Forces `spec.replicas` to `1`.
    Embedded,
    /// PostgreSQL, either CloudNativePG-managed or external.
    Postgres,
    /// SlateDB on object storage. Diskless clusters.
    Slatedb,
}

/// Embedded (Fjall) storage settings. See [`StorageSpec::embedded`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedStorageSpec {
    /// Requested PVC size, e.g. `"10Gi"`.
    pub size: String,
    /// `StorageClass` name; the cluster default is used if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
}

/// PostgreSQL storage settings. See [`StorageSpec::postgres`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PostgresStorageSpec {
    /// A CloudNativePG `Cluster` name in the same namespace, to read connection details from its
    /// generated `<name>-app` Secret. Mutually exclusive with `host` in practice (the reconciler
    /// prefers `cloud_native_pg_cluster` when both are set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_native_pg_cluster: Option<String>,
    /// An externally managed PostgreSQL host, for a non-CloudNativePG database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Database name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Connecting role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The Secret key holding the connection password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret_ref: Option<SecretKeyRef>,
}

/// SlateDB-on-object-storage settings. See [`StorageSpec::slatedb`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SlatedbStorageSpec {
    /// The object store URL (`s3://bucket/prefix`, ...).
    pub bucket_url: String,
    /// Number of virtual storage shards.
    #[serde(default = "default_shard_count")]
    pub shard_count: u32,
}

fn default_shard_count() -> u32 {
    256
}

/// `spec` of a `Homeserver`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, CustomResource)]
#[kube(
    group = "hs.matrix.org",
    version = "v1alpha1",
    kind = "Homeserver",
    namespaced,
    shortname = "hs",
    status = "OperatorStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"ServerName","type":"string","jsonPath":".spec.serverName"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct HomeserverSpec {
    /// The Matrix server name (`server.server_name`). Immutable after creation in practice
    /// (changing it after any room exists is not supported by any Matrix homeserver — see
    /// `hs_config::server::ServerConfig`'s doc comment); the reconciler does not enforce
    /// immutability itself (no Kubernetes CRD-level immutability marker for this yet), but the
    /// change would need a full new deployment in practice.
    pub server_name: String,
    /// The externally reachable base URL, if different from `https://{serverName}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    /// Number of replicas. Forced to `1` by the reconciler when `storage` is `Embedded`.
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    /// The `hs` image to run.
    pub image: ImageSpec,
    /// Storage backend.
    pub storage: StorageSpec,
    /// The Ed25519 signing key (`hs generate-signing-key` format), from an existing `Secret`.
    /// Required — the operator never generates or stores a signing key itself, matching
    /// `deploy/helm/hs`'s same policy (a signing key must survive the CR being deleted and
    /// recreated).
    pub signing_key_secret_ref: SecretKeyRef,
    /// `auth.registration_shared_secret`, from an existing `Secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_shared_secret_ref: Option<SecretKeyRef>,
    /// Extra native `hs-config` YAML, appended to the operator-generated `ConfigMap` the same way
    /// `deploy/helm/hs/values.yaml`'s `extraConfig` does.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_config: serde_json::Map<String, serde_json::Value>,
}

fn default_replicas() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HomeserverSpec {
        HomeserverSpec {
            server_name: "example.org".to_owned(),
            public_base_url: None,
            replicas: 3,
            image: ImageSpec {
                repository: "ghcr.io/matrix-org/hs".to_owned(),
                tag: Some("0.0.1".to_owned()),
                digest: None,
                pull_policy: None,
            },
            storage: StorageSpec {
                backend: StorageBackend::Postgres,
                embedded: None,
                postgres: Some(PostgresStorageSpec {
                    cloud_native_pg_cluster: Some("hs-pg".to_owned()),
                    host: None,
                    database: None,
                    user: None,
                    password_secret_ref: None,
                }),
                slatedb: None,
            },
            signing_key_secret_ref: SecretKeyRef {
                name: "hs-signing-key".to_owned(),
                key: "signing.key".to_owned(),
            },
            registration_shared_secret_ref: None,
            extra_config: serde_json::Map::new(),
        }
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        let back: HomeserverSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.server_name, spec.server_name);
        assert_eq!(back.replicas, spec.replicas);
    }

    #[test]
    fn spec_round_trips_through_yaml() {
        let spec = sample();
        let yaml = serde_yaml_ng::to_string(&spec).unwrap();
        let back: HomeserverSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back.server_name, spec.server_name);
    }

    #[test]
    fn storage_backend_is_camel_case() {
        let embedded = StorageSpec {
            backend: StorageBackend::Embedded,
            embedded: Some(EmbeddedStorageSpec {
                size: "10Gi".to_owned(),
                storage_class_name: None,
            }),
            postgres: None,
            slatedb: None,
        };
        let json = serde_json::to_value(&embedded).unwrap();
        assert_eq!(json["backend"], "embedded");
    }
}
