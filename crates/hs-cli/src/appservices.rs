//! Opens `hs-appservice`'s [`Registry`], imports `config.appservices.registration_files`
//! (`hs_config::AppservicesConfig`, Synapse's `app_service_config_files`) into it once, and wires
//! the registry into `hs-auth`'s `AuthState::appservices` via
//! `hs_appservice::auth_registry::RegistryAppserviceAdapter` -- already built by track 11 for
//! exactly this purpose (that module's doc: "replacing the stub `InMemoryAppserviceRegistry`").
//!
//! Also builds the [`PingService`](hs_appservice::ping::PingService) `crate::serve` mounts as
//! `POST /_matrix/client/v1/appservice/{appserviceId}/ping` (`hs_appservice::routes::ping_router`).
//!
//! # Registration files are imported once, not loaded (decision 0010)
//!
//! A bridge is registered, changed and removed through the admin API and the web interface's
//! Bridges section. The registry is durable, so it is the truth; a registration file on disk is
//! a way *into* it, for a deployment migrating from Synapse's `app_service_config_files`, and
//! nothing more:
//!
//! - The first start that sees a listed file reads it and adds its registration to the registry
//!   -- unless an appservice with that id is already registered, which is left exactly as it is.
//!   Either way the file is recorded as imported ([`hs_config::store::ImportRecord`], in the
//!   configuration store), logged, and written to the audit log as `appservices.import` by the
//!   caller ([`audit_imports`]).
//! - Every later start skips a file that has been recorded, without reading it. A bridge edited
//!   or removed in the interface stays edited or removed across restarts, and a file deleted
//!   after its import no longer stops the server starting.
//!
//! The record is keyed by the path as written. Listing a new path imports it; changing a file
//! already imported does nothing -- change the bridge in the interface instead.

use std::path::Path;
use std::sync::Arc;

use hs_appservice::ping::{HttpPingTransport, PingService};
use hs_appservice::registration::{Registration, RegistrationError};
use hs_appservice::registry::Registry;
use hs_config::ConfigStore;
use hs_config::store::{ImportRecord, StoreError};
use hs_kv::KvBackend;

/// The [`ImportRecord::kind`] for an appservice registration file.
pub const IMPORT_KIND: &str = "appservice_registration";

