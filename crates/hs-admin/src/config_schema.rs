//! Which configuration settings are secrets, and what the admin API is allowed to say about
//! them.
//!
//! A secret must never leave this API in the clear, and the only trustworthy record of which
//! settings *are* secrets is the JSON Schema `schemars` derives from
//! [`hs_config::Config`](hs_config::Config) itself: [`hs_config::SecretString`]'s schema carries
//! `"x-secret": true`. Deciding by field name would be a guess that quietly goes wrong the day
//! somebody adds a secret called `pepper` or a harmless field called `client_secret_file`, and
//! the failure would be invisible — a leak looks exactly like a successful response.
//!
//! So this module walks the derived schema and collects the *shape* of every secret-valued
//! setting. It has to be a shape rather than a list of pointers, because secrets live under
//! arrays (`/auth/oidc_providers/0/client_secret`) and inside enum variants that only one storage
//! backend has (`/media/storage/secret_access_key`). [`SecretPaths`] therefore holds patterns
//! whose `*` token matches any one array index or map key, and matching is what redaction and
//! patch handling both drive off.

use std::sync::OnceLock;

use serde_json::{Map, Value};

/// The JSON a redacted secret is rendered as, per the OpenAPI `ConfigSection` schema: the client
/// learns that a value is set without learning what it is.
pub const SECRET_PLACEHOLDER_KEY: &str = "$secret";

/// One token of a [`SecretPaths`] pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// An object property with exactly this name.
    Key(String),
    /// Any one array index or map key — the element type is what carries the secret, and every
    /// element of it does.
    Any,
}

/// Every secret-valued setting in [`hs_config::Config`], as JSON Pointer patterns.
#[derive(Debug, Clone, Default)]
pub struct SecretPaths {
    patterns: Vec<Vec<Token>>,
}

impl SecretPaths {
    /// Derives the set from a JSON Schema document (the root schema plus its `$defs`).
    #[must_use]
    pub fn from_schema(schema: &Value) -> Self {
        let defs = schema
            .get("$defs")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut patterns = Vec::new();
        let mut path = Vec::new();
        let mut visiting = Vec::new();
        walk(schema, &defs, &mut path, &mut patterns, &mut visiting);
        Self { patterns }
    }

    /// True when the setting at `pointer` (a whole-configuration JSON Pointer such as
    /// `/auth/session_secret`) holds a secret.
    #[must_use]
    pub fn is_secret(&self, pointer: &str) -> bool {
        let tokens = split_pointer(pointer);
        self.patterns.iter().any(|pattern| {
            pattern.len() == tokens.len()
                && pattern
                    .iter()
                    .zip(&tokens)
                    .all(|(expected, actual)| match expected {
                        Token::Key(key) => key == actual,
                        Token::Any => true,
                    })
        })
    }

    /// Replaces every secret value inside `value` with `{"$secret": true}`, in place. `prefix` is
    /// `value`'s own JSON Pointer within the whole configuration — `""` for a whole-config
    /// document, `"/auth"` for one section's values.
    ///
    /// A secret that is not set stays absent: reporting `{"$secret": true}` for a null would tell
    /// an operator a password is configured when none is.
    pub fn redact(&self, value: &mut Value, prefix: &str) {
        if !value.is_null() && self.is_secret(prefix) {
            *value = placeholder();
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    self.redact(child, &format!("{prefix}/{}", escape_token(key)));
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    self.redact(child, &format!("{prefix}/{index}"));
                }
            }
            _ => {}
        }
    }

    /// Removes from `patch` every secret the client echoed back as `{"$secret": true}`, in place,
    /// and reports whether anything was removed.
    ///
    /// The web interface renders a form from a section it was served with its secrets already
    /// redacted, so an untouched password field comes back as the placeholder it was shown.
    /// Storing that literally would replace the real secret with a JSON object and lock the
    /// operator out of their own database; dropping the key from the merge patch is what "the
    /// operator did not touch this field" actually means. A placeholder anywhere the schema does
    /// *not* call a secret is left exactly where it is, so it fails validation and is reported,
    /// rather than being silently discarded.
    pub fn strip_echoed_secrets(&self, patch: &mut Value, prefix: &str) -> bool {
        let mut stripped = false;
        match patch {
            Value::Object(map) => {
                let echoed: Vec<String> = map
                    .iter()
                    .filter(|(key, child)| {
                        is_placeholder(child)
                            && self.is_secret(&format!("{prefix}/{}", escape_token(key)))
                    })
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in echoed {
                    map.remove(&key);
                    stripped = true;
                }
                for (key, child) in map.iter_mut() {
                    stripped |= self
                        .strip_echoed_secrets(child, &format!("{prefix}/{}", escape_token(key)));
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    stripped |= self.strip_echoed_secrets(child, &format!("{prefix}/{index}"));
                }
            }
            _ => {}
        }
        stripped
    }
}

