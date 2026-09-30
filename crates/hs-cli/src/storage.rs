//! Opens the `hs-kv` storage backend a native config selects.
//!
//! `hs_config::StorageConfig` has three variants. Two of them open: `Embedded`
//! ([`hs_kv::fjall_backend::FjallBackend`]) and `Postgres`
//! ([`hs_kv::postgres_backend::PostgresBackend`], which passes the same `hs-kv` conformance suite
//! — see `docs/status/01-storage-engine.md` for the two documented divergences and what they mean
//! for range-scan fencing). `Slatedb` has no `hs-kv` implementation and fails here with a clear,
//! actionable error rather than silently falling back to something else.
//!
//! A second, deeper gap this module cannot paper over: even for `Embedded`, nothing downstream
//! consumes the opened [`hs_kv::fjall_backend::FjallBackend`] yet. `hs-auth`'s
//! [`hs_auth::store::AuthStore`] trait (the seam it was designed around, see that crate's
//! `store` module docs) has exactly one implementation today,
//! [`hs_auth::store::memory::InMemoryAuthStore`] — there is no `hs-kv`-backed `AuthStore`. `hs
//! serve` therefore opens the configured backend (proving the config plumbing works end to end,
//! and giving future consumers — an `hs-kv`-backed `AuthStore`, `hs-tables`, room storage — a
//! place to plug in) but every request in this milestone is still served from the in-memory auth
//! store regardless of `storage.backend`. This is recorded as an interface needed from track 01
//! (and 07, for the `AuthStore` impl) in the status file, not fixed here (`hs-cli` does not edit
//! other tracks' crates).

use std::path::Path;

/// Errors opening the configured storage backend.
#[derive(Debug, thiserror::Error)]
pub enum StorageOpenError {
    /// `storage.backend` selected a backend `hs-kv` does not implement yet.
    #[error(
        "storage.backend = {backend:?} has no hs-kv implementation yet (embedded and postgres \
         are wired up in `hs serve` today); see docs/status/01-storage-engine.md"
    )]
    BackendNotImplemented {
        /// The backend name, for the error message (`"slatedb"`).
        backend: &'static str,
    },
    /// The PostgreSQL database could not be opened.
    #[error("failed to open the postgres storage backend at {host}:{port}/{database}: {source}")]
    Postgres {
        /// The configured host, for the error message.
        host: String,
        /// The configured port.
        port: u16,
        /// The configured database name.
        database: String,
        /// The underlying `hs-kv` error.
        #[source]
        source: hs_kv::KvError,
    },
    /// `storage.postgres.ssl_mode` asked for TLS (or a certificate check) the server could not
    /// satisfy, or its root certificate file could not be used.
    #[error(
        "storage.postgres.ssl_mode = {ssl_mode} could not be satisfied connecting to \
         {host}:{port}/{database}: {detail}. Give the server TLS (or, for verify-ca and \
         verify-full, a certificate that chains to storage.postgres.ssl_root_cert and, for \
         verify-full, names {host}), or lower ssl_mode on purpose."
    )]
    PostgresTls {
        /// The mode asked for.
        ssl_mode: hs_config::storage::PostgresSslMode,
        /// The configured host, for the error message.
        host: String,
        /// The configured port.
        port: u16,
        /// The configured database name.
        database: String,
        /// What the TLS layer said.
        detail: String,
    },
    /// The embedded Fjall database's directory could not be created.
    #[error("failed to create the data directory {path:?}: {source}")]
    FjallDataDir {
        /// The directory that could not be created.
        path: std::path::PathBuf,
        /// The underlying `hs-kv` error.
        #[source]
        source: hs_kv::KvError,
    },
    /// The embedded Fjall database could not be opened.
    ///
    /// Overwhelmingly the reason is that something else already has it open — the embedded
    /// backend takes an exclusive lock on its directory, and `hs config` against a running server
    /// hits this every time. The underlying error says `Locked` and nothing else, which is true
    /// and useless, so the message names the two ways out.
    #[error(
        "failed to open the embedded storage backend at {path:?}: {source}\n\
         If this says `Locked`, another process already has this database open — most likely \
         `hs serve`. Stop the server first, or make the change through the admin web interface, \
         which can apply it without a restart."
    )]
    Fjall {
        /// The data directory that failed to open.
        path: std::path::PathBuf,
        /// The underlying `hs-kv` error.
        #[source]
        source: hs_kv::KvError,
    },
}

