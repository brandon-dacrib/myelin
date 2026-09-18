//! Error type for `hs-spec-coverage`.

use std::path::PathBuf;

/// Something went wrong loading the spec tree or a `routes.json` manifest.
#[derive(Debug, thiserror::Error)]
pub enum CoverageError {
    /// Could not read a file.
    #[error("reading {path}: {source}")]
    Io {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A spec YAML file did not parse.
    #[error("parsing {path} as YAML: {source}")]
    Yaml {
        /// The file that failed to parse.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: serde_yaml_ng::Error,
    },

    /// A `routes.json` manifest did not parse.
    #[error("parsing {path} as routes.json: {source}")]
    Json {
        /// The file that failed to parse.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: serde_json::Error,
    },

    /// `spec_root` (the directory expected to hold `client-server/`, `server-server/`, ...) does
    /// not exist. Points the caller at `tools/fetch-refs.sh`.
    #[error(
        "spec directory {0} does not exist; run tools/fetch-refs.sh to clone refs/matrix-spec (network required)"
    )]
    MissingSpecRoot(PathBuf),
}
