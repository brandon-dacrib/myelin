//! `GET /_matrix/client/versions`: the supported spec versions list and the config-driven
//! `unstable_features` map every Matrix client and mautrix bridge calls first
//! (`PLAN.md` Appendix B, "Feature flags probed on `/versions`", is the subset bridges branch on
//! immediately; `docs/synapse-inventory.md`'s "`unstable_features` advertised by /versions (37)"
//! is the full reference list). This was a blocking gap: nothing served this route at all, so
//! Complement's image contract (which requires `/versions` to answer `200` before any test runs)
//! could never be satisfied, and every client/bridge that probes it first would fail immediately.
//!
//! # Why the defaults below are what they are
//!
//! `versions`: the exact set Synapse 1.161 advertises (`docs/synapse-inventory.md`'s "Spec
//! versions advertised" line — our compatibility reference). A homeserver's `/versions` response
//! is a protocol-level "which API dialects do I speak" declaration, not a per-endpoint capability
//! promise — that is what `unstable_features` and plain per-route 404s are for. Matching the
//! reference implementation's own claimed set keeps client/bridge version-gated branches behaving
//! the same way against us as against Synapse.
//!
//! `unstable_features`: **empty** by default, deliberately. Every one of the 37 flags
//! `docs/synapse-inventory.md` lists, and every one `PLAN.md` Appendix B lists bridges as
//! actually probing, gates a feature this server does not implement yet: as of this change,
//! `hs serve` mounts only `hs-auth`'s legacy login/register/devices routes plus this module and
//! [`crate::capabilities`] (see `crates/hs-cli/src/serve.rs::build_router` for the complete,
//! current list). Advertising async media, appservice ping, authenticated media, mutual rooms,
//! extended profiles, MatrixRTC, or any other flag whose route is not mounted here would make a
//! bridge take a code path this server cannot serve, which is worse for interop than the bridge
//! falling back to its no-flag behavior. As other tracks' routers get mounted into `hs serve`, add
//! their justified flags to [`default_unstable_features`] next to the route that justifies each
//! one, and note the addition in `docs/status/12-platform-and-kubernetes.md`.
//!
//! # Configuration
//!
//! `unstable_features` cannot live in the main native `hs-config` YAML file: `hs_config::Config`
//! denies unknown top-level keys (see that crate's `unknown_top_level_key_is_rejected` test), and
//! this track does not own that schema. Instead, `hs serve --capabilities-config <path>`
//! (optional) points at a small standalone YAML file:
//!
//! ```yaml
//! unstable_features:
//!   org.matrix.msc1234: true
//! ```
//!
//! merged onto [`default_unstable_features`] (entries in the file win; an explicit `false` can
//! also suppress a built-in default). This is a stopgap `hs-cli`-owned config surface, not a
//! long-term home — `docs/status/12-platform-and-kubernetes.md` lists "give `hs-config` a real
//! `capabilities` section" as an interface needed from track 13.

use std::collections::BTreeMap;
use std::path::Path;

use axum::Json;
use axum::extract::Extension;
use serde::{Deserialize, Serialize};

/// The `GET /_matrix/client/versions` response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionsResponse {
    /// Every spec version this server claims to speak.
    pub versions: Vec<String>,
    /// Unstable/experimental feature flags, `MSC-identifier -> enabled`.
    pub unstable_features: BTreeMap<String, bool>,
}