/// The storage backend `hs serve` opened, or a note explaining why it could not for a backend
/// that has no `hs-kv` implementation yet (see this module's docs).
pub enum OpenedStorage {
    /// The embedded Fjall backend, opened at its configured data directory.
    Embedded(hs_kv::fjall_backend::FjallBackend),
    /// The PostgreSQL backend, connected to its configured database.
    Postgres(hs_kv::postgres_backend::PostgresBackend),
}

impl std::fmt::Debug for OpenedStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenedStorage::Embedded(_) => f.write_str("OpenedStorage::Embedded(..)"),
            OpenedStorage::Postgres(_) => f.write_str("OpenedStorage::Postgres(..)"),
        }
    }
}

/// Opens the storage backend `config.storage` selects.
///
/// # Errors
/// Returns [`StorageOpenError::BackendNotImplemented`] for `slatedb` (no `hs-kv` backend exists
/// for it), [`StorageOpenError::FjallDataDir`] if the embedded backend's data directory could not
/// be created, [`StorageOpenError::Fjall`] if it could not be opened (usually because another
/// process holds its lock), or
/// [`StorageOpenError::Postgres`]/[`StorageOpenError::PostgresTls`] for the PostgreSQL backend.
pub fn open_storage(config: &hs_config::StorageConfig) -> Result<OpenedStorage, StorageOpenError> {
    match config {
        hs_config::StorageConfig::Embedded(embedded) => {
            open_embedded(&embedded.data_dir).map(OpenedStorage::Embedded)
        }
        hs_config::StorageConfig::Postgres(pg) => open_postgres(pg).map(OpenedStorage::Postgres),
        hs_config::StorageConfig::Slatedb(_) => {
            Err(StorageOpenError::BackendNotImplemented { backend: "slatedb" })
        }
    }
}

/// Opens the PostgreSQL backend from its configured connection fields: `ssl_mode` and
/// `ssl_root_cert`, `schema` and `pool_size` all reach `PostgresBackend::open_with`. A TLS
/// failure comes back as [`StorageOpenError::PostgresTls`], naming the setting.
fn open_postgres(
    config: &hs_config::storage::PostgresStorageConfig,
) -> Result<hs_kv::postgres_backend::PostgresBackend, StorageOpenError> {
    let mut dsn = format!(
        "host={} port={} dbname={} user={}",
        dsn_value(&config.host),
        config.port,
        dsn_value(&config.database),
        dsn_value(&config.user),
    );
    // An empty `password=` is a parse error to the `postgres` crate ("unexpected EOF"), not an
    // empty password, so a config without one (peer or trust authentication) leaves it out.
    if let Some(password) = config.password.as_str().filter(|p| !p.is_empty()) {
        dsn.push_str(" password=");
        dsn.push_str(&dsn_value(password));
    }
    let options = hs_kv::postgres_backend::PostgresOpenOptions {
        schema: config.schema.clone(),
        pool_size: config.pool_size,
        tls: hs_kv::postgres_tls::PgTlsOptions {
            mode: ssl_mode_of(config.ssl_mode),
            root_cert: config.ssl_root_cert.clone(),
        },
    };
    hs_kv::postgres_backend::PostgresBackend::open_with(&dsn, &options).map_err(|source| {
        let tls = std::error::Error::source(&source)
            .and_then(|e| e.downcast_ref::<hs_kv::postgres_tls::PgTlsError>());
        match tls {
            Some(tls) => StorageOpenError::PostgresTls {
                ssl_mode: config.ssl_mode,
                host: config.host.clone(),
                port: config.port,
                database: config.database.clone(),
                detail: tls.detail.clone(),
            },
            None => StorageOpenError::Postgres {
                host: config.host.clone(),
                port: config.port,
                database: config.database.clone(),
                source,
            },
        }
    })
}

