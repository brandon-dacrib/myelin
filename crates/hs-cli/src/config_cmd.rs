//! `hs config`: reading and changing the stored configuration without a browser.
//!
//! The web interface is the intended way to administer this server, but an operator locked out of
//! it -- a listener bound to the wrong address, an admin account nobody can log into, a container
//! that will not finish starting -- needs a way in that does not depend on the server running.
//! These subcommands are that way in: they open the same database `hs serve` would, through the
//! same layers ([`crate::bootstrap`]), and write through the same [`hs_config::ConfigStore`] the
//! admin API writes through, so a change made here and a change made in the UI are the same kind
//! of change and appear in the same history.
//!
//! # One writer at a time
//!
//! The embedded backend holds an exclusive lock on its directory, so these commands work on a
//! *stopped* server. Against a running one they fail to open the database, and the answer is the
//! web interface or the admin API -- which is the right answer anyway, since a change made there
//! can take effect without a restart.
//!
//! # Secrets
//!
//! [`show`](run) and `get` redact every value the configuration schema marks `x-secret`
//! ([`hs_config::SecretString`]), found by walking the schema rather than by a hardcoded list, so
//! a secret added to `hs-config` later is redacted here without anybody remembering to come back.
//! `export` does not redact: it is a backup that `import` must be able to restore.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Map, Value};

use hs_config::document::{self, Origin};
use hs_config::{Config, Resolved};

use crate::bootstrap::Booted;

/// What replaces a secret's value in human-facing output.
const REDACTED: &str = "<redacted>";

/// Errors running an `hs config` subcommand.
#[derive(Debug, thiserror::Error)]
pub enum ConfigCmdError {
    /// The pointer was not a JSON Pointer into a configuration section.
    #[error(
        "{pointer:?} is not a setting: write it as a JSON Pointer naming a section and a field, \
         like /auth/enable_registration"
    )]
    BadPointer {
        /// What the caller passed.
        pointer: String,
    },
    /// `set` was given the literal `null`, which as a merge patch means "remove", not "set".
    #[error("to clear a setting write `hs config unset {pointer}`, which says what it does")]
    NullIsUnset {
        /// The pointer the caller was setting.
        pointer: String,
    },
    /// The setting is pinned by an `HS__` environment variable, so storing it would have no
    /// effect.
    #[error(
        "{pointers:?} {are} set by an HS__ environment variable, which outranks the database: \
         storing a value here would be remembered and then ignored. Change the environment \
         instead, or unset the variable to let the stored value take effect."
    )]
    PinnedByEnvironment {
        /// The pointers the environment pins.
        pointers: Vec<String>,
        /// "is" or "are", so the message reads as English either way.
        are: &'static str,
    },
    /// The resulting configuration would not be valid.
    #[error("that change would make the configuration invalid: {0}")]
    WouldNotValidate(#[source] Box<hs_config::ConfigError>),
    /// The store refused or failed the write.
    #[error(transparent)]
    Store(#[from] hs_config::StoreError),
    /// A file could not be read or written.
    #[error("{path:?}: {source}")]
    Io {
        /// The path that failed.
        path: std::path::PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// An imported file was not valid YAML or JSON, or was not a mapping of sections.
    #[error("{path:?} is not a configuration document: {detail}")]
    NotADocument {
        /// The file.
        path: std::path::PathBuf,
        /// What was wrong with it.
        detail: String,
    },
    /// The configuration does not currently resolve, so there is nothing effective to show.
    #[error(transparent)]
    Unresolvable(#[from] hs_config::ConfigError),
    /// `get` was asked for a setting that does not exist.
    #[error("no such setting: {pointer}")]
    NoSuchSetting {
        /// The pointer asked for.
        pointer: String,
    },
}

/// Which output shape `hs config show` produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowFormat {
    /// An aligned table, for a person.
    Table,
    /// One JSON document, for a script.
    Json,
}

impl std::str::FromStr for ShowFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "table" => Ok(ShowFormat::Table),
            "json" => Ok(ShowFormat::Json),
            other => Err(format!("expected `table` or `json`, got {other:?}")),
        }
    }
}

