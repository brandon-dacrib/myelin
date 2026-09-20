//! Booting through the configuration layers, and the data directory that makes a config file
//! optional.
//!
//! `hs serve` used to read one YAML file and run on whatever it said. It now resolves the three
//! layers `hs_config::layered` defines -- the bootstrap file, the database, and `HS__`
//! environment variables, in that precedence order -- so that a setting changed in the web
//! interface is what the server runs on after a restart, rather than being silently undone by a
//! `homeserver.yaml` nobody remembers is mounted.
//!
//! # The order of operations, and why it is not a straight line
//!
//! The database holds the configuration, and the configuration says where the database is. That
//! circle is broken by [`hs_config::store::BOOTSTRAP_SECTIONS`]: `storage` is read before the
//! database opens and can never be stored in it. So:
//!
//! 1. Read the bootstrap file (if any) and the `HS__` environment into a two-layer document.
//! 2. Deserialize *only* `storage` out of it and open that backend. A half-configured server has
//!    no `server.server_name` yet -- it may be sitting in the database we have not opened -- so
//!    this step deliberately does not validate the whole configuration.
//! 3. Open [`hs_config::ConfigStore`] on that backend and, if nothing has ever been written to
//!    it, seed it from the bootstrap file. Re-running is a no-op, which is what makes it safe to
//!    leave the file mounted forever.
//! 4. Put the database in as the middle layer and resolve for real.
//!
//! # `--data-dir`: one directory instead of four hand-edits
//!
//! Three settings are filesystem paths that default relative to the process's working directory
//! (`./data`, `./signing-keys`, `./media-store`). In a container the working directory is not
//! writable and not mounted, which is why the published image needed three of its four mandatory
//! hand-edits: an operator had to repoint each path at the volume by hand before the server could
//! write anything.
//!
//! `--data-dir <root>` (or `HS_DATA_DIR`) replaces all three with one answer: the database goes
//! in `<root>/db`, signing keys in `<root>/keys`, media in `<root>/media`. These are contributed
//! as *defaults*, in the file layer, and only for paths no other layer sets -- so a config file,
//! an `HS__` variable or a value stored in the database still wins, and an operator who has
//! already pointed media at S3 does not get a local directory merged into it.
//!
//! It is a flag with an environment twin rather than an `HS__` variable because an `HS__`
//! variable sets exactly one leaf of the schema and this sets three. `HS__STORAGE__DATA_DIR`
//! still exists and still means precisely what it says -- the embedded database's own directory,
//! nothing else -- and, being in the environment layer, it wins over the `<root>/db` this derives.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use hs_config::document::merge_patch;
use hs_config::store::{ChangeRecord, ConfigMeta, StoreError, Stored};
use hs_config::{Config, ConfigError, ConfigStore, FileLayer, Layers, Resolved};

use crate::storage::{OpenedStorage, StorageOpenError, open_storage};

/// What the file layer's path is called when there is no file: the layer still exists, because
/// it is where `--data-dir`'s derived paths are contributed, but no file backs it.
pub const NO_FILE: &str = "<command line>";

/// The environment twin of `--data-dir`. Deliberately a single-underscore name: it is not an
/// `HS__SECTION__FIELD` override (see [`hs_config::env`]), because it does not set one field.
pub const DATA_DIR_ENV: &str = "HS_DATA_DIR";

/// Where the configuration a boot started from came from.
#[derive(Debug)]
pub enum ConfigSource {
    /// No file at all: `hs serve --data-dir ... --server-name ...`, everything else defaulted or
    /// already in the database.
    None,
    /// A native `hs-config` YAML file (`-c`).
    Native(PathBuf),
    /// A Synapse `homeserver.yaml`, already translated by [`crate::synapse_serve`]. The
    /// translated configuration seeds the database exactly as a native file's would; only the
    /// reading of it differs.
    Translated {
        /// The Synapse file it was translated from, for provenance.
        path: PathBuf,
        /// The translation's result.
        config: Box<Config>,
    },
}

impl ConfigSource {
    /// The label recorded as [`ConfigMeta::seeded_from`], so an operator can later ask the web
    /// interface where the stored configuration originally came from.
    fn label(&self) -> String {
        match self {
            ConfigSource::None => "command line".to_owned(),
            ConfigSource::Native(path) => path.display().to_string(),
            ConfigSource::Translated { path, .. } => format!("{} (Synapse)", path.display()),
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::None => None,
            ConfigSource::Native(path) | ConfigSource::Translated { path, .. } => Some(path),
        }
    }
}

