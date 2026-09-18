//! File-backed secrets: every secret-bearing config field comes in a pair,
//! `field` (inline) and `field_file` (a path read at load time), and
//! exactly one of the two may be set.
//!
//! This mirrors the Docker/Kubernetes convention (and Synapse's own
//! `*_path` secret options) of injecting secrets as mounted files rather
//! than environment variables or inline YAML, without losing the ability to
//! write a literal value in a dev config or an `HS__` override.

use std::fmt;
use std::path::{Path, PathBuf};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// An inline secret value. Deserializes from a plain string; `Debug` output
/// is redacted so secrets never land in logs or panic messages by accident.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(pub Option<String>);

impl SecretString {
    /// True when a value is set.
    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }

    /// The secret's bytes, if set.
    pub fn as_str(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(_) => f.write_str("SecretString(\"<redacted>\")"),
            None => f.write_str("SecretString(None)"),
        }
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self(Some(s.to_owned()))
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(Some(s))
    }
}

impl JsonSchema for SecretString {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretString".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "An inline secret value. Prefer the matching `*_file` key to avoid putting secrets in the config file.",
            "x-secret": true
        })
    }
}

/// Reads a secret file, trimming a single trailing newline (the common
/// convention for files written by `echo` or Kubernetes secret mounts).
fn read_secret_file(field: &str, path: &Path) -> Result<String, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::SecretFile {
        field: field.to_owned(),
        path: path.to_owned(),
        source,
    })?;
    Ok(raw.strip_suffix('\n').map(str::to_owned).unwrap_or(raw))
}

/// Resolves one `field` / `field_file` pair in place: if `file` is set and
/// `secret` is not, reads the file into `secret`. Errors if both are set.
/// Does nothing if neither is set — an unset secret is the caller's problem,
/// caught (or not) by validation.
pub fn resolve_secret_pair(
    field: &str,
    secret: &mut SecretString,
    file: &Option<PathBuf>,
) -> Result<(), ConfigError> {
    match (&secret.0, file) {
        (Some(_), Some(_)) => Err(ConfigError::SecretConflict {
            field: field.to_owned(),
        }),
        (None, Some(path)) => {
            secret.0 = Some(read_secret_file(field, path)?);
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = SecretString::from("s3kr1t");
        assert_eq!(format!("{s:?}"), "SecretString(\"<redacted>\")");
        assert_eq!(
            format!("{:?}", SecretString::default()),
            "SecretString(None)"
        );
    }

    #[test]
    fn resolves_from_file_and_trims_newline() {
        let dir = tempfile_dir();
        let path = dir.join("secret.txt");
        std::fs::write(&path, "hunter2\n").unwrap();
        let mut secret = SecretString::default();
        let file = Some(path);
        resolve_secret_pair("auth.session_secret", &mut secret, &file).unwrap();
        assert_eq!(secret.as_str(), Some("hunter2"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn conflict_when_both_set() {
        let mut secret = SecretString::from("inline");
        let file = Some(PathBuf::from("/nonexistent"));
        let err = resolve_secret_pair("auth.session_secret", &mut secret, &file).unwrap_err();
        assert!(
            matches!(err, ConfigError::SecretConflict { field } if field == "auth.session_secret")
        );
    }

    #[test]
    fn missing_file_is_reported_with_field_name() {
        let mut secret = SecretString::default();
        let file = Some(PathBuf::from("/definitely/not/a/real/path/xyz"));
        let err = resolve_secret_pair("storage.postgres.password", &mut secret, &file).unwrap_err();
        match err {
            ConfigError::SecretFile { field, .. } => assert_eq!(field, "storage.postgres.password"),
            other => panic!("wrong error: {other:?}"),
        }
    }

    // Minimal temp-dir helper so this crate does not need a dev-dependency
    // on `tempfile` just for one test.
    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
