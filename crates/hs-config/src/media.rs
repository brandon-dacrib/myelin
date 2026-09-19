//! Media repository configuration: object storage backend, upload limits,
//! thumbnailing and URL previews. See `PLAN.md` D6.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};
use crate::secret::{SecretString, resolve_secret_pair};
use crate::{ByteSize, ConfigError, Duration};

/// Where media bytes live. Restart required to change (see
/// [`crate::reload`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum MediaStorageBackend {
    /// Local filesystem. Corresponds to Synapse's `media_store_path`.
    Local {
        /// Root directory for stored media.
        path: PathBuf,
    },
    /// S3-compatible object storage.
    S3 {
        /// Bucket name.
        bucket: String,
        /// Region, if the endpoint requires one.
        #[serde(default)]
        region: Option<String>,
        /// Custom endpoint for S3-compatible services (MinIO, R2, ...).
        #[serde(default)]
        endpoint: Option<String>,
        /// Access key ID.
        #[serde(default)]
        access_key_id: Option<String>,
        /// Inline secret access key. Prefer `secret_access_key_file`.
        #[serde(default)]
        secret_access_key: SecretString,
        /// Path to a file containing the secret access key.
        #[serde(default)]
        secret_access_key_file: Option<PathBuf>,
    },
    /// Google Cloud Storage.
    Gcs {
        /// Bucket name.
        bucket: String,
        /// Path to a service account JSON key file.
        #[serde(default)]
        service_account_key_file: Option<PathBuf>,
    },
    /// Azure Blob Storage.
    Azure {
        /// Container name.
        container: String,
        /// Storage account name.
        account: String,
        /// Inline access key. Prefer `access_key_file`.
        #[serde(default)]
        access_key: SecretString,
        /// Path to a file containing the access key.
        #[serde(default)]
        access_key_file: Option<PathBuf>,
    },
}

impl Default for MediaStorageBackend {
    fn default() -> Self {
        MediaStorageBackend::Local {
            path: PathBuf::from("./media-store"),
        }
    }
}

/// One generated thumbnail size. Corresponds to one entry in Synapse's
/// `thumbnail_sizes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThumbnailSize {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// Resize method.
    pub method: ThumbnailMethod,
}

/// How a thumbnail is fit to its target size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThumbnailMethod {
    /// Crop to exactly fill the target box.
    Crop,
    /// Scale to fit within the target box, preserving aspect ratio.
    Scale,
}

fn default_thumbnail_sizes() -> Vec<ThumbnailSize> {
    vec![
        ThumbnailSize {
            width: 32,
            height: 32,
            method: ThumbnailMethod::Crop,
        },
        ThumbnailSize {
            width: 96,
            height: 96,
            method: ThumbnailMethod::Crop,
        },
        ThumbnailSize {
            width: 320,
            height: 240,
            method: ThumbnailMethod::Scale,
        },
        ThumbnailSize {
            width: 640,
            height: 480,
            method: ThumbnailMethod::Scale,
        },
        ThumbnailSize {
            width: 800,
            height: 600,
            method: ThumbnailMethod::Scale,
        },
    ]
}

fn default_max_upload_size() -> ByteSize {
    ByteSize::mib(50)
}

fn default_remote_media_retention() -> Option<Duration> {
    None
}

const fn default_true() -> bool {
    true
}

