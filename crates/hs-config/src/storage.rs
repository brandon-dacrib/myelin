//! Storage backend selection: embedded (Fjall), PostgreSQL, or SlateDB on
//! object storage. See `PLAN.md` section 6.5.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};
use crate::secret::{SecretString, resolve_secret_pair};
use crate::{ConfigError, Duration};

/// Which storage backend this replica uses, and its backend-specific
/// settings. Restart required to change (see [`crate::reload`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum StorageConfig {
    /// Fjall, embedded in-process. Single node, the small-ARM-host mode,
    /// and tests.
    Embedded(EmbeddedStorageConfig),
    /// PostgreSQL. The default clustered backend.
    Postgres(PostgresStorageConfig),
    /// SlateDB on object storage. Diskless clusters.
    Slatedb(SlatedbStorageConfig),
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig::Embedded(EmbeddedStorageConfig::default())
    }
}

/// Settings for the embedded Fjall backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedStorageConfig {
    /// Directory holding the embedded database files.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

impl Default for EmbeddedStorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
        }
    }
}

/// Settings for the PostgreSQL backend. Corresponds to Synapse's
/// `database` block with `name: psycopg2`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PostgresStorageConfig {
    /// Database host.
    pub host: String,
    /// Database port.
    #[serde(default = "default_pg_port")]
    pub port: u16,
    /// Database name.
    pub database: String,
    /// Connecting role.
    pub user: String,
    /// Inline password. Prefer `password_file`.
    #[serde(default)]
    pub password: SecretString,
    /// Path to a file containing the password.
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    /// Connection pool size. Corresponds to Synapse's
    /// `database.args.cp_max`.
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
    /// Require TLS for the connection.
    #[serde(default)]
    pub tls: bool,
}

fn default_pg_port() -> u16 {
    5432
}

fn default_pool_size() -> u32 {
    10
}

/// Settings for the SlateDB-on-object-storage backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SlatedbStorageConfig {
    /// The object store URL (`s3://bucket/prefix`, `gs://...`,
    /// `az://...`), passed to the `object_store` crate.
    pub bucket_url: String,
    /// Number of virtual storage shards. Fixed at cluster creation; see
    /// `docs/rfcs/0001-cluster-ownership.md`.
    #[serde(default = "default_shard_count")]
    pub shard_count: u32,
    /// How long a writer may go without renewing its manifest lease before
    /// another replica is allowed to fence it and take over the shard.
    #[serde(default = "default_lease_duration")]
    pub lease_duration: Duration,
}

fn default_shard_count() -> u32 {
    256
}

fn default_lease_duration() -> Duration {
    Duration::from_secs(30)
}

impl Validate for StorageConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        match self {
            StorageConfig::Embedded(e) => {
                if e.data_dir.as_os_str().is_empty() {
                    errors.push(format!("{prefix}.embedded.data_dir"), "must not be empty");
                }
            }
            StorageConfig::Postgres(p) => {
                if p.host.trim().is_empty() {
                    errors.push(format!("{prefix}.postgres.host"), "must not be empty");
                }
                if p.database.trim().is_empty() {
                    errors.push(format!("{prefix}.postgres.database"), "must not be empty");
                }
                if p.user.trim().is_empty() {
                    errors.push(format!("{prefix}.postgres.user"), "must not be empty");
                }
                if p.pool_size == 0 {
                    errors.push(format!("{prefix}.postgres.pool_size"), "must be at least 1");
                }
            }
            StorageConfig::Slatedb(s) => {
                if s.bucket_url.trim().is_empty() {
                    errors.push(format!("{prefix}.slatedb.bucket_url"), "must not be empty");
                } else if !["s3://", "gs://", "az://", "memory://"]
                    .iter()
                    .any(|scheme| s.bucket_url.starts_with(scheme))
                {
                    errors.push(
                        format!("{prefix}.slatedb.bucket_url"),
                        format!(
                            "{:?} must start with s3://, gs://, az:// or memory://",
                            s.bucket_url
                        ),
                    );
                }
                if s.shard_count == 0 {
                    errors.push(
                        format!("{prefix}.slatedb.shard_count"),
                        "must be at least 1",
                    );
                }
            }
        }
    }
}

impl StorageConfig {
    /// Resolves any `*_file` secrets this backend carries.
    pub(crate) fn resolve_secrets(&mut self, prefix: &str) -> Result<(), ConfigError> {
        if let StorageConfig::Postgres(p) = self {
            resolve_secret_pair(
                &format!("{prefix}.postgres.password"),
                &mut p.password,
                &p.password_file,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_is_valid() {
        let mut errors = ValidationErrors::new();
        StorageConfig::default().validate("storage", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn postgres_requires_host_database_user() {
        let mut errors = ValidationErrors::new();
        StorageConfig::Postgres(PostgresStorageConfig {
            host: String::new(),
            port: default_pg_port(),
            database: String::new(),
            user: String::new(),
            password: SecretString::default(),
            password_file: None,
            pool_size: default_pool_size(),
            tls: false,
        })
        .validate("storage", &mut errors);
        assert_eq!(errors.0.len(), 3);
    }

    #[test]
    fn slatedb_rejects_unknown_scheme() {
        let mut errors = ValidationErrors::new();
        StorageConfig::Slatedb(SlatedbStorageConfig {
            bucket_url: "ftp://example/bucket".into(),
            shard_count: default_shard_count(),
            lease_duration: default_lease_duration(),
        })
        .validate("storage", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert!(errors.0[0].message.contains("s3://"));
    }

    #[test]
    fn tag_field_round_trips() {
        let yaml = "backend: postgres\nhost: db\ndatabase: hs\nuser: hs\n";
        let cfg: StorageConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(cfg, StorageConfig::Postgres(_)));
    }
}
