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

/// Mutual-TLS material for the mesh: this replica's own certificate and key, and the private
/// cluster CA every peer's certificate must chain to. All three are file paths (the Kubernetes
/// convention for TLS material is a mounted `kubernetes.io/tls` Secret, which is how the chart
/// supplies them), so there is no inline/`*_file` pair here the way secrets have.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeshTlsConfig {
    /// PEM certificate chain this replica presents to peers, leaf first. Its DNS subject
    /// alternative names must include the host part of `advertise_address`, because a peer
    /// verifies this replica's certificate against the name it dialled; in the chart every pod
    /// shares one wildcard certificate for the headless Service's domain.
    pub certificate_path: PathBuf,
    /// PEM private key for `certificate_path`.
    pub private_key_path: PathBuf,
    /// PEM certificate(s) of the private cluster CA. A peer whose certificate does not chain to
    /// this CA is refused at the TLS handshake, before any mesh request is read. Never a public
    /// CA: the mesh is a closed set of replicas under one operator.
    pub ca_certificate_path: PathBuf,
    /// If set, a peer's certificate must additionally carry a DNS subject alternative name
    /// ending in this suffix (for example `.hs-headless.matrix.svc.cluster.local`), so a
    /// certificate the same CA issued for something other than this cluster's pods is refused
    /// too. Unset means any certificate chained to the CA is accepted.
    #[serde(default)]
    pub peer_san_suffix: Option<String>,
}

/// Internal mesh transport between replicas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeshConfig {
    /// Port the mesh listener binds.
    #[serde(default = "default_mesh_port")]
    pub port: u16,
    /// The address peers dial to reach *this* replica's mesh listener: a host name or IP
    /// address, optionally with a `:port` (an IPv6 address must be written in brackets). This
    /// is what the replica writes into the shared replica registry, so every other replica
    /// forwards requests here; it must be reachable from every peer and, with `tls` set, must
    /// be a name the certificate in `tls.certificate_path` is valid for. Without a port,
    /// `port` is used. Unset, a clustered replica falls back to its first listener's bind
    /// address (or `127.0.0.1` for a wildcard bind) and logs a warning: right only when every
    /// replica shares one host, as in a local two-process test. In Kubernetes set it per pod
    /// from the environment (`HS__CLUSTER__MESH__ADVERTISE_ADDRESS`) to the pod's stable DNS
    /// name, `<pod>.<headless service>.<namespace>.svc.<cluster domain>`; the chart does this.
    /// Ignored in single-node mode.
    #[serde(default)]
    pub advertise_address: Option<String>,
    /// Mutual TLS for mesh connections: every replica presents a certificate and verifies its
    /// peer's against a private CA, so nothing but another replica of this cluster can forward
    /// a request or be forwarded one. `None` uses the shared-secret mode (tests and trusted
    /// networks only; see RFC 0001 section 16).
    #[serde(default)]
    pub tls: Option<MeshTlsConfig>,
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
            advertise_address: None,
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
        if let Some(addr) = &self.mesh.advertise_address
            && addr.trim().is_empty()
        {
            errors.push(
                format!("{prefix}.mesh.advertise_address"),
                "must not be empty when set (unset it to fall back to the listener's bind address)",
            );
        }
        if let Some(tls) = &self.mesh.tls {
            for (name, path) in [
                ("certificate_path", &tls.certificate_path),
                ("private_key_path", &tls.private_key_path),
                ("ca_certificate_path", &tls.ca_certificate_path),
            ] {
                if path.as_os_str().is_empty() {
                    errors.push(format!("{prefix}.mesh.tls.{name}"), "must not be empty");
                }
            }
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
    fn mesh_advertise_address_and_tls_parse_from_yaml() {
        let cfg: ClusterConfig = serde_yaml_ng::from_str(concat!(
            "single_node: false\n",
            "mesh:\n",
            "  port: 8449\n",
            "  advertise_address: hs-0.hs-headless.matrix.svc.cluster.local\n",
            "  tls:\n",
            "    certificate_path: /etc/hs/secrets/mesh-tls/tls.crt\n",
            "    private_key_path: /etc/hs/secrets/mesh-tls/tls.key\n",
            "    ca_certificate_path: /etc/hs/secrets/mesh-tls/ca.crt\n",
            "    peer_san_suffix: .hs-headless.matrix.svc.cluster.local\n",
        ))
        .unwrap();
        assert_eq!(
            cfg.mesh.advertise_address.as_deref(),
            Some("hs-0.hs-headless.matrix.svc.cluster.local")
        );
        let tls = cfg.mesh.tls.as_ref().expect("tls parsed");
        assert_eq!(
            tls.ca_certificate_path,
            PathBuf::from("/etc/hs/secrets/mesh-tls/ca.crt")
        );
        assert_eq!(
            tls.peer_san_suffix.as_deref(),
            Some(".hs-headless.matrix.svc.cluster.local")
        );
        let mut errors = ValidationErrors::new();
        cfg.validate("cluster", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn mesh_tls_peer_san_suffix_is_optional() {
        let cfg: ClusterConfig = serde_yaml_ng::from_str(concat!(
            "mesh:\n",
            "  tls:\n",
            "    certificate_path: a.pem\n",
            "    private_key_path: b.pem\n",
            "    ca_certificate_path: ca.pem\n",
        ))
        .unwrap();
        assert_eq!(cfg.mesh.tls.unwrap().peer_san_suffix, None);
    }

    #[test]
    fn mesh_tls_requires_the_ca() {
        // A certificate and key alone are what a public-facing listener needs; the mesh also
        // needs the CA it verifies peers against, and a config that forgot it must not parse
        // into "mutual TLS without verification".
        let err = serde_yaml_ng::from_str::<ClusterConfig>(concat!(
            "mesh:\n",
            "  tls:\n",
            "    certificate_path: a.pem\n",
            "    private_key_path: b.pem\n",
        ))
        .unwrap_err();
        assert!(err.to_string().contains("ca_certificate_path"), "{err}");
    }

    #[test]
    fn rejects_empty_advertise_address_and_empty_tls_paths() {
        let mut cfg = ClusterConfig::default();
        cfg.mesh.advertise_address = Some("  ".into());
        cfg.mesh.tls = Some(MeshTlsConfig {
            certificate_path: PathBuf::new(),
            private_key_path: PathBuf::from("k.pem"),
            ca_certificate_path: PathBuf::from("ca.pem"),
            peer_san_suffix: None,
        });
        let mut errors = ValidationErrors::new();
        cfg.validate("cluster", &mut errors);
        let paths: Vec<_> = errors.0.iter().map(|e| e.path.as_str()).collect();
        assert!(
            paths.contains(&"cluster.mesh.advertise_address"),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"cluster.mesh.tls.certificate_path"),
            "{paths:?}"
        );
        assert_eq!(errors.0.len(), 2, "{paths:?}");
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