/// What a caller (a subcommand's arguments) hands [`boot`].
#[derive(Debug)]
pub struct BootOptions {
    /// Where the bootstrap configuration comes from.
    pub source: ConfigSource,
    /// `--data-dir`: the one directory everything this server writes lives under. Falls back to
    /// [`DATA_DIR_ENV`].
    pub data_dir: Option<PathBuf>,
    /// `--server-name`: declared once, on the first run, and remembered in the database from then
    /// on. Ignored (with a note) if the database already knows a different one.
    pub server_name: Option<String>,
}

/// Errors booting through the layers.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// The bootstrap file could not be read.
    #[error("failed to read {path:?}: {source}")]
    ReadFile {
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The bootstrap file is not valid YAML, or is a YAML document this schema cannot be
    /// expressed in (a mapping keyed by something other than a string).
    #[error("failed to parse {path:?}: {source}")]
    ParseFile {
        /// The path that failed.
        path: PathBuf,
        /// The underlying parse error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The `storage` section could not be understood well enough to open a database.
    #[error("the storage configuration is unusable: {0}")]
    Storage(#[source] Box<ConfigError>),
    /// The storage backend could not be opened.
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    /// The configuration store could not be opened, read or seeded.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The merged configuration does not deserialize, resolve or validate.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Neither a config file, a `--data-dir`, nor `HS_DATA_DIR` was given, so there is nothing to
    /// say where this server's database lives.
    #[error(
        "nowhere to put this server's data: pass --data-dir <dir> (or set {DATA_DIR_ENV}) for a \
         fresh install, or -c <file> for an existing one"
    )]
    NoDataDir,
}

/// A booted server's configuration: the layers it was assembled from, the storage it was opened
/// on, and the store the web interface writes through.
pub struct Booted {
    /// The three layers, for a caller that needs to validate a proposed change against them
    /// ([`Layers::resolve_with_patch`]) or ask what the environment pins.
    pub layers: Layers,
    /// The opened storage backend, handed to [`crate::serve::spawn_serve_with_storage`] rather
    /// than reopened -- the embedded backend holds an exclusive lock on its directory, so opening
    /// it twice in one process fails.
    pub storage: OpenedStorage,
    /// The configuration store on that backend.
    pub store: OpenedConfigStore,
    /// The stored configuration's metadata: revision, last writer, seed provenance.
    pub meta: ConfigMeta,
    /// `Some(label)` when *this* boot seeded the store, i.e. this was the first run.
    pub seeded: Option<String>,
    /// Things worth telling the operator once telemetry is up: a `--server-name` the database
    /// disagrees with, a file whose settings the database now overrides.
    pub notes: Vec<String>,
}

impl Booted {
    /// Merges, deserializes and validates the layers.
    ///
    /// Separate from [`boot`] because `hs config set` must still work on a server whose
    /// configuration does not yet resolve -- that is frequently the reason somebody is running
    /// `hs config set` in the first place.
    ///
    /// # Errors
    /// As [`Layers::resolve`].
    pub fn resolve(&self) -> Result<Resolved, ConfigError> {
        self.layers.resolve()
    }

    /// Re-reads the database layer from the store.
    ///
    /// [`Layers::database`] is a snapshot taken when the store was opened, so anything that
    /// writes to the store has to say so here or the next `resolve` will answer with the
    /// configuration from before the write -- which reads as the write having been lost.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the store could not be read.
    pub fn refresh(&mut self) -> Result<(), StoreError> {
        let stored = self.store.load()?;
        self.layers.database = stored.document;
        self.meta = stored.meta;
        Ok(())
    }
}

/// A [`ConfigStore`] over whichever backend the `storage` section selected.
///
/// `ConfigStore` is generic over its backend and `hs config` is not: this enum is the one place
/// that difference is absorbed, so the subcommands are written once rather than once per backend.
pub enum OpenedConfigStore {
    /// The embedded Fjall backend.
    Embedded(ConfigStore<hs_kv::fjall_backend::FjallBackend>),
    /// The PostgreSQL backend.
    Postgres(ConfigStore<hs_kv::postgres_backend::PostgresBackend>),
}

impl std::fmt::Debug for OpenedConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenedConfigStore::Embedded(_) => f.write_str("OpenedConfigStore::Embedded(..)"),
            OpenedConfigStore::Postgres(_) => f.write_str("OpenedConfigStore::Postgres(..)"),
        }
    }
}

