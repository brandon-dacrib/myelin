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
    /// `storage.postgres.tls` was set, which this backend cannot honour.
    #[error(
        "storage.postgres.tls is set, but the hs-kv postgres backend connects without TLS \
         (NoTls) and would silently send credentials in the clear. Terminate TLS in front of \
         PostgreSQL, or unset this field to acknowledge a plaintext connection."
    )]
    PostgresTlsUnsupported,
    /// The embedded Fjall database could not be opened.
    #[error("failed to open the embedded storage backend at {path:?}: {source}")]
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
/// for it), [`StorageOpenError::Fjall`] if the embedded backend's data directory could not be
/// opened, or [`StorageOpenError::Postgres`]/[`StorageOpenError::PostgresTlsUnsupported`] for the
/// PostgreSQL backend.
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

/// Opens the PostgreSQL backend from its configured connection fields.
///
/// `tls` is refused rather than ignored: the backend connects with `NoTls`, so honouring the flag
/// silently would send credentials in the clear on a connection the operator asked to encrypt.
/// `schema` is fixed to `"public"` — `PostgresBackend::open` takes it as a parameter, but
/// `hs_config::PostgresStorageConfig` has no field for it yet. `pool_size` is likewise not
/// plumbed through (the backend fixes its own pool size); both are noted in
/// `docs/status/01-storage-engine.md`.
fn open_postgres(
    config: &hs_config::storage::PostgresStorageConfig,
) -> Result<hs_kv::postgres_backend::PostgresBackend, StorageOpenError> {
    if config.tls {
        return Err(StorageOpenError::PostgresTlsUnsupported);
    }
    let password = config.password.as_str().unwrap_or_default();
    let dsn = format!(
        "host={} port={} dbname={} user={} password={}",
        config.host, config.port, config.database, config.user, password
    );
    hs_kv::postgres_backend::PostgresBackend::open(&dsn, "public").map_err(|source| {
        StorageOpenError::Postgres {
            host: config.host.clone(),
            port: config.port,
            database: config.database.clone(),
            source,
        }
    })
}

fn open_embedded(path: &Path) -> Result<hs_kv::fjall_backend::FjallBackend, StorageOpenError> {
    std::fs::create_dir_all(path).map_err(|e| StorageOpenError::Fjall {
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

    fn postgres_config(tls: bool) -> hs_config::StorageConfig {
        hs_config::StorageConfig::Postgres(hs_config::storage::PostgresStorageConfig {
            // Deliberately unroutable: this test is about which error comes back, not about
            // reaching a database. A real-database test lives in `hs-kv`'s own conformance suite.
            host: "127.0.0.1".into(),
            port: 1,
            database: "hs".into(),
            user: "hs".into(),
            password: hs_config::SecretString::default(),
            password_file: None,
            pool_size: 10,
            tls,
        })
    }

    #[test]
    fn postgres_tls_is_refused_rather_than_silently_ignored() {
        // The backend connects with `NoTls`. Honouring `tls: true` by ignoring it would send the
        // password in the clear on a connection the operator explicitly asked to encrypt.
        let err = open_storage(&postgres_config(true)).unwrap_err();
        assert!(matches!(err, StorageOpenError::PostgresTlsUnsupported));
    }

    #[test]
    fn an_unreachable_postgres_reports_where_it_tried_to_connect() {
        let err = open_storage(&postgres_config(false)).unwrap_err();
        match err {
            StorageOpenError::Postgres {
                host,
                port,
                database,
                ..
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 1);
                assert_eq!(database, "hs");
            }
            other => panic!("expected a Postgres open failure, got {other:?}"),
        }
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
