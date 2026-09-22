//! Loads `config.appservices.registration_files` (`hs_config::AppservicesConfig`, Synapse's
//! `app_service_config_files`) into `hs-appservice`'s [`Registry`], and wires that registry into
//! `hs-auth`'s `AuthState::appservices` via `hs_appservice::auth_registry::RegistryAppserviceAdapter`
//! — already built by track 11 for exactly this purpose (that module's doc: "replacing the stub
//! `InMemoryAppserviceRegistry`"). `hs-cli` only needs to call it; no bridging code of its own.
//!
//! Also builds the [`PingService`](hs_appservice::ping::PingService) `crate::serve` mounts as
//! `POST /_matrix/client/v1/appservice/{appserviceId}/ping` (`hs_appservice::routes::ping_router`).

use std::sync::Arc;

use hs_appservice::ping::{HttpPingTransport, PingService};
use hs_appservice::registration::{Registration, RegistrationError};
use hs_appservice::registry::Registry;
use hs_kv::KvBackend;

/// Errors loading the static appservice registration files a native config lists.
#[derive(Debug, thiserror::Error)]
pub enum LoadAppservicesError {
    /// Opening the registry's own keyspaces failed.
    #[error("failed to open the appservice registry: {0}")]
    OpenRegistry(#[source] hs_appservice::error::AppserviceError),
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
    /// A syntactically valid registration was rejected by the registry itself (a duplicate id,
    /// a token collision with another registration file, or a namespace conflict).
    #[error("could not register appservice from {path:?}: {source}")]
    Add {
        /// The file whose registration was rejected.
        path: std::path::PathBuf,
        #[source]
        source: hs_appservice::error::AppserviceError,
    },
}

/// What `hs serve` needs to mount the appservice surface: the registry (for
/// `RegistryAppserviceAdapter` and the ping service) and the ping service itself.
pub struct LoadedAppservices<B: KvBackend> {
    /// The opened, populated registry.
    pub registry: Arc<Registry<B>>,
    /// The ping service, ready to mount via `hs_appservice::routes::ping_router`.
    pub ping_service: Arc<PingService<B>>,
}

/// Opens an appservice [`Registry`] over `backend` and loads every file in
/// `config.registration_files` into it.
///
/// # Errors
/// See [`LoadAppservicesError`]. The first problem encountered stops loading (unlike
/// `hs_config`'s own validation, which collects every error) — a registration file the operator
/// listed but that fails to load is exactly the kind of misconfiguration that should stop the
/// server from starting with a partially-loaded appservice set, rather than silently serving with
/// some bridges missing.
pub fn load<B: KvBackend>(
    config: &hs_config::AppservicesConfig,
    backend: B,
    server_name: &ruma::ServerName,
) -> Result<LoadedAppservices<B>, LoadAppservicesError> {
    let registry =
        Registry::open(backend, server_name).map_err(LoadAppservicesError::OpenRegistry)?;
    for path in &config.registration_files {
        let yaml = std::fs::read_to_string(path).map_err(|source| LoadAppservicesError::Read {
            path: path.clone(),
            source,
        })?;
        let registration =
            Registration::parse_yaml(&yaml).map_err(|source| LoadAppservicesError::Parse {
                path: path.clone(),
                source,
            })?;
        // `import`, not `add`: the registry is durable, so on every boot after the first the
        // registration is already there. `add` refused it as a conflict with itself, and a
        // server with a bridge configured started exactly once. (Every test of this ran over a
        // memory backend, where there is no second boot.)
        registry
            .import(&registration)
            .map_err(|source| LoadAppservicesError::Add {
                path: path.clone(),
                source,
            })?;
        tracing::info!(id = %registration.id, path = %path.display(), "loaded appservice registration");
    }
    let registry = Arc::new(registry);
    let ping_service = Arc::new(PingService::new(
        registry.clone(),
        Arc::new(HttpPingTransport::new()),
    ));
    Ok(LoadedAppservices {
        registry,
        ping_service,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

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
    }

    /// The registry persists, and the file is read at every boot: the second boot must find
    /// the registration already there and be content, and an edit to the file must take.
    #[test]
    fn loads_the_same_registration_file_on_every_boot_and_follows_its_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("irc.yaml");
        let registration = |url: &str| {
            format!(
                "id: irc\nurl: '{url}'\nas_token: as_secret\nhs_token: hs_secret\n\
                 sender_localpart: ircbot\nnamespaces:\n  users:\n    - regex: '@irc_.*'\n      \
                 exclusive: true\n"
            )
        };
        std::fs::write(&path, registration("http://bridge.local:1")).unwrap();
        let config = hs_config::AppservicesConfig {
            registration_files: vec![path.clone()],
            ..hs_config::AppservicesConfig::default()
        };
        // One backend across "boots": `MemoryBackend` clones share their store.
        let backend = MemoryBackend::new();
        let server_name = ruma::server_name!("example.org");

        load(&config, backend.clone(), server_name).unwrap();
        let again = load(&config, backend.clone(), server_name).expect("the second boot");
        assert_eq!(again.registry.list().unwrap().len(), 1);

        std::fs::write(&path, registration("http://bridge.local:2")).unwrap();
        let edited = load(&config, backend, server_name).unwrap();
        assert_eq!(
            edited.registry.get("irc").unwrap().unwrap().url.as_deref(),
            Some("http://bridge.local:2")
        );
    }

    #[test]
    fn loads_a_real_registration_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("irc.yaml");
        std::fs::write(
            &path,
            "id: irc\nurl: 'http://localhost:1234'\nas_token: as_secret\nhs_token: hs_secret\n\
             sender_localpart: ircbot\nnamespaces:\n  users:\n    - regex: '@irc_.*'\n      \
             exclusive: true\n",
        )
        .unwrap();
        let config = hs_config::AppservicesConfig {
            registration_files: vec![path],
            ..hs_config::AppservicesConfig::default()
        };
        let loaded = load(
            &config,
            MemoryBackend::new(),
            ruma::server_name!("example.org"),
        )
        .unwrap();
        let rows = loaded.registry.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "irc");
    }

    #[test]
    fn a_missing_registration_file_is_an_error() {
        let config = hs_config::AppservicesConfig {
            registration_files: vec![std::path::PathBuf::from("/nonexistent/does-not-exist.yaml")],
            ..hs_config::AppservicesConfig::default()
        };
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
}