/// A value in the `postgres` crate's key-value connection string: single-quoted, with `\` and
/// `'` escaped, so a password with a space or a quote in it arrives intact.
fn dsn_value(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        if c == '\\' || c == '\'' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// The configuration's mode as `hs-kv`'s. The two enums are the same five names; `hs-config`
/// keeps its own so the schema and the storage crate do not depend on each other.
fn ssl_mode_of(mode: hs_config::storage::PostgresSslMode) -> hs_kv::postgres_tls::PgSslMode {
    use hs_config::storage::PostgresSslMode as C;
    use hs_kv::postgres_tls::PgSslMode as K;
    match mode {
        C::Disable => K::Disable,
        C::Prefer => K::Prefer,
        C::Require => K::Require,
        C::VerifyCa => K::VerifyCa,
        C::VerifyFull => K::VerifyFull,
    }
}

fn open_embedded(path: &Path) -> Result<hs_kv::fjall_backend::FjallBackend, StorageOpenError> {
    std::fs::create_dir_all(path).map_err(|e| StorageOpenError::FjallDataDir {
        path: path.to_owned(),
        source: hs_kv::KvError::backend(e),
    })?;
    hs_kv::fjall_backend::FjallBackend::open(path).map_err(|source| StorageOpenError::Fjall {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_an_embedded_backend_in_a_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config =
            hs_config::StorageConfig::Embedded(hs_config::storage::EmbeddedStorageConfig {
                data_dir: dir.path().to_owned(),
            });
        let opened = open_storage(&config).unwrap();
        let OpenedStorage::Embedded(backend) = opened else {
            panic!("an embedded config must open the embedded backend");
        };
        // Prove the handle actually works: open a keyspace and do a trivial round trip.
        use hs_kv::KvBackend as _;
        let ks = backend.keyspace("healthcheck").unwrap();
        let mut txn = backend.begin().unwrap();
        {
            use hs_kv::KvWrite as _;
            txn.put(&ks, b"k", b"v").unwrap();
        }
        assert!(backend.commit(txn).unwrap().is_ok());
    }

    fn postgres_config(
        ssl_mode: hs_config::storage::PostgresSslMode,
        ssl_root_cert: Option<std::path::PathBuf>,
    ) -> hs_config::StorageConfig {
        hs_config::StorageConfig::Postgres(hs_config::storage::PostgresStorageConfig {
            // Deliberately unroutable: this test is about which error comes back, not about
            // reaching a database. Real-database tests live in `hs-kv`'s own suite and in
            // `tests/postgres_tls.rs`.
            host: "127.0.0.1".into(),
            port: 1,
            database: "hs".into(),
            user: "hs".into(),
            password: hs_config::SecretString::default(),
            password_file: None,
            pool_size: 10,
            schema: "public".into(),
            ssl_mode,
            ssl_root_cert,
        })
    }

    #[test]
    fn an_unusable_root_certificate_names_the_ssl_mode_setting_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-ca.pem");
        let err = open_storage(&postgres_config(
            hs_config::storage::PostgresSslMode::VerifyFull,
            Some(missing),
        ))
        .unwrap_err();
        match &err {
            StorageOpenError::PostgresTls {
                ssl_mode, detail, ..
            } => {
                assert_eq!(*ssl_mode, hs_config::storage::PostgresSslMode::VerifyFull);
                assert!(detail.contains("missing-ca.pem"), "{detail}");
            }
            other => panic!("expected a TLS error, got {other:?}"),
        }
        assert!(
            err.to_string()
                .starts_with("storage.postgres.ssl_mode = verify-full could not be satisfied"),
            "{err}"
        );
    }

    #[test]
    fn an_unreachable_postgres_reports_where_it_tried_to_connect() {
        let err = open_storage(&postgres_config(
            hs_config::storage::PostgresSslMode::Prefer,
            None,
        ))
        .unwrap_err();
        match err {
            StorageOpenError::Postgres {
                host,
                port,
                database,
                source,
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 1);
                assert_eq!(database, "hs");
                // It got as far as dialing (a refused connection), not a DSN parse error.
                let text = source.to_string();
                assert!(text.contains("connect"), "{text}");
            }
            other => panic!("expected a Postgres open failure, got {other:?}"),
        }
    }

    #[test]
    fn the_connection_string_quotes_every_value_and_omits_an_empty_password() {
        assert_eq!(dsn_value("plain"), "'plain'");
        assert_eq!(
            dsn_value("it's a \\ pass word"),
            "'it\\'s a \\\\ pass word'"
        );
        let parsed: postgres::Config = format!(
            "host={} port=5432 dbname={} user={} password={}",
            dsn_value("db.internal"),
            dsn_value("hs"),
            dsn_value("hs"),
            dsn_value("it's a \\ pass word")
        )
        .parse()
        .expect("the quoted form parses");
        assert_eq!(parsed.get_password(), Some(&b"it's a \\ pass word"[..]));
        assert_eq!(parsed.get_dbname(), Some("hs"));
        // The empty-password case is what `an_unreachable_postgres_reports_where_it_tried_to_
        // connect` exercises: without the omission it failed to parse before ever connecting.
    }

    #[test]
    fn slatedb_is_still_reported_as_not_implemented() {
        let config = hs_config::StorageConfig::Slatedb(hs_config::storage::SlatedbStorageConfig {
            bucket_url: "s3://bucket/prefix".into(),
            shard_count: 16,
            lease_duration: "30s".parse().expect("30s is a valid duration"),
        });
        let err = open_storage(&config).unwrap_err();
        assert!(matches!(
            err,
            StorageOpenError::BackendNotImplemented { backend: "slatedb" }
        ));
    }
}