/// The effective configuration, every setting with the layer that set it, secrets redacted.
///
/// "Effective" means the schema's view, not the layers' view: a setting nothing sets is listed at
/// its default rather than omitted, because an operator asking what this server is running on
/// wants the whole answer, and the [`Origin::Default`] in the origin column is what tells them
/// nobody chose it.
///
/// # Errors
/// Returns [`ConfigCmdError::Unresolvable`] if the layers do not currently make a valid
/// configuration.
pub fn show(booted: &Booted, format: ShowFormat) -> Result<String, ConfigCmdError> {
    let resolved = booted.resolve()?;
    let rows = settings(&resolved);
    match format {
        ShowFormat::Json => {
            let document = serde_json::json!({
                "file": booted.layers.file.as_ref().map(|f| f.path.display().to_string()),
                "revision": booted.meta.revision,
                "seeded_from": booted.meta.seeded_from,
                "updated_by": booted.meta.updated_by,
                "settings": rows.iter().map(|(pointer, value, origin)| serde_json::json!({
                    "pointer": pointer,
                    "value": value,
                    "origin": origin.as_str(),
                })).collect::<Vec<_>>(),
            });
            Ok(serde_json::to_string_pretty(&document)
                .unwrap_or_else(|e| format!("{{\"error\": {e:?}}}")))
        }
        ShowFormat::Table => {
            let width = rows.iter().map(|(p, _, _)| p.len()).max().unwrap_or(8);
            let value_width = rows
                .iter()
                .map(|(_, v, _)| render(v).len())
                .max()
                .unwrap_or(5)
                .min(48);
            let mut out = String::new();
            for (pointer, value, origin) in &rows {
                out.push_str(&format!(
                    "{pointer:width$}  {:value_width$}  {}\n",
                    render(value),
                    origin.as_str()
                ));
            }
            out.push_str(&format!(
                "\n{} settings; database revision {}{}{}\n",
                rows.len(),
                booted.meta.revision,
                booted
                    .meta
                    .seeded_from
                    .as_ref()
                    .map(|s| format!(", seeded from {s}"))
                    .unwrap_or_default(),
                // A server started without a file still has a file *layer* — it is where the
                // `--data-dir` paths are contributed — but calling `<command line>` a bootstrap
                // file would send an operator looking for a file that does not exist.
                booted
                    .layers
                    .file
                    .as_ref()
                    .filter(|f| f.path != std::path::Path::new(crate::bootstrap::NO_FILE))
                    .map(|f| format!(", bootstrap file {}", f.path.display()))
                    .unwrap_or_else(|| ", no bootstrap file".to_owned()),
            ));
            Ok(out)
        }
    }
}

/// One effective setting's value, redacted if it is a secret.
///
/// # Errors
/// Returns [`ConfigCmdError::NoSuchSetting`] if nothing in the configuration is at that pointer.
pub fn get(booted: &Booted, pointer: &str) -> Result<String, ConfigCmdError> {
    let resolved = booted.resolve()?;
    let document = effective_document(&resolved.config);
    let value = document
        .pointer(pointer)
        .ok_or_else(|| ConfigCmdError::NoSuchSetting {
            pointer: pointer.to_owned(),
        })?;
    let secrets = secret_pointers();
    Ok(render(&redact(pointer, value, &secrets)))
}

/// Stores one setting, after checking that the configuration it would produce is valid and that
/// nothing above the database would override it anyway.
///
/// # Errors
/// See [`ConfigCmdError`].
pub fn set(booted: &mut Booted, pointer: &str, raw: &str) -> Result<String, ConfigCmdError> {
    let value = parse_value(raw);
    if value.is_null() {
        return Err(ConfigCmdError::NullIsUnset {
            pointer: pointer.to_owned(),
        });
    }
    write(booted, pointer, value, "set")
}

/// Clears one setting, so it falls back to the bootstrap file or the schema default.
///
/// # Errors
/// See [`ConfigCmdError`].
pub fn unset(booted: &mut Booted, pointer: &str) -> Result<String, ConfigCmdError> {
    write(booted, pointer, Value::Null, "unset")
}