/// Dispatches one method over both backends. Written as a macro rather than a trait object
/// because `ConfigStore`'s methods are not object safe (they are generic in the backend, not in
/// themselves) and four delegations by hand are four places to make the same typo.
macro_rules! on_store {
    ($self:expr, $store:ident => $body:expr) => {
        match $self {
            OpenedConfigStore::Embedded($store) => $body,
            OpenedConfigStore::Postgres($store) => $body,
        }
    };
}

impl OpenedConfigStore {
    /// Opens the configuration keyspace on an already-opened storage backend.
    ///
    /// # Errors
    /// Returns [`StoreError::Kv`] if the keyspace could not be opened.
    pub fn open(storage: &OpenedStorage) -> Result<Self, StoreError> {
        Ok(match storage {
            OpenedStorage::Embedded(backend) => {
                OpenedConfigStore::Embedded(ConfigStore::open(backend.clone())?)
            }
            OpenedStorage::Postgres(backend) => {
                OpenedConfigStore::Postgres(ConfigStore::open(backend.clone())?)
            }
        })
    }

    /// See [`ConfigStore::load`].
    ///
    /// # Errors
    /// As [`ConfigStore::load`].
    pub fn load(&self) -> Result<Stored, StoreError> {
        on_store!(self, store => store.load())
    }

    /// See [`ConfigStore::seed`].
    ///
    /// # Errors
    /// As [`ConfigStore::seed`].
    pub fn seed(&self, document: &Value, source: &str, now_ms: i64) -> Result<bool, StoreError> {
        on_store!(self, store => store.seed(document, source, now_ms))
    }

    /// See [`ConfigStore::patch_section`].
    ///
    /// # Errors
    /// As [`ConfigStore::patch_section`].
    pub fn patch_section(
        &self,
        section: &str,
        patch: &Value,
        actor: Option<&str>,
        now_ms: i64,
    ) -> Result<Stored, StoreError> {
        on_store!(self, store => store.patch_section(section, patch, actor, now_ms, None))
    }

    /// See [`ConfigStore::history`].
    ///
    /// # Errors
    /// As [`ConfigStore::history`].
    pub fn history(&self, limit: usize) -> Result<Vec<ChangeRecord>, StoreError> {
        on_store!(self, store => store.history(limit))
    }
}

/// Where a `--data-dir` root puts each of the three things this server writes to disk.
///
/// The database gets a subdirectory of its own rather than the root itself: the embedded backend
/// treats its directory as private, and putting the signing keys and the media store *inside* it
/// would leave the operator's own files interleaved with the database's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataLayout {
    /// `<root>/db`
    pub database: PathBuf,
    /// `<root>/keys`
    pub signing_keys: PathBuf,
    /// `<root>/media`
    pub media: PathBuf,
}

impl DataLayout {
    /// The layout under `root`.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            database: root.join("db"),
            signing_keys: root.join("keys"),
            media: root.join("media"),
        }
    }
}

