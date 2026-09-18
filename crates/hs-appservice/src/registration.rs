//! The appservice registration file: parsing, validation and export.
//!
//! Covers every field `PLAN.md` Appendix B lists as read by the mautrix frameworks — `id`, `url`
//! (nullable), `as_token`, `hs_token`, `sender_localpart`, `rate_limited`, `namespaces` (`users`,
//! `aliases`, `rooms`, each with `regex` and `exclusive`), `protocols`, `receive_ephemeral` /
//! `de.sorunome.msc2409.push_ephemeral`, `org.matrix.msc3202`, `io.element.msc4190` — plus
//! `push_ephemeral`, Synapse's own retained alias for the same flag (both `push_ephemeral` and
//! `de.sorunome.msc2409.push_ephemeral` appear in the wild; Synapse accepts both).
//!
//! Unrecognized top-level keys are preserved verbatim in [`Registration::extra`] rather than
//! rejected, both because Synapse itself is permissive about unknown registration keys and because
//! a registration round-tripped through [`Registration::to_yaml`] (the admin API's registration
//! export, `PLAN.md` section 8.2 — "a bridge added through the registry can export its
//! registration file for the bridge's own config") must not silently drop fields a bridge's own
//! config file needs.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::namespace::{NamespaceRule, Namespaces};
use crate::regexp::NamespacePatternError;

/// A parsed, validated appservice registration.
#[derive(Debug, Clone)]
pub struct Registration {
    /// The registration's own `id`. Distinct from `sender_localpart`: this is the registry's
    /// primary key and appears nowhere in the Matrix protocol itself, only in admin tooling.
    pub id: String,
    /// The URL the homeserver pushes transactions to. `None` (a YAML `null`, not a missing key)
    /// marks a first-class double-puppeting registration: `PLAN.md` section 8.2 requires these are
    /// "never pushed to" — see [`crate::scheduler`].
    pub url: Option<String>,
    /// The token the appservice presents to authenticate its own client-server API calls.
    pub as_token: String,
    /// The token the homeserver presents when it calls the appservice (transactions, ping).
    pub hs_token: String,
    /// The localpart of this appservice's own "bot" user.
    pub sender_localpart: String,
    /// Whether ordinary rate limits apply to this appservice's sender and masqueraded users.
    /// Defaults to `true` (rate limited), matching the spec's default; bridges set this `false`
    /// to get `PLAN.md` section 8.1 point 7's rate-limit exemption.
    pub rate_limited: bool,
    /// `namespaces.{users,aliases,rooms}`.
    pub namespaces: Namespaces,
    /// Third-party network protocol IDs this appservice answers `/thirdparty/*` queries for.
    pub protocols: Vec<String>,
    /// MSC2409 ephemeral event delivery, requested as `receive_ephemeral` (stable) or
    /// `de.sorunome.msc2409.push_ephemeral` / `push_ephemeral` (legacy spellings still sent by
    /// some bridges and read by Synapse).
    pub receive_ephemeral: bool,
    /// MSC3202: device-list changes, one-time-key counts and fallback-key-type fields on
    /// transactions, plus permission to use `org.matrix.msc3202.device_id` device masquerading.
    /// Requested as a truthy `org.matrix.msc3202` key.
    pub msc3202: bool,
    /// MSC4190: device creation and deletion via the appservice API without a login flow.
    /// Requested as a truthy `io.element.msc4190` key.
    pub msc4190: bool,
    /// Every top-level registration key not named above, preserved verbatim for export
    /// round-tripping.
    pub extra: Map<String, Value>,
}