fn write(
    booted: &mut Booted,
    pointer: &str,
    value: Value,
    verb: &str,
) -> Result<String, ConfigCmdError> {
    let (section, patch) = patch_for(pointer, value)?;
    let pinned = booted.layers.pinned_by_environment(&section, &patch);
    if !pinned.is_empty() {
        return Err(ConfigCmdError::PinnedByEnvironment {
            are: if pinned.len() == 1 { "is" } else { "are" },
            pointers: pinned,
        });
    }
    // Validate the configuration the change would produce, not the change itself: a value can be
    // fine on its own and contradict another section, and a section can only become valid once
    // something else is set.
    booted
        .layers
        .resolve_with_patch(&section, &patch)
        .map_err(|e| ConfigCmdError::WouldNotValidate(Box::new(e)))?;
    booted
        .store
        .patch_section(&section, &patch, Some(&actor()), now_ms())?;
    booted.refresh()?;
    Ok(format!(
        "{verb} {pointer}; database revision {}\n",
        booted.meta.revision
    ))
}

/// Replays a document of sections into the database, as `hs config set` would one at a time.
///
/// The whole document is validated as one before anything is written: half an imported
/// configuration is worse than none, and the layers can only tell whether the *result* is valid.
///
/// # Errors
/// See [`ConfigCmdError`].
pub fn import(booted: &mut Booted, path: &Path) -> Result<String, ConfigCmdError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigCmdError::Io {
        path: path.to_owned(),
        source,
    })?;
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&raw).map_err(|e| ConfigCmdError::NotADocument {
            path: path.to_owned(),
            detail: e.to_string(),
        })?;
    let document: Value =
        serde_json::to_value(&yaml).map_err(|e| ConfigCmdError::NotADocument {
            path: path.to_owned(),
            detail: e.to_string(),
        })?;
    let Some(sections) = document.as_object() else {
        return Err(ConfigCmdError::NotADocument {
            path: path.to_owned(),
            detail: "expected a mapping of configuration sections at the top level".to_owned(),
        });
    };

    let mut skipped = Vec::new();
    let mut importable: Vec<(String, Value)> = Vec::new();
    for (name, value) in sections {
        if hs_config::store::is_bootstrap_section(name) {
            skipped.push(name.clone());
        } else {
            importable.push((name.clone(), value.clone()));
        }
    }

    let mut candidate = booted.layers.clone();
    let mut database = candidate.database.clone();
    for (name, value) in &importable {
        document::merge_patch(
            &mut database,
            &Value::Object([(name.clone(), value.clone())].into_iter().collect()),
        );
    }
    candidate.database = database;
    candidate
        .resolve()
        .map_err(|e| ConfigCmdError::WouldNotValidate(Box::new(e)))?;

    let actor = actor();
    for (name, value) in &importable {
        booted
            .store
            .patch_section(name, value, Some(&actor), now_ms())?;
    }
    booted.refresh()?;
    let revision = booted.meta.revision;

    let mut out = format!(
        "imported {} section{} from {}; database revision {revision}\n",
        importable.len(),
        if importable.len() == 1 { "" } else { "s" },
        path.display()
    );
    if !skipped.is_empty() {
        out.push_str(&format!(
            "skipped {skipped:?}: that section says where this server's database is, so it is \
             read before the database opens and cannot be stored in it\n"
        ));
    }
    Ok(out)
}

/// The stored configuration as YAML: exactly what [`import`] consumes, so a server can be
/// rebuilt from it.
///
/// Unredacted, deliberately -- a backup with `<redacted>` where the registration shared secret
/// used to be restores a server that cannot register anybody.
///
/// # Errors
/// Returns [`ConfigCmdError::Store`] if the store could not be read.
pub fn export(booted: &Booted) -> Result<String, ConfigCmdError> {
    let stored = booted.store.load()?;
    let body = serde_yaml_ng::to_string(&stored.document)
        .unwrap_or_else(|e| format!("# failed to render: {e}\n"));
    Ok(format!(
        "# Myelin configuration, exported from the database at revision {}.\n\
         # Restore with `hs config import <this file>`. Contains secrets.\n\
         {body}",
        stored.meta.revision
    ))
}

