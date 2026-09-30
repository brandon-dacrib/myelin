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
    /// Connection pool size: the most open connections this replica holds. Corresponds to
    /// Synapse's `database.args.cp_max`.
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
    /// The PostgreSQL schema every table lives in; created at startup if missing. One safe SQL
    /// identifier (`[A-Za-z_][A-Za-z0-9_]*`, at most 55 bytes). Several homeservers can share
    /// one database by each taking a schema of their own.
    #[serde(default = "default_pg_schema")]
    pub schema: String,
    /// Whether and how the connection is encrypted, in libpq's terms: `disable` (in the clear),
    /// `prefer` (encrypted when the server offers it, no certificate check; the default),
    /// `require` (encrypted or refused, no certificate check), `verify-ca` (encrypted, and the
    /// server's certificate chains to `ssl_root_cert`), or `verify-full` (`verify-ca`, and the
    /// certificate names `host`). Corresponds to Synapse's `database.args.sslmode`, which libpq
    /// reads (Synapse's `allow` is treated as `prefer`).
    ///
    /// The earlier boolean `tls` key still loads, as an alias: `tls: true` is `require` and
    /// `tls: false` is `disable`. A file with both keys is refused as a duplicate.
    #[serde(default, alias = "tls", deserialize_with = "deserialize_ssl_mode")]
    #[schemars(with = "PostgresSslMode")]
    pub ssl_mode: PostgresSslMode,
    /// A PEM file of CA certificates the server's certificate must chain to, for the
    /// `verify-ca` and `verify-full` modes. When unset, those modes use the platform's trust
    /// store. Corresponds to Synapse's `database.args.sslrootcert`. A self-signed server
    /// certificate is its own root here, as long as it is not marked as a CA
    /// (`basicConstraints=CA:FALSE`; OpenSSL's `req -x509` marks one as a CA by default, and
    /// such a certificate is refused as a server certificate).
    #[serde(default)]
    pub ssl_root_cert: Option<PathBuf>,
}

/// libpq's `sslmode` values, as `storage.postgres.ssl_mode` takes them. See
/// [`PostgresStorageConfig::ssl_mode`] for what each one checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PostgresSslMode {
    /// Never negotiate TLS.
    Disable,
    /// Encrypt when the server offers it; no certificate check. The default.
    #[default]
    Prefer,
    /// Encrypt or refuse; no certificate check.
    Require,
    /// Encrypt, and the server's certificate must chain to the trusted CA.
    VerifyCa,
    /// `verify-ca`, and the certificate must name the host connected to.
    VerifyFull,
}

impl PostgresSslMode {
    /// The libpq spelling (`disable`, `prefer`, `require`, `verify-ca`, `verify-full`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PostgresSslMode::Disable => "disable",
            PostgresSslMode::Prefer => "prefer",
            PostgresSslMode::Require => "require",
            PostgresSslMode::VerifyCa => "verify-ca",
            PostgresSslMode::VerifyFull => "verify-full",
        }
    }
}

impl std::fmt::Display for PostgresSslMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Reads `ssl_mode` as one of its names, or the older `tls` boolean (see
/// [`PostgresStorageConfig::ssl_mode`]).
fn deserialize_ssl_mode<'de, D>(deserializer: D) -> Result<PostgresSslMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Bool(bool),
        Mode(PostgresSslMode),
    }
    match Wire::deserialize(deserializer) {
        Ok(Wire::Bool(true)) => Ok(PostgresSslMode::Require),
        Ok(Wire::Bool(false)) => Ok(PostgresSslMode::Disable),
        Ok(Wire::Mode(mode)) => Ok(mode),
        Err(_) => Err(serde::de::Error::custom(
            "expected one of disable, prefer, require, verify-ca, verify-full (or the older \
             `tls: true`/`false`)",
        )),
    }
}

fn default_pg_port() -> u16 {
    5432
}

fn default_pool_size() -> u32 {
    10
}

fn default_pg_schema() -> String {
    "public".to_owned()
}