/// An error parsing or validating a registration file.
#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    /// The file was not valid YAML.
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    /// The top-level document was not a YAML mapping.
    #[error("registration must be a YAML mapping at the top level")]
    NotAMapping,
    /// A required field was absent.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    /// A field was present but not the expected JSON type.
    #[error("field `{field}` must be a {expected}")]
    WrongType {
        /// The offending field's dotted path.
        field: String,
        /// A human description of the expected shape (`"string"`, `"boolean"`, `"array"`, ...).
        expected: &'static str,
    },
    /// A namespace's `regex` did not compile.
    #[error("in namespaces.{category}[{index}]: {source}")]
    Namespace {
        /// `"users"`, `"aliases"` or `"rooms"`.
        category: &'static str,
        /// The index within that category's array.
        index: usize,
        /// The underlying compile error.
        #[source]
        source: NamespacePatternError,
    },
}

fn as_str<'a>(obj: &'a Map<String, Value>, field: &str) -> Result<Option<&'a str>, RegistrationError> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(RegistrationError::WrongType {
            field: field.to_string(),
            expected: "string",
        }),
    }
}

fn required_str(obj: &Map<String, Value>, field: &'static str) -> Result<String, RegistrationError> {
    as_str(obj, field)?
        .map(str::to_string)
        .ok_or(RegistrationError::MissingField(field))
}

fn as_bool(
    obj: &Map<String, Value>,
    field: &str,
    default: bool,
) -> Result<bool, RegistrationError> {
    match obj.get(field) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        // Some registration authors set these MSC flags to a non-bool truthy value (an empty
        // object, historically, for `org.matrix.msc3202` before it became a plain bool in the
        // stabilized field). Treat presence of anything other than an explicit `false`/`null` as
        // opt-in, since the field existing at all is the operator's signal of intent.
        Some(Value::Null) => Ok(default),
        Some(_) => Ok(true),
    }
}

fn as_str_array(obj: &Map<String, Value>, field: &str) -> Result<Vec<String>, RegistrationError> {
    match obj.get(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| RegistrationError::WrongType {
                        field: format!("{field}[{i}]"),
                        expected: "string",
                    })
            })
            .collect(),
        Some(_) => Err(RegistrationError::WrongType {
            field: field.to_string(),
            expected: "array of strings",
        }),
    }
}

fn parse_namespace_category(
    obj: &Map<String, Value>,
    category: &'static str,
) -> Result<Vec<NamespaceRule>, RegistrationError> {
    let Some(namespaces) = obj.get("namespaces").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let Some(entries) = namespaces.get(category) else {
        return Ok(Vec::new());
    };
    let Value::Array(entries) = entries else {
        return Err(RegistrationError::WrongType {
            field: format!("namespaces.{category}"),
            expected: "array",
        });
    };
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let entry = entry
                .as_object()
                .ok_or_else(|| RegistrationError::WrongType {
                    field: format!("namespaces.{category}[{index}]"),
                    expected: "mapping",
                })?;
            let regex = as_str(entry, "regex")?.ok_or(RegistrationError::MissingField("regex"))?;
            let exclusive = as_bool(entry, "exclusive", false)?;
            NamespaceRule::compile(regex, exclusive).map_err(|source| RegistrationError::Namespace {
                category,
                index,
                source,
            })
        })
        .collect()
}

/// Vendor-prefixed keys this parser consumes explicitly; everything else falls into
/// [`Registration::extra`] on parse, and is written back out by [`Registration::to_yaml`] via the
/// canonical field name plus, where a legacy alias is still commonly read, that alias too.
const CONSUMED_KEYS: &[&str] = &[
    "id",
    "url",
    "as_token",
    "hs_token",
    "sender_localpart",
    "rate_limited",
    "namespaces",
    "protocols",
    "receive_ephemeral",
    "de.sorunome.msc2409.push_ephemeral",
    "push_ephemeral",
    "org.matrix.msc3202",
    "io.element.msc4190",
];