/// Errors importing the registration files a configuration lists.
#[derive(Debug, thiserror::Error)]
pub enum LoadAppservicesError {
    /// Opening the registry's own keyspaces failed.
    #[error("failed to open the appservice registry: {0}")]
    OpenRegistry(#[source] hs_appservice::error::AppserviceError),
    /// Reading or writing the import records in the configuration store failed.
    #[error("failed to read or record appservice registration imports: {0}")]
    ImportRecords(#[source] StoreError),
    /// A registration file could not be read.
    #[error("failed to read appservice registration file {path:?}: {source}")]
    Read {
        /// The file that failed to read.
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A registration file's YAML did not parse as a valid registration.
    #[error("invalid appservice registration file {path:?}: {source}")]
    Parse {
        /// The file that failed to parse.
        path: std::path::PathBuf,
        #[source]
        source: RegistrationError,
    },
    /// A syntactically valid registration was rejected by the registry itself (a token collision
    /// with another appservice, or a namespace conflict).
    #[error("could not register appservice from {path:?}: {source}")]
    Add {
        /// The file whose registration was rejected.
        path: std::path::PathBuf,
        #[source]
        source: hs_appservice::error::AppserviceError,
    },
}

/// What one start did with one listed registration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Read and added to the registry by this start.
    Imported,
    /// Read, but the registry already had an appservice with its id, which was left alone.
    AlreadyRegistered,
    /// Recorded as imported by an earlier start (or, racing this one, another replica); not read.
    ImportedEarlier,
}

/// One listed registration file and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedRegistration {
    /// The file, as the configuration lists it.
    pub path: std::path::PathBuf,
    /// The appservice id it names, when this start read it.
    pub id: Option<String>,
    /// What happened.
    pub outcome: ImportOutcome,
}

/// What `hs serve` needs to mount the appservice surface: the registry (for
/// `RegistryAppserviceAdapter` and the ping service) and the ping service itself.
pub struct LoadedAppservices<B: KvBackend> {
    /// The opened, populated registry.
    pub registry: Arc<Registry<B>>,
    /// The ping service, ready to mount via `hs_appservice::routes::ping_router`.
    pub ping_service: Arc<PingService<B>>,
    /// Every listed registration file and what this start did with it, for [`audit_imports`].
    pub imports: Vec<ImportedRegistration>,
}

/// Opens an appservice [`Registry`] over `backend` and imports each file in
/// `config.registration_files` that has not been imported before (see the module doc).
///
/// # Errors
/// See [`LoadAppservicesError`]. The first problem stops the start: a registration file listed
/// for import that cannot be imported is a migration that did not happen, and a server that
/// started anyway would be missing a bridge nobody was told about.
pub fn load<B: KvBackend>(
    config: &hs_config::AppservicesConfig,
    backend: B,
    server_name: &ruma::ServerName,
) -> Result<LoadedAppservices<B>, LoadAppservicesError> {
    let records =
        ConfigStore::open(backend.clone()).map_err(LoadAppservicesError::ImportRecords)?;
    let registry =
        Registry::open(backend, server_name).map_err(LoadAppservicesError::OpenRegistry)?;
    let mut imports = Vec::with_capacity(config.registration_files.len());
    for path in &config.registration_files {
        imports.push(import_once(&registry, &records, path)?);
    }
    let registry = Arc::new(registry);
    let ping_service = Arc::new(PingService::new(
        registry.clone(),
        Arc::new(HttpPingTransport::new()),
    ));
    Ok(LoadedAppservices {
        registry,
        ping_service,
        imports,
    })
}

fn import_once<B: KvBackend>(
    registry: &Registry<B>,
    records: &ConfigStore<B>,
    path: &Path,
) -> Result<ImportedRegistration, LoadAppservicesError> {
    let key = path.display().to_string();
    if let Some(earlier) = records
        .import_record(IMPORT_KIND, &key)
        .map_err(LoadAppservicesError::ImportRecords)?
    {
        tracing::info!(
            path = %path.display(),
            outcome = %earlier.outcome,
            "appservice registration file was imported earlier and is not read again; manage the \
             bridge in the Bridges section of the admin interface"
        );
        return Ok(ImportedRegistration {
            path: path.to_owned(),
            id: None,
            outcome: ImportOutcome::ImportedEarlier,
        });
    }

    let yaml = std::fs::read_to_string(path).map_err(|source| LoadAppservicesError::Read {
        path: path.to_owned(),
        source,
    })?;
    let registration =
        Registration::parse_yaml(&yaml).map_err(|source| LoadAppservicesError::Parse {
            path: path.to_owned(),
            source,
        })?;
    let existing = registry
        .get(&registration.id)
        .map_err(|source| LoadAppservicesError::Add {
            path: path.to_owned(),
            source,
        })?;
    let (outcome, summary) = if existing.is_some() {
        (
            ImportOutcome::AlreadyRegistered,
            format!(
                "not imported: an appservice with id {} was already registered, and was left as it was",
                registration.id
            ),
        )
    } else {
        // `import`, not `add`: two replicas starting at once may both get here for the same
        // file, and the second must find the first's row an update, not a conflict.
        registry
            .import(&registration)
            .map_err(|source| LoadAppservicesError::Add {
                path: path.to_owned(),
                source,
            })?;
        (
            ImportOutcome::Imported,
            format!("imported as {}", registration.id),
        )
    };
    let recorded = records
        .record_import(&ImportRecord {
            kind: IMPORT_KIND.to_owned(),
            key,
            outcome: summary.clone(),
            at_ms: now_ms(),
        })
        .map_err(LoadAppservicesError::ImportRecords)?;
    tracing::info!(
        id = %registration.id,
        path = %path.display(),
        outcome = %summary,
        "appservice registration file imported once; from now on the bridge is managed in the \
         Bridges section of the admin interface, and this file is not read again"
    );
    Ok(ImportedRegistration {
        path: path.to_owned(),
        id: Some(registration.id),
        // Another replica recorded the same file first: it owns the import (and its audit
        // entry); what this one did was an idempotent upsert of the same registration.
        outcome: if recorded {
            outcome
        } else {
            ImportOutcome::ImportedEarlier
        },
    })
}

/// Writes one `appservices.import` audit entry per registration file this start imported (or
/// found already registered), with the system as the actor and the file as the change. The
/// audit log is where an operator looks for how a bridge came to exist; one that arrived from a
/// file should be there too.
///
/// # Errors
/// The audit sink's own error: an import that could not be audited is reported, not hidden.
pub async fn audit_imports(
    audit: &dyn hs_admin::audit::AuditSink,
    imports: &[ImportedRegistration],
) -> Result<(), hs_admin::audit::AuditError> {
    use hs_admin::model::{Actor, AuditChange, AuditEntry, AuditOutcome, ResourceRef};

    for import in imports {
        let Some(id) = &import.id else { continue };
        let status = match import.outcome {
            ImportOutcome::Imported => 201,
            // Nothing changed in the registry; recorded so the log says why the file was not used.
            ImportOutcome::AlreadyRegistered => 200,
            ImportOutcome::ImportedEarlier => continue,
        };
        let mut actor = Actor::system();
        actor.display_name = Some("registration file import".to_owned());
        let mut entry = AuditEntry::new(
            "appservices.import",
            actor,
            ResourceRef::new("appservice", id.clone()),
            AuditOutcome::success(status),
        );
        entry.changes = vec![AuditChange {
            pointer: "/registration_file".to_owned(),
            from: None,
            to: Some(serde_json::Value::String(import.path.display().to_string())),
        }];
        audit.append(entry).await?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn registration(url: &str) -> String {
        format!(
            "id: irc\nurl: '{url}'\nas_token: as_secret\nhs_token: hs_secret\n\
             sender_localpart: ircbot\nnamespaces:\n  users:\n    - regex: '@irc_.*'\n      \
             exclusive: true\n"
        )
    }

    fn config_listing(path: &Path) -> hs_config::AppservicesConfig {
        hs_config::AppservicesConfig {
            registration_files: vec![path.to_owned()],
            ..hs_config::AppservicesConfig::default()
        }
    }

    #[test]
    fn loads_with_no_registration_files_configured() {
        let config = hs_config::AppservicesConfig::default();
        let loaded = load(
            &config,
            MemoryBackend::new(),
            ruma::server_name!("example.org"),
        )
        .unwrap();
        assert!(loaded.registry.list().unwrap().is_empty());
        assert!(loaded.imports.is_empty());
    }

    #[test]
    fn a_real_registration_file_is_imported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("irc.yaml");
        std::fs::write(&path, registration("http://localhost:1234")).unwrap();
        let loaded = load(
            &config_listing(&path),
            MemoryBackend::new(),
            ruma::server_name!("example.org"),
        )
        .unwrap();
        let rows = loaded.registry.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "irc");
        assert_eq!(
            loaded.imports,
            vec![ImportedRegistration {
                path,
                id: Some("irc".to_owned()),
                outcome: ImportOutcome::Imported,
            }]
        );
    }

