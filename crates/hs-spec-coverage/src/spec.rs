//! Parses the Matrix spec's OpenAPI trees (`refs/matrix-spec/data/api/<family>/*.yaml`) into a
//! flat list of `(method, path)` routes.
//!
//! Each file under a family directory (`client-server`, `server-server`, `application-service`,
//! `identity`, `push-gateway`) is an independent OpenAPI 3.1 document covering a handful of
//! related endpoints; a `definitions/` (and, for `server-server`, `examples/`) subdirectory holds
//! only `$ref` targets, never `paths` of its own, so this module reads exactly the top-level
//! `*.yaml` files in each family directory and does not recurse. Every such file declares its own
//! `servers[0].variables.basePath.default` (the mounted base path, e.g. `/_matrix/client/v3`);
//! this module prepends that to every path in the file's `paths` map, so [`SpecRoute::path`] is
//! already the full path a router would register it at — directly comparable to a `routes.json`
//! entry's `path` (RFC 0005), no extra join step required by the caller.

use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml_ng::Value as Yaml;

use crate::error::CoverageError;

/// The five Matrix APIs `docs/workstreams/14-test-and-conformance.md` and `PLAN.md` section 12
/// (layer L2) require coverage for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ApiFamily {
    /// `client-server`: the API clients speak to a homeserver.
    ClientServer,
    /// `server-server`: federation between homeservers.
    ServerServer,
    /// `application-service`: the homeserver-to-appservice push API.
    ApplicationService,
    /// `identity`: the identity service API.
    Identity,
    /// `push-gateway`: the homeserver-to-push-gateway notify API.
    PushGateway,
}

impl ApiFamily {
    /// Every family, in a stable order (used for iteration and report ordering).
    pub const ALL: [ApiFamily; 5] = [
        ApiFamily::ClientServer,
        ApiFamily::ServerServer,
        ApiFamily::ApplicationService,
        ApiFamily::Identity,
        ApiFamily::PushGateway,
    ];

    /// The subdirectory of `refs/matrix-spec/data/api/` this family's OpenAPI files live in.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::ClientServer => "client-server",
            Self::ServerServer => "server-server",
            Self::ApplicationService => "application-service",
            Self::Identity => "identity",
            Self::PushGateway => "push-gateway",
        }
    }

    /// The fallback base path used only if a file in this family is missing its own
    /// `servers[0].variables.basePath.default` (every file observed in the spec as of this
    /// writing declares one; this is a defensive default, not the primary source of truth).
    #[must_use]
    pub fn default_base_path(self) -> &'static str {
        match self {
            Self::ClientServer => "/_matrix/client/v3",
            Self::ServerServer => "/_matrix/federation/v1",
            Self::ApplicationService => "/_matrix/app/v1",
            Self::Identity => "/_matrix/identity/v2",
            Self::PushGateway => "/_matrix/push/v1",
        }
    }

    /// The `routes.json` `surface` value (`docs/rfcs/0005-routes-json-manifest.md`) this family
    /// corresponds to. RFC 0005 only lists `matrix-client`, `matrix-federation` and
    /// `matrix-appservice` (it predates this crate); it explicitly documents `surface` as an open
    /// enum consumers must tolerate unknown values on, so `identity` and `push-gateway` are new
    /// values in the same `matrix-<family>` shape rather than a breaking change to the format.
    /// See `docs/status/14-test-and-conformance.md` for the note recording this.
    #[must_use]
    pub fn manifest_surface(self) -> &'static str {
        match self {
            Self::ClientServer => "matrix-client",
            Self::ServerServer => "matrix-federation",
            Self::ApplicationService => "matrix-appservice",
            Self::Identity => "matrix-identity",
            Self::PushGateway => "matrix-push-gateway",
        }
    }
}

impl std::fmt::Display for ApiFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.dir_name())
    }
}

/// One route as declared by the spec: an HTTP method and a full path (base path already
/// prepended), plus enough provenance to point a human at the source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SpecRoute {
    /// Upper-case HTTP method (`GET`, `POST`, ...).
    pub method: String,
    /// The full path, e.g. `/_matrix/client/v3/rooms/{roomId}/join`.
    pub path: String,
    /// Which of the five APIs this route belongs to.
    pub family: ApiFamily,
    /// The operation's `operationId`, if the spec gave it one.
    pub operation_id: Option<String>,
    /// The file this route was declared in, relative to the family directory (e.g.
    /// `joining.yaml`), for pointing a human at the spec source.
    pub source_file: String,
}

const HTTP_METHODS: &[&str] = &["get", "put", "post", "delete", "patch", "head", "options"];

