//! Opens the `hs-kv` storage backend a native config selects.
//!
//! Seam gap (see `docs/status/12-platform-and-kubernetes.md`): `hs_config::StorageConfig` has
//! three variants (`Embedded`, `Postgres`, `Slatedb`), but as of this writing `hs-kv`
//! (owned by track 01) only ships [`hs_kv::memory::MemoryBackend`] and
//! [`hs_kv::fjall_backend::FjallBackend`] — there is no PostgreSQL or SlateDB `KvBackend` yet.
//! `hs serve` can therefore only actually open storage for `StorageConfig::Embedded`; the other
//! two variants are accepted by config validation (they are valid *configuration*) but fail at
//! this step with a clear, actionable error rather than silently falling back to something else.
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
        "storage.backend = {backend:?} has no hs-kv implementation yet (only the embedded Fjall \
         backend is wired up in `hs serve` today); see docs/status/12-platform-and-kubernetes.md"
    )]
    BackendNotImplemented {
        /// The backend name, for the error message (`"postgres"` or `"slatedb"`).
        backend: &'static str,
    },
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
}

impl std::fmt::Debug for OpenedStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenedStorage::Embedded(_) => f.write_str("OpenedStorage::Embedded(..)"),
        }
    }
}

/// Opens the storage backend `config.storage` selects.
///
/// # Errors
/// Returns [`StorageOpenError::BackendNotImplemented`] for `postgres` or `slatedb` (no `hs-kv`
/// backend exists for either yet), or [`StorageOpenError::Fjall`] if the embedded backend's data
/// directory could not be opened.
pub fn open_storage(config: &hs_config::StorageConfig) -> Result<OpenedStorage, StorageOpenError> {
    match config {
        hs_config::StorageConfig::Embedded(embedded) => {
            open_embedded(&embedded.data_dir).map(OpenedStorage::Embedded)
        }
        hs_config::StorageConfig::Postgres(_) => Err(StorageOpenError::BackendNotImplemented {
            backend: "postgres",
        }),
        hs_config::StorageConfig::Slatedb(_) => {
            Err(StorageOpenError::BackendNotImplemented { backend: "slatedb" })
        }
    }
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
        let OpenedStorage::Embedded(backend) = opened;
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

    #[test]
    fn postgres_backend_is_reported_as_not_implemented() {
        let config =
            hs_config::StorageConfig::Postgres(hs_config::storage::PostgresStorageConfig {
                host: "db".into(),
                port: 5432,
                database: "hs".into(),
                user: "hs".into(),
                password: hs_config::SecretString::default(),
                password_file: None,
                pool_size: 10,
                tls: false,
            });
        let err = open_storage(&config).unwrap_err();
        assert!(matches!(
            err,
            StorageOpenError::BackendNotImplemented {
                backend: "postgres"
            }
        ));
    }
}
