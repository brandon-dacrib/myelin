//! Builds `hs-media`'s [`MediaState`] from native config, and (track assignment item 2) attaches
//! content scanning per `docs/status/09-media.md`'s "The homeserver startup wiring itself" gap:
//! that track built every piece (`ScanningConfig::from_yaml` / `::validated`,
//! `ScanEngine::new`, `MediaRepository::with_scanning`) but noted "no `hs-*` binary reads
//! `media.scanning` from a config file or calls `MediaRepository::with_scanning` at startup yet" —
//! this module is that wiring.
//!
//! # Why `media.scanning` is its own file, not part of `-c`/`--config`
//!
//! `hs_config::MediaConfig` has no `scanning` field (`crates/hs-media/src/scanning/config.rs`'s
//! own module doc: `ScanningConfig` "deliberately does **not** live in `hs_config::MediaConfig`"
//! since track 09 does not own that crate). Track 13 owns folding it in; until that lands, this
//! follows the exact precedent `crate::versions` already set for the same shaped problem
//! (`unstable_features` also cannot live in the main config file) — an optional
//! `--media-scanning-config <path>` YAML file, read with [`hs_media::scanning::ScanningConfig::from_yaml`]
//! directly (the shape `deploy/media-scanning/media-scanning.yaml` already documents). Omitting
//! the flag is exactly [`hs_media::scanning::ScanningConfig::default`] — mode `off`, zero
//! behavioral change, matching every existing `MediaRepository` caller.

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
    /// `--media-scanning-config`'s YAML did not parse, or failed `ScanningConfig::validated`
    /// (an unset failure policy while scanning is enabled, an empty provider config, etc).
    #[error("invalid --media-scanning-config: {0}")]
    InvalidScanningConfig(hs_media::MediaError),
    /// Building the configured scan provider (or opening its verdict cache) failed.
    #[error("failed to build the content scanning engine: {0}")]
    ScanEngine(hs_media::MediaError),
}

/// Builds a [`MediaState`] from native config: the object-store backend
/// (`hs_media::store::build`, already written against `hs_config::MediaStorageBackend`), an
/// `hs-tables`-backed [`hs_media::metadata::MetadataStore`] over `backend`, an unlimited
/// [`hs_media::policy::InMemoryQuotaPolicy`] (`hs_config::MediaConfig` has no per-user/per-server
/// quota fields yet — see `docs/status/12-platform-and-kubernetes.md`), and — if
/// `media_scanning_config` is given — a real [`ScanEngine`] attached via
/// [`MediaRepository::with_scanning`].
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

    if let Some(path) = media_scanning_config {
        let engine = build_scan_engine(path, backend, metrics)?;
        repository = repository.with_scanning(engine);
    }

    Ok(MediaState {
        auth,
        repository: Arc::new(repository),
        legacy_media_enabled: config.media.allow_legacy_unauthenticated_media,
        legacy_freeze_ms: None,
    })
}

fn build_scan_engine<B: KvBackend>(
    path: &std::path::Path,
    backend: B,
    metrics: &hs_telemetry::metrics::Metrics,
) -> Result<ScanEngine<B>, MediaSetupError> {
    let yaml =
        std::fs::read_to_string(path).map_err(|source| MediaSetupError::ReadScanningConfig {
            path: path.to_owned(),
            source,
        })?;
    let config = ScanningConfig::from_yaml(&yaml)
        .and_then(ScanningConfig::validated)
        .map_err(MediaSetupError::InvalidScanningConfig)?;
    let scan_metrics = Arc::new(ScanMetrics::register(metrics));
    // `TracingAuditSink`: scan decisions land in the structured log stream until an operator
    // needs the admin API's audit surface (RFC 0011, still design-only per
    // `docs/status/09-media.md` — `ScanAdmin` has no implementation to hand a richer sink to
    // yet).
    ScanEngine::new(config, backend, scan_metrics, Arc::new(TracingAuditSink))
        .map_err(MediaSetupError::ScanEngine)
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
