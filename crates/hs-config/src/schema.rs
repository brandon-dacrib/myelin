//! The configuration's JSON Schema, annotated with when each setting takes effect.
//!
//! [`json_schema`] is `schemars::schema_for!(Config)` with one addition: every setting the
//! reload boundary classifies ([`crate::reload::SETTINGS`]) carries `"x-applies"` --
//! `"bootstrap"`, `"hot"` or `"restart"` -- on its property, so the admin API, the management
//! interface and the generated `docs/config.md` all read the classification from the one table
//! rather than each keeping a copy. A setting inside a classified one (`per_second` inside
//! `/rate_limits/message`) inherits its nearest annotated ancestor's value.
//!
//! [`field_pointers`] walks the same schema and lists every setting it declares, as JSON
//! Pointers into the whole configuration; the reload boundary's tests use it to fail the moment a
//! setting is added without a classification.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use crate::Config;
use crate::reload::{Applies, SETTINGS};

/// The key every classified setting's property carries in [`json_schema`].
pub const APPLIES_KEY: &str = "x-applies";

/// The plain derived schema, without the classification.
fn derived() -> Value {
    // A derived schema always serializes; `Null` would only make every lookup below miss.
    serde_json::to_value(schemars::schema_for!(Config)).unwrap_or(Value::Null)
}

/// The whole configuration's JSON Schema, with `x-applies` on every classified setting. Built
/// once per process.
#[must_use]
pub fn json_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| annotate(derived()).0)
}

/// `schema` with `x-applies` written onto the property of every setting in
/// [`SETTINGS`], and the settings whose property could not be found or that met an annotation
/// of another kind at the same place (a type shared between two settings classified
/// differently). The second list is empty for the real schema; a test makes sure of it.
fn annotate(mut schema: Value) -> (Value, Vec<String>) {
    let mut problems = Vec::new();
    for setting in SETTINGS {
        let Some(site) = locate(&schema, setting.pointer) else {
            problems.push(format!("{}: no such property", setting.pointer));
            continue;
        };
        let Some(Value::Object(property)) = schema.pointer_mut(&site) else {
            problems.push(format!("{}: {site} is not an object", setting.pointer));
            continue;
        };
        let value = Value::String(setting.applies.as_str().to_owned());
        match property.get(APPLIES_KEY) {
            Some(existing) if *existing != value => problems.push(format!(
                "{}: {site} is already {existing}, not {value}",
                setting.pointer
            )),
            _ => {
                property.insert(APPLIES_KEY.to_owned(), value);
            }
        }
    }
    (schema, problems)
}

/// The JSON Pointer, within the schema document, of the property schema that declares the
/// setting at `pointer` (a whole-configuration pointer such as `/telemetry/logging/level`).
fn locate(schema: &Value, pointer: &str) -> Option<String> {
    let mut site = String::new();
    for token in pointer.strip_prefix('/')?.split('/') {
        site = find_property(schema, &site, token, &mut Vec::new())?;
    }
    Some(site)
}