/// The data-directory root in effect: `--data-dir`, else [`DATA_DIR_ENV`], else none.
fn data_root(explicit: Option<&Path>) -> Option<PathBuf> {
    explicit.map(Path::to_path_buf).or_else(|| {
        std::env::var_os(DATA_DIR_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
}

/// Reads a bootstrap YAML file as the sparse document it is -- keys the operator actually wrote,
/// not a full configuration with every default filled in. An empty file is an empty document
/// rather than a parse error, since "a file that sets nothing" is a legitimate thing to mount.
fn read_file_document(path: &Path) -> Result<Value, BootError> {
    let raw = std::fs::read_to_string(path).map_err(|source| BootError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&raw).map_err(|source| BootError::ParseFile {
            path: path.to_owned(),
            source: Box::new(source),
        })?;
    if yaml.is_null() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::to_value(&yaml).map_err(|source| BootError::ParseFile {
        path: path.to_owned(),
        source: Box::new(source),
    })
}

/// A translated Synapse configuration as a *sparse* document: only the top-level sections that
/// differ from the schema's own defaults.
///
/// Whole sections rather than individual leaves, because `storage` and `media.storage` are
/// internally tagged enums -- a leaf-level diff could store `path` without the `backend` that
/// says which variant it belongs to, and the result would no longer deserialize. Section
/// granularity is coarse but can never produce a document that does not parse.
fn sparse_sections(config: &Config) -> Value {
    let full = serde_json::to_value(config).unwrap_or_else(|_| Value::Object(Map::new()));
    let defaults =
        serde_json::to_value(Config::default()).unwrap_or_else(|_| Value::Object(Map::new()));
    let mut out = Map::new();
    if let Some(sections) = full.as_object() {
        for (name, value) in sections {
            if defaults.get(name) != Some(value) {
                out.insert(name.clone(), value.clone());
            }
        }
    }
    Value::Object(out)
}

/// The document the bootstrap file layer starts from: what the operator wrote (or what their
/// Synapse config translated to), plus the settings they declared on the command line.
///
/// This is also exactly what seeds the database on a first run. The derived `--data-dir` paths
/// are deliberately *not* part of it: they are recomputed from the flag on every boot, so moving
/// the data directory moves the whole server rather than leaving machine-specific paths behind in
/// a database that may later be restored somewhere else.
fn declared_document(options: &BootOptions) -> Result<Value, BootError> {
    let mut document = match &options.source {
        ConfigSource::None => Value::Object(Map::new()),
        ConfigSource::Native(path) => read_file_document(path)?,
        ConfigSource::Translated { config, .. } => sparse_sections(config),
    };
    if let Some(server_name) = &options.server_name {
        merge_patch(
            &mut document,
            &serde_json::json!({"server": {"server_name": server_name}}),
        );
    }
    Ok(document)
}

/// The `--data-dir` defaults that do not collide with anything `context` already sets.
///
/// `context` is every other layer's merged view. A path is contributed only when nothing there
/// mentions it, which is what keeps this from corrupting a tagged enum: merging
/// `{backend: local, path: ...}` underneath a configured `backend: s3` would produce an `s3`
/// variant carrying a stray `path` key, and `deny_unknown_fields` would reject the whole
/// configuration at startup.
fn derived_document(layout: &DataLayout, context: &Value, include_storage: bool) -> Value {
    let mut out = Value::Object(Map::new());
    if include_storage && context.pointer("/storage").is_none() {
        merge_patch(
            &mut out,
            &serde_json::json!({"storage": {"backend": "embedded", "data_dir": layout.database}}),
        );
    }
    if context.pointer("/server/signing_key_path").is_none() {
        merge_patch(
            &mut out,
            &serde_json::json!({"server": {"signing_key_path": layout.signing_keys}}),
        );
    }
    if context.pointer("/media/storage").is_none() {
        merge_patch(
            &mut out,
            &serde_json::json!({"media": {"storage": {"backend": "local", "path": layout.media}}}),
        );
    }
    out
}

/// Names the storage backend when a layer configured the section without saying which one it is.
///
/// `storage` is an internally tagged enum, so `{data_dir: /srv/db}` -- exactly what
/// `HS__STORAGE__DATA_DIR` on its own produces, and what an operator writes when they only want
/// to move the database -- fails to deserialize with "missing field `backend`", a message that
/// does not point at anything they can act on. The default backend is embedded, so say so on
/// their behalf, in the *file* layer: it is the lowest one, so anything that does name a backend
/// still wins, and the tag is present for every later merge rather than only for the probe below.
///
/// Returns whether it changed anything.
fn ensure_storage_backend(file_document: &mut Value, merged: &Value) -> bool {
    let Some(storage) = merged.get("storage") else {
        return false;
    };
    if !storage.is_object() || storage.get("backend").is_some() {
        return false;
    }
    merge_patch(
        file_document,
        &serde_json::json!({"storage": {"backend": "embedded"}}),
    );
    true
}

/// Deserializes just the `storage` section out of a merged bootstrap document.
///
/// The whole configuration cannot be validated here: on every boot after the first, settings as
/// basic as `server.server_name` live in the database this section is about to open. So a
/// throwaway configuration is built from the storage section plus the one placeholder that makes
/// the rest of the schema valid, and only its `storage` is kept. Anything else wrong with the
/// document is caught by the real resolve in step 4, with the database in place and a better
/// error message.
fn bootstrap_storage(merged: &Value) -> Result<hs_config::StorageConfig, BootError> {
    let Some(section) = merged.get("storage") else {
        return Ok(hs_config::StorageConfig::default());
    };
    let probe = serde_json::json!({
        "server": {"server_name": "bootstrap.invalid"},
        "storage": section.clone(),
    });
    Config::from_json(&probe)
        .map(|config| config.storage)
        .map_err(|e| BootError::Storage(Box::new(e)))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Resolves the bootstrap layers, opens storage, seeds the configuration store on a first run,
/// and returns the layers with the database in place.
///
/// # Errors
/// See [`BootError`].
pub fn boot(options: &BootOptions) -> Result<Booted, BootError> {
    boot_with_env(options, std::env::vars().collect())
}

/// [`boot`]'s body with the environment passed in, so tests can exercise the layering without
/// mutating the process environment out from under every other test in the binary.
///
/// # Errors
/// See [`BootError`].
pub fn boot_with_env(
    options: &BootOptions,
    env_vars: Vec<(String, String)>,
) -> Result<Booted, BootError> {
    let mut notes = Vec::new();
    let declared = declared_document(options)?;
    let environment = hs_config::env::override_document(env_vars.clone());
    let layout = data_root(options.data_dir.as_deref()).map(|root| DataLayout::new(&root));

    // Step 1-2: file + environment, enough to find the database.
    let mut file_document = declared.clone();
    if let Some(layout) = &layout {
        let context = hs_config::document::merge_all([&declared, &environment]);
        let derived = derived_document(layout, &context, true);
        // Derived values sit *underneath* what the operator declared: merging the other way round
        // would let a convenience default overwrite a path they wrote out by hand.
        let mut base = derived;
        merge_patch(&mut base, &declared);
        file_document = base;
    }
    let file_path = options
        .source
        .path()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(NO_FILE));
    let mut bootstrap_layers = Layers::bootstrap(
        Some(FileLayer {
            path: file_path.clone(),
            document: file_document.clone(),
        }),
        env_vars.clone(),
    );
    let mut merged = bootstrap_layers.merged();
    if ensure_storage_backend(&mut file_document, &merged) {
        bootstrap_layers = Layers::bootstrap(
            Some(FileLayer {
                path: file_path.clone(),
                document: file_document.clone(),
            }),
            env_vars.clone(),
        );
        merged = bootstrap_layers.merged();
    }
    let storage_config = bootstrap_storage(&merged)?;
    if layout.is_none() && options.source.path().is_none() && merged.pointer("/storage").is_none() {
        return Err(BootError::NoDataDir);
    }

    // Step 3: open the store, and seed it if this is the very first run.
    let storage = open_storage(&storage_config)?;
    let store = OpenedConfigStore::open(&storage)?;
    let mut stored = store.load()?;
    let mut seeded = None;
    let seedable = declared
        .as_object()
        .is_some_and(|sections| sections.keys().any(|name| name != "storage"));
    if stored.meta.revision == 0 && seedable {
        // Validate before writing: a bootstrap file that does not resolve should not be copied
        // into the database, where it would outrank the corrected file on the next boot.
        bootstrap_layers.resolve()?;
        let label = options.source.label();
        if store.seed(&declared, &label, now_ms())? {
            seeded = Some(label);
            stored = store.load()?;
        }
    }

    // Step 4: the database goes in as the middle layer, and the `--data-dir` defaults are
    // recomputed now that we can see what it holds.
    if let Some(layout) = &layout {
        let context = hs_config::document::merge_all([&declared, &stored.document, &environment]);
        let mut base = derived_document(layout, &context, false);
        // `storage` was settled in step 2 and cannot be revisited: the database is already open.
        if let Some(storage) = file_document.get("storage") {
            merge_patch(&mut base, &serde_json::json!({"storage": storage.clone()}));
        }
        merge_patch(&mut base, &declared);
        file_document = base;
    }
    let mut layers = Layers {
        file: Some(FileLayer {
            path: file_path,
            document: file_document.clone(),
        }),
        database: stored.document.clone(),
        environment,
    };
    if ensure_storage_backend(&mut file_document, &layers.merged())
        && let Some(file) = layers.file.as_mut()
    {
        file.document = file_document;
    }

    if let Some(asked) = &options.server_name {
        let effective = layers
            .merged()
            .pointer("/server/server_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(effective) = effective
            && &effective != asked
        {
            notes.push(format!(
                "--server-name {asked} was ignored: this server is already {effective}, which is \
                 recorded in its database and in every event it has ever signed"
            ));
        }
    }

    Ok(Booted {
        layers,
        storage,
        store,
        meta: stored.meta,
        seeded,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(dir: &Path, server_name: Option<&str>) -> BootOptions {
        BootOptions {
            source: ConfigSource::None,
            data_dir: Some(dir.to_owned()),
            server_name: server_name.map(str::to_owned),
        }
    }

    /// The whole point of item 2: a working server with no YAML anywhere, and every path it
    /// writes to underneath the one directory the operator named.
    #[test]
    fn a_data_dir_and_a_server_name_are_a_whole_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let booted = boot_with_env(&options(dir.path(), Some("example.org")), Vec::new()).unwrap();
        let resolved = booted.resolve().unwrap();
        assert_eq!(resolved.config.server.server_name, "example.org");
        assert_eq!(
            resolved.config.server.signing_key_path,
            dir.path().join("keys")
        );
        match &resolved.config.storage {
            hs_config::StorageConfig::Embedded(e) => {
                assert_eq!(e.data_dir, dir.path().join("db"));
            }
            other => panic!("expected the embedded backend, got {other:?}"),
        }
        match &resolved.config.media.storage {
            hs_config::media::MediaStorageBackend::Local { path } => {
                assert_eq!(path, &dir.path().join("media"));
            }
            other => panic!("expected local media storage, got {other:?}"),
        }
        assert_eq!(booted.seeded.as_deref(), Some("command line"));
    }

    /// The second boot needs only the data directory: the server name was seeded into the
    /// database the first time and the database outranks the (now absent) command line.
    #[test]
    fn the_second_boot_remembers_the_server_name() {
        let dir = tempfile::tempdir().unwrap();
        {
            let booted =
                boot_with_env(&options(dir.path(), Some("example.org")), Vec::new()).unwrap();
            drop(booted);
        }
        let booted = boot_with_env(&options(dir.path(), None), Vec::new()).unwrap();
        assert!(booted.seeded.is_none(), "the store was already seeded");
        assert_eq!(
            booted.resolve().unwrap().config.server.server_name,
            "example.org"
        );
    }

    /// A `--server-name` that disagrees with the one this server has been running as is reported
    /// rather than applied: it is embedded in every user ID and event already signed.
    #[test]
    fn a_contradictory_server_name_is_reported_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        drop(boot_with_env(&options(dir.path(), Some("example.org")), Vec::new()).unwrap());
        let booted =
            boot_with_env(&options(dir.path(), Some("other.example")), Vec::new()).unwrap();
        assert_eq!(
            booted.resolve().unwrap().config.server.server_name,
            "example.org"
        );
        assert!(
            booted.notes.iter().any(|n| n.contains("other.example")),
            "expected a note about the ignored --server-name, got {:?}",
            booted.notes
        );
    }

    /// The database beats the file it was seeded from -- checked here through the real boot path
    /// rather than only in `hs-config`'s unit test, because the order the CLI assembles the
    /// layers in is where it would actually go wrong.
    #[test]
    fn a_stored_setting_outranks_the_file_that_seeded_it() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("homeserver.yaml");
        std::fs::write(
            &config_path,
            format!(
                "server:\n  server_name: example.org\nauth:\n  enable_registration: false\nstorage:\n  backend: embedded\n  data_dir: {:?}\n",
                dir.path().join("db")
            ),
        )
        .unwrap();
        let opts = BootOptions {
            source: ConfigSource::Native(config_path.clone()),
            data_dir: None,
            server_name: None,
        };
        let booted = boot_with_env(&opts, Vec::new()).unwrap();
        assert!(!booted.resolve().unwrap().config.auth.enable_registration);
        booted
            .store
            .patch_section(
                "auth",
                &serde_json::json!({"enable_registration": true}),
                Some("test"),
                now_ms(),
            )
            .unwrap();
        drop(booted);

        let booted = boot_with_env(&opts, Vec::new()).unwrap();
        let resolved = booted.resolve().unwrap();
        assert!(
            resolved.config.auth.enable_registration,
            "the file still says false; the database must win"
        );
        assert_eq!(
            resolved.origin("/auth/enable_registration"),
            hs_config::Origin::Database
        );
    }

    /// `HS__STORAGE__DATA_DIR` sets one leaf and says nothing about `backend`. Without the
    /// default this injects, the internally tagged enum fails with "missing field `backend`"
    /// before the server ever reaches a useful error message.
    #[test]
    fn a_storage_section_without_a_backend_is_taken_as_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let opts = BootOptions {
            source: ConfigSource::None,
            data_dir: None,
            server_name: Some("example.org".to_owned()),
        };
        let env = vec![
            (
                "HS__STORAGE__DATA_DIR".to_owned(),
                dir.path().join("db").display().to_string(),
            ),
            (
                "HS__SERVER__SERVER_NAME".to_owned(),
                "example.org".to_owned(),
            ),
        ];
        let booted = boot_with_env(&opts, env).unwrap();
        match &booted.resolve().unwrap().config.storage {
            hs_config::StorageConfig::Embedded(e) => {
                assert_eq!(e.data_dir, dir.path().join("db"));
            }
            other => panic!("expected the embedded backend, got {other:?}"),
        }
    }

    /// An `HS__` variable outranks the data directory's derived default, and the derived default
    /// is not merged into a media backend the operator configured as something else -- merging
    /// `path` into an `s3` variant would fail `deny_unknown_fields` at startup.
    #[test]
    fn derived_paths_yield_to_every_other_layer() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("homeserver.yaml");
        std::fs::write(
            &config_path,
            "server:\n  server_name: example.org\nmedia:\n  storage:\n    backend: s3\n    bucket: media\n",
        )
        .unwrap();
        let opts = BootOptions {
            source: ConfigSource::Native(config_path),
            data_dir: Some(dir.path().to_owned()),
            server_name: None,
        };
        let env = vec![(
            "HS__SERVER__SIGNING_KEY_PATH".to_owned(),
            dir.path().join("pinned-keys").display().to_string(),
        )];
        let booted = boot_with_env(&opts, env).unwrap();
        let resolved = booted.resolve().unwrap();
        assert_eq!(
            resolved.config.server.signing_key_path,
            dir.path().join("pinned-keys")
        );
        assert!(matches!(
            resolved.config.media.storage,
            hs_config::media::MediaStorageBackend::S3 { .. }
        ));
    }

    /// A bootstrap file that does not validate must not be copied into the database, where it
    /// would outrank the corrected file on the next boot and be very hard to explain.
    #[test]
    fn an_invalid_bootstrap_file_is_not_seeded() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("homeserver.yaml");
        std::fs::write(
            &config_path,
            format!(
                "server:\n  server_name: \"\"\nstorage:\n  backend: embedded\n  data_dir: {:?}\n",
                dir.path().join("db")
            ),
        )
        .unwrap();
        let opts = BootOptions {
            source: ConfigSource::Native(config_path),
            data_dir: None,
            server_name: None,
        };
        assert!(matches!(
            boot_with_env(&opts, Vec::new()),
            Err(BootError::Config(_))
        ));

        // And nothing was written: a second attempt still sees a virgin store.
        let opts = BootOptions {
            source: ConfigSource::None,
            data_dir: Some(dir.path().to_owned()),
            server_name: Some("example.org".to_owned()),
        };
        let booted = boot_with_env(&opts, Vec::new()).unwrap();
        assert!(booted.seeded.is_some());
    }

    #[test]
    fn with_no_file_and_no_data_dir_there_is_nowhere_to_put_anything() {
        let opts = BootOptions {
            source: ConfigSource::None,
            data_dir: None,
            server_name: Some("example.org".to_owned()),
        };
        assert!(matches!(
            boot_with_env(&opts, Vec::new()),
            Err(BootError::NoDataDir)
        ));
    }

    #[test]
    fn a_translated_synapse_config_seeds_only_what_it_changed() {
        let mut config = Config::default();
        config.server.server_name = "example.org".to_owned();
        let document = sparse_sections(&config);
        assert_eq!(
            document.pointer("/server/server_name"),
            Some(&Value::String("example.org".to_owned()))
        );
        assert!(
            document.get("federation").is_none(),
            "a section left entirely at its defaults is not worth storing"
        );
    }

    #[test]
    fn an_empty_bootstrap_file_is_an_empty_document_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.yaml");
        std::fs::write(&path, "# nothing but a comment\n").unwrap();
        assert_eq!(
            read_file_document(&path).unwrap(),
            Value::Object(Map::new())
        );
    }

    #[test]
    fn the_data_layout_keeps_the_database_in_its_own_subdirectory() {
        let layout = DataLayout::new(Path::new("/srv/myelin"));
        assert_eq!(layout.database, PathBuf::from("/srv/myelin/db"));
        assert_eq!(layout.signing_keys, PathBuf::from("/srv/myelin/keys"));
        assert_eq!(layout.media, PathBuf::from("/srv/myelin/media"));
    }
}
