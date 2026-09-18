//! Errors produced while loading configuration, and the [`Validate`] trait
//! every config section implements.

use std::fmt;
use std::path::PathBuf;

/// Errors from reading, parsing, overriding or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("failed to read config file {path:?}: {source}")]
    ReadFile {
        /// The path that was opened.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The config file (or an environment override value) was not valid
    /// YAML, or did not match the schema.
    #[error("failed to parse configuration: {0}")]
    Parse(#[from] serde_yaml_ng::Error),

    /// A secret file referenced by a `*_file` key could not be read.
    #[error("failed to read secret file {path:?} for `{field}`: {source}")]
    SecretFile {
        /// The dotted config path of the field the secret belongs to.
        field: String,
        /// The file path that was opened.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Both the inline field and its `_file` companion were set.
    #[error(
        "both `{field}` and `{field}_file` are set; set only one (the file wins nothing — this is an error, not a fallback)"
    )]
    SecretConflict {
        /// The dotted config path of the field.
        field: String,
    },

    /// One or more validation checks failed.
    #[error("configuration is invalid:\n{0}")]
    Validation(ValidationErrors),
}

/// One validation failure: the dotted path of the offending field and a
/// human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Dotted path, e.g. `storage.postgres.pool_size`.
    pub path: String,
    /// A message safe to show an operator directly.
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// A collection of [`ValidationError`]s gathered across every section so an
/// operator sees every problem in one pass instead of fixing them one at a
/// time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl ValidationErrors {
    /// An empty error set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a failure at `path`.
    pub fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(ValidationError {
            path: path.into(),
            message: message.into(),
        });
    }

    /// Merges another section's errors into this one.
    pub fn extend(&mut self, other: ValidationErrors) {
        self.0.extend(other.0);
    }

    /// True when nothing failed.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Converts to `Result`, wrapping in [`ConfigError::Validation`] when
    /// non-empty.
    pub fn into_result(self) -> Result<(), ConfigError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(self))
        }
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "  - {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// Implemented by every config section. `validate` never panics and never
/// does I/O; it only checks internal consistency of already-parsed values.
pub trait Validate {
    /// Appends any problems found to `errors`, prefixing paths with
    /// `prefix` (the section's own key, e.g. `"storage"`).
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors);
}