/// Loads every route declared by the top-level `*.yaml` files under `spec_root/<family>/`.
///
/// # Errors
/// Returns [`CoverageError::MissingSpecRoot`] if `spec_root/<family>` does not exist,
/// [`CoverageError::Io`] if a file cannot be read, or [`CoverageError::Yaml`] if a file is not
/// valid YAML.
pub fn load_family(spec_root: &Path, family: ApiFamily) -> Result<Vec<SpecRoute>, CoverageError> {
    let dir = spec_root.join(family.dir_name());
    if !dir.is_dir() {
        return Err(CoverageError::MissingSpecRoot(dir));
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|source| CoverageError::Io {
            path: dir.clone(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    entries.sort();

    let mut routes = Vec::new();
    for file in entries {
        routes.extend(load_file(&file, family)?);
    }
    Ok(routes)
}

/// [`load_family`] for every family in [`ApiFamily::ALL`], concatenated.
///
/// # Errors
/// See [`load_family`].
pub fn load_all(spec_root: &Path) -> Result<Vec<SpecRoute>, CoverageError> {
    let mut routes = Vec::new();
    for family in ApiFamily::ALL {
        routes.extend(load_family(spec_root, family)?);
    }
    Ok(routes)
}

fn load_file(path: &Path, family: ApiFamily) -> Result<Vec<SpecRoute>, CoverageError> {
    let text = fs::read_to_string(path).map_err(|source| CoverageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let doc: Yaml = serde_yaml_ng::from_str(&text).map_err(|source| CoverageError::Yaml {
        path: path.to_path_buf(),
        source,
    })?;

    // Only files that declare both `openapi` and `paths` are operation documents; everything
    // else under a family directory (there should be none at the top level, but be defensive) is
    // skipped rather than treated as an error, since this module's contract is "enumerate the
    // routes the spec declares," not "validate the spec's own file layout."
    if doc.get("openapi").is_none() {
        return Ok(Vec::new());
    }
    let Some(paths) = doc.get("paths").and_then(Yaml::as_mapping) else {
        return Ok(Vec::new());
    };

    let base_path = base_path_of(&doc).unwrap_or_else(|| family.default_base_path().to_string());
    let source_file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut routes = Vec::new();
    for (path_key, path_item) in paths {
        let Some(path_template) = path_key.as_str() else {
            continue;
        };
        let Some(operations) = path_item.as_mapping() else {
            continue;
        };
        let full_path = format!("{base_path}{path_template}");
        for (method_key, operation) in operations {
            let Some(method_name) = method_key.as_str() else {
                continue;
            };
            if !HTTP_METHODS.contains(&method_name) {
                // `parameters`, `summary`, `description`, `servers`, ... at the path-item level.
                continue;
            }
            let operation_id = operation
                .as_mapping()
                .and_then(|m| m.get("operationId"))
                .and_then(Yaml::as_str)
                .map(String::from);
            routes.push(SpecRoute {
                method: method_name.to_ascii_uppercase(),
                path: full_path.clone(),
                family,
                operation_id,
                source_file: source_file.clone(),
            });
        }
    }
    Ok(routes)
}

/// Reads `servers[0].variables.basePath.default` from a parsed OpenAPI document.
fn base_path_of(doc: &Yaml) -> Option<String> {
    doc.get("servers")?
        .as_sequence()?
        .first()?
        .get("variables")?
        .get("basePath")?
        .get("default")?
        .as_str()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, contents: &str) {
        let mut f = fs::File::create(dir.join(name)).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn parses_a_minimal_operation_file() {
        let tmp = tempfile::tempdir().unwrap();
        let family_dir = tmp.path().join("client-server");
        fs::create_dir_all(&family_dir).unwrap();
        write(
            &family_dir,
            "login.yaml",
            r#"
openapi: 3.1.0
info: {title: t, version: "1.0.0"}
paths:
  /login:
    get:
      operationId: getLoginFlows
      responses: {"200": {description: ok}}
    post:
      operationId: login
      responses: {"200": {description: ok}}
servers:
  - url: "{protocol}://{hostname}{basePath}"
    variables:
      protocol: {default: https}
      hostname: {default: localhost:8008}
      basePath: {default: /_matrix/client/v3}
"#,
        );

        let routes = load_family(tmp.path(), ApiFamily::ClientServer).unwrap();
        assert_eq!(routes.len(), 2);
        let mut paths: Vec<_> = routes
            .iter()
            .map(|r| (r.method.as_str(), r.path.as_str()))
            .collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![
                ("GET", "/_matrix/client/v3/login"),
                ("POST", "/_matrix/client/v3/login")
            ]
        );
        assert_eq!(routes[0].source_file, "login.yaml");
    }

    #[test]
    fn skips_files_without_a_paths_key() {
        let tmp = tempfile::tempdir().unwrap();
        let family_dir = tmp.path().join("client-server");
        let definitions_dir = family_dir.join("definitions");
        fs::create_dir_all(&definitions_dir).unwrap();
        // Only top-level files are read; a definitions/ file would be skipped anyway even if
        // this test placed one directly at the top level with no `paths`.
        write(
            &family_dir,
            "not_an_operation.yaml",
            "openapi: 3.1.0\ninfo: {title: t, version: \"1.0.0\"}\n",
        );
        write(&definitions_dir, "ignored.yaml", "type: object\n");

        let routes = load_family(tmp.path(), ApiFamily::ClientServer).unwrap();
        assert!(routes.is_empty());
    }

    #[test]
    fn missing_family_dir_is_an_error_pointing_at_fetch_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let err = load_family(tmp.path(), ApiFamily::ClientServer).unwrap_err();
        assert!(matches!(err, CoverageError::MissingSpecRoot(_)));
    }

    #[test]
    fn falls_back_to_the_family_default_base_path_when_undeclared() {
        let tmp = tempfile::tempdir().unwrap();
        let family_dir = tmp.path().join("push-gateway");
        fs::create_dir_all(&family_dir).unwrap();
        write(
            &family_dir,
            "notify.yaml",
            "openapi: 3.1.0\ninfo: {title: t, version: \"1.0.0\"}\npaths:\n  /notify:\n    post:\n      responses: {\"200\": {description: ok}}\n",
        );
        let routes = load_family(tmp.path(), ApiFamily::PushGateway).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/_matrix/push/v1/notify");
    }
}
