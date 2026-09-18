//! `hs serve --synapse-config <path>`: translates a Synapse `homeserver.yaml` through
//! `hs_compat::translate::translate`, prints the translation report, and continues exactly as
//! `hs serve -c <native.yaml>` would (`docs/compat/cli-shims.md`).

use std::path::Path;

use hs_compat::{TranslateError, TranslateOptions, TranslationReport, translate};
use hs_config::Config;

/// The outcome of loading a configuration via `--synapse-config`: the resulting native `Config`
/// (with process environment `HS__` overrides already applied on top, per the shim spec) and the
/// translation report to print.
#[derive(Debug)]
pub struct SynapseServeConfig {
    /// The translated (and environment-overridden) configuration.
    pub config: Config,
    /// The full translation report, for printing before the server starts.
    pub report: TranslationReport,
}

/// Errors loading and translating a Synapse config for `hs serve --synapse-config`. Each variant
/// carries what step 3/4 of `docs/compat/cli-shims.md`'s `hs serve --synapse-config` section says
/// to print.
#[derive(Debug, thiserror::Error)]
pub enum SynapseServeError {
    /// The source file could not be read.
    #[error("failed to read {path:?}: {source}")]
    ReadFile {
        /// The path that failed.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The source set unsupported or unrecognized keys and `--allow-unsupported-synapse-config`
    /// was not passed. Display already formats one line per blocking key (see
    /// `hs_compat::translate::TranslateError`'s `Display`).
    #[error("{0}")]
    Unsupported(TranslateError),
    /// The source was not valid YAML, or the translated configuration failed `hs-config`'s own
    /// validation.
    #[error("{0}")]
    Invalid(TranslateError),
    /// Re-validating the translated config after applying `HS__` environment overrides failed.
    #[error("configuration is invalid after applying environment overrides: {0}")]
    PostEnvValidation(#[from] hs_config::ConfigError),
}

/// Loads and translates a Synapse `homeserver.yaml` at `path`, applying `HS__` environment
/// overrides on top of the translated result exactly as a native `-c` load would.
///
/// # Errors
/// See [`SynapseServeError`].
pub fn load_synapse_config(
    path: &Path,
    allow_unsupported: bool,
    env_vars: impl IntoIterator<Item = (String, String)>,
) -> Result<SynapseServeConfig, SynapseServeError> {
    let contents = std::fs::read_to_string(path).map_err(|source| SynapseServeError::ReadFile {
        path: path.to_owned(),
        source,
    })?;

    let (config, report) =
        translate(&contents, TranslateOptions { allow_unsupported }).map_err(|e| match &e {
            TranslateError::Unsupported { .. } => SynapseServeError::Unsupported(e),
            TranslateError::Yaml(_) | TranslateError::Config(_) => SynapseServeError::Invalid(e),
        })?;

    let config = reapply_env_overrides(config, env_vars)?;
    Ok(SynapseServeConfig { config, report })
}

/// Re-serializes `config` to a YAML tree, applies `HS__` overrides, then re-parses/resolves/
/// validates via [`Config::from_value`] — the same path a native `-c` load takes after
/// `apply_process_env_overrides`.
///
/// One wrinkle `translate` leaves behind: for any Synapse `*_path`-style secret option (e.g.
/// `registration_shared_secret_path`), `translate` both sets the native `..._file` field *and*
/// (via its own internal `config.resolve_secrets()` call) resolves it into the inline field —
/// leaving both set simultaneously. Re-running `resolve_secrets` on that tree (which
/// [`Config::from_value`] always does) would then hit `ConfigError::SecretConflict`, since a
/// field and its `_file` companion being set together is normally an operator error, not an
/// artifact of translation. [`strip_resolved_secret_file_siblings`] removes exactly the `_file`
/// sibling of every secret whose plain field is already populated, generically (structurally,
/// not by a hardcoded field list — so it does not need updating as other tracks add new
/// secret-bearing config fields), before the round trip.
fn reapply_env_overrides(
    config: Config,
    env_vars: impl IntoIterator<Item = (String, String)>,
) -> Result<Config, hs_config::ConfigError> {
    let mut value = serde_yaml_ng::to_value(&config).expect("Config always serializes");
    strip_resolved_secret_file_siblings(&mut value);
    hs_config::env::apply_env_overrides(&mut value, env_vars);
    Config::from_value(value)
}

/// Recursively removes any mapping key ending in `_file` whose non-`_file` stem is also present
/// (and non-null) in the same mapping. See [`reapply_env_overrides`] for why this is needed.
fn strip_resolved_secret_file_siblings(value: &mut serde_yaml_ng::Value) {
    match value {
        serde_yaml_ng::Value::Mapping(map) => {
            let stems_present: Vec<String> = map
                .keys()
                .filter_map(|k| k.as_str())
                .filter(|k| !matches!(map.get(*k), Some(serde_yaml_ng::Value::Null) | None))
                .map(str::to_owned)
                .collect();
            let to_remove: Vec<String> = map
                .keys()
                .filter_map(|k| k.as_str())
                .filter(|k| k.ends_with("_file"))
                .filter(|k| stems_present.contains(&k[..k.len() - "_file".len()].to_owned()))
                .map(str::to_owned)
                .collect();
            for key in to_remove {
                map.remove(serde_yaml_ng::Value::String(key));
            }
            for (_, v) in map.iter_mut() {
                strip_resolved_secret_file_siblings(v);
            }
        }
        serde_yaml_ng::Value::Sequence(seq) => {
            for v in seq.iter_mut() {
                strip_resolved_secret_file_siblings(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_file_siblings_whose_stem_is_present() {
        let yaml = "a: 1\na_file: /tmp/x\nb_file: /tmp/y\nnested:\n  c: 2\n  c_file: /tmp/z\n";
        let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
        strip_resolved_secret_file_siblings(&mut value);
        let map = value.as_mapping().unwrap();
        assert!(
            map.get(serde_yaml_ng::Value::String("a_file".into()))
                .is_none()
        );
        assert!(
            map.get(serde_yaml_ng::Value::String("b_file".into()))
                .is_some(),
            "b has no stem `b`, so b_file must survive"
        );
        let nested = map
            .get(serde_yaml_ng::Value::String("nested".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert!(
            nested
                .get(serde_yaml_ng::Value::String("c_file".into()))
                .is_none()
        );
    }

    #[test]
    fn end_to_end_translate_then_env_override() {
        let dir =
            std::env::temp_dir().join(format!("hs-cli-synapse-serve-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("homeserver.yaml");
        std::fs::write(&path, "server_name: example.org\n").unwrap();

        let result = load_synapse_config(
            &path,
            false,
            [(
                "HS__SERVER__SERVER_NAME".to_string(),
                "overridden.example".to_string(),
            )],
        )
        .unwrap();
        assert_eq!(result.config.server.server_name, "overridden.example");
        assert!(!result.report.outcomes.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn secret_path_style_synapse_option_does_not_conflict_on_env_reapply() {
        let dir = std::env::temp_dir().join(format!(
            "hs-cli-synapse-serve-secret-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let secret_path = dir.join("shared_secret");
        std::fs::write(&secret_path, "topsecret\n").unwrap();
        let hs_yaml = dir.join("homeserver.yaml");
        std::fs::write(
            &hs_yaml,
            format!(
                "server_name: example.org\nregistration_shared_secret_path: {:?}\n",
                secret_path
            ),
        )
        .unwrap();

        let result = load_synapse_config(&hs_yaml, false, std::iter::empty()).unwrap();
        assert_eq!(
            result.config.auth.registration_shared_secret.as_str(),
            Some("topsecret")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unsupported_key_is_reported_as_unsupported_error() {
        let dir = std::env::temp_dir().join(format!(
            "hs-cli-synapse-serve-unsupported-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("homeserver.yaml");
        std::fs::write(
            &path,
            "server_name: example.org\ngc_thresholds: [100, 10, 10]\n",
        )
        .unwrap();

        let err = load_synapse_config(&path, false, std::iter::empty()).unwrap_err();
        assert!(matches!(err, SynapseServeError::Unsupported(_)));
        std::fs::remove_dir_all(dir).ok();
    }
}