impl Registration {
    /// Parses a registration file's YAML text.
    ///
    /// # Errors
    /// Returns [`RegistrationError`] on malformed YAML, a wrong-typed or missing required field,
    /// or a namespace pattern that fails to compile under both regex engines.
    pub fn parse_yaml(yaml: &str) -> Result<Self, RegistrationError> {
        let yaml_value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml)?;
        let json_value = serde_json::to_value(yaml_value)
            .map_err(|e| RegistrationError::WrongType {
                field: e.to_string(),
                expected: "representable as JSON",
            })?;
        let Value::Object(obj) = json_value else {
            return Err(RegistrationError::NotAMapping);
        };
        Self::from_object(obj)
    }

    fn from_object(obj: Map<String, Value>) -> Result<Self, RegistrationError> {
        let id = required_str(&obj, "id")?;
        let url = as_str(&obj, "url")?.map(str::to_string);
        let as_token = required_str(&obj, "as_token")?;
        let hs_token = required_str(&obj, "hs_token")?;
        let sender_localpart = required_str(&obj, "sender_localpart")?;
        let rate_limited = as_bool(&obj, "rate_limited", true)?;
        let namespaces = Namespaces {
            users: parse_namespace_category(&obj, "users")?,
            aliases: parse_namespace_category(&obj, "aliases")?,
            rooms: parse_namespace_category(&obj, "rooms")?,
        };
        let protocols = as_str_array(&obj, "protocols")?;

        // Stable name first, then the two legacy spellings mautrix and Synapse both still write.
        let receive_ephemeral = as_bool(&obj, "receive_ephemeral", false)?
            || as_bool(&obj, "de.sorunome.msc2409.push_ephemeral", false)?
            || as_bool(&obj, "push_ephemeral", false)?;
        let msc3202 = as_bool(&obj, "org.matrix.msc3202", false)?;
        let msc4190 = as_bool(&obj, "io.element.msc4190", false)?;

        let extra: Map<String, Value> = obj
            .into_iter()
            .filter(|(k, _)| !CONSUMED_KEYS.contains(&k.as_str()))
            .collect();

        Ok(Self {
            id,
            url,
            as_token,
            hs_token,
            sender_localpart,
            rate_limited,
            namespaces,
            protocols,
            receive_ephemeral,
            msc3202,
            msc4190,
            extra,
        })
    }

    /// Renders this registration back to YAML, suitable for a bridge's own config file
    /// (`PLAN.md` section 8.2's registration export). Writes both the stable and the
    /// `de.sorunome.msc2409.push_ephemeral` legacy spelling for `receive_ephemeral` when true, so
    /// the exported file works unchanged against bridge code that still only recognizes the
    /// legacy key. Fields captured in [`Registration::extra`] on import are written back verbatim.
    #[must_use]
    pub fn to_yaml(&self) -> String {
        let mut obj = Map::new();
        obj.insert("id".to_string(), Value::String(self.id.clone()));
        obj.insert(
            "url".to_string(),
            self.url.clone().map_or(Value::Null, Value::String),
        );
        obj.insert(
            "as_token".to_string(),
            Value::String(self.as_token.clone()),
        );
        obj.insert(
            "hs_token".to_string(),
            Value::String(self.hs_token.clone()),
        );
        obj.insert(
            "sender_localpart".to_string(),
            Value::String(self.sender_localpart.clone()),
        );
        obj.insert(
            "rate_limited".to_string(),
            Value::Bool(self.rate_limited),
        );

        let mut namespaces = Map::new();
        for (key, rules) in [
            ("users", &self.namespaces.users),
            ("aliases", &self.namespaces.aliases),
            ("rooms", &self.namespaces.rooms),
        ] {
            let arr: Vec<Value> = rules
                .iter()
                .map(|r| {
                    let mut m = Map::new();
                    m.insert(
                        "regex".to_string(),
                        Value::String(r.pattern.source().to_string()),
                    );
                    m.insert("exclusive".to_string(), Value::Bool(r.exclusive));
                    Value::Object(m)
                })
                .collect();
            namespaces.insert(key.to_string(), Value::Array(arr));
        }
        obj.insert("namespaces".to_string(), Value::Object(namespaces));

        obj.insert(
            "protocols".to_string(),
            Value::Array(self.protocols.iter().cloned().map(Value::String).collect()),
        );

        if self.receive_ephemeral {
            obj.insert("receive_ephemeral".to_string(), Value::Bool(true));
            obj.insert(
                "de.sorunome.msc2409.push_ephemeral".to_string(),
                Value::Bool(true),
            );
        }
        if self.msc3202 {
            obj.insert("org.matrix.msc3202".to_string(), Value::Bool(true));
        }
        if self.msc4190 {
            obj.insert("io.element.msc4190".to_string(), Value::Bool(true));
        }

        for (k, v) in &self.extra {
            obj.insert(k.clone(), v.clone());
        }

        // Sort keys for a stable, diffable export (BTreeMap re-orders the JSON map before
        // handing it to the YAML serializer, which otherwise preserves insertion order).
        let sorted: BTreeMap<String, Value> = obj.into_iter().collect();
        serde_yaml_ng::to_string(&sorted).expect("a Registration always serializes to valid YAML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registration file shaped like the one `mautrix-whatsapp`'s `example-config.yaml` and
    /// `mautrix-go`'s `appservice.Create` generator document: MSC2409 ephemeral events, MSC3202
    /// device lists/OTK counts, MSC4190 device management, one exclusive user namespace, one
    /// exclusive alias namespace, no room namespace, rate-limit exemption for the bridge bot.
    const MAUTRIX_WHATSAPP: &str = r#"
id: whatsapp
url: http://localhost:29318
as_token: "aaaabbbbccccddddeeeeffffgggghhhh0000111122223333444455556666"
hs_token: "1111222233334444555566667777888899990000aaaabbbbccccddddeeee"
sender_localpart: whatsappbot
rate_limited: false
namespaces:
  users:
    - regex: '^@whatsapp_.*:example\.org$'
      exclusive: true
  aliases:
    - regex: '^#whatsapp_.*:example\.org$'
      exclusive: true
  rooms: []
receive_ephemeral: true
de.sorunome.msc2409.push_ephemeral: true
org.matrix.msc3202: true
io.element.msc4190: true
protocols:
  - whatsapp
"#;

    /// A registration file shaped like a legacy `mautrix-python` bridge's, which historically only
    /// wrote the unstable `de.sorunome.msc2409.push_ephemeral` key (no `receive_ephemeral`) and
    /// omitted `rate_limited` entirely (so the spec default, `true`, applies).
    const LEGACY_MAUTRIX_PYTHON: &str = r#"
id: telegram
url: "http://localhost:29317"
as_token: as_secret_token_telegram
hs_token: hs_secret_token_telegram
sender_localpart: telegrambot
namespaces:
  users:
    - regex: "@telegram_.+:example\\.org"
      exclusive: true
protocols: ["telegram"]
de.sorunome.msc2409.push_ephemeral: true
"#;

    /// A double-puppeting registration: `url: null`, a *non-exclusive* user namespace covering
    /// every local user, matching `PLAN.md` section 8.2 ("`url: null` registrations ... are how
    /// double puppeting works").
    const DOUBLE_PUPPET: &str = r#"
id: whatsapp_double_puppet
url: null
as_token: dp_as_token
hs_token: dp_hs_token
sender_localpart: whatsappbot
namespaces:
  users:
    - regex: '@.*:example\.org'
      exclusive: false
"#;

    #[test]
    fn parses_the_full_mautrix_whatsapp_shape() {
        let reg = Registration::parse_yaml(MAUTRIX_WHATSAPP).unwrap();
        assert_eq!(reg.id, "whatsapp");
        assert_eq!(reg.url.as_deref(), Some("http://localhost:29318"));
        assert_eq!(reg.sender_localpart, "whatsappbot");
        assert!(!reg.rate_limited);
        assert!(reg.receive_ephemeral);
        assert!(reg.msc3202);
        assert!(reg.msc4190);
        assert_eq!(reg.protocols, vec!["whatsapp".to_string()]);
        assert_eq!(reg.namespaces.users.len(), 1);
        assert!(reg.namespaces.users[0].exclusive);
        assert!(
            reg.namespaces.users[0].is_match("@whatsapp_alice:example.org")
        );
        assert_eq!(reg.namespaces.aliases.len(), 1);
        assert!(reg.namespaces.rooms.is_empty());
    }

    #[test]
    fn legacy_ephemeral_spelling_alone_is_honored() {
        let reg = Registration::parse_yaml(LEGACY_MAUTRIX_PYTHON).unwrap();
        assert!(reg.receive_ephemeral);
        assert!(!reg.msc3202);
        assert!(!reg.msc4190);
        // rate_limited defaults to true when absent.
        assert!(reg.rate_limited);
    }

    #[test]
    fn null_url_parses_to_none_and_is_distinct_from_missing() {
        let reg = Registration::parse_yaml(DOUBLE_PUPPET).unwrap();
        assert_eq!(reg.url, None);
        assert!(!reg.namespaces.users[0].exclusive);
    }

    #[test]
    fn missing_required_field_is_an_error() {
        let bad = "id: x\nurl: http://x\nsender_localpart: x\n";
        let err = Registration::parse_yaml(bad).unwrap_err();
        assert!(matches!(err, RegistrationError::MissingField("as_token")));
    }

    #[test]
    fn malformed_namespace_regex_is_a_namespace_error() {
        let bad = r#"
id: x
url: null
as_token: t1
hs_token: t2
sender_localpart: x
namespaces:
  users:
    - regex: "(unterminated"
      exclusive: true
"#;
        let err = Registration::parse_yaml(bad).unwrap_err();
        assert!(matches!(
            err,
            RegistrationError::Namespace {
                category: "users",
                index: 0,
                ..
            }
        ));
    }

    #[test]
    fn unknown_top_level_keys_survive_into_extra() {
        let with_vendor_extra = r#"
id: x
url: null
as_token: t1
hs_token: t2
sender_localpart: x
com.beeper.bridge_state_endpoint: "/bridgestate"
"#;
        let reg = Registration::parse_yaml(with_vendor_extra).unwrap();
        assert_eq!(
            reg.extra.get("com.beeper.bridge_state_endpoint"),
            Some(&Value::String("/bridgestate".to_string()))
        );
    }

    #[test]
    fn round_trips_through_to_yaml_and_back() {
        let reg = Registration::parse_yaml(MAUTRIX_WHATSAPP).unwrap();
        let exported = reg.to_yaml();
        let reparsed = Registration::parse_yaml(&exported).unwrap();
        assert_eq!(reparsed.id, reg.id);
        assert_eq!(reparsed.url, reg.url);
        assert_eq!(reparsed.as_token, reg.as_token);
        assert_eq!(reparsed.hs_token, reg.hs_token);
        assert!(reparsed.receive_ephemeral);
        assert!(reparsed.msc3202);
        assert!(reparsed.msc4190);
        assert_eq!(reparsed.namespaces.users.len(), 1);
    }

    #[test]
    fn export_writes_legacy_ephemeral_alias_too() {
        let reg = Registration::parse_yaml(MAUTRIX_WHATSAPP).unwrap();
        let exported = reg.to_yaml();
        assert!(exported.contains("de.sorunome.msc2409.push_ephemeral: true"));
        assert!(exported.contains("receive_ephemeral: true"));
    }

    #[test]
    fn extra_fields_round_trip_through_export() {
        let with_vendor_extra = r#"
id: x
url: null
as_token: t1
hs_token: t2
sender_localpart: x
com.beeper.bridge_state_endpoint: "/bridgestate"
"#;
        let reg = Registration::parse_yaml(with_vendor_extra).unwrap();
        let exported = reg.to_yaml();
        let reparsed = Registration::parse_yaml(&exported).unwrap();
        assert_eq!(
            reparsed.extra.get("com.beeper.bridge_state_endpoint"),
            Some(&Value::String("/bridgestate".to_string()))
        );
    }
}
