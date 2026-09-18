//! Cluster topology: single-node vs. multi-replica, shard counts and mesh
//! transport. See `docs/rfcs/0001-cluster-ownership.md` (track 03) and
//! `PLAN.md` D2. Restart required to change (see [`crate::reload`]): shard
//! counts and mesh identity are agreed with every other replica.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::Duration;
use crate::error::{Validate, ValidationErrors};
use crate::secret::{SecretString, resolve_secret_pair};

const fn default_true() -> bool {
    true
}

fn default_shard_count() -> u32 {
    256
}

fn default_mesh_port() -> u16 {
    8449
}

fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(2)
}

fn default_lease_ttl() -> Duration {
    Duration::from_secs(10)
}

/// Internal mesh transport between replicas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeshConfig {
    /// Port the mesh listener binds.
    #[serde(default = "default_mesh_port")]
    pub port: u16,
    /// mTLS material for mesh connections. `None` uses the shared-secret
    /// mode (tests and trusted networks only; see RFC 0001 section 16).
    #[serde(default)]
    pub tls: Option<crate::listeners::TlsConfig>,
    /// Inline shared secret for non-mTLS mesh auth. Prefer
    /// `shared_secret_file`.
    #[serde(default)]
    pub shared_secret: SecretString,
    /// Path to a file containing the mesh shared secret.
    #[serde(default)]
    pub shared_secret_file: Option<PathBuf>,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            port: default_mesh_port(),
            tls: None,
            shared_secret: SecretString::default(),
            shared_secret_file: None,
        }
    }
}

/// Cluster topology and ownership tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// Run as a single replica owning everything, with the ownership
    /// manager inert and no mesh listener. Corresponds to `hs serve
    /// --single-node`.
    #[serde(default = "default_true")]
    pub single_node: bool,
    /// Number of room ownership shards. Fixed at cluster creation.
    #[serde(default = "default_shard_count")]
    pub room_shards: u32,
    /// Number of user-session ownership shards. Fixed at cluster creation.
    #[serde(default = "default_shard_count")]
    pub user_shards: u32,
    /// Internal replica-to-replica mesh.
    #[serde(default)]
    pub mesh: MeshConfig,
    /// How often a replica renews its liveness heartbeat.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: Duration,
    /// How long a lease survives without a heartbeat before another
    /// replica may claim ownership.
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl: Duration,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            single_node: true,
            room_shards: default_shard_count(),
            user_shards: default_shard_count(),
            mesh: MeshConfig::default(),
            heartbeat_interval: default_heartbeat_interval(),
            lease_ttl: default_lease_ttl(),
        }
    }
}

impl Validate for ClusterConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.room_shards == 0 {
            errors.push(format!("{prefix}.room_shards"), "must be at least 1");
        }
        if self.user_shards == 0 {
            errors.push(format!("{prefix}.user_shards"), "must be at least 1");
        }
        if self.heartbeat_interval.is_zero() {
            errors.push(
                format!("{prefix}.heartbeat_interval"),
                "must be greater than 0",
            );
        }
        if !self.single_node && self.lease_ttl.as_millis() <= self.heartbeat_interval.as_millis() {
            errors.push(
                format!("{prefix}.lease_ttl"),
                format!(
                    "must be greater than heartbeat_interval ({} <= {}), or every missed heartbeat triggers a failover",
                    self.lease_ttl, self.heartbeat_interval
                ),
            );
        }
    }
}

impl ClusterConfig {
    /// Resolves any `*_file` secrets this section carries.
    pub(crate) fn resolve_secrets(&mut self, prefix: &str) -> Result<(), ConfigError> {
        resolve_secret_pair(
            &format!("{prefix}.mesh.shared_secret"),
            &mut self.mesh.shared_secret,
            &self.mesh.shared_secret_file,
        )
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        ClusterConfig::default().validate("cluster", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_lease_ttl_not_greater_than_heartbeat_when_clustered() {
        let mut cfg = ClusterConfig {
            single_node: false,
            ..Default::default()
        };
        cfg.lease_ttl = cfg.heartbeat_interval;
        let mut errors = ValidationErrors::new();
        cfg.validate("cluster", &mut errors);
        assert!(errors.0.iter().any(|e| e.path == "cluster.lease_ttl"));
    }

    #[test]
    fn allows_equal_ttl_in_single_node_mode() {
        // Single-node mode has no failover, so the relationship does not
        // matter operationally; only clustered mode is checked.
        let mut cfg = ClusterConfig::default();
        cfg.lease_ttl = cfg.heartbeat_interval;
        let mut errors = ValidationErrors::new();
        cfg.validate("cluster", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_zero_shards() {
        let mut cfg = ClusterConfig::default();
        cfg.room_shards = 0;
        cfg.user_shards = 0;
        let mut errors = ValidationErrors::new();
        cfg.validate("cluster", &mut errors);
        assert_eq!(errors.0.len(), 2);
    }
}