/// The most recent configuration changes, newest first -- the same history the web interface
/// shows, for an operator who is not in a browser.
///
/// # Errors
/// Returns [`ConfigCmdError::Store`] if the store could not be read.
pub fn history(booted: &Booted, limit: usize) -> Result<String, ConfigCmdError> {
    let records = booted.store.history(limit)?;
    if records.is_empty() {
        return Ok("no configuration changes recorded\n".to_owned());
    }
    let mut out = String::new();
    for record in records {
        out.push_str(&format!(
            "r{} {} {} {}\n",
            record.revision,
            record.at_ms,
            record.actor.as_deref().unwrap_or("-"),
            serde_json::to_string(&serde_json::json!({&record.section: record.patch}))
                .unwrap_or_default()
        ));
    }
    Ok(out)
}

/// Splits a JSON Pointer into the section it addresses and the merge patch that sets that one
/// leaf within it. `/auth/enable_registration` with `true` becomes `("auth", {"enable_registration": true})`.
fn patch_for(pointer: &str, value: Value) -> Result<(String, Value), ConfigCmdError> {
    let bad = || ConfigCmdError::BadPointer {
        pointer: pointer.to_owned(),
    };
    let rest = pointer.strip_prefix('/').ok_or_else(bad)?;
    let mut tokens = rest
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>();
    if tokens.iter().any(String::is_empty) {
        return Err(bad());
    }
    let section = tokens.remove(0);
    let mut patch = value;
    for token in tokens.into_iter().rev() {
        patch = Value::Object([(token, patch)].into_iter().collect());
    }
    Ok((section, patch))
}

/// Parses a value written on a command line. JSON first, so `true`, `8008` and `["a","b"]` arrive
/// as the types the schema expects; anything JSON rejects is a plain string, which is what makes
/// `hs config set /server/public_baseurl https://example.org` work without quoting. A value that
/// really is the string `"true"` can be written with JSON's own quotes.
fn parse_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// Renders a value for a person: strings bare (so `hs config get` is pipe-able), everything else
/// as compact JSON.
fn render(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".to_owned()),
    }
}

/// Every leaf of the effective configuration, with the layer that set it.
fn settings(resolved: &Resolved) -> Vec<(String, Value, Origin)> {
    let document = effective_document(&resolved.config);
    let secrets = secret_pointers();
    document::leaf_pointers(&document)
        .into_iter()
        .map(|pointer| {
            let value = document.pointer(&pointer).cloned().unwrap_or(Value::Null);
            let value = redact(&pointer, &value, &secrets);
            let origin = resolved.origin(&pointer);
            (pointer, value, origin)
        })
        .collect()
}

/// The validated configuration as a JSON document -- every field, including the ones no layer
/// set, which is what makes "show me everything and where it came from" answerable.
fn effective_document(config: &Config) -> Value {
    serde_json::to_value(config).unwrap_or_else(|_| Value::Object(Map::new()))
}

fn redact(pointer: &str, value: &Value, secrets: &BTreeSet<String>) -> Value {
    if value.is_null() || !secrets.contains(&normalize_pointer(pointer)) {
        value.clone()
    } else {
        Value::String(REDACTED.to_owned())
    }
}

/// Drops array indices out of a pointer, so a secret inside a list of identity providers matches
/// the schema path that described it once.
fn normalize_pointer(pointer: &str) -> String {
    pointer
        .split('/')
        .filter(|token| !(!token.is_empty() && token.chars().all(|c| c.is_ascii_digit())))
        .collect::<Vec<_>>()
        .join("/")
}

/// Every configuration path whose value is a secret, read out of the JSON Schema's `x-secret`
/// marker rather than listed here by hand.
///
/// The failure this prevents is the quiet one: somebody adds a secret-bearing field to
/// `hs-config`, nobody thinks about this file, and `hs config show` starts printing it.
fn secret_pointers() -> BTreeSet<String> {
    let schema = serde_json::to_value(schemars::schema_for!(Config))
        .unwrap_or_else(|_| Value::Object(Map::new()));
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = BTreeSet::new();
    collect_secrets(&schema, &defs, &mut String::new(), &mut out, 0);
    out
}