/// Media repository settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MediaConfig {
    /// Storage backend. Corresponds to Synapse's `media_storage_providers`
    /// (simplified to one active backend; a caching remote provider is a
    /// separate, orthogonal setting in Synapse we fold into `local` plus
    /// the object-store cache layer).
    #[serde(default)]
    pub storage: MediaStorageBackend,
    /// Maximum accepted upload size. Corresponds to Synapse's
    /// `max_upload_size`.
    #[serde(default = "default_max_upload_size")]
    pub max_upload_size: ByteSize,
    /// Thumbnail sizes to pre-generate/serve on demand. Corresponds to
    /// Synapse's `thumbnail_sizes`.
    #[serde(default = "default_thumbnail_sizes")]
    pub thumbnail_sizes: Vec<ThumbnailSize>,
    /// Enable `GET /_matrix/media/*/preview_url`. Corresponds to Synapse's
    /// `url_preview_enabled`.
    #[serde(default)]
    pub url_preview_enabled: bool,
    /// IP ranges URL previews must not fetch from (SSRF protection).
    /// Corresponds to Synapse's `url_preview_ip_range_blacklist`.
    #[serde(default = "default_preview_blocklist")]
    pub url_preview_ip_range_blocklist: Vec<String>,
    /// How long to keep cached copies of remote media. `None` means keep
    /// forever. Corresponds to Synapse's
    /// `media_retention.remote_media_lifetime`.
    #[serde(default = "default_remote_media_retention")]
    pub remote_media_retention: Option<Duration>,
    /// Serve the pre-authentication-media (legacy, unauthenticated)
    /// endpoints alongside the authenticated ones. Corresponds to
    /// Synapse's `enable_authenticated_media` (inverted: this flag adds
    /// the legacy endpoints rather than removing the new ones, since
    /// authenticated media is not optional here).
    #[serde(default = "default_true")]
    pub allow_legacy_unauthenticated_media: bool,
    /// Per-request timeout for a `preview_url` fetch (each redirect hop is
    /// timed separately). Synapse has no config knob for this — it hardcodes
    /// a 30-second body-read timeout in its HTTP client
    /// (`refs/synapse/synapse/http/client.py`'s `get_file`, `timeout_deferred(...,
    /// timeout=30, ...)`); this project exposes it as a real setting instead.
    /// See `docs/rfcs/0006-url-previews.md` section 4.5 and
    /// `hs_media::preview::FetchLimits::timeout`, which this field is meant
    /// to populate (`hs-media` cannot depend on this crate's consumer wiring
    /// — see that crate's status file for the one-line change needed).
    #[serde(default = "default_url_preview_timeout")]
    pub url_preview_timeout: Duration,
    /// Maximum response body size accepted for a `preview_url` page fetch or
    /// its `og:image` fetch (each capped independently at this value).
    /// Corresponds to Synapse's `max_spider_size`, whose own default is
    /// `"10M"` (`refs/synapse/synapse/config/repository.py`:
    /// `self.max_spider_size = self.parse_size(config.get("max_spider_size",
    /// "10M"))`). See `hs_media::preview::FetchLimits::max_body_bytes`.
    #[serde(default = "default_url_preview_max_fetch_size")]
    pub url_preview_max_fetch_size: ByteSize,
    /// How long a `preview_url` response is cached before it is fetched
    /// again. Synapse has no config knob for this either — it hardcodes a
    /// one-hour cache lifetime (`refs/synapse/synapse/media/url_previewer.py`:
    /// `ONE_HOUR = 60 * 60 * 1000`, used as both the in-memory and the
    /// on-disk `url_cache` expiry); this project exposes it as a real
    /// setting instead. See `hs_media::preview::DEFAULT_PREVIEW_CACHE_TTL_MS`,
    /// which this field is meant to replace.
    #[serde(default = "default_url_preview_cache_lifetime")]
    pub url_preview_cache_lifetime: Duration,
}

fn default_url_preview_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_url_preview_max_fetch_size() -> ByteSize {
    ByteSize::mib(10)
}

fn default_url_preview_cache_lifetime() -> Duration {
    Duration::from_hours(1)
}

fn default_preview_blocklist() -> Vec<String> {
    vec![
        "127.0.0.0/8".into(),
        "10.0.0.0/8".into(),
        "172.16.0.0/12".into(),
        "192.168.0.0/16".into(),
        "100.64.0.0/10".into(),
        "169.254.0.0/16".into(),
        "::1/128".into(),
        "fe80::/10".into(),
        "fc00::/7".into(),
    ]
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            storage: MediaStorageBackend::default(),
            max_upload_size: default_max_upload_size(),
            thumbnail_sizes: default_thumbnail_sizes(),
            url_preview_enabled: false,
            url_preview_ip_range_blocklist: default_preview_blocklist(),
            remote_media_retention: default_remote_media_retention(),
            allow_legacy_unauthenticated_media: true,
            url_preview_timeout: default_url_preview_timeout(),
            url_preview_max_fetch_size: default_url_preview_max_fetch_size(),
            url_preview_cache_lifetime: default_url_preview_cache_lifetime(),
        }
    }
}

impl Validate for MediaConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.max_upload_size.as_u64() == 0 {
            errors.push(
                format!("{prefix}.max_upload_size"),
                "must be greater than 0",
            );
        }
        for (i, t) in self.thumbnail_sizes.iter().enumerate() {
            if t.width == 0 || t.height == 0 {
                errors.push(
                    format!("{prefix}.thumbnail_sizes[{i}]"),
                    format!(
                        "width and height must be non-zero (got {}x{})",
                        t.width, t.height
                    ),
                );
            }
        }
        for (i, cidr) in self.url_preview_ip_range_blocklist.iter().enumerate() {
            if let Err(e) = validate_cidr(cidr) {
                errors.push(format!("{prefix}.url_preview_ip_range_blocklist[{i}]"), e);
            }
        }
        if self.url_preview_timeout.is_zero() {
            errors.push(
                format!("{prefix}.url_preview_timeout"),
                "must be greater than 0",
            );
        }
        if self.url_preview_max_fetch_size.as_u64() == 0 {
            errors.push(
                format!("{prefix}.url_preview_max_fetch_size"),
                "must be greater than 0",
            );
        }
        match &self.storage {
            MediaStorageBackend::Local { path } => {
                if path.as_os_str().is_empty() {
                    errors.push(format!("{prefix}.storage.path"), "must not be empty");
                }
            }
            MediaStorageBackend::S3 { bucket, .. } => {
                if bucket.trim().is_empty() {
                    errors.push(format!("{prefix}.storage.bucket"), "must not be empty");
                }
            }
            MediaStorageBackend::Gcs { bucket, .. } => {
                if bucket.trim().is_empty() {
                    errors.push(format!("{prefix}.storage.bucket"), "must not be empty");
                }
            }
            MediaStorageBackend::Azure {
                container, account, ..
            } => {
                if container.trim().is_empty() {
                    errors.push(format!("{prefix}.storage.container"), "must not be empty");
                }
                if account.trim().is_empty() {
                    errors.push(format!("{prefix}.storage.account"), "must not be empty");
                }
            }
        }
    }
}