/// The redaction marker itself.
fn placeholder() -> Value {
    let mut map = Map::new();
    map.insert(SECRET_PLACEHOLDER_KEY.to_owned(), Value::Bool(true));
    Value::Object(map)
}

/// True for exactly `{"$secret": true}` — nothing looser, so a real object that happens to carry
/// a `$secret` key alongside others is not mistaken for the marker.
fn is_placeholder(value: &Value) -> bool {
    value.as_object().is_some_and(|map| {
        map.len() == 1 && map.get(SECRET_PLACEHOLDER_KEY) == Some(&Value::Bool(true))
    })
}

/// Descends one subschema, recording a pattern wherever `x-secret` appears.
///
/// Every structural keyword that can put a value somewhere is followed: `properties` names a
/// token, array items and map values contribute a wildcard, and the combinators (`$ref`,
/// `oneOf`, `anyOf`, `allOf`, `if`/`then`/`else`) contribute nothing to the path but must still
/// be entered — `storage`'s backend choice and `media.storage`'s are internally-tagged enums, so
/// every one of their settings is behind a `oneOf`, including two secrets.
fn walk(
    schema: &Value,
    defs: &Map<String, Value>,
    path: &mut Vec<Token>,
    out: &mut Vec<Vec<Token>>,
    visiting: &mut Vec<String>,
) {
    let Some(object) = schema.as_object() else {
        return;
    };

    if object.get("x-secret") == Some(&Value::Bool(true)) {
        out.push(path.clone());
        return;
    }

    if let Some(name) = object
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
    {
        // A schema that refers to itself (none today, but nothing stops one) would otherwise
        // recurse until the stack ran out.
        if !visiting.iter().any(|seen| seen == name)
            && let Some(target) = defs.get(name)
        {
            visiting.push(name.to_owned());
            walk(target, defs, path, out, visiting);
            visiting.pop();
        }
    }

    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            path.push(Token::Key(key.clone()));
            walk(child, defs, path, out, visiting);
            path.pop();
        }
    }

    for keyword in ["items", "additionalProperties", "propertyNames"] {
        if let Some(child) = object.get(keyword) {
            path.push(Token::Any);
            walk(child, defs, path, out, visiting);
            path.pop();
        }
    }

    for keyword in ["prefixItems", "patternProperties"] {
        match object.get(keyword) {
            Some(Value::Array(items)) => {
                for child in items {
                    path.push(Token::Any);
                    walk(child, defs, path, out, visiting);
                    path.pop();
                }
            }
            Some(Value::Object(map)) => {
                for child in map.values() {
                    path.push(Token::Any);
                    walk(child, defs, path, out, visiting);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = object.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                walk(branch, defs, path, out, visiting);
            }
        }
    }

    for keyword in ["then", "else", "not"] {
        if let Some(child) = object.get(keyword) {
            walk(child, defs, path, out, visiting);
        }
    }
}

/// Splits a JSON Pointer into its unescaped tokens (RFC 6901). The empty pointer is no tokens,
/// which matches nothing, so the root is never mistaken for a secret.
fn split_pointer(pointer: &str) -> Vec<String> {
    pointer
        .strip_prefix('/')
        .map(|rest| {
            rest.split('/')
                .map(|token| token.replace("~1", "/").replace("~0", "~"))
                .collect()
        })
        .unwrap_or_default()
}

/// Escapes one JSON Pointer token (RFC 6901), mirroring `hs_config::document`'s own escaping so
/// the pointers this module builds and the ones that crate reports are the same strings.
fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// The JSON Schema for the whole configuration, derived once per process.
///
/// `schemars` builds this from the same type the server deserializes, so a setting cannot exist
/// in one and be missing from the other. It is what the management interface renders its forms
/// from — types, defaults, enums, descriptions and all — which is why no field name appears
/// anywhere in this crate.
pub fn config_json_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::to_value(schemars::schema_for!(hs_config::Config))
            .expect("a derived JSON Schema is always representable as JSON")
    })
}