/// Whether `name` is one safe, unquoted SQL identifier, the shape `hs-kv` accepts for a schema.
fn is_safe_sql_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && name.len() <= 55
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
                if !is_safe_sql_identifier(&p.schema) {
                    errors.push(
                        format!("{prefix}.postgres.schema"),
                        format!(
                            "{:?} must be one SQL identifier: a letter or underscore, then \
                             letters, digits or underscores, at most 55 bytes",
                            p.schema
                        ),
                    );
                }
                if p.ssl_root_cert.is_some()
                    && !matches!(
                        p.ssl_mode,
                        PostgresSslMode::VerifyCa | PostgresSslMode::VerifyFull
                    )
                {
                    errors.push(
                        format!("{prefix}.postgres.ssl_root_cert"),
                        format!(
                            "is only used by ssl_mode verify-ca or verify-full, not {}; set one of \
                             those, or drop it",
                            p.ssl_mode
                        ),
                    );
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
            schema: default_pg_schema(),
            ssl_mode: PostgresSslMode::default(),
            ssl_root_cert: None,
        })
        .validate("storage", &mut errors);
        assert_eq!(errors.0.len(), 3);
    }

    fn postgres(yaml: &str) -> PostgresStorageConfig {
        let base = "backend: postgres\nhost: db\ndatabase: hs\nuser: hs\n";
        match serde_yaml_ng::from_str(&format!("{base}{yaml}")).unwrap() {
            StorageConfig::Postgres(p) => p,
            other => panic!("expected postgres, got {other:?}"),
        }
    }

    #[test]
    fn postgres_defaults_are_prefer_public_and_ten() {
        let p = postgres("");
        assert_eq!(p.ssl_mode, PostgresSslMode::Prefer);
        assert_eq!(p.ssl_root_cert, None);
        assert_eq!(p.schema, "public");
        assert_eq!(p.pool_size, 10);
        let mut errors = ValidationErrors::new();
        StorageConfig::Postgres(p).validate("storage", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn ssl_mode_takes_every_libpq_name() {
        for (name, mode) in [
            ("disable", PostgresSslMode::Disable),
            ("prefer", PostgresSslMode::Prefer),
            ("require", PostgresSslMode::Require),
            ("verify-ca", PostgresSslMode::VerifyCa),
            ("verify-full", PostgresSslMode::VerifyFull),
        ] {
            assert_eq!(postgres(&format!("ssl_mode: {name}\n")).ssl_mode, mode);
            assert_eq!(mode.to_string(), name);
        }
        let err = serde_yaml_ng::from_str::<StorageConfig>(
            "backend: postgres\nhost: db\ndatabase: hs\nuser: hs\nssl_mode: allow\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("verify-full"), "{err}");
    }

    #[test]
    fn the_old_tls_boolean_is_an_alias_for_require_or_disable() {
        assert_eq!(postgres("tls: true\n").ssl_mode, PostgresSslMode::Require);
        assert_eq!(postgres("tls: false\n").ssl_mode, PostgresSslMode::Disable);
        let both = serde_yaml_ng::from_str::<StorageConfig>(
            "backend: postgres\nhost: db\ndatabase: hs\nuser: hs\ntls: true\nssl_mode: prefer\n",
        );
        assert!(both.is_err(), "both keys at once must be refused");
    }

    #[test]
    fn ssl_mode_serializes_under_its_new_name() {
        let yaml =
            serde_yaml_ng::to_string(&StorageConfig::Postgres(postgres("tls: true\n"))).unwrap();
        assert!(yaml.contains("ssl_mode: require"), "{yaml}");
        assert!(!yaml.contains("tls:"), "{yaml}");
    }

    #[test]
    fn a_root_cert_needs_a_verify_mode_and_a_schema_must_be_an_identifier() {
        let mut errors = ValidationErrors::new();
        StorageConfig::Postgres(postgres(
            "ssl_mode: require\nssl_root_cert: /etc/hs/pg-ca.pem\nschema: hs-prod\n",
        ))
        .validate("storage", &mut errors);
        let paths: Vec<&str> = errors.0.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["storage.postgres.schema", "storage.postgres.ssl_root_cert"]
        );

        let mut errors = ValidationErrors::new();
        StorageConfig::Postgres(postgres(
            "ssl_mode: verify-full\nssl_root_cert: /etc/hs/pg-ca.pem\nschema: hs_prod\n",
        ))
        .validate("storage", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
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