    /// Decision 0010: the file is a way into the registry, read once. After that the registry,
    /// edited through the admin API, is the truth: an edit made there survives a restart with the
    /// file still listed, an edit to the file does not take, and removing the bridge removes it.
    #[test]
    fn a_registration_file_is_imported_once_and_then_managed_through_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("irc.yaml");
        std::fs::write(&path, registration("http://bridge.local:1")).unwrap();
        let config = config_listing(&path);
        // One backend across "boots": `MemoryBackend` clones share their store.
        let backend = MemoryBackend::new();
        let server_name = ruma::server_name!("example.org");

        let first = load(&config, backend.clone(), server_name).unwrap();
        assert_eq!(first.imports[0].outcome, ImportOutcome::Imported);
        // What `PATCH /api/v1/appservices/irc` does.
        first
            .registry
            .update("irc", &serde_json::json!({"url": "http://bridge.local:9"}))
            .unwrap();
        drop(first);

        std::fs::write(&path, registration("http://bridge.local:2")).unwrap();
        let second = load(&config, backend.clone(), server_name).expect("the second boot");
        assert_eq!(second.imports[0].outcome, ImportOutcome::ImportedEarlier);
        assert_eq!(
            second.registry.get("irc").unwrap().unwrap().url.as_deref(),
            Some("http://bridge.local:9"),
            "the change made through the API stands; the edited file is not read again"
        );
        // What `DELETE /api/v1/appservices/irc` does.
        second.registry.remove("irc").unwrap();
        drop(second);