/// The secret settings of [`hs_config::Config`], derived once per process from
/// [`config_json_schema`].
pub fn secret_paths() -> &'static SecretPaths {
    static PATHS: OnceLock<SecretPaths> = OnceLock::new();
    PATHS.get_or_init(|| SecretPaths::from_schema(config_json_schema()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The point of deriving this from the schema rather than from field names: these are the
    /// real secrets of the real `Config`, found without this crate naming one of them in
    /// anything but a test assertion. Two of them are only reachable through an
    /// internally-tagged enum variant, and one through an array.
    #[test]
    fn every_secret_in_the_real_config_is_found_through_the_schema() {
        let paths = secret_paths();
        for pointer in [
            "/auth/session_secret",
            "/auth/registration_shared_secret",
            "/auth/password/pepper",
            "/auth/mas_delegation/shared_secret",
            "/auth/oidc_providers/0/client_secret",
            "/auth/oidc_providers/7/client_secret",
            "/storage/password",
            "/media/storage/secret_access_key",
            "/media/storage/access_key",
            "/telemetry/sentry/dsn",
            "/cluster/mesh/shared_secret",
        ] {
            assert!(
                paths.is_secret(pointer),
                "{pointer} was not seen as a secret"
            );
        }
    }

    /// The other half of the same guarantee: a `*_file` path is not a secret (it is a filename,
    /// and hiding it would hide a misconfiguration), and neither is a setting that merely sits
    /// next to one.
    #[test]
    fn ordinary_settings_are_not_secrets() {
        let paths = secret_paths();
        for pointer in [
            "/auth/session_secret_file",
            "/auth/enable_registration",
            "/storage/password_file",
            "/storage/host",
            "/auth/oidc_providers/0/client_id",
            "/cluster/shared_secret",
            "",
            "/auth",
        ] {
            assert!(
                !paths.is_secret(pointer),
                "{pointer} was mistaken for a secret"
            );
        }
    }

    #[test]
    fn redaction_replaces_the_value_and_leaves_its_neighbours_alone() {
        let paths = secret_paths();
        let mut section = json!({
            "host": "db.example",
            "password": "hunter2",
            "password_file": "/run/secrets/pg",
        });
        paths.redact(&mut section, "/storage");
        assert_eq!(section["password"], json!({"$secret": true}));
        assert_eq!(section["host"], json!("db.example"));
        assert_eq!(section["password_file"], json!("/run/secrets/pg"));
    }

    #[test]
    fn redaction_reaches_into_arrays() {
        let paths = secret_paths();
        let mut section = json!({
            "oidc_providers": [
                {"idp_id": "a", "client_id": "public", "client_secret": "s3kr1t"},
                {"idp_id": "b", "client_id": "public"},
            ],
        });
        paths.redact(&mut section, "/auth");
        assert_eq!(
            section["oidc_providers"][0]["client_secret"],
            json!({"$secret": true})
        );
        assert_eq!(section["oidc_providers"][0]["client_id"], json!("public"));
        assert_eq!(section["oidc_providers"][1].get("client_secret"), None);
    }

    /// A secret nobody has set reads as absent, not as "something is configured here".
    #[test]
    fn an_unset_secret_is_not_reported_as_set() {
        let paths = secret_paths();
        let mut section = json!({"session_secret": Value::Null});
        paths.redact(&mut section, "/auth");
        assert_eq!(section["session_secret"], Value::Null);
    }

    #[test]
    fn an_echoed_placeholder_is_dropped_from_the_patch() {
        let paths = secret_paths();
        let mut patch = json!({
            "session_secret": {"$secret": true},
            "enable_registration": true,
        });
        assert!(paths.strip_echoed_secrets(&mut patch, "/auth"));
        assert_eq!(patch, json!({"enable_registration": true}));
    }

    /// A placeholder where no secret lives stays put, so validation rejects it and says so,
    /// rather than the request quietly doing nothing.
    #[test]
    fn a_placeholder_at_an_ordinary_setting_is_left_to_fail_validation() {
        let paths = secret_paths();
        let mut patch = json!({"enable_registration": {"$secret": true}});
        assert!(!paths.strip_echoed_secrets(&mut patch, "/auth"));
        assert_eq!(patch, json!({"enable_registration": {"$secret": true}}));
    }

    /// Setting a secret is a normal write: only the exact placeholder means "leave it alone".
    #[test]
    fn a_real_new_secret_survives_stripping() {
        let paths = secret_paths();
        let mut patch = json!({"session_secret": "a-new-one"});
        assert!(!paths.strip_echoed_secrets(&mut patch, "/auth"));
        assert_eq!(patch, json!({"session_secret": "a-new-one"}));
    }

    #[test]
    fn the_derived_schema_describes_every_section() {
        let schema = config_json_schema();
        let properties = schema["properties"].as_object().expect("an object schema");
        for name in hs_config::reload::SECTION_NAMES {
            assert!(
                properties.contains_key(*name),
                "{name} is missing from the schema"
            );
        }
    }
}
