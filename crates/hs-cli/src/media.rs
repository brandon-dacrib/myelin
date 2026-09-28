//! Builds `hs-media`'s [`MediaState`] from native config, and attaches content scanning
//! (`ScanEngine::new`, `MediaRepository::with_scanning`) when the configuration turns it on.
//!
//! # `media.scanning` is a setting like any other
//!
//! Content scanning is the `media.scanning` section of the configuration (`hs_config::scanning`,
//! re-exported by `hs-media`), so it is stored in the database and changed through the admin API
//! and the web interface (decision 0010). The default is mode `off`, and with it no engine is
//! attached -- exactly what every `MediaRepository` caller had before scanning existed.
//!
//! `--media-scanning-config <path>` predates that and still works: a standalone YAML file of the
//! same shape (`deploy/media-scanning/media-scanning.yaml`), which replaces the configured
//! section wholesale, with a warning. It is deprecated -- a file on one process's disk is not how
//! this server is administered, and the setting it overrides is invisible in the interface.

use std::sync::Arc;

use hs_kv::KvBackend;
use hs_media::repository::MediaRepository;
use hs_media::scanning::audit::TracingAuditSink;
use hs_media::scanning::metrics::ScanMetrics;
use hs_media::scanning::{ScanEngine, ScanningConfig};
use hs_media::state::MediaState;
use hs_media::thumbnail::ThumbnailPolicy;