fn collect_secrets(
    schema: &Value,
    defs: &Map<String, Value>,
    prefix: &mut String,
    out: &mut BTreeSet<String>,
    depth: usize,
) {
    // The schema is a tree with `$ref`s back into `$defs`; a recursive type would otherwise spin
    // here forever. Nothing in this configuration nests anywhere near this deep.
    if depth > 16 {
        return;
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && let Some(name) = reference.strip_prefix("#/$defs/")
        && let Some(target) = defs.get(name)
    {
        collect_secrets(target, defs, prefix, out, depth + 1);
        return;
    }
    if schema.get("x-secret").and_then(Value::as_bool) == Some(true) {
        out.insert(prefix.clone());
        return;
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            let mark = prefix.len();
            prefix.push('/');
            prefix.push_str(key);
            collect_secrets(child, defs, prefix, out, depth + 1);
            prefix.truncate(mark);
        }
    }
    // An array's elements and a map's values live at the same configuration path as far as an
    // operator is concerned: `normalize_pointer` strips the index back off before matching.
    for key in ["items", "additionalProperties"] {
        if let Some(child) = schema.get(key)
            && child.is_object()
        {
            collect_secrets(child, defs, prefix, out, depth + 1);
        }
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = schema.get(key).and_then(Value::as_array) {
            for branch in branches {
                collect_secrets(branch, defs, prefix, out, depth + 1);
            }
        }
    }
}

