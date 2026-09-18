//! One namespace declaration (`namespaces.{users,aliases,rooms}[]`) from an appservice
//! registration, and the three-category [`Namespaces`] container.

use serde::{Deserialize, Serialize};

use crate::regexp::{NamespacePattern, NamespacePatternError};

/// Which of the three namespace categories a registration declares. Mirrors the spec's
/// `application-service/definitions/namespace.yaml` and its three call sites (`users`, `aliases`,
/// `rooms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamespaceKind {
    /// Full user IDs (`@_ircbridge_.*:example.org`).
    Users,
    /// Room aliases (`#_ircbridge_.*:example.org`).
    Aliases,
    /// Room IDs. Unlike users and aliases this is rarely used by mautrix bridges (rooms are
    /// usually created by the bridge itself, not matched by pattern) but is part of the spec and
    /// some legacy `mautrix-python` bridges and `matrix-appservice-irc` declare it.
    Rooms,
}

impl NamespaceKind {
    /// The registration YAML key for this category.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Aliases => "aliases",
            Self::Rooms => "rooms",
        }
    }
}

/// One compiled namespace rule: a pattern and whether it is exclusive.
///
/// Exclusive namespaces (`exclusive: true`) reserve every matching ID to this appservice alone —
/// no other appservice may register an overlapping exclusive namespace, and (for the `users`
/// category) no human may register a matching localpart. Non-exclusive namespaces just grant
/// masquerading rights (`user_id`) without reserving anything, which is exactly the mechanism
/// double puppeting relies on: a bridge's `users` namespace is non-exclusive so it can masquerade
/// as a real, already-registered human account without claiming to own that account's ID.
#[derive(Debug, Clone)]
pub struct NamespaceRule {
    /// The compiled pattern, matched with Python `re.match` (prefix) semantics — see
    /// [`crate::regexp`].
    pub pattern: NamespacePattern,
    /// Whether this namespace is exclusive. Defaults to `false` when the registration omits the
    /// key, matching the spec's `namespace.yaml` default.
    pub exclusive: bool,
}

impl NamespaceRule {
    /// Compiles a rule from its registration-file fields.
    ///
    /// # Errors
    /// Returns [`NamespacePatternError`] if `regex` does not compile under either regex engine.
    pub fn compile(regex: &str, exclusive: bool) -> Result<Self, NamespacePatternError> {
        Ok(Self {
            pattern: NamespacePattern::compile(regex)?,
            exclusive,
        })
    }

    /// True if `text` matches this rule's pattern.
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        self.pattern.is_match(text)
    }
}

/// The three namespace categories a registration declares, each a list of rules matched in
/// declaration order (the first match wins for "is this exclusive", matching Synapse's behavior
/// of checking every declared namespace and treating any exclusive match as exclusive regardless
/// of order — see [`Namespaces::exclusive_match`]).
#[derive(Debug, Clone, Default)]
pub struct Namespaces {
    /// `namespaces.users`.
    pub users: Vec<NamespaceRule>,
    /// `namespaces.aliases`.
    pub aliases: Vec<NamespaceRule>,
    /// `namespaces.rooms`.
    pub rooms: Vec<NamespaceRule>,
}

impl Namespaces {
    /// The rules for one category.
    #[must_use]
    pub fn category(&self, kind: NamespaceKind) -> &[NamespaceRule] {
        match kind {
            NamespaceKind::Users => &self.users,
            NamespaceKind::Aliases => &self.aliases,
            NamespaceKind::Rooms => &self.rooms,
        }
    }

    /// True if any rule in `kind` matches `text`, regardless of exclusivity — "is this appservice
    /// interested in this ID at all" (Synapse's `is_interested_in_user`/`is_interested_in_alias`).
    #[must_use]
    pub fn is_interested(&self, kind: NamespaceKind, text: &str) -> bool {
        self.category(kind).iter().any(|r| r.is_match(text))
    }

    /// True if any *exclusive* rule in `kind` matches `text` — "does this appservice exclusively
    /// own this ID" (Synapse's `is_exclusive_user`/`is_exclusive_alias`), which is what namespace
    /// conflict detection and human-registration blocking key on.
    #[must_use]
    pub fn exclusive_match(&self, kind: NamespaceKind, text: &str) -> bool {
        self.category(kind)
            .iter()
            .any(|r| r.exclusive && r.is_match(text))
    }
}

