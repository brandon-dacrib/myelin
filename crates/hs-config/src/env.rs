//! `HS__section__key` environment variable overrides.
//!
//! The path after the `HS__` prefix is split on `__` (double underscore);
//! each segment becomes one level of nesting into the parsed config tree,
//! lower-cased to match the schema's `snake_case` field names. This is why
//! the separator is two underscores rather than one: field names like
//! `registration_shared_secret` already contain single underscores.
//!
//! Examples: `HS__SERVER__SERVER_NAME=example.org` sets `server.server_name`;
//! `HS__RATE_LIMITS__LOGIN__PER_SECOND=0.5` sets
//! `rate_limits.login.per_second`.
//!
//! Each value is parsed as YAML scalar first (so `true`, `8008`, `1.5`
//! become their typed form), falling back to a plain string when that
//! fails or produces a sequence/mapping the value clearly isn't (a bare
//! word with a colon, for instance).

use serde_yaml_ng::Value;

/// The required prefix on every recognized override variable.
pub const PREFIX: &str = "HS__";

/// The nesting separator within the path.
const SEP: &str = "__";

/// Applies every `(key, value)` pair whose key starts with [`PREFIX`] onto
/// `root`, creating intermediate mappings as needed. Pairs that do not start
/// with the prefix are ignored. Returns the number of overrides applied.
pub fn apply_env_overrides<I>(root: &mut Value, vars: I) -> usize
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut applied = 0;
    for (key, raw_value) in vars {
        let Some(path) = key.strip_prefix(PREFIX) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        let segments: Vec<String> = path.split(SEP).map(|s| s.to_ascii_lowercase()).collect();
        if segments.iter().any(String::is_empty) {
            // e.g. `HS__SERVER__` or `HS____NAME`: not a valid path, skip
            // rather than guess.
            continue;
        }
        let value = parse_scalar(&raw_value);
        set_path(root, &segments, value);
        applied += 1;
    }
    applied
}

/// Convenience wrapper reading overrides from the process environment.
pub fn apply_process_env_overrides(root: &mut Value) -> usize {
    apply_env_overrides(root, std::env::vars())
}

/// The overrides in `vars`, as a sparse configuration document on their own rather than applied
/// to something.
///
/// This is the environment *layer* (see [`crate::document::Origin::Environment`]): the admin API
/// needs to know which settings the environment pins so it can report them as read-only instead
/// of accepting an edit it knows the next restart -- or the next request -- will ignore.
#[must_use]
pub fn override_document<I>(vars: I) -> serde_json::Value
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut root = Value::Mapping(Default::default());
    apply_env_overrides(&mut root, vars);
    serde_json::to_value(&root).unwrap_or(serde_json::Value::Null)
}

fn parse_scalar(raw: &str) -> Value {
    // A bare scalar only: reject anything that parses to a mapping or
    // sequence, since `HS__` values are meant to set one leaf, not splice a
    // YAML document, and stray colons or dashes in a plain string
    // (a server name, a URL) must not be reinterpreted as YAML structure.
    match serde_yaml_ng::from_str::<Value>(raw) {
        Ok(v @ (Value::Bool(_) | Value::Number(_) | Value::Null)) => v,
        _ => Value::String(raw.to_owned()),
    }
}

fn set_path(root: &mut Value, segments: &[String], value: Value) {
    if !matches!(root, Value::Mapping(_)) {
        *root = Value::Mapping(Default::default());
    }
    let Value::Mapping(map) = root else {
        unreachable!()
    };
    match segments {
        [] => unreachable!("caller guarantees at least one segment"),
        [last] => {
            map.insert(Value::String(last.clone()), value);
        }
        [head, rest @ ..] => {
            let entry = map
                .entry(Value::String(head.clone()))
                .or_insert_with(|| Value::Mapping(Default::default()));
            set_path(entry, rest, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_document_is_just_the_overrides() {
        let doc = override_document([
            (
                "HS__AUTH__ENABLE_REGISTRATION".to_string(),
                "true".to_string(),
            ),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ]);
        assert_eq!(
            doc,
            serde_json::json!({"auth": {"enable_registration": true}}),
            "only HS__ variables, and nothing the config file said"
        );
    }

    #[test]
    fn sets_nested_scalar() {
        let mut root = Value::Mapping(Default::default());
        let n = apply_env_overrides(
            &mut root,
            [(
                "HS__SERVER__SERVER_NAME".to_string(),
                "example.org".to_string(),
            )],
        );
        assert_eq!(n, 1);
        assert_eq!(
            root.get("server")
                .unwrap()
                .get("server_name")
                .unwrap()
                .as_str(),
            Some("example.org")
        );
    }

    #[test]
    fn parses_typed_scalars() {
        let mut root = Value::Mapping(Default::default());
        apply_env_overrides(
            &mut root,
            [
                ("HS__LISTENERS__ENABLED".to_string(), "true".to_string()),
                (
                    "HS__STORAGE__POSTGRES__PORT".to_string(),
                    "5433".to_string(),
                ),
                (
                    "HS__FEDERATION__CLIENT_TIMEOUT".to_string(),
                    "30s".to_string(),
                ),
            ],
        );
        assert_eq!(
            root.get("listeners").unwrap().get("enabled").unwrap(),
            &Value::Bool(true)
        );
        assert_eq!(
            root.get("storage")
                .unwrap()
                .get("postgres")
                .unwrap()
                .get("port")
                .unwrap(),
            &Value::Number(5433.into())
        );
        // "30s" is not a YAML scalar type other than string, and our
        // Duration type parses it from the string form.
        assert_eq!(
            root.get("federation")
                .unwrap()
                .get("client_timeout")
                .unwrap()
                .as_str(),
            Some("30s")
        );
    }

    #[test]
    fn ignores_unprefixed_and_malformed_keys() {
        let mut root = Value::Mapping(Default::default());
        let n = apply_env_overrides(
            &mut root,
            [
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("HS__".to_string(), "x".to_string()),
                ("HS__SERVER__".to_string(), "x".to_string()),
            ],
        );
        assert_eq!(n, 0);
    }

    #[test]
    fn does_not_reinterpret_urls_as_yaml_structure() {
        let mut root = Value::Mapping(Default::default());
        apply_env_overrides(
            &mut root,
            [(
                "HS__SERVER__PUBLIC_BASEURL".to_string(),
                "https://example.org:8448".to_string(),
            )],
        );
        assert_eq!(
            root.get("server")
                .unwrap()
                .get("public_baseurl")
                .unwrap()
                .as_str(),
            Some("https://example.org:8448")
        );
    }

    #[test]
    fn later_overrides_replace_earlier_ones_at_the_same_path() {
        let mut root = Value::Mapping(Default::default());
        apply_env_overrides(
            &mut root,
            [
                (
                    "HS__SERVER__SERVER_NAME".to_string(),
                    "first.example".to_string(),
                ),
                (
                    "HS__SERVER__SERVER_NAME".to_string(),
                    "second.example".to_string(),
                ),
            ],
        );
        assert_eq!(
            root.get("server")
                .unwrap()
                .get("server_name")
                .unwrap()
                .as_str(),
            Some("second.example")
        );
    }
}