/// Errors building the media repository and its state.
#[derive(Debug, thiserror::Error)]
pub enum MediaSetupError {
    /// Building the configured object-store backend failed.
    #[error(transparent)]
    Store(#[from] hs_media::MediaError),
    /// `--media-scanning-config` could not be read.
    #[error("failed to read --media-scanning-config {path:?}: {source}")]
    ReadScanningConfig {
        /// The path that failed.
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `--media-scanning-config`'s YAML did not parse, or the scanning configuration in force
    /// failed `ScanningConfig::validated` (an unset failure policy while scanning is enabled, an
    /// empty provider config, etc).
    #[error("invalid media.scanning configuration: {0}")]
    InvalidScanningConfig(hs_media::MediaError),
    /// Building the configured scan provider (or opening its verdict cache) failed.
    #[error("failed to build the content scanning engine: {0}")]
    ScanEngine(hs_media::MediaError),
}

/// Builds a [`MediaState`] from native config: the object-store backend
/// (`hs_media::store::build`, already written against `hs_config::MediaStorageBackend`), an
/// `hs-tables`-backed [`hs_media::metadata::MetadataStore`] over `backend`, an unlimited
/// [`hs_media::policy::InMemoryQuotaPolicy`] (`hs_config::MediaConfig` has no per-user/per-server
/// quota fields yet — see `docs/status/12-platform-and-kubernetes.md`), and — when
/// `config.media.scanning` is switched on, or the deprecated `media_scanning_config` file is
/// given — a real [`ScanEngine`] attached via [`MediaRepository::with_scanning`].
///
/// # Errors
/// See [`MediaSetupError`].
pub fn build_media_state<B: KvBackend>(
    config: &hs_config::Config,
    backend: B,
    auth: hs_auth::state::AuthState,
    media_scanning_config: Option<&std::path::Path>,
    metrics: &hs_telemetry::metrics::Metrics,
) -> Result<MediaState<B>, MediaSetupError> {
    let object_store = hs_media::store::build(&config.media.storage)?;
    let metadata = hs_media::metadata::MetadataStore::open(backend.clone())?;
    let policy = Arc::new(hs_media::policy::InMemoryQuotaPolicy::unlimited());
    let thumbnail_policy = ThumbnailPolicy {
        configured_sizes: config.media.thumbnail_sizes.clone(),
        ..ThumbnailPolicy::default()
    };
    let server_name = config.server.server_name.clone();

    let mut repository = MediaRepository::new(
        object_store,
        metadata,
        Arc::new(config.media.clone()),
        policy,
        thumbnail_policy,
        server_name,
        now_ms,
    );

    let scanning = match media_scanning_config {
        Some(path) => {
            tracing::warn!(
                path = %path.display(),
                "--media-scanning-config is deprecated and replaces the media.scanning setting \
                 wholesale; set media.scanning in the admin interface's Configuration page instead"
            );
            Some(read_scanning_file(path)?)
        }
        None if config.media.scanning.is_enabled() => Some(config.media.scanning.clone()),
        None => None,
    };
    if let Some(scanning) = scanning {
        let engine = build_scan_engine(scanning, backend, metrics)?;
        repository = repository.with_scanning(engine);
    }

    Ok(MediaState {
        auth,
        repository: Arc::new(repository),
        legacy_media_enabled: config.media.allow_legacy_unauthenticated_media,
        legacy_freeze_ms: None,
    })
}

fn read_scanning_file(path: &std::path::Path) -> Result<ScanningConfig, MediaSetupError> {
    let yaml =
        std::fs::read_to_string(path).map_err(|source| MediaSetupError::ReadScanningConfig {
            path: path.to_owned(),
            source,
        })?;
    ScanningConfig::from_yaml(&yaml).map_err(|e| {
        MediaSetupError::InvalidScanningConfig(hs_media::MediaError::InvalidInput(e.to_string()))
    })
}

fn build_scan_engine<B: KvBackend>(
    config: ScanningConfig,
    backend: B,
    metrics: &hs_telemetry::metrics::Metrics,
) -> Result<ScanEngine<B>, MediaSetupError> {
    let config = hs_media::scanning::config::validated(config)
        .map_err(MediaSetupError::InvalidScanningConfig)?;
    let scan_metrics = Arc::new(ScanMetrics::register(metrics));
    // `TracingAuditSink`: scan decisions land in the structured log stream until an operator
    // needs the admin API's audit surface (RFC 0011, still design-only per
    // `docs/status/09-media.md` — `ScanAdmin` has no implementation to hand a richer sink to
    // yet).
    ScanEngine::new(config, backend, scan_metrics, Arc::new(TracingAuditSink))
        .map_err(MediaSetupError::ScanEngine)
}

/// How `hs-media` fetches another server's media: over the federation mount's own client, so a
/// media fetch is discovered, signed, IP-checked and backed off exactly like every other
/// federation request, and a destination that is down is known to be down by both.
pub struct FederationMediaTransport {
    client: Arc<hs_federation::client::FederationClient>,
}

impl FederationMediaTransport {
    /// Wraps the federation client.
    #[must_use]
    pub fn new(client: Arc<hs_federation::client::FederationClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl hs_media::remote::RemoteMediaTransport for FederationMediaTransport {
    async fn get(
        &self,
        origin: &str,
        path: &str,
        signed: bool,
        max_bytes: usize,
    ) -> Result<hs_media::remote::RemoteResponse, hs_media::remote::TransportError> {
        use hs_federation::client::ClientError;
        match self.client.get_media(origin, path, signed, max_bytes).await {
            Ok(response) => Ok(hs_media::remote::RemoteResponse {
                status: response.status,
                content_type: response.content_type,
                content_disposition: response.content_disposition,
                location: response.location,
                body: response.body,
            }),
            Err(ClientError::ResponseTooLarge(_)) => {
                Err(hs_media::remote::TransportError::TooLarge)
            }
            Err(other) => Err(hs_media::remote::TransportError::Failed(other.to_string())),
        }
    }
}

/// Lets `repository` fetch other servers' media through `client`, counting into `metrics`.
pub fn install_remote_media<B: KvBackend>(
    repository: &MediaRepository<B>,
    client: Arc<hs_federation::client::FederationClient>,
    metrics: &hs_telemetry::metrics::Metrics,
) {
    let installed = repository.install_remote_media(
        Arc::new(FederationMediaTransport::new(client)),
        hs_media::remote::RemoteMediaMetrics::register(metrics),
    );
    if !installed {
        tracing::warn!("remote media fetching was already installed; keeping the first");
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;

    fn test_config() -> hs_config::Config {
        let mut config = hs_config::Config::default();
        config.server.server_name = "example.org".to_owned();
        config
    }

    #[test]
    fn builds_a_media_state_with_no_scanning_configured() {
        let state = build_media_state(
            &test_config(),
            MemoryBackend::new(),
            hs_auth::state::AuthState::in_memory(),
            None,
            &hs_telemetry::metrics::Metrics::new(),
        )
        .unwrap();
        assert!(state.legacy_media_enabled);
    }

    /// Decision 0010: scanning is switched on through the configuration (the database, the admin
    /// API), not a file named on the command line.
    #[test]
    fn attaches_a_scan_engine_when_the_configuration_turns_scanning_on() {
        let mut config = test_config();
        config.media.scanning = ScanningConfig::from_yaml(
            "mode: block\nprovider: icap\nfail: closed\nicap:\n  host: c-icap\n  service: virus_scan\n",
        )
        .unwrap();
        build_media_state(
            &config,
            MemoryBackend::new(),
            hs_auth::state::AuthState::in_memory(),
            None,
            &hs_telemetry::metrics::Metrics::new(),
        )
        .unwrap();
    }

    #[test]
    fn attaches_a_real_scan_engine_from_a_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scanning.yaml");
        std::fs::write(&path, "mode: off\n").unwrap();
        let state = build_media_state(
            &test_config(),
            MemoryBackend::new(),
            hs_auth::state::AuthState::in_memory(),
            Some(&path),
            &hs_telemetry::metrics::Metrics::new(),
        )
        .unwrap();
        // `mode: off` behaves identically to no scanning attached; this asserts the file was at
        // least read and accepted, not that scanning changes anything observable from here.
        assert!(state.legacy_media_enabled);
    }

    #[test]
    fn rejects_an_invalid_scanning_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scanning.yaml");
        // `block` mode with no `fail` policy is `ScanningConfig::validated`'s own documented
        // configuration error.
        std::fs::write(
            &path,
            "mode: block\nprovider: icap\nicap:\n  host: c-icap\n  service: virus_scan\n",
        )
        .unwrap();
        let err = match build_media_state(
            &test_config(),
            MemoryBackend::new(),
            hs_auth::state::AuthState::in_memory(),
            Some(&path),
            &hs_telemetry::metrics::Metrics::new(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected an invalid-scanning-config error"),
        };
        assert!(matches!(err, MediaSetupError::InvalidScanningConfig(_)));
    }
}