/// Who a change is recorded as having been made by. The store's history is shown in the web
/// interface next to changes made through the admin API, which record a Matrix user ID, so this
/// says plainly that it came from a shell instead of pretending to be a user.
fn actor() -> String {
    match std::env::var("USER") {
        Ok(user) if !user.is_empty() => format!("hs config ({user})"),
        _ => "hs config".to_owned(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The layers a caller assembled, for tests that want to exercise a write without a database.
#[cfg(test)]
fn layers_for_test() -> hs_config::Layers {
    hs_config::Layers {
        file: None,
        database: serde_json::json!({"server": {"server_name": "example.org"}}),
        environment: Value::Object(Map::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_becomes_a_section_and_a_nested_patch() {
        let (section, patch) = patch_for("/auth/enable_registration", Value::Bool(true)).unwrap();
        assert_eq!(section, "auth");
        assert_eq!(patch, serde_json::json!({"enable_registration": true}));
    }

    #[test]
    fn a_deep_pointer_nests_all_the_way_down() {
        let (section, patch) =
            patch_for("/media/storage/path", Value::String("/srv/media".into())).unwrap();
        assert_eq!(section, "media");
        assert_eq!(
            patch,
            serde_json::json!({"storage": {"path": "/srv/media"}})
        );
    }

    #[test]
    fn a_pointer_naming_only_a_section_sets_the_whole_section() {
        let (section, patch) =
            patch_for("/auth", serde_json::json!({"enable_registration": true})).unwrap();
        assert_eq!(section, "auth");
        assert_eq!(patch, serde_json::json!({"enable_registration": true}));
    }

    #[test]
    fn a_pointer_without_a_leading_slash_is_refused() {
        assert!(matches!(
            patch_for("auth/enable_registration", Value::Bool(true)),
            Err(ConfigCmdError::BadPointer { .. })
        ));
        assert!(matches!(
            patch_for("/auth//x", Value::Bool(true)),
            Err(ConfigCmdError::BadPointer { .. })
        ));
    }

    #[test]
    fn values_arrive_as_the_type_the_schema_expects() {
        assert_eq!(parse_value("true"), Value::Bool(true));
        assert_eq!(parse_value("8008"), serde_json::json!(8008));
        assert_eq!(parse_value("[1,2]"), serde_json::json!([1, 2]));
        assert_eq!(
            parse_value("https://example.org"),
            Value::String("https://example.org".into())
        );
        assert_eq!(parse_value("30s"), Value::String("30s".into()));
        assert_eq!(parse_value("\"true\""), Value::String("true".into()));
    }

    /// The list is derived from the schema, so this checks the walk finds the secrets that exist
    /// today rather than restating them as the expected answer.
    #[test]
    fn every_secret_in_the_schema_is_found() {
        let secrets = secret_pointers();
        for expected in [
            "/auth/registration_shared_secret",
            "/storage/password",
            "/telemetry/sentry/dsn",
            "/cluster/mesh/shared_secret",
            "/auth/password/pepper",
        ] {
            assert!(
                secrets.contains(expected),
                "{expected} is a secret in the schema but was not found; found {secrets:?}"
            );
        }
        assert!(
            !secrets.contains("/server/server_name"),
            "a plain setting must not be redacted"
        );
    }

    #[test]
    fn a_secret_is_redacted_and_a_plain_setting_is_not() {
        let secrets = secret_pointers();
        assert_eq!(
            redact(
                "/auth/registration_shared_secret",
                &Value::String("hunter2".into()),
                &secrets
            ),
            Value::String(REDACTED.into())
        );
        assert_eq!(
            redact(
                "/server/server_name",
                &Value::String("example.org".into()),
                &secrets
            ),
            Value::String("example.org".into())
        );
    }

    #[test]
    fn an_array_index_does_not_hide_a_secret() {
        assert_eq!(
            normalize_pointer("/auth/oidc/providers/0/client_secret"),
            "/auth/oidc/providers/client_secret"
        );
        assert_eq!(
            normalize_pointer("/auth/registration_shared_secret"),
            "/auth/registration_shared_secret"
        );
    }

    #[test]
    fn the_effective_document_carries_settings_nobody_set() {
        let mut config = Config::default();
        config.server.server_name = "example.org".to_owned();
        let document = effective_document(&config);
        assert!(
            document.pointer("/federation/client_timeout").is_some(),
            "a default must still be listed, with `default` as its origin"
        );
    }

    fn booted(dir: &std::path::Path, env: Vec<(String, String)>) -> Booted {
        crate::bootstrap::boot_with_env(
            &crate::bootstrap::BootOptions {
                source: crate::bootstrap::ConfigSource::None,
                data_dir: Some(dir.to_owned()),
                server_name: Some("example.org".to_owned()),
            },
            env,
        )
        .unwrap()
    }

    /// The whole reason `hs config set` exists: what it writes is what the server runs on next
    /// time, and it says so when asked where the value came from.
    #[test]
    fn a_setting_written_here_is_what_the_next_boot_runs_on() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut booted = booted(dir.path(), Vec::new());
            assert!(!booted.resolve().unwrap().config.auth.enable_registration);
            set(&mut booted, "/auth/enable_registration", "true").unwrap();
        }
        let booted = booted(dir.path(), Vec::new());
        let resolved = booted.resolve().unwrap();
        assert!(resolved.config.auth.enable_registration);
        assert_eq!(
            resolved.origin("/auth/enable_registration"),
            Origin::Database
        );
        assert!(
            show(&booted, ShowFormat::Table)
                .unwrap()
                .contains("/auth/enable_registration"),
            "show must list the setting it was just told about"
        );
        assert_eq!(get(&booted, "/auth/enable_registration").unwrap(), "true");
    }

    /// A value that would not survive validation is refused before it is stored, so the database
    /// never holds a configuration the server cannot boot on.
    #[test]
    fn a_change_that_would_not_validate_is_refused_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut booted = booted(dir.path(), Vec::new());
        let err = set(&mut booted, "/server/server_name", "").unwrap_err();
        assert!(matches!(err, ConfigCmdError::WouldNotValidate(_)), "{err}");
        assert_eq!(
            booted.resolve().unwrap().config.server.server_name,
            "example.org",
            "the refused change left the running configuration alone"
        );
    }

    /// Writing a setting the environment pins would be stored faithfully and then ignored, which
    /// is the kind of success an operator finds out about hours later.
    #[test]
    fn a_setting_the_environment_pins_is_refused_rather_than_quietly_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut booted = booted(
            dir.path(),
            vec![(
                "HS__AUTH__ENABLE_REGISTRATION".to_owned(),
                "false".to_owned(),
            )],
        );
        let err = set(&mut booted, "/auth/enable_registration", "true").unwrap_err();
        match err {
            ConfigCmdError::PinnedByEnvironment { pointers, .. } => {
                assert_eq!(pointers, vec!["/auth/enable_registration".to_owned()]);
            }
            other => panic!("expected a pinned-by-environment refusal, got {other}"),
        }
    }

    /// `unset` puts a setting back to what it would have been if nobody had ever touched it.
    #[test]
    fn unset_returns_a_setting_to_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut booted = booted(dir.path(), Vec::new());
        set(&mut booted, "/auth/enable_registration", "true").unwrap();
        unset(&mut booted, "/auth/enable_registration").unwrap();
        let resolved = booted.resolve().unwrap();
        assert!(!resolved.config.auth.enable_registration);
        assert_eq!(
            resolved.origin("/auth/enable_registration"),
            Origin::Default
        );
    }

    /// Export then import is how a server is rebuilt somewhere else, so the round trip has to
    /// preserve what was stored.
    #[test]
    fn export_round_trips_through_import() {
        let source = tempfile::tempdir().unwrap();
        let backup = tempfile::tempdir().unwrap().keep();
        let backup_file = backup.join("config.yaml");
        {
            let mut booted = booted(source.path(), Vec::new());
            set(&mut booted, "/auth/enable_registration", "true").unwrap();
            set(&mut booted, "/federation/client_timeout", "45s").unwrap();
            std::fs::write(&backup_file, export(&booted).unwrap()).unwrap();
        }

        let restored = tempfile::tempdir().unwrap();
        let mut booted = booted(restored.path(), Vec::new());
        import(&mut booted, &backup_file).unwrap();
        let resolved = booted.resolve().unwrap();
        assert!(resolved.config.auth.enable_registration);
        assert_eq!(
            resolved.config.federation.client_timeout,
            hs_config::Duration::from_secs(45)
        );
        std::fs::remove_dir_all(backup).ok();
    }

    /// `storage` says where the database is, so it cannot be restored *into* that database. An
    /// import that carries one says so rather than pretending it worked.
    #[test]
    fn importing_the_storage_section_is_reported_not_silently_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut booted = booted(dir.path(), Vec::new());
        let file = dir.path().join("import.yaml");
        std::fs::write(
            &file,
            "storage:\n  backend: embedded\n  data_dir: /somewhere/else\nauth:\n  enable_registration: true\n",
        )
        .unwrap();
        let report = import(&mut booted, &file).unwrap();
        assert!(report.contains("storage"), "{report}");
        assert!(booted.resolve().unwrap().config.auth.enable_registration);
    }

    /// A secret must not appear in `show`, which is the output an operator pastes into an issue.
    #[test]
    fn show_does_not_print_a_secret_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let mut booted = booted(dir.path(), Vec::new());
        set(&mut booted, "/auth/registration_shared_secret", "hunter2").unwrap();
        let table = show(&booted, ShowFormat::Table).unwrap();
        assert!(!table.contains("hunter2"), "{table}");
        assert!(table.contains(REDACTED));
        assert_eq!(
            get(&booted, "/auth/registration_shared_secret").unwrap(),
            REDACTED
        );
        assert!(
            export(&booted).unwrap().contains("hunter2"),
            "an export is a backup: redacting it would restore a server that cannot register"
        );
    }

    /// `show` lists every setting, including ones no layer set, and attributes each one.
    #[test]
    fn settings_attribute_each_value_to_the_layer_that_set_it() {
        let resolved = layers_for_test().resolve().unwrap();
        let rows = settings(&resolved);
        let server_name = rows
            .iter()
            .find(|(p, _, _)| p == "/server/server_name")
            .expect("server_name is in the effective configuration");
        assert_eq!(server_name.2, Origin::Database);
        let untouched = rows
            .iter()
            .find(|(p, _, _)| p == "/federation/client_timeout")
            .expect("an unset setting is still listed");
        assert_eq!(untouched.2, Origin::Default);
    }
}