        // And a file deleted after its import no longer stops the server starting.
        std::fs::remove_file(&path).unwrap();
        let third = load(&config, backend.clone(), server_name).expect("the third boot");
        assert!(
            third.registry.get("irc").unwrap().is_none(),
            "a bridge removed through the API is not resurrected from the file"
        );
        let records = ConfigStore::open(backend).unwrap();
        let recorded = records.imports(IMPORT_KIND).unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].outcome, "imported as irc");
    }

    /// A file naming an appservice the registry already has (registered through the API before
    /// the file was listed) leaves the registered one alone.
    #[test]
    fn a_file_for_an_appservice_already_registered_does_not_overwrite_it() {
        let dir = tempfile::tempdir().unwrap();
        let backend = MemoryBackend::new();
        let server_name = ruma::server_name!("example.org");
        let registry = Registry::open(backend.clone(), server_name).unwrap();
        registry
            .add(&Registration::parse_yaml(&registration("http://registered.local")).unwrap())
            .unwrap();
        drop(registry);

        let path = dir.path().join("irc.yaml");
        std::fs::write(&path, registration("http://from-the-file.local")).unwrap();
        let loaded = load(&config_listing(&path), backend, server_name).unwrap();
        assert_eq!(loaded.imports[0].outcome, ImportOutcome::AlreadyRegistered);
        assert_eq!(
            loaded.registry.get("irc").unwrap().unwrap().url.as_deref(),
            Some("http://registered.local")
        );
    }

    #[test]
    fn a_missing_registration_file_is_an_error_on_its_first_import() {
        let config = config_listing(Path::new("/nonexistent/does-not-exist.yaml"));
        let err = match load(
            &config,
            MemoryBackend::new(),
            ruma::server_name!("example.org"),
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected a missing-file error"),
        };
        assert!(matches!(err, LoadAppservicesError::Read { .. }));
    }

    #[tokio::test]
    async fn an_import_is_audited_once_as_the_system() {
        use hs_admin::audit::AuditSink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("irc.yaml");
        std::fs::write(&path, registration("http://localhost:1234")).unwrap();
        let backend = MemoryBackend::new();
        let server_name = ruma::server_name!("example.org");
        let audit = hs_admin::audit::InMemoryAuditSink::new();

        let first = load(&config_listing(&path), backend.clone(), server_name).unwrap();
        audit_imports(&audit, &first.imports).await.unwrap();
        let second = load(&config_listing(&path), backend, server_name).unwrap();
        audit_imports(&audit, &second.imports).await.unwrap();

        let entries = audit
            .query(&hs_admin::audit::AuditFilter::default())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1, "the second start imported nothing");
        let entry = &entries[0];
        assert_eq!(entry.action, "appservices.import");
        assert_eq!(entry.actor.kind, hs_admin::model::ActorKind::System);
        assert_eq!(entry.target.id, "irc");
        assert_eq!(
            entry.changes[0].to,
            Some(serde_json::Value::String(path.display().to_string()))
        );
    }
}