/// The exact spec-version set Synapse 1.161 advertises (`docs/synapse-inventory.md`), our
/// compatibility reference. Kept as a `Vec` (not sorted lexically — `v1.10` through `v1.12` would
/// sort before `v1.2` as plain strings) in the conventional numeric reading order.
#[must_use]
pub fn supported_versions() -> Vec<String> {
    [
        "r0.0.1", "r0.1.0", "r0.2.0", "r0.3.0", "r0.4.0", "r0.5.0", "r0.6.0", "r0.6.1", "v1.1",
        "v1.2", "v1.3", "v1.4", "v1.5", "v1.6", "v1.7", "v1.8", "v1.9", "v1.10", "v1.11", "v1.12",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// See the module doc: empty today, on purpose. A function (not a constant) so the doc comment
/// sits next to the thing it documents and future entries are obviously additions, not edits to
/// a literal.
#[must_use]
pub fn default_unstable_features() -> BTreeMap<String, bool> {
    BTreeMap::new()
}

/// Errors loading `--capabilities-config`.
#[derive(Debug, thiserror::Error)]
pub enum CapabilitiesConfigError {
    /// The file could not be read.
    #[error("failed to read {path:?}: {source}")]
    Read {
        /// The path that failed.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file was not valid YAML in the expected shape.
    #[error("failed to parse {path:?} as YAML: {source}")]
    Parse {
        /// The path that failed.
        path: std::path::PathBuf,
        /// The underlying YAML error.
        #[source]
        source: serde_yaml_ng::Error,
    },
}

#[derive(Debug, Default, Deserialize)]
struct CapabilitiesConfigFile {
    #[serde(default)]
    unstable_features: BTreeMap<String, bool>,
}

/// Loads `unstable_features` overrides from an optional `--capabilities-config` file and merges
/// them onto [`default_unstable_features`] (file entries win).
///
/// # Errors
/// Returns [`CapabilitiesConfigError`] if `path` is `Some` and the file cannot be read or parsed.
/// `path: None` always succeeds with just the built-in defaults.
pub fn load_unstable_features(
    path: Option<&Path>,
) -> Result<BTreeMap<String, bool>, CapabilitiesConfigError> {
    let mut features = default_unstable_features();
    if let Some(path) = path {
        let contents =
            std::fs::read_to_string(path).map_err(|source| CapabilitiesConfigError::Read {
                path: path.to_owned(),
                source,
            })?;
        let file: CapabilitiesConfigFile =
            serde_yaml_ng::from_str(&contents).map_err(|source| {
                CapabilitiesConfigError::Parse {
                    path: path.to_owned(),
                    source,
                }
            })?;
        features.extend(file.unstable_features);
    }
    Ok(features)
}

/// `GET /_matrix/client/versions` handler. Takes the feature map via [`Extension`] rather than
/// axum `State` so it can be registered directly on an `hs_http::router::Builder<()>` alongside
/// every other route (see `crate::serve::build_router`), with the actual value injected as an
/// outer layer once after the router is built.
pub async fn get_versions(
    Extension(features): Extension<std::sync::Arc<BTreeMap<String, bool>>>,
) -> Json<VersionsResponse> {
    Json(VersionsResponse {
        versions: supported_versions(),
        unstable_features: (*features).clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_unstable_features_is_empty() {
        assert!(default_unstable_features().is_empty());
    }

    #[test]
    fn supported_versions_matches_synapse_inventory_count() {
        // docs/synapse-inventory.md: 8 r0.x.x entries + 12 v1.x entries (v1.1 through v1.12).
        assert_eq!(supported_versions().len(), 20);
        assert!(supported_versions().contains(&"v1.1".to_owned()));
        assert!(supported_versions().contains(&"r0.6.1".to_owned()));
    }

    #[test]
    fn load_unstable_features_with_no_path_returns_defaults() {
        let features = load_unstable_features(None).unwrap();
        assert_eq!(features, default_unstable_features());
    }

    #[test]
    fn load_unstable_features_merges_file_onto_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capabilities.yaml");
        std::fs::write(
            &path,
            "unstable_features:\n  org.matrix.msc9999: true\n  org.matrix.msc8888: false\n",
        )
        .unwrap();
        let features = load_unstable_features(Some(&path)).unwrap();
        assert_eq!(features.get("org.matrix.msc9999"), Some(&true));
        assert_eq!(features.get("org.matrix.msc8888"), Some(&false));
    }

    #[test]
    fn load_unstable_features_reports_a_missing_file_clearly() {
        let err =
            load_unstable_features(Some(std::path::Path::new("/no/such/file.yaml"))).unwrap_err();
        assert!(matches!(err, CapabilitiesConfigError::Read { .. }));
    }

    #[test]
    fn versions_response_serializes_with_expected_field_names() {
        let mut features = BTreeMap::new();
        features.insert("org.matrix.msc1234".to_owned(), true);
        let body = VersionsResponse {
            versions: vec!["v1.1".to_owned()],
            unstable_features: features,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["versions"], serde_json::json!(["v1.1"]));
        assert_eq!(json["unstable_features"]["org.matrix.msc1234"], true);
    }
}