/// A minimal CIDR syntax check (`<ip>/<prefix>` or a bare IP), without
/// pulling in a dedicated crate: enough to catch typos with a useful
/// message, not a full validator.
fn validate_cidr(s: &str) -> Result<(), String> {
    let (addr, prefix) = match s.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (s, None),
    };
    let ip: std::net::IpAddr = addr
        .parse()
        .map_err(|_| format!("{s:?} is not a valid CIDR (bad address {addr:?})"))?;
    if let Some(p) = prefix {
        let bits: u8 = p
            .parse()
            .map_err(|_| format!("{s:?} has a non-numeric prefix length"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if bits > max {
            return Err(format!(
                "{s:?} has prefix length {bits} but {addr:?} allows at most {max}"
            ));
        }
    }
    Ok(())
}

impl MediaConfig {
    /// Resolves any `*_file` secrets this backend carries.
    pub(crate) fn resolve_secrets(&mut self, prefix: &str) -> Result<(), ConfigError> {
        match &mut self.storage {
            MediaStorageBackend::S3 {
                secret_access_key,
                secret_access_key_file,
                ..
            } => {
                resolve_secret_pair(
                    &format!("{prefix}.storage.secret_access_key"),
                    secret_access_key,
                    secret_access_key_file,
                )?;
            }
            MediaStorageBackend::Azure {
                access_key,
                access_key_file,
                ..
            } => {
                resolve_secret_pair(
                    &format!("{prefix}.storage.access_key"),
                    access_key,
                    access_key_file,
                )?;
            }
            MediaStorageBackend::Local { .. } | MediaStorageBackend::Gcs { .. } => {}
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        MediaConfig::default().validate("media", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_zero_size_thumbnail() {
        let mut cfg = MediaConfig::default();
        cfg.thumbnail_sizes = vec![ThumbnailSize {
            width: 0,
            height: 96,
            method: ThumbnailMethod::Crop,
        }];
        let mut errors = ValidationErrors::new();
        cfg.validate("media", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert!(errors.0[0].message.contains("0x96"));
    }

    #[test]
    fn rejects_bad_cidr() {
        let mut cfg = MediaConfig::default();
        cfg.url_preview_ip_range_blocklist = vec!["not-an-ip".into()];
        let mut errors = ValidationErrors::new();
        cfg.validate("media", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert!(errors.0[0].message.contains("not-an-ip"));
    }

    #[test]
    fn rejects_prefix_too_long_for_ipv4() {
        assert!(validate_cidr("10.0.0.0/33").is_err());
        assert!(validate_cidr("10.0.0.0/24").is_ok());
        assert!(validate_cidr("::1/129").is_err());
    }

    #[test]
    fn rejects_zero_url_preview_timeout() {
        let mut cfg = MediaConfig::default();
        cfg.url_preview_timeout = Duration::ZERO;
        let mut errors = ValidationErrors::new();
        cfg.validate("media", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert_eq!(errors.0[0].path, "media.url_preview_timeout");
    }

    #[test]
    fn rejects_zero_url_preview_max_fetch_size() {
        let mut cfg = MediaConfig::default();
        cfg.url_preview_max_fetch_size = ByteSize::bytes(0);
        let mut errors = ValidationErrors::new();
        cfg.validate("media", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert_eq!(errors.0[0].path, "media.url_preview_max_fetch_size");
    }

    #[test]
    fn url_preview_defaults_match_synapse_behavior() {
        // See the field doc comments for exactly which `refs/synapse` source lines these were
        // checked against (Synapse hardcodes all three; none are Synapse config options).
        let cfg = MediaConfig::default();
        assert_eq!(cfg.url_preview_timeout, Duration::from_secs(30));
        assert_eq!(cfg.url_preview_max_fetch_size, ByteSize::mib(10));
        assert_eq!(cfg.url_preview_cache_lifetime, Duration::from_hours(1));
    }

    #[test]
    fn url_preview_fields_round_trip_through_yaml() {
        let yaml = "url_preview_timeout: \"5s\"\nurl_preview_max_fetch_size: \"2M\"\nurl_preview_cache_lifetime: \"30m\"\n";
        let cfg: MediaConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.url_preview_timeout, Duration::from_secs(5));
        assert_eq!(cfg.url_preview_max_fetch_size, ByteSize::mib(2));
        assert_eq!(cfg.url_preview_cache_lifetime, Duration::from_mins(30));
    }
}
