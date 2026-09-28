//! Where to migrate from: the Synapse deployment the admin API's Migration area copies
//! (`migration.*`, `hs_compat::migration`).
//!
//! An administered section (decision 0010): the Migration page of the interface writes it through
//! `config.update` when an operator points the server at a Synapse database, and
//! `POST /api/v1/migration/start` names it (`source_secret_ref`, `/migration/synapse` by default)
//! rather than carrying a connection string itself. The password is a secret: stored, never
//! returned. Nothing here is read at startup; a migration reads it when it starts, so it is
//! reloadable.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};
use crate::secret::SecretString;

/// The migration source, if any.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationConfig {
    /// The Synapse deployment to migrate from. Unset: there is nothing to migrate.
    #[serde(default)]
    pub synapse: Option<SynapseSourceConfig>,
}

/// A Synapse deployment to copy: its PostgreSQL database and, optionally, its media store. The
/// database is only ever read; Synapse keeps working until cutover.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SynapseSourceConfig {
    /// Synapse's PostgreSQL database: the `database.args` of its `homeserver.yaml`. A Synapse on
    /// SQLite is moved to PostgreSQL first, with Synapse's own `synapse_port_db`.
    pub database: SynapseDatabaseConfig,
    /// Synapse's `media_store_path`, as this server sees it (the same volume, mounted). Unset:
    /// media records are copied but the files are not, and local media is missing until they
    /// are.
    #[serde(default)]
    pub media_store_path: Option<PathBuf>,
    /// Rows read from Synapse per batch. Each batch is one step the copy can be paused, resumed
    /// or stopped between.
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
}

/// A connection to Synapse's PostgreSQL database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SynapseDatabaseConfig {
    /// Database host. Synapse's `database.args.host`.
    pub host: String,
    /// Database port. Synapse's `database.args.port`.
    #[serde(default = "default_pg_port")]
    pub port: u16,
    /// Database name. Synapse's `database.args.database` (or `dbname`).
    pub database: String,
    /// Connecting role. A read-only role is enough: nothing is ever written. Synapse's
    /// `database.args.user`.
    pub user: String,
    /// The role's password. Synapse's `database.args.password`.
    #[serde(default)]
    pub password: SecretString,
}

fn default_pg_port() -> u16 {
    5432
}

fn default_batch_size() -> u32 {
    500
}

impl SynapseDatabaseConfig {
    /// A `tokio_postgres`-style key/value connection string. It holds the password: never log
    /// it, never answer it.
    #[must_use]
    pub fn connection_string(&self) -> String {
        fn quote(value: &str) -> String {
            format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
        }
        let mut dsn = format!(
            "host={} port={} dbname={} user={}",
            quote(&self.host),
            self.port,
            quote(&self.database),
            quote(&self.user)
        );
        if let Some(password) = &self.password.0 {
            dsn.push_str(&format!(" password={}", quote(password)));
        }
        dsn
    }

    /// `user@host:port/database`: what the database is, without the password, for logs, the
    /// audit log and `MigrationStatus.source`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "postgresql://{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
    }
}

impl Validate for MigrationConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        let Some(synapse) = &self.synapse else {
            return;
        };
        let at = format!("{prefix}.synapse");
        if synapse.database.host.trim().is_empty() {
            errors.push(format!("{at}.database.host"), "must not be empty");
        }
        if synapse.database.database.trim().is_empty() {
            errors.push(format!("{at}.database.database"), "must not be empty");
        }
        if synapse.database.user.trim().is_empty() {
            errors.push(format!("{at}.database.user"), "must not be empty");
        }
        if synapse.database.port == 0 {
            errors.push(format!("{at}.database.port"), "must not be 0");
        }
        if synapse.batch_size == 0 || synapse.batch_size > 100_000 {
            errors.push(format!("{at}.batch_size"), "must be between 1 and 100000");
        }
        if let Some(path) = &synapse.media_store_path
            && !path.is_absolute()
        {
            errors.push(
                format!("{at}.media_store_path"),
                "must be an absolute path (Synapse's media_store_path, as this server sees it)",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn a_synapse_source_is_read_with_defaults_and_its_password_stays_hidden() {
        let config = Config::from_yaml(
            "server:\n  server_name: example.org\nmigration:\n  synapse:\n    database:\n      host: db\n      database: synapse\n      user: synapse_ro\n      password: \"it's secret\"\n",
        )
        .unwrap();
        let synapse = config.migration.synapse.unwrap();
        assert_eq!(synapse.database.port, 5432);
        assert_eq!(synapse.batch_size, 500);
        assert_eq!(synapse.media_store_path, None);
        assert_eq!(
            synapse.database.describe(),
            "postgresql://synapse_ro@db:5432/synapse"
        );
        assert_eq!(
            synapse.database.connection_string(),
            "host='db' port=5432 dbname='synapse' user='synapse_ro' password='it\\'s secret'"
        );
        assert!(!format!("{synapse:?}").contains("secret'"));
    }

    #[test]
    fn a_relative_media_store_and_an_empty_host_are_refused() {
        let err = Config::from_yaml(
            "server:\n  server_name: example.org\nmigration:\n  synapse:\n    database: {host: '', database: s, user: u}\n    media_store_path: media\n",
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("migration.synapse.database.host"), "{text}");
        assert!(
            text.contains("migration.synapse.media_store_path"),
            "{text}"
        );
    }
}
