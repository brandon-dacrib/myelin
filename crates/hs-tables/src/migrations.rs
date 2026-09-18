//! A migration runner with a schema version table, so schema changes are applied once, in order,
//! and are safe to run again (already-applied migrations are skipped, not re-run).
//!
//! Each [`Migration`] runs inside its own retried, serializable transaction
//! ([`hs_kv::transact`]); the schema version is bumped to that migration's version inside the same
//! transaction, so a crash between "the migration's writes committed" and "the version was
//! recorded" cannot happen — either both happened, or neither did.

use hs_kv::{KvBackend, KvError, KvRead, KvWrite, TransactConfig, transact};

const META_KEYSPACE: &str = "_hs_tables_schema_meta";
const VERSION_KEY: &[u8] = b"version";

type MigrationFn<B> =
    Box<dyn Fn(&B, &mut <B as KvBackend>::Txn) -> Result<(), KvError> + Send + Sync>;

/// One schema change. Versions start at 1 (version 0 means "nothing has been applied yet") and
/// must be applied in strictly increasing order; [`run_migrations`] enforces this against the
/// order `migrations` is given in, not just against what has already run.
pub struct Migration<B: KvBackend> {
    version: u32,
    description: &'static str,
    run: MigrationFn<B>,
}

impl<B: KvBackend> Migration<B> {
    /// Declares a migration. `run` receives the backend (to open new keyspaces the migration
    /// introduces) and the write transaction it must do all of its work inside.
    ///
    /// # Panics
    /// Panics if `version` is 0.
    pub fn new(
        version: u32,
        description: &'static str,
        run: impl Fn(&B, &mut B::Txn) -> Result<(), KvError> + Send + Sync + 'static,
    ) -> Self {
        assert!(
            version > 0,
            "migration versions start at 1; 0 means \"nothing applied yet\""
        );
        Self {
            version,
            description,
            run: Box::new(run),
        }
    }
}

/// An error from [`run_migrations`].
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The `hs-kv` backend reported an error, from either the runner's own bookkeeping or a
    /// migration's `run` closure.
    #[error(transparent)]
    Kv(#[from] KvError),
    /// `migrations` was not given in strictly increasing version order.
    #[error(
        "migrations must be given in strictly increasing version order; after version {current}, \
         got version {got} ({description:?})"
    )]
    OutOfOrder {
        /// The highest version seen so far in the input order.
        current: u32,
        /// The out-of-order version that violated it.
        got: u32,
        /// That migration's description.
        description: &'static str,
    },
}

/// Reads the schema version currently recorded for `backend`, or `0` if no migration has ever
/// been applied.
///
/// # Errors
/// Returns [`KvError`] on backend failure.
pub fn current_version<B: KvBackend>(backend: &B) -> Result<u32, KvError> {
    let meta = backend.keyspace(META_KEYSPACE)?;
    let snapshot = backend.snapshot();
    let raw = snapshot.get(&meta, VERSION_KEY)?;
    Ok(raw
        .and_then(|bytes| <[u8; 4]>::try_from(bytes.as_ref()).ok())
        .map(u32::from_be_bytes)
        .unwrap_or(0))
}

/// Brings `backend`'s schema up to date by applying every migration in `migrations` whose version
/// is greater than the currently recorded one, in order. Returns the resulting schema version
/// (which is `migrations.last().version` if any ran, or the pre-existing version if none did).
///
/// Safe to call on every process start: migrations already applied are skipped without running
/// their `run` closure again.
///
/// # Errors
/// Returns [`MigrationError::OutOfOrder`] if `migrations` is not given in strictly increasing
/// version order. Returns [`MigrationError::Kv`] if a migration's closure, or the runner's own
/// bookkeeping, fails.
pub fn run_migrations<B: KvBackend>(
    backend: &B,
    migrations: &[Migration<B>],
) -> Result<u32, MigrationError> {
    let meta = backend.keyspace(META_KEYSPACE)?;
    let mut applied = current_version(backend)?;

    let mut last_seen = 0u32;
    for migration in migrations {
        if migration.version <= last_seen {
            return Err(MigrationError::OutOfOrder {
                current: last_seen,
                got: migration.version,
                description: migration.description,
            });
        }
        last_seen = migration.version;

        if migration.version <= applied {
            continue;
        }

        transact(backend, TransactConfig::default(), |txn| {
            (migration.run)(backend, txn)?;
            txn.put(&meta, VERSION_KEY, &migration.version.to_be_bytes())?;
            Ok(())
        })?;

        applied = migration.version;
    }

    Ok(applied)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hs_kv::memory::MemoryBackend;

    use super::*;

    #[test]
    fn migrations_apply_in_order_and_record_the_version() {
        let backend = MemoryBackend::new();
        assert_eq!(current_version(&backend).unwrap(), 0);

        let migrations: Vec<Migration<MemoryBackend>> = vec![
            Migration::<MemoryBackend>::new(1, "create rooms keyspace", |backend, txn| {
                let ks = backend.keyspace("rooms")?;
                txn.put(&ks, b"seed", b"v1")?;
                Ok(())
            }),
            Migration::<MemoryBackend>::new(2, "add a second seed row", |backend, txn| {
                let ks = backend.keyspace("rooms")?;
                txn.put(&ks, b"seed2", b"v2")?;
                Ok(())
            }),
        ];

        let version = run_migrations(&backend, &migrations).unwrap();
        assert_eq!(version, 2);
        assert_eq!(current_version(&backend).unwrap(), 2);

        let ks = backend.keyspace("rooms").unwrap();
        let snap = backend.snapshot();
        assert_eq!(
            snap.get(&ks, b"seed").unwrap().unwrap(),
            bytes::Bytes::from_static(b"v1")
        );
        assert_eq!(
            snap.get(&ks, b"seed2").unwrap().unwrap(),
            bytes::Bytes::from_static(b"v2")
        );
    }

    #[test]
    fn already_applied_migrations_do_not_run_again() {
        let backend = MemoryBackend::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let make_migrations = |calls: Arc<AtomicUsize>| -> Vec<Migration<MemoryBackend>> {
            vec![Migration::<MemoryBackend>::new(
                1,
                "counted",
                move |_backend, _txn| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )]
        };

        run_migrations(&backend, &make_migrations(calls.clone())).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A second run (as would happen on every process start) must not re-run it.
        run_migrations(&backend, &make_migrations(calls.clone())).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn out_of_order_migrations_are_rejected() {
        let backend = MemoryBackend::new();
        let migrations: Vec<Migration<MemoryBackend>> = vec![
            Migration::<MemoryBackend>::new(2, "second", |_b, _t| Ok(())),
            Migration::<MemoryBackend>::new(1, "first", |_b, _t| Ok(())),
        ];
        let err = run_migrations(&backend, &migrations).unwrap_err();
        assert!(matches!(
            err,
            MigrationError::OutOfOrder {
                current: 2,
                got: 1,
                ..
            }
        ));
    }

    #[test]
    #[should_panic(expected = "version")]
    fn version_zero_panics() {
        let _: Migration<MemoryBackend> = Migration::new(0, "invalid", |_b, _t| Ok(()));
    }
}