/// The serializable, uncompiled form of [`NamespaceRule`] — what actually gets written to the
/// store, since a compiled [`NamespacePattern`] holds engine-specific state that does not
/// (de)serialize. Recompiled back into a [`NamespaceRule`] with [`NamespaceRuleSpec::compile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceRuleSpec {
    /// The pattern's original source text.
    pub regex: String,
    /// Whether this namespace is exclusive.
    #[serde(default)]
    pub exclusive: bool,
}

impl NamespaceRuleSpec {
    /// Compiles this spec into a live [`NamespaceRule`].
    ///
    /// # Errors
    /// Returns [`NamespacePatternError`] if `regex` does not compile.
    pub fn compile(&self) -> Result<NamespaceRule, NamespacePatternError> {
        NamespaceRule::compile(&self.regex, self.exclusive)
    }
}

impl From<&NamespaceRule> for NamespaceRuleSpec {
    fn from(rule: &NamespaceRule) -> Self {
        Self {
            regex: rule.pattern.source().to_string(),
            exclusive: rule.exclusive,
        }
    }
}

/// The serializable, uncompiled form of [`Namespaces`]. See [`NamespaceRuleSpec`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespacesSpec {
    /// `namespaces.users`.
    #[serde(default)]
    pub users: Vec<NamespaceRuleSpec>,
    /// `namespaces.aliases`.
    #[serde(default)]
    pub aliases: Vec<NamespaceRuleSpec>,
    /// `namespaces.rooms`.
    #[serde(default)]
    pub rooms: Vec<NamespaceRuleSpec>,
}

impl NamespacesSpec {
    /// Compiles every rule in every category.
    ///
    /// # Errors
    /// Returns the first [`NamespacePatternError`] encountered.
    pub fn compile(&self) -> Result<Namespaces, NamespacePatternError> {
        Ok(Namespaces {
            users: self.users.iter().map(NamespaceRuleSpec::compile).collect::<Result<_, _>>()?,
            aliases: self.aliases.iter().map(NamespaceRuleSpec::compile).collect::<Result<_, _>>()?,
            rooms: self.rooms.iter().map(NamespaceRuleSpec::compile).collect::<Result<_, _>>()?,
        })
    }
}

impl From<&Namespaces> for NamespacesSpec {
    fn from(ns: &Namespaces) -> Self {
        Self {
            users: ns.users.iter().map(NamespaceRuleSpec::from).collect(),
            aliases: ns.aliases.iter().map(NamespaceRuleSpec::from).collect(),
            rooms: ns.rooms.iter().map(NamespaceRuleSpec::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, exclusive: bool) -> NamespaceRule {
        NamespaceRule::compile(pattern, exclusive).unwrap()
    }

    #[test]
    fn is_interested_ignores_exclusivity() {
        let ns = Namespaces {
            users: vec![rule(r"^@irc_.*:example\.org$", false)],
            ..Default::default()
        };
        assert!(ns.is_interested(NamespaceKind::Users, "@irc_bob:example.org"));
        assert!(!ns.exclusive_match(NamespaceKind::Users, "@irc_bob:example.org"));
    }

    #[test]
    fn exclusive_match_requires_exclusive_flag() {
        let ns = Namespaces {
            users: vec![rule(r"^@irc_.*:example\.org$", true)],
            ..Default::default()
        };
        assert!(ns.exclusive_match(NamespaceKind::Users, "@irc_bob:example.org"));
        assert!(!ns.exclusive_match(NamespaceKind::Users, "@other:example.org"));
    }

    #[test]
    fn spec_round_trips_through_json_and_recompiles() {
        let ns = Namespaces {
            users: vec![rule(r"^@irc_.*:example\.org$", true)],
            ..Default::default()
        };
        let spec = NamespacesSpec::from(&ns);
        let json = serde_json::to_string(&spec).unwrap();
        let back: NamespacesSpec = serde_json::from_str(&json).unwrap();
        let recompiled = back.compile().unwrap();
        assert!(recompiled.exclusive_match(NamespaceKind::Users, "@irc_bob:example.org"));
    }
}