/// Looks for property `key` in the schema at `site`, following a `$ref` and every combinator
/// branch (an optional section is an `anyOf` with `null`; a tagged enum is a `oneOf`).
fn find_property(
    schema: &Value,
    site: &str,
    key: &str,
    visiting: &mut Vec<String>,
) -> Option<String> {
    let node = schema.pointer(site)?;
    if node
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(key))
    {
        return Some(format!("{site}/properties/{}", escape(key)));
    }
    if let Some(reference) = node.get("$ref").and_then(Value::as_str)
        && let Some(target) = reference.strip_prefix('#')
        && !visiting.iter().any(|seen| seen == target)
    {
        visiting.push(target.to_owned());
        let found = find_property(schema, target, key, visiting);
        visiting.pop();
        if found.is_some() {
            return found;
        }
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = node.get(keyword).and_then(Value::as_array) {
            for index in 0..branches.len() {
                if let Some(found) =
                    find_property(schema, &format!("{site}/{keyword}/{index}"), key, visiting)
                {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Every setting the schema declares, as whole-configuration JSON Pointers: each section, each
/// field of a section, and each field of a field that is itself a structure, through optional
/// structures and every variant of a tagged enum. The elements of a list or a map are not
/// settings of their own (a listener, an OIDC provider): the list is.
#[must_use]
pub fn field_pointers() -> BTreeSet<String> {
    let schema = derived();
    let mut out = BTreeSet::new();
    walk_fields(&schema, &schema, "", &mut out, &mut Vec::new());
    out
}

fn walk_fields(
    schema: &Value,
    node: &Value,
    pointer: &str,
    out: &mut BTreeSet<String>,
    visiting: &mut Vec<String>,
) {
    let Some(object) = node.as_object() else {
        return;
    };
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            if is_tag(object, key) {
                continue;
            }
            let child_pointer = format!("{pointer}/{}", escape(key));
            out.insert(child_pointer.clone());
            walk_fields(schema, child, &child_pointer, out, visiting);
        }
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && let Some(target) = reference.strip_prefix('#')
        && !visiting.iter().any(|seen| seen == target)
        && let Some(resolved) = schema.pointer(target)
    {
        visiting.push(target.to_owned());
        walk_fields(schema, resolved, pointer, out, visiting);
        visiting.pop();
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = object.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                walk_fields(schema, branch, pointer, out, visiting);
            }
        }
    }
}

/// True when `key` is an internally tagged enum's discriminator (`backend`, with a `const`
/// value): it chooses a variant rather than being a setting.
fn is_tag(object: &Map<String, Value>, key: &str) -> bool {
    object
        .get("properties")
        .and_then(|properties| properties.get(key))
        .is_some_and(|property| property.get("const").is_some())
}

/// Escapes one JSON Pointer token (RFC 6901).
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// The classification `json_schema` carries for the property at `pointer`, if that exact
/// setting is annotated. For anything else use [`crate::reload::applies`].
#[must_use]
pub fn annotated(pointer: &str) -> Option<Applies> {
    let schema = json_schema();
    let site = locate(schema, pointer)?;
    match schema.pointer(&site)?.get(APPLIES_KEY)?.as_str()? {
        "bootstrap" => Some(Applies::Bootstrap),
        "hot" => Some(Applies::Hot),
        "restart" => Some(Applies::Restart),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_classified_setting_is_annotated_without_conflict() {
        let (_, problems) = annotate(derived());
        assert!(problems.is_empty(), "{problems:#?}");
        for setting in SETTINGS {
            assert_eq!(
                annotated(setting.pointer),
                Some(setting.applies),
                "{}",
                setting.pointer
            );
        }
    }

    #[test]
    fn the_walk_finds_sections_fields_nested_fields_and_variants() {
        let fields = field_pointers();
        for expected in [
            "/server",
            "/server/public_baseurl",
            "/telemetry/logging/level",
            "/rate_limits/message/burst_count",
            // A field of one variant of a tagged enum.
            "/storage/host",
            "/media/storage/bucket",
            // A field of an optional structure.
            "/telemetry/sentry/dsn",
        ] {
            assert!(fields.contains(expected), "{expected} not found");
        }
        assert!(!fields.contains("/storage/backend"), "a tag is no setting");
        assert!(
            !fields
                .iter()
                .any(|f| f.starts_with("/listeners/listeners/")),
            "a list's elements are not settings"
        );
    }

    #[test]
    fn the_annotated_schema_is_the_derived_one_plus_the_annotations() {
        fn strip(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    map.remove(APPLIES_KEY);
                    map.values_mut().for_each(strip);
                }
                Value::Array(items) => items.iter_mut().for_each(strip),
                _ => {}
            }
        }
        let mut annotated = json_schema().clone();
        strip(&mut annotated);
        assert_eq!(annotated, derived());
    }
}
